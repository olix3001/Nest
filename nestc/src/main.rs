#![feature(box_patterns)]

pub(crate) mod common;
mod parser;

use std::process::ExitCode;

use common::diagnostic::simple_error;
use common::emitter::render;
use common::source::SourceMap;
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

    let mut sources = SourceMap::new();
    let file = sources.add(path.clone(), source);
    let src = &sources.file(file).unwrap().src;

    let (ast, errors) = Parser::parse_file(src, file);
    print!("{}", tree_to_string(&ast));

    if errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprintln!("\n{} parse error(s) in {path}:\n", errors.len());
        for err in &errors {
            let diag = simple_error(file, err.span, err.message.clone());
            eprint!("{}", render(&diag, &sources));
        }
        ExitCode::FAILURE
    }
}
