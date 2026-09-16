//! The Nest language server, over stdin and stdout.
//!
//! It finds twig on `PATH`, or where the `twig` initialization option or
//! `NEST_TWIG` says, and twig finds `nestc` the way it always does.

mod analysis;
mod server;
mod workspace;

use std::process::ExitCode;
use std::sync::Arc;

use lsp_server::Connection;

use workspace::{Toolchain, Twig};

fn main() -> ExitCode {
    let (conn, io) = Connection::stdio();
    let result = server::run(&conn, |options| -> Arc<dyn Toolchain> {
        let program = options
            .get("twig")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| std::env::var("NEST_TWIG").ok().filter(|p| !p.is_empty()))
            .unwrap_or_else(|| "twig".to_string());
        Arc::new(Twig { program })
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
