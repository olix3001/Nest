#![feature(box_patterns)]

pub(crate) mod common;
mod parser;

use std::process::ExitCode;

use parser::ast::FileId;
use parser::parse::Parser;
use parser::pretty::tree_to_string;

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

    let (ast, errors) = Parser::parse_file(&source, FileId(0));
    print!("{}", tree_to_string(&ast));

    if errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprintln!("\n{} parse error(s) in {path}:", errors.len());
        for err in &errors {
            eprintln!("  {}..{}: {}", err.span.start, err.span.end, err.message);
        }
        ExitCode::FAILURE
    }
}
