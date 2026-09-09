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
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", mainsrc)));
    session.register_package(
        "mathpkg",
        "@public triple :: func (n: isize) -> isize { return n }\n",
    );
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
    ast.meta::<Ty>(id).expect("node has an inferred type")
}

#[test]
fn literal_defaults_to_isize_without_context() {
    let session = analyze_mem(&[("main", "f :: func () { const x := 7 }")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(7)))
    });
    assert_eq!(ty, Ty::isize());
}

#[test]
fn literal_takes_annotated_type() {
    let session = analyze_mem(&[("main", "f :: func () { const x: i32 := 7 }")], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let file = entry_file(&session);
    let ty = node_ty(&session, file, |k| {
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(7)))
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
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(3)))
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
    let in_stmts = func
        .body
        .stmts
        .iter()
        .any(|s| matches!(s, Stmt::Expr(e) if is_guarded_loop(e)));
    let in_tail = func.body.tail.as_deref().is_some_and(is_guarded_loop);
    assert!(
        in_stmts || in_tail,
        "while did not lower to a guarded loop: {:#?}",
        func.body
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
    assert_eq!(func.body.defers.len(), 1, "{:#?}", func.body);
    assert!(
        matches!(&func.body.defers[0], Expr::Call { .. }),
        "defer body is not the call: {:#?}",
        func.body.defers
    );
    // ...and is not copied ahead of either `return`, even though there are two.
    assert!(
        !func
            .body
            .stmts
            .iter()
            .any(|s| matches!(s, Stmt::Expr(Expr::Call { .. }))),
        "deferred call was duplicated into the statement list: {:#?}",
        func.body.stmts
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
    let ret = func.body.stmts.iter().find_map(|s| match s {
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
fn ir_snap_bitwise_and_shift_stay_primitive() {
    // `& | ^ << >>` are not routed through operator traits in the bootstrap:
    // they stay a primitive `Binary`.
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
    insta::assert_snapshot!(ir_text(
        "un :: func (a: i32, b: bool) -> i32 {\n  let n := -a\n  let bn := ~a\n  let no := !b\n  return n\n}\n"
    ));
}

#[test]
fn ir_snap_ref_and_deref() {
    insta::assert_snapshot!(ir_text(
        "rd :: func (p: *i32) -> i32 {\n  let r := &p\n  return p.*\n}\n"
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
fn ir_snap_try_propagate_desugars_to_match() {
    insta::assert_snapshot!(ir_text_lenient(
        "fallible :: func () -> Result.<i32, i32> { return .ok(1) }\ntp :: func () -> i32 {\n  const x := fallible().?\n  return x\n}\n"
    ));
}

#[test]
fn ir_snap_try_abort_desugars_to_match() {
    insta::assert_snapshot!(ir_text_lenient(
        "fallible :: func () -> Result.<i32, i32> { return .ok(1) }\nta :: func () -> i32 {\n  const x := fallible().!\n  return x\n}\n"
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
        matches!(k, NodeKind::Lit(crate::parser::ast::Lit::Int(1)))
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
    // A blanket `impl <T> Add for T` and a concrete `impl Add for Foo` both
    // apply to `Foo`; the concrete one wins with no ambiguity.
    let src = "\
Foo :: struct { n: i32 }
impl <T> Add for T { Output :: T  add :: func (self: T, rhs: T) -> T { return self } }
impl Add for Foo { Output :: Foo  add :: func (self: Foo, rhs: Foo) -> Foo { return self } }
f :: func (a: Foo, b: Foo) -> Foo { return a + b }
";
    let s = analyze1(src);
    assert!(!s.has_errors(), "{:#?}", s.diagnostics);
    let file = entry_file(&s);
    assert!(is_nominal_named(&s, &binop_ty(&s, file, BinOp::Add), "Foo"));
}

#[test]
fn two_equally_specific_impls_are_ambiguous() {
    // Two concrete impls both match an unconstrained self; the choice is a tie.
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
        diag_contains(&s, "multiple applicable impls"),
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
    // `.new()` construction: `xs : Vector.<isize>`, `ws : Vector.<string>`.
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



