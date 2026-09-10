//! Stage-level tests for semantic analysis: prelude, cross-file imports,
//! packages, glob/selective binding, `#lang` collection, resolution of uses to
//! definitions, and `for` / `.?` desugaring.

use crate::parser::ast::{Ast, NodeId, NodeKind};

use super::def::DefKind;
use super::session::{MemLoader, Session};
use super::{DefMeta, Resolution, analyze};

/// Build a session over an in-memory file set, analyze `entry`, and return it.
fn analyze_mem(files: &[(&str, &str)], entry: &str) -> Session {
    let mut loader = MemLoader::new();
    for (name, src) in files {
        loader = loader.with(name, src);
    }
    let mut session = Session::with_loader(Box::new(loader));
    let file = session.load_entry(entry).expect("entry loads");
    analyze(&mut session, file);
    session
}

/// The entry file id (the first non-core, non-package file loaded).
fn entry_file(session: &Session) -> crate::common::source::FileId {
    // Files whose name doesn't start with '<' and isn't the core package.
    session
        .files
        .keys()
        .copied()
        .filter(|f| !session.pkg_of.contains_key(f))
        .min_by_key(|f| f.0)
        .expect("an entry file")
}

/// A lowered function's body. Every function tested here has one — a `None`
/// body is a declaration (`extern("c") func …`), which these tests never build.
fn body_of(f: &crate::ir::Function) -> &crate::ir::Block {
    f.body.as_ref().expect("a function with a body")
}

/// Find the first node in `ast` matching `pred`.
fn find(ast: &Ast, mut pred: impl FnMut(&NodeKind) -> bool) -> Option<NodeId> {
    ast.ids().find(|&id| pred(&ast.node(id).kind))
}

fn resolution(session: &Session, file: crate::common::source::FileId, node: NodeId) -> Resolution {
    session.asts[&file]
        .meta::<Resolution>(node)
        .expect("node has a resolution")
}

#[test]
fn prelude_types_resolve_to_core() {
    // `Option` is not declared here; it must resolve through the prelude to core.
    let session = analyze_mem(
        &[("main", "x :: func () -> Option.<i32> { return .none }")],
        "main",
    );
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    // The `Option` path inside the return type.
    let path = find(ast, |k| {
        matches!(k, NodeKind::Path { segments } if segments.len() == 1 && segments[0].as_str() == "Option")
    })
    .expect("an `Option` path");
    match resolution(&session, file, path) {
        Resolution::Def(d) => {
            assert_eq!(session.defs.canonical_string(d), "core.Option");
            assert_eq!(session.defs.get(d).kind, DefKind::Enum);
        }
        other => panic!("Option did not resolve to a def: {other:?}"),
    }
}

#[test]
fn primitive_resolves_to_builtin() {
    let session = analyze_mem(&[("main", "x :: func (n: i32) {}")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    let path = find(
        ast,
        |k| matches!(k, NodeKind::Path { segments } if segments[0].as_str() == "i32"),
    )
    .unwrap();
    match resolution(&session, file, path) {
        Resolution::Def(d) => assert_eq!(session.defs.get(d).kind, DefKind::Primitive),
        other => panic!("i32 unresolved: {other:?}"),
    }
}

#[test]
fn arbitrary_width_and_alias_primitives_resolve() {
    // A non-power-of-two width, an alias (`u1` == `bool`), and a float width all
    // synthesize / resolve without error; `i1` and a bad float width do not.
    let session = analyze_mem(&[("main", "a :: func (x: u7, y: f80, z: u1) {}")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);

    let bad = analyze_mem(&[("main", "a :: func (x: i1) {}")], "main");
    assert!(bad.has_errors(), "`i1` should not resolve");
    let bad = analyze_mem(&[("main", "a :: func (x: f100) {}")], "main");
    assert!(bad.has_errors(), "`f100` should not resolve");
}

#[test]
fn cross_file_import_resolves_member() {
    let math = "\
@public add :: func (a: isize, b: isize) -> isize { return a + b }
priv_helper :: func () {}
";
    let use_math = "\
math :: import \"math.nest\"
main :: func () { const a := math.add(1, 2) }
";
    let session = analyze_mem(&[("math", math), ("use_math", use_math)], "use_math");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    // `math.add` is FieldAccess { base: Path[math], name: add }.
    let fa = find(
        ast,
        |k| matches!(k, NodeKind::FieldAccess { name, .. } if name.as_str() == "add"),
    )
    .expect("math.add field access");
    match resolution(&session, file, fa) {
        Resolution::Def(d) => {
            assert_eq!(session.defs.canonical_string(d), "add");
            assert_eq!(session.defs.get(d).kind, DefKind::Func);
        }
        other => panic!("math.add unresolved: {other:?}"),
    }
}

#[test]
fn private_cross_file_member_is_hidden() {
    let math = "priv_helper :: func () {}\n@public add :: func () {}\n";
    let use_math = "\
math :: import \"math.nest\"
main :: func () { math.priv_helper() }
";
    let session = analyze_mem(&[("math", math), ("use_math", use_math)], "use_math");
    // Accessing a private member across files is an error.
    assert!(session.has_errors());
}

#[test]
fn selective_import_binds_named_members() {
    let lib = "@public foo :: func () {}\n@public bar :: func () {}\n";
    let main = "\
{ foo } :: import \"lib.nest\"
main :: func () { foo() }
";
    let session = analyze_mem(&[("lib", lib), ("main", main)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    let call_target = find(
        ast,
        |k| matches!(k, NodeKind::Path { segments } if segments[0].as_str() == "foo"),
    )
    .unwrap();
    assert!(matches!(
        resolution(&session, file, call_target),
        Resolution::Def(_)
    ));
}

#[test]
fn glob_import_brings_members_into_scope() {
    let lib = "@public foo :: func () {}\n";
    let main = "\
* :: import \"lib.nest\"
main :: func () { foo() }
";
    let session = analyze_mem(&[("lib", lib), ("main", main)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
}

#[test]
fn package_import_resolves() {
    let mainsrc = "\
mp :: import <mathpkg>
main :: func () { const x := mp.triple(3) }
";
    // A package is registered by the *path* of its root file, which the session's
    // loader resolves — so an in-memory package root is as valid as an on-disk one.
    let mut session = Session::with_loader(Box::new(
        MemLoader::new()
            .with("main", mainsrc)
            .with("mathpkg", "@public triple :: func (n: isize) -> isize { return n }\n"),
    ));
    session.register_package("mathpkg", "mathpkg");
    let file = session.load_entry("main").unwrap();
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let ast = &session.asts[&file];
    let fa = find(
        ast,
        |k| matches!(k, NodeKind::FieldAccess { name, .. } if name.as_str() == "triple"),
    )
    .unwrap();
    assert!(matches!(resolution(&session, file, fa), Resolution::Def(_)));
}

#[test]
fn core_is_an_ordinary_multi_file_package() {
    // `core` gets no special loading path: it is registered by the path of its
    // root file, and its siblings are reached by ordinary relative imports that
    // re-export into the root. Swapping in a two-file `core` proves the compiler
    // finds its `#lang` items by *tag* — not by name, file, or position — which is
    // the whole reason the library is not baked into the compiler.
    //
    // Note the trait is called `Plus`, not `Add`: only `#lang("add")` matters.
    let loader = MemLoader::new()
        .with("main", "f :: func (a: i32, b: i32) -> i32 { return a + b }\n")
        .with("fakecore", "@public * :: import \"fakeops.nest\"\n")
        .with(
            "fakeops",
            "@public Plus :: #lang(\"add\") trait <Rhs> {\n  Output :: type\n  add :: func (self: Self, rhs: Rhs) -> Self.Output\n}\n",
        );
    let mut session = Session::with_loader(Box::new(loader));
    // Re-registering `core` replaces the default on-disk one.
    session.register_package("core", "fakecore");
    let file = session.load_entry("main").unwrap();
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    // The operator reached the trait in the *sibling* file, not the root.
    let add = session.lang_items.get("add").expect("add lang item");
    assert_eq!(session.defs.get(add).name.as_str(), "Plus");
}

#[test]
fn lang_items_collected_from_core() {
    let session = analyze_mem(&[("main", "x :: func () {}")], "main");
    for tag in [
        "option",
        "result",
        "ordering",
        "add",
        "iterator",
        "try",
        "into_iterator",
    ] {
        assert!(
            session.lang_items.get(tag).is_some(),
            "missing #lang item {tag}"
        );
    }
}

#[test]
fn unknown_name_is_an_error() {
    let session = analyze_mem(&[("main", "x :: func () { does_not_exist() }")], "main");
    assert!(session.has_errors());
}

#[test]
fn defs_carry_canonical_names() {
    let session = analyze_mem(&[("main", "@public greet :: func () {}")], "main");
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    // The ConstBind for greet carries a DefMeta.
    let bind = find(ast, |k| matches!(k, NodeKind::ConstBind { .. })).unwrap();
    let DefMeta(def) = ast.meta::<DefMeta>(bind).expect("def meta on binding");
    assert_eq!(session.defs.get(def).name.as_str(), "greet");
    assert!(session.defs.get(def).vis.is_public());
}

#[test]
fn for_loop_is_desugared() {
    let src = "\
main :: func () {
  for x in items {
    consume(x)
  }
}
";
    // `items` / `consume` are unknown, but desugaring is structural and still
    // runs; check the `for` node was lowered to a block with a loop.
    let session = analyze_mem(&[("main", src)], "main");
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    assert!(
        find(ast, |k| matches!(k, NodeKind::For { .. })).is_none(),
        "a `for` node survived desugaring"
    );
    assert!(
        find(ast, |k| matches!(k, NodeKind::Loop { .. })).is_some(),
        "desugared `for` produced no loop"
    );
}

#[test]
fn try_operator_is_desugared() {
    let src = "\
main :: func () -> isize {
  const x := fallible().?
  return x
}
";
    let session = analyze_mem(&[("main", src)], "main");
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    assert!(
        find(ast, |k| matches!(k, NodeKind::Try { .. })).is_none(),
        "a `.?` node survived desugaring"
    );
    assert!(
        find(ast, |k| matches!(k, NodeKind::MatchExpr { .. })).is_some(),
        "desugared `.?` produced no match"
    );
}

// ===< type inference >===

use super::ty::{FloatWidth, IntWidth, Ty};

/// The finalized [`Ty`] attached to the first node matching `pred`.
fn node_ty(
    session: &Session,
    file: crate::common::source::FileId,
    mut pred: impl FnMut(&NodeKind) -> bool,
) -> Ty {
    let ast = &session.asts[&file];
    let id = ast
        .ids()
        .find(|&id| pred(&ast.node(id).kind))
        .expect("a matching node");
    // A comptime literal keeps its own `comptime_int` / `comptime_float` type and
    // records the conversion its context asked for; the type that matters to
    // these tests is the one it converts *to*.
    if let Some(c) = ast.meta::<crate::sema::infer::Coercion>(id) {
        return c.to;
    }
    ast.meta::<Ty>(id).expect("node has an inferred type")
}

#[test]
fn literal_defaults_to_isize_without_context() {
    let session = analyze_mem(&[("main", "f :: func () { const x := 7 }")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(v)) if *v == 7.into())
    });
    assert_eq!(ty, Ty::isize());
}

#[test]
fn literal_takes_annotated_type() {
    let session = analyze_mem(&[("main", "f :: func () { const x: i32 := 7 }")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(v)) if *v == 7.into())
    });
    assert_eq!(
        ty,
        Ty::Int {
            signed: true,
            width: IntWidth::Fixed(32)
        }
    );
}

#[test]
fn float_literal_defaults_to_f64() {
    // A float literal is a `comptime_float` (an `f128`), but with nothing to pin
    // its width it collapses to `f64`.
    let session = analyze_mem(&[("main", "f :: func () { const x := 1.5 }")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Float(_)))
    });
    assert_eq!(ty, Ty::Float(FloatWidth::F64));
}

#[test]
fn float_literal_too_precise_for_the_default_f64_is_an_error() {
    let session = analyze_mem(
        &[("main", "f :: func () { const x := 1.00000000000000000001 }")],
        "main",
    );
    let msg = format!("{:#?}", session.diagnostics);
    assert!(
        msg.contains("too large or too precise"),
        "expected a width diagnostic, got {msg}"
    );
}

#[test]
fn float_literal_too_precise_is_accepted_at_an_explicit_wide_width() {
    // The collapse to `f64` is what loses the value; an `f128` annotation keeps it.
    let session = analyze_mem(
        &[(
            "main",
            "f :: func () { const x: f128 := 1.00000000000000000001 }",
        )],
        "main",
    );
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Float(_)))
    });
    assert_eq!(ty, Ty::Float(FloatWidth::F128));
}

#[test]
fn type_mismatch_is_reported() {
    // `n` is `i32`; `return n < n` yields `bool`, which cannot be the `i32` result.
    let session = analyze_mem(
        &[("main", "f :: func (n: i32) -> i32 { return n < n }")],
        "main",
    );
    assert!(session.has_errors(), "expected a type mismatch");
}

#[test]
fn call_argument_type_flows_to_literal() {
    let src = "\
g :: func (x: i16) -> i16 { return x }
f :: func () { const y := g(3) }
";
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    // The literal `3` is constrained to the parameter type `i16`.
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(v)) if *v == 3.into())
    });
    assert_eq!(
        ty,
        Ty::Int {
            signed: true,
            width: IntWidth::Fixed(16)
        }
    );
}

// ===< IR lowering >===

use crate::ir::{Expr, Stmt};

#[test]
fn while_lowers_to_loop_with_break() {
    let src = "\
f :: func (n: i32) {
  let i := 0
  while i < n { i = i + 1 }
}
";
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let program = &session.ir[&file];
    let func = program
        .funcs
        .iter()
        .find(|f| f.name.as_str() == "f")
        .expect("func f");
    // Somewhere in the body there is a `loop` whose first statement is an `if`
    // that `break`s (the desugared `while` guard). It may be a statement or the
    // block's tail expression.
    let is_guarded_loop = |e: &Expr| {
        matches!(e, Expr::Loop { body, .. }
            if matches!(body.stmts.first(), Some(Stmt::Expr(Expr::If { .. }))))
    };
    let body = body_of(func);
    let in_stmts = body
        .stmts
        .iter()
        .any(|s| matches!(s, Stmt::Expr(e) if is_guarded_loop(e)));
    let in_tail = body.tail.as_deref().is_some_and(is_guarded_loop);
    assert!(
        in_stmts || in_tail,
        "while did not lower to a guarded loop: {body:#?}"
    );
}

#[test]
fn defer_is_recorded_on_its_block_not_copied_to_exits() {
    let src = "\
cleanup :: func () {}
f :: func () -> i32 {
  defer cleanup()
  if true { return 0 }
  return 1
}
";
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let program = &session.ir[&file];
    let func = program
        .funcs
        .iter()
        .find(|f| f.name.as_str() == "f")
        .expect("func f");
    // The deferred body is recorded once on the block that owns it...
    let body = body_of(func);
    assert_eq!(body.defers.len(), 1, "{body:#?}");
    assert!(
        matches!(&body.defers[0], Expr::Call { .. }),
        "defer body is not the call: {:#?}",
        body.defers
    );
    // ...and is not copied ahead of either `return`, even though there are two.
    assert!(
        !body
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Expr(Expr::Call { .. }))),
        "deferred call was duplicated into the statement list: {:#?}",
        body.stmts
    );
}

#[test]
fn field_access_through_pointer_is_explicit_deref() {
    let src = "\
P :: struct { x: i32 }
get :: func (p: *P) -> i32 { return p.x }
";
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let program = &session.ir[&file];
    let func = program
        .funcs
        .iter()
        .find(|f| f.name.as_str() == "get")
        .expect("func get");
    // The returned `p.x` is `(p.*).x` — a Field over an explicit Deref.
    let ret = body_of(func).stmts.iter().find_map(|s| match s {
        Stmt::Return(Some(e)) => Some(e),
        _ => None,
    });
    let is_deref_field = matches!(
        ret,
        Some(Expr::Field { base, .. }) if matches!(**base, Expr::Deref { .. })
    );
    assert!(
        is_deref_field,
        "field access not lowered to deref+field: {ret:#?}"
    );
}

// ===< IR snapshots (insta) >===

/// Analyze `src` as the entry file and render its lowered IR to text.
fn ir_text(src: &str) -> String {
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file])
}

#[test]
fn ir_snapshot_control_flow_and_defer() {
    // Pointer field auto-deref, `while` -> guarded `loop`, `defer` spliced before
    // the `return` and at block end, literals unified to the field type.
    let src = "\
Point :: struct { x: i32, y: i32 }
cleanup :: func () {}
dist :: func (p: *Point, n: i32) -> i32 {
  let acc := p.x + p.y
  let i := 0
  while i < n {
    acc = acc + i
    i = i + 1
  }
  defer cleanup()
  if acc > 10 { return acc }
  acc
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snapshot_match_and_enum() {
    let src = "\
Color :: enum { red, green, blue }
pick :: func (c: Color) -> i32 {
  return c.match {
    .red => 1,
    .green => 2,
    .blue => 3,
  }
}
maybe :: func (b: bool) -> Option.<i32> {
  if b { return .some(5) }
  return .none
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snapshot_if_match_lowers_to_match() {
    // `if match .some(v) := o { v } else { 0 }` becomes a two-arm `match`.
    let src = "\
choose :: func (o: Option.<i32>) -> i32 {
  if match .some(v) := o {
    return v
  } else {
    return 0
  }
}
";
    insta::assert_snapshot!(ir_text(src));
}

// ===< IR lowering snapshots (extensive) >===
//
// One snapshot per lowering construct, to lock down that each surface form is
// resolved (names → `Local`/`Global`, methods/operators → the right callee) and
// lowered to the expected IR shape with the expected types. `ir_text` asserts
// the program analyzes without diagnostics; `ir_text_lenient` is used only for
// the `.?` / `.!` desugarings, whose enum-payload types are a documented stub
// (they stay `Error`), so the snapshot still shows the lowered structure.

/// Like [`ir_text`], but does not require a clean analysis — for constructs
/// whose types the bootstrap cannot yet fully infer (enum payloads).
fn ir_text_lenient(src: &str) -> String {
    let session = analyze_mem(&[("main", src)], "main");
    let file = entry_file(&session);
    crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file])
}

#[test]
fn ir_snap_scalar_literals() {
    insta::assert_snapshot!(ir_text(
        "lits :: func () {\n  let i := 7\n  let f := 2.5\n  let b := true\n  let c := 'z'\n  let s := \"hi\"\n}\n"
    ));
}

#[test]
fn ir_snap_arithmetic_operators() {
    insta::assert_snapshot!(ir_text(
        "arith :: func (a: i32, b: i32) -> i32 {\n  let s := a + b\n  let d := a - b\n  let m := a * b\n  let q := a / b\n  let r := a % b\n  return s\n}\n"
    ));
}

#[test]
fn ir_snap_bitwise_and_shift_dispatch_through_their_traits() {
    // `& | ^ << >>` reach `BitAnd` / `BitOr` / `BitXor` / `Shl` / `Shr` exactly
    // as `+` reaches `Add` (§6.13). On the integer core the builtin row wins and
    // tags the call, so codegen still emits one instruction — the uniform call
    // shape costs nothing, and a user `impl Shl for BitSet` slots into the same
    // node.
    insta::assert_snapshot!(ir_text(
        "bits :: func (a: i32, b: i32) -> i32 {\n  let an := a & b\n  let orr := a | b\n  let xr := a ^ b\n  let sl := a << b\n  let sr := a >> b\n  return an\n}\n"
    ));
}

#[test]
fn ir_snap_comparisons_stay_primitive() {
    insta::assert_snapshot!(ir_text(
        "cmp :: func (a: i32, b: i32) -> bool {\n  let e := a == b\n  let ne := a != b\n  let lt := a < b\n  let le := a <= b\n  let gt := a > b\n  let ge := a >= b\n  return e\n}\n"
    ));
}

#[test]
fn ir_snap_logical_and_or() {
    insta::assert_snapshot!(ir_text(
        "logic :: func (a: bool, b: bool) -> bool { return a && b || a }\n"
    ));
}

#[test]
fn ir_snap_unary_operators() {
    // `-a` and `~a` are `Neg.neg` / `BitNot.bitnot`; `!a` is not a trait call
    // (boolean negation dispatches on nothing).
    insta::assert_snapshot!(ir_text(
        "un :: func (a: i32, b: bool) -> i32 {\n  let n := -a\n  let bn := ~a\n  let no := !b\n  return n\n}\n"
    ));
}

#[test]
fn ir_snap_ref_and_deref() {
    // Pointers survive into the IR *with their permission*: `&` / `&mut` are
    // `Expr::Ref { mutable }`, `.*` is `Expr::Deref`, and the `mut` rides on the
    // `Ty::Ptr` rather than being tracked beside it. A function that receives a
    // `*mut` is tagged `#mutating` — that pair (the type says what may be
    // written, the tag says who may write) is what the IR-level mutability check
    // reads, and it is why nothing has to re-derive it from the syntax.
    insta::assert_snapshot!(ir_text(
        "rd :: func (p: *i32, q: *mut i32) -> i32 {\n  let r := &p\n  let w := &mut q\n  q.* = 1\n  return p.*\n}\n"
    ));
}

#[test]
fn ir_snap_tuple_and_tuple_index() {
    insta::assert_snapshot!(ir_text(
        "tup :: func () -> isize {\n  let t := (1, 2, 3)\n  return t.0 + t.1 + t.2\n}\n"
    ));
}

#[test]
fn ir_snap_free_call_and_recursion() {
    insta::assert_snapshot!(ir_text(
        "fac :: func (n: isize) -> isize {\n  if n < 2 { return 1 }\n  return n * fac(n - 1)\n}\n"
    ));
}

#[test]
fn ir_snap_method_call() {
    insta::assert_snapshot!(ir_text(
        "P :: struct { x: i32 }\nimpl P {\n  get :: func (self: *P) -> i32 { return self.x }\n}\nuse_m :: func (p: *P) -> i32 { return p.get() }\n"
    ));
}

#[test]
fn ir_snap_struct_construct_named() {
    insta::assert_snapshot!(ir_text(
        "P :: struct { x: i32, y: i32 }\nmk :: func () -> P { return P { x: 1, y: 2 } }\n"
    ));
}

#[test]
fn ir_snap_composite_inferred() {
    insta::assert_snapshot!(ir_text(
        "P :: struct { x: i32 }\nmk :: func () -> P { return .{ x: 9 } }\n"
    ));
}

#[test]
fn ir_snap_array_positional() {
    insta::assert_snapshot!(ir_text(
        "arr :: func () {\n  let a: []i32 := .{ 1, 2, 3 }\n}\n"
    ));
}

#[test]
fn ir_snap_array_repeat() {
    insta::assert_snapshot!(ir_text(
        "rep :: func () {\n  let a: [4]i32 := .{ 0 ; 4 }\n}\n"
    ));
}

#[test]
fn ir_snap_if_else_and_if_no_else() {
    insta::assert_snapshot!(ir_text(
        "br :: func (c: bool) -> i32 {\n  if c { return 1 }\n  let x := if c { 10 } else { 20 }\n  return x\n}\n"
    ));
}

#[test]
fn ir_snap_loop_break_value() {
    insta::assert_snapshot!(ir_text("lp :: func () -> i32 { return loop { break 5 } }\n"));
}

#[test]
fn ir_snap_while_loop() {
    insta::assert_snapshot!(ir_text(
        "wl :: func (n: i32) -> i32 {\n  let mut i := 0\n  while i < n { i = i + 1 }\n  return i\n}\n"
    ));
}

#[test]
fn ir_snap_for_desugars_to_loop() {
    insta::assert_snapshot!(ir_text_lenient(
        "consume :: func (x: i32) {}\nfr :: func (xs: []i32) {\n  for x in xs { consume(x) }\n}\n"
    ));
}

#[test]
fn ir_snap_match_variant_binding() {
    insta::assert_snapshot!(ir_text(
        "mo :: func (o: Option.<i32>) -> i32 {\n  return o.match {\n    .some(v) => v,\n    .none => 0,\n  }\n}\n"
    ));
}

#[test]
fn try_abort_in_a_value_position_does_not_force_void() {
    // `$abort` diverges, so the `.err` arm it sits in contributes no type: the
    // `match` takes the `.ok` arm's, and `.!` is usable where a value is wanted.
    let session = analyze_mem(
        &[(
            "main",
            "f :: func () -> Result.<i32, i32> { return .ok(1) }\ng :: func () -> i32 { return f().! }\n",
        )],
        "main",
    );
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
}

#[test]
fn bounded_type_param_resolves_its_methods_through_the_bound() {
    let src = "\
Weigh :: trait { weight :: func (self: *Self) -> i32 }
heavy :: func <T: Weigh> (t: *T) -> i32 { return t.weight() }
";
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    // The call resolves, and `Self` in the trait's signature became `T`.
    let program = &session.ir[&file];
    let text = crate::ir::pretty::program_to_string(&session.defs, program);
    assert!(text.contains("func(*T) -> i32"), "{text}");
    assert!(!text.contains("<error>"), "{text}");
}

#[test]
fn ir_snap_dyn_coercion_and_dispatch() {
    // `*T` unsizes to `*dyn Trait` when `T: Trait` (§3.2). The IR keeps the
    // erased pointee on the cast so a later stage can pick the vtable, and a
    // method call on the object resolves to the trait's own declaration.
    let src = "\
ToJson :: trait { render :: func (self: *Self) -> i32 }
Cat :: struct { age: i32 }
impl ToJson for Cat {
  render :: func (self: *Cat) -> i32 { return self.age }
}
go :: func (c: *Cat) -> i32 {
  let j: *dyn ToJson := c
  return j.render()
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_using_field_upcasts() {
    // `@using` gives the outer struct an implicit upcast to the field's type
    // (§3.10). Lowering makes it explicit: a value copies the sub-object
    // (`e.t`), a pointer takes its address (`&e.t`).
    let src = "\
Transform :: struct { x: i32, y: i32 }
Entity :: struct {
  @using t: Transform,
  hp: i32,
}
take :: func (t: Transform) -> i32 { return t.x }
nudge :: func (t: *mut Transform) {}
impl Transform {
  mag :: func (self: Transform) -> i32 { return self.x }
}
up :: func (e: Entity, p: *mut Entity) -> i32 {
  nudge(p)
  let m := e.mag()
  return take(e)
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_patterns_keep_their_structure() {
    // Struct / slice / range / `@` / `&` patterns are retained in the IR rather
    // than collapsing to a wildcard: a later decision-tree pass needs the
    // fields, the `..` position, and the `..<` / `..=` distinction.
    let src = "\
Point :: struct { x: i32, y: i32 }
ps :: func (p: Point, xs: []i32, n: i32, r: *i32) -> i32 {
  let a := p.match {
    { x: 0, y } => y,
    { x, .. } => x,
  }
  let c := xs.match {
    [first, .. rest, last] => first,
    [] => 0,
  }
  let d := n.match {
    0..<10 => 1,
    10..=20 => 2,
    big @ 21 => big,
    _ => 0,
  }
  let e := r.match {
    &v => v,
  }
  return a
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_match_guard_or_and_literal() {
    insta::assert_snapshot!(ir_text(
        "ml :: func (n: i32) -> i32 {\n  return n.match {\n    0 => 10,\n    1 | 2 => 20,\n    x if x > 5 => 30,\n    _ => 0,\n  }\n}\n"
    ));
}

#[test]
fn ir_snap_defer_multiple_recorded_once() {
    insta::assert_snapshot!(ir_text(
        "cleanup :: func () {}\ndm :: func (c: bool) -> i32 {\n  defer cleanup()\n  defer cleanup()\n  if c { return 1 }\n  return 2\n}\n"
    ));
}

#[test]
fn ir_snap_try_propagate_branches_on_control_flow() {
    // `.?` is a `Try.branch` plus a two-arm match on `ControlFlow`; the failure
    // arm returns `FromResidual.from_residual`, resolved to the impl for the
    // *enclosing function's* return type.
    insta::assert_snapshot!(ir_text(
        "fallible :: func () -> Result.<i32, i32> { return .ok(1) }\ntp :: func () -> Result.<i32, i32> {\n  const x := fallible().?\n  return .ok(x)\n}\n"
    ));
}

#[test]
fn ir_snap_try_propagate_on_an_option() {
    // The same shape for a type whose residual carries nothing: `Option`'s
    // `from_residual` rebuilds `.none`.
    insta::assert_snapshot!(ir_text(
        "head :: func () -> Option.<i32> { return .none }\ntpo :: func () -> Option.<i32> {\n  const x := head().?\n  return .some(x)\n}\n"
    ));
}

#[test]
fn ir_snap_try_abort_lowers_to_unwrap() {
    insta::assert_snapshot!(ir_text(
        "fallible :: func () -> Result.<i32, i32> { return .ok(1) }\nta :: func () -> i32 {\n  const x := fallible().!\n  return x\n}\n"
    ));
}

#[test]
fn ir_snap_const_generic_length_and_len() {
    // `N` stays symbolic inside the generic body and is solved per call site;
    // `.len()` is `core`'s inherent method on the sequences; its body is `$len`.
    insta::assert_snapshot!(ir_text(
        "count :: func <const N: usize, T> (a: [N]T) -> usize { return a.len() }\nf :: func (s: []i32) -> usize {\n  const a := [_]i32 { 1, 2, 3 }\n  return count(a) + a.len() + $len(s)\n}\n"
    ));
}

#[test]
fn ir_snap_array_unsizes_to_a_slice() {
    insta::assert_snapshot!(ir_text(
        "take :: func (s: []i32) -> usize { return s.len() }\nf :: func () -> usize {\n  const a := [_]i32 { 1, 2, 3 }\n  return take(a)\n}\n"
    ));
}

#[test]
fn ir_snap_static_trait_call_targets_the_selected_impl() {
    insta::assert_snapshot!(ir_text(
        "Make :: trait { make :: func (n: i32) -> Self }\nW :: struct { v: i32 }\nimpl Make for W { make :: func (n: i32) -> W { return W { v: n } } }\nf :: func () -> W { return Make.make(3) }\n"
    ));
}

#[test]
fn ir_snap_intrinsic_call() {
    insta::assert_snapshot!(ir_text(
        "ic :: func (x: i64) -> i32 { return $cast.<i32>(x) }\n"
    ));
}

#[test]
fn ir_snap_index_and_slice() {
    insta::assert_snapshot!(ir_text(
        "ix :: func (a: []i32) -> i32 {\n  let b := a[1..<3]\n  return a[0]\n}\n"
    ));
}

#[test]
fn ir_snap_range_forms_pick_variants() {
    // Every surface range form keeps its bound count and its `..<` / `..=`
    // distinction as a distinct `#lang("range")` enum variant.
    insta::assert_snapshot!(ir_text(
        "rs :: func (a: []i32) {\n  let e := a[1..<3]\n  let i := a[1..=3]\n  let f := a[1..]\n  let t := a[..<3]\n  let ti := a[..=3]\n  let u := a[..]\n}\n"
    ));
}

#[test]
fn ir_snap_global_const_reference() {
    insta::assert_snapshot!(ir_text("G :: 42\ngref :: func () -> isize { return G }\n"));
}

#[test]
fn ir_snap_nested_block_expression() {
    insta::assert_snapshot!(ir_text(
        "nb :: func () -> i32 {\n  let x := {\n    let y := 1\n    y + 2\n  }\n  return x\n}\n"
    ));
}

#[test]
fn ir_snap_auto_deref_field_chain() {
    insta::assert_snapshot!(ir_text(
        "Q :: struct { v: i32 }\nP :: struct { inner: Q }\nchain :: func (p: *P) -> i32 { return p.inner.v }\n"
    ));
}

#[test]
fn ir_snap_user_operators_reach_their_impls() {
    // Every operator that is a trait call, on a type that implements it (§6.13):
    // the prefix unaries, a **heterogeneous** shift (`Rhs` is `i32`, not `Self`,
    // which is why the operands cannot be forced equal before selection),
    // equality, the four relations through the one `Ord.cmp`, and indexing on
    // both sides of an assignment.
    let src = "\
Bits :: struct { w: i32 }
impl Neg for Bits { Output :: Bits  neg :: func (self: Bits) -> Bits { return self } }
impl BitNot for Bits { Output :: Bits  bitnot :: func (self: Bits) -> Bits { return self } }
impl Shl.<i32> for Bits { Output :: Bits  shl :: func (self: Bits, rhs: i32) -> Bits { return self } }
impl Eq for Bits { eq :: func (self: Bits, rhs: Bits) -> bool { return true } }
impl Ord for Bits { cmp :: func (self: Bits, rhs: Bits) -> Ordering { return .equal } }
impl Index.<i32> for Bits { Output :: i32  index :: func (self: *Bits, i: i32) -> *i32 { return &self.w } }
impl IndexMut.<i32> for Bits { Output :: i32  index_mut :: func (self: *mut Bits, i: i32) -> *mut i32 { return &mut self.w } }
ops :: func (a: Bits, b: Bits, g: *mut Bits) -> bool {
  const n := -a
  const c := ~a
  const s := a << 2
  const ne := a != b
  const lt := a < b
  const r := g[1]
  g[2] = 9
  return ne && lt
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn an_operator_operand_is_checked_against_the_impls_own_signature() {
    // `Shl.<i32>` shifts by an `i32`. Shifting by a `Bits` is not a mismatch the
    // operator decided — it is that no impl of `Shl.<Bits>` exists, which is
    // what the operands being free to differ makes it possible to say.
    let src = "\
Bits :: struct { w: i32 }
impl Shl.<i32> for Bits { Output :: Bits  shl :: func (self: Bits, rhs: i32) -> Bits { return self } }
f :: func (a: Bits, b: Bits) -> Bits { return a << b }
";
    assert!(
        first_error(src).contains("does not implement `core.Shl.<Bits>`"),
        "{src}"
    );
}

#[test]
fn two_impls_on_one_type_may_each_bind_output() {
    // A type's namespace hosts every trait impl on it, so `Add` and `Mul` both
    // park an `Output` there. Neither redeclares the other: the projection goes
    // through the impl that bound it.
    analyze_clean(
        "V :: struct { n: i32 }\nimpl Add for V { Output :: V  add :: func (self: V, rhs: V) -> V { return self } }\nimpl Mul for V { Output :: V  mul :: func (self: V, rhs: V) -> V { return self } }\nf :: func (a: V, b: V) -> V { return a + b * a }\n",
    );
}

#[test]
fn a_destructuring_let_keeps_its_pattern_in_the_ir() {
    // `let (a, b) := t` binds two names; the IR keeps the whole pattern rather
    // than dropping to an initializer evaluated for effect.
    let s = analyze_clean("f :: func (t: (i32, i32)) -> i32 {\n  const (a, b) := t\n  return a\n}\n");
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("let (a, b): (i32, i32)"), "{ir}");
}

// ===< `Self`, dynamic dispatch, and coherence >===

#[test]
fn ir_snap_self_resolves_per_impl_kind() {
    // `Self` is the *implementing* type, and there are four ways a call can
    // arrive at a signature that mentions it. Each lands on a different answer,
    // and the IR shows all four side by side:
    //
    //   - an inherent impl        -> the type itself (`Counter`)
    //   - a trait impl            -> the impl's target (`Counter` again, but via
    //                                the impl's own signature)
    //   - a bounded type param    -> the parameter (`T`), still generic
    //   - a trait object          -> `dyn Step`, the erased type
    //
    // The last two are the interesting ones: neither has picked an impl, so the
    // call carries the trait and waits — `#generic` for monomorphization,
    // `#virtual` for the vtable.
    let src = "\
Step :: trait {
  step :: func (self: *Self) -> i32
  twice :: func (self: *Self) -> i32
}
Counter :: struct { n: i32 }
impl Counter {
  bump :: func (self: *Self) -> i32 { return self.n }
}
impl Step for Counter {
  step :: func (self: *Self) -> i32 { return self.n }
  twice :: func (self: *Self) -> i32 { return self.n }
}
inherent :: func (c: *Counter) -> i32 { return c.bump() }
concrete :: func (c: *Counter) -> i32 { return c.step() }
bounded  :: func <T: Step> (t: *T) -> i32 { return t.step() }
erased   :: func (d: *dyn Step) -> i32 { return d.step() }
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn self_in_a_structural_impl_is_the_structural_target() {
    // `impl <T> []T` has no named type for `Self` to point at, and it still
    // means the target: `self: *Self` there is a `*[]T`. This is the same
    // binding `core` relies on for `.len()`.
    let s = analyze_clean(
        "Sum :: trait { sum :: func (self: *Self) -> usize }\nimpl <T> Sum for []T { sum :: func (self: *Self) -> usize { return $len(self) } }\nf :: func (s: []i32) -> usize { return s.sum() }\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("(&s: []i32): *[]i32"), "{ir}");
}

#[test]
fn a_trait_object_is_only_a_type_behind_a_pointer() {
    // `dyn Trait` is unsized — its size is the erased type's, which is exactly
    // what the type no longer says. Every position that needs a size rejects it.
    for src in [
        "T1 :: trait { m :: func (self: *Self) -> i32 }\nf :: func (x: dyn T1) -> i32 { return 0 }\n",
        "T1 :: trait { m :: func (self: *Self) -> i32 }\nS :: struct { f: dyn T1 }\n",
        "T1 :: trait { m :: func (self: *Self) -> i32 }\nf :: func (xs: []dyn T1) -> i32 { return 0 }\n",
    ] {
        assert!(
            first_error(src).contains("use it behind a pointer"),
            "{src}"
        );
    }
    // Behind one it is fine, including as a slice's *element*: the pointer is
    // what has the size.
    analyze_clean(
        "T1 :: trait { m :: func (self: *Self) -> i32 }\nf :: func (xs: []*dyn T1, y: *mut dyn T1) -> i32 { return 0 }\n",
    );
}

#[test]
fn dyn_needs_a_trait() {
    assert!(
        first_error("f :: func (x: *dyn i32) -> i32 { return 0 }\n")
            .contains("is not a trait"),
    );
}

#[test]
fn ir_snap_dyn_mutating_dispatch_and_explicit_cast() {
    // A `*mut T` unsizes to a `*mut dyn Trait`, which is what lets a mutating
    // method be called through the object; and `$cast.<*dyn Trait>(p)` is the
    // *same* unsizing written out, so it lowers to the same fat-pointer node
    // rather than to a reinterpretation of bits.
    let src = "\
Draw :: trait {
  area :: func (self: *Self) -> i32
  scale :: func (self: *mut Self, k: i32)
}
Box2 :: struct { w: i32 }
impl Draw for Box2 {
  area :: func (self: *Self) -> i32 { return self.w }
  scale :: func (self: *mut Self, k: i32) { self.w = self.w * k }
}
use_dyn :: func (b: *Box2, m: *mut Box2) -> i32 {
  const explicit := $cast.<*dyn Draw>(b)
  let w: *mut dyn Draw := m
  w.scale(2)
  return explicit.area()
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn an_inherent_impl_must_live_where_its_type_is_defined() {
    // Two libraries each adding a `double` to `i32` would be an unresolvable
    // clash at every call site, and an inherent method has no trait name to
    // qualify it with. Only the defining package may write one (§4.8).
    assert!(
        first_error("impl i32 { double :: func (self: i32) -> i32 { return self } }\n")
            .contains("must live where its type is defined"),
    );
    // The built-in sequences are the language's — which for this purpose means
    // `core`'s. That is *why* `.len()` lives there.
    assert!(
        first_error("impl <T> []T { first :: func (self: *Self) -> usize { return 0 } }\n")
            .contains("must live where its type is defined"),
    );
    // The defining program may, of course.
    analyze_clean(
        "Mine :: struct { n: i32 }\nimpl Mine { get :: func (self: Mine) -> i32 { return self.n } }\n",
    );
}

#[test]
fn a_trait_impl_needs_the_trait_or_the_type_to_be_its_own() {
    // Foreign trait + foreign type is the pair two libraries can write
    // identically with no way to prefer either.
    assert!(
        first_error(
            "impl Eq for Option.<i32> { eq :: func (self: Option.<i32>, rhs: Option.<i32>) -> bool { return true } }\n"
        )
        .contains("both belong to other packages"),
    );
    // A local trait on a foreign type is fine…
    analyze_clean(
        "Tag :: trait { tag :: func (self: Self) -> i32 }\nimpl Tag for i32 { tag :: func (self: i32) -> i32 { return 0 } }\n",
    );
    // …and so is a foreign trait whose self type *mentions* a local type, even
    // though its head does not: `Cfg` is what makes this impl this package's
    // business.
    analyze_clean(
        "Cfg :: struct { n: i32 }\nimpl Eq for Result.<i32, Cfg> {\n  eq :: func (self: Result.<i32, Cfg>, rhs: Result.<i32, Cfg>) -> bool { return true }\n}\n",
    );
}

// ===< examples smoke test >===

// ===< trait system: selection, projection, operators >===

use super::def::DefId;
use crate::ir::{BuiltinOp, Program};
use crate::parser::ast::BinOp;

/// Analyze a single entry file `main`.
fn analyze1(src: &str) -> Session {
    analyze_mem(&[("main", src)], "main")
}

/// A user `impl Add for Vec3` whose `Output` is `Vec3`.
const VEC3_ADD: &str = "\
Vec3 :: struct { x: i32 }
impl Add for Vec3 {
  Output :: Vec3
  add :: func (self: Vec3, rhs: Vec3) -> Vec3 { return self }
}
";

/// The finalized type of the first `Binary` node with operator `op`.
fn binop_ty(session: &Session, file: crate::common::source::FileId, op: BinOp) -> Ty {
    node_ty(
        session,
        file,
        |k| matches!(k, NodeKind::Binary { op: o, .. } if *o == op),
    )
}

/// Whether a (finalized) type is the nominal type whose canonical name ends in
/// `name`.
fn is_nominal_named(session: &Session, ty: &Ty, name: &str) -> bool {
    matches!(ty, Ty::Nominal { def, .. } if session.defs.canonical_string(*def).ends_with(name))
}

fn i32_ty() -> Ty {
    Ty::Int {
        signed: true,
        width: IntWidth::Fixed(32),
    }
}

fn diag_contains(session: &Session, needle: &str) -> bool {
    session
        .diagnostics
        .iter()
        .any(|d| d.message.contains(needle))
}

/// Collect every lowered `Call` in a program as `(builtin tag, callee def)`.
fn calls_of(program: &Program) -> Vec<(Option<BuiltinOp>, Option<DefId>)> {
    struct C(Vec<(Option<BuiltinOp>, Option<DefId>)>);
    impl crate::ir::Visitor for C {
        fn visit_expr(&mut self, e: &Expr) {
            if let Expr::Call {
                builtin, callee, ..
            } = e
            {
                let def = match callee.as_ref() {
                    Expr::Global(d, _) => Some(*d),
                    _ => None,
                };
                self.0.push((*builtin, def));
            }
            crate::ir::walk_expr(self, e);
        }
    }
    let mut c = C(Vec::new());
    for f in &program.funcs {
        crate::ir::Visitor::visit_function(&mut c, f);
    }
    c.0
}

// --- operators: builtins ---

#[test]
fn operator_i32_projects_to_i32() {
    let s = analyze1("f :: func (a: i32, b: i32) -> i32 { return a + b }");
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    assert_eq!(binop_ty(&s, file, BinOp::Add), i32_ty());
}

#[test]
fn every_builtin_arithmetic_op_types_and_lowers() {
    // Each of + - * / % projects `Output = Self` and lowers to a builtin call.
    for (op, sym, tag) in [
        (BinOp::Add, "+", BuiltinOp::Add),
        (BinOp::Sub, "-", BuiltinOp::Sub),
        (BinOp::Mul, "*", BuiltinOp::Mul),
        (BinOp::Div, "/", BuiltinOp::Div),
        (BinOp::Rem, "%", BuiltinOp::Rem),
    ] {
        let src = format!("f :: func (a: i32, b: i32) -> i32 {{ return a {sym} b }}");
        let s = analyze1(&src);
        assert!(!s.has_errors(), "op {sym}: {:#?}", s.diagnostics);
        let file = entry_file(&s);
        assert_eq!(binop_ty(&s, file, op), i32_ty(), "op {sym} type");
        let tags: Vec<_> = calls_of(&s.ir[&file]).into_iter().map(|(b, _)| b).collect();
        assert!(
            tags.contains(&Some(tag)),
            "op {sym} missing builtin tag {tag:?}: {tags:?}"
        );
    }
}

#[test]
fn operator_mixed_widths() {
    // Distinct fixed widths each select their own primitive; a literal adapts to
    // the operand width.
    let s =
        analyze1("f :: func (a: i16, b: i64) -> i64 { let x := a + a  let y := b + 1  return y }");
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    // The literal `1` was pinned to `i64` by its operand.
    let ty = node_ty(&s, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(v)) if *v == 1.into())
    });
    assert_eq!(
        ty,
        Ty::Int {
            signed: true,
            width: IntWidth::Fixed(64)
        }
    );
}

#[test]
fn operator_result_pinned_late_by_return() {
    // The width of `x + 2` is only known from the function's return type — the
    // obligation is solved after backward flow (builtin matches the numeric var).
    let s = analyze1("f :: func () -> i16 { let x := 1  let y := x + 2  return y }");
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    assert_eq!(
        binop_ty(&s, file, BinOp::Add),
        Ty::Int {
            signed: true,
            width: IntWidth::Fixed(16)
        }
    );
}

#[test]
fn builtin_op_is_o1_recognizable_in_ir() {
    let s = analyze1("f :: func (a: i32, b: i32) -> i32 { return a + b }");
    let file = entry_file(&s);
    let calls = calls_of(&s.ir[&file]);
    assert_eq!(calls.len(), 1, "one lowered call");
    // The builtin tag is present, so codegen recognizes the primitive op in O(1).
    assert_eq!(calls[0].0, Some(BuiltinOp::Add));
}

// --- operators / projection: user impls ---

#[test]
fn user_add_projects_its_output() {
    let src = format!("{VEC3_ADD}f :: func (a: Vec3, b: Vec3) -> Vec3 {{ return a + b }}");
    let s = analyze1(&src);
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    let ty = binop_ty(&s, file, BinOp::Add);
    assert!(
        is_nominal_named(&s, &ty, "Vec3"),
        "expected Vec3, got {ty:?}"
    );
}

#[test]
fn user_operator_lowers_to_trait_call_not_builtin() {
    let src = format!("{VEC3_ADD}f :: func (a: Vec3, b: Vec3) -> Vec3 {{ return a + b }}");
    let s = analyze1(&src);
    let file = entry_file(&s);
    let calls = calls_of(&s.ir[&file]);
    // The `a + b` call is a real trait-method call (no builtin tag) to `add`.
    let op_call = calls
        .iter()
        .find(|(b, def)| b.is_none() && def.is_some_and(|d| s.defs.get(d).name.as_str() == "add"))
        .expect("a user `add` trait call");
    assert_eq!(op_call.0, None, "user operator must not be builtin-tagged");
}

#[test]
fn generic_impl_serves_a_family_and_projects_via_its_argument() {
    // `impl <T> Add for Wrap.<T>` applies to `Wrap.<i32>`, and its projected
    // `Output = Wrap.<T>` resolves to `Wrap.<i32>` — the impl (and its
    // projection) is chosen by the generic argument.
    let src = "\
Wrap :: struct <T> { v: T }
impl <T> Add for Wrap.<T> {
  Output :: Wrap.<T>
  add :: func (self: Wrap.<T>, rhs: Wrap.<T>) -> Wrap.<T> { return self }
}
f :: func (a: Wrap.<i32>, b: Wrap.<i32>) -> Wrap.<i32> { return a + b }
";
    let s = analyze1(src);
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    let ty = binop_ty(&s, file, BinOp::Add);
    match ty {
        Ty::Nominal { def, args } => {
            assert!(s.defs.canonical_string(def).ends_with("Wrap"));
            assert_eq!(args, vec![i32_ty()], "Output projected to Wrap.<i32>");
        }
        other => panic!("expected Wrap.<i32>, got {other:?}"),
    }
}

#[test]
fn concrete_impl_beats_generic() {
    // A blanket `impl <T> Tag for T` and a concrete `impl Tag for Foo` both
    // apply to `Foo`; the concrete one wins with no ambiguity. The trait is
    // declared here rather than reused from `core` because a blanket impl of a
    // foreign trait is exactly what coherence forbids (§4.8).
    let src = "\
Tag :: trait { tag :: func (self: Self) -> i32 }
Foo :: struct { n: i32 }
impl <T> Tag for T { tag :: func (self: T) -> i32 { return 0 } }
impl Tag for Foo { tag :: func (self: Foo) -> i32 { return 1 } }
f :: func (a: Foo) -> i32 { return a.tag() }
";
    let s = analyze1(src);
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    // The call targets the concrete impl's `tag`, not the blanket one — the
    // body that returns `1`.
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    let concrete = ir
        .lines()
        .any(|l| l.contains("func tag(self: Foo)"));
    assert!(concrete, "{ir}");
    assert!(ir.contains("(Foo.tag: func(Foo) -> i32)"), "{ir}");
}

#[test]
fn two_equally_specific_impls_are_ambiguous() {
    // A user `impl Add for i32` is exactly as specific as the builtin row for
    // the integer family, so a concrete `i32 + i32` has no best choice. (It is
    // also an orphan-rule violation — the point here is only that selection
    // reports the tie rather than silently picking one.)
    let src = "\
impl Add for i32 { Output :: i32  add :: func (self: i32, rhs: i32) -> i32 { return self } }
f :: func (a: i32, b: i32) -> i32 { return a + b }
";
    let s = analyze1(src);
    assert!(s.has_errors());
    assert!(
        diag_contains(&s, "multiple applicable impls"),
        "{:#?}",
        s.diagnostics
    );
}

#[test]
fn several_impls_matching_an_unknown_self_defer_rather_than_conflict() {
    // Two impls both fit only because nothing says what `a` is. That is a
    // missing annotation, not a tie between impls — reporting an ambiguity here
    // would also break every obligation whose self type is solved later (a `.?`
    // learns its `FromResidual` impl from the function's return type).
    let src = "\
Foo :: struct { n: i32 }
Bar :: struct { n: i32 }
impl Add for Foo { Output :: Foo  add :: func (self: Foo, rhs: Foo) -> Foo { return self } }
impl Add for Bar { Output :: Bar  add :: func (self: Bar, rhs: Bar) -> Bar { return self } }
f :: func (a, b) {
  a + b
  return
}
";
    let s = analyze1(src);
    assert!(s.has_errors());
    assert!(
        diag_contains(&s, "type annotations needed"),
        "{:#?}",
        s.diagnostics
    );
    assert!(
        !diag_contains(&s, "multiple applicable impls"),
        "{:#?}",
        s.diagnostics
    );
}

#[test]
fn operator_on_type_without_impl_is_an_error() {
    let s = analyze1("f :: func (a: bool, b: bool) -> bool { return a + b }");
    assert!(s.has_errors());
    assert!(
        diag_contains(&s, "does not implement"),
        "{:#?}",
        s.diagnostics
    );
}

#[test]
fn chained_operators_all_resolve() {
    // Solving the inner `+` unblocks the outer one; every node types to `i32`.
    let s = analyze1("f :: func (a: i32, b: i32, c: i32) -> i32 { return a + b * c }");
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    assert_eq!(binop_ty(&s, file, BinOp::Add), i32_ty());
    assert_eq!(binop_ty(&s, file, BinOp::Mul), i32_ty());
}

// --- comparison trait bounds ---

#[test]
fn comparing_primitives_needs_no_impl() {
    let s = analyze1("f :: func (a: i32, b: i32) -> bool { return a < b }");
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
}

#[test]
fn comparing_a_user_type_requires_eq() {
    // `==` on a user struct witnesses `Eq`; provided, it type-checks.
    let ok = "\
Id :: struct { n: i32 }
impl Eq for Id { eq :: func (self: Id, rhs: Id) -> bool { return true } }
f :: func (a: Id, b: Id) -> bool { return a == b }
";
    let s = analyze1(ok);
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);

    // Without an `Eq` impl it is a "does not implement" error.
    let bad = "\
Id :: struct { n: i32 }
f :: func (a: Id, b: Id) -> bool { return a == b }
";
    let s = analyze1(bad);
    assert!(s.has_errors());
    assert!(diag_contains(&s, "does not implement"), "{:#?}", s.diagnostics);
}

// --- fulfillment / diagnostics ---

#[test]
fn unconstrained_element_type_needs_annotation() {
    // `Vector.new()` with no later `push` leaves the element variable unsolved:
    // a real "type annotations needed", no longer silently swallowed.
    let src = "\
Vector :: struct <T> { len: usize }
impl <T> Vector.<T> {
  new :: func () -> Vector.<T> { return Vector.<T> { len: 0 } }
}
f :: func () { let xs := Vector.new() }
";
    let s = analyze1(src);
    assert!(s.has_errors());
    assert!(
        diag_contains(&s, "type annotations needed"),
        "{:#?}",
        s.diagnostics
    );
}

// --- in-scope trait filtering ---

#[test]
fn only_in_scope_traits_are_selection_candidates() {
    // `core.Add` is always in scope (prelude); a user trait defined in another
    // file is a candidate only where it is imported by name. The candidate set
    // is exactly what impl selection filters on.
    let lib = "@public MyTrait :: trait { m :: func (self: Self) -> i32 }\n";

    // The entry namespace-imports `lib` (so `MyTrait` is loaded but not brought
    // into scope as a bare name).
    let ns_import = analyze_mem(
        &[
            ("lib", lib),
            ("main", "lib :: import \"lib.nest\"\nf :: func () {}\n"),
        ],
        "main",
    );
    let entry = entry_file(&ns_import);
    let mytrait = ns_import
        .defs
        .iter()
        .find(|d| d.kind == DefKind::Trait && d.name.as_str() == "MyTrait")
        .map(|d| d.id)
        .expect("MyTrait def loaded");
    let add = ns_import
        .defs
        .resolve_alias(ns_import.lang_items.get("add").expect("core.Add"));
    let set = super::infer::in_scope_traits(
        &ns_import.defs,
        &ns_import.prelude_globs,
        ns_import.files[&entry].ns,
    );
    assert!(set.contains(&add), "Add is always in scope via the prelude");
    assert!(
        !set.contains(&mytrait),
        "a merely-loaded trait is not in scope"
    );

    // Selectively importing `MyTrait` makes it a candidate.
    let selective = analyze_mem(
        &[
            ("lib", lib),
            (
                "main",
                "{ MyTrait } :: import \"lib.nest\"\nf :: func () {}\n",
            ),
        ],
        "main",
    );
    let entry2 = entry_file(&selective);
    let mytrait2 = selective
        .defs
        .iter()
        .find(|d| d.kind == DefKind::Trait && d.name.as_str() == "MyTrait")
        .map(|d| d.id)
        .expect("MyTrait def loaded");
    let set2 = super::infer::in_scope_traits(
        &selective.defs,
        &selective.prelude_globs,
        selective.files[&entry2].ns,
    );
    assert!(set2.contains(&mytrait2), "an imported trait is a candidate");
}

// --- IR snapshots ---

#[test]
fn ir_snapshot_operator_builtin_i32() {
    insta::assert_snapshot!(ir_text(
        "f :: func (a: i32, b: i32) -> i32 { return a + b }"
    ));
}

#[test]
fn ir_snapshot_operator_user_vec3() {
    let src = format!("{VEC3_ADD}combine :: func (a: Vec3, b: Vec3) -> Vec3 {{ return a + b }}");
    insta::assert_snapshot!(ir_text(&src));
}

#[test]
fn examples_analyze_without_errors() {
    // Every shipped example must pass the whole pipeline (parse -> resolve ->
    // desugar -> infer -> lower) with no diagnostics, guarding against
    // regressions from type-name or inference changes.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("examples dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("nest") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let mut session = Session::new();
        let file = session
            .sources
            .add(path.to_string_lossy().into_owned(), src.clone());
        let (ast, errs) = crate::parser::parse::Parser::parse_file(&src, file);
        assert!(errs.is_empty(), "parse errors in {path:?}: {errs:?}");
        session.asts.insert(file, ast);
        analyze(&mut session, file);
        assert!(
            !session.has_errors(),
            "{path:?} produced diagnostics: {:#?}",
            session.diagnostics
        );
        checked += 1;
    }
    assert!(
        checked >= 5,
        "expected to check the example files, saw {checked}"
    );
}

#[test]
fn ir_snapshot_generic_element_inferred_from_push() {
    // The container's element type is fixed by the `.push` call site, not by the
    // `.new()` construction: `xs : Vector.<isize>`, `ws : Vector.<str>`.
    let src = "\
Vector :: struct <T> { len: usize }
impl <T> Vector.<T> {
  new :: func () -> Vector.<T> { return Vector.<T> { len: 0 } }
  push :: func (self: *mut Vector.<T>, value: T) { self.len = self.len + 1 }
}
build_ints :: func () -> usize {
  let xs := Vector.new()
  xs.push(1)
  return xs.len
}
build_strings :: func () {
  let ws := Vector.new()
  ws.push(\"hi\")
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_comptime_casts_are_explicit() {
    // A literal is a `comptime_int` with no runtime representation; every point
    // one becomes a runtime integer is an explicit `$cast` in the IR, so no
    // conversion is left implicit for a later stage to rediscover.
    insta::assert_snapshot!(ir_text(
        "P :: struct { x: i32 }\ncc :: func (n: i32) -> i32 {\n  let a: i8 := 5\n  const p := P { x: 1 }\n  return n + 2\n}\n"
    ));
}

// ===< regressions: bugs found by the language-wide audit >===

/// Assert `src` analyzes with no diagnostics, returning the session.
fn analyze_clean(src: &str) -> Session {
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    session
}

/// The first diagnostic message `src` produces.
fn first_error(src: &str) -> String {
    let session = analyze_mem(&[("main", src)], "main");
    session
        .diagnostics
        .first()
        .map(|d| d.message.clone())
        .unwrap_or_else(|| panic!("expected a diagnostic, got none"))
}

#[test]
fn namespace_constant_is_comptime_and_types_each_use_on_its_own() {
    // `A :: 42` has no single runtime type: it settles per use site.
    let session = analyze_clean("A :: 42\nf :: func () {\n  const a: i8 := A\n  const b: i64 := A\n}\n");
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    assert!(text.contains("let a: i8"), "{text}");
    assert!(text.contains("let b: i64"), "{text}");
}

#[test]
fn inferred_composite_literal_takes_its_type_from_context() {
    // `.{ ... }` learns its type from the annotation, the parameter, or the
    // return type — which unification only knows *after* the body is walked.
    let src = "\
P :: struct { x: i32, y: i32 }
take :: func (p: P) -> i32 { return p.x }
mk :: func () -> P { return .{ x: 1, y: 2 } }
f :: func () -> i32 {
  const p: P := .{ x: 1, y: 2 }
  return take(.{ x: 3, y: 4 })
}
";
    let session = analyze_clean(src);
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    // Every one is a real struct construction, not an untyped bag of values.
    assert_eq!(text.matches("P { x:").count(), 3, "{text}");
    assert!(!text.contains("aggregate"), "{text}");
}

#[test]
fn composite_literal_fields_are_checked_against_the_struct() {
    assert!(first_error("P :: struct { x: i32, y: i32 }\nf :: func () { const p := P { x: 1 } }\n")
        .contains("missing field `y`"));
    assert!(first_error("P :: struct { x: i32 }\nf :: func () { const p := P { x: 1, z: 2 } }\n")
        .contains("has no field `z`"));
    assert!(
        first_error("P :: struct { x: i32 }\nf :: func () { const p := P { x: 1, x: 2 } }\n")
            .contains("more than once")
    );
    assert!(first_error("f :: func () { const a: [3]i32 := .{ 1, 2 } }\n")
        .contains("2 element(s) but `[3]i32` needs 3"));
}

#[test]
fn explicit_type_arguments_instantiate_the_callee() {
    let session = analyze_clean(
        "id :: func <T> (x: T) -> T { return x }\nf :: func () { const a := id.<i32>(1) }\n",
    );
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    assert!(text.contains("let a: i32"), "{text}");
    assert!(
        first_error("id :: func <T> (x: T) -> T { return x }\nf :: func () { const a := id.<i32, i64>(1) }\n")
            .contains("takes 1 generic argument(s) but 2")
    );
}

#[test]
fn a_generic_type_named_without_arguments_infers_them() {
    let session = analyze_clean(
        "Box :: struct <T> { item: T }\nf :: func () { const b: Box.<i64> := Box { item: 4 } }\n",
    );
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    assert!(text.contains("Box.<i64>"), "{text}");
}

#[test]
fn a_method_on_a_generic_impl_solves_the_impl_generics_from_the_receiver() {
    // The receiver is already a pointer, so `*Self` lines up with it directly;
    // going for the pointee instead left `T` unsolved.
    let session = analyze_clean(
        "Box :: struct <T> { it: T }\nimpl <T> Box.<T> { get :: func (self: *Box.<T>) -> T { return self.it } }\nf :: func (b: *Box.<i32>) { const x := b.get() }\n",
    );
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    assert!(text.contains("let x: i32"), "{text}");
}

#[test]
fn an_impl_inherits_the_traits_default_method_body() {
    let session = analyze_clean(
        "T :: trait { m :: func (self: *Self) -> i32 { return 7 } }\nS0 :: struct { v: i32 }\nimpl T for S0 {}\nf :: func (s: *S0) -> i32 { return s.m() }\n",
    );
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    assert!(!text.contains("<error>"), "{text}");
}

#[test]
fn an_unknown_field_or_method_is_reported_not_silently_erased() {
    assert!(first_error("P :: struct { x: i32 }\nf :: func (p: P) -> i32 { return p.y }\n")
        .contains("no field `y`"));
    assert!(first_error("f :: func (a: i32) { const u := a.nope() }\n")
        .contains("no method `nope`"));
}

#[test]
fn intrinsics_have_their_own_result_types() {
    // `$size_of` is a `usize` whatever `T` is, and `$new` allocates a `*mut T`.
    analyze_clean(
        "C :: struct { n: i32 }\nf :: func () {\n  const a: usize := $size_of.<i32>()\n  const b: *mut C := $new.<C>()\n  const c: []mut u8 := $make.<[]u8>(16)\n}\n",
    );
}

#[test]
fn a_distinct_type_does_not_convert_implicitly() {
    assert!(
        first_error("Meters :: distinct i32\nf :: func (m: Meters) -> i32 { return m }\n")
            .contains("type mismatch")
    );
    // A plain alias still does.
    analyze_clean("Alias :: i32\nf :: func (a: Alias) -> i32 { return a }\n");
}

#[test]
fn a_comptime_int_must_fit_the_type_it_settles_on() {
    assert!(first_error("f :: func () { const a: i8 := 300 }\n").contains("does not fit in `i8`"));
    assert!(first_error("f :: func () { const a: u8 := -1 }\n").contains("does not fit in `u8`"));
    // A constant is checked at each use, against that use's type.
    assert!(first_error("A :: 300\nf :: func () { const a: i8 := A }\n").contains("does not fit"));
    // The exact value survives however large it was written, and the minimum of
    // a signed type is not mistaken for its magnitude.
    analyze_clean("f :: func () {\n  const a: i8 := -128\n  const b: i256 := 99999999999999999999999999999999999999999999\n}\n");
}

#[test]
fn an_unknown_enum_variant_or_wrong_payload_is_reported() {
    assert!(first_error("E :: enum { a }\nf :: func () { const x: E := .nope }\n")
        .contains("has no variant `.nope`"));
    assert!(
        first_error("E :: enum { a(i32) }\nf :: func () { const x: E := .a(1, 2) }\n")
            .contains("takes 1 value(s) but 2")
    );
}

#[test]
fn break_and_continue_require_an_enclosing_loop() {
    assert!(first_error("f :: func () { break }\n").contains("`break` outside of a loop"));
    assert!(first_error("f :: func () { continue }\n").contains("`continue` outside of a loop"));
    // A `while` counts as one.
    analyze_clean("f :: func (n: i32) {\n  let i := 0\n  while i < n { i += 1\n    if i == 2 { break }\n    continue }\n}\n");
}

#[test]
fn an_using_field_must_be_a_struct() {
    assert!(first_error("P :: struct { @using n: i32 }\n").contains("must be a struct"));
    analyze_clean("A :: struct { v: i32 }\nP :: struct { @using a: *A }\n");
}

#[test]
fn a_namespace_scope_let_must_be_static() {
    assert!(first_error("let G: i32 := 0\n").contains("must be `#static`"));
    analyze_clean("#static let G: i32 := 0\n");
}

#[test]
fn attribute_arguments_are_not_program_names() {
    // `all` in `@public(all)` is compiler vocabulary, not something to resolve.
    analyze_clean("@public(all) P :: struct { x: i32 }\nf :: func (p: P) -> i32 { return p.x }\n");
}

#[test]
fn field_uses_are_bound_to_their_definitions() {
    // Both literal forms and the access itself carry the field's def, so later
    // stages can talk about a field without re-deriving it from a name.
    let session = analyze_clean(
        "P :: struct { x: i32, y: i32 }\nf :: func (p: *P) -> i32 {\n  const q: P := .{ x: 1, y: 2 }\n  return p.x\n}\n",
    );
    let file = entry_file(&session);
    let ast = &session.asts[&file];
    let bound = ast
        .ids()
        .filter(|&id| {
            matches!(
                ast.node(id).kind,
                NodeKind::FieldInit { .. } | NodeKind::FieldAccess { .. }
            ) && matches!(ast.meta::<Resolution>(id), Some(Resolution::Def(d))
                if session.defs.get(d).kind == DefKind::Field)
        })
        .count();
    assert_eq!(bound, 3, "expected both field inits and the access to bind");
}

// ===< const generics, array lengths, and `.len()` >===

#[test]
fn an_array_length_is_part_of_the_type() {
    assert!(
        first_error("f :: func () {\n  const a: [3]i32 := .{ 1, 2, 3 }\n  const b: [4]i32 := a\n}\n")
            .contains("expected `[4]i32`, found `[3]i32`"),
    );
}

#[test]
fn an_underscore_length_is_inferred_from_the_literal() {
    analyze_clean("f :: func () {\n  const a := [_]i32 { 1, 2, 3 }\n  const b: [3]i32 := a\n}\n");
}

#[test]
fn a_positional_literal_must_match_a_fixed_length() {
    assert!(
        first_error("f :: func () {\n  const a: [4]i32 := .{ 1, 2, 3 }\n}\n")
            .contains("element(s)"),
    );
}

#[test]
fn a_repeat_literals_count_is_the_arrays_length() {
    assert!(
        first_error("f :: func () {\n  const a: [3]i32 := .{ 0; 5 }\n}\n")
            .contains("repeats 5 time(s) but the array is `[3]`"),
    );
    // …and it decides the length when nothing else did.
    let s = analyze_clean("f :: func () {\n  const a := [_]i32 { 0; 4 }\n  const b: [4]i32 := a\n}\n");
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("let a: [4]i32"), "{ir}");
}

#[test]
fn a_repeat_literals_count_must_be_a_compile_time_value() {
    assert!(
        first_error("f :: func (n: usize) {\n  const a: [3]i32 := .{ 0; n }\n}\n")
            .contains("array length"),
    );
}

#[test]
fn a_const_generic_parameter_is_inferred_from_an_argument() {
    // `N` is solved by the call site's array length, and `[N]T` in the signature
    // instantiates to `[3]i32`.
    let s = analyze_clean(
        "count :: func <const N: usize, T> (a: [N]T) -> usize { return N }\nf :: func () -> usize {\n  const a := [_]i32 { 1, 2, 3 }\n  return count(a)\n}\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("func([3]i32) -> usize"), "{ir}");
}

#[test]
fn a_const_generic_parameter_can_be_pinned_by_turbofish() {
    analyze_clean(
        "zeros :: func <const N: usize> () -> [N]u8 { return .{ 0; N } }\nf :: func () -> [4]u8 { return zeros.<4>() }\n",
    );
}

#[test]
fn a_const_generic_parameter_is_a_value_not_a_type() {
    assert!(
        first_error("f :: func <const N: usize> () -> N { return 0 }\n")
            .contains("a value, not a type"),
    );
}

#[test]
fn a_const_generic_parameter_on_a_type_declaration_is_rejected() {
    // `Ty::Nominal` identity is `(def, type-args)`; there is no slot for a
    // value, so this is diagnosed rather than silently mistyped.
    assert!(
        first_error("Buf :: struct <const N: usize> { n: i32 }\n")
            .contains("not supported on a type declaration"),
    );
}

#[test]
fn an_array_length_must_be_a_constant() {
    assert!(
        first_error("f :: func (n: usize) {\n  const a: [n]i32 := .{ 1 }\n}\n")
            .contains("array length"),
    );
}

#[test]
fn a_named_constant_is_a_usable_array_length() {
    analyze_clean(
        "SIZE :: 3\nf :: func () {\n  const a: [SIZE]i32 := .{ 1, 2, 3 }\n  const b: [3]i32 := a\n}\n",
    );
}

#[test]
fn len_is_an_inherent_method_the_receiver_type_picks() {
    // `.len()` is not compiler syntax: it is `core`'s inherent method on the
    // sequences, and which impl runs is decided by the receiver's type — the
    // `[N]T` one for an array, the `[]T` one for a slice. Neither call names a
    // trait, because neither impl has one.
    let s = analyze_clean(
        "f :: func (s: []i32) {\n  const a := [_]i32 { 1, 2, 3 }\n  const n := a.len()\n  const m := s.len()\n}\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("(&a: [3]i32): *[3]i32"), "{ir}");
    assert!(ir.contains("(&s: []i32): *[]i32"), "{ir}");
    assert!(!ir.contains("#virtual") && !ir.contains("#generic"), "{ir}");
}

#[test]
fn the_len_intrinsic_folds_on_a_fixed_array_and_reads_a_slice_header() {
    // `$len` is the primitive `.len()`'s body is written in. On a `[N]T` whose
    // `N` is known it folds to the literal count right here; on a `[]T` it stays
    // for the header read, and on a still-generic `[N]T` it stays for
    // monomorphization to substitute.
    let s = analyze_clean(
        "count :: func <const N: usize, T> (a: [N]T) -> usize { return $len(a) }\nf :: func (s: []i32) -> usize {\n  const a := [_]i32 { 1, 2, 3 }\n  return $len(a) + $len(s)\n}\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("$len(a: [N]T): usize"), "{ir}");
    assert!(ir.contains("3: usize"), "{ir}");
    assert!(ir.contains("$len(s: []i32): usize"), "{ir}");
}

#[test]
fn len_works_through_a_mutable_slice() {
    // `[]mut T` calls a method declared on `[]T`: dropping a write permission is
    // always safe, so the receiver satisfies the `self` parameter.
    analyze_clean("f :: func (s: []mut i32) -> usize { return s.len() }\n");
}

#[test]
fn the_len_intrinsic_rejects_a_type_with_no_length() {
    assert!(
        first_error("f :: func (n: i32) -> usize { return $len(n) }\n")
            .contains("`$len` needs an array or a slice"),
    );
}

#[test]
fn a_fixed_array_unsizes_to_a_read_only_slice() {
    // One `func (s: []T)` serves every length; the IR shows the full sub-slice
    // the source left implicit — the same `$slice` an explicit `a[..]` emits.
    let s = analyze_clean(
        "take :: func (s: []i32) -> usize { return s.len() }\nf :: func () -> usize {\n  const a := [_]i32 { 1, 2, 3 }\n  return take(a)\n}\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(
        ir.contains("$slice(a: [3]i32, .full: core.Range.<usize>): []i32"),
        "{ir}"
    );
}

#[test]
fn a_fixed_array_does_not_unsize_to_a_mutable_slice() {
    // Handing out a mutable view is a permission the coercion must not grant.
    assert!(
        first_error("take :: func (s: []mut i32) -> usize { return s.len() }\nf :: func () -> usize {\n  const a := [_]i32 { 1, 2, 3 }\n  return take(a)\n}\n")
            .contains("type mismatch"),
    );
}

#[test]
fn an_unknown_field_on_a_slice_is_still_reported() {
    assert!(
        first_error("f :: func (s: []i32) -> usize { return s.size }\n")
            .contains("no field `size`"),
    );
}

#[test]
fn ir_snap_array_lengths_through_the_pipeline() {
    // Every way a length reaches the IR, in one place:
    //
    //   - `[_]T { … }`  — the literal's element count becomes the type's `N`
    //   - `[3]i32`      — written out, and the same type as the inferred one
    //   - `[N]T`        — still a `const` parameter; `$len` cannot fold yet and
    //                     stays for monomorphization to substitute
    //   - `$len(a)`     — folds to the literal on a known `N`
    //   - `a.len()`     — `core`'s inherent method; which impl runs is picked by
    //                     the receiver's type, so an array and a slice reach
    //                     different ones
    //   - `take(a)`     — the `[3]i32` -> `[]i32` unsizing, spelled as the
    //                     whole sub-slice it means
    let src = "\
count :: func <const N: usize, T> (a: [N]T) -> usize { return $len(a) }
take :: func (s: []i32) -> usize { return s.len() }
f :: func () -> usize {
  const inferred := [_]i32 { 1, 2, 3 }
  const written: [3]i32 := inferred
  const folded := $len(written)
  const method := written.len()
  return count(inferred) + folded + method + take(written)
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_tuple_structs_are_structs_with_positional_names() {
    // A tuple struct has no machinery of its own: its members are ordinary
    // fields named by their positions (§3.3), so every form below lands on the
    // same two IR nodes a record struct uses.
    //
    //   - `Pair(1, 2)`  — a callee naming a type constructs it, and the
    //                     arguments are checked against the field types (the
    //                     literals come out `i32`, not the default integer)
    //   - `.{ 3, 4 }`   — the positional composite literal builds the *same*
    //                     `Construct`; which syntax was written does not survive
    //   - `p.0`         — an `Expr::Field` carrying the field's def, so a later
    //                     stage reads the offset off the struct's own definition
    //   - `t.1`         — on an anonymous tuple there is no def to name, so this
    //                     one stays a structural `TupleIndex`
    //   - `Pair(a, b)`  — the pattern destructures positionally
    //   - `Wrap.<T>`    — the fields substitute like any other generic struct's
    let src = "\
Pair :: struct (i32, i32)
Wrap :: struct <T> (T)

f :: func () -> i32 {
  let mut p := Pair(1, 2)
  const q: Pair := .{ 3, 4 }
  const w := Wrap(5)
  const t := (6, 7)
  p.0 = p.1 + t.1
  return q.match {
    Pair(a, b) => a + b + w.0 + p.0,
  }
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn a_tuple_struct_call_checks_its_arguments() {
    // The construction form is not a function call, so it does not borrow the
    // function-call diagnostics: the arity is the struct's field count, and a
    // record struct rejects the call shape outright — it is built with a
    // composite literal, whose fields are checked where the literal is inferred.
    for (src, needle) in [
        (
            "Pair :: struct (i32, i32)\nf :: func () { const p := Pair(1, 2, 3) }\n",
            "`Pair` has 2 field(s) but 3 were supplied",
        ),
        (
            "Pair :: struct (i32, i32)\nf :: func () { const p := Pair(1, true) }\n",
            "type mismatch: expected `i32`, found `bool`",
        ),
        (
            "Rec :: struct { a: i32 }\nf :: func () { const r := Rec(1) }\n",
            "`Rec` is not a tuple struct",
        ),
    ] {
        let s = analyze_mem(&[("main", src)], "main");
        assert!(diag_contains(&s, needle), "{:#?}", s.diagnostics);
    }
}

#[test]
fn a_positional_access_out_of_range_says_so() {
    // The tuple case reports the arity rather than the type: its elements are
    // often still unsolved variables here, and the count is the whole story.
    for (src, needle) in [
        (
            "f :: func () -> i32 {\n  const t := (1, 2)\n  return t.5\n}\n",
            "index 5 is out of range for a tuple of 2 element(s)",
        ),
        (
            "Pair :: struct (i32, i32)\nf :: func (p: Pair) -> i32 { return p.7 }\n",
            "no field `7` on `Pair`",
        ),
    ] {
        let s = analyze_mem(&[("main", src)], "main");
        assert!(diag_contains(&s, needle), "{:#?}", s.diagnostics);
    }
}

#[test]
fn ir_snap_named_arguments_bind_to_their_parameters() {
    // Named arguments are bound to parameters during inference (§5.3), so the IR
    // is positional *always*: all three calls below lower to the identical
    // argument list, including the one written fully out of order. Nothing after
    // inference has to know that a name was ever written.
    let src = "\
mk :: func (host: str, port: i32, backlog: i32) -> i32 { return port }

Router :: struct { port: i32 }
impl Router {
  listen :: func (self: *Router, host: str, port: i32) -> i32 { return port }
}

f :: func (r: *Router) -> i32 {
  const a := mk(\"h\", 8080, 5)
  const b := mk(\"h\", port: 8080, backlog: 5)
  const c := mk(host: \"h\", backlog: 5, port: 8080)
  const d := r.listen(\"h\", port: 80)
  return a + b + c + d
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_default_arguments_are_filled_at_the_call_site() {
    // A default is lowered against its declaration and *cloned* into every call
    // that omits it (§5.2), so the IR is a plain positional call with no notion
    // that anything was left out. The literal `0` picks up the parameter's `i32`
    // exactly as a written argument would, and `pad`'s two calls each get their
    // own copy of the same lowered default.
    let src = "\
g :: func (x: i32, y: i32 := 0) -> i32 { return x + y }

pad :: func (s: str, width: usize := 8, fill: char := \'x\') -> usize { return width }

Router :: struct { port: i32 }
impl Router {
  listen :: func (self: *Router, host: str, port: i32 := 80) -> i32 { return port }
}

f :: func (r: *Router) -> i32 {
  const a := g(1)
  const b := g(1, 2)
  const c := g(y: 5, x: 1)
  const d := pad(\"h\")
  const e := pad(\"h\", fill: \'-\')
  const m := r.listen(\"h\")
  return a + b + c + m
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_a_default_may_be_any_constant_form() {
    // The forms §5.2 admits, each reaching the call site: a path to a `const`
    // item, a composite literal, a `$cast`, and a `const` generic parameter.
    //
    // Two things to read off this snapshot. The composite literal is rebuilt at
    // the call site rather than shared, which is what keeps a mutable default
    // from becoming Python's shared-default trap. And the `const` generic stays
    // *symbolic* (`const N`) — it is the caller's type arguments that give it a
    // value, which monomorphization substitutes later.
    let src = "\
PORT :: 8080
Cfg :: struct { a: i32, b: i32 }

g :: func (x: i32, p: i32 := PORT, c: Cfg := .{ a: 1, b: 2 }, w: u8 := $cast.<u8>(3)) -> i32 {
  return x
}

h :: func <const N: usize> (x: usize, y: usize := N) -> usize { return x + y }

f :: func () -> i32 {
  const a := g(1)
  const b := g(1)
  const c := h.<4>(1)
  return a + b
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn ir_snap_a_distinct_type_inherits_the_methods_of_its_representation() {
    // §2.4: a `distinct T` inherits `T`'s methods, `T` does not gain the
    // distinct type's, and a method the distinct type declares itself wins over
    // an inherited one of the same name.
    //
    // Read off the IR: `inherited` casts the receiver to `Base` — the
    // representations are identical, so reaching it is a reinterpretation and
    // costs nothing — while `own` and the overriding `shared` take the receiver
    // as written, calling `Wrapper`'s `shared` and not `Base`'s.
    let src = "\
Base :: struct { n: usize }
impl Base {
  inherited :: func (self: *Base) -> usize { return self.n }
  shared    :: func (self: *Base) -> usize { return 1 }
}

Wrapper :: distinct Base
impl Wrapper {
  own    :: func (self: *Wrapper) -> usize { return 2 }
  shared :: func (self: *Wrapper) -> usize { return 3 }
}

f :: func (w: Wrapper) -> usize {
  return w.inherited() + w.own() + w.shared()
}
";
    insta::assert_snapshot!(ir_text(src));
}

#[test]
fn distinct_method_inheritance_is_one_way() {
    // The asymmetry is the whole point: a `distinct T` is `T` plus an invariant
    // and some extra operations, so the operations that assume the invariant
    // must not be reachable on `T`, where it does not hold.
    let s = analyze_mem(
        &[(
            "main",
            "MyStr :: distinct []u8\n             impl MyStr {\n  shout :: func (self: MyStr) -> usize { return 1 }\n}\n             g :: func (b: []u8) -> usize { return b.shout() }\n",
        )],
        "main",
    );
    assert!(diag_contains(&s, "no method `shout` on `[]u8`"), "{:#?}", s.diagnostics);
}

#[test]
fn a_string_literal_is_the_core_str_lang_item() {
    // `str` is not a compiler primitive: it is `#lang("str") distinct []u8` in
    // core, found by tag like every other language item. A literal therefore
    // types as a nominal `core.str`, and inherits `[]T`'s methods.
    let session = analyze_clean(
        "f :: func () -> usize {\n  const s := \"héllo\"\n  return s.len()\n}\n",
    );
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    assert!(text.contains("core.str"), "{text}");
    // The length is the *byte* length, inherited from the slice impl.
    assert!(text.contains("core.<impl []T>.len"), "{text}");
}

#[test]
fn a_default_argument_is_lowered_in_its_own_file() {
    // The default belongs to the file that *declares* the parameter, and so do
    // the types inference stamped on it — but it is filled in at a call site
    // that may be in another file entirely (every call into `core` is). Lowering
    // therefore reads the default out of the declaring file's arena, not the
    // caller's; if it read the caller's, the node ids would land on unrelated
    // expressions and the argument would come out silently wrong.
    let lib = "\
@public scaled :: func (x: i32, by: i32 := 7) -> i32 { return x * by }
";
    let main = "\
lib :: import \"lib.nest\"
main :: func () -> i32 { return lib.scaled(2) }
";
    let session = analyze_mem(&[("lib", lib), ("main", main)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let text = crate::ir::pretty::program_to_string(&session.defs, &session.ir[&file]);
    // The cross-file default reached the call site with its own value and type.
    assert!(text.contains("$cast(7: comptime_int): i32"), "{text}");
}

#[test]
fn default_argument_rules_are_enforced() {
    // Each case reports exactly one diagnostic: a defaulted signature must not
    // also draw the plain equality-arity complaint, or one mistake reads as two.
    for (src, needle) in [
        (
            "g :: func (x: i32 := 0, y: i32) -> i32 { return x + y }\nf :: func () { const a := g(1, 2) }\n",
            "parameter `y` has no default but follows `x`, which does",
        ),
        (
            "g :: func (x: i32, y: i32 := 0) -> i32 { return x + y }\nf :: func () { const a := g() }\n",
            "this function takes 1 to 2 argument(s) but 0 were supplied",
        ),
        (
            "g :: func (x: i32, y: i32 := 0) -> i32 { return x + y }\nf :: func () { const a := g(1, 2, 3) }\n",
            "this function takes 1 to 2 argument(s) but 3 were supplied",
        ),
        (
            "g :: func (x: i32, y: i32 := true) -> i32 { return x }\nf :: func () { const a := g(1) }\n",
            "type mismatch: expected `i32`, found `bool`",
        ),
        (
            "g :: func (x: i32, y: i32 := 0, z: i32 := 0) -> i32 { return x }\nf :: func () { const a := g(z: 1) }\n",
            "missing argument for parameter `x`",
        ),
        // A default is evaluated at the call site, so it cannot name a parameter
        // of the function it belongs to — there is no such binding yet, and
        // lowering would otherwise emit a load from the *caller's* frame.
        (
            "g :: func (a: i32, b: i32 := a) -> i32 { return a + b }\nf :: func () { const q := g(1) }\n",
            "a default argument cannot name the parameter `a`",
        ),
        // A *later* parameter is not in scope at all where the default is
        // written, so the resolver rejects it first and this never reaches the
        // parameter check. Asserted so the two rules stay distinguishable: if
        // scoping ever changes, the check above is what has to catch this.
        (
            "g :: func (a: i32 := b, b: i32 := 0) -> i32 { return a }\nf :: func () { const q := g() }\n",
            "cannot resolve name `b`",
        ),
        // The receiver is a parameter like any other.
        (
            "T :: struct { n: i32 }\nimpl T {\n  m :: func (self: *T, x: i32 := self.n) -> i32 { return x }\n}\n",
            "a default argument cannot name the parameter `self`",
        ),
        // Naming a parameter twice in one default is still one mistake.
        (
            "g :: func (a: i32, b: i32 := a + a) -> i32 { return b }\nf :: func () { const q := g(1) }\n",
            "a default argument cannot name the parameter `a`",
        ),
    ] {
        let s = analyze_mem(&[("main", src)], "main");
        assert!(diag_contains(&s, needle), "{:#?}", s.diagnostics);
        assert_eq!(s.diagnostics.len(), 1, "{:#?}", s.diagnostics);
    }
}

#[test]
fn named_argument_rules_are_enforced() {
    // Each of these reports exactly one diagnostic: a failed binding must not
    // then be re-checked positionally, or one mistake reads as several.
    const MK: &str = "mk :: func (host: str, port: i32, backlog: i32) -> i32 { return port }\n";
    for (call, needle) in [
        (
            "mk(port: 80, \"h\", 5)",
            "a positional argument cannot follow a named one",
        ),
        ("mk(\"h\", prot: 80, backlog: 5)", "`mk` has no parameter named `prot`"),
        (
            "mk(\"h\", port: 80, port: 81)",
            "argument for parameter `port` supplied twice",
        ),
        ("mk(host: \"h\", port: 80)", "missing argument for parameter `backlog`"),
    ] {
        let src = format!("{MK}f :: func () -> i32 {{ return {call} }}\n");
        let s = analyze_mem(&[("main", &src)], "main");
        let errors: Vec<&str> = s
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            errors.iter().any(|m| m.contains(needle)),
            "expected {needle:?} in {errors:#?}"
        );
        assert_eq!(errors.len(), 1, "expected one diagnostic, got {errors:#?}");
    }
    // A callee with no parameter *names* to bind to: a function-typed value.
    let s = analyze_mem(
        &[("main", "f :: func (g: func (i32) -> i32) -> i32 { return g(x: 1) }\n")],
        "main",
    );
    assert!(
        diag_contains(&s, "cannot pass argument `x` by name"),
        "{:#?}",
        s.diagnostics
    );
}

#[test]
fn a_const_generic_parameter_must_be_an_integer() {
    // `Const` represents one kind of compile-time value: an unsigned integer.
    // The check is at the *declaration*, so a parameter that is declared and
    // never used is still rejected, and an `impl`'s generics — which belong to
    // no function — are covered too.
    for (src, needle) in [
        (
            "Point :: struct { x: i32, y: i32 }\nf :: func <const X: Point> () {}\n",
            "a `const` generic parameter must have an integer type, but `X` is `Point`",
        ),
        (
            "f :: func <const B: bool> () {}\n",
            "a `const` generic parameter must have an integer type, but `B` is `bool`",
        ),
        (
            "T :: struct { a: i32 }\nimpl <const X: T> T { m :: func (self: *Self) {} }\n",
            "a `const` generic parameter must have an integer type",
        ),
    ] {
        let s = analyze_mem(&[("main", src)], "main");
        assert!(diag_contains(&s, needle), "{:#?}", s.diagnostics);
    }
    // The integer case that the language actually uses stays clean.
    let s = analyze_clean("f :: func <const N: usize> (a: [N]i32) -> usize { return $len(a) }\n");
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
}

#[test]
fn an_array_length_travels_with_the_const_parameter() {
    // A `[N]T` parameter keeps `N` symbolic all the way into the IR: the call
    // site's `[3]i32` solves it for *that* instantiation, and the callee's body
    // still says `[N]T` because it is one body for every length.
    let s = analyze_clean(
        "count :: func <const N: usize, T> (a: [N]T) -> usize { return $len(a) }\nf :: func () -> usize {\n  const a := [_]i32 { 1, 2, 3, 4 }\n  return count(a)\n}\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("$len(a: [N]T): usize"), "{ir}");
    assert!(ir.contains("(count: func([4]i32) -> usize)"), "{ir}");
}

// ===< static trait calls >===

#[test]
fn a_trait_method_named_through_its_trait_takes_self_from_context() {
    // `Make.make(3)` has no receiver: `Self` is decided by the return type, and
    // lowering points at the impl that was selected, not the declaration.
    let s = analyze_clean(
        "Make :: trait { make :: func (n: i32) -> Self }\nW :: struct { v: i32 }\nimpl Make for W { make :: func (n: i32) -> W { return W { v: n } } }\nf :: func () -> W { return Make.make(3) }\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("W.make"), "{ir}");
}

#[test]
fn a_static_trait_call_with_no_impl_for_the_context_type_is_reported() {
    assert!(
        first_error(
            "Make :: trait { make :: func (n: i32) -> Self }\nW :: struct { v: i32 }\nf :: func () -> W { return Make.make(3) }\n"
        )
        .contains("does not implement"),
    );
}

// ===< `.?` / `.!` through `Try` >===

#[test]
fn try_propagate_works_on_an_option() {
    let s = analyze_clean(
        "head :: func () -> Option.<i32> { return .none }\nf :: func () -> Option.<i32> {\n  const v := head().?\n  return .some(v)\n}\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("core.Option.from_residual"), "{ir}");
}

#[test]
fn try_propagate_converts_a_residual_through_a_user_impl() {
    // The whole point of `FromResidual` being its own trait: an `Io` residual
    // reaches a `Cfg`-returning function because an impl says how.
    analyze_clean(
        "Io :: struct { n: i32 }\nCfg :: struct { n: i32 }\nimpl <T> FromResidual.<Io> for Result.<T, Cfg> {\n  from_residual :: func (r: Io) -> Result.<T, Cfg> { return .err(Cfg { n: r.n }) }\n}\nread :: func () -> Result.<i32, Io> { return .err(Io { n: 1 }) }\nload :: func () -> Result.<i32, Cfg> {\n  const v := read().?\n  return .ok(v)\n}\n",
    );
}

#[test]
fn try_propagate_across_unrelated_residuals_is_reported() {
    // A `Result`'s residual has no way into an `Option`, and nothing declared
    // one; the diagnostic names the residual that has no conversion.
    let msg = first_error(
        "read :: func () -> Result.<i32, str> { return .ok(1) }\nf :: func () -> Option.<i32> {\n  const v := read().?\n  return .some(v)\n}\n",
    );
    assert!(msg.contains("core.FromResidual.<core.str>"), "{msg}");
}

#[test]
fn try_propagate_in_a_function_that_is_not_a_try_type_is_reported() {
    let msg = first_error(
        "read :: func () -> Result.<i32, str> { return .ok(1) }\nf :: func () -> i32 {\n  const v := read().?\n  return v\n}\n",
    );
    assert!(msg.contains("`i32` does not implement"), "{msg}");
}

#[test]
fn try_abort_is_the_try_unwrap_call() {
    let s = analyze_clean(
        "head :: func () -> Option.<i32> { return .none }\nf :: func () -> i32 { return head().! }\n",
    );
    let file = entry_file(&s);
    let ir = crate::ir::pretty::program_to_string(&s.defs, &s.ir[&file]);
    assert!(ir.contains("unwrap"), "{ir}");
}
