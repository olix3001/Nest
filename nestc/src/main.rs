use std::process::ExitCode;

use nestc::common::diagnostic::Diagnostic;
use nestc::common::source::SourceMap;
use nestc::driver::{ColorChoice, ErrorFormat, run};

fn main() -> ExitCode {
    match run(std::env::args().skip(1)) {
        Ok(code) => code,
        Err(message) => {
            // The format is re-read from the arguments rather than handed back
            // out of `run`, because the failure may be the argument parsing
            // itself — and a tool that asked for JSON still wants this one in
            // JSON.
            let format = requested("--error-format")
                .and_then(|v| ErrorFormat::parse(&v).ok())
                .unwrap_or_default();
            let color = requested("--color")
                .and_then(|v| ColorChoice::parse(&v).ok())
                .unwrap_or_default()
                .enabled();
            // No file, so no labels: this is a failure *of* the compilation
            // rather than one found in a program.
            format.emit(&Diagnostic::error(message), &SourceMap::new(), color);
            ExitCode::FAILURE
        }
    }
}

/// `flag`'s value as the command line wrote it, for printing a failure that may
/// be the argument parsing itself — the message about *that* has to be printed
/// somehow.
fn requested(flag: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.strip_prefix(&format!("{flag}=")) {
            Some(v) => return Some(v.to_string()),
            None if arg == flag => return args.next(),
            None => {}
        }
    }
    None
}
