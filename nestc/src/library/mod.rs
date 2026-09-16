//! Libraries: what a package compiled on its own leaves behind for the packages
//! compiled against it.
//!
//! ### `.nmeta`: the package, analyzed
//!
//! Everything a package compiled against this one asks about it, already
//! answered: every definition with its namespace, every file's tree with the
//! facts resolution and inference left on it, and every function lowered to IR
//! **before** monomorphization — a generic function is instantiated by whoever
//! calls it, so its body has to travel. Loading one ([`read::load`]) puts all of
//! that into a session as though the package had been analyzed there, and the
//! analysis passes skip its files.
//!
//! Two consequences shape the rest. A package is analyzed **once**, where it is
//! compiled; and the files a reader has are the metadata's, so a package's
//! source does not have to exist where it is used. The text of each file does
//! travel, because a diagnostic pointing into a library still wants to show the
//! line.
//!
//! ### `.nlib`: the package, compiled
//!
//! The metadata and the objects, in one `ar` archive (`archive`). Linking a
//! program links every library's objects; compiling against one reads only its
//! metadata member.

pub mod archive;
pub mod codec;
pub mod fingerprint;
pub mod metas;
pub mod read;
pub mod write;

use serde::{Deserialize, Serialize};

use crate::common::source::FileId;
use crate::common::symbol::Symbol;
use crate::ir::{IrId, Program};
use crate::parser::ast::{Ast, NodeId};
use crate::sema::def::{Def, DefId};

use codec::{Bases, Counts};

/// The first bytes of every `.nmeta`.
pub const MAGIC: &[u8; 8] = b"NESTMETA";

/// The layout of what follows the magic. Raised whenever anything written
/// changes shape, so an old library is refused by name rather than misread.
pub const FORMAT: u32 = 1;

/// What a reader checks before it reads anything else.
#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    pub format: u32,
    /// The compiler that wrote it. The data is this compiler's own tables, so a
    /// library is read only by the compiler that wrote it.
    pub compiler: String,
    /// The package's name.
    pub name: String,
    /// What the build was *for*: the generated `core/target.nest`, which is
    /// exactly what analysis read about the target. A library compiled for one
    /// target is not a library for another.
    pub target: String,
    /// Every package an id in the body belongs to. The first is this one; the
    /// rest must already be loaded when this one is.
    pub packages: Vec<String>,
    /// How many ids of each kind this package owns.
    pub counts: Counts,
    /// The primitives the body names, which a reader creates before reading.
    pub builtins: Vec<Symbol>,
    /// The files the package was compiled from, by the names they were read
    /// under — what checking it is up to date reads again.
    pub inputs: Vec<String>,
    /// The settings it was compiled with, as `-C print=options` writes them.
    pub settings: String,
    /// Everything above and each library it was compiled against, hashed
    /// (`fingerprint`).
    pub fingerprint: u64,
}

/// Everything else.
#[derive(Serialize, Deserialize)]
pub struct Body {
    pub files: Vec<FileRecord>,
    /// The package's root file.
    pub root: FileId,
    pub defs: Vec<Def>,
    /// The `#lang` tags this package's definitions claim, and whether each was
    /// `core`'s claim.
    pub lang_items: Vec<(Symbol, DefId, bool)>,
    /// Each file's IR, before monomorphization.
    pub programs: Vec<(FileId, Program)>,
    pub ir_facts: Vec<(IrId, metas::MetaValue)>,
}

/// One file of the package.
#[derive(Serialize, Deserialize)]
pub struct FileRecord {
    pub name: String,
    pub src: String,
    /// The namespace the file is.
    pub ns: DefId,
    pub ast: Ast,
    pub facts: Vec<(NodeId, metas::MetaValue)>,
}

/// A package a session read from a library, and where its ids landed.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub name: String,
    pub bases: Bases,
    pub counts: Counts,
    pub root: FileId,
    /// Whether files compiled here may import it: a direct dependency may, a
    /// dependency of a dependency is only there to be read.
    pub importable: bool,
    /// Where it was read from, which linking reads again for the objects.
    pub path: std::path::PathBuf,
    /// Its header's fingerprint, which a library compiled against it hashes.
    pub fingerprint: u64,
}

impl Loaded {
    pub fn owns_def(&self, def: DefId) -> bool {
        def.0 >= self.bases.def && def.0 < self.bases.def + self.counts.defs
    }

    pub fn owns_file(&self, file: FileId) -> bool {
        file.0 >= self.bases.file && file.0 < self.bases.file + self.counts.files
    }

    pub fn owns_ir(&self, id: IrId) -> bool {
        id.0 >= self.bases.ir && id.0 < self.bases.ir + self.counts.ir
    }
}

/// The compiler's identity, as a header spells it.
pub fn compiler_id() -> String {
    format!("nestc {}", env!("CARGO_PKG_VERSION"))
}
