# Handoff: Phase 8 is done — LIR, plus the four phase-7 audit fixes

**Generated**: 2026-09-12
**Branch**: `main`
**Status**: **438 tests pass**, `cargo clippy` reports 90 warnings (the 73
baseline plus 17 dead-code entries in the new `lir` module — fields codegen will
read and nothing does yet). Every file in `examples/*.nest` compiles.

**Read `design/roadmap.md` §8 first**, then `design/lir.md`. This document is the
*state*. **Phase 9 (LIR: drops and safepoints) is next and is unblocked.**

## What is built (this session)

### The four audit fixes, committed first (`588e605`)

- [x] **Layout arithmetic is checked**, and a type the target cannot address is
      a **diagnostic** rather than an overflow. `LayoutError::TooLarge`, reported
      by the stamping pass — the one layout failure no earlier check owns. The
      ceiling is `isize::MAX` on the target, and `design/lir.md` §7cc states the
      rule and why it is `isize` and not `usize`.
- [x] **A bad array length in a signature is reported once.**
      `stamp_generics` re-resolved the signature purely to collect the generic
      parameters it mentions, and resolving a type *reports*. It now reads back
      what inference already recorded (`Inferer::inferred_sig_ty`).
- [x] **A type alias's right-hand side is checked at its declaration**, and
      reported once however often the alias is used. Two halves: a declaration
      pass that expands every alias the file declares, and `ConstSlotReported`,
      which makes every diagnostic in the `const_*` family at most once per node
      — a type node is resolved once per *use*, not once.
- [x] **`size_of` of a type already in error adds no second diagnostic.**
      `ConstError::reported` is the channel; `Ty::mentions_error` is the test.

### Phase 8 — LIR

`nestc/src/lir/mod.rs` is the representation, `lower.rs` the IR → LIR walk,
`pretty.rs` the dump `design/lir.md` §1 writes. `main.rs` prints it as
`===< LIR >===`.

- [x] **Basic blocks, terminators, places** (§1). Locals up front; `goto`,
      `switch`, `return`, `unreachable` and nothing else.
- [x] **`match` as a decision tree** (§4) — the discriminant read **once**.
- [x] **`defer` placed on every exit path** (§3), as ordinary blocks on a
      cleanup ladder, one rung per *kind* of exit.
- [x] **Aggregates flattened to structs** (§7b); arrays are not.
- [x] **Vtables as constants**, with the slot filling recorded by
      monomorphization (`mono::VtableSlots`).
- [x] **`overflow=trap` as an edge** (§7d), through `distinct` integers too.
- [x] **Intrinsics gone as calls** (§9).
- [x] **Everything §7c lists carried**: a span per statement, a source name per
      local, both names per function.
- [x] **12 snapshot tests and 2 well-formedness tests.**

## The four load-bearing decisions

### `defer` came with this phase, not with phase 9

The roadmap put §3 in phase 9 with drops and safepoints. It is here instead,
because the alternative was a control-flow lowering that silently discarded
`defer` bodies — a wrong program that looks like a right one. Drops and
safepoints stay in phase 9 and attach to the ladder this phase builds, which is
the part of §3 they actually needed.

The rungs are shared per **kind** of exit and not per exit *site*: three
`return`s in one scope enter one rung. They cannot be shared across kinds,
because what follows differs — a `return` continues to the function's exit, a
`break` only to the loop's. That is also why `return` writes a slot instead of
returning directly: the real `return` happens after the ladder, and a rung shared
by three of them cannot tell which value it is carrying.

### No phi, and no block-level input list

Both were tried and both were removed on the user's call. A local is a **slot**,
addressable, and reading one written in another block is an ordinary read. The
value of that is that everything downstream — drops, safepoints, codegen — sees
one mechanism rather than two, and a merge needs no special form. What a merging
expression (`if`, `match`, `&&`) does is write its branches' results into one
slot and read it after the join.

### Monomorphization owns a vtable's slots, not the LIR lowering

`mono::VtableSlots` is stamped on the `*T` → `*dyn Trait` coercion — the only
place in the program where the trait and the concrete type are written down
together. Re-selecting the impl in `lir` would be a second implementation of a
selection free to disagree with the first, and the instantiated method that fills
a slot **does not exist** until monomorphization makes it.

### Pattern types are threaded down, never read off the pattern node

A pattern is matched *against* a type; its own node carries whatever inference
left there, which for a variant payload element is nothing. The first version
read `meta.ty(pattern.id)` and produced `checked_mul(undef, undef)` — the
bindings were silently skipped. `test_pattern` and `bind_irrefutable` now take
the type of the value at the place and derive each sub-pattern's from it.

## Failed approaches (don't repeat these)

Everything in the previous handoffs' lists still stands. New this session:

- **A `phi` instruction, LLVM style.** Built, then removed: with locals as slots
  it is a second mechanism for something the first already does, and a `break`
  or `return` that leaves through a ladder cannot be a phi operand anyway — the
  ladder is shared, so the predecessor cannot tell the exits apart.
- **`Block::inputs`, a per-block list of locals read from elsewhere.** Also
  built, also removed. It is derived data that can drift, and every consumer that
  wants it (liveness, root maps) computes it in the form it actually needs.
- **A `Cleanup` structure holding unplaced defer blocks.** The first attempt
  lowered defer bodies into blocks nothing jumped to and recorded them beside the
  function. It is not a representation of the program; the ladder is.
- **Reading a pattern's type off its own node.** See above.
- **Re-testing the discriminant inside a group.** The first grouped `match`
  emitted the switch *and* then let each arm test the variant again — three extra
  blocks per variant, for a question with a known answer. `chain` takes the
  already-selected variant and tests only the payload.
- **One "nothing matched" block per variant group.** They are all the same block.
- **`Layouts::substitution` left private.** LIR needs a member's type *as a use
  site sees it*, so `member_types` / `variant_member_types` are public now.
  Substituting is layout's job and the answer has to be reachable from outside.
- **`ty.is_int()` as the overflow test.** `usize` is `distinct uint.<PTR_BITS>`
  since §3.1, so the commonest integer a program writes said no.
- **Lowercasing a whole rendered instruction** to spell `Mul` as `mul`. It
  lowercases the operands too, and an operand may be a string.

## Key decisions

| Decision | Rationale |
|---|---|
| A local is a slot; no phi, no block inputs | One mechanism, and a ladder-shared exit could not be a phi operand anyway |
| `defer` is lowered here, not in phase 9 | A CFG that drops `defer` bodies is not a CFG of the program |
| A ladder rung per *kind* of exit | §3's requirement is once-per-body, not once-per-site; the continuation is what differs |
| `return` writes a slot | The real `return` is after the ladder, and the rung is shared |
| The discriminant is read once, and a group does not re-test it | §4's stated invariant, and three blocks per variant otherwise |
| A guarded catch-all sends the match down the linear route | Its failure means a different next arm in each group |
| A vtable's slots are monomorphization's answer | The instantiated method does not exist until that pass makes it |
| A call is an instruction, not a terminator | A panic does not unwind (§2) — the whole reason the CFG stays the size of the source |
| Pattern types are threaded down | A pattern is matched *against* a type; its node carries none |
| `overflow=trap` is an edge in the graph | Every later pass has to see it to be correct (§7d) |
| Arrays do not flatten | §7b's four reasons: a value index, a length in the type, size, and a GC run |
| The type table is the closure of what LIR uses | "Every type" is not enumerable — the same reason layout is a query |
| `TooLarge` is the one layout failure the stamping pass reports | Every other one is already somebody's diagnostic |

## Current state

**Working**: everything. `cd nestc && cargo test` → **438 passed**. `cargo
clippy` → 90 warnings. Every file in `examples/*.nest` compiles clean.

**Broken**: nothing.

**Uncommitted changes**: none.

## `core`'s `.len()` was calling itself — fixed

`packages/core/slice.nest` used to write

```nest
@public len :: #intrinsic("len") func <T> (x: T) -> usize
impl <T> []T {
  len :: #inline func (self: *Self) -> usize { return len(self) }
}
```

and the inner `len` resolved to the **impl's own member**, not to the
namespace's intrinsic, so every `s.len()` on a slice was infinite recursion. The
LIR is what made it plain — `_1 := call core.<impl []T>.<i32>.len(self_0)` inside
`len` itself — and it dated from phase 3, when `len` became an intrinsic.

The fix is not to qualify the call (nothing in the language spells the
namespace from inside its own file). It is that **the method is the intrinsic**:

```nest
impl <T> []T {
  len :: #intrinsic("len") func (self: *Self) -> usize
}
```

There was never a body to write. `#intrinsic` means the compiler supplies one
(§6.4), and a forwarding call was only ever a way of naming which one. The
result is better in three ways: the recursion is gone, a fixed array's `.len()`
now folds to its literal length at the call site (it could not before — the fold
happens in `lower_intrinsic`, and the method call was not one), and a slice's is
a single member read in LIR (`_7.*.len`) rather than a call.

The **resolution** question behind it is still open and is worth its own look: an
impl member shadows the enclosing namespace for a bare call, which is not what
Rust does and is a trap anywhere else it comes up. Nothing depends on it now.

## Deliberately not done

- **Drops (§5) and GC safepoints (§6).** Phase 9. Both attach to the ladder §3
  now builds.
- **`#soa`.** Still warned about rather than consumed. It now has what it was
  waiting for — a place projection is `Projection::Index` over an array — so this
  is a real task rather than a blocked one: a `#soa` `[N]Particle` needs a column
  per field and an `&a[i]` that no longer names a contiguous `Particle`.
- **A slice literal's storage.** `[]T { a, b }` reaches LIR as the composite
  intrinsic; where the elements live is an allocation question.
- **`a[i]` on a sequence is still compiler syntax.** `core/ops.nest` already
  declares `Index` / `IndexMut` with `#lang` tags, spec §6.13 already says `a[i]`
  is `Index.index(&a, i).*`, and a *user* type already goes that route — the two
  built-in sequences are the anomaly, special-cased in inference and lowering.
  The `len` change above is the pattern for closing it: an `index` /
  `index_mut` intrinsic row, `impl <T> Index.<usize> for []T` in core with the
  member marked `#intrinsic`, and the special case deleted. It is a front-end
  change and wants its own commit. LIR needs nothing: `$index` is a `Ref` to the
  place `index_into` already builds.
- **ABI classification**, dead-code elimination, cross-compilation-unit generics,
  moving `+`/`-` out of `sema::builtins`: all unchanged.
- **`Ty` in LIR.** Locals are typed with `sema::ty::Ty`, which is concrete by
  here; there is no separate `LirTy`. The flattened definitions in
  `Program::types` are what a backend reads for contents.

## Files to know

| File | Why it matters |
|---|---|
| `design/lir.md` | The specification. §5 and §6 are what is left. |
| `design/roadmap.md` §8, §9 | §8 is what was built; §9 is next. |
| `nestc/src/lir/mod.rs` | `Program`, `Function`, `Block`, `Place`, `Rvalue`, `Terminator`, `TypeDef`. |
| `nestc/src/lir/lower.rs` | The walk. The scope/ladder machinery is `push_scope` / `ladder` / `rung`; `match` is `lower_match` / `chain` / `test_pattern`. |
| `nestc/src/lir/pretty.rs` | The dump. `:=` introduces, `=` stores. |
| `nestc/src/ir/mono.rs` | `Instance` (names), `VtableSlots` (slot filling), `type_key`. |
| `nestc/src/ir/layout.rs` | `of` / `fields` / `enum_layout` / `member_types` / `max_size`. |
| `nestc/src/sema/tests.rs` | `lir_text`, the 12 `lir_snapshot_*` tests, and the two well-formedness ones. |

## Code context

```
func run(n_0: i32) -> i32  // _NC3run
  let _1: i32
  let _4: i32
bb0:                    // entry
  _1 := cast 10 : comptime_int -> i32
  _2 := n_0 > _1
  switch _2 { 1 => bb1, _ => bb2 }
bb1:                    // then
  _4 := 1
  goto bb5
bb4:                    // return
  return _4
bb5:                    // defer 0 (return)
  call cleanup()
  goto bb4
```

```rust
// lir/mod.rs — the whole instruction set
pub enum StmtKind {
    Assign { place: Place, value: Rvalue },
    Call { dest: Option<Place>, callee: Callee, args: Vec<Operand> },
}
pub enum TermKind {
    Goto(BlockId),
    Switch { value: Operand, arms: Vec<(i128, BlockId)>, otherwise: BlockId },
    Return(Option<Operand>),
    Unreachable,
}
pub enum Projection { Field { index, name }, Index(Operand), Deref, Variant { index, name } }
```

**The non-obvious bits.**

*The `Lowerer` holds `&'c ir::Function` beside `&mut Cx`.* Both come out of the
same `Linked`, and the function reference is a copy of a shared one — that is
what lets the walk read the body while mutating the program being built.

*A block whose terminator is already set silently drops later statements.*
That is `push`'s job, and it is what makes "everything after a `return` in the
same straight run" disappear without the walk having to track it.

*`self.at` is moved around freely and always restored.* `rung` and
`return_block` both build a block elsewhere and put `self.at` back; forgetting to
is how statements end up in the wrong block.

*The type table is built **last**, in `Cx::collect_types`, from the locals the
functions ended up with.* Adding a type to a local after that point would not
reach the table.

## Resume instructions

1. `cd nestc && cargo test` — expect **438 passed**.
2. See the phase working:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
   Shape :: enum { dot, circle(i32), rect { w: i32, h: i32 } }
   cleanup :: func () {}
   area :: func (s: Shape) -> i32 {
     defer cleanup()
     return s.match { .dot => 0, .circle(r) => r, .rect { w, h } => w * h }
   }
   @public main :: func () { let x := area(.circle(3)) }
   EOF
   ./target/debug/nestc /tmp/e.nest | sed -n '/===< LIR/,$p'
   ```
   Expect: one `discriminant`, one `switch` over three variants, a
   `defer 0 (return)` block every exit goes through, and `checked_mul` with an
   `overflow` edge.
3. **Phase 9 (LIR: drops and safepoints) is next.** `design/lir.md` §5 and §6.
   Both attach to the ladder: an allocation that does not outlive its scope is a
   `drop` on the same rungs, and a safepoint carries the live pointers at a call.
   §6's `reloc` discipline is the part with a real decision in it.
4. The natural follow-up, if you want a small one first: close the `a[i]`
   special case the same way `.len()` was closed — see **Deliberately not done**.
5. Whatever you touch, verify with all three:
   - `cargo test` (438 and rising)
   - `for f in ../examples/*.nest; do ./target/debug/nestc "$f" >/dev/null || echo "FAIL $f"; done`
   - `cargo clippy` — compare the warning **set**, not the count.

## Edge cases and known limits

Everything in the previous handoff's list still holds. New or changed:

- **A `distinct` is a one-member struct in LIR**, so `usize` prints as
  `struct { 0: u64 }`. That is §2.4's rule made literal, and it is why the
  overflow check has to look through one.
- **An enum's payload is `[N]u8`.** The variants' real member types are on the
  definition, not in the payload member: one variant is live at a time and the
  bytes are shared, so there is no single type the member could have.
- **The checked-arithmetic pair is a `(T, bool)` tuple**, which means it shows up
  in the type table as an ordinary flattened tuple. That is correct — it is a
  real value a real local holds — and it is why `overflow=wrap` produces a
  smaller table.
- **A block with no terminator after the walk is unreachable**, and is sealed as
  `unreachable`. The well-formedness test is what catches an edge to one that was
  allocated and never filled.
- **Spans are off in the snapshots** (`lir_text` passes `None` for the source
  map) so that a snapshot is not a record of line numbers in `tests.rs`. The
  driver passes them.
- **Only the entry file's functions are snapshotted.** `core` is linked into
  every program and its lowering is not what any of those tests is about.

## Warnings

Everything in the previous handoff's Warnings section still applies — run
everything from `nestc/`, `cargo test` does not rebuild the binary, compare the
clippy warning *set*, `touch` before re-running clippy, never `cargo fmt` with no
arguments, regenerate snapshots with `INSTA_UPDATE=always cargo test` then
`rm -f src/*/snapshots/*.snap.new` and `sed -i '' '/^assertion_line: /d'`, many
tests assert *exactly one* diagnostic, a new intrinsic is a row in
`sema/intrinsics.rs` first, `#lang` discovery is by tag only.

New:

- **`src/lir/` is not `rustfmt`-clean by accident.** It was written formatted;
  if you reformat, pass the file explicitly, never `cargo fmt` bare.
- **The LIR dump is printed by the driver for every file**, so a panic in
  lowering shows up as a failure of the examples loop. That is deliberate: the
  loop is the only place the whole of `core` gets lowered.

## User notes

- Commits are **title-only**: no body, no co-author trailer, changes bundled as
  `feat: a + feat: b + fix: c`.
- The design moves. Commit green states often so a redesign costs one commit,
  not a session.
- When a note in `design/roadmap.md` is departed from, say so *there* and in the
  reply — `defer` landing in phase 8 is this session's worked example.
