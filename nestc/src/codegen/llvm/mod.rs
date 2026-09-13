//! The LLVM backend.
//!
//! It answers the two questions [`Codegen`] asks — what machine is this, and
//! emit this unit — and everything hard about it is in [`unit`], which walks one
//! [`Unit`] and builds a module.
//!
//! ## Behind a feature flag
//!
//! `--features llvm`, and off by default. A compiler whose test suite cannot run
//! without a system LLVM is a compiler most people cannot build, and everything
//! before code generation — parsing, inference, monomorphization, layout, the
//! whole of LIR — is testable without one. What the flag buys is the last step.
//!
//! ## LLVM is what says how wide a pointer is
//!
//! [`target_info`](LlvmBackend::target_info) resolves a triple through LLVM's
//! own target registry and reports the data layout's answer. A table in this
//! compiler mapping `aarch64-apple-darwin` to 64 would be a second source of
//! truth, and the one that matters is the one that will lay out the code.

use std::path::Path;
use std::sync::Once;

use inkwell::OptimizationLevel;
use inkwell::context::Context;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};

use super::{Codegen, CodegenError, Endian, OutputKind, TargetInfo};
use crate::common::options::{ARCHES, OSES};
use crate::lir::Unit;

mod unit;

/// Code generation through LLVM.
#[derive(Debug, Default)]
pub struct LlvmBackend {
    /// The triple [`Codegen::target_info`] resolved, which every later
    /// [`Codegen::emit_unit`] generates for. `None` until it is asked, which
    /// only happens in a test that emits without resolving first.
    triple: Option<String>,
}

/// LLVM's target registry is global and initializing it twice is not defined.
/// Every entry point below goes through here first.
fn initialize() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| Target::initialize_all(&InitializationConfig::default()));
}

/// The [`TargetMachine`] for a triple, which is what every question below is
/// really asked of.
///
/// The three settings are the ones a first backend has no reason to vary.
/// `RelocMode::PIC` because every current system links position-independent
/// executables and a build tool should not have to ask; `CodeModel::Default`
/// because LLVM picks the right one per target; and `OptimizationLevel::None`
/// because optimization is a `-C opt-level` this compiler does not have yet, and
/// a backend that silently optimized would make the first debugging session
/// harder than it needs to be.
fn machine(triple: &str) -> Result<TargetMachine, CodegenError> {
    initialize();
    let triple = TargetTriple::create(triple);
    let target = Target::from_triple(&triple).map_err(|e| {
        CodegenError::Unsupported(format!(
            "LLVM does not know the target triple `{}`: {e}",
            triple.as_str().to_string_lossy()
        ))
    })?;
    target
        .create_target_machine(
            &triple,
            "generic",
            "",
            OptimizationLevel::None,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| {
            CodegenError::Failed(format!(
                "LLVM has no target machine for `{}`",
                triple.as_str().to_string_lossy()
            ))
        })
}

impl Codegen for LlvmBackend {
    fn name(&self) -> &'static str {
        "llvm"
    }

    fn target_info(&mut self, triple: Option<&str>) -> Result<TargetInfo, CodegenError> {
        initialize();
        let triple = match triple {
            Some(t) => t.to_string(),
            None => TargetMachine::get_default_triple()
                .as_str()
                .to_string_lossy()
                .into_owned(),
        };
        let machine = machine(&triple)?;
        let data = machine.get_target_data();

        // The pointer width and the byte order both come off the data layout,
        // which is LLVM's own statement about the machine. The layout string
        // starts with `e` for little-endian and `E` for big.
        let pointer_bits = data.get_pointer_byte_size(None) * 8;
        let layout = data.get_data_layout();
        let endian = if layout.as_str().to_string_lossy().starts_with('E') {
            Endian::Big
        } else {
            Endian::Little
        };

        // The OS and the architecture, on the other hand, are names that reach
        // **source**: `core`'s generated `target.nest` declares `OS: Os :: .macos`
        // and a program branches on it. So they are mapped onto the fixed lists
        // this compiler has spellings for, and a triple LLVM understands but
        // this compiler has no name for is refused rather than guessed at —
        // laying a program out for a machine whose name is wrong is worse than
        // not compiling it.
        let (arch, os) = names(&triple)?;
        self.triple = Some(triple.clone());
        Ok(TargetInfo {
            triple,
            pointer_bits,
            endian,
            os,
            arch,
        })
    }

    fn emit_unit(
        &mut self,
        unit: &Unit,
        kind: OutputKind,
        out: &Path,
    ) -> Result<(), CodegenError> {
        // A `Context` owns every type and value built against it and everything
        // here borrows from it, so it is created per unit rather than held on
        // the backend. That is also what a unit *is* (§11) — an independent
        // value — so nothing is lost and the lifetimes stay local.
        let context = Context::create();
        // The triple `target_info` resolved, not the host's: a `--target` that
        // decided how every type was laid out has to decide what is emitted too.
        let triple = match &self.triple {
            Some(t) => TargetTriple::create(t),
            None => TargetMachine::get_default_triple(),
        };
        let machine = machine(&triple.as_str().to_string_lossy())?;
        let pointer_bytes = machine.get_target_data().get_pointer_byte_size(None) as u64;
        let module = unit::build(&context, unit, pointer_bytes)?;
        module.set_triple(&triple);
        module.set_data_layout(&machine.get_target_data().get_data_layout());

        // Verification is not optional. A module LLVM rejects is a bug in the
        // lowering or in this file, and the message it gives naming the
        // instruction is the whole diagnostic; writing the file first and
        // failing later would throw that away.
        module
            .verify()
            .map_err(|e| CodegenError::Failed(format!("LLVM rejected unit `{}`:\n{e}", unit.name)))?;

        match kind {
            OutputKind::Ir => module
                .print_to_file(out)
                .map_err(|e| CodegenError::Failed(e.to_string())),
            OutputKind::Object => machine
                .write_to_file(&module, FileType::Object, out)
                .map_err(|e| CodegenError::Failed(e.to_string())),
            OutputKind::Assembly => machine
                .write_to_file(&module, FileType::Assembly, out)
                .map_err(|e| CodegenError::Failed(e.to_string())),
        }
    }

    fn extension(&self, kind: OutputKind) -> &'static str {
        match kind {
            // LLVM's own textual form is `.ll`, not the generic `.ir`.
            OutputKind::Ir => "ll",
            other => other.extension(),
        }
    }
}

/// The architecture and OS names *this compiler* uses, read off a triple.
///
/// Both are checked against the fixed lists rather than passed through, because
/// they reach source through `core`'s generated `target.nest`: an architecture
/// spelled in a way `os.nest` has no variant for would produce a `core` that
/// does not compile, which is a confusing way to learn that a triple is
/// unsupported.
fn names(triple: &str) -> Result<(&'static str, &'static str), CodegenError> {
    let parts: Vec<&str> = triple.split('-').collect();
    let bad = |what: &str| {
        CodegenError::Unsupported(format!(
            "this compiler has no name for the {what} in `{triple}`; \
             it knows {}",
            if what == "architecture" {
                ARCHES.join(", ")
            } else {
                OSES.join(", ")
            }
        ))
    };

    // LLVM spells some architectures differently than this compiler's `Arch`
    // enum does, and the aliases are the ones a person actually types.
    let arch = parts
        .first()
        .and_then(|a| match *a {
            "arm64" => Some("aarch64"),
            "amd64" => Some("x86_64"),
            other => ARCHES.iter().copied().find(|x| *x == other),
        })
        .ok_or_else(|| bad("architecture"))?;

    // The OS is not at a fixed index — `x86_64-unknown-linux-gnu` and
    // `wasm32-wasi` disagree — so it is looked for. `darwin` carries a version
    // suffix (`darwin24`) often enough that the check is a prefix.
    let os = parts
        .iter()
        .find_map(|p| {
            if p.starts_with("darwin") || p.starts_with("macos") || *p == "ios" {
                return Some("macos");
            }
            if p.starts_with("freebsd") {
                return Some("freebsd");
            }
            if *p == "unknown" || p.is_empty() {
                // The vendor field, which is not the OS. A bare
                // `wasm32-unknown-unknown` has no OS and lands on `none` below.
                return None;
            }
            OSES.iter().copied().find(|o| *o == *p)
        })
        .or_else(|| {
            // `wasm32-unknown-unknown` is a real target with no operating
            // system, and `none` is exactly the name this compiler has for that.
            (arch == "wasm32").then_some("none")
        })
        .ok_or_else(|| bad("operating system"))?;

    Ok((arch, os))
}

#[cfg(test)]
mod tests;
