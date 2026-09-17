//! Builds `runtime/nest_runtime.c` into an archive `nestc` can link against.
//!
//! Every Nest program needs the runtime: the entry point calls `nest_init`, an
//! allocation calls `nest_alloc`, a trapped overflow calls `nest_trap`. So a
//! compiler that can link a program has to be able to *find* it, and the two
//! ways to arrange that are to ship it beside the binary or to build it here.
//! This is the second, because it keeps a from-source `cargo build` into a
//! working compiler with no install step.
//!
//! **A failure of the C compiler is not a build failure.** The C compiler is
//! needed to link a program, not to compile one, and a machine without it can
//! still build `nestc` and run its whole test suite. What it gets instead is an
//! unset `NEST_RUNTIME_LIB`, and a message from the driver naming `-C runtime=`
//! at the point where a link is actually attempted.
//!
//! **A missing collector is.** The runtime allocates from Boehm (bdwgc) and has
//! no other allocator, so a compiler that found a C compiler but no `gc.h`
//! would build programs that cannot be linked. It is looked for under
//! `BDW_GC_PREFIX`, then `pkg-config bdw-gc`, then `brew --prefix bdw-gc`, then
//! `/usr/local` and `/usr`. The library it found is handed to the linker as
//! `NEST_GC_LIB`: the static archive when there is one, so a linked program does
//! not need the collector installed to run.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = root.join("../runtime/nest_runtime.c");
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=BDW_GC_PREFIX");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("nest_runtime.o");
    let archive = out.join("libnest_runtime.a");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    if !Command::new(&cc).arg("--version").output().is_ok_and(|o| o.status.success()) {
        return;
    }
    let Some(gc) = find_gc() else {
        panic!(
            "the Boehm collector (bdwgc) was not found, and the Nest runtime needs it.\n\
             Install it (`brew install bdw-gc`, or your distribution's libgc-dev), \
             or set BDW_GC_PREFIX to the directory holding include/gc.h"
        );
    };
    let include = format!("-I{}", gc.include.display());
    if !run(&cc, &["-c", "-O2", "-fPIC", &include], &source, &object) {
        panic!("`{cc}` failed to compile {}", source.display());
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
        println!("cargo:rustc-env=NEST_GC_LIB={}", gc.lib);
    }
}

/// Where the collector is: the directory holding `gc.h`, and what to put on a
/// link line for the library, tab-separated — a path to `libgc.a`, or
/// `-L<dir>` and `-lgc` when only a shared library is installed. Linux adds
/// `-lpthread`, which a static `libgc.a` there needs and does not bring.
struct Gc {
    include: PathBuf,
    lib: String,
}

fn find_gc() -> Option<Gc> {
    let mut prefixes: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("BDW_GC_PREFIX") {
        prefixes.push(PathBuf::from(p));
    }
    if let Some(p) = output("pkg-config", &["--variable=prefix", "bdw-gc"]) {
        prefixes.push(PathBuf::from(p));
    }
    if let Some(p) = output("brew", &["--prefix", "bdw-gc"]) {
        prefixes.push(PathBuf::from(p));
    }
    prefixes.push(PathBuf::from("/usr/local"));
    prefixes.push(PathBuf::from("/usr"));

    for prefix in prefixes {
        let include = prefix.join("include");
        if !include.join("gc.h").exists() {
            continue;
        }
        let multiarch = output("cc", &["-print-multiarch"]).filter(|m| !m.is_empty());
        let mut dirs = vec![prefix.join("lib"), prefix.join("lib64")];
        if let Some(m) = multiarch {
            dirs.push(prefix.join("lib").join(m));
        }
        for dir in &dirs {
            let archive = dir.join("libgc.a");
            if archive.exists() {
                return Some(Gc { include, lib: with_threads(archive.display().to_string()) });
            }
        }
        for dir in &dirs {
            if ["libgc.dylib", "libgc.so"].iter().any(|l| dir.join(l).exists()) {
                return Some(Gc { include, lib: with_threads(format!("-L{}\t-lgc", dir.display())) });
            }
        }
    }
    None
}

fn with_threads(lib: String) -> String {
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => format!("{lib}\t-lpthread"),
        _ => lib,
    }
}

/// A tool's trimmed standard output, if it ran and succeeded.
fn output(tool: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(tool).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
