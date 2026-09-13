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
use common::emitter::render;
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
                           ast, ir, mono, lir   the compiler's own dumps, to stdout
                           obj, asm, backend-ir what the backend writes, to files
                         (default: ast,ir,mono,lir)
  -C <key>=<value>       a build setting; see below
  -h, --help             this

settings (-C):
  backend=<name>         which code generator (default: the first compiled in)
  codegen-units=N        how many codegen units the program is split into
                         (default: 1 — the whole program in one)
  overflow=trap|wrap     what a run-time integer overflow does (default: trap)
  pointer-width=16|32|64 override the target's pointer width
  os=<name>              override the target's operating system
  arch=<name>            override the target's architecture
  profile=debug|release  the build profile's name, readable from source
  print=options          print the resolved settings and exit
";

/// What was asked for, parsed.
///
/// The default is **the dumps**, not an object, and that is a statement about
/// today rather than a design: the only backend compiled in cannot produce an
/// object, so defaulting to one would make every bare `nestc foo.nest` an error.
/// The line to change when that stops being true is in [`Emit::default`].
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
}

impl Default for Emit {
    fn default() -> Self {
        Emit {
            ast: true,
            ir: true,
            mono: true,
            lir: true,
            backend: Vec::new(),
        }
    }
}

impl Emit {
    /// Parse an `--emit` list. Every name is checked, and an unknown one is an
    /// error for the same reason an unknown `-C` key is.
    fn parse(list: &str) -> Result<Emit, String> {
        let mut e = Emit {
            ast: false,
            ir: false,
            mono: false,
            lir: false,
            backend: Vec::new(),
        };
        for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "ast" => e.ast = true,
                "ir" => e.ir = true,
                "mono" => e.mono = true,
                "lir" => e.lir = true,
                "obj" => e.backend.push(OutputKind::Object),
                "asm" => e.backend.push(OutputKind::Assembly),
                "backend-ir" => e.backend.push(OutputKind::Ir),
                other => {
                    return Err(format!(
                        "`--emit` does not know `{other}`; it takes ast, ir, mono, lir, obj, asm, backend-ir"
                    ));
                }
            }
        }
        Ok(e)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("nestc: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The compilation, with every failure a message rather than an exit.
fn run() -> Result<ExitCode, String> {
    let mut path: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut triple: Option<String> = None;
    let mut backend_name: Option<String> = None;
    let mut emit: Option<Emit> = None;
    let mut print_options = false;
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
    if (emit.lir || !emit.backend.is_empty()) && type_checked {
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
            write_units(backend.as_mut(), &program, *kind, out.as_deref(), &path)?;
        }
    }

    // Warnings are printed too, and do not fail the build: a lint is evidence
    // that the author probably meant something else, not a claim that the
    // program is wrong.
    if !session.diagnostics.is_empty() {
        eprintln!("\n{} diagnostic(s):\n", session.diagnostics.len());
        for diag in &session.diagnostics {
            eprint!("{}", render(diag, &session.sources));
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
) -> Result<(), String> {
    let ext = backend.extension(kind);
    // With no `-o`, the entry file's stem: `foo.nest` produces `foo.o` beside
    // the invocation, which is what every other compiler does.
    let base = match out {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(Path::new(entry).file_stem().unwrap_or_default()),
    };

    for (i, unit) in program.units.iter().enumerate() {
        let path = if program.units.len() == 1 && out.is_some() {
            base.clone()
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
    }
    Ok(())
}
