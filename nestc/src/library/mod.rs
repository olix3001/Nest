//! Libraries: what a package compiled on its own leaves behind for the packages
//! compiled against it.
//!
//! ### `.nlib`: the library, and the only artifact
//!
//! One file per package, an `ar` archive (`archive`) of three kinds of member:
//!
//! - `nest.nmeta`, the **analysis**: every definition with its namespace and
//!   what it declares, every `impl` it writes, the `#lang` tags — what a
//!   package compiled against this one needs in order to typecheck against it.
//!   **Not** its syntax trees: a question about a definition is answered from
//!   what analysis concluded, never by reading the source again.
//! - `nest.nir`, the **IR**, before monomorphization: a generic function is
//!   instantiated by whoever calls it, so its body has to travel. It is read
//!   beside the metadata, and separate from it because typechecking alone does
//!   not need it.
//! - `u0.o`, `u1.o`, …, one **object** per codegen unit.
//!
//! Loading one ([`read::load`]) puts the first two into a session as though the
//! package had been analyzed there, and the analysis passes skip its files.
//!
//! Two consequences shape the rest. A package is analyzed **once**, where it is
//! compiled; and the files a reader has are the library's, so a package's
//! source does not have to exist where it is used. The text of each file does
//! travel, because a diagnostic pointing into a library still wants to show the
//! line.

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
use crate::sema::decl::Decl;
use crate::sema::def::{Def, DefId};
use crate::sema::impls::ImplInfo;

use codec::{Bases, Counts};

/// The first bytes of a library's metadata member.
pub const MAGIC: &[u8; 8] = b"NESTMETA";

/// The layout of what follows the magic. Raised whenever anything written
/// changes shape, so an old library is refused by name rather than misread.
pub const FORMAT: u32 = 9;

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

/// The analysis, which is the rest of the `nest.nmeta` member.
#[derive(Serialize, Deserialize)]
pub struct Meta {
    pub files: Vec<FileRecord>,
    /// The package's root file.
    pub root: FileId,
    pub defs: Vec<Def>,
    /// What each of them declares (`crate::sema::decl`) — the answers a package
    /// compiled against this one would otherwise have to read off a tree it
    /// does not have.
    pub decls: Vec<(DefId, Decl)>,
    /// Every `impl` the package writes, resolved into types
    /// (`crate::sema::impls`). An impl is a candidate at every selection in
    /// every package that reads this one, so it travels rather than being read
    /// out of syntax again.
    pub impls: Vec<ImplInfo>,
    /// The `#lang` tags this package's definitions claim, and whether each was
    /// `core`'s claim.
    pub lang_items: Vec<(Symbol, DefId, bool)>,
}

/// The `nest.nir` member: each file's IR, before monomorphization, and the
/// facts the passes left on it.
///
/// It is its own member because it is read for a different reason than the
/// metadata is — a compilation that only typechecks against this package never
/// looks at it — and because the ids inside it are translated by the same
/// encoding, so the two are written and read together.
#[derive(Serialize, Deserialize)]
pub struct Ir {
    pub programs: Vec<(FileId, Program)>,
    pub facts: Vec<(IrId, metas::MetaValue)>,
}

/// One file of the package.
///
/// **No tree.** A library carries what analysis concluded about the file, not
/// the syntax it concluded it from: the declaration table
/// (`crate::sema::decl`), the impls, and the IR. The tree used to be here and
/// was **67%** of `std`'s metadata, against 1.6% for the table that replaced
/// it; reading it was nearly the whole cost of using a library.
///
/// The `src` stays, and is the one thing about the file that is still the
/// source: a diagnostic that points into a dependency shows the line, and
/// nothing in the metadata can reconstruct that.
#[derive(Serialize, Deserialize)]
pub struct FileRecord {
    pub name: String,
    pub src: String,
    /// The namespace the file is.
    pub ns: DefId,
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

#[cfg(test)]
mod tests {
    use crate::sema::session::{MemLoader, Session};

    /// One package's library members, and the session that produced them.
    fn compile_library(mut session: Session, package: &str) -> (Vec<u8>, Vec<u8>) {
        // Through `load_package`, so the session knows the file is the
        // package's root — which is what makes it a library's to write.
        let root = session
            .load_package(package)
            .unwrap_or_else(|| panic!("`{package}` loads"));
        crate::sema::analyze(&mut session, root);
        assert!(
            !session.has_errors(),
            "`{package}`: {:#?}",
            session.diagnostics
        );
        super::write::members(&session, package, root).expect("the library is writable")
    }

    /// A program compiled against a library sees what the library's own
    /// compilation concluded, and nothing it would have had to re-read.
    ///
    /// The whole chain in one process: `core` written as a library, a package
    /// written against that, and a program written against both. It is the
    /// shape `just build` has, and the one a `cargo test` alone never takes —
    /// which is how a library that links against nothing got onto `main` once
    /// already (see `codegen::llvm::tests::a_library_internalizes_nothing`).
    #[test]
    fn a_program_compiles_against_a_library() {
        let path = std::path::Path::new("<test>");

        // `core`, from its real source: a mock one would not exercise the
        // shapes a library actually has to carry.
        let mut core_session = Session::new();
        let core_root = core_session.load_package("core").expect("core loads");
        crate::sema::analyze(&mut core_session, core_root);
        assert!(
            !core_session.has_errors(),
            "{:#?}",
            core_session.diagnostics
        );
        let (core_meta, core_ir) =
            super::write::members(&core_session, "core", core_root).expect("core is writable");

        // A package of our own, compiled against `core` as a library.
        let shapes_src = "{ Location } :: import <core/loc>\n\
                          @public\n\
                          Circle :: struct { radius: f64 }\n\
                          @public\n\
                          Shape :: trait { area :: func (self: *Self) -> f64 }\n\
                          impl Shape for Circle {\n\
                          @public\n\
                          area :: func (self: *Circle) -> f64 { return self.radius }\n\
                          }\n\
                          @public\n\
                          SIDES :: 6\n\
                          @public\n\
                          ALSO :: SIDES\n\
                          @public\n\
                          NAME :: \"circle\"\n\
                          @public\n\
                          WIDTH: u16 :: 80\n\
                          @public\n\
                          scaled :: func (radius: f64, by: f64 := 2.0) -> f64 { return radius * by }\n\
                          @public\n\
                          line_of :: func (loc: Location := #caller_location) -> u32 { return loc.line }\n";
        let mut shapes_session = Session::with_loader(Box::new(
            MemLoader::new().with("shapes", shapes_src).with("main", ""),
        ));
        super::read::load(&mut shapes_session, &core_meta, &core_ir, path, true)
            .expect("core loads as a library");
        shapes_session.register_package("shapes", "shapes");
        let (shapes_meta, shapes_ir) = compile_library(shapes_session, "shapes");

        // And a program against both. The call goes through a trait `impl`
        // declared in one library over a type declared in the same one — every
        // answer it needs is metadata, since neither tree is here.
        // The call goes through a **trait object**, so the impl has to be
        // selected and a vtable built out of it — which is the question only
        // the library's metadata can answer, its tree being elsewhere.
        //
        // The constants and the two defaults are the rest of what only the
        // metadata can answer: a comptime constant settles on a different width
        // at each of the two uses below, an omitted argument is filled from the
        // expression the library lowered once, and `#caller_location` is filled
        // from *this* file rather than from the declaration's own line.
        let program = "shapes :: import <shapes>\n\
                       { SIDES } :: import <shapes>\n\
                       main :: func () {\n\
                       let c := shapes.Circle { radius: 2.0 }\n\
                       const obj: *dyn shapes.Shape := &c\n\
                       let a := obj.area()\n\
                       let b := a + 1.0\n\
                       let small: u8 := shapes.SIDES\n\
                       let big: i64 := shapes.ALSO\n\
                       let name: str := shapes.NAME\n\
                       let width := shapes.WIDTH\n\
                       let twice := shapes.scaled(2.0)\n\
                       let by := shapes.scaled(2.0, 3.0)\n\
                       let here := shapes.line_of()\n\
                       let arr: [SIDES]u8 := [_]u8 { 1, 2, 3, 4, 5, 6 }\n\
                       }\n";
        let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", program)));
        super::read::load(&mut session, &core_meta, &core_ir, path, true)
            .expect("core loads as a library");
        super::read::load(&mut session, &shapes_meta, &shapes_ir, path, true)
            .expect("shapes loads as a library");
        let file = session.load_entry("main").expect("the program loads");
        crate::sema::analyze(&mut session, file);
        assert!(!session.has_errors(), "{:#?}", session.diagnostics);

        // Nothing in the program lowered to an error type. A question about a
        // library that went unanswered is not a diagnostic — it is a
        // well-formed node of type `Ty::Error`, and an unfilled default
        // argument is exactly that — so this is the check that sees it.
        let mut residue = Vec::new();
        crate::ir::check::residue::check(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &mut residue,
        );
        assert!(residue.is_empty(), "{residue:#?}");

        // And the two defaults arrived as what they are: one an expression the
        // library lowered and kept on its parameter, the other the marker that
        // says the value is the *call's* own position.
        let param = |name: &str, i: usize| {
            let def = session
                .defs
                .iter()
                .find(|d| d.name.as_str() == name)
                .unwrap_or_else(|| panic!("`{name}` is in the def table"));
            match session.decls.get(&def.id) {
                Some(crate::sema::decl::Decl::Func(f)) => f.params[i].lowered,
                other => panic!("`{name}` is recorded as {other:#?}"),
            }
        };
        let Some(crate::sema::decl::ParamDefault::Value(id)) = param("scaled", 1) else {
            panic!("`scaled`'s default did not travel: {:?}", param("scaled", 1));
        };
        assert!(
            session.ir_meta.get::<crate::ir::DefaultValue>(id).is_some(),
            "the default's expression is not in the metadata that travelled"
        );
        assert!(
            matches!(
                param("line_of", 0),
                Some(crate::sema::decl::ParamDefault::CallerLocation)
            ),
            "`line_of`'s `#caller_location` did not travel: {:?}",
            param("line_of", 0)
        );
    }
}
