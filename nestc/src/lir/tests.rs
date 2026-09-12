//! Stage-level tests for IR → LIR lowering (`design/lir.md`).
//!
//! Every test here compiles a whole program from **source** — the entry file
//! plus `core` — and looks at the LIR that came out the far end. The earlier
//! stages are tested in `sema::tests`, which is where the AST → IR half lives;
//! what is checked here is only what this pass decides: the control-flow graph,
//! the places, the checks §7d makes real, the cleanup ladder, the drops, and
//! the safepoints.

use crate::sema::analyze;
use crate::sema::session::{MemLoader, Session};
use crate::sema::tests::{analyze_mem, ir_text, messages};
use crate::sema::ty::Ty;

// ===< LIR snapshots (insta) >===
//
// The LIR is a graph, and a graph is the one representation where a reader
// cannot reconstruct intent from the shape: every construct becomes the same
// jumps. So these are snapshots rather than assertions about one line — what
// they lock down is the *whole* lowering of each construct, which is the only
// form in which "did this `while` become the right three blocks" is a question
// anyone can answer by looking.

/// Analyze `src` as the entry file and hand back the whole lowered program,
/// `core` included.
///
/// The rendered forms below filter `core` out, because a test about one
/// construct should not be a record of the standard library. The invariant
/// checks want the opposite: `core` is code a backend has to emit too, and a
/// shape it alone produces is exactly the one nothing else would catch.
fn lir_whole_program(src: &str) -> crate::lir::Program {
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", src)));
    let file = session.load_entry("main").expect("entry loads");
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let layouts = crate::ir::layout::Layouts::new(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        session.options.target,
    );
    crate::lir::lower(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        &layouts,
        &session.options,
        &session.lang_items,
        &session.sources,
    )
}

/// Analyze `src` as the entry file and render the whole program's LIR.
///
/// Spans are left off: they are carried on every statement (§7c) and printing
/// them here would make every snapshot a record of line numbers in this file.
/// `lir_text_with_spans` is for the one test that is *about* them.
fn lir_text(src: &str) -> String {
    lir_program(src, Default::default())
}

/// [`lir_text`], for a chosen set of build options. Only `overflow=` changes
/// what is lowered (§7d), which is what that test is for.
fn lir_text_with(src: &str, options: crate::common::options::Options) -> String {
    lir_program(src, options)
}

fn lir_program(src: &str, options: crate::common::options::Options) -> String {
    let mut session = {
        let mut loader = MemLoader::new();
        loader = loader.with("main", src);
        let mut s = Session::with_loader(Box::new(loader));
        s.options = options;
        s
    };
    let file = session.load_entry("main").expect("entry loads");
    analyze(&mut session, file);
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let layouts = crate::ir::layout::Layouts::new(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        session.options.target,
    );
    let program = crate::lir::lower(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        &layouts,
        &session.options,
        &session.lang_items,
        &session.sources,
    );
    // Only the entry file's functions: `core` is linked into every program and
    // its lowering is not what any of these tests is about.
    let entry: Vec<crate::lir::Function> = program
        .funcs
        .iter()
        .filter(|f| session.linked.file_of(f.def) == Some(file))
        .cloned()
        .collect();
    let program = crate::lir::Program {
        funcs: entry,
        ..program
    };
    crate::lir::pretty::program_to_string(&session.defs, None, &program)
}

/// `while` is a `loop` with a guard by the time the IR has it (§ the IR's
/// shape), and a `loop` is a back edge here. The three blocks a reader should
/// see are the head that tests, the body that jumps back, and the exit.
#[test]
fn lir_snapshot_while_loop_and_break() {
    let src = "\
count :: func (n: i32) -> i32 {
  let acc := 0
  let i := 0
  while i < n {
    if i == 3 { break }
    acc = acc + i
    i = i + 1
  }
  return acc
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// §4's shape: the discriminant is read **once**, one switch chooses the
/// variant, and each group projects only the payload its arm named.
#[test]
fn lir_snapshot_match_reads_the_discriminant_once() {
    let src = "\
Shape :: enum { dot, circle(i32), rect { w: i32, h: i32 } }
area :: func (s: Shape) -> i32 {
  return s.match {
    .dot => 0,
    .circle(r) => r,
    .rect { w, h } => w,
  }
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// A guard is a test like any other, except that failing it falls through to
/// the **next arm** rather than to the next test — which is why the candidates
/// are a chain (§4).
#[test]
fn lir_snapshot_a_failed_guard_falls_through_to_the_next_arm() {
    let src = "\
pick :: func (o: Option.<i32>) -> i32 {
  return o.match {
    .some(x) if x > 0 => x,
    .some(y) => y,
    .none => 0,
  }
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// §3's ladder: a `defer` body is a block, every exit jumps into it, and it is
/// emitted once per **kind** of exit rather than once per exit site. Two
/// `return`s in one scope share a rung.
#[test]
fn lir_snapshot_defer_is_a_block_every_exit_jumps_through() {
    let src = "\
cleanup :: func () {}
run :: func (n: i32) -> i32 {
  defer cleanup()
  if n > 10 { return 1 }
  if n > 5 { return 2 }
  return 3
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// `overflow=trap` is a **second block and an extra edge**, not a flag on an
/// instruction — which is why it is lowering's decision and not codegen's
/// (§7d).
#[test]
fn lir_snapshot_overflow_trap_is_an_edge() {
    let src = "\
add :: func (a: i32, b: i32) -> i32 { return a + b }
";
    insta::assert_snapshot!("overflow_trap", lir_text(src));
    let wrap = crate::common::options::Options {
        overflow: crate::common::options::OverflowMode::Wrap,
        ..Default::default()
    };
    insta::assert_snapshot!("overflow_wrap", lir_text_with(src, wrap));
}

/// §7b: a tuple, an enum, a slice and a `distinct` are all structs here, and an
/// array is not. The type table is what a backend and a debugger read.
#[test]
fn lir_snapshot_aggregates_are_flattened_to_structs() {
    let src = "\
Meters :: distinct f64
Pair :: struct { a: u8, b: u32 }
Tag :: enum { none, one(u8), two(u32, u8) }
shapes :: func (p: Pair, t: Tag, s: []i32, a: [3]u8, q: (i32, bool), m: Meters) {}
";
    insta::assert_snapshot!(lir_text(src));
}

/// A `dyn` call is an indirect call through a vtable slot, and the vtable is a
/// constant of function pointers in the trait's declaration order (§7b, §9).
#[test]
fn lir_snapshot_dynamic_dispatch_goes_through_a_vtable_slot() {
    let src = "\
Speak :: trait { say :: func (self: *Self) -> i32 }
Dog :: struct { n: i32 }
impl Speak for Dog { say :: func (self: *Self) -> i32 { return self.n } }
heard :: func (d: *Dog) -> i32 {
  let s: *dyn Speak := d
  return s.say()
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// Places are paths (§1): a field by name, a deref, an index by a run-time
/// value. Nothing else is a place, and everything else gets a slot.
#[test]
fn lir_snapshot_places_are_paths() {
    let src = "\
Inner :: struct { v: i32 }
Outer :: struct { i: Inner }
reach :: func (o: *mut Outer, xs: []mut i32, k: usize) {
  o.*.i.v = 1
  xs[k] = o.*.i.v
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// `&&` and `||` are control flow, not operations: the right operand must not
/// run when the left already decided the answer.
#[test]
fn lir_snapshot_short_circuit_is_control_flow() {
    let src = "\
both :: func (a: bool, b: bool) -> bool { return a && b }
";
    insta::assert_snapshot!(lir_text(src));
}

/// §2: a panic does not unwind, so a call to a `-> never` function is an
/// ordinary instruction followed by `unreachable`, and the block ends there.
#[test]
fn lir_snapshot_a_diverging_call_ends_the_block() {
    let src = "\
checked :: func (n: i32) -> i32 {
  if n < 0 { panic(\"negative\") }
  return n
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// §9: `size_of.<T>()` is the number layout computed. Nothing reaching codegen
/// is ever a call to a function that does not exist.
#[test]
fn lir_snapshot_intrinsics_are_operations_not_calls() {
    let src = "\
{ size_of, align_of } :: import <core/mem>
Header :: #packed struct { magic: u32, tag: u8 }
sizes :: func () -> usize { return size_of.<Header>() + align_of.<Header>() }
";
    insta::assert_snapshot!(lir_text(src));
}

/// One generic function is **several** functions here (§7): monomorphization
/// runs before this pass, so a backend never sees a type parameter and never
/// has to instantiate anything itself.
#[test]
fn lir_snapshot_a_generic_function_is_one_function_per_instantiation() {
    let src = "\
id :: func <T> (x: T) -> T { return x }
both :: func (n: i32, c: bool) -> i32 {
  let a := id.<i32>(n)
  let b := id.<bool>(c)
  if b { return a }
  return 0
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// A bound that a *generic* satisfies is resolved at the call, not at run time:
/// the instantiation names the impl's member directly and there is no vtable in
/// the program at all. The contrast with
/// [`lir_snapshot_dynamic_dispatch_goes_through_a_vtable_slot`] is the whole
/// point — the same source method, two different instructions.
#[test]
fn lir_snapshot_a_bound_on_a_generic_needs_no_vtable() {
    let src = "\
Speak :: trait { say :: func (self: *Self) -> i32 }
Dog :: struct { n: i32 }
impl Speak for Dog { say :: func (self: *Self) -> i32 { return self.n } }
heard :: func <T: Speak> (t: *T) -> i32 { return t.say() }
call :: func (d: *Dog) -> i32 { return heard.<Dog>(d) }
";
    insta::assert_snapshot!(lir_text(src));
}

/// A `#static` is a **global** with an initializer, and a read of one is a
/// place with a global base rather than a slot. A plain `::` constant is not a
/// global at all by this point: it was folded into every use.
#[test]
fn lir_snapshot_a_static_is_a_place_and_a_constant_is_a_value() {
    let src = "\
LIMIT :: 10
#static counter: u32 :: 0
bump :: func () -> u32 {
  counter = counter + 1
  return counter
}
at_limit :: func (n: i32) -> bool { return n == LIMIT }
";
    insta::assert_snapshot!(lir_text(src));
}

/// A `str` pattern is **text** equality, and text equality is `core`'s
/// `bytes_eq` (`#lang("bytes_eq")`) — the same function `impl Eq for str`
/// calls, so a pattern and an `==` cannot disagree. Comparing the two `{ ptr,
/// len }` headers with one machine `Eq` would be comparing addresses.
#[test]
fn lir_snapshot_a_text_pattern_calls_bytes_eq() {
    let src = "\
kind :: func (s: str) -> i32 {
  return s.match {
    \"yes\" => 1,
    \"no\" => 0,
    _ => -1,
  }
}
same :: func (a: str, b: str) -> bool { return a == b }
";
    insta::assert_snapshot!(lir_text(src));
}

/// Dividing by zero is not overflow — `overflow=wrap` says what `MAX + 1`
/// means and has nothing to say about `x / 0` — so the check is unconditional
/// (§7d). A divisor the evaluator knows is not zero needs no branch, which is
/// why the second function here is three instructions.
#[test]
fn lir_snapshot_dividing_by_zero_is_a_check_of_its_own() {
    let src = "\
by_value :: func (a: i32, b: i32) -> i32 { return a / b }
by_constant :: func (a: i32) -> i32 { return a % 2 }
";
    insta::assert_snapshot!(lir_text(src));
}

/// `#unsafe` (§9) removes the checks the *program* is responsible for — the
/// bounds comparison and the zero comparison are both gone here.
///
/// The overflow trap stays, and that is not an oversight: `overflow=` is a
/// build-wide decision about what `MAX + 1` **means** (§7d), not a check on a
/// program that might be wrong, so a directive about safety checks has nothing
/// to say to it. Compiling this with `overflow=wrap` is what removes it.
#[test]
fn lir_snapshot_unsafe_removes_the_checks_the_program_owns() {
    let src = "\
raw :: #unsafe func (s: []i32, k: usize, d: i32) -> i32 { return s[k] / d }
";
    insta::assert_snapshot!(lir_text(src));
}

/// Casts (§6.11). A conversion between two run-time types is a `Cast` rvalue a
/// backend maps onto one instruction; a `transmute` is a reinterpretation of
/// the same bits; and a cast whose **source is a literal** is neither — it is
/// folded, because `cast.<u16>(7)` names a constant and a comptime type is not
/// something a machine holds.
#[test]
fn lir_snapshot_a_cast_of_a_literal_is_the_literal() {
    let src = "\
{ transmute } :: import <core/mem>
conv :: func (n: i32, f: f64) -> u8 {
  let wide := cast.<i64>(n)
  let narrow := cast.<u8>(wide)
  let single := cast.<f32>(f)
  let bits := transmute.<u32>(n)
  let folded := cast.<u16>(7)
  return narrow
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// The cleanup ladder has one rung per **kind** of exit (§3), and a loop is
/// where all four kinds appear at once: falling out of the body, `continue`,
/// `break`, and `return` each run the `defer` and then go somewhere different.
#[test]
fn lir_snapshot_a_loop_body_has_a_rung_for_each_kind_of_exit() {
    let src = "\
cleanup :: func () {}
scan :: func (n: i32) -> i32 {
  let mut i := 0
  while i < n {
    defer cleanup()
    i = i + 1
    if i == 2 { continue }
    if i == 3 { break }
    if i == 4 { return 4 }
  }
  return i
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// An operator on a user type is a **call** to the impl's member (§6.13), with
/// nothing left of the `+` — while the same `+` on the `i32` inside it is the
/// machine operation, because that impl's member is `#intrinsic`.
#[test]
fn lir_snapshot_an_operator_on_a_user_type_is_a_call() {
    let src = "\
{ Add } :: import <core/ops>
V :: struct { x: i32 }
impl Add for V {
  Output :: V
  add :: func (self: V, rhs: V) -> V { return V { x: self.x + rhs.x } }
}
total :: func (a: V, b: V) -> V { return a + b }
";
    insta::assert_snapshot!(lir_text(src));
}

/// A function with no body is a **declaration**: no blocks, no locals, and a
/// call to it is an ordinary direct call. A backend emits the reference and
/// lets the linker find it.
#[test]
fn lir_snapshot_an_extern_function_has_no_blocks() {
    let src = "\
puts :: extern(\"c\") func (s: *u8) -> i32
shout :: func (s: *u8) -> i32 { return puts(s) }
";
    insta::assert_snapshot!(lir_text(src));
}

/// An array of constants is **one constant** — `{ 1, 2, 3 }` in the dump, not
/// three stores — and an array is the one aggregate §7b does not flatten, so
/// the type stays `[3]i32`.
///
/// Indexing one needs a **place**, because `xs[i]` goes through `core`'s
/// `Index` impl and that takes `&xs`. A `::` constant has no address, so the
/// value is written into a slot first and the slot is what is pointed at. This
/// is the shape rather than a fold: the element is not read out at compile
/// time, which it could be and is worth doing later.
#[test]
fn lir_snapshot_an_array_constant_is_a_value_and_indexing_one_needs_a_place() {
    let src = "\
TABLE: [3]i32 :: .{ 1, 2, 3 }
from_constant :: func () -> i32 { return TABLE[1] }
from_local :: func () -> i32 {
  const t: [3]i32 := .{ 1, 2, 3 }
  return t[1]
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// Safepoints (§6) are on the calls, the allocation, and the loop's **back
/// edge** — the three places a collector can run — and each carries the roots
/// live *before* the instruction, which is the set a stack map has to describe.
#[test]
fn lir_snapshot_a_back_edge_carries_the_live_roots() {
    let src = "\
{ new } :: import <core/mem>
Node :: struct { v: i32 }
work :: func (n: *mut Node) {}
walk :: func (n: i32) -> i32 {
  let p := new.<Node>()
  let mut i := 0
  while i < n {
    work(p)
    i = i + 1
  }
  return p.*.v
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// An allocation that is **returned** escapes (§5), so there is no drop on any
/// exit and the collector owns it. The ladder is empty, which is the point: the
/// pass says nothing rather than guessing.
#[test]
fn lir_snapshot_an_allocation_that_escapes_gets_no_drop() {
    let src = "\
{ new } :: import <core/mem>
Node :: struct { v: i32 }
build :: func () -> *mut Node {
  let p := new.<Node>()
  p.*.v = 1
  return p
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// One decision tree over several kinds of test (§4): a tuple's members, a
/// literal, an or-pattern, a range, and a guard all become comparisons on the
/// same chain of candidates, and a `_` arm is the edge that is left.
#[test]
fn lir_snapshot_a_decision_tree_mixes_tuples_ranges_and_guards() {
    let src = "\
pair :: func (t: (i32, bool)) -> i32 {
  return t.match {
    (0, true) => 1,
    (n, _) if n > 10 => 2,
    _ => 0,
  }
}
grade :: func (n: u8) -> i32 {
  return n.match {
    0..<10 => 1,
    10..=200 => 2,
    201 | 202 => 3,
    _ => 0,
  }
}
";
    insta::assert_snapshot!(lir_text(src));
}

// ===< LIR well-formedness >===

/// Every block ends in exactly one terminator and every edge points at a block
/// that exists.
///
/// A snapshot says what one lowering produced; this says what *any* of them may
/// produce. The two guard different things: a snapshot catches a change, and
/// this catches a graph that is not a graph — an edge to a block that was
/// allocated and never filled, which is the failure mode a CFG builder has.
#[test]
fn every_lir_edge_points_at_a_block_that_exists() {
    let src = "\
Shape :: enum { dot, circle(i32), rect { w: i32, h: i32 } }
cleanup :: func () {}
area :: func (s: Shape) -> i32 {
  defer cleanup()
  let acc := 0
  let i := 0
  while i < 4 {
    if i == 2 { continue }
    acc = acc + s.match { .dot => 0, .circle(r) => r, .rect { w, h } => w * h }
    i = i + 1
    if acc > 100 { return acc }
  }
  return acc
}
main :: func () { let x := area(.circle(3)) }
";
    let session = {
        let mut loader = MemLoader::new();
        loader = loader.with("main", src);
        let mut s = Session::with_loader(Box::new(loader));
        let file = s.load_entry("main").expect("entry loads");
        analyze(&mut s, file);
        s
    };
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let layouts = crate::ir::layout::Layouts::new(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        session.options.target,
    );
    let program = crate::lir::lower(
        &session.defs,
        &session.ir_meta,
        &session.linked,
        &layouts,
        &session.options,
        &session.lang_items,
        &session.sources,
    );
    for f in &program.funcs {
        for b in &f.blocks {
            let targets: Vec<crate::lir::BlockId> = match &b.term.kind {
                crate::lir::TermKind::Goto(t) => vec![*t],
                crate::lir::TermKind::Switch {
                    arms, otherwise, ..
                } => arms.iter().map(|(_, t)| *t).chain([*otherwise]).collect(),
                _ => Vec::new(),
            };
            for t in targets {
                assert!(
                    (t.0 as usize) < f.blocks.len(),
                    "{}: bb{} jumps to bb{}, which does not exist",
                    f.name,
                    b.id.0,
                    t.0
                );
            }
        }
        // Every local an instruction names has a slot. Slots are dense and
        // assigned in order, so this is the check that a projection built for
        // one function did not escape into another.
        for b in &f.blocks {
            for s in &b.stmts {
                if let crate::lir::StmtKind::Assign { place, .. } = &s.kind
                    && let crate::lir::Base::Local(id) = place.base
                {
                    assert!(
                        (id.0 as usize) < f.locals.len(),
                        "{}: local _{} has no slot",
                        f.name,
                        id.0
                    );
                }
            }
        }
    }
}

/// §3's requirement, as an assertion rather than a picture: a `defer` body
/// appears **once** however many paths run it. Three `return`s in one scope
/// share one rung.
#[test]
fn a_defer_body_is_emitted_once_per_kind_of_exit_not_once_per_site() {
    let src = "\
cleanup :: func () {}
run :: func (n: i32) -> i32 {
  defer cleanup()
  if n > 10 { return 1 }
  if n > 5 { return 2 }
  return 3
}
";
    let text = lir_text(src);
    let calls = text.matches("call cleanup()").count();
    assert_eq!(calls, 1, "{text}");
}

// ===< Indexing, through the trait like every other operator >===

/// `a[i]` is `Index.index(&a, i).*` for **every** type (§6.13), the built-in
/// sequences included. Their impls are in `core` and their members are
/// `#intrinsic`, so the call is the address computation and the compiler
/// carries no special case for what indexing means.
#[test]
fn indexing_a_sequence_goes_through_the_index_trait() {
    let src = "\
f :: func (s: []i32, a: [3]i32) -> i32 { return s[0] + a[1] }
";
    let ir = ir_text(src);
    assert!(ir.contains("$index("), "{ir}");
    // And the index is a `usize`, because that is what `Index.<usize>` says it
    // is. It used to be whatever an unconstrained integer literal defaulted to.
    assert!(ir.contains(": usize"), "{ir}");
}

/// A sequence's write permission is in its **type**, not in its receiver — so
/// the sequences implement `Index` and not `IndexMut`, and `s[0] = 1` on a
/// `[]mut T` is legal however immutably the binding holding it was declared
/// (§2.3). Routing the write side through `IndexMut`'s `*mut Self` would refuse
/// every one of them.
#[test]
fn a_mutable_slice_is_writable_through_an_immutable_binding() {
    let src = "\
f :: func (s: []mut i32) { s[0] = 1 }
g :: func () { const a := [_]i32 { 1, 2, 3 }\n  let b := a[0] }
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
}

/// ...and the message when the permission is absent still names the sequence,
/// not the pointer the desugaring put in between.
#[test]
fn writing_to_a_read_only_slice_still_names_the_slice() {
    let msgs = messages("f :: func (s: []i32) { s[0] = 1 }\n");
    assert_eq!(msgs.len(), 1, "{msgs:#?}");
    assert!(
        msgs[0].contains("element of a read-only slice"),
        "{msgs:#?}"
    );
}

/// A composite literal is a value and a `::` binding *is* its value (§2.5), so
/// a constant array has one — and indexing it is a question the evaluator can
/// answer, even though the desugaring puts a pointer in the middle.
#[test]
fn a_constant_array_folds_and_can_be_indexed() {
    let src = "\
A: [3]i32 :: .{ 10, 20, 30 }
B: i32 :: A[1]
main :: func () {}
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
    let ir = ir_text(src);
    assert!(ir.contains("// = { 10, 20, 30 }"), "{ir}");
    assert!(ir.contains("// = 20"), "{ir}");
}

/// §7b: an array kept its own shape, so element `i` is a projection. A slice
/// did not — it is `{ ptr, len }` — so reaching its element is the pointer it
/// holds moved along by `i`. That arithmetic exists in LIR and nowhere above it.
#[test]
fn lir_snapshot_indexing_a_slice_is_pointer_arithmetic() {
    let src = "\
read :: func (s: []i32, a: [3]i32, k: usize) -> i32 { return s[k] + a[k] }
";
    insta::assert_snapshot!(lir_text(src));
}

// ===< Bounds >===

/// A `[N]T` carries its length in its type (§3.2), so when the index is also a
/// compile-time value the comparison has two known numbers in it and an answer
/// that cannot change. There is no input and no build setting under which `a[7]`
/// on a `[3]i32` is anything but a trap, so it is refused where it is written.
#[test]
fn a_constant_index_past_a_fixed_arrays_end_is_a_compile_error() {
    for src in [
        "f :: func () -> i32 {\n  const a := [_]i32 { 1, 2, 3 }\n  return a[7]\n}\n",
        // Through a named constant, and through arithmetic: the same evaluator
        // answers both, so neither is a different case.
        "K: usize :: 5\nf :: func (a: [3]i32) -> i32 { return a[K] }\n",
        "f :: func (a: [3]i32) -> i32 { return a[1 + 2] }\n",
        // The last index is `N - 1`; `N` itself is one past the end.
        "f :: func (a: [3]i32) -> i32 { return a[3] }\n",
    ] {
        let msgs = messages(src);
        assert_eq!(msgs.len(), 1, "{src}\n{msgs:#?}");
        assert!(msgs[0].contains("out of bounds"), "{msgs:#?}");
    }
}

/// And an index that is known to be *in* range is not a diagnostic and not a
/// branch either — the comparison has a known answer, so there is nothing to
/// emit.
#[test]
fn a_constant_index_in_range_is_neither_reported_nor_checked() {
    let src = "f :: func (a: [3]i32) -> i32 { return a[2] }\n";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
    let lir = lir_text(src);
    assert!(!lir.contains("out of bounds"), "{lir}");
}

/// A slice's length is a run-time value, so there is nothing to compare against
/// until the program runs — which is exactly what §3.2 promises: a trap.
#[test]
fn a_slice_index_is_checked_against_its_length_at_run_time() {
    let lir = lir_text("f :: func (s: []i32, k: usize) -> i32 { return s[k] }\n");
    assert!(lir.contains("k_1 < "), "{lir}");
    assert!(
        lir.contains("call core.panic(\"index out of bounds\""),
        "{lir}"
    );
}

/// An array with a run-time index is checked too — against the constant its
/// type carries.
#[test]
fn a_fixed_array_with_a_runtime_index_is_checked_against_its_length() {
    let lir = lir_text("f :: func (a: [3]i32, k: usize) -> i32 { return a[k] }\n");
    assert!(lir.contains("k_1 < 3"), "{lir}");
}

/// `#unsafe` disables the run-time safety checks in its scope (§9). The whole
/// meaning of the directive is that they are off, so a check emitted anyway
/// would make it a comment.
#[test]
fn unsafe_turns_the_bounds_check_off() {
    let lir = lir_text("f :: #unsafe func (s: []i32, k: usize) -> i32 { return s[k] }\n");
    assert!(!lir.contains("out of bounds"), "{lir}");
}

/// The shape §3.2's trap takes: a comparison, an edge, and a block that does not
/// come back — the same one `overflow=trap` takes, for the same reason.
#[test]
fn lir_snapshot_a_bounds_check_is_a_comparison_and_an_edge() {
    let src = "\
get :: func (s: []i32, k: usize) -> i32 { return s[k] }
";
    insta::assert_snapshot!(lir_text(src));
}

// ===< Phase 9: drops and safepoints (`design/lir.md` §5, §6) >===

/// A `comptime_int` is not a machine type, and the `$cast` the IR carries out of
/// a literal is bookkeeping about where its type came from (§6.5). LIR folds it
/// into the definition: `10` written where an `i32` is wanted **is** an `i32`.
#[test]
fn a_comptime_literal_reaches_lir_as_a_literal() {
    let lir = lir_text("f :: func () -> i32 { return 10 }\n");
    assert!(!lir.contains("comptime_int"), "{lir}");
    assert!(!lir.contains("cast"), "{lir}");
    assert!(lir.contains("return 10"), "{lir}");
}

/// An enum is `{ tag, payload }` by §7b, so reading the discriminant is a member
/// read and needs no operation of its own.
#[test]
fn the_discriminant_is_a_member_read() {
    let lir = lir_text(
        "\
Shape :: enum { dot, circle(i32) }
f :: func (s: Shape) -> i32 { return s.match { .dot => 0, .circle(r) => r } }
",
    );
    assert!(lir.contains(":= s_0.tag"), "{lir}");
    assert!(!lir.contains("discriminant"), "{lir}");
}

/// The compiler's own failures go through `core`'s `panic`, found by its
/// `#lang("panic")` tag — the same function a written `panic("...")` calls, so
/// there is no `$panic` operation for a backend to invent a meaning for.
#[test]
fn a_trapped_overflow_calls_cores_panic() {
    let lir = lir_text_with(
        "f :: func (a: i32, b: i32) -> i32 { return a + b }\n",
        crate::common::options::Options {
            overflow: crate::common::options::OverflowMode::Trap,
            ..Default::default()
        },
    );
    assert!(lir.contains("call core.panic(\"integer overflow\""), "{lir}");
    assert!(!lir.contains("$panic"), "{lir}");
}

/// A program may answer a `#lang` tag `core` already answered: `core`'s claim is
/// a **default**, which is what makes the panic handler replaceable (§9).
#[test]
fn a_program_may_replace_cores_panic_handler() {
    let src = "\
{ Location } :: import <core/loc>
{ trap } :: import <core/fail>
my_handler :: #lang(\"panic_handler\") func (msg: str, loc: Location) -> never { trap() }
@public main :: func () {}
";
    let session = analyze_mem(&[("main", src)], "main");
    assert!(!session.has_errors(), "{:#?}", session.diagnostics);
    let def = session
        .lang_items
        .get("panic_handler")
        .expect("the tag is claimed");
    assert_eq!(session.defs.get(def).name.as_str(), "my_handler");
}

/// Two claims from the same side are still the duplicate they always were: the
/// rule above is about `core` being the fallback, not about the tag being a
/// free-for-all.
#[test]
fn two_claims_on_one_lang_tag_in_one_program_are_an_error() {
    let src = "\
{ Location } :: import <core/loc>
{ trap } :: import <core/fail>
a :: #lang(\"panic_handler\") func (msg: str, loc: Location) -> never { trap() }
b :: #lang(\"panic_handler\") func (msg: str, loc: Location) -> never { trap() }
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("duplicate `#lang(\"panic_handler\")`")),
        "{:#?}",
        messages(src)
    );
}

/// §5: an allocation nothing outside the scope can reach is freed explicitly,
/// on the cleanup ladder every exit goes through.
#[test]
fn an_allocation_that_does_not_escape_is_dropped() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () -> i32 {
  let p := new.<Node>()
  p.*.x = 5
  return p.*.x
}
",
    );
    assert!(lir.contains("drop p_"), "{lir}");
}

/// "Passed to any call" escapes, and deliberately so (§5): without per-function
/// summaries there is no way to know whether a callee retains what it is given,
/// and guessing wrong frees memory something still references.
#[test]
fn an_allocation_passed_to_a_call_is_not_dropped() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
sink :: func (p: *mut Node) {}
f :: func () -> i32 {
  let p := new.<Node>()
  sink(p)
  return p.*.x
}
",
    );
    assert!(!lir.contains("drop "), "{lir}");
}

/// A returned allocation outlives its scope, which is the first rule §5 lists.
#[test]
fn a_returned_allocation_is_not_dropped() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () -> *mut Node {
  let p := new.<Node>()
  return p
}
",
    );
    assert!(!lir.contains("drop "), "{lir}");
}

/// A call is a safepoint, and it carries the pointer-holding locals something
/// after it still reads (§6).
#[test]
fn a_call_carries_the_live_pointers() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
sink :: func (p: *mut Node) {}
f :: func () -> i32 {
  let p := new.<Node>()
  sink(p)
  return p.*.x
}
",
    );
    assert!(lir.contains("@safepoint { live: [p_"), "{lir}");
    assert!(lir.contains("reloc p_"), "{lir}");
}

/// A loop that calls nothing and allocates nothing would otherwise be a region
/// the collector can never interrupt, so its **back edge** is a safepoint (§6).
#[test]
fn a_loop_back_edge_is_a_safepoint() {
    let lir = lir_text(
        "\
f :: func (n: i32) -> i32 {
  let i := 0
  while i < n { i = i + 1 }
  return i
}
",
    );
    let lines: Vec<&str> = lir.lines().collect();
    assert!(
        lines.windows(2).any(|w| {
            w[0].trim_start().starts_with("goto") && w[1].trim_start().starts_with("@safepoint")
        }),
        "{lir}"
    );
}

/// A non-pointer local is never a root (§6), and precision is not only a
/// performance question: with a moving collector an over-approximate live set
/// means relocating objects nothing will ever read again.
#[test]
fn an_integer_local_is_never_a_root() {
    let lir = lir_text(
        "\
g :: func (n: i32) -> i32 { return n }
f :: func () -> i32 {
  let a := 1
  let b := g(a)
  return a + b
}
",
    );
    assert!(lir.contains("@safepoint { live: [] }"), "{lir}");
    assert!(!lir.contains("reloc a_"), "{lir}");
}

/// The whole of §5 and §6 in one function: an allocation dropped on the ladder,
/// a call that is a safepoint, and the relocations that make it a definition.
#[test]
fn lir_snapshot_drops_ride_the_ladder_and_calls_are_safepoints() {
    let src = "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
report :: func (n: i32) {}
f :: func (c: bool) -> i32 {
  let p := new.<Node>()
  defer report(0)
  if c { return p.*.x }
  report(p.*.x)
  return 0
}
";
    insta::assert_snapshot!(lir_text(src));
}

// ===< Is LIR low enough? (`design/lir.md` §1, §10) >===
//
// The question phase 10 turns on: can a backend walk this and emit code without
// re-deriving anything? A snapshot cannot answer it — it says what one program
// lowered to. These walk **every** function of a program that uses most of the
// language, `core` included, and assert the properties an LLVM (or C, or wasm)
// emitter needs to be true of all of them.

/// A program broad enough for the invariants below to have something to chew on.
const BROAD: &str = "\
{ new, make, drop, size_of } :: import <core/mem>
Shape :: enum { dot, circle(i32), rect { w: i32, h: i32 } }
Node :: struct { x: i32, tag: u8 }
Speak :: trait { say :: func (self: *Self) -> i32 }
Dog :: struct { n: i32 }
impl Speak for Dog { say :: func (self: *Self) -> i32 { return self.*.n } }
cleanup :: func () {}
area :: func (s: Shape) -> i32 {
  defer cleanup()
  return s.match {
    .dot => 0,
    .circle(r) if r > 3 => r,
    .circle(r) => -r,
    .rect { w, h } => w * h,
  }
}
words :: func (s: str) -> i32 { return s.match { \"hi\" => 1, \"\" => 2, _ => 0 } }
sum :: func (xs: []i32) -> i32 {
  let acc := 0
  let i: usize := 0
  while i < xs.len() { acc = acc + xs[i] i = i + 1 }
  return acc
}
ratio :: func (a: i32, b: i32) -> i32 { return a / b }
scoped :: func () -> i32 {
  let p := new.<Node>()
  p.*.x = 1
  return p.*.x
}
manual :: func () -> i32 {
  let q := new.<Node>()
  let v := q.*.x
  drop(q)
  return v
}
dyn_call :: func (d: *Dog) -> i32 { let s: *dyn Speak := d return s.say() }
widen :: func (n: u8) -> i64 { return n }
@public main :: func () {
  let a := area(.circle(4))
  let b := sum(make.<[]i32>(3))
  let c := ratio(9, 3)
  let d := scoped()
  let e := manual()
  let f := dyn_call(&Dog { n: 1 })
  let g := widen(2)
  let h := words(\"hi\")
  let i := size_of.<Node>()
}
";

/// Visit every place and operand a function mentions, wherever they hide.
fn each_place(f: &crate::lir::Function, mut visit: impl FnMut(&crate::lir::Place)) {
    use crate::lir::{Callee, Operand, Rvalue, StmtKind, TermKind};
    fn op(o: &Operand, visit: &mut impl FnMut(&crate::lir::Place)) {
        if let Operand::Copy(p) = o {
            visit(p);
        }
    }
    let visit = &mut visit;
    for b in &f.blocks {
        for s in &b.stmts {
            match &s.kind {
                StmtKind::Assign { place, value } => {
                    visit(place);
                    match value {
                        Rvalue::Use(o) | Rvalue::Unary { operand: o, .. } => op(o, visit),
                        Rvalue::Ref { place, .. } => visit(place),
                        Rvalue::Cast { value, .. } => op(value, visit),
                        Rvalue::Binary { lhs, rhs, .. } => {
                            op(lhs, visit);
                            op(rhs, visit);
                        }
                        Rvalue::Offset { ptr, index, .. } => {
                            op(ptr, visit);
                            op(index, visit);
                        }
                        Rvalue::Aggregate { fields, .. } => {
                            fields.iter().for_each(|o| op(o, visit))
                        }
                        Rvalue::Builtin { args, .. } => args.iter().for_each(|o| op(o, visit)),
                    }
                }
                StmtKind::Call { dest, callee, args } => {
                    if let Some(d) = dest {
                        visit(d);
                    }
                    if let Callee::Indirect(o) = callee {
                        op(o, visit);
                    }
                    args.iter().for_each(|o| op(o, visit));
                }
                StmtKind::Intrinsic { dest, args, .. } => {
                    if let Some(d) = dest {
                        visit(d);
                    }
                    args.iter().for_each(|o| op(o, visit));
                }
                StmtKind::Drop(o) => op(o, visit),
            }
        }
        match &b.term.kind {
            TermKind::Switch { value, .. } => op(value, visit),
            TermKind::Return(Some(v)) => op(v, visit),
            _ => {}
        }
    }
}

/// Nothing in a lowered program has a type a machine cannot hold.
///
/// `comptime_int` is the one that used to get through — the `$cast` out of a
/// literal left the source type on an operand (§9). `Never` and `Void` are the
/// other two, and they are why an intrinsic is a statement with an optional
/// destination rather than an [`Rvalue`]: a slot typed "no value" is a slot no
/// register file has.
#[test]
fn no_local_has_a_type_a_machine_cannot_hold() {
    let program = lir_whole_program(BROAD);
    for f in &program.funcs {
        for l in &f.locals {
            assert!(
                !matches!(
                    l.ty,
                    Ty::ComptimeInt
                        | Ty::ComptimeFloat
                        | Ty::ComptimeStr
                        | Ty::Never
                        | Ty::Void
                        | Ty::Error
                        | Ty::Var(_)
                ),
                "{}: _{} is typed `{}`",
                f.name,
                l.id.0,
                l.ty.display(&crate::sema::def::DefTable::new())
            );
        }
    }
}

/// Every place names a slot the function has, wherever the place appears — an
/// argument, a switch operand, a drop, a safepoint's live set.
#[test]
fn every_place_and_live_local_names_a_slot_that_exists() {
    let program = lir_whole_program(BROAD);
    for f in &program.funcs {
        each_place(f, |p| {
            if let crate::lir::Base::Local(id) = p.base {
                assert!(
                    (id.0 as usize) < f.locals.len(),
                    "{}: _{} has no slot",
                    f.name,
                    id.0
                );
            }
        });
        for b in &f.blocks {
            let points = b
                .stmts
                .iter()
                .filter_map(|s| s.safepoint.as_ref())
                .chain(b.term.safepoint.as_ref());
            for sp in points {
                for l in &sp.live {
                    assert!(
                        (l.0 as usize) < f.locals.len(),
                        "{}: safepoint names _{}, which has no slot",
                        f.name,
                        l.0
                    );
                }
            }
        }
    }
}

/// Block ids are their index, and block 0 is the entry. A backend that builds
/// LLVM blocks in one pass needs both, and neither is worth re-deriving.
#[test]
fn block_ids_are_dense_and_zero_is_the_entry() {
    let program = lir_whole_program(BROAD);
    for f in &program.funcs {
        for (i, b) in f.blocks.iter().enumerate() {
            assert_eq!(b.id.0 as usize, i, "{}: bb{} is at index {i}", f.name, b.id.0);
        }
    }
}

/// Every direct call names a function the program contains. A `Callee::Static`
/// carries the symbol the linker sees (§7), so a name with nothing behind it is
/// a link failure the compiler could have caught.
#[test]
fn every_direct_call_names_a_function_in_the_program() {
    let program = lir_whole_program(BROAD);
    let known: std::collections::HashSet<&str> =
        program.funcs.iter().map(|f| f.symbol.as_str()).collect();
    for f in &program.funcs {
        for b in &f.blocks {
            for s in &b.stmts {
                if let crate::lir::StmtKind::Call {
                    callee: crate::lir::Callee::Static { symbol, .. },
                    ..
                } = &s.kind
                {
                    assert!(
                        known.contains(symbol.as_str()),
                        "{} calls `{symbol}`, which the program does not define",
                        f.name
                    );
                }
            }
        }
    }
}

/// A `Binary` is a **machine** instruction, so neither side of one is ever an
/// aggregate.
///
/// This is the invariant the `str` literal pattern broke: it emitted
/// `s == "hi"` on a `{ ptr, len }`, which no target can compare and which would
/// have compared *addresses* if a backend had tried. Text equality is a call to
/// `core`'s `#lang("bytes_eq")` now, so nothing structural reaches an `icmp`.
#[test]
fn no_binary_operation_has_an_aggregate_operand() {
    let program = lir_whole_program(BROAD);
    for f in &program.funcs {
        for b in &f.blocks {
            for s in &b.stmts {
                let crate::lir::StmtKind::Assign {
                    value: crate::lir::Rvalue::Binary { op, lhs, rhs },
                    ..
                } = &s.kind
                else {
                    continue;
                };
                for side in [lhs, rhs] {
                    let scalar = match side {
                        crate::lir::Operand::Const(crate::lir::Constant::Value(v)) => !matches!(
                            v,
                            crate::ir::ConstValue::Str(_)
                                | crate::ir::ConstValue::Bytes(_)
                                | crate::ir::ConstValue::Aggregate(_)
                                | crate::ir::ConstValue::Variant { .. }
                        ),
                        _ => true,
                    };
                    assert!(scalar, "{}: {op:?} has an aggregate operand", f.name);
                }
            }
        }
    }
}

/// Every named type a local has is in the program's own table, so a backend
/// never has to reach back into the compiler's def table for a layout (§7b).
#[test]
fn every_nominal_local_type_is_in_the_type_table() {
    let program = lir_whole_program(BROAD);
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", BROAD)));
    let file = session.load_entry("main").expect("entry loads");
    analyze(&mut session, file);
    let keys: std::collections::HashSet<&str> =
        program.types.iter().map(|t| t.key.as_str()).collect();
    // Through pointers and arrays too: a `*Node` is useless to a backend
    // unless `Node`'s layout is reachable, and the table is where it lives.
    fn walk(defs: &crate::sema::def::DefTable, ty: &Ty, f: &mut impl FnMut(&Ty), depth: usize) {
        if depth > 6 {
            return;
        }
        match ty {
            Ty::Nominal { .. } | Ty::Tuple(_) | Ty::Slice { .. } => f(ty),
            _ => {}
        }
        match ty {
            Ty::Ptr { inner, .. } | Ty::Array { inner, .. } | Ty::Slice { inner, .. } => {
                walk(defs, inner, f, depth + 1)
            }
            Ty::Tuple(elems) => elems.iter().for_each(|t| walk(defs, t, f, depth + 1)),
            _ => {}
        }
    }
    for func in &program.funcs {
        for l in &func.locals {
            walk(
                &session.defs,
                &l.ty,
                &mut |ty| {
                    let key = crate::ir::mono::type_key(&session.defs, ty);
                    assert!(
                        keys.contains(key.as_str()),
                        "{}: _{} mentions `{key}`, which has no definition in the table",
                        func.name,
                        l.id.0
                    );
                },
                0,
            );
        }
    }
}

// ===< `drop`, written by hand (§6.9) >===

/// The program's `drop(p)` and the compiler's are the **same instruction**, so
/// there is one thing for a backend to implement rather than two.
#[test]
fn a_written_drop_is_the_same_instruction_the_compiler_emits() {
    let lir = lir_text(
        "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () -> i32 {
  let p := new.<Node>()
  let v := p.*.x
  drop(p)
  return v
}
",
    );
    assert_eq!(lir.matches("drop p_").count(), 1, "{lir}");
}

/// Writing it takes the question on: escape analysis stops answering, because
/// passing a local to anything is what disqualifies it (§5's whitelist). Two
/// drops of one object would be a double free.
#[test]
fn a_written_drop_replaces_the_automatic_one() {
    let lir = lir_text(
        "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () {
  let p := new.<Node>()
  p.*.x = 1
  drop(p)
}
",
    );
    assert_eq!(lir.matches("drop ").count(), 1, "{lir}");
    assert!(!lir.contains("cleanup"), "{lir}");
}

/// `drop` of something no local names is allowed and tracks nothing: there is
/// no name to forbid afterwards.
#[test]
fn dropping_through_a_field_is_allowed_and_untracked() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { next: *mut Node }
f :: func (n: *mut Node) { drop(n.*.next) }
@public main :: func () {}
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
    let lir = lir_text(src);
    assert!(lir.contains("drop "), "{lir}");
}

/// A pointer whose object was freed is the one thing a collected language exists
/// to make impossible, so it is a compile error rather than a run-time surprise.
#[test]
fn using_a_value_after_dropping_it_is_refused() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () -> i32 {
  let p := new.<Node>()
  drop(p)
  return p.*.x
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("`p` is used after it was dropped")),
        "{:#?}",
        messages(src)
    );
}

#[test]
fn dropping_the_same_value_twice_is_refused() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () {
  let p := new.<Node>()
  drop(p)
  drop(p)
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("`p` is dropped twice")),
        "{:#?}",
        messages(src)
    );
}

/// A drop on **either** side of a branch counts afterwards: "dropped on some
/// paths" is not a state a program can be in, and only one of the two answers
/// is safe.
#[test]
fn a_drop_in_one_branch_forbids_the_use_after_the_branch() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func (c: bool) -> i32 {
  let p := new.<Node>()
  if c { drop(p) }
  return p.*.x
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("used after it was dropped")),
        "{:#?}",
        messages(src)
    );
}

/// The same, through a `match` arm.
#[test]
fn a_drop_in_a_match_arm_forbids_the_use_after_the_match() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
Pick :: enum { yes, no }
f :: func (k: Pick) -> i32 {
  let p := new.<Node>()
  let _ := k.match { .yes => { drop(p) 1 }, .no => 0 }
  return p.*.x
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("used after it was dropped")),
        "{:#?}",
        messages(src)
    );
}

/// A loop body runs again. Dropping something declared outside it is a double
/// free with no use in between to blame, so the *drop* is the diagnostic.
#[test]
fn dropping_an_outer_value_inside_a_loop_is_refused() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () {
  let p := new.<Node>()
  loop { drop(p) break }
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("`p` is dropped inside a loop")),
        "{:#?}",
        messages(src)
    );
}

/// A value declared **inside** the loop is a different object each iteration,
/// so dropping it there is exactly right.
#[test]
fn dropping_a_value_declared_inside_the_loop_is_fine() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func (n: i32) {
  let i := 0
  while i < n {
    let p := new.<Node>()
    drop(p)
    i = i + 1
  }
}
@public main :: func () {}
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
}

/// Assigning to the local gives it an object again; refusing the next use would
/// be refusing a correct program.
#[test]
fn assigning_after_a_drop_revives_the_value() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () -> i32 {
  let mut p := new.<Node>()
  drop(p)
  p = new.<Node>()
  return p.*.x
}
@public main :: func () {}
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
}

/// Reading the value on the way *into* the drop is not a use after it.
#[test]
fn the_drops_own_argument_is_not_a_use_after_drop() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func () { let p := new.<Node>() drop(p) }
@public main :: func () {}
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
}

// ===< Dividing by zero (§7d) >===

/// It is **not** overflow, so it is not the `overflow=` setting's to turn off:
/// `wrap` says what `i32::MAX + 1` means and has nothing to say about `x / 0`.
#[test]
fn division_by_zero_traps_even_when_overflow_wraps() {
    let lir = lir_text_with(
        "f :: func (a: i32, b: i32) -> i32 { return a / b }\n",
        crate::common::options::Options {
            overflow: crate::common::options::OverflowMode::Wrap,
            ..Default::default()
        },
    );
    assert!(lir.contains("division by zero"), "{lir}");
    assert!(lir.contains("call core.panic(\"division by zero\""), "{lir}");
}

/// `%` has the same fault for the same reason.
#[test]
fn remainder_by_zero_traps_too() {
    let lir = lir_text("f :: func (a: i32, b: i32) -> i32 { return a % b }\n");
    assert!(lir.contains("division by zero"), "{lir}");
}

/// A divisor that is a known non-zero constant has a comparison whose answer
/// cannot change, so it gets no branch — the same rule the bounds check follows.
#[test]
fn a_constant_non_zero_divisor_needs_no_check() {
    let lir = lir_text("f :: func (a: i32) -> i32 { return a / 2 }\n");
    assert!(!lir.contains("division by zero"), "{lir}");
}

/// `#unsafe` is the one thing that removes it (§9), because that is the whole
/// meaning of the directive.
#[test]
fn unsafe_turns_the_zero_check_off() {
    let lir = lir_text("f :: #unsafe func (a: i32, b: i32) -> i32 { return a / b }\n");
    assert!(!lir.contains("division by zero"), "{lir}");
}

/// A float divided by zero is an infinity, which is a value and not a fault.
#[test]
fn a_float_division_has_no_zero_check() {
    let lir = lir_text("f :: func (a: f64, b: f64) -> f64 { return a / b }\n");
    assert!(!lir.contains("division by zero"), "{lir}");
}

// ===< Text equality (§6.13) >===

/// A `str` is `{ ptr, len }` by LIR (§7b), so comparing one with `==` would
/// compare *addresses*. Text compares by its bytes, and the comparison lives in
/// `core` so a pattern and an `==` cannot disagree.
#[test]
fn a_string_literal_pattern_calls_cores_byte_equality() {
    let lir = lir_text("f :: func (s: str) -> i32 { return s.match { \"hi\" => 1, _ => 0 } }\n");
    // `str` is a `distinct []u8` and a `distinct` is its representation by
    // this level (§9), so the argument is the slice itself rather than a
    // member read out of a wrapper.
    assert!(lir.contains("call core.bytes_eq(s_0, b\"hi\")"), "{lir}");
}

/// `==` on two `str`s now resolves at all — it did not before, because nothing
/// implemented `Eq` for `str` — and it reaches the same `bytes_eq` the pattern
/// does, one hop further along.
#[test]
fn str_equality_goes_through_the_eq_impl_to_the_same_function() {
    let src = "eq :: func (a: str, b: str) -> bool { return a == b }\n@public main :: func () {}\n";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
    assert!(
        lir_text(src).contains("call core.<impl str>.<as core.Eq>.eq"),
        "{}",
        lir_text(src)
    );
    // And that impl is the one line of forwarding it looks like.
    let whole = lir_whole_program(src);
    let eq = whole
        .funcs
        .iter()
        .find(|f| f.name.contains("<impl str>") && f.name.ends_with(".eq"))
        .expect("core implements Eq for str");
    let calls: Vec<&str> = eq
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match &s.kind {
            crate::lir::StmtKind::Call {
                callee: crate::lir::Callee::Static { name, .. },
                ..
            } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(calls.contains(&"core.bytes_eq"), "{calls:?}");
}

/// A byte-string literal is the same question one level down: `[]u8` needs no
/// projection to reach its bytes.
#[test]
fn a_byte_string_pattern_compares_bytes_directly() {
    let lir = lir_text("f :: func (b: []u8) -> i32 { return b.match { b\"hi\" => 1, _ => 0 } }\n");
    assert!(lir.contains("call core.bytes_eq(b_0, b\"hi\")"), "{lir}");
}

/// The empty pattern is a length test and nothing else, which is what the
/// library function does with it — there is no special case here.
#[test]
fn an_empty_string_pattern_is_the_same_call() {
    let lir = lir_text("f :: func (s: str) -> i32 { return s.match { \"\" => 1, _ => 0 } }\n");
    assert!(lir.contains("call core.bytes_eq(s_0, b\"\")"), "{lir}");
}

// ===< Safepoints, the cases the first pass got wrong >===

/// `gc_collect` asks for a collection outright, so it is a safepoint like a call
/// is (§6).
#[test]
fn a_gc_collect_is_a_safepoint() {
    let lir = lir_text(
        "\
{ new, gc_collect } :: import <core/mem>
{ gc_collect } :: import <core/gc>
Node :: struct { x: i32 }
f :: func () -> i32 {
  let p := new.<Node>()
  gc_collect()
  return p.*.x
}
",
    );
    assert!(lir.contains("$gc_collect()"), "{lir}");
    assert!(lir.contains("@safepoint { live: [p_"), "{lir}");
}

/// Liveness ends at the last **read**, and `gc_keep_alive` is a read (§6). It
/// needs no special case in the pass, which is the test.
#[test]
fn gc_keep_alive_extends_a_live_range_across_a_call() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
{ gc_keep_alive } :: import <core/gc>
Node :: struct { x: i32 }
sink :: func (n: i32) {}
f :: func () {
  let p := new.<Node>()
  sink(p.*.x)
  gc_keep_alive(p)
}
",
    );
    // Without the keep-alive `p` is dead at the `sink` call — its last read was
    // the argument. With it, the call has to trace `p`.
    let at_call = lir
        .lines()
        .skip_while(|l| !l.contains("call sink"))
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(at_call.contains("p_"), "{lir}");
}

/// A struct holding a pointer is a root: the frame slot holding it is where that
/// pointer lives (§6).
#[test]
fn a_struct_holding_a_pointer_is_a_root() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
Pair :: struct { p: *mut Node, n: i32 }
sink :: func (n: i32) {}
f :: func (pair: Pair) -> i32 {
  sink(pair.n)
  return pair.p.*.x
}
",
    );
    assert!(lir.contains("reloc pair_0"), "{lir}");
}

/// An enum whose flattened payload is `[N]u8` says nothing about pointers, so
/// the question has to be asked of the **variants**.
#[test]
fn an_enum_variant_holding_a_pointer_makes_the_enum_a_root() {
    let lir = lir_text(
        "\
{ new } :: import <core/mem>
Node :: struct { x: i32 }
Maybe :: enum { none, some(*mut Node) }
sink :: func () {}
f :: func (m: Maybe) -> i32 {
  sink()
  return m.match { .none => 0, .some(p) => p.*.x }
}
",
    );
    assert!(lir.contains("reloc m_0"), "{lir}");
}

/// An `i32` is never a root, and precision is not only a performance question:
/// with a moving collector an over-approximate set relocates objects nothing
/// will read again (§6).
#[test]
fn a_call_taking_only_integers_has_an_empty_live_set() {
    let lir = lir_text(
        "\
g :: func (n: i32) -> i32 { return n }
f :: func () -> i32 { return g(1) + 2 }
",
    );
    assert!(lir.contains("@safepoint { live: [] }"), "{lir}");
}

/// Both back edges of a nested loop are safepoints: an inner loop that never
/// finishes would otherwise pin the collector out just as an outer one would.
#[test]
fn every_loop_back_edge_is_a_safepoint() {
    let lir = lir_text(
        "\
f :: func (n: i32) -> i32 {
  let t := 0
  let i := 0
  while i < n {
    let j := 0
    while j < n { t = t + 1 j = j + 1 }
    i = i + 1
  }
  return t
}
",
    );
    let lines: Vec<&str> = lir.lines().collect();
    let edges = lines
        .windows(2)
        .filter(|w| {
            w[0].trim_start().starts_with("goto") && w[1].trim_start().starts_with("@safepoint")
        })
        .count();
    assert_eq!(edges, 2, "{lir}");
}

/// A branch that **leaves** has no "afterwards", so its drop must not reach the
/// code below it. Merging it anyway refused a program with nothing wrong.
#[test]
fn a_drop_in_a_branch_that_returns_does_not_reach_the_code_after_it() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func (c: bool) -> i32 {
  let p := new.<Node>()
  if c { drop(p) return 0 }
  return p.*.x
}
@public main :: func () {}
";
    assert!(messages(src).is_empty(), "{:#?}", messages(src));
}

/// A `defer` body runs on the way out, which is *after* everything above it —
/// including a drop.
#[test]
fn a_defer_that_reads_a_dropped_value_is_refused() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
sink :: func (n: i32) {}
f :: func () {
  let p := new.<Node>()
  defer sink(p.*.x)
  drop(p)
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("used after it was dropped")),
        "{:#?}",
        messages(src)
    );
}

/// The loop rule is about the loop the value was declared *outside* of, so an
/// inner loop dropping an outer loop's value is caught too.
#[test]
fn dropping_an_outer_loops_value_in_an_inner_loop_is_refused() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
f :: func (n: i32) {
  let i := 0
  while i < n {
    let p := new.<Node>()
    let j := 0
    while j < n {
      drop(p)
      j = j + 1
    }
    i = i + 1
  }
}
";
    assert!(
        messages(src)
            .iter()
            .any(|m| m.contains("dropped inside a loop")),
        "{:#?}",
        messages(src)
    );
}

/// A `make`d slice drops through its **pointer**: the header is `{ ptr, len }`
/// by this level (§7b) and the allocation is what the first member names, so the
/// instruction's operand is an address like every other one's.
#[test]
fn an_allocated_slice_drops_through_its_pointer() {
    let lir = lir_text(
        "\
{ make } :: import <core/mem>
f :: func () -> i32 {
  let xs := make.<[]i32>(4)
  return 1
}
",
    );
    assert!(lir.contains("drop xs_2.ptr"), "{lir}");
}

/// And in practice a slice a program actually *uses* is not dropped, because
/// every use of one goes through `&xs` — `.len()` and `xs[i]` both do — and
/// taking a local's address is not on §5's whitelist.
///
/// It is recorded as a test rather than left to be discovered: the blunt rule is
/// deliberate, but "slices are effectively never freed early" is a consequence
/// of it worth knowing before someone reads the pass and expects otherwise.
#[test]
fn a_slice_that_is_read_from_escapes_and_is_not_dropped() {
    let lir = lir_text(
        "\
{ make } :: import <core/mem>
f :: func () -> usize {
  let xs := make.<[]i32>(4)
  return xs.len()
}
",
    );
    assert!(!lir.contains("drop "), "{lir}");
}

/// §5 and §6 with a written `drop` in them: the program's free and the
/// compiler's are one instruction, and the object stays traceable up to it.
#[test]
fn lir_snapshot_a_written_drop_is_the_compilers_own_instruction() {
    let src = "\
{ new, drop } :: import <core/mem>
Node :: struct { x: i32 }
report :: func (n: i32) {}
f :: func () -> i32 {
  let kept := new.<Node>()
  let freed := new.<Node>()
  report(freed.*.x)
  drop(freed)
  return kept.*.x
}
";
    insta::assert_snapshot!(lir_text(src));
}



/// A `distinct` adds **no type** at this level, whatever it is distinct from
/// (§9). Over a struct it is that struct, over an enum that enum, over another
/// `distinct` whatever that one ends at — and through a pointer or a slice as
/// well, since `*Handle` is `*Point` for the same reason.
///
/// The peel the IR writes as a cast has nothing left to do: both sides of
/// `cast.<Point>(h)` are `Point` here, so it is a move.
#[test]
fn lir_snapshot_a_distinct_is_its_representation() {
    let src = "\
Point :: struct { x: i32, y: i32 }
Handle :: distinct Point
Color :: enum { red, green, blue(i32) }
Shade :: distinct Color
Deep :: distinct Handle
take :: func (h: Handle, s: Shade, d: Deep, p: *Handle, xs: []Handle) -> i32 {
  return cast.<Point>(h).x
}
mk :: func (p: Point) -> Handle { return cast.<Handle>(p) }
hue :: func (s: Shade) -> i32 {
  return cast.<Color>(s).match { .red => 1, .green => 2, .blue(n) => n }
}
";
    let lir = lir_text(src);
    for gone in ["Handle", "Shade", "Deep"] {
        assert!(!lir.contains(gone), "`{gone}` survived into LIR:\n{lir}");
    }
    insta::assert_snapshot!(lir);
}

/// The scalar case, which is the one every program hits: `usize` is
/// `distinct uint.<64>` in `core` (§3.1), and a one-member struct wrapping a
/// `u64` is not passed like a `u64` under any C ABI.
#[test]
fn a_distinct_scalar_is_the_scalar_and_not_a_wrapper() {
    let lir = lir_text("f :: func (n: usize, m: str) -> usize { return n }\n");
    assert!(lir.contains("func f(n_0: u64, m_1: []u8) -> u64"), "{lir}");
    assert!(!lir.contains("type usize"), "{lir}");
    assert!(!lir.contains("type core.str"), "{lir}");
}
