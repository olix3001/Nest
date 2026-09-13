#![feature(box_patterns)]

pub(crate) mod common;
mod codegen;
mod ir;
mod lir;
mod parser;
mod sema;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use codegen::{Codegen, OutputKind};
use common::diagnostic::Diagnostic;
use common::emitter::{render, render_json};
use common::options::Options;
use sema::session::Session;

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
const USAGE: &str = "\
usage: nestc [options] <file.nest>

options:
  -o <path>              where to write the output; with several codegen units,
                         the base that each unit's name is appended to
  --target <triple>      the machine to generate code for (default: the host)
  --emit <list>          comma-separated, any of:
                           link                 an executable, linked (default)
                           ast, ir, mono, lir   the compiler's own dumps, to stdout
                           obj, asm, backend-ir what the backend writes, to files
  -L <dir>               a directory to search for packages; `<foo/...>` is
                         <dir>/foo/foo.nest. Repeatable, in order
  --error-format <form>  human (default) or json — one JSON object per line,
                         on stderr, for a tool that consumes them
  -C <key>=<value>       a build setting; see below
  -h, --help             this

settings (-C):
  backend=<name>         which code generator (default: the first compiled in)
  codegen-units=N        how many codegen units the program is split into
                         (default: 1 — the whole program in one)
  entry=auto|none        synthesize a C `main` calling the program's `main`
                         when it has one (default: auto)
  linker=<path>          the linker driver used by `--emit link` (default: cc)
  link-arg=<arg>         one more argument for it; repeatable, in order
  runtime=<path>         the runtime archive to link, overriding the one built
                         beside this compiler
  overflow=trap|wrap     what a run-time integer overflow does (default: trap)
  pointer-width=16|32|64 override the target's pointer width
  os=<name>              override the target's operating system
  arch=<name>            override the target's architecture
  profile=debug|release  the build profile's name, readable from source
  print=options          print the resolved settings and exit
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
        }
    }

    fn parse(list: &str) -> Result<Emit, String> {
        let mut e = Emit::nothing();
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "ast" => e.ast = true,
                "ir" => e.ir = true,
                "mono" => e.mono = true,
                "lir" => e.lir = true,
                "obj" => e.backend.push(OutputKind::Object),
                "asm" => e.backend.push(OutputKind::Assembly),
                "backend-ir" => e.backend.push(OutputKind::Ir),
                "link" => e.link = true,
                other => {
                    return Err(format!(
                        "`--emit` does not know `{other}`; it takes link, ast, ir, mono, lir, obj, asm, backend-ir"
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
enum ErrorFormat {
    #[default]
    Human,
    Json,
}

impl ErrorFormat {
    fn parse(value: &str) -> Result<ErrorFormat, String> {
        match value {
            "human" => Ok(ErrorFormat::Human),
            "json" => Ok(ErrorFormat::Json),
            other => Err(format!(
                "`--error-format` takes `human` or `json`, not `{other}`"
            )),
        }
    }

    /// Write one diagnostic to stderr.
    fn emit(self, diag: &Diagnostic, sources: &common::source::SourceMap) {
        match self {
            ErrorFormat::Human => eprint!("{}", render(diag, sources)),
            ErrorFormat::Json => eprint!("{}", render_json(diag, sources)),
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            // The format is re-read from the arguments rather than handed back
            // out of `run`, because the failure may be the argument parsing
            // itself — and a tool that asked for JSON still wants this one in
            // JSON.
            match requested_format() {
                ErrorFormat::Human => eprintln!("nestc: {message}"),
                // No file, so no labels: this is a failure *of* the compilation
                // rather than one found in a program.
                format => format.emit(
                    &Diagnostic::error(message),
                    &common::source::SourceMap::new(),
                ),
            }
            ExitCode::FAILURE
        }
    }
}

/// `--error-format` as the command line asked for it, defaulting on anything
/// unparseable — the message about *that* has to be printed somehow.
fn requested_format() -> ErrorFormat {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--error-format=") {
            Some(v) => Some(v.to_string()),
            None if arg == "--error-format" => args.next(),
            None => None,
        };
        if let Some(v) = value {
            return ErrorFormat::parse(&v).unwrap_or_default();
        }
    }
    ErrorFormat::Human
}

/// The compilation, with every failure a message rather than an exit.
fn run() -> Result<ExitCode, String> {
    let mut path: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut triple: Option<String> = None;
    let mut backend_name: Option<String> = None;
    let mut emit: Option<Emit> = None;
    let mut print_options = false;
    let mut format = ErrorFormat::default();
    // Where to look for a package nothing registered. The driver holds them
    // rather than `Options` because they are about finding source, not about
    // what is built from it.
    let mut search_paths: Vec<String> = Vec::new();
    // The link's settings, which are the driver's for the same reason `backend`
    // is: no pass reads them, and what is compiled does not change because a
    // different linker will run afterwards.
    let mut link_options = codegen::link::LinkOptions::default();
    // The `-C` settings are collected rather than applied, because some of them
    // **override** what the backend is about to report about the machine and an
    // override has to be applied second (see below).
    let mut settings: Vec<(String, String)> = Vec::new();

    let mut args = std::env::args().skip(1);
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
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
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
                    "print" => return Err(format!("`print` takes `options`, not `{val}`")),
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

    // **The backend answers first.** The layout engine and `core`'s generated
    // `target.nest` both read the target, and both run long before code is
    // generated, so the machine's facts have to be settled before analysis
    // starts — from the thing that will actually generate the code rather than
    // from a default that could disagree with it.
    let mut backend = codegen::select(backend_name.as_deref())?;
    let info = backend
        .target_info(triple.as_deref())
        .map_err(|e| e.to_string())?;

    let mut options = Options {
        target: info.target(),
        ..Options::default()
    };
    // And the `-C` overrides land on top, which is the order that makes them
    // overrides: `-C pointer-width=32` is a person saying they know better than
    // the triple, and it is allowed to.
    for (key, value) in &settings {
        options.set(key, value)?;
    }

    if print_options {
        print!("{}", options.render());
        return Ok(ExitCode::SUCCESS);
    }

    let emit = emit.unwrap_or_default();
    let Some(path) = path else {
        eprint!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    };
    let source =
        std::fs::read_to_string(&path).map_err(|err| format!("cannot read {path}: {err}"))?;

    // Analyze the entry file (and everything it imports) through the session.
    let mut session = Session::new();
    session.options = options;
    // Before anything is loaded: a package is found the first time it is
    // imported, and `core` is imported by the prelude.
    for dir in &search_paths {
        session.add_search_path(dir);
    }
    let file = session.sources.add(path.clone(), source.clone());
    let (ast, parse_errors) = parser::parse::Parser::parse_file(&source, file);
    for err in parse_errors {
        session.error(file, err.span, err.message);
    }
    session.asts.insert(file, ast);
    sema::analyze(&mut session, file);

    if emit.ast {
        // The resolved, annotated tree, plus the whole-program def table.
        print!("{}", sema::pretty::tree_to_string(&session, file));
        print!("\n{}", sema::pretty::defs_to_string(&session));
    }

    // The lowered IR of the entry file.
    if emit.ir && let Some(program) = session.ir.get(&file) {
        print!(
            "\n===< IR >===\n{}",
            ir::pretty::program_to_string(&session.defs, &session.ir_meta, program)
        );
    }

    // The monomorphized whole program: every function concrete, every call with
    // a callee, every symbol decided. It is empty when analysis reported an
    // error, because monomorphization does not run on a program that does not
    // type-check.
    let type_checked = !session.linked.is_empty() && !session.has_errors();
    if emit.mono && type_checked {
        print!(
            "\n===< MONO >===\n{}",
            ir::pretty::mono_to_string(&session.defs, &session.ir_meta, &session.linked)
        );
    }

    // The LIR: the same program as a graph. Locals up front, basic blocks,
    // explicit jumps, every aggregate flattened to a struct, every symbol
    // decided (`design/lir.md` §1). Like the mono dump it is empty when
    // analysis reported an error, for the same reason — and so, for the same
    // reason, is everything a backend would have been handed.
    if (emit.lir || emit.link || !emit.backend.is_empty()) && type_checked {
        let layouts = ir::layout::Layouts::new(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            session.options.target,
        );
        let program = lir::lower(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &layouts,
            &session.options,
            &session.lang_items,
            &session.sources,
        );
        if emit.lir {
            print!(
                "\n===< LIR >===\n{}",
                lir::pretty::program_to_string(Some(&session.sources), &program)
            );
        }
        for kind in &emit.backend {
            // When a program is also being produced, `-o` names *it*: the
            // object and the executable would otherwise be written to one path,
            // and the second one would win silently.
            write_units(
                backend.as_mut(),
                &program,
                *kind,
                out.as_deref(),
                &path,
                !emit.link,
            )?;
        }
        if emit.link {
            link_program(
                backend.as_mut(),
                &program,
                out.as_deref(),
                &path,
                &link_options,
            )?;
        }
    }

    // Warnings are printed too, and do not fail the build: a lint is evidence
    // that the author probably meant something else, not a claim that the
    // program is wrong.
    if !session.diagnostics.is_empty() {
        // The count and the blank lines are for a person reading a terminal. A
        // JSON stream is one object per line and nothing else, so that a
        // consumer can read it a line at a time.
        if format == ErrorFormat::Human {
            eprintln!("\n{} diagnostic(s):\n", session.diagnostics.len());
        }
        for diag in &session.diagnostics {
            format.emit(diag, &session.sources);
        }
    }
    Ok(if session.has_errors() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
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

/// Compile to objects and link them into a program.
///
/// The objects go to a **temporary** directory and are deleted afterwards,
/// because they are not what was asked for: `--emit link,obj` is how a person
/// says they want to keep them, and it writes them where `-o` points like any
/// other emission. A build tool that wants to cache them asks for them.
fn link_program(
    backend: &mut dyn Codegen,
    program: &lir::Program,
    out: Option<&Path>,
    entry: &str,
    options: &codegen::link::LinkOptions,
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

    let scratch = temp_dir(entry)?;
    let objects = write_units(
        backend,
        program,
        OutputKind::Object,
        Some(&scratch.join(
            program_path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("out")),
        )),
        entry,
        true,
    );
    let result = objects.and_then(|objects| codegen::link::link(&objects, &program_path, options));
    // The scratch directory goes whether the link worked or not, and a failure
    // to remove it is not a failure of the compilation: the object files are
    // already written or already not.
    let _ = std::fs::remove_dir_all(&scratch);
    result
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
    fn an_unknown_emit_name_is_refused() {
        let err = Emit::parse("obj,exe").expect_err("`exe` is not a name here");
        assert!(err.contains("does not know `exe`"), "{err}");
    }
}
