//! Builds `runtime/nest_runtime.c` into an archive `nestc` can link against.
//!
//! Every Nest program needs the runtime: the entry point calls `nest_init`, an
//! allocation calls `nest_alloc`, a trapped overflow calls `nest_trap`. So a
//! compiler that can link a program has to be able to *find* it, and the two
//! ways to arrange that are to ship it beside the binary or to build it here.
//! This is the second, because it keeps a from-source `cargo build` into a
//! working compiler with no install step.
//!
//! **A failure here is not a build failure.** The C compiler is needed to link a
//! program, not to compile one, and a machine without it can still build `nestc`
//! and run its whole test suite. What it gets instead is an unset
//! `NEST_RUNTIME_LIB`, and a message from the driver naming `-C runtime=` at the
//! point where a link is actually attempted.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = root.join("../runtime/nest_runtime.c");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=CC");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("nest_runtime.o");
    let archive = out.join("libnest_runtime.a");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    // `-O2` and no `-DNEST_GC_BOEHM`: the leaking allocator, which is the build
    // with no dependencies at all. Which collector is linked is a **link-time**
    // choice (`runtime/nest_runtime.c`), so choosing the one that needs nothing
    // installed is the only choice a build script can make for everyone.
    if !run(&cc, &["-c", "-O2", "-fPIC"], &source, &object) {
        return;
    }
    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let ok = Command::new(&ar)
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-env=NEST_RUNTIME_LIB={}", archive.display());
    }
}

fn run(cc: &str, flags: &[&str], source: &Path, object: &Path) -> bool {
    Command::new(cc)
        .args(flags)
        .arg(source)
        .arg("-o")
        .arg(object)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
