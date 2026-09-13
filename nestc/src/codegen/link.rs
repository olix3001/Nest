//! Turning object files into a program.
//!
//! **This is not a backend's job**, which is why it is here and not behind the
//! [`Codegen`](super::Codegen) trait. A backend answers what machine this is and
//! writes one object; what to do with several objects afterwards is the same
//! question on every target and is answered by a tool this compiler does not
//! ship. So `nestc` does what every other compiler that is not a linker does: it
//! invokes the platform's C compiler as the **linker driver**, because that is
//! the program that knows where `crt1.o` is, which libraries the system needs,
//! and what a dynamic loader on this platform is called.
//!
//! The runtime comes along with it. Every program calls `nest_init` before its
//! own `main` and `nest_alloc` on the way through, so the shim
//! (`runtime/nest_runtime.c`) is not optional and is not the program's to
//! remember — it is built beside the compiler (`build.rs`) and linked in here.
//! The **standard library** is the opposite and deliberately so: it is a package
//! a build tool resolves, and nothing here knows its name.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the runtime archive `build.rs` produced ended up, if it produced one.
const RUNTIME: Option<&str> = option_env!("NEST_RUNTIME_LIB");

/// What a link needs beyond the objects: the tool, the runtime, and whatever
/// the person invoking it knows that this compiler does not.
///
/// These are `-C` keys the **driver** reads rather than fields on
/// [`Options`](crate::common::options::Options), for the same reason `-C backend=`
/// is: no pass reads them. Nothing about the program being compiled changes
/// because a different linker will be run afterwards.
#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// `-C linker=` — the linker *driver*, `cc` by default.
    pub linker: String,
    /// `-C link-arg=` — one extra argument, repeatable, passed through in the
    /// order given. `-lgc` for a Boehm runtime, `-L`/`-l` for a C library a
    /// program declares with `extern("c")`.
    pub args: Vec<String>,
    /// `-C runtime=` — the runtime archive or object, overriding the one built
    /// beside the compiler. A cross-compilation needs this, because the runtime
    /// built here is for the host.
    pub runtime: Option<PathBuf>,
}

impl Default for LinkOptions {
    fn default() -> Self {
        LinkOptions {
            linker: "cc".to_string(),
            args: Vec::new(),
            runtime: None,
        }
    }
}

/// The runtime archive `build.rs` produced, if it produced one and it is still
/// there.
///
/// Public because a test that links a program has to be able to say "not on this
/// machine" rather than fail: the runtime needs a C compiler at build time, and
/// the compiler itself does not.
pub fn built_runtime() -> Option<PathBuf> {
    RUNTIME.map(PathBuf::from).filter(|p| p.exists())
}

impl LinkOptions {
    /// The runtime to link, or the reason there is none.
    ///
    /// A missing runtime is an error **here**, at the link, rather than at the
    /// build of the compiler: a `nestc` that cannot link is still a `nestc` that
    /// compiles, dumps and emits objects, and the machine that built it may not
    /// be the machine that needed a C compiler.
    fn runtime_path(&self) -> Result<PathBuf, String> {
        if let Some(p) = &self.runtime {
            return Ok(p.clone());
        }
        built_runtime().ok_or_else(|| {
            "no runtime to link: `nest_runtime.c` was not built beside this compiler.\n\
             Build it and pass it with `-C runtime=<path>`, or emit an object with \
             `--emit obj` and link it yourself"
                .to_string()
        })
    }
}

/// Link `objects` into an executable at `out`.
///
/// The linker's own output is **not** captured: a linker's diagnostics are
/// already addressed to a person, and rewrapping them in this compiler's voice
/// would only hide which tool actually failed.
pub fn link(objects: &[PathBuf], out: &Path, options: &LinkOptions) -> Result<(), String> {
    let runtime = options.runtime_path()?;
    let mut command = Command::new(&options.linker);
    command.args(objects).arg(&runtime).arg("-o").arg(out);
    command.args(&options.args);

    let status = command.status().map_err(|e| {
        format!(
            "cannot run the linker `{}`: {e}\nset another with `-C linker=<path>`",
            options.linker
        )
    })?;
    if !status.success() {
        return Err(format!("`{}` failed: {status}", options.linker));
    }
    Ok(())
}
