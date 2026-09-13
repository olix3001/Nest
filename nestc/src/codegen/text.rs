//! The backend that writes LIR out as text.
//!
//! It exists for two reasons, and neither is that anybody wants a `.lir` file.
//!
//! **It is the second implementation.** A trait with one implementation is a
//! trait shaped like that implementation; the way to find the places where
//! [`Codegen`] had accidentally assumed LLVM is to write a backend that is not
//! LLVM and see what does not fit. This one is small enough to be obviously
//! correct and different enough to be a real test of the shape.
//!
//! **It is what a build with no LLVM has.** Code generation is behind a feature
//! flag, because a compiler whose test suite cannot run without a system LLVM is
//! a compiler most contributors cannot build. With the flag off this is the only
//! backend, and `nestc` still resolves a target, still lays types out for it and
//! still runs every pass — it just cannot produce an object, and says so.
//!
//! It emits [`OutputKind::Ir`] only. "The backend's own intermediate form" is
//! LIR here, which is the joke and also the truth.

use std::path::Path;

use super::{Codegen, CodegenError, Endian, OutputKind, TargetInfo};
use crate::common::options::{ARCHES, OSES};
use crate::lir::Unit;

/// A backend that writes the LIR dump.
#[derive(Debug)]
pub struct TextBackend;

impl Codegen for TextBackend {
    fn name(&self) -> &'static str {
        "lir"
    }

    fn target_info(&self, triple: Option<&str>) -> Result<TargetInfo, CodegenError> {
        let triple = match triple {
            Some(t) => t.to_string(),
            None => host_triple(),
        };
        parse_triple(&triple)
    }

    fn emit_unit(
        &mut self,
        unit: &Unit,
        kind: OutputKind,
        out: &Path,
    ) -> Result<(), CodegenError> {
        if kind != OutputKind::Ir {
            return Err(CodegenError::Unsupported(format!(
                "the `lir` backend writes LIR text, not {}; \
                 build with `--features llvm` for an object file",
                kind.name()
            )));
        }
        // No source map: a `.lir` file is read on its own, and a span pointing
        // into a file the reader may not have is noise rather than context.
        std::fs::write(out, crate::lir::pretty::unit_to_string(None, unit))?;
        Ok(())
    }

    fn extension(&self, _kind: OutputKind) -> &'static str {
        "lir"
    }
}

/// The triple of the machine the compiler is running on.
///
/// Assembled from what the Rust compiler knew when *this* compiler was built,
/// which is the right source for it: it is the same machine.
fn host_triple() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    // The vendor field is the one nothing reads and every triple has. `apple`
    // for Darwin because the real spelling of that triple is load-bearing
    // elsewhere (a linker sees it), and `unknown` for everything else.
    let vendor = if os == "macos" { "apple" } else { "unknown" };
    let os = match os {
        "macos" => "darwin",
        other => other,
    };
    format!("{arch}-{vendor}-{os}")
}

/// Read a triple into the facts the front end needs.
///
/// A **small** parser, and deliberately not a good one: it recognizes the
/// architectures and systems this compiler has names for
/// ([`ARCHES`], [`OSES`]) and refuses everything else. The real resolution
/// belongs to a backend that has a target table — LLVM's — and this exists so
/// that a build without one can still say `--target wasm32-unknown-unknown` and
/// get a 32-bit machine rather than silently getting the host's 64.
fn parse_triple(triple: &str) -> Result<TargetInfo, CodegenError> {
    let parts: Vec<&str> = triple.split('-').collect();
    let bad = |what: &str| {
        CodegenError::Unsupported(format!(
            "the `lir` backend does not know the {what} in the target triple `{triple}`"
        ))
    };

    let arch = *ARCHES
        .iter()
        .find(|a| parts.first() == Some(&**a))
        .ok_or_else(|| bad("architecture"))?;

    // The OS is not at a fixed position — `x86_64-unknown-linux-gnu` and
    // `aarch64-apple-darwin` put it third and third, but `wasm32-wasi` puts it
    // second — so it is looked for rather than indexed.
    let os = parts
        .iter()
        .find_map(|p| match *p {
            // `darwin` is what a triple says and `macos` is what this compiler
            // calls it, which is the one spelling difference worth having.
            "darwin" | "macosx" | "macos" => Some("macos"),
            other => OSES.iter().copied().find(|o| *o == other),
        })
        .ok_or_else(|| bad("operating system"))?;

    let pointer_bits = match arch {
        "wasm32" => 32,
        _ => 64,
    };

    Ok(TargetInfo {
        triple: triple.to_string(),
        pointer_bits,
        // Every architecture this compiler names is little-endian. A big-endian
        // one is a row in `ARCHES` and a row here, and the reason [`Endian`]
        // exists before anything reads it.
        endian: Endian::Little,
        os,
        arch,
    })
}
