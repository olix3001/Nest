//! A record of what the server was asked and what it did, when `NEST_LSP_LOG`
//! names a file to write it to.
//!
//! An editor shows what a server answered, not why it took as long as it did or
//! which analysis an answer came out of. The questions this answers are the ones
//! asked of a server that behaves differently in an editor than it does when
//! driven by hand: which messages arrived, whether completion read an analysis
//! or ran one, and how long a person waited.
//!
//! It is off unless the variable is set, and writing to it costs a line of text
//! per message.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Open the log, if `NEST_LSP_LOG` says where. Later calls do nothing.
pub fn open() {
    FILE.get_or_init(|| {
        let path = std::env::var("NEST_LSP_LOG").ok()?;
        let file = File::create(&path)
            .map_err(|e| eprintln!("nest-lsp: cannot write the log at `{path}`: {e}"))
            .ok()?;
        eprintln!("nest-lsp: logging to `{path}`");
        Some(Mutex::new(file))
    });
    let _ = START.get_or_init(Instant::now);
}

/// Whether anything is written at all, so that a caller can skip the work of
/// describing what it did.
pub fn on() -> bool {
    FILE.get().is_some_and(Option::is_some)
}

/// Write one line, stamped with the seconds since the server started.
pub fn write(what: std::fmt::Arguments<'_>) {
    let Some(Some(file)) = FILE.get() else { return };
    let at = START.get().map_or(0.0, |s| s.elapsed().as_secs_f64());
    let Ok(mut file) = file.lock() else { return };
    let _ = writeln!(file, "{at:9.3} {what}");
    let _ = file.flush();
}

/// `log::line!("...", ...)`, evaluated only when the log is open.
macro_rules! line {
    ($($arg:tt)*) => {
        if crate::log::on() {
            crate::log::write(format_args!($($arg)*));
        }
    };
}

pub(crate) use line;
