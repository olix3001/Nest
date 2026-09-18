//! What a backend is, as an interface.
//!
//! `design/lir.md` §10 says what a backend has to *answer for*; this module says
//! how it is asked. There is one trait, and a backend is a value implementing
//! it: the compiler holds a `Box<dyn Codegen>` and never names a concrete one
//! outside [`select`]. That is the whole point — an LLVM backend, a C backend
//! and a wasm backend should be three files that no other file has to learn
//! about, and the way to keep that true is for the driver to have no branch on
//! which one it is holding.
//!
//! ## The two things a backend is asked
//!
//! **What machine is this**, and **emit this unit**. They are separated because
//! they happen at opposite ends of a compilation: the machine's facts are needed
//! before the first type is laid out, and emission happens after everything
//! else. Putting them in one trait is what makes the front end's answer to
//! "how wide is a pointer" come from the thing that will actually generate the
//! code, rather than from a default that might disagree with it.
//!
//! ## The target's facts come from the backend
//!
//! [`Target`](crate::common::options::Target) used to be a default the compiler
//! carried. It is now what a backend *reports*, resolved from a target triple:
//! LLVM knows what `aarch64-apple-darwin` means, and a table in this compiler
//! repeating that would be a second source of truth able to drift from the one
//! that matters. The driver asks first, writes the answer into
//! [`Options`](crate::common::options::Options), and only then runs analysis —
//! so `core`'s generated `target.nest` and the layout engine both see the
//! machine the code is being generated for.
//!
//! `-C pointer-width` / `-C os` / `-C arch` survive as **overrides**, for a
//! build with no backend compiled in and for tests that want a 16-bit machine
//! without owning one.

use std::path::Path;

use crate::common::options::{Options, Target};
use crate::lir::Unit;

pub mod link;
#[cfg(feature = "llvm")]
pub mod llvm;
pub mod text;

/// The facts about a machine that the compiler needs before it can lay out a
/// type or generate code for one.
///
/// It is deliberately small. Everything here is something a pass *earlier than
/// codegen* reads — the layout engine asks how wide a pointer is, and `core`'s
/// generated `target.nest` names the OS and the architecture so a program can
/// branch on them. Facts only the backend needs (the ABI's register
/// classification, the alignment of an `f80`) are not here, because the backend
/// already has them and a copy in the middle would be a copy able to be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    /// The triple the backend resolved, in the backend's own spelling.
    pub triple: String,
    /// The width in bits of a pointer, of `usize` and of `isize`.
    pub pointer_bits: u32,
    /// Which end the low byte is at.
    pub endian: Endian,
    /// The OS, as one of [`crate::common::options::OSES`].
    pub os: &'static str,
    /// The architecture, as one of [`crate::common::options::ARCHES`].
    pub arch: &'static str,
}

impl TargetInfo {
    /// The facts, in the shape the rest of the compiler reads them.
    ///
    /// [`Target`] is what a pass asks; this is what a backend answers. They are
    /// two types rather than one because they are two different statements: a
    /// `TargetInfo` is what the machine *is*, reported by something that knows,
    /// and a `Target` is what this compilation was *configured for*, which
    /// `-C` overrides may still have a say in.
    pub fn target(&self) -> Target {
        Target {
            pointer_bits: self.pointer_bits,
            os: self.os,
            arch: self.arch,
        }
    }
}

/// Byte order.
///
/// Nothing reads it yet — the layout engine lays a struct out the same way
/// either way, and the const evaluator works in arbitrary precision rather than
/// in bytes. It is carried because the first thing that *will* need it is a
/// blob constant's bytes (§9), and a backend that had to be asked again at that
/// point is a backend that could answer differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// What to write out for one unit.
///
/// Three, and each is a *file a backend produces*. The compiler's own dumps —
/// the AST, the IR, the LIR — are not here: they are the driver's, they exist
/// with no backend at all, and a backend asked to print LIR would be a backend
/// asked to do something that has nothing to do with generating code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// A relocatable object file. The default, and the only one a linker wants.
    Object,
    /// Target assembly, as text.
    Assembly,
    /// The backend's own intermediate form, as text — LLVM IR for the LLVM
    /// backend. For reading and for bug reports; nothing consumes it.
    Ir,
}

impl OutputKind {
    /// The extension a file of this kind conventionally gets. A backend may
    /// disagree — a wasm backend's object is a `.wasm` — which is why this is
    /// the *default* and [`Codegen::extension`] is what is actually asked.
    pub fn extension(self) -> &'static str {
        match self {
            OutputKind::Object => "o",
            OutputKind::Assembly => "s",
            OutputKind::Ir => "ir",
        }
    }

    /// How `--emit=` spells it.
    pub fn name(self) -> &'static str {
        match self {
            OutputKind::Object => "obj",
            OutputKind::Assembly => "asm",
            OutputKind::Ir => "backend-ir",
        }
    }
}

/// Why a backend could not do what it was asked.
///
/// Three kinds, and the split is about *who has to fix it*. A
/// [`Unsupported`](CodegenError::Unsupported) is the user asking for something
/// this backend does not do — a bad triple, an output kind it has no writer for
/// — and is reported like a bad flag. A [`Failed`](CodegenError::Failed) is the
/// backend refusing a unit, which is either a bug here or a bug in the lowering.
/// An [`Io`](CodegenError::Io) is the file system.
#[derive(Debug)]
pub enum CodegenError {
    /// The backend does not do this, and saying so is the whole answer.
    Unsupported(String),
    /// The backend tried and could not.
    Failed(String),
    Io(std::io::Error),
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodegenError::Unsupported(m) | CodegenError::Failed(m) => write!(f, "{m}"),
            CodegenError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CodegenError {}

impl From<std::io::Error> for CodegenError {
    fn from(e: std::io::Error) -> Self {
        CodegenError::Io(e)
    }
}

/// A backend.
///
/// Object-safe on purpose: the driver holds one as a `Box<dyn Codegen>` and has
/// no type parameter threaded through it, so adding a backend touches this
/// module's [`select`] and nothing else.
///
/// It takes **one unit at a time** (§11). Which units exist, what they are
/// called and what file each one is written to are the driver's business, and
/// keeping them there is what makes compiling them in parallel a scheduling
/// question rather than a redesign — the trait already says a unit is emitted on
/// its own.
pub trait Codegen {
    /// How `-C backend=` selects it, and how a diagnostic names it.
    fn name(&self) -> &'static str;

    /// Resolve a target triple — or, with `None`, the machine the compiler is
    /// running on — into the facts the front end needs.
    ///
    /// This is asked **before analysis**, because the layout engine and `core`'s
    /// generated `target.nest` both read the answer. A backend that cannot
    /// resolve the triple says [`CodegenError::Unsupported`] and the driver
    /// stops: continuing would mean laying types out for a machine that is not
    /// the one being compiled for.
    /// It takes `&mut self` because resolving a target **configures** the
    /// backend: the triple decided here is the one every later `emit_unit` has
    /// to generate for, and a backend that could not remember it would quietly
    /// emit for the host whenever a `--target` was given.
    fn target_info(&mut self, triple: Option<&str>) -> Result<TargetInfo, CodegenError>;

    /// Take the settings that decide how code is generated rather than what it
    /// does — `opt-level` and `target-cpu` — once they are resolved, before the
    /// first [`emit_unit`](Codegen::emit_unit). A backend with nothing to tune
    /// ignores them.
    fn configure(&mut self, _options: &Options) {}

    /// Write one unit out as `kind`, to `out`.
    ///
    /// The path is complete, extension included; the driver built it. A backend
    /// that does not produce `kind` says [`CodegenError::Unsupported`] rather
    /// than writing something else.
    fn emit_unit(&mut self, unit: &Unit, kind: OutputKind, out: &Path) -> Result<(), CodegenError>;

    /// The extension a file of this kind gets from *this* backend. The default
    /// is the conventional one; a backend whose object files are `.wasm` is why
    /// this is overridable.
    fn extension(&self, kind: OutputKind) -> &'static str {
        kind.extension()
    }
}

/// Every backend that is compiled in, in the order `-C backend=` prefers them.
///
/// The list is short and explicit rather than a registry a backend registers
/// itself into: a compiler should not have a set of backends that depends on
/// link order, and a person reading this file should be able to see all of them.
pub fn backends() -> Vec<Box<dyn Codegen>> {
    let mut all: Vec<Box<dyn Codegen>> = Vec::new();
    // LLVM first, so it is the default wherever it is compiled in: a compiler
    // that can produce an object file should, without being asked.
    #[cfg(feature = "llvm")]
    all.push(Box::new(llvm::LlvmBackend::default()));
    all.push(Box::new(text::TextBackend));
    all
}

/// The backend named, or the default one.
///
/// The default is the **first** in [`backends`], which is the order that file
/// lists them in. An unknown name is an error and never a fallback, for the
/// reason an unknown `-C` key is: a typo would otherwise build something other
/// than what was asked for, silently.
pub fn select(name: Option<&str>) -> Result<Box<dyn Codegen>, String> {
    let mut all = backends();
    match name {
        None => Ok(all.remove(0)),
        Some(want) => {
            if let Some(i) = all.iter().position(|b| b.name() == want) {
                return Ok(all.remove(i));
            }
            let names: Vec<&str> = all.iter().map(|b| b.name()).collect();
            Err(format!(
                "`backend` must be one of {}, not `{want}`",
                names.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests;
