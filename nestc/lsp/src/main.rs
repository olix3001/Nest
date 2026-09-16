//! The Nest language server, over stdin and stdout.
//!
//! twig is found at `--twig`, then where the `twig` initialization option or
//! `NEST_TWIG` says, then on `PATH`. `--nestc` (or the `nestc` option) is the
//! compiler twig runs, which otherwise it finds the way it always does.

mod analysis;
mod complete;
mod ide;
mod server;
mod workspace;

use std::process::ExitCode;
use std::sync::Arc;

use lsp_server::Connection;

use workspace::{Toolchain, Twig};

const USAGE: &str = "\
usage: nest-lsp [options]

Speaks the Language Server Protocol on stdin and stdout.

options:
  --twig <path>   the twig to prepare workspaces with (default: `twig` on PATH)
  --nestc <path>  the nestc twig compiles with (default: twig's own choice)
  -h, --help      this
";

/// `path` with a leading `~/` written as the home directory, which nothing
/// between a settings file and `exec` would do otherwise.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

fn main() -> ExitCode {
    let mut twig: Option<String> = None;
    eprintln!(
        "nest-lsp: started with {:?}",
        std::env::args().skip(1).collect::<Vec<_>>()
    );
    let mut nestc: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--twig" => &mut twig,
            "--nestc" => &mut nestc,
            other => {
                eprint!("nest-lsp: unknown argument `{other}`\n{USAGE}");
                return ExitCode::FAILURE;
            }
        };
        match args.next() {
            Some(value) => *slot = Some(value),
            None => {
                eprintln!("nest-lsp: `{arg}` wants a path");
                return ExitCode::FAILURE;
            }
        }
    }

    let (conn, io) = Connection::stdio();
    let result = server::run(&conn, |options| -> Arc<dyn Toolchain> {
        let option = |name: &str| {
            options
                .get(name)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let program = twig
            .or_else(|| option("twig"))
            .or_else(|| std::env::var("NEST_TWIG").ok().filter(|p| !p.is_empty()))
            .unwrap_or_else(|| "twig".to_string());
        let nestc = nestc.or_else(|| option("nestc")).map(|p| expand_home(&p));
        let program = expand_home(&program);
        // Stderr is where an editor's language server log shows it.
        eprintln!(
            "nest-lsp: twig is `{program}`, nestc is `{}`",
            nestc.as_deref().unwrap_or("twig's own")
        );
        Arc::new(Twig { program, nestc })
    });
    drop(conn);
    let joined = io.join();
    match result.and(joined.map_err(|e| e.to_string())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("nest-lsp: {why}");
            ExitCode::FAILURE
        }
    }
}
