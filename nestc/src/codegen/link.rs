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

/// The collector the runtime was compiled against, as tab-separated link-line
/// arguments: `libgc.a`, or `-L<dir>` and `-lgc`. It comes after the runtime on
/// every link, `-C runtime=` included — an archive only contributes what
/// something still needs, so a runtime that does not call Boehm pulls in none of
/// it.
const GC: Option<&str> = option_env!("NEST_GC_LIB");

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
    /// `-C partial-linker=` — the tool that merges several objects into one
    /// (`ld -r`, a *partial* or *relocatable* link). It is not the same tool as
    /// `linker`: that one is a C compiler being asked to find `crt1.o` and the
    /// system libraries, and this one must do no such thing.
    pub partial_linker: String,
    /// `-C link-arg=` — one extra argument, repeatable, passed through in the
    /// order given, for whatever this compiler has no spelling of its own for.
    pub args: Vec<String>,
    /// `-l <name>` — a C library to link against, by the name the linker knows
    /// it by: `-l m`, `-lsfml-graphics`. What a program that declares a symbol
    /// `extern("c")` needs, so it has a spelling of its own rather than an
    /// argument passed blindly through.
    pub libs: Vec<String>,
    /// `--link-search <dir>` — a directory to look for those libraries in.
    ///
    /// Not `-L`, which this compiler already spends on *package* search paths.
    /// The name is the one a build script's `twig:link-search` directive will
    /// carry, so the two agree.
    pub search: Vec<PathBuf>,
    /// `-C runtime=` — the runtime archive or object, overriding the one built
    /// beside the compiler. A cross-compilation needs this, because the runtime
    /// built here is for the host.
    pub runtime: Option<PathBuf>,
}

impl Default for LinkOptions {
    fn default() -> Self {
        LinkOptions {
            linker: "cc".to_string(),
            partial_linker: "ld".to_string(),
            args: Vec::new(),
            libs: Vec::new(),
            search: Vec::new(),
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
    crate::common::install::shipped("libnest_runtime.a")
        .or_else(|| RUNTIME.map(PathBuf::from).filter(|p| p.exists()))
}

/// The collector's link-line arguments: the `libgc.a` an installed compiler
/// ships (with `-lpthread` on Linux, which a static one needs), or what
/// `build.rs` found.
fn gc_args() -> Vec<String> {
    if let Some(lib) = crate::common::install::shipped("libgc.a") {
        let mut args = vec![lib.to_string_lossy().into_owned()];
        if cfg!(target_os = "linux") {
            args.push("-lpthread".to_string());
        }
        return args;
    }
    GC.into_iter()
        .flat_map(|l| l.split('\t'))
        .map(str::to_string)
        .collect()
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

/// Merge several objects into one at `out`, with a **partial link**.
///
/// A codegen unit is a unit of *work* (§11), not an artifact: splitting a
/// program into four is how four cores compile it, and nothing downstream
/// should have to learn that a program is four files today and three tomorrow.
/// So `--emit obj` produces one object however many units were used, and this is
/// what makes that true — `ld -r`, which resolves what it can between the inputs
/// and leaves the rest for the real link.
pub fn combine(objects: &[PathBuf], out: &Path, options: &LinkOptions) -> Result<(), String> {
    let status = Command::new(&options.partial_linker)
        .arg("-r")
        .args(objects)
        .arg("-o")
        .arg(out)
        .status()
        .map_err(|e| {
            format!(
                "cannot run `{}` to merge {} objects: {e}\nset another with `-C partial-linker=<path>`",
                options.partial_linker,
                objects.len()
            )
        })?;
    if !status.success() {
        return Err(format!("`{} -r` failed: {status}", options.partial_linker));
    }
    Ok(())
}

/// Warnings the linker emits that nobody reading them can act on.
///
/// Apple's linker reports the `REFERENCED_DYNAMICALLY` bit as deprecated for
/// three Mach exception handlers — `_catch_exception_raise` and its two
/// neighbours — which Boehm sets with a `.desc` directive in its own source.
/// They are in the collector's archive, they are printed **once per link** on
/// macOS, and a Nest program's link is where they surface. There is nothing to
/// fix here and nothing to fix there; the only thing they do is bury a real
/// warning under three that are not.
///
/// This is a *line* filter and not `-Wl,-w`: every other thing the linker has
/// to say still reaches the person who ran it.
const MUFFLED: [&str; 1] = ["REFERENCED_DYNAMICALLY flag on symbol"];

/// Link `objects` into an executable at `out`.
///
/// The linker's own output is **not** rewrapped in this compiler's voice: a
/// linker's diagnostics are already addressed to a person, and restating them
/// would only hide which tool actually failed. It is passed through line by
/// line rather than inherited only so that [`MUFFLED`] can be dropped.
pub fn link(objects: &[PathBuf], out: &Path, options: &LinkOptions) -> Result<(), String> {
    let runtime = options.runtime_path()?;
    let mut command = Command::new(&options.linker);
    command.args(objects).arg(&runtime);
    command.args(gc_args());
    command.arg("-o").arg(out);
    command.args(&options.args);
    // After the objects, which is where a linker that resolves left to right
    // wants them: a library is searched for the symbols the objects before it
    // left undefined.
    for dir in &options.search {
        command.arg("-L").arg(dir);
    }
    for lib in &options.libs {
        command.arg(format!("-l{lib}"));
    }

    let done = command.output().map_err(|e| {
        format!(
            "cannot run the linker `{}`: {e}\nset another with `-C linker=<path>`",
            options.linker
        )
    })?;
    say(&done.stdout, &mut std::io::stdout());
    say(&done.stderr, &mut std::io::stderr());
    if !done.status.success() {
        return Err(format!("`{}` failed: {}", options.linker, done.status));
    }
    Ok(())
}

/// Write what the linker said, without the lines that say nothing.
///
/// A line the filter does not recognize is written **as bytes**: a linker names
/// files, and a path is not required to be UTF-8.
fn say(said: &[u8], to: &mut impl std::io::Write) {
    for line in said.split_inclusive(|b| *b == b'\n') {
        let text = String::from_utf8_lossy(line);
        if MUFFLED.iter().any(|m| text.contains(m)) {
            continue;
        }
        let _ = to.write_all(line);
    }
    let _ = to.flush();
}
