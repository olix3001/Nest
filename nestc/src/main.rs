#![feature(box_patterns)]

pub(crate) mod common;
mod ir;
mod parser;
mod sema;

use std::process::ExitCode;

use common::emitter::render;
use common::options::Options;
use sema::session::Session;

/// Usage, printed on a bad invocation.
///
/// This driver is a **scaffold**: it exists so the compiler can be run on a
/// file, and it prints the trees rather than producing an object. The real one
/// arrives with code generation. What is not scaffolding is `-C`: a build tool
/// hands the compiler its resolved settings, and that interface should be the
/// same one it will keep (see [`Options`]).
const USAGE: &str = "\
usage: nestc [-C key=value]... <file.nest>

settings (-C):
  overflow=trap|wrap     what a run-time integer overflow does (default: trap)
  pointer-width=16|32|64 the target's pointer width (default: 64)
  print=options          print the resolved settings and exit
";

/// Parse the file given as the first CLI argument and print its AST (or the
/// diagnostics). With no argument, print usage. Exits non-zero if parsing
/// reported any error.
fn main() -> ExitCode {
    let mut options = Options::default();
    let mut path = None;
    let mut print_options = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-C" => {
                let Some(setting) = args.next() else {
                    eprintln!("nestc: `-C` wants a `key=value` setting");
                    return ExitCode::FAILURE;
                };
                let Some((key, value)) = setting.split_once('=') else {
                    eprintln!("nestc: `-C {setting}` is not a `key=value` setting");
                    return ExitCode::FAILURE;
                };
                // `print` is the driver's, not the compiler's: it asks what the
                // settings resolved to, which is how a build tool checks that
                // the profile it translated arrived intact.
                if key == "print" && value == "options" {
                    print_options = true;
                    continue;
                }
                if let Err(err) = options.set(key, value) {
                    eprintln!("nestc: {err}");
                    return ExitCode::FAILURE;
                }
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => path = Some(other.to_string()),
        }
    }

    if print_options {
        print!("{}", options.render());
        return ExitCode::SUCCESS;
    }

    let Some(path) = path else {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    };

    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("nestc: cannot read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };

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

    // The resolved, annotated tree, plus the whole-program def table.
    print!("{}", sema::pretty::tree_to_string(&session, file));
    print!("\n{}", sema::pretty::defs_to_string(&session));

    // The lowered IR of the entry file.
    if let Some(program) = session.ir.get(&file) {
        print!(
            "\n===< IR >===\n{}",
            ir::pretty::program_to_string(&session.defs, &session.ir_meta, program)
        );
    }

    // The monomorphized whole program: every function concrete, every call with
    // a callee, every symbol decided. It is empty when analysis reported an
    // error, because monomorphization does not run on a program that does not
    // type-check.
    if !session.linked.is_empty() && !session.has_errors() {
        print!(
            "\n===< MONO >===\n{}",
            ir::pretty::mono_to_string(&session.defs, &session.ir_meta, &session.linked)
        );
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
    if !session.has_errors() {
        return ExitCode::SUCCESS;
    }
    ExitCode::FAILURE
}
