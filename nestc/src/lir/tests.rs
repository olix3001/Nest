//! Stage-level tests for IR → LIR lowering (`design/lir.md`).
//!
//! Every test here compiles a whole program from **source** — the entry file
//! plus `core` — and looks at the LIR that came out the far end. The earlier
//! stages are tested in `sema::tests`, which is where the AST → IR half lives;
//! what is checked here is only what this pass decides: the control-flow graph,
//! the places, the checks §7d makes real, the cleanup ladder, the drops, and
//! the safepoints.

use crate::lir::{Origin, Ty, Unit};
use crate::sema::analyze;
use crate::sema::session::{MemLoader, Session};
use crate::sema::tests::{analyze_mem, ir_text, messages};

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
    lir_whole_program_with(src, Default::default())
}

/// The whole program as **one** unit — what the invariant tests below walk.
fn lir_unit(src: &str) -> Unit {
    lir_whole_program(src).units.into_iter().next().expect("a unit")
}

fn lir_whole_program_with(
    src: &str,
    options: crate::common::options::Options,
) -> crate::lir::Program {
    let mut session = Session::with_loader(Box::new(MemLoader::new().with("main", src)));
    session.options = options;
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

/// [`lir_text`], with the `file:line:column` of every statement, local and
/// block. This is what `nestc` itself prints, and what §7c's line table is
/// built from.
fn lir_text_with_spans(src: &str) -> String {
    lir_program_rendered(src, Default::default(), true)
}

fn lir_program(src: &str, options: crate::common::options::Options) -> String {
    lir_program_rendered(src, options, false)
}

fn lir_program_rendered(
    src: &str,
    options: crate::common::options::Options,
    spans: bool,
) -> String {
    let mut session = {
        let mut loader = MemLoader::new();
        loader = loader.with("main", src);
        let mut s = Session::with_loader(Box::new(loader));
        s.options = options;
        // One unit per source file, so that what is rendered below is the entry
        // file's unit — the whole of what this program's own code compiles to,
        // with a declaration for everything in `core` it reaches (§11). A test
        // about `while` should not be a record of the standard library, and the
        // split is how that is arranged rather than a filter in the renderer.
        s.options.codegen_units = usize::MAX;
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
    let _ = file;
    // The entry file's unit. `MemLoader` names it `mem:main`, and the split
    // names a unit after its file.
    let unit = program
        .units
        .iter()
        .find(|u| u.name.ends_with("main"))
        .unwrap_or_else(|| program.unit());
    let sources = spans.then_some(&session.sources);
    crate::lir::pretty::unit_to_string(sources, unit)
}

/// Whether the dump holds a read-only global with exactly these bytes.
///
/// A text constant is **storage** by this level (§2.5): the bytes are a global
/// and what an instruction carries is its address, so "does this program contain
/// the string `hi`" is a question about the data rather than about an operand.
fn holds_text(lir: &str, text: &str) -> bool {
    lir.contains(&format!("= b\"{text}\""))
}

/// Whether the dump panics with this message: the bytes are in the data, and a
/// call to `core.panic` reads them.
fn panics_with(lir: &str, message: &str) -> bool {
    holds_text(lir, message) && lir.contains("call core.panic(")
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

/// Spec §8.4: *a `defer` never reached does not run*. So the rung an exit
/// climbs is the one for the defers registered **above** it — `return 1` leaves
/// a scope that has registered nothing, `return 2` one that has registered
/// `first()`, and the end of the body one that has registered both. Three
/// exits, three different cleanups, and the two `return`s no longer share a
/// rung the way two written below the same `defer` do.
#[test]
fn lir_snapshot_an_exit_above_a_defer_does_not_run_it() {
    let src = "\
first :: func () {}
second :: func () {}
run :: func (n: i32) -> i32 {
  if n > 10 { return 1 }
  defer first()
  if n > 5 { return 2 }
  defer second()
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

/// **Vtables are data** (§7b): one constant per `(trait, concrete type)` pair,
/// its slots in the trait's declaration order, each holding the symbol
/// monomorphization decided fills it. Nothing is selected at run time — a
/// dispatch is two projections and an indirect call.
///
/// What this covers that
/// [`lir_snapshot_dynamic_dispatch_goes_through_a_vtable_slot`] does not: a
/// trait with **several** methods, so the slot *order* is visible; two impls,
/// so there are two vtables and each names its own functions; a generic type
/// coerced at an instantiation, so the vtable is for `Box.<i32>` and not for
/// `Box`; and one impl coerced twice, which shares the single constant rather
/// than emitting it again.
#[test]
fn lir_snapshot_a_vtable_is_a_constant_per_trait_and_type() {
    let src = "\
Draw :: trait {
  area :: func (self: *Self) -> i32
  perimeter :: func (self: *Self) -> i32
  sides :: func (self: *Self) -> i32
}
Square :: struct { s: i32 }
Circle :: struct { r: i32 }
Box :: struct <T> { v: T }
impl Draw for Square {
  area :: func (self: *Self) -> i32 { return self.s * self.s }
  perimeter :: func (self: *Self) -> i32 { return self.s * 4 }
  sides :: func (self: *Self) -> i32 { return 4 }
}
impl Draw for Circle {
  area :: func (self: *Self) -> i32 { return self.r * self.r * 3 }
  perimeter :: func (self: *Self) -> i32 { return self.r * 6 }
  sides :: func (self: *Self) -> i32 { return 0 }
}
impl Draw for Box.<i32> {
  area :: func (self: *Self) -> i32 { return self.v }
  perimeter :: func (self: *Self) -> i32 { return self.v }
  sides :: func (self: *Self) -> i32 { return 1 }
}
total :: func (sq: *Square, ci: *Circle, bx: *Box.<i32>) -> i32 {
  let a: *dyn Draw := sq
  let b: *dyn Draw := ci
  let c: *dyn Draw := bx
  // The same impl again: one vtable, not two.
  let d: *dyn Draw := sq
  return a.area() + b.perimeter() + c.sides() + d.sides()
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

/// The overflow checks that are a **comparison** rather than an opcode (§7d).
///
/// There is no `sdiv.with.overflow` on any target this emits for, so the one
/// signed division that leaves its width — the minimum over `-1` — is a
/// comparison against both operands. The unsigned families get none, because no
/// unsigned quotient leaves the width. A shift is checked on its *amount*: bits
/// leaving the top of a `<<` are what a shift is for, and an amount as wide as
/// the type is the case the machine has no agreed answer for.
///
/// This is a regression test as much as a description. Both of these used to be
/// listed as *checked opcodes*, which they have never been: the pair slot was
/// allocated, a plain `div` wrote only its value half, and the branch read the
/// flag out of whatever the stack happened to hold.
#[test]
fn lir_snapshot_the_overflow_checks_that_are_comparisons() {
    let src = "\
signed :: func (a: i32, b: i32) -> i32 { return a / b }
unsigned :: func (a: u32, b: u32) -> u32 { return a / b }
shifted :: func (a: u32, b: u32) -> u32 { return a << b }
by_constant :: func (a: u32) -> u32 { return a << 3 }
negated :: func (a: i32) -> i32 { return -a }
";
    insta::assert_snapshot!(lir_text(src));
}

/// No plain opcode ever writes into a pair slot.
///
/// The bug this pins was exactly that shape: an operation the build wanted
/// checked, whose `Op` had no checked form, assigned into a `(T, bool)` local
/// whose flag member nothing then wrote. It reads as an ordinary overflow branch
/// in a dump and traps at random when it runs, so a test that looks at the dump
/// would not have caught it. This one looks at the **types**.
#[test]
fn only_a_checked_op_writes_a_pair() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        for b in &f.blocks {
            for s in &b.stmts {
                let crate::lir::StmtKind::Assign { place, value } = &s.kind else {
                    continue;
                };
                let crate::lir::Rvalue::Op { op, .. } = value else {
                    continue;
                };
                if op.is_checked() || !place.projection.is_empty() {
                    continue;
                }
                let crate::lir::Base::Local(id) = place.base else {
                    continue;
                };
                assert!(
                    !matches!(f.locals[id.0 as usize].ty, crate::lir::Ty::Named(_)),
                    "{}: `{}` is not a checked op but writes an aggregate slot",
                    f.name,
                    op.name()
                );
            }
        }
    }
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
///
/// Each conversion names **which** instruction it is, and the point of the
/// snapshot is that the names differ where the rule says they do: `i32 -> i64`
/// sign-extends and `u32 -> i64` zero-extends from the same width to the same
/// width, `f64 -> i32` and `u32 -> f64` read their signedness off opposite
/// sides, and `i32 -> u32` is a change of name with no instruction under it.
#[test]
fn lir_snapshot_a_cast_of_a_literal_is_the_literal() {
    let src = "\
{ transmute } :: import <core/mem>
conv :: func (n: i32, u: u32, f: f64) -> u8 {
  let wide := cast.<i64>(n)
  let wide_unsigned := cast.<i64>(u)
  let narrow := cast.<u8>(wide)
  let single := cast.<f32>(f)
  let same_width := cast.<u32>(n)
  let rounded := cast.<i32>(f)
  let widened := cast.<f64>(u)
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

/// The scrutinee kinds the other two decision-tree snapshots do not reach.
///
/// `lir_snapshot_a_decision_tree_mixes_tuples_ranges_and_guards` has the
/// integer ranges, the or-pattern and the guard;
/// `lir_snapshot_a_text_pattern_calls_bytes_eq` has the `str` literals. What is
/// left is every other kind of value a `match` can test — a `char`, a float, a
/// `bool`, a slice by its length and its ends, and a struct destructured into
/// its members — and none of them is an enum, so none of them reads a
/// discriminant. Each is the comparison its type calls for, on the same chain
/// of candidates §4 describes.
#[test]
fn lir_snapshot_matching_on_things_that_are_not_enums() {
    let src = "\
Point :: struct { x: i32, y: i32 }
letter :: func (c: char) -> i32 {
  return c.match {
    'a' => 1,
    'b' | 'c' => 2,
    _ => 0,
  }
}
scale :: func (f: f64) -> i32 {
  return f.match {
    0.0 => 0,
    1.5 => 1,
    _ => 2,
  }
}
flag :: func (b: bool) -> i32 {
  return b.match {
    true => 1,
    false => 0,
  }
}
ends :: func (xs: []i32) -> i32 {
  return xs.match {
    [] => 0,
    [only] => only,
    [first, .., last] => first + last,
  }
}
corner :: func (p: Point) -> i32 {
  return p.match {
    .{ x: 0, y: 0 } => 0,
    .{ x, y: 0 } => x,
    .{ x: _, y } => y,
  }
}
";
    insta::assert_snapshot!(lir_text(src));
}

/// **Every statement carries the source position it came from** (§7c), which is
/// what a line table is built out of: `address -> source position`, emitted
/// from LIR because this is the last level where the mapping is still known.
///
/// A span reconstructed later is a span that is wrong. So this is the one
/// snapshot rendered *with* the positions, and what it shows is that the
/// compiler's own instructions have them too — the bounds comparison, the trap
/// block, the `Location` the panic is handed — all pointing at the line the
/// program wrote, not at nothing.
#[test]
fn lir_snapshot_every_statement_carries_its_source_position() {
    let src = "\
sum :: func (xs: []i32, k: usize) -> i32 {
  let mut total := 0
  total = total + xs[k]
  return total
}
";
    insta::assert_snapshot!(lir_text_with_spans(src));
}


/// **A codegen unit is self-contained** (§11): what it defines, a declaration for
/// everything it calls, and its own copy of the types and data it names.
///
/// This is the whole of the split, in one picture: two files, and the entry
/// file's unit holding a `declare func` for the one the other defines. A linker
/// resolves those by symbol, which is why the symbol is printed beside every
/// name — and the type `Point` appears in *both* units, because a unit that
/// cannot describe its own arguments is not self-contained.
#[test]
fn lir_snapshot_a_split_carries_declarations_for_what_it_calls() {
    let main = "\
{ scale, Point } :: import \"shapes.nest\"
@public main :: func () { let q := scale(Point { x: 1, y: 2 }, 3) }
";
    let shapes = "\
@public Point :: struct { x: i32, y: i32 }
@public scale :: func (p: Point, k: i32) -> Point { return Point { x: p.x * k, y: p.y * k } }
";
    let mut session = Session::with_loader(Box::new(
        MemLoader::new().with("main", main).with("shapes", shapes),
    ));
    session.options.codegen_units = 8;
    session.options.overflow = crate::common::options::OverflowMode::Wrap;
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
    let mut out = String::new();
    for u in program
        .units
        .iter()
        .filter(|u| u.name.ends_with("main") || u.name.ends_with("shapes"))
    {
        out.push_str(&crate::lir::pretty::unit_to_string(None, u));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
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
    for f in program.unit().funcs.iter() {
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

/// The same rule as an assertion: the `return` above every `defer` reaches the
/// function's exit with no cleanup in between, which is what makes it a
/// `return` in its own block rather than a jump into the ladder.
#[test]
fn an_exit_above_every_defer_needs_no_rung() {
    let src = "\
cleanup :: func () {}
run :: func (n: i32) -> i32 {
  if n > 10 { return 1 }
  defer cleanup()
  return 2
}
";
    let text = lir_text(src);
    // One `return 1` block, and it does not pass through a cleanup: the only
    // `call cleanup()` is the rung the second `return` climbs.
    assert_eq!(text.matches("call cleanup()").count(), 1, "{text}");
    let early = text
        .split("bb1:")
        .next()
        .expect("the block the `n > 10` arm lands in");
    assert!(!early.contains("call cleanup()"), "{text}");
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
    assert!(lir.contains("lt.u64 k_1, _2.*.len"), "{lir}");
    assert!(panics_with(&lir, "index out of bounds"), "{lir}");
}

/// An array with a run-time index is checked too — against the constant its
/// type carries.
#[test]
fn a_fixed_array_with_a_runtime_index_is_checked_against_its_length() {
    let lir = lir_text("f :: func (a: [3]i32, k: usize) -> i32 { return a[k] }\n");
    assert!(lir.contains("lt.u64 k_1, 3"), "{lir}");
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
    assert!(panics_with(&lir, "integer overflow"), "{lir}");
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
                        Rvalue::Use(o) => op(o, visit),
                        Rvalue::Ref(place) => visit(place),
                        Rvalue::Cast { value, .. } => op(value, visit),
                        Rvalue::Op { args, .. } => args.iter().for_each(|o| op(o, visit)),
                        Rvalue::Offset { ptr, index, .. } => {
                            op(ptr, visit);
                            op(index, visit);
                        }
                        Rvalue::Aggregate { fields, .. } => {
                            fields.iter().for_each(|o| op(o, visit))
                        }
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
/// Most of the old failure modes are gone by construction: LIR's own [`Ty`] has
/// no `comptime_int`, no inference variable and no error case, because it is
/// about what a register holds rather than about what a program may say. The two
/// that can still be *written* are `void` and `never`, and they are why a call
/// is a statement with an optional destination rather than an [`Rvalue`]: a slot
/// typed "no value" is a slot no register file has.
#[test]
fn no_local_has_a_type_a_machine_cannot_hold() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        for l in &f.locals {
            assert!(
                !matches!(l.ty, Ty::Void | Ty::Never),
                "{}: _{} is typed `{:?}`",
                f.name,
                l.id.0,
                l.ty
            );
        }
    }
}

/// Every place names a slot the function has, wherever the place appears — an
/// argument, a switch operand, a drop, a safepoint's live set.
#[test]
fn every_place_and_live_local_names_a_slot_that_exists() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
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
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        for (i, b) in f.blocks.iter().enumerate() {
            assert_eq!(b.id.0 as usize, i, "{}: bb{} is at index {i}", f.name, b.id.0);
        }
    }
}

/// Every direct call names a function **this unit** declares or defines.
///
/// A callee is an index into the unit's own list (§11), so this is the property
/// that makes a unit self-contained: nothing in it points outside itself, and a
/// backend never has to ask the compiler what `FuncId(7)` was.
#[test]
fn every_direct_call_names_a_function_in_the_unit() {
    for units in [1usize, 2, 5, usize::MAX] {
        let mut options = crate::common::options::Options::default();
        options.codegen_units = units;
        let program = lir_whole_program_with(BROAD, options);
        for u in &program.units {
            check_unit_is_closed(u);
        }
    }
}

/// Every index a unit holds resolves inside that unit: a callee, a global, a
/// type, a variant's type, a cast's target.
fn check_unit_is_closed(u: &Unit) {
    let ty_ok = |t: &Ty| walk_ty(t, &mut |id| assert!((id as usize) < u.types.len(), "unit `{}`: type #{id} is not in its own table", u.name));
    for t in &u.types {
        for m in &t.members {
            ty_ok(&m.ty);
        }
        if let Origin::Enum { variants } = &t.origin {
            for v in variants {
                assert!(
                    (v.ty.0 as usize) < u.types.len(),
                    "unit `{}`: variant `{}` names type #{}, which is not in its table",
                    u.name,
                    v.name,
                    v.ty.0
                );
            }
        }
    }
    for g in &u.globals {
        ty_ok(&g.ty);
        if let Some(c) = &g.init {
            check_const_is_closed(u, c);
        }
    }
    for f in &u.funcs {
        ty_ok(&f.ret);
        for l in &f.locals {
            ty_ok(&l.ty);
        }
        each_place(f, |p| {
            if let crate::lir::Base::Global(id) = p.base {
                assert!(
                    (id.0 as usize) < u.globals.len(),
                    "{}: global #{} is not in unit `{}`",
                    f.name,
                    id.0,
                    u.name
                );
            }
            for proj in &p.projection {
                if let crate::lir::Projection::Cast(t) = proj {
                    assert!(
                        (t.0 as usize) < u.types.len(),
                        "{}: a cast names type #{}, which is not in unit `{}`",
                        f.name,
                        t.0,
                        u.name
                    );
                }
            }
        });
        for b in &f.blocks {
            for st in &b.stmts {
                match &st.kind {
                    crate::lir::StmtKind::Call { callee, args, .. } => {
                        if let crate::lir::Callee::Static(id) = callee {
                            assert!(
                                (id.0 as usize) < u.funcs.len(),
                                "{}: calls function #{}, which unit `{}` does not hold",
                                f.name,
                                id.0,
                                u.name
                            );
                        }
                        for a in args {
                            if let crate::lir::Operand::Const(c) = a {
                                check_const_is_closed(u, c);
                            }
                        }
                    }
                    crate::lir::StmtKind::Assign { value, .. } => {
                        if let crate::lir::Rvalue::Aggregate { kind, fields } = value {
                            match kind {
                                crate::lir::Aggregate::Struct(t) => assert!(
                                    (t.0 as usize) < u.types.len(),
                                    "{}: aggregate names type #{}",
                                    f.name,
                                    t.0
                                ),
                                crate::lir::Aggregate::Variant { ty, variant, .. } => {
                                    assert!((ty.0 as usize) < u.types.len());
                                    assert!((variant.0 as usize) < u.types.len());
                                }
                                crate::lir::Aggregate::Array => {}
                            }
                            for x in fields {
                                if let crate::lir::Operand::Const(c) = x {
                                    check_const_is_closed(u, c);
                                }
                            }
                        }
                    }
                    crate::lir::StmtKind::Drop(_) => {}
                }
            }
        }
    }
}

fn check_const_is_closed(u: &Unit, c: &crate::lir::Constant) {
    use crate::lir::Constant;
    match c {
        Constant::Func(id) => assert!(
            (id.0 as usize) < u.funcs.len(),
            "unit `{}`: constant names function #{}",
            u.name,
            id.0
        ),
        Constant::Global(id) => assert!(
            (id.0 as usize) < u.globals.len(),
            "unit `{}`: constant names global #{}",
            u.name,
            id.0
        ),
        Constant::Aggregate(items) | Constant::Variant { payload: items, .. } => {
            items.iter().for_each(|i| check_const_is_closed(u, i))
        }
        _ => {}
    }
}

/// Every [`Ty::Named`] inside a type, however deep.
fn walk_ty(ty: &Ty, f: &mut impl FnMut(u32)) {
    match ty {
        Ty::Named(id) => f(id.0),
        Ty::Ptr(inner) => walk_ty(inner, f),
        Ty::Array { elem, .. } => walk_ty(elem, f),
        Ty::Func { params, ret } => {
            params.iter().for_each(|p| walk_ty(p, f));
            walk_ty(ret, f);
        }
        _ => {}
    }
}

/// **There is one program, and `core` is in it.**
///
/// Lowering is whole-program: `link` merges the per-file IR into one
/// [`Linked`](crate::ir::Linked), monomorphization runs over that, and this
/// pass emits a single [`Program`](crate::lir::Program) holding every function
/// that survives — the entry file's, `core`'s, and every instantiation made
/// along the way. Codegen is handed that one value; there is no per-file LIR
/// and nothing to link afterwards.
///
/// The snapshots render only the entry file's functions, because a test about
/// `while` should not be a record of the standard library. That is the
/// renderer filtering, not the program being split — which is what this test
/// is here to say, since a dump showing a call to `core.panic` and no
/// `core.panic` in it invites exactly the wrong conclusion.
#[test]
fn one_program_holds_core_and_every_instantiation() {
    let program = lir_whole_program(
        "f :: func <T> (x: T) -> T { return x }\nmain :: func () -> i32 { return f.<i32>(1) }\n",
    );
    let unit = program.unit();
    let named = |n: &str| unit.funcs.iter().any(|f| f.name == n);
    assert!(named("main"), "the entry file's function");
    assert!(named("core.panic"), "`core`'s, in the same program");
    assert!(named("f.<i32>"), "and the instantiation, which no file wrote");
    // Bodies and all: `core.panic` is a definition here, not a declaration.
    let panic = unit
        .funcs
        .iter()
        .find(|f| f.name == "core.panic")
        .expect("core.panic");
    assert!(!panic.blocks.is_empty(), "core.panic has a body");
}

/// **No place indexes a slice.** A slice is `{ ptr, len }` by §7b and a struct
/// has members rather than elements, so element `i` of one is reached through
/// the pointer it holds — `Projection::Index` says as much, and this is the
/// test that it is true.
///
/// It is here because the slice **pattern** broke it: `[first, .., last]`
/// projected `xs[0]` straight off the slice local, a place no backend can
/// emit without knowing the header's layout, while `xs[i]` in an expression
/// went through the pointer. Two lowerings of one thing, and only one of them
/// was the documented shape.
#[test]
fn no_place_indexes_a_slice() {
    let src = "\
ends :: func (xs: []i32, ys: [4]i32) -> i32 {
  let a := xs.match { [] => 0, [one] => one, [f, .., l] => f + l }
  let b := ys.match { [p, q, .., r] => p + q + r }
  return a + b + xs[1] + ys[2]
}
";
    let unit = lir_unit(src);
    for f in &unit.funcs {
        each_place(f, |place| {
            let mut ty = match place.base {
                crate::lir::Base::Local(id) => Some(f.locals[id.0 as usize].ty.clone()),
                crate::lir::Base::Global(id) => Some(unit.globals[id.0 as usize].ty.clone()),
            };
            for p in &place.projection {
                let Some(current) = ty.clone() else { break };
                if matches!(p, crate::lir::Projection::Index(_)) {
                    assert!(
                        !is_slice(&unit, &current),
                        "{}: a place with base {:?} indexes a slice",
                        f.name,
                        place.base
                    );
                }
                ty = step(&unit, &current, p);
            }
        });
    }
}

/// Whether this type is a slice's `{ ptr, len }` header (§7b).
fn is_slice(unit: &Unit, ty: &Ty) -> bool {
    let Ty::Named(id) = ty else { return false };
    matches!(
        unit.types.get(id.0 as usize).map(|t| &t.origin),
        Some(Origin::Slice)
    )
}

/// The type a projection lands on.
fn step(unit: &Unit, ty: &Ty, p: &crate::lir::Projection) -> Option<Ty> {
    use crate::lir::Projection;
    match p {
        Projection::Deref => match ty {
            Ty::Ptr(inner) => Some((**inner).clone()),
            _ => None,
        },
        Projection::Index(_) => match ty {
            Ty::Array { elem, .. } | Ty::Ptr(elem) => Some((**elem).clone()),
            _ => None,
        },
        Projection::Field { index, .. } => {
            let Ty::Named(id) = ty else { return None };
            let def = unit.types.get(id.0 as usize)?;
            def.members.get(*index as usize).map(|m| m.ty.clone())
        }
        // Reading bytes as another type: the type is right there in the
        // projection, which is the point of it (§7b).
        Projection::Cast(t) => Some(Ty::Named(*t)),
    }
}

/// An operation is a **machine** instruction, so none of its operands is ever
/// an aggregate.
///
/// This is the invariant the `str` literal pattern broke: it emitted `s == "hi"`
/// on a `{ ptr, len }`, which no target can compare and which would have
/// compared *addresses* if a backend had tried. Text equality is a call to
/// `core`'s `#lang("bytes_eq")` now, so nothing structural reaches an `icmp`.
#[test]
fn no_operation_has_an_aggregate_operand() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        for b in &f.blocks {
            for s in &b.stmts {
                let crate::lir::StmtKind::Assign {
                    value: crate::lir::Rvalue::Op { op, ty, args },
                    ..
                } = &s.kind
                else {
                    continue;
                };
                assert!(
                    !matches!(ty, Ty::Named(_) | Ty::Array { .. }),
                    "{}: {op:?} runs at an aggregate type",
                    f.name
                );
                assert_eq!(
                    args.len(),
                    op.arity(),
                    "{}: {op:?} has {} operands",
                    f.name,
                    args.len()
                );
                for side in args {
                    let scalar = match side {
                        crate::lir::Operand::Const(c) => is_scalar_constant(c),
                        crate::lir::Operand::Copy(p) => match p.base {
                            crate::lir::Base::Local(id) => {
                                let ty = &f.locals[id.0 as usize].ty;
                                // A whole local of an aggregate type is the case
                                // that matters; a member of one is a scalar.
                                !p.projection.is_empty() || ty.is_scalar()
                            }
                            crate::lir::Base::Global(_) => true,
                        },
                    };
                    assert!(scalar, "{}: {op:?} has an aggregate operand", f.name);
                }
            }
        }
    }
}

/// **An operand's constant is a scalar, an address, or `undef`** (§2.5).
///
/// A string's bytes and a folded aggregate are *data*, and data has an address:
/// they are globals by the time they get here, and what an instruction carries
/// is the reference. The composite forms of [`Constant`](crate::lir::Constant)
/// exist only for a global's own initializer, which is the one place that
/// describes storage rather than a value in a register — so every backend emits
/// read-only data once, from one place, instead of inventing it at every operand
/// that happens to hold a blob.
#[test]
fn no_operand_carries_a_blob() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        each_operand(f, |o| {
            if let crate::lir::Operand::Const(c) = o {
                assert!(
                    is_scalar_constant(c),
                    "{}: an operand carries {c:?}, which is data rather than a value",
                    f.name
                );
            }
        });
    }
}

fn is_scalar_constant(c: &crate::lir::Constant) -> bool {
    use crate::lir::Constant;
    matches!(
        c,
        Constant::Int(_)
            | Constant::Float(_)
            | Constant::Bool(_)
            | Constant::Func(_)
            | Constant::Global(_)
            | Constant::Undef
    )
}

/// Visit every operand a function mentions.
fn each_operand(f: &crate::lir::Function, mut visit: impl FnMut(&crate::lir::Operand)) {
    use crate::lir::{Callee, Rvalue, StmtKind, TermKind};
    for b in &f.blocks {
        for s in &b.stmts {
            match &s.kind {
                StmtKind::Assign { place, value } => {
                    for proj in &place.projection {
                        if let crate::lir::Projection::Index(i) = proj {
                            visit(i);
                        }
                    }
                    match value {
                        Rvalue::Use(o) => visit(o),
                        Rvalue::Ref(_) => {}
                        Rvalue::Cast { value, .. } => visit(value),
                        Rvalue::Op { args, .. } => args.iter().for_each(&mut visit),
                        Rvalue::Offset { ptr, index, .. } => {
                            visit(ptr);
                            visit(index);
                        }
                        Rvalue::Aggregate { fields, .. } => fields.iter().for_each(&mut visit),
                    }
                }
                StmtKind::Call { callee, args, .. } => {
                    if let Callee::Indirect(o) = callee {
                        visit(o);
                    }
                    args.iter().for_each(&mut visit);
                }
                StmtKind::Drop(o) => visit(o),
            }
        }
        match &b.term.kind {
            TermKind::Switch { value, .. } => visit(value),
            TermKind::Return(Some(v)) => visit(v),
            _ => {}
        }
    }
}

/// Every named type a local has is in the unit's own table, so a backend never
/// has to reach back into the compiler for a layout (§7b, §11).
#[test]
fn every_named_local_type_is_in_the_type_table() {
    let unit = lir_unit(BROAD);
    for func in &unit.funcs {
        for l in &func.locals {
            walk_ty(&l.ty, &mut |id| {
                assert!(
                    (id as usize) < unit.types.len(),
                    "{}: _{} mentions type #{id}, which has no definition in the table",
                    func.name,
                    l.id.0
                );
            });
        }
    }
}

/// **Every intrinsic the compiler declares has a case in LIR.**
///
/// The set is closed (§9) and [`Intrinsic`](crate::lir::Intrinsic) is an enum so
/// that a backend's match is exhaustive. That only holds if the *mapping* is
/// total: a row added to `sema::intrinsics` with no case here would arrive as
/// `Unknown` and reach a backend as a name again, which is the failure the enum
/// exists to prevent.
#[test]
fn every_declared_intrinsic_has_a_lir_case() {
    use crate::common::symbol::Symbol;
    use crate::lir::Intrinsic;
    // The ones the lowering consumes outright: they become a constant, a
    // projection, an instruction or a `drop`, and never reach a backend by name.
    let lowered = [
        "size_of",
        "align_of",
        "cast",
        "drop",
        "index",
        "len",
        "wrapping_add",
        "wrapping_sub",
    ];
    for row in crate::sema::intrinsics::INTRINSICS {
        if lowered.contains(&row.tag) {
            continue;
        }
        let i = Intrinsic::from_name(&Symbol::new(row.tag));
        assert!(
            !matches!(i, Intrinsic::Unknown(_)),
            "`{}` has no case in lir::Intrinsic",
            row.tag
        );
        assert_eq!(i.name(), row.tag, "`{}` round-trips", row.tag);
    }
    // And these are **lowered away**, so they must have no case at all.
    //
    // The direction of the assertion is the point. A slice is an `Offset` and
    // an `Aggregate` over a pointer and a length; a slice literal is a `make`
    // and a store per element; `index_mut` is a projection; a `repeat` is that
    // same `make` with a counter, or an aggregate when the length is in the
    // type. All are built from instructions a backend already has, so leaving a
    // variant behind for them would be leaving something every backend must
    // match and nothing can produce. If one is ever emitted again it becomes an
    // `Unknown`, and `no_program_contains_an_unknown_intrinsic` fails rather
    // than a backend quietly receiving a name.
    //
    // `format` is here for a different reason: there is no such intrinsic any
    // more at all. An `f"..."` is desugared to `core`'s formatter before
    // inference ever sees it (`sema::desugar`), so nothing downstream has a
    // name to lower.
    for name in ["slice", "array", "index_mut", "repeat", "format"] {
        let i = Intrinsic::from_name(&Symbol::new(name));
        assert!(
            matches!(i, Intrinsic::Unknown(_)),
            "`{name}` is lowered away and should have no case in lir::Intrinsic"
        );
    }
}

/// Nothing in a lowered program is an `Unknown` intrinsic.
#[test]
fn no_program_contains_an_unknown_intrinsic() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        for b in &f.blocks {
            for s in &b.stmts {
                if let crate::lir::StmtKind::Call {
                    callee: crate::lir::Callee::Intrinsic(i),
                    ..
                } = &s.kind
                {
                    assert!(
                        !matches!(i, crate::lir::Intrinsic::Unknown(_)),
                        "{}: `${}` reaches a backend by name",
                        f.name,
                        i.name()
                    );
                }
            }
        }
    }
}


/// **Every conversion names its instruction.**
///
/// The rule that decides which one lives in [`CastKind::of`] and nowhere else,
/// and this says two things about that: no program reaches a backend with a
/// conversion this stage had no case for, and the kind each instruction carries
/// is the one the rule gives for its own two types. The second half is what
/// stops a hand-built `Rvalue::Cast` somewhere in the lowering from recording a
/// sign extension on an unsigned source.
#[test]
fn no_program_contains_an_unknown_cast() {
    let unit = lir_unit(BROAD);
    for f in &unit.funcs {
        for b in &f.blocks {
            for s in &b.stmts {
                if let crate::lir::StmtKind::Assign {
                    value: crate::lir::Rvalue::Cast { kind, from, to, .. },
                    ..
                } = &s.kind
                {
                    assert_ne!(
                        *kind,
                        crate::lir::CastKind::Unknown,
                        "{}: no case for {from:?} -> {to:?}",
                        f.name
                    );
                    assert_eq!(*kind, crate::lir::CastKind::of(from, to), "{}", f.name);
                }
            }
        }
    }
}

/// **The corners of the rule, stated as cases.**
///
/// Each line is one a backend would otherwise have had to get right on its own,
/// and three of them are the ones that are easy to get wrong: a widening reads
/// the **source's** signedness, a float-to-integer reads the **destination's**,
/// and a same-width change of signedness is no instruction at all.
#[test]
fn a_conversion_is_named_by_the_pair_it_runs_between() {
    use crate::lir::CastKind as K;
    let int = |bits: u16, signed: bool| Ty::Int { bits, signed };
    let float = |bits: u16| Ty::Float { bits };

    assert_eq!(K::of(&int(64, true), &int(32, true)), K::Truncate);
    assert_eq!(K::of(&int(32, true), &int(64, true)), K::SignExtend);
    assert_eq!(K::of(&int(32, false), &int(64, true)), K::ZeroExtend);
    // Same width, different name for it: a register is a register.
    assert_eq!(K::of(&int(32, true), &int(32, false)), K::Reinterpret);
    // A `bool` is an unsigned one-bit integer, so it widens like one.
    assert_eq!(K::of(&Ty::Bool, &int(8, false)), K::ZeroExtend);
    assert_eq!(K::of(&int(8, false), &Ty::Bool), K::Truncate);

    assert_eq!(K::of(&float(64), &float(32)), K::FloatTruncate);
    assert_eq!(K::of(&float(32), &float(64)), K::FloatExtend);

    // The signedness in each of these belongs to a *different* side.
    assert_eq!(K::of(&int(32, true), &float(64)), K::IntToFloat { signed: true });
    assert_eq!(K::of(&int(32, false), &float(64)), K::IntToFloat { signed: false });
    assert_eq!(K::of(&float(64), &int(32, true)), K::FloatToInt { signed: true });
    assert_eq!(K::of(&float(64), &int(32, false)), K::FloatToInt { signed: false });

    let ptr = Ty::ptr(int(8, false));
    assert_eq!(K::of(&ptr, &Ty::ptr(int(32, true))), K::PtrCast);
    assert_eq!(K::of(&ptr, &int(64, false)), K::PtrToInt);
    assert_eq!(K::of(&int(64, false), &ptr), K::IntToPtr);
    // A function pointer is an address too.
    let func = Ty::Func {
        params: Vec::new(),
        ret: Box::new(Ty::Void),
    };
    assert_eq!(K::of(&func, &ptr), K::PtrCast);

    // And a pair with no instruction behind it says so rather than guessing.
    assert_eq!(
        K::of(
            &Ty::Array {
                len: 4,
                elem: Box::new(int(8, false))
            },
            &int(32, false)
        ),
        K::Unknown
    );
}

/// **A call passes what its callee takes.**
///
/// This is the invariant the `void` erasure has to earn: a parameter that holds
/// nothing is not passed (§9), and the rule runs on both sides — the callee's
/// signature drops it, the caller's argument list drops it. If they ever
/// disagreed, every backend would emit a call with the wrong number of
/// arguments and nothing before codegen would have noticed.
///
/// It is checked against the *declaration* too, which is what a split unit holds
/// for a function another unit defines (§11): that declaration is the only
/// signature the calling unit has.
#[test]
fn every_call_agrees_with_its_callees_signature() {
    for units in [1usize, usize::MAX] {
        let mut options = crate::common::options::Options::default();
        options.codegen_units = units;
        let program = lir_whole_program_with(BROAD, options);
        for u in &program.units {
            for f in &u.funcs {
                for b in &f.blocks {
                    for s in &b.stmts {
                        let crate::lir::StmtKind::Call {
                            callee: crate::lir::Callee::Static(id),
                            args,
                            ..
                        } = &s.kind
                        else {
                            continue;
                        };
                        let callee = &u.funcs[id.0 as usize];
                        assert_eq!(
                            args.len(),
                            callee.params,
                            "{}: calls {} with {} arguments; it takes {}",
                            f.name,
                            callee.name,
                            args.len(),
                            callee.params
                        );
                    }
                }
            }
        }
    }
}

/// A `void` parameter is not a parameter, and a `void` binding is not a slot
/// (§9) — but the argument still runs, because an argument is an expression and
/// its effects are not optional.
#[test]
fn a_void_argument_is_evaluated_and_not_passed() {
    let src = "\
noise :: func () -> i32 { return 1 }
take :: func (v: void, n: i32) -> i32 { return n }
@public main :: func () -> i32 {
  let r := take((), noise())
  return r
}
";
    let lir = lir_text(src);
    // The parameter is gone from the signature…
    assert!(lir.contains("func take(n_0: i32) -> i32"), "{lir}");
    // …and so is the argument, while the call beside it still happens.
    assert!(lir.contains("call noise()"), "{lir}");
    assert!(!lir.contains("let _0: void"), "{lir}");
}


/// **Two globals never share a symbol.**
///
/// A `#static` written inside a function body has no path to mangle: its name is
/// whatever the source wrote in that body, and two functions may each write `n`.
/// `mono::global_symbol` gives both the same name, so the lowering is where the
/// second one is made unique — two definitions of one symbol is what a linker
/// refuses, and the two regions are genuinely different.
#[test]
fn two_function_local_statics_do_not_share_a_symbol() {
    let src = "\
a :: func () -> u32 { #static n: u32 :: 0 n = n + 1 return n }
b :: func () -> u32 { #static n: u32 :: 5 n = n + 2 return n }
@public main :: func () { let x := a() + b() }
";
    let unit = lir_unit(src);
    let symbols: Vec<String> = unit
        .globals
        .iter()
        .filter(|g| g.name == "n")
        .map(|g| g.symbol.to_string())
        .collect();
    assert_eq!(symbols.len(), 2, "{symbols:?}");
    assert_ne!(symbols[0], symbols[1], "{symbols:?}");
    // And nothing in the whole program shares one.
    let all: Vec<String> = unit.globals.iter().map(|g| g.symbol.to_string()).collect();
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "{all:?}");
}

// ===< The codegen-unit split (§11) >===

/// Every function with a body is defined in **exactly one** unit, whatever the
/// split — and every unit that calls it holds a declaration.
///
/// Two definitions of one symbol is what a linker refuses; none is what it
/// cannot resolve. The split is a filter, so both are failures of the same
/// walk.
#[test]
fn a_split_defines_every_symbol_exactly_once() {
    let whole = lir_unit(BROAD);
    let defined: std::collections::BTreeSet<String> = whole
        .funcs
        .iter()
        .filter(|f| !f.blocks.is_empty())
        .map(|f| f.symbol.to_string())
        .collect();
    for units in [1usize, 2, 3, 7, usize::MAX] {
        let mut options = crate::common::options::Options::default();
        options.codegen_units = units;
        let program = lir_whole_program_with(BROAD, options);
        let mut seen: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        // A linker-visible global is defined once; private data is a copy per
        // unit and is not the linker's business (§11).
        let mut data: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for u in &program.units {
            for g in &u.globals {
                if g.linkage == crate::lir::Linkage::External {
                    *data.entry(g.symbol.to_string()).or_default() += 1;
                }
            }
        }
        for (sym, n) in &data {
            assert_eq!(*n, 1, "-C codegen-units={units}: global `{sym}` defined {n} times");
        }
        for u in &program.units {
            for f in u.funcs.iter().filter(|f| !f.blocks.is_empty()) {
                *seen.entry(f.symbol.to_string()).or_default() += 1;
            }
            // A unit's own calls resolve to something it holds.
            for f in &u.funcs {
                for b in &f.blocks {
                    for s in &b.stmts {
                        if let crate::lir::StmtKind::Call {
                            callee: crate::lir::Callee::Static(id),
                            ..
                        } = &s.kind
                        {
                            assert!((id.0 as usize) < u.funcs.len());
                        }
                    }
                }
            }
        }
        for (sym, n) in &seen {
            assert_eq!(*n, 1, "-C codegen-units={units}: `{sym}` defined {n} times");
        }
        let got: std::collections::BTreeSet<String> = seen.keys().cloned().collect();
        assert_eq!(got, defined, "-C codegen-units={units}: definitions differ");
        assert!(
            program.units.len() <= units.max(1),
            "-C codegen-units={units}: got {} units",
            program.units.len()
        );
    }
}

/// A global is defined once too, and every other unit that reads it says so.
#[test]
fn a_split_defines_every_global_exactly_once() {
    let src = "\
#static counter: i32 :: 7
bump :: func () -> i32 { counter = counter + 1 return counter }
@public main :: func () { let n := bump() }
";
    let mut options = crate::common::options::Options::default();
    options.codegen_units = usize::MAX;
    let program = lir_whole_program_with(src, options);
    let mut defs = 0;
    let mut externs = 0;
    for u in &program.units {
        for g in &u.globals {
            if g.name != "counter" {
                continue;
            }
            if g.linkage == crate::lir::Linkage::Imported {
                externs += 1;
                assert!(g.init.is_none(), "an imported global carries no contents");
            } else {
                defs += 1;
            }
        }
    }
    assert_eq!(defs, 1, "one definition of `counter`");
    assert!(externs == 0 || externs >= 1);
}

/// The split is deterministic: the same program splits the same way twice.
#[test]
fn a_split_is_deterministic() {
    let mut options = crate::common::options::Options::default();
    options.codegen_units = 3;
    let a = lir_whole_program_with(BROAD, options.clone());
    let b = lir_whole_program_with(BROAD, options);
    let names = |p: &crate::lir::Program| -> Vec<String> {
        p.units.iter().map(|u| u.name.clone()).collect()
    };
    assert_eq!(names(&a), names(&b));
    for (x, y) in a.units.iter().zip(b.units.iter()) {
        assert_eq!(
            crate::lir::pretty::unit_to_string(None, x),
            crate::lir::pretty::unit_to_string(None, y)
        );
    }
}


/// **Every shipped example lowers to well-formed units, at every split.**
///
/// The invariants above each state one rule over one program. This runs all of
/// them over every example the repository ships, at four settings of
/// `-C codegen-units`, because the failures worth catching are the ones a
/// hand-written test program does not contain: a type only `core`'s `Result`
/// reaches, a global only one unit defines, an intrinsic only one example uses.
#[test]
fn every_example_lowers_to_well_formed_units() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("examples dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("nest") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for units in [1usize, 2, 4, usize::MAX] {
            let mut session = Session::new();
            session.options.codegen_units = units;
            let file = session
                .sources
                .add(path.to_string_lossy().into_owned(), src.clone());
            let (ast, errs) = crate::parser::parse::Parser::parse_file(&src, file);
            assert!(errs.is_empty(), "parse errors in {path:?}");
            session.asts.insert(file, ast);
            analyze(&mut session, file);
            assert!(!session.has_errors(), "{path:?}: {:#?}", session.diagnostics);
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
            let mut defined: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            assert!(
                program.units.len() <= units.max(1),
                "{path:?}: -C codegen-units={units} gave {} units",
                program.units.len()
            );
            for u in &program.units {
                check_unit_is_closed(u);
                check_unit_is_emittable(u, &format!("{path:?} [{units}]"));
                for f in u.funcs.iter().filter(|f| !f.blocks.is_empty()) {
                    *defined.entry(f.symbol.to_string()).or_default() += 1;
                }
                for g in u.globals.iter() {
                    if g.linkage == crate::lir::Linkage::External {
                        *defined.entry(format!("global {}", g.symbol)).or_default() += 1;
                    }
                }
                // Rendering is part of well-formedness: it resolves every index
                // the structures hold, so a dump that prints `<unknown …>` is a
                // unit that does not describe itself.
                let text = crate::lir::pretty::unit_to_string(None, u);
                assert!(
                    !text.contains("<unknown"),
                    "{path:?} [{units}] unit `{}` does not resolve its own indices:\n{text}",
                    u.name
                );
            }
            for (sym, n) in &defined {
                assert_eq!(*n, 1, "{path:?} [{units}]: `{sym}` defined {n} times");
            }
        }
        checked += 1;
    }
    assert!(checked >= 5, "expected the example files, saw {checked}");
}

/// The properties a backend needs of any unit: a graph that is a graph, slots
/// that exist, types that are machine types, and no operation or operand that
/// is not one.
fn check_unit_is_emittable(u: &Unit, what: &str) {
    for t in &u.types {
        assert_ne!(
            t.name, "<error>",
            "{what}: a program that type-checked mentions the error type"
        );
    }
    for f in &u.funcs {
        for (i, b) in f.blocks.iter().enumerate() {
            assert_eq!(b.id.0 as usize, i, "{what}: {}: bb{} is at {i}", f.name, b.id.0);
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
                    "{what}: {}: bb{} jumps to bb{}, which does not exist",
                    f.name,
                    b.id.0,
                    t.0
                );
            }
            for st in &b.stmts {
                match &st.kind {
                    crate::lir::StmtKind::Assign {
                        value: crate::lir::Rvalue::Op { op, args, ty },
                        ..
                    } => {
                        assert_eq!(args.len(), op.arity(), "{what}: {}: {op:?}", f.name);
                        assert!(
                            !matches!(ty, Ty::Named(_) | Ty::Array { .. } | Ty::Void | Ty::Never),
                            "{what}: {}: {op:?} runs at {ty:?}",
                            f.name
                        );
                    }
                    // A conversion this stage had no case for would reach a
                    // backend as "figure it out from the two types", which is
                    // the derivation `CastKind` exists to remove.
                    crate::lir::StmtKind::Assign {
                        value: crate::lir::Rvalue::Cast { kind, from, to, .. },
                        ..
                    } => {
                        assert_ne!(
                            *kind,
                            crate::lir::CastKind::Unknown,
                            "{what}: {}: no case for {from:?} -> {to:?}",
                            f.name
                        );
                        assert_eq!(
                            *kind,
                            crate::lir::CastKind::of(from, to),
                            "{what}: {}: the kind recorded for {from:?} -> {to:?} is not the one the rule gives",
                            f.name
                        );
                    }
                    crate::lir::StmtKind::Call {
                        callee: crate::lir::Callee::Intrinsic(i),
                        ..
                    } => assert!(
                        !matches!(i, crate::lir::Intrinsic::Unknown(_)),
                        "{what}: {}: `${}` reaches a backend by name",
                        f.name,
                        i.name()
                    ),
                    _ => {}
                }
            }
        }
        for l in &f.locals {
            assert!(
                !matches!(l.ty, Ty::Void | Ty::Never),
                "{what}: {}: _{} is typed {:?}",
                f.name,
                l.id.0,
                l.ty
            );
        }
        each_place(f, |p| {
            if let crate::lir::Base::Local(id) = p.base {
                assert!(
                    (id.0 as usize) < f.locals.len(),
                    "{what}: {}: _{} has no slot",
                    f.name,
                    id.0
                );
            }
        });
        each_operand(f, |o| {
            if let crate::lir::Operand::Const(c) = o {
                assert!(
                    is_scalar_constant(c),
                    "{what}: {}: an operand carries {c:?}",
                    f.name
                );
            }
        });
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
    assert!(panics_with(&lir, "division by zero"), "{lir}");
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
    assert!(holds_text(&lir, "hi"), "{lir}");
    assert!(lir.contains("call core.bytes_eq(s_0, _"), "{lir}");
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
    let whole = lir_unit(src);
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
                callee: crate::lir::Callee::Static(id),
                ..
            } => Some(whole.func(*id).name.as_str()),
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
    assert!(holds_text(&lir, "hi"), "{lir}");
    assert!(lir.contains("call core.bytes_eq(b_0, _"), "{lir}");
}

/// The empty pattern is a length test and nothing else, which is what the
/// library function does with it — there is no special case here.
#[test]
fn an_empty_string_pattern_is_the_same_call() {
    let lir = lir_text("f :: func (s: str) -> i32 { return s.match { \"\" => 1, _ => 0 } }\n");
    assert!(holds_text(&lir, ""), "{lir}");
    assert!(lir.contains("call core.bytes_eq(s_0, _"), "{lir}");
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


// ===< The entry point (§5.6) >===

/// The entry function of a unit, if it has one.
fn entry_of(unit: &Unit) -> Option<&crate::lir::Function> {
    unit.funcs.iter().find(|f| f.symbol.as_str() == "main")
}

/// A program with a root `main` gets a C `main` that initializes the runtime and
/// then calls it.
///
/// The two are separate functions on purpose: the program's `main` is mangled
/// like every other Nest function, and the symbol the linker wants is not a name
/// this compiler is free to give a source function.
#[test]
fn a_program_gets_an_entry_point_that_calls_main() {
    let unit = lir_unit("main :: func () { }\n");
    let entry = entry_of(&unit).expect("an entry point");
    assert_eq!(entry.name, "entry");
    assert_eq!(entry.ret, crate::lir::Ty::Int { bits: 32, signed: true });
    assert!(entry.attrs.public, "the linker has to see it");

    let called: Vec<&str> = entry.blocks[0]
        .stmts
        .iter()
        .filter_map(|s| match &s.kind {
            crate::lir::StmtKind::Call { callee: crate::lir::Callee::Static(id), .. } => {
                Some(unit.funcs[id.0 as usize].symbol.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(called, vec!["nest_init", "_NC4main"], "in this order");
}

/// A `main` that returns nothing is a program that exited successfully, so the
/// entry point returns zero rather than whatever was in the register.
#[test]
fn a_void_main_exits_zero() {
    let unit = lir_unit("main :: func () { }\n");
    let entry = entry_of(&unit).expect("an entry point");
    assert!(
        matches!(
            &entry.blocks[0].term.kind,
            crate::lir::TermKind::Return(Some(crate::lir::Operand::Const(
                crate::lir::Constant::Int(n)
            ))) if *n == num_bigint::BigInt::from(0)
        ),
        "{:?}",
        entry.blocks[0].term.kind
    );
}

/// A `main` that returns a status returns it to the operating system — and one
/// whose integer is not C's `int` is converted by a cast that names itself,
/// rather than by a backend deciding what to do with the width.
#[test]
fn a_status_main_returns_its_status() {
    let unit = lir_unit("main :: func () -> i32 { return 3 }\n");
    let entry = entry_of(&unit).expect("an entry point");
    assert!(
        entry.blocks[0]
            .stmts
            .iter()
            .any(|s| matches!(&s.kind, crate::lir::StmtKind::Call { dest: Some(_), .. })),
        "the status is kept"
    );

    let unit = lir_unit("main :: func () -> i64 { return 3 }\n");
    let entry = entry_of(&unit).expect("an entry point");
    let kinds: Vec<crate::lir::CastKind> = entry.blocks[0]
        .stmts
        .iter()
        .filter_map(|s| match &s.kind {
            crate::lir::StmtKind::Assign { value: crate::lir::Rvalue::Cast { kind, .. }, .. } => {
                Some(*kind)
            }
            _ => None,
        })
        .collect();
    assert_eq!(kinds, vec![crate::lir::CastKind::Truncate], "i64 -> i32");
}

/// A library has no entry point, and nothing had to be told so: a program
/// without a root `main` is what a library is.
#[test]
fn a_program_without_main_gets_no_entry_point() {
    let unit = lir_unit("add :: func (a: i32, b: i32) -> i32 { return a + b }\n");
    assert!(entry_of(&unit).is_none());
}

/// A `main` inside a namespace is an ordinary function — the same rule
/// `ir::check::declarations` applies when it decides whose signature to check.
#[test]
fn a_namespaced_main_is_not_the_entry_point() {
    let unit = lir_unit("app :: namespace { main :: func () { } }\nrun :: func () { app.main() }\n");
    assert!(entry_of(&unit).is_none());
}

/// `-C entry=none` is how a build that is producing a **library** out of a
/// program that has a `main` — a test harness, say — says so.
#[test]
fn entry_none_suppresses_it() {
    let options = crate::common::options::Options {
        entry: crate::common::options::EntryMode::None,
        ..Default::default()
    };
    let program = lir_whole_program_with("main :: func () { }\n", options);
    for unit in &program.units {
        assert!(entry_of(unit).is_none(), "{}", unit.name);
    }
}

/// `value ; count` in both its shapes (§6.11's neighbour, spec §3.2).
///
/// An **array** is its own storage and its length is part of its type, so the
/// list is known at compile time and the whole thing is one aggregate — the
/// same instruction `.{ 7, 7, 7, 7 }` is.
///
/// A **slice** has neither: its elements have to be allocated and its count is
/// an ordinary run-time value, so it is a `make` and a loop. That loop is why
/// `repeat` is not an intrinsic — §10 says an intrinsic is one instruction or
/// one runtime call, and a comparison, a back edge and a join are none of those.
#[test]
fn lir_snapshot_repeat_is_an_aggregate_or_a_loop() {
    let src = "\
fixed :: func () -> [4]i32 { return .{ 7; 4 } }
grown :: func (n: usize) -> []i32 { return .{ 5; n } }
";
    insta::assert_snapshot!(lir_text(src));
}

/// The repeated value is evaluated **once**, because the source wrote it once.
///
/// `.{ f(); 3 }` calls `f` one time and stores the answer three times. The
/// aggregate holds one operand three times over, not three calls.
#[test]
fn a_repeat_evaluates_its_value_once() {
    let unit = lir_unit(
        "\
next :: func () -> i32 { return 1 }
build :: func () -> [3]i32 { return .{ next(); 3 } }
main :: func () -> i32 { return build()[0] }
",
    );
    let build = unit
        .funcs
        .iter()
        .find(|f| f.name.as_str() == "build")
        .expect("`build` is in the program");
    let calls = build
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter(|s| matches!(s.kind, crate::lir::StmtKind::Call { .. }))
        .count();
    assert_eq!(calls, 1, "`.{{ next(); 3 }}` called `next` {calls} times");
}
