#![feature(box_patterns)]

pub(crate) mod common;
mod ir;
mod parser;
mod sema;

use std::process::ExitCode;

use common::emitter::render;
use sema::session::Session;

/// Parse the file given as the first CLI argument and print its AST (or the
/// diagnostics). With no argument, print usage. Exits non-zero if parsing
/// reported any error.
fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: nestc <file.nest>");
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
