//! The command line: what `nestc` was asked for, and doing it.
//!
//! [`Invocation`] is public for the tools that are handed a `nestc` command line
//! rather than running one — the language server gets each package's from
//! `twig metadata` — so that a flag means one thing wherever it is read.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::codegen::{self, Codegen, OutputKind};
use crate::common::diagnostic::{Diagnostic, Severity};
use crate::common::emitter::{render, render_json};
use crate::common::options::Options;
use crate::common;
use crate::sema::session::{FileLoader, Session};
use crate::{ir, library, lir, parser, sema};

/// Usage, printed on a bad invocation.
///
/// Three kinds of flag, and the split is about who sets them. `-C` is the
/// **build's**: settings a tool translated from a profile and handed over
/// resolved, which is why an unknown one is an error rather than a default
/// ([`Options`]). `--target` and `--emit` and `-o` are the **invocation's**:
/// what machine, what files, where. And `-h` is the person's.
///
/// A future `twig` (the package tool) drives all three, which is the reason the
/// spellings here are rustc's: a build tool that already knows one compiler's
/// vocabulary should not have to learn a second one to say the same thing.
pub const USAGE: &str = "\
usage: nestc [options] <file.nest>

options:
  -o <path>              where to write the output; with several codegen units,
                         the base that each unit's name is appended to
  --target <triple>      the machine to generate code for (default: the host)
  --emit <list>          comma-separated, any of:
                           link                 an executable, linked (default)
                           ast, ir, mono, lir   the compiler's own dumps, to stdout
                           obj, asm, backend-ir what the backend writes, to files
                           nlib                 the package as a library: its
                                                metadata, its IR and its objects
                         a dump or a backend output written `kind=path` goes
                         to that file instead
  -L <dir>               a directory to search for packages; `<foo/...>` is
                         <dir>/foo/package.nest. Repeatable, in order
  -l <name>              a C library to link against, as the linker names it:
                         `-l m`, `-lsfml-graphics`. Repeatable, in order.
                         `--link-lib <name>` is the same flag spelled out
  --link-search <dir>    a directory to look for those libraries in. Not `-L`,
                         which is this compiler's package search path
  --color <when>         auto (default), always or never: colour in human
                         diagnostics; auto means when stderr is a terminal
                         and `NO_COLOR` is not set
  --error-format <form>  human (default) or json — one JSON object per line,
                         on stderr, for a tool that consumes them
  --package <name>=<path>
                         a package pinned to a root file, beating any -L search
  --extern <name>=<path> a compiled library (.nlib) this compilation
                         depends on and may import. Repeatable
  --indirect <name>=<path>
                         a library a dependency was compiled against: read, and
                         linked, but not importable. Repeatable
  --obj-dir <dir>        keep the objects a link or a library is made from in
                         <dir>, rather than in a temporary directory removed
                         afterwards
  --test                 build a test binary: keep the entry package's `@test`
                         functions and run them instead of its `main`
  --up-to-date           compile nothing: exit 0 when the library at `-o` was
                         compiled from these files, settings and libraries as
                         they are now, and 1 when it would be compiled again
  -C <key>=<value>       a build setting; see below
  -h, --help             this

settings (-C):
  backend=<name>         which code generator (default: the first compiled in)
  codegen-units=N        how many codegen units the program is split into
                         (default: 1 — the whole program in one)
  entry=auto|none        synthesize a C `main` calling the program's `main`
                         when it has one (default: auto)
  linker=<path>          the linker driver used by `--emit link` (default: cc)
  partial-linker=<path>  merges several codegen units into one object
                         (default: ld, run as `ld -r`)
  link-arg=<arg>         one more argument for it; repeatable, in order
  runtime=<path>         the runtime archive to link, overriding the one built
                         beside this compiler
  overflow=trap|wrap     what a run-time integer overflow does (default: trap)
  opt-level=0|1|2|3|s|z  how hard the backend optimizes (default: 0)
  target-cpu=<name>      the processor the code may assume: generic (default),
                         native for this one, or any name the backend knows
  pointer-width=16|32|64 override the target's pointer width
  os=<name>              override the target's operating system
  arch=<name>            override the target's architecture
  profile=debug|release  the build profile's name, readable from source
  print=options          print the resolved settings and exit
  print=packages         print the packages that ship with this compiler, one
                         `name=root` per line, and exit
";

/// What was asked for, parsed.
///
/// The default is **a linked executable**, which is what `cc foo.c` and
/// `rustc foo.rs` both do and what a person running `nestc foo.nest` means. The
/// dumps are still one flag away and nothing about them changed; what changed is
/// that they stopped being what a bare invocation produces, now that there is a
/// backend that can produce a program.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Emit {
    /// The resolved, annotated tree, and the whole-program def table.
    ast: bool,
    /// The entry file's IR.
    ir: bool,
    /// The monomorphized whole program.
    mono: bool,
    /// The LIR, unit by unit.
    lir: bool,
    /// What the backend writes, in the order asked for.
    backend: Vec<OutputKind>,
    /// An executable: objects, and then the linker over them.
    link: bool,
    /// The entry package as a library: its metadata, its IR and its objects in
    /// one archive.
    nlib: bool,
    /// The outputs named with a path, `kind=path`, and where each goes.
    paths: Vec<(String, PathBuf)>,
}

impl Default for Emit {
    fn default() -> Self {
        Emit {
            link: true,
            ..Emit::nothing()
        }
    }
}

impl Emit {
    /// Parse an `--emit` list. Every name is checked, and an unknown one is an
    /// error for the same reason an unknown `-C` key is.
    /// Nothing asked for, which is what a parsed `--emit` starts from: a list
    /// that names something is a list that names *only* that.
    fn nothing() -> Emit {
        Emit {
            ast: false,
            ir: false,
            mono: false,
            lir: false,
            backend: Vec::new(),
            link: false,
            nlib: false,
            paths: Vec::new(),
        }
    }

    /// Where the output `name` was asked to go, when a path was given.
    fn path(&self, name: &str) -> Option<&Path> {
        self.paths.iter().find(|(n, _)| n == name).map(|(_, p)| p.as_path())
    }

    fn parse(list: &str) -> Result<Emit, String> {
        let mut e = Emit::nothing();
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            // `kind=path`, rustc's spelling: for a dump or a file the backend
            // writes. A link, a library and metadata already follow `-o`.
            let name = match item.split_once('=') {
                Some((name, path)) => {
                    if !matches!(name, "ast" | "ir" | "mono" | "lir" | "obj" | "asm" | "backend-ir") {
                        return Err(format!("`--emit {name}` takes no path; it follows `-o`"));
                    }
                    e.paths.push((name.to_string(), PathBuf::from(path)));
                    name
                }
                None => item,
            };
            match name {
                "ast" => e.ast = true,
                "ir" => e.ir = true,
                "mono" => e.mono = true,
                "lir" => e.lir = true,
                "obj" => e.backend.push(OutputKind::Object),
                "asm" => e.backend.push(OutputKind::Assembly),
                "backend-ir" => e.backend.push(OutputKind::Ir),
                "link" => e.link = true,
                "nlib" => e.nlib = true,
                other => {
                    return Err(format!(
                        "`--emit` does not know `{other}`; it takes link, ast, ir, mono, lir, obj, asm, backend-ir, nlib"
                    ));
                }
            }
        }
        Ok(e)
    }
}

/// How diagnostics are written.
///
/// The compiler's own failures go out the same way its diagnostics do, because
/// a tool that asked for JSON asked for *everything* in JSON: a `nestc:` line on
/// stderr in the middle of a stream of objects is a parse error, and it would
/// arrive exactly when something had already gone wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorFormat {
    #[default]
    Human,
    Json,
}

impl ErrorFormat {
    pub fn parse(value: &str) -> Result<ErrorFormat, String> {
        match value {
            "human" => Ok(ErrorFormat::Human),
            "json" => Ok(ErrorFormat::Json),
            other => Err(format!(
                "`--error-format` takes `human` or `json`, not `{other}`"
            )),
        }
    }

    /// Write one diagnostic to stderr, in colour if `color` says so and the
    /// format is one a person reads.
    pub fn emit(self, diag: &Diagnostic, sources: &common::source::SourceMap, color: bool) {
        match self {
            ErrorFormat::Human => eprint!("{}", render(diag, sources, color)),
            ErrorFormat::Json => eprint!("{}", render_json(diag, sources)),
        }
    }
}

/// Whether human diagnostics are coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    pub fn parse(value: &str) -> Result<ColorChoice, String> {
        match value {
            "auto" => Ok(ColorChoice::Auto),
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            other => Err(format!(
                "`--color` takes `auto`, `always` or `never`, not `{other}`"
            )),
        }
    }

    /// `auto` is a terminal on stderr and no `NO_COLOR` (<https://no-color.org>).
    /// A build tool that captures stderr and shows it to a person passes
    /// `always` instead, because what it captured is not a terminal.
    pub fn enabled(self) -> bool {
        use std::io::IsTerminal as _;
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                std::io::stderr().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            }
        }
    }
}

/// A `nestc` command line, parsed.
pub struct Invocation {
    /// The entry file.
    pub path: Option<String>,
    out: Option<PathBuf>,
    triple: Option<String>,
    backend_name: Option<String>,
    emit: Option<Emit>,
    print_options: bool,
    print_packages: bool,
    up_to_date: bool,
    /// `--test`: build the entry package as a test binary.
    test: bool,
    /// Where a link's or a library's objects are kept, when they are.
    obj_dir: Option<PathBuf>,
    format: ErrorFormat,
    color: ColorChoice,
    help: bool,
    search_paths: Vec<String>,
    packages: Vec<(String, String)>,
    externs: Vec<(String, PathBuf, bool)>,
    link_options: codegen::link::LinkOptions,
    settings: Vec<(String, String)>,
}

impl Invocation {
    /// Read `args`, the command line without the program's name.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Invocation, String> {
        let mut help = false;
        let mut path: Option<String> = None;
        let mut out: Option<PathBuf> = None;
        let mut triple: Option<String> = None;
        let mut backend_name: Option<String> = None;
        let mut emit: Option<Emit> = None;
        let mut print_options = false;
        let mut print_packages = false;
        let mut up_to_date = false;
        let mut test = false;
        let mut obj_dir: Option<PathBuf> = None;
        let mut format = ErrorFormat::default();
        let mut color = ColorChoice::default();
        // Where to look for a package nothing registered. The driver holds them
        // rather than `Options` because they are about finding source, not about
        // what is built from it.
        let mut search_paths: Vec<String> = Vec::new();
        // Packages pinned by name, which is how a build tool that has already
        // resolved a dependency hands the answer over rather than a place to look.
        let mut packages: Vec<(String, String)> = Vec::new();
        // Compiled libraries, and whether this compilation may import each: what a
        // build tool passes once it compiles one package at a time.
        let mut externs: Vec<(String, PathBuf, bool)> = Vec::new();
        // The link's settings, which are the driver's for the same reason `backend`
        // is: no pass reads them, and what is compiled does not change because a
        // different linker will run afterwards.
        let mut link_options = codegen::link::LinkOptions::default();
        // The `-C` settings are collected rather than applied, because some of them
        // **override** what the backend is about to report about the machine and an
        // override has to be applied second (see below).
        let mut settings: Vec<(String, String)> = Vec::new();

        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            // A flag that takes a value, in either spelling: `--emit x`, `--emit=x`.
            let value = |flag: &str, args: &mut dyn Iterator<Item = String>| {
                if let Some(v) = arg.strip_prefix(&format!("{flag}=")) {
                    return Ok(v.to_string());
                }
                args.next()
                    .ok_or_else(|| format!("`{flag}` wants a value"))
            };
            match arg.as_str() {
                "-h" | "--help" => {
                    help = true;
                    break;
                }
                a if a == "-o" || a.starts_with("-o=") => {
                    out = Some(PathBuf::from(value("-o", &mut args)?));
                }
                a if a == "--target" || a.starts_with("--target=") => {
                    triple = Some(value("--target", &mut args)?);
                }
                a if a == "-L" || a.starts_with("-L") && a.len() > 2 => {
                    // `-L dir` and `-Ldir` both, which is what rustc accepts.
                    search_paths.push(match arg.strip_prefix("-L") {
                        Some("") => args.next().ok_or("`-L` wants a directory")?,
                        Some(rest) => rest.trim_start_matches('=').to_string(),
                        None => unreachable!(),
                    });
                }
                a if a == "--package" || a.starts_with("--package=") => {
                    let spec = value("--package", &mut args)?;
                    let (name, root) = spec
                        .split_once('=')
                        .ok_or_else(|| format!("`--package {spec}` is not a `name=path` pair"))?;
                    packages.push((name.to_string(), root.to_string()));
                }
                a if a == "--extern" || a.starts_with("--extern=") || a == "--indirect" || a.starts_with("--indirect=") => {
                    let flag = if a.starts_with("--extern") { "--extern" } else { "--indirect" };
                    let spec = value(flag, &mut args)?;
                    let (name, lib) = spec
                        .split_once('=')
                        .ok_or_else(|| format!("`{flag} {spec}` is not a `name=path` pair"))?;
                    externs.push((name.to_string(), PathBuf::from(lib), flag == "--extern"));
                }
                a if a == "-l" || a.starts_with("-l") && a.len() > 2 => {
                    // `-l name` and `-lname` both, which is what every C
                    // toolchain accepts and what a person pasting a
                    // `pkg-config` line has in hand.
                    link_options.libs.push(match arg.strip_prefix("-l") {
                        Some("") => args.next().ok_or("`-l` wants a library name")?,
                        Some(rest) => rest.trim_start_matches('=').to_string(),
                        None => unreachable!(),
                    });
                }
                a if a == "--link-lib" || a.starts_with("--link-lib=") => {
                    link_options.libs.push(value("--link-lib", &mut args)?);
                }
                a if a == "--link-search" || a.starts_with("--link-search=") => {
                    link_options
                        .search
                        .push(PathBuf::from(value("--link-search", &mut args)?));
                }
                a if a == "--obj-dir" || a.starts_with("--obj-dir=") => {
                    obj_dir = Some(PathBuf::from(value("--obj-dir", &mut args)?));
                }
                "--up-to-date" => up_to_date = true,
                "--test" => test = true,
                a if a == "--color" || a.starts_with("--color=") => {
                    color = ColorChoice::parse(&value("--color", &mut args)?)?;
                }
                a if a == "--error-format" || a.starts_with("--error-format=") => {
                    format = ErrorFormat::parse(&value("--error-format", &mut args)?)?;
                }
                a if a == "--emit" || a.starts_with("--emit=") => {
                    emit = Some(Emit::parse(&value("--emit", &mut args)?)?);
                }
                a if a == "-C" || a.starts_with("-C") && a.len() > 2 => {
                    // `-C k=v` and `-Ck=v` both, which is what rustc accepts.
                    let setting = match arg.strip_prefix("-C") {
                        Some("") => args.next().ok_or("`-C` wants a `key=value` setting")?,
                        Some(rest) => rest.to_string(),
                        None => unreachable!(),
                    };
                    let (key, val) = setting
                        .split_once('=')
                        .ok_or_else(|| format!("`-C {setting}` is not a `key=value` setting"))?;
                    match key {
                        // Two `-C` keys belong to the driver rather than to the
                        // compilation: what to print instead of compiling, and which
                        // backend to hold. No pass reads either, which is exactly
                        // why neither is in `Options`.
                        "print" if val == "options" => print_options = true,
                        "print" if val == "packages" => print_packages = true,
                        "print" => {
                            return Err(format!("`print` takes `options` or `packages`, not `{val}`"));
                        }
                        "backend" => backend_name = Some(val.to_string()),
                        "linker" => link_options.linker = val.to_string(),
                        "link-arg" => link_options.args.push(val.to_string()),
                        "runtime" => link_options.runtime = Some(PathBuf::from(val)),
                        _ => settings.push((key.to_string(), val.to_string())),
                    }
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown flag `{other}`"));
                }
                other => path = Some(other.to_string()),
            }
        }

        Ok(Invocation {
            path,
            out,
            triple,
            backend_name,
            emit,
            print_options,
            print_packages,
            up_to_date,
            test,
            obj_dir,
            format,
            color,
            help,
            search_paths,
            packages,
            externs,
            link_options,
            settings,
        })
    }

    /// The backend, and the settings it and the `-C` flags resolve to.
    fn backend(&self) -> Result<(Box<dyn Codegen>, Options), String> {
        // **The backend answers first.** The layout engine and `core`'s generated
        // `target.nest` both read the target, and both run long before code is
        // generated, so the machine's facts have to be settled before analysis
        // starts — from the thing that will actually generate the code rather than
        // from a default that could disagree with it.
        let mut backend = codegen::select(self.backend_name.as_deref())?;
        let info = backend
            .target_info(self.triple.as_deref())
            .map_err(|e| e.to_string())?;

        let mut options = Options {
            target: info.target(),
            ..Options::default()
        };
        // And the `-C` overrides land on top, which is the order that makes them
        // overrides: `-C pointer-width=32` is a person saying they know better than
        // the triple, and it is allowed to.
        for (key, value) in &self.settings {
            options.set(key, value)?;
        }
        // `--test` is a flag rather than a `-C` setting because it is not a
        // property of the build the way `overflow=` is: it changes *what is
        // built* from the same sources, which is the kind of thing `--emit` is.
        options.test = self.test;
        backend.configure(&options);
        Ok((backend, options))
    }

    /// A session to analyze the entry in, with `options`, the packages and
    /// search paths registered, and the libraries read, and every file read
    /// through `loader`.
    fn prepare(&self, loader: Box<dyn FileLoader>, options: Options) -> Result<Session, String> {
        let mut session = Session::with_loader(loader);
        session.options = options;
        // Before anything is loaded: a package is found the first time it is
        // imported, and `core` is imported by the prelude.
        for dir in &self.search_paths {
            session.add_search_path(dir);
        }
        // After the search paths, though the order does not matter: a pinned
        // package wins over a searched one wherever it was registered.
        for (name, root) in &self.packages {
            session.register_package(name, root);
        }
        // Libraries before the entry is read: they are packages already analyzed,
        // and the analysis of this one resolves against them.
        load_libraries(&mut session, &self.externs)?;
        Ok(session)
    }

    /// A session set up the way this command line would compile in, for a
    /// tool that analyzes the entry rather than compiling it.
    pub fn session(&self, loader: Box<dyn FileLoader>) -> Result<Session, String> {
        let (_, options) = self.backend()?;
        self.prepare(loader, options)
    }
}

/// The compilation `args` asks for, with every failure a message rather than an
/// exit.
pub fn run(args: impl IntoIterator<Item = String>) -> Result<ExitCode, String> {
    let inv = Invocation::parse(args)?;
    if inv.help {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let (mut backend, options) = inv.backend()?;
    let (out, emit, format, color) = (inv.out.clone(), inv.emit.clone(), inv.format, inv.color);
    let (externs, link_options) = (&inv.externs, &inv.link_options);

    if inv.print_options {
        print!("{}", options.render());
        return Ok(ExitCode::SUCCESS);
    }
    // What a build tool registers as `--package` for the packages it does not
    // resolve itself, so that the `core` and `std` it builds against are this
    // compiler's — the answer `NEST_CORE` and `NEST_STD` change, and the one a
    // build with no `--package` would have used.
    if inv.print_packages {
        println!("core={}", sema::session::default_core_path());
        println!("std={}", sema::session::default_std_path());
        return Ok(ExitCode::SUCCESS);
    }

    if inv.up_to_date {
        let out = out.ok_or("`--up-to-date` asks about the library at `-o`, and there is no `-o`")?;
        let mut probe = Session::new();
        probe.options = options;
        return Ok(if library_is_fresh(&probe, &out, externs) {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    let emit = emit.unwrap_or_default();
    let Some(path) = inv.path.clone() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    };
    let source =
        std::fs::read_to_string(&path).map_err(|err| format!("cannot read {path}: {err}"))?;

    // Analyze the entry file (and everything it imports) through the session.
    let mut session = inv.prepare(Box::new(sema::session::FsLoader), options)?;
    let file = session.sources.add(path.clone(), source.clone());
    let (ast, parse_errors) = parser::parse::Parser::parse_file(&source, file);
    for err in parse_errors {
        session.error(file, err.span, err.message);
    }
    session.asts.insert(file, ast);
    sema::analyze(&mut session, file);

    if emit.ast {
        // The resolved, annotated tree, plus the whole-program def table.
        let text = format!(
            "{}\n{}",
            sema::pretty::tree_to_string(&session, file),
            sema::pretty::defs_to_string(&session)
        );
        dump(&emit, "ast", "", &text)?;
    }

    // The lowered IR of the entry file.
    if emit.ir && let Some(program) = session.ir.get(&file) {
        let text = ir::pretty::program_to_string(&session.defs, &session.ir_meta, program);
        dump(&emit, "ir", "\n===< IR >===\n", &text)?;
    }

    // The monomorphized whole program: every function concrete, every call with
    // a callee, every symbol decided. It is empty when analysis reported an
    // error, because monomorphization does not run on a program that does not
    // type-check.
    let type_checked = !session.linked.is_empty() && !session.has_errors();
    if emit.mono && type_checked {
        let text = ir::pretty::mono_to_string(&session.defs, &session.ir_meta, &session.linked);
        dump(&emit, "mono", "\n===< MONO >===\n", &text)?;
    }

    // The LIR: the same program as a graph. Locals up front, basic blocks,
    // explicit jumps, every aggregate flattened to a struct, every symbol
    // decided (`design/lir.md` §1). Like the mono dump it is empty when
    // analysis reported an error, for the same reason — and so, for the same
    // reason, is everything a backend would have been handed.
    if emit.nlib && !type_checked && !session.has_errors() {
        return Err(format!("{path} declares nothing to make a library of"));
    }
    if (emit.lir || emit.link || emit.nlib || !emit.backend.is_empty()) && type_checked {
        let layouts = ir::layout::Layouts::new(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            session.options.target,
        );
        // Which tests the entry point will run, when this is a test build. It is
        // asked here rather than inside the lowering because the answer is the
        // session's: which package a *file* belongs to is a thing only the thing
        // that loaded it knows.
        let tests = if session.options.test {
            session.entry_package_tests()
        } else {
            Vec::new()
        };
        let program = lir::lower::lower_against_libraries(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &layouts,
            &session.options,
            &session.lang_items,
            &session.sources,
            &tests,
            &|def| session.is_foreign_def(def),
        );
        if emit.lir {
            let text = lir::pretty::program_to_string(Some(&session.sources), &program);
            dump(&emit, "lir", "\n===< LIR >===\n", &text)?;
        }
        for kind in &emit.backend {
            // When a program is also being produced, `-o` names *it*: the
            // object and the executable would otherwise be written to one path,
            // and the second one would win silently.
            let named = emit.path(match kind {
                OutputKind::Object => "obj",
                OutputKind::Assembly => "asm",
                OutputKind::Ir => "backend-ir",
            });
            let (out, exact) = match named {
                Some(p) => (Some(p.to_path_buf()), true),
                None => (out.clone(), !emit.link),
            };
            match kind {
                // **One object, always.** How many codegen units a program was
                // split into is a fact about how it was *compiled*, not about
                // what it produces.
                OutputKind::Object => write_object(
                    backend.as_mut(),
                    &program,
                    out.as_deref(),
                    &path,
                    exact,
                    link_options,
                )?,
                // Assembly and the backend's IR stay one file per unit. They
                // are for reading, and two units' text concatenated is not the
                // assembly of anything — a real merge is what `ld -r` does, and
                // it does it to objects.
                kind => {
                    write_units(backend.as_mut(), &program, *kind, out.as_deref(), &path, exact)?;
                }
            }
        }
        if emit.nlib {
            write_library(
                &session,
                file,
                backend.as_mut(),
                &program,
                out.as_deref(),
                &path,
                inv.obj_dir.as_deref(),
            )?;
        }
        if emit.link {
            let libraries: Vec<PathBuf> =
                session.libraries.iter().map(|l| l.path.clone()).collect();
            link_program(
                backend.as_mut(),
                &program,
                out.as_deref(),
                &path,
                link_options,
                &libraries,
                inv.obj_dir.as_deref(),
            )?;
        }
    }

    // Warnings are printed too, and do not fail the build: a lint is evidence
    // that the author probably meant something else, not a claim that the
    // program is wrong.
    if !session.diagnostics.is_empty() {
        let color = color.enabled();
        for diag in &session.diagnostics {
            format.emit(diag, &session.sources, color);
        }
        // The count is for a person reading a terminal. A JSON stream is one
        // object per line and nothing else, so that a consumer can read it a
        // line at a time.
        if format == ErrorFormat::Human
            && let Some(summary) = summary(&session.diagnostics)
        {
            format.emit(&summary, &session.sources, color);
        }
    }
    Ok(if session.has_errors() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The line closing a run that reported something: how many errors and warnings,
/// and — when there were errors — that nothing was built.
fn summary(diagnostics: &[Diagnostic]) -> Option<Diagnostic> {
    let count = |severity| diagnostics.iter().filter(|d| d.severity == severity).count();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let (errors, warnings) = (count(Severity::Error), count(Severity::Warning));
    Some(match (errors, warnings) {
        (0, 0) => return None,
        (0, w) => Diagnostic::warning(format!("{} emitted", plural(w, "warning"))),
        (e, 0) => Diagnostic::error(format!("aborting due to {}", plural(e, "previous error"))),
        (e, w) => Diagnostic::error(format!(
            "aborting due to {}; {} emitted",
            plural(e, "previous error"),
            plural(w, "warning")
        )),
    })
}

/// Hand every unit to the backend, one file each.
///
/// The naming is the driver's, deliberately (`codegen::Codegen`): a backend is
/// told the path and writes to it, so which units exist and what they are called
/// never becomes something a backend has an opinion about. One unit with an
/// explicit `-o` writes exactly that path — which is what a build tool driving
/// one file at a time wants — and everything else appends the unit's name, since
/// several units sharing a path would be several units overwriting each other.
fn write_units(
    backend: &mut dyn Codegen,
    program: &lir::Program,
    kind: OutputKind,
    out: Option<&Path>,
    entry: &str,
    // Whether `-o` names this output exactly, or is a base to build a name
    // from. It does not when something else is claiming that path — a link.
    exact: bool,
) -> Result<Vec<PathBuf>, String> {
    let ext = backend.extension(kind);
    // With no `-o`, the entry file's stem: `foo.nest` produces `foo.o` beside
    // the invocation, which is what every other compiler does.
    let base = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(Path::new(entry).file_stem().unwrap_or_default()),
    };

    let mut written = Vec::with_capacity(program.units.len());
    for (i, unit) in program.units.iter().enumerate() {
        let path = if program.units.len() == 1 && out.is_some() {
            if exact {
                base.clone()
            } else {
                // The extension is **appended**, not replaced: `-o a.out` asks
                // for a file called `a.out`, and the object beside it is
                // `a.out.o` rather than a different `a.o`.
                let mut p = base.clone();
                let name = p.file_name().map(|s| s.to_string_lossy().into_owned());
                p.set_file_name(format!("{}.{ext}", name.unwrap_or_else(|| "out".into())));
                p
            }
        } else {
            let mut p = base.clone();
            // A unit is named after a source file and may hold characters a path
            // should not — `mem:main` is one this compiler produces — so the
            // name is reduced to what is safe. The index keeps two units whose
            // names reduce to the same thing apart.
            let safe: String = unit
                .name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect();
            let stem = p.file_name().map(|s| s.to_string_lossy().into_owned());
            p.set_file_name(format!(
                "{}.{i}.{safe}.{ext}",
                stem.unwrap_or_else(|| "out".into())
            ));
            p
        };
        backend
            .emit_unit(unit, kind, &path)
            .map_err(|e| format!("{}: {e}", backend.name()))?;
        written.push(path);
    }
    Ok(written)
}

/// Emit **one** object for the whole program, whatever it was split into.
///
/// With one unit that is one emission. With several it is several emissions into
/// a scratch directory and a partial link over them
/// ([`codegen::link::combine`]) — which is also the shape parallel code
/// generation wants, since the units are independent and only the merge is not.
pub(crate) fn write_object(
    backend: &mut dyn Codegen,
    program: &lir::Program,
    out: Option<&Path>,
    entry: &str,
    exact: bool,
    options: &codegen::link::LinkOptions,
) -> Result<(), String> {
    let ext = backend.extension(OutputKind::Object);
    let path = artifact_path(out, entry, exact, ext);

    if program.units.len() == 1 {
        return backend
            .emit_unit(&program.units[0], OutputKind::Object, &path)
            .map_err(|e| format!("{}: {e}", backend.name()));
    }

    let scratch = temp_dir(entry)?;
    let result = (|| {
        let mut objects = Vec::with_capacity(program.units.len());
        for (i, unit) in program.units.iter().enumerate() {
            let part = scratch.join(format!("{i}.{ext}"));
            backend
                .emit_unit(unit, OutputKind::Object, &part)
                .map_err(|e| format!("{}: {e}", backend.name()))?;
            objects.push(part);
        }
        codegen::link::combine(&objects, &path, options)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// Where a single-file output goes: `-o` when it names this artifact, `-o` plus
/// the extension when something else has claimed that name, and the entry
/// file's stem when there is no `-o` at all.
fn artifact_path(out: Option<&Path>, entry: &str, exact: bool, ext: &str) -> PathBuf {
    match out {
        Some(p) if exact => p.to_path_buf(),
        Some(p) => {
            let mut p = p.to_path_buf();
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned());
            p.set_file_name(format!("{}.{ext}", name.unwrap_or_else(|| "out".into())));
            p
        }
        None => {
            let stem = Path::new(entry).file_stem().unwrap_or_default();
            PathBuf::from(format!("{}.{ext}", stem.to_string_lossy()))
        }
    }
}

/// Compile to objects and link them into a program.
///
/// The objects go to a **temporary** directory and are deleted afterwards,
/// because they are not what was asked for: `--emit link,obj` is how a person
/// says they want to keep them, and it writes them where `-o` points like any
/// other emission. A build tool that wants to keep them passes `--obj-dir`, and
/// every object the link read stays there: the program's units, and each
/// library's, named after the library.
#[allow(clippy::too_many_arguments)]
fn link_program(
    backend: &mut dyn Codegen,
    program: &lir::Program,
    out: Option<&Path>,
    entry: &str,
    options: &codegen::link::LinkOptions,
    libraries: &[PathBuf],
    obj_dir: Option<&Path>,
) -> Result<(), String> {
    // A program starts at `main`, and without one the link fails deep inside the
    // C runtime's startup with a message about a symbol nobody wrote. The
    // compiler knows the answer already and can say it in the program's terms.
    if !program
        .units
        .iter()
        .any(|u| u.funcs.iter().any(|f| f.symbol.as_str() == "main"))
    {
        return Err(format!(
            "nothing to link: {entry} has no `main` at file scope, so there is nowhere for a program to start (§5.6).\nCompile it with `--emit obj` if it is a library"
        ));
    }

    // With no `-o`, the entry file's stem and no extension: `prog.nest` becomes
    // `prog`, which is what a person who just typed the file name wants back.
    let program_path = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(Path::new(entry).file_stem().unwrap_or_default()),
    };

    let scratch = Scratch::new(obj_dir, entry)?;
    let objects = write_units(
        backend,
        program,
        OutputKind::Object,
        Some(&scratch.dir.join(
            program_path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("out")),
        )),
        entry,
        // With the extension, since the objects may be kept beside others.
        false,
    );
    // Every library's objects join the program's: the code a library's
    // declarations here call is in them.
    let objects = objects.and_then(|mut objects| {
        for lib in libraries {
            let stem = lib.file_stem().unwrap_or_default().to_string_lossy();
            for (name, bytes) in library::archive::objects_of(lib)? {
                let path = scratch.dir.join(format!("{stem}.{name}"));
                std::fs::write(&path, bytes)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                objects.push(path);
            }
        }
        Ok(objects)
    });
    let result = objects.and_then(|objects| codegen::link::link(&objects, &program_path, options));
    scratch.finish();
    result
}

/// Whether the library at `out` is what compiling it again would produce: its
/// fingerprint, computed from the files it names and the libraries given as
/// they are now (`library::fingerprint`). Anything unreadable is stale.
fn library_is_fresh(session: &Session, out: &Path, externs: &[(String, PathBuf, bool)]) -> bool {
    let Ok(bytes) = library::archive::metadata_of(out) else {
        return false;
    };
    let Ok((header, _)) = library::read::header(&bytes) else {
        return false;
    };
    let settings = session.options.render();
    if header.settings != settings {
        return false;
    }
    let mut contents = Vec::with_capacity(header.inputs.len());
    for name in &header.inputs {
        match std::fs::read(name) {
            Ok(bytes) => contents.push((name.as_str(), bytes)),
            Err(_) => return false,
        }
    }
    let mut libraries: Vec<(String, u64, bool)> = Vec::new();
    for (_, path, importable) in externs {
        let Ok(bytes) = library::archive::metadata_of(path) else {
            return false;
        };
        let Ok((lib, _)) = library::read::header(&bytes) else {
            return false;
        };
        match libraries.iter_mut().find(|(n, ..)| *n == lib.name) {
            Some(existing) => existing.2 |= importable,
            None => libraries.push((lib.name, lib.fingerprint, *importable)),
        }
    }
    let target = session.target_module_source();
    let fingerprint = library::fingerprint::compute(&library::fingerprint::Inputs {
        compiler: &library::compiler_id(),
        target: &target,
        settings: &settings,
        package: &header.name,
        files: contents.iter().map(|(n, b)| (*n, b.as_slice())).collect(),
        libraries: libraries.iter().map(|(n, f, i)| (n.as_str(), *f, *i)).collect(),
    });
    fingerprint == header.fingerprint
}

/// Read every library given on the command line into `session`, each after the
/// libraries it was compiled against.
///
/// The order on the command line is not that order and does not have to be: the
/// headers say which packages each one names, and that is sorted here.
fn load_libraries(session: &mut Session, externs: &[(String, PathBuf, bool)]) -> Result<(), String> {
    let mut pending = Vec::with_capacity(externs.len());
    for (name, path, importable) in externs {
        let bytes = library::archive::metadata_of(path)?;
        let (header, _) = library::read::header(&bytes)
            .map_err(|e| format!("`{}`: {e}", path.display()))?;
        if &header.name != name {
            return Err(format!(
                "`{}` is the library `{}`, not `{name}`",
                path.display(),
                header.name
            ));
        }
        pending.push((header.packages, bytes, path, *importable));
    }
    while !pending.is_empty() {
        let ready = pending.iter().position(|(packages, ..)| {
            packages[1..]
                .iter()
                .all(|p| session.libraries.iter().any(|l| &l.name == p))
        });
        let Some(i) = ready else {
            let (packages, _, path, _) = &pending[0];
            let missing: Vec<&String> = packages[1..]
                .iter()
                .filter(|p| !session.libraries.iter().any(|l| &l.name == *p))
                .collect();
            return Err(format!(
                "`{}` was compiled against {}, which {} not given (with --extern or --indirect)",
                path.display(),
                missing.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", "),
                if missing.len() == 1 { "was" } else { "were" }
            ));
        };
        let (_, bytes, path, importable) = pending.remove(i);
        let ir = library::archive::ir_of(path)?;
        library::read::load(session, &bytes, &ir, path, importable)
            .map_err(|e| format!("`{}`: {e}", path.display()))?;
    }
    Ok(())
}

/// Write the entry package as a library: its metadata, its IR and its objects
/// in one archive.
///
/// `-o` names the archive; without one it is the package's name with `.nlib`
/// for its extension.
fn write_library(
    session: &Session,
    entry_file: common::source::FileId,
    backend: &mut dyn Codegen,
    program: &lir::Program,
    out: Option<&Path>,
    entry: &str,
    obj_dir: Option<&Path>,
) -> Result<(), String> {
    let Some(package) = session.pkg_of.get(&entry_file) else {
        return Err(format!(
            "{entry} is not a package's root, so there is no package to make a library of; \
             name it with `--package <name>={entry}`"
        ));
    };
    let (metadata, ir) = library::write::members(session, package, entry_file)?;
    let scratch = Scratch::new(obj_dir, entry)?;
    let objects = write_units(
        backend,
        program,
        OutputKind::Object,
        Some(&scratch.dir.join(package)),
        entry,
        false,
    );
    let archive = objects.and_then(|objects| {
        let mut members = vec![
            (library::archive::METADATA.to_string(), metadata),
            (library::archive::IR.to_string(), ir),
        ];
        for (i, object) in objects.iter().enumerate() {
            let bytes = std::fs::read(object)
                .map_err(|e| format!("cannot read {}: {e}", object.display()))?;
            members.push((format!("u{i}.o"), bytes));
        }
        library::archive::write(&members)
    });
    scratch.finish();
    let path = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(package).with_extension("nlib"),
    };
    std::fs::write(&path, archive?).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// A dump: to the file `--emit name=path` named, or to stdout after `header`.
fn dump(emit: &Emit, name: &str, header: &str, text: &str) -> Result<(), String> {
    match emit.path(name) {
        Some(path) => {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
        }
        None => {
            print!("{header}{text}");
            Ok(())
        }
    }
}

/// Where a link or a library writes the objects it is made from: the directory
/// `--obj-dir` named, kept, or one of this process's own, removed when done.
struct Scratch {
    dir: PathBuf,
    keep: bool,
}

impl Scratch {
    fn new(obj_dir: Option<&Path>, entry: &str) -> Result<Scratch, String> {
        match obj_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                Ok(Scratch { dir: dir.to_path_buf(), keep: true })
            }
            None => Ok(Scratch { dir: temp_dir(entry)?, keep: false }),
        }
    }

    /// Done with the objects. A temporary directory goes whether the work
    /// succeeded or not, and a failure to remove it is not a failure of the
    /// compilation: the objects are already written or already not.
    fn finish(self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// A directory of this process's own to put intermediate objects in.
fn temp_dir(entry: &str) -> Result<PathBuf, String> {
    let stem = Path::new(entry)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let safe: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let dir = std::env::temp_dir().join(format!("nestc-{}-{safe}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegen::OutputKind;

    /// A bare `nestc foo.nest` produces a **program**, like every other compiler
    /// a person invokes that way.
    #[test]
    fn the_default_is_a_linked_program() {
        let e = Emit::default();
        assert!(e.link);
        assert!(!e.ast && !e.ir && !e.mono && !e.lir);
        assert!(e.backend.is_empty());
    }

    /// An `--emit` list names **only** what it names: asking for the LIR is not
    /// also asking for a program, because the flag is what was asked for rather
    /// than what was added to the default.
    #[test]
    fn an_emit_list_replaces_the_default() {
        let e = Emit::parse("lir").expect("a list");
        assert!(e.lir);
        assert!(!e.link, "asking for a dump is not asking for a program too");
    }

    /// The backend outputs keep their order, because `--emit asm,obj` asks for
    /// two files and a person reading the second wants to know which is which.
    #[test]
    fn emit_keeps_the_order_of_backend_outputs() {
        let e = Emit::parse("link,asm,obj").expect("a list");
        assert!(e.link);
        assert_eq!(e.backend, vec![OutputKind::Assembly, OutputKind::Object]);
    }

    /// `-o` names the artifact when nothing else claims that name, and grows
    /// the extension when a link does. With no `-o` at all it is the entry
    /// file's stem, beside the invocation.
    #[test]
    fn an_artifact_path_follows_o() {
        let out = PathBuf::from("build/prog");
        assert_eq!(
            artifact_path(Some(&out), "src/prog.nest", true, "o"),
            PathBuf::from("build/prog")
        );
        // The extension is appended rather than replacing one: `-o a.out` asked
        // for a file called `a.out`.
        assert_eq!(
            artifact_path(Some(Path::new("a.out")), "src/prog.nest", false, "o"),
            PathBuf::from("a.out.o")
        );
        assert_eq!(
            artifact_path(None, "src/prog.nest", true, "o"),
            PathBuf::from("prog.o")
        );
    }

    /// A tool that asked for JSON gets the compiler's own failures in JSON too,
    /// including the one about an unparseable `--error-format` — which is the
    /// one case that has to fall back, since there is no format to print it in.
    #[test]
    fn the_error_format_is_a_fixed_list() {
        assert_eq!(ErrorFormat::parse("json"), Ok(ErrorFormat::Json));
        assert_eq!(ErrorFormat::parse("human"), Ok(ErrorFormat::Human));
        assert_eq!(ErrorFormat::default(), ErrorFormat::Human);
        let err = ErrorFormat::parse("short").expect_err("`short` is rustc's, not this one's");
        assert!(err.contains("`human` or `json`"), "{err}");
    }

    /// An unknown name is an **error**, for the same reason an unknown `-C` key
    /// is: it arrives from a build tool, and a typo would otherwise silently
    /// produce nothing.
    #[test]
    fn an_emit_kind_may_name_its_file() {
        let e = Emit::parse("link,ir=build/a.ir,backend-ir=build/a.ll").expect("a list");
        assert!(e.link && e.ir);
        assert_eq!(e.backend, vec![OutputKind::Ir]);
        assert_eq!(e.path("ir"), Some(Path::new("build/a.ir")));
        assert_eq!(e.path("backend-ir"), Some(Path::new("build/a.ll")));
        let err = Emit::parse("link=a").expect_err("a link follows `-o`");
        assert!(err.contains("takes no path"), "{err}");
    }

    #[test]
    fn an_unknown_emit_name_is_refused() {
        let err = Emit::parse("obj,exe").expect_err("`exe` is not a name here");
        assert!(err.contains("does not know `exe`"), "{err}");
    }
}
