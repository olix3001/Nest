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
pub const FORMAT: u32 = 14;

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
    use crate::common::source::FileId;
    use crate::sema::session::{MemLoader, Session};

    /// A library's two members: its metadata and its IR.
    type Members = (Vec<u8>, Vec<u8>);

    /// Where a library is said to have come from. Nothing reads it back — the
    /// objects are the only thing a path is needed for, and these tests link
    /// nothing.
    const NOWHERE: &str = "<test>";

    /// One package's library members, and the session that produced them.
    fn compile_library(mut session: Session, package: &str) -> Members {
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

    /// `core`, from its real source, written once for every test below.
    ///
    /// A mock `core` would not exercise the shapes a library actually has to
    /// carry, and analyzing the real one per test is most of what these would
    /// cost.
    fn core() -> &'static Members {
        static CORE: std::sync::OnceLock<Members> = std::sync::OnceLock::new();
        CORE.get_or_init(|| {
            let mut session = Session::new();
            let root = session.load_package("core").expect("core loads");
            crate::sema::analyze(&mut session, root);
            assert!(!session.has_errors(), "{:#?}", session.diagnostics);
            super::write::members(&session, "core", root).expect("core is writable")
        })
    }

    /// The library `name` is, compiled from `src` against `against` — which is
    /// `core` and whatever other libraries the source imports.
    fn library(name: &str, src: &str, against: &[&Members]) -> Members {
        let mut session =
            Session::with_loader(Box::new(MemLoader::new().with(name, src).with("main", "")));
        for (meta, ir) in against {
            super::read::load(&mut session, meta, ir, std::path::Path::new(NOWHERE), true)
                .expect("a library loads");
        }
        session.register_package(name, name);
        compile_library(session, name)
    }

    /// A program compiled against `against`, and the file it was written in.
    ///
    /// This is the shape that matters: **neither tree is here**. Every question
    /// the program's inference and lowering ask about a definition in one of
    /// those libraries is answered from its metadata or not at all.
    fn program(src: &str, against: &[&Members]) -> (Session, FileId) {
        let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", src)));
        for (meta, ir) in against {
            super::read::load(&mut session, meta, ir, std::path::Path::new(NOWHERE), true)
                .expect("a library loads");
        }
        let file = session.load_entry("main").expect("the program loads");
        crate::sema::analyze(&mut session, file);
        (session, file)
    }

    /// The program compiled, and lowered to no error type.
    ///
    /// Both halves are needed. A question about a library that went unanswered
    /// is usually **not** a diagnostic: it is a well-formed IR node of type
    /// `Ty::Error` — an argument nothing filled in, a length nothing knew — and
    /// the residue check is what sees one.
    fn clean(session: &Session, file: FileId) {
        assert!(!session.has_errors(), "{:#?}", session.diagnostics);
        let mut residue = Vec::new();
        crate::ir::check::residue::check(
            &session.defs,
            &session.ir_meta,
            &session.linked,
            &mut residue,
        );
        assert!(residue.is_empty(), "{residue:#?}");
        assert!(
            session.ir.contains_key(&file),
            "the program produced no IR at all"
        );
    }

    /// What compiling the program reported, as messages.
    fn errors(session: &Session) -> Vec<String> {
        session
            .diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect()
    }

    /// A library of one of everything whose answer used to be read off a tree.
    ///
    /// Shared by the tests below so that each names the one thing it is about,
    /// and so that they all read the *same* metadata — a fact that travels for
    /// one question and not another is a bug these would otherwise miss.
    fn shapes() -> &'static Members {
        static SHAPES: std::sync::OnceLock<Members> = std::sync::OnceLock::new();
        SHAPES.get_or_init(|| {
            library(
                "shapes",
                r#"{ Location } :: import <core/loc>
{ cast } :: import <core/mem>

@public SIDES :: 6
@public ALSO :: SIDES
@public BIG :: 300
@public WIDTH: u16 :: 80
@public LIMIT :: SIDES * 2
@public NAME :: "circle"

@public Shape :: trait { area :: func (self: *Self) -> i32 }

@public(all) Square :: struct { side: i32 }

impl Shape for Square {
    @public area :: func (self: *Square) -> i32 { return self.side * self.side }
}

@public Tag :: distinct u16

@public width_of :: func (t: Tag) -> u16 { return cast.<u16>(t) }

@public Side :: Square

@public Either :: enum {
    left(i32),
    right(str),
}

@public describe :: func (e: Either) -> i32 {
    return e.match {
        .left(n) => n,
        .right(_) => 0,
    }
}

@public scaled :: func (n: i32, by: i32 := 3) -> i32 { return n * by }

@public line_of :: func (loc: Location := #caller_location) -> u32 { return loc.line }

@public doubled :: func <T: Shape> (s: *T) -> i32 { return s.area() * 2 }

@public Scale :: trait <By> { scale :: func (self: *Self, by: By) -> i32 }

impl Scale.<i32> for Square {
    @public scale :: func (self: *Square, by: i32) -> i32 { return self.side * by }
}

impl Scale.<u8> for Square {
    @public scale :: func (self: *Square, by: u8) -> i32 { return self.side + cast.<i32>(by) }
}

// The bound carries an **argument**, and `Square` implements the trait twice —
// so which impl this reaches is decided by the `i32`, and by nothing else.
@public scaled_by :: func <T: Scale.<i32>> (s: *T) -> i32 { return s.scale(4) }

// A struct whose fields this package keeps to itself (§4.4). A program reading
// this library names the type and calls the function, and cannot touch the
// fields.
@public(fields: package) Meters :: struct { m: i32 }

@public meters :: func (m: i32) -> Meters { return Meters { m: m } }

// An overload set (§4.3) — a name for two functions, which a program reading
// this library has to be able to choose between.
@public sized_n :: func (n: i32) -> i32 { return n }
@public sized_s :: func (s: Square) -> i32 { return s.side }
@public sized :: func { sized_n, sized_s }
"#,
                &[core()],
            )
        })
    }

    /// The file of `shapes` a program is given is **name, text and namespace**.
    ///
    /// The tree is what this format no longer has, and the reason for every
    /// other test here: it is 67% of what `std`'s metadata used to be, and each
    /// question below is one that had been answered by reading it.
    #[test]
    fn a_library_brings_no_syntax_tree() {
        let (session, file) = program("main :: func () { }\n", &[core(), shapes()]);
        clean(&session, file);
        let foreign: Vec<FileId> = session
            .sources
            .files()
            .map(|f| f.id)
            .filter(|&id| session.is_foreign_file(id))
            .collect();
        assert!(
            foreign.len() > 1,
            "only {} foreign files: the libraries did not load",
            foreign.len()
        );
        for id in foreign {
            let name = &session.sources.file(id).expect("the file is there").name;
            assert!(
                !session.asts.contains_key(&id),
                "`{name}` arrived with a tree"
            );
            // Its text did arrive: a diagnostic pointing into a dependency
            // shows the line, and nothing in the metadata could rebuild it.
            assert!(
                !session
                    .sources
                    .file(id)
                    .expect("the file is there")
                    .src
                    .is_empty(),
                "`{name}` arrived without its text"
            );
        }
    }

    /// A trait impl written in one library, over a type from the same one, is
    /// selected through a **trait object** by a program that has neither tree.
    ///
    /// The call goes through a `*dyn`, so the impl has to be found and a vtable
    /// built out of it — which is the question only the metadata can answer.
    #[test]
    fn an_impl_travels_and_is_selected_through_a_trait_object() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let sq := shapes.Square { side: 4 }
    const obj: *dyn shapes.Shape := &sq
    let a := obj.area()
    let b := a + 1
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);
    }

    /// A library compiled against a library: three packages deep, with the
    /// middle one's metadata naming ids that belong to the first.
    #[test]
    fn a_library_compiled_against_a_library_is_read_through_both() {
        let middle = library(
            "middle",
            r#"shapes :: import <shapes>
{ SIDES, Shape, Square } :: import <shapes>

@public sides :: func () -> [SIDES]u8 {
    return [_]u8 { 1, 2, 3, 4, 5, 6 }
}

@public area_of :: func (side: i32) -> i32 {
    let sq := Square { side: side }
    const obj: *dyn Shape := &sq
    return obj.area()
}

@public doubled :: func (n: i32) -> i32 { return shapes.scaled(n) }
"#,
            &[core(), shapes()],
        );
        let (session, file) = program(
            r#"middle :: import <middle>

main :: func () {
    let a := middle.area_of(3)
    let d := middle.doubled(5)
    let n := middle.sides().len()
}
"#,
            &[core(), shapes(), &middle],
        );
        clean(&session, file);
    }

    /// A comptime constant from a library settles **per use**: `SIDES` is a `u8`
    /// here and an `i64` there, which is why what travels is its shape and not a
    /// type.
    #[test]
    fn a_comptime_constant_from_a_library_settles_at_each_use() {
        let (session, file) = program(
            r#"{ SIDES, ALSO, NAME, WIDTH } :: import <shapes>

main :: func () {
    let narrow: u8 := SIDES
    let wide: i64 := SIDES
    // A constant that names another inherits its comptime-ness.
    let also: u16 := ALSO
    // A declared type is the same at every use.
    let w := WIDTH
    let name: str := NAME
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);
    }

    /// And its **value** travels with it, so a literal that does not fit is
    /// refused where it is written — the check a program gets for free in its
    /// own package.
    #[test]
    fn a_constant_that_does_not_fit_is_refused_across_a_library() {
        let (session, _) = program(
            r#"{ BIG } :: import <shapes>

main :: func () {
    let small: u8 := BIG
}
"#,
            &[core(), shapes()],
        );
        assert!(
            errors(&session)
                .iter()
                .any(|m| m.contains("does not fit in `u8`")),
            "{:#?}",
            errors(&session)
        );
    }

    /// A constant is an **array length** in another package, folded expression
    /// included: that needs the value, not the type.
    #[test]
    fn a_constant_from_a_library_is_an_array_length() {
        let (session, file) = program(
            r#"{ SIDES, LIMIT } :: import <shapes>

main :: func () {
    let six: [SIDES]u8 := [_]u8 { 1, 2, 3, 4, 5, 6 }
    let twelve: [LIMIT]u8 := [_]u8 { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12 }
    let n := six.len() + twelve.len()
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);
    }

    /// An omitted argument is filled in from the expression the **library**
    /// lowered, which is the one thing in the declaration table that is IR.
    #[test]
    fn an_omitted_argument_is_filled_in_from_the_library() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let three := shapes.scaled(1)
    let six := shapes.scaled(2, 3)
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);

        // The default arrived as what it is, and its expression is in the
        // metadata that travelled with it.
        let def = session
            .defs
            .iter()
            .find(|d| d.name.as_str() == "scaled")
            .expect("`scaled` is in the def table");
        let Some(crate::sema::decl::Decl::Func(f)) = session.decls.get(&def.id) else {
            panic!("`scaled` is not recorded as a function");
        };
        let Some(crate::sema::decl::ParamDefault::Value(id)) = f.params[1].lowered else {
            panic!("`scaled`'s default did not travel: {:?}", f.params[1]);
        };
        assert!(
            session.ir_meta.get::<crate::ir::DefaultValue>(id).is_some(),
            "the default's expression is not in the metadata that travelled"
        );
    }

    /// `@public(fields: package)` reaches the package's own files and stops
    /// there: a program that reads the library names the type, not its fields.
    #[test]
    fn a_package_private_field_does_not_leave_its_package() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let m := shapes.meters(3)
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);

        let (session, _) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let m := shapes.meters(3)
    let n := m.m
}
"#,
            &[core(), shapes()],
        );
        assert!(
            session
                .diagnostics
                .iter()
                .any(|d| d.message.contains("the field `m` of `Meters` is private")),
            "{:#?}",
            session.diagnostics
        );
    }

    /// An overload set travels: the namespace a library brings carries every
    /// function of a name, so a call here picks among all of them rather than
    /// among the one `members` happens to hold.
    #[test]
    fn an_overload_set_travels_with_the_library() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let n := shapes.sized(7)
    let s := shapes.sized(shapes.Square { side: 3 })
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);

        // The set's members are in the table, and the call to each chose its
        // own.
        let both: Vec<_> = session
            .defs
            .iter()
            .filter(|d| matches!(d.name.as_str(), "sized_n" | "sized_s"))
            .map(|d| d.id)
            .collect();
        assert_eq!(both.len(), 2, "the overload set did not travel whole");
        let ast = &session.asts[&file];
        let chosen: std::collections::HashSet<_> = ast
            .ids()
            .filter_map(|id| match &ast.node(id).kind {
                crate::parser::ast::NodeKind::Call { callee, .. } => {
                    match ast.meta::<crate::sema::Resolution>(*callee) {
                        Some(crate::sema::Resolution::Def(d)) => Some(d),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        assert!(
            both.iter().all(|d| chosen.contains(d)),
            "the two calls did not pick one overload each: {chosen:?}"
        );
    }

    /// `#caller_location` is the exception among defaults: what travels is the
    /// marker, because the value is **which call site asked** and the
    /// declaration's own lowering of it is never the answer.
    #[test]
    fn a_caller_location_default_is_filled_in_at_the_call() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let here := shapes.line_of()
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);

        let def = session
            .defs
            .iter()
            .find(|d| d.name.as_str() == "line_of")
            .expect("`line_of` is in the def table");
        let Some(crate::sema::decl::Decl::Func(f)) = session.decls.get(&def.id) else {
            panic!("`line_of` is not recorded as a function");
        };
        assert!(
            matches!(
                f.params[0].lowered,
                Some(crate::sema::decl::ParamDefault::CallerLocation)
            ),
            "`line_of`'s `#caller_location` did not travel: {:?}",
            f.params[0]
        );

        // And the location it was filled in with is the program's own file,
        // not the library's: the literal is in `main`'s body.
        let main = session.ir[&file]
            .funcs
            .iter()
            .find(|f| session.defs.get(f.def).name.as_str() == "main")
            .expect("`main` was lowered");
        let body = main.body.as_ref().expect("`main` has a body");
        let mut files = Vec::new();
        collect_strings(body, &mut files);
        assert!(
            files.iter().any(|f| f.ends_with("main")),
            "the location filled in names {files:?}, not the program's own file"
        );
    }

    /// Every string literal in `block`, however deep — for the one assertion
    /// that has to look at what a default was filled in *with*.
    fn collect_strings(block: &crate::ir::Block, out: &mut Vec<String>) {
        use crate::ir::{ExprKind, StmtKind};
        fn expr(e: &crate::ir::Expr, out: &mut Vec<String>) {
            match &e.kind {
                ExprKind::Lit(crate::parser::ast::Lit::Str(s)) => out.push(s.clone()),
                ExprKind::Construct { fields, .. } => {
                    for (_, f) in fields {
                        expr(f, out);
                    }
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        expr(a, out);
                    }
                }
                _ => {}
            }
        }
        for s in &block.stmts {
            match &s.kind {
                StmtKind::Let { init, .. } => expr(init, out),
                StmtKind::Expr(e) => expr(e, out),
                _ => {}
            }
        }
    }

    /// A `distinct` type keeps its **representation**: a literal reaches it, and
    /// the library's own function reads it back out.
    #[test]
    fn a_distinct_type_keeps_its_representation() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let w := shapes.width_of(7)
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);
    }

    /// An `enum`'s payload and a type **alias**'s expansion: one is matched in
    /// the package that declared it, the other names a type here.
    #[test]
    fn an_enum_payload_and_an_alias_travel() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let n := shapes.describe(.left(3))
    let s := shapes.describe(.right("no"))
    // `Side` is an alias for `Square`, so this is a `Square`.
    let sq: shapes.Side := shapes.Square { side: 2 }
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);
    }

    /// A generic function from a library, instantiated **here**: its bound is
    /// satisfied by an impl that also came out of metadata, and its body is
    /// monomorphized from the IR the library carried.
    #[test]
    fn a_generic_function_from_a_library_is_instantiated_here() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let sq := shapes.Square { side: 3 }
    let d := shapes.doubled.<shapes.Square>(&sq)
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);
    }

    /// A bound's **arguments** travel with the parameter.
    ///
    /// `scaled_by` is declared `<T: Scale.<i32>>` and `Square` implements
    /// `Scale` twice, so the `i32` is the whole of what tells the two apart —
    /// and the tree that wrote it is not in this compilation. The *trait* half
    /// of a bound was already on the parameter's def; this is the half that is
    /// a type, and so had to wait for inference and be written down.
    ///
    /// Asserted against the table rather than through a program on purpose. No
    /// program reaches the old behaviour today: a library's body is inferred
    /// where the library was compiled, and every call it makes through a bound
    /// arrives here already resolved, so nothing asks this question of a
    /// foreign parameter yet. What the recording buys is that the answer is
    /// there when something does, instead of an empty list that reads as "no
    /// arguments were written" — and that the read is not a panic on a tree
    /// this compilation does not have.
    #[test]
    fn a_bounds_arguments_travel_with_the_parameter() {
        let (session, file) = program(
            r#"shapes :: import <shapes>

main :: func () {
    let sq := shapes.Square { side: 3 }
    let n := shapes.scaled_by.<shapes.Square>(&sq)
}
"#,
            &[core(), shapes()],
        );
        clean(&session, file);

        let func = session
            .defs
            .iter()
            .find(|d| d.name.as_str() == "scaled_by")
            .expect("`scaled_by` came out of the library");
        let param = match session.decls.get(&func.id) {
            Some(crate::sema::decl::Decl::Func(f)) => f
                .generics
                .first()
                .and_then(|g| g.def)
                .expect("its parameter has a def"),
            other => panic!("`scaled_by` is not a function: {other:?}"),
        };
        let scale = session
            .defs
            .iter()
            .find(|d| d.kind == crate::sema::def::DefKind::Trait && d.name.as_str() == "Scale")
            .expect("`Scale` came out of the library");
        let decls = crate::sema::decl::Decls::new(&session.defs, &session.asts, &session.decls);
        let args = decls
            .param_bound_args(param, scale.id)
            .expect("the bound was recorded");
        assert_eq!(args, vec![crate::sema::ty::Ty::int(32, true)], "{args:?}");
    }

    /// Whether a function takes a **receiver** travels.
    ///
    /// It is not derivable from the recorded parameters, which leave the
    /// receiver out, and the tree that would say so is not here. The language
    /// server is what asks — a method is offered after `value.` and a free
    /// function is not — so before this every function in `core` and `std`
    /// looked like a method.
    #[test]
    fn whether_a_function_takes_a_receiver_travels() {
        let (session, file) = program("main :: func () { }\n", &[core(), shapes()]);
        clean(&session, file);
        let decls = crate::sema::decl::Decls::new(&session.defs, &session.asts, &session.decls);
        let by_name = |name: &str| {
            session
                .defs
                .iter()
                .find(|d| d.kind == crate::sema::def::DefKind::Func && d.name.as_str() == name)
                .unwrap_or_else(|| panic!("no function `{name}`"))
                .id
        };
        assert!(decls.takes_receiver(by_name("area")), "`area` is a method");
        assert!(
            !decls.takes_receiver(by_name("doubled")),
            "`doubled` is a free function"
        );
    }

    /// The same library read twice — once directly, once as another's
    /// dependency — is one library, and the ids it brought are not placed twice.
    #[test]
    fn a_library_named_twice_is_read_once() {
        let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", "")));
        let path = std::path::Path::new(NOWHERE);
        super::read::load(&mut session, &core().0, &core().1, path, true).expect("core loads");
        let after_first = session.defs.len();
        super::read::load(&mut session, &core().0, &core().1, path, false)
            .expect("core loads again");
        assert_eq!(
            session.defs.len(),
            after_first,
            "reading `core` twice placed its definitions twice"
        );
    }

    /// Metadata of another format is refused by name rather than misread.
    #[test]
    fn metadata_of_another_format_is_refused() {
        let (meta, _) = core().clone();
        let (header, _) = super::read::header(&meta).expect("the header reads");
        assert_eq!(header.format, super::FORMAT);

        // The format is the first field of the header, and the header is
        // postcard: a `u32` there is a varint, so a small value is one byte.
        let at = super::MAGIC.len() + 4;
        let mut bent = meta.clone();
        bent[at] = bent[at].wrapping_add(1);
        let why = super::read::header(&bent).expect_err("a bent format is refused");
        assert!(why.contains("format"), "{why}");
    }
}
