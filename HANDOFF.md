# Handoff: Phase 9 is done, and LIR has been audited against a real backend

**Generated**: 2026-09-12
**Branch**: `main`
**Status**: **521 tests pass**, `cargo clippy` reports 83 warnings (the same
dead-code-shaped set as before — fields codegen will read and nothing does yet).
Every file in `examples/*.nest` compiles.

**If you are picking this up to do the next piece of work, read
"Next: LIR still has special cases that should be ordinary data" below first** —
it is a planned, unimplemented change list, and item A (vtables become ordinary
globals) is what the user asked for most recently.

**Read `design/roadmap.md` §9 first**, then `design/lir.md` §5, §6 and **§10**.
§10 is new and is phase 10's brief: the whole instruction set a backend answers
for, the four things it does itself, and the one place LIR still reaches back
into the compiler. This document is the *state*. **Phase 10 (codegen and the
real driver) is next and is unblocked.**

## What is built (this session)

### Four things the user asked for before phase 9

All four are about LIR being small enough that a C, LLVM or wasm backend has a
short list of things to answer for.

- [x] **No `comptime_int` in LIR.** `_5 := cast 10 : comptime_int -> i32` is now
      `_5 := 10`. The `$cast` out of a literal is bookkeeping about where the
      literal's type came from (§6.5) and inference is the only stage that read
      it; `comptime_int` is a type no backend has a register for, so the cast
      folds into the definition. It is the const evaluator that folds it — the
      same one `check::bounds` asks — so a literal cannot mean one thing here and
      another there.
- [x] **No `discriminant` operation.** An enum is `{ tag, payload }` by §7b, so
      `_2 := s_0.tag` is an ordinary member read. §4's "read once" is a property
      of the decision tree, not of the instruction set. `Rvalue::Discriminant` is
      gone.
- [x] **No `$panic`.** `panic` is an ordinary function in `core` found by
      `#lang("panic")`, and a trapped overflow or an out-of-bounds index is
      lowered to a **call** to it with a `Location` built from the failing
      operation's own span. The one intrinsic left is `trap`, which is a machine
      instruction (`ud2`, `brk`, `unreachable`).
- [x] **The panic handler is `core`'s and the program may replace it.**
      `#lang("panic_handler")`, defaulting to `trap()`, because `core` has no I/O
      and cannot know whether the target has a console.

Decision trees were the fourth thing asked about; they were already built, in
phase 8 (§4).

### Phase 9 — drops and safepoints

`nestc/src/lir/escape.rs` and `nestc/src/lir/safepoint.rs` are new.

- [x] **Escape analysis** (§5), intra-procedural and deliberately blunt.
- [x] **`drop` on the cleanup ladder**, after the `defer` bodies on the same
      rung. A new `StmtKind::Drop(Operand)`.
- [x] **Safepoints** (§6) at a call, an allocation and a loop back edge, each
      carrying the roots live **before** the statement.
- [x] **Real liveness** — backward dataflow to a fixed point over the locals
      whose type can hold a reference.
- [x] **12 new tests** and a 13th snapshot.

### `drop`, written by hand

`drop(p)` is an intrinsic in `core/mem.nest` now (spec §6.9), lowering to the
**same** `StmtKind::Drop` the escape analysis emits. Writing it takes the
question on:

- escape analysis stops answering for that value — passing a local to anything
  disqualifies it, and a `drop` is a call, so the object is freed once;
- `ir/check/dropped.rs` refuses a later use of the name, a second drop, and a
  drop — inside a loop — of something declared outside it.

### The audit: six invariant tests, four real defects

The question was whether a backend can walk LIR and emit code without
re-deriving anything. Six tests in `sema::tests` now assert the shape rather than
describe it (no local has a type a machine cannot hold, every place names a slot,
block ids are dense, every direct call names a function the program defines, no
`Binary` has an aggregate operand, every named type a local mentions is in the
table). Writing them found four things:

- [x] **`let _2: never`.** `$trap()` was an `Rvalue` assigned to a slot no
      machine has. Intrinsics are `StmtKind::Intrinsic { dest: Option<Place> }`
      now, mirroring `Call` — `void` and `never` locals are gone from the whole
      program.
- [x] **`s.match { "hi" => ... }` compared pointers.** A `str` is `{ ptr, len }`
      by LIR, and the pattern emitted `Rvalue::Binary { Eq }` on it — not an
      instruction any target has, and if a backend had tried, two copies of
      `"hi"` in different buffers would not have matched. It is a call to
      `core`'s `#lang("bytes_eq")` now.
- [x] **`str` had no `Eq` impl**, so `a == b` was refused while a pattern
      silently miscompiled. `core` implements it, over the same `bytes_eq`.
- [x] **Dividing by zero was undefined.** It is not overflow, so `overflow=wrap`
      has nothing to say about it; an integer `/` or `%` now traps
      unconditionally, and only `#unsafe` removes the check.

## The load-bearing decisions

### A `#lang` tag `core` claims is a **default**

The override rule is general, not a panic-handler special case: a tag may be
claimed once inside `core` and once outside, the outside claim wins, and two
claims from the same side stay the duplicate error they were. `LangItems` stores
`(DefId, from_core)`, and `from_core` is `pkg_of[file] == "core"` — the same
question `impls::check_coherence` already asks.

The reason is that "the compiler finds what it needs by tag, never by name" only
holds up if a tag can be *re-answered*. `core` is by definition the fallback
library; a tag it claims that nothing may override is a name coupling wearing a
tag's clothes.

### The compiler's panics call `core.panic`, not the handler directly

Both would have to build a `Location`, so it is not about cost. It is that there
should be one place in a program where a panic happens: a program that replaces
the handler replaces what an overflow does too, and a compiler-private abort
beside a library panic would be two ways for a program to die, reported
differently.

### Escape analysis runs on the IR tree, not on the CFG

A scope is **lexical**, and once control flow is a graph there are no scopes left
to ask about — the ladder is what remains of them. Asking the tree costs a walk
instead of a dataflow, and the answer arrives before the ladder is built, so
`push_scope` registers a drop the way it registers a `defer` and the machinery
phase 8 already has places it. Doing it on the CFG would mean rediscovering the
scopes the ladder was built from.

### A whitelist, not a blacklist

A candidate survives only if **every** mention of it is the base of a place being
read or written (`p.*.x`, `p.*`, `p.*.x = 1`). Anything else disqualifies it,
including forms that would be provably fine. A whitelist that is wrong leaks; a
blacklist that is wrong corrupts memory.

### Text equality lives in `core`, not in the compiler

The first fix for the `str` pattern unrolled the bytes in LIR — a length test and
one comparison per character. It works and it is low-level, and it is still
wrong: it puts a second definition of "are these bytes equal" in the compiler,
free to disagree with the one `impl Eq for str` uses. The lowering emits a call
to `#lang("bytes_eq")` instead, found by tag the way `panic` is, and the `Eq`
impl calls the same function. A pattern and an `==` cannot disagree because there
is one of them.

### A safepoint's `live` is one list, not two

It is the root set the collector traces **and** the `reloc` redefinitions,
because with a moving collector every root holds a different address afterwards.
`live: [p]` beside `p := reloc p` is the same fact written twice, and two copies
of a fact are two things that can disagree. The dump prints the `reloc` lines
because a redefinition is what the list *means*.

### The live set is the one **before** the statement

Collection happens while the statement is running — inside the callee, inside the
allocator — and at that moment the destination has not been written. The first
version used the live-out set and produced `_0 := $new() @safepoint { live: [_0] }`,
which asks the collector to relocate whatever the slot happened to hold.

## Failed approaches (don't repeat these)

Everything in the previous handoffs' lists still stands. New this session:

- **`#lang("panic")` written on the line above the binding.** `#lang("prelude")`
  is written that way on an `import` and works; on a `func` it does not. The tag
  is read off the *value*'s directives (`collect::lang_tag`), so it has to be
  `panic :: #lang("panic") func ...`. The symptom was a function whose IR header
  printed the directive while `lang_items` had no entry.
- **`place_reads` after `kill` on an assignment.** A write to a whole local is
  not a read of it; calling both put the destination straight back into the live
  set and propagated a bogus root around every loop. `write()` now either kills
  (whole local) or reads (projected), never both.
- **`drop p` treated as killing `p`** in the backward walk. A drop **reads** the
  pointer it frees; removing it there made the allocation untraceable across
  every safepoint before it.
- **`nominal_holds_pointer` answering "yes" for every named type.** The
  flattened type table (`Program::types`) is built by then and says exactly which
  members a type has, so the question has a real answer. An enum needs the
  *variants'* member types: its flattened payload is `[N]u8` and says nothing.
- **A separate `reloc` statement per root.** It is the live set written a second
  time. See above.
- **A `Safepoint` block of its own.** The association between "this call" and
  "the collection that may happen inside it" is what an LLVM statepoint needs,
  and a separate block loses it. It is a field on `Stmt` and on `Terminator`.
- **Unrolling a `str` pattern's bytes in LIR.** Built, then removed on the
  user's call: it is a second definition of byte equality inside the compiler.
  The comparison belongs in `core`, where `impl Eq for str` can share it.
- **`nominal_holds_pointer` answering "yes" for every named type.** Listed
  before; worth repeating beside the invariant test that would now catch it.
- **Merging a diverging branch's dropped state.** `if c { drop(p) return 0 }`
  then `p.*.x` is correct, and the first `check::dropped` refused it. A branch
  that leaves has no "afterwards".
- **Filling a ladder rung at scope-pop time** so a late `let` could join it. It
  is the most delicate machinery in the lowering and the payoff is one
  disqualified allocation. The ordering rule in `lir::escape` handles it instead:
  a candidate declared after an exit in the same scope is not a candidate.

## Key decisions

| Decision | Rationale |
|---|---|
| A `#lang` tag from `core` is a default an outside claim overrides | "Found by tag, never by name" needs a tag to be re-answerable |
| The compiler's failures call `core.panic` | One place where a panic happens; the handler override covers them too |
| `trap` stays an intrinsic | Stopping the processor is a machine instruction and no library can write it |
| A comptime cast folds into the literal | `comptime_int` is a type no backend has a register for |
| The discriminant is a member read | An enum *is* `{ tag, payload }`; "read once" is the tree's property |
| Escape analysis on the IR tree | A scope is lexical; the CFG has none left |
| Drops after defers on the same rung | A `defer` may still read the object |
| A whitelist of allowed uses | Wrong whitelist leaks; wrong blacklist corrupts |
| `live` is the root set *and* the relocs | One fact, one place |
| The live set is the one before the statement | The destination is not written when the collector runs |
| Back edges by DFS, not dominators | Same answer on the graphs this lowering produces, twenty lines |
| A root is any type that can *contain* a reference | The frame slot holding a struct with a pointer is where that pointer lives |
| An intrinsic is a statement with an optional destination | A slot typed `void` or `never` is a slot no machine has |
| Text equality is a call to `core`, found by tag | One definition of it, shared by the pattern and by `Eq` |
| `drop`'s operand is always a pointer | An instruction whose operand is sometimes a struct is one a backend must switch on |
| A written `drop` disqualifies the automatic one | Not a special case — passing a local to anything does |
| Dividing by zero traps whatever `overflow=` says | `wrap` says what `MAX + 1` *means*; `x / 0` has no meaning to give |
| A diverging branch does not merge its dropped state | There is no "afterwards" on a path that returns |

## Current state

**Working**: everything. `cd nestc && cargo test` → **521 passed**. `cargo
clippy` → 83 warnings. Every file in `examples/*.nest` compiles clean.

**Broken**: nothing.

**Uncommitted changes**: none.

### The LIR tests moved, and `distinct` stopped being a struct

`nestc/src/lir/tests.rs` + `nestc/src/lir/snapshots/` — the IR → LIR tests used
to sit in `sema::tests`, which tests AST → IR. They test a different pass and
now live with it. `sema::tests` is `pub(crate)` so the few helpers they share
(`analyze_mem`, `ir_text`, `messages`) still resolve.

**A `distinct T` is now `T` at LIR**, not a one-member struct wrapping it. The
old shape put `type usize = struct { 0: u64 }` in the type table and typed
every `usize` local as that struct — and a struct of one scalar is not passed
like the scalar under any C ABI, so it was a distinction that cost something at
the FFI boundary and meant nothing anywhere. `Cx::strip` peels the chain (and
goes through pointers, slices, arrays and tuples, but not into a nominal's
generic arguments), applied where a type is read from the IR and where the type
table's members are built. Three now-dead distinct paths went with it —
`byte_slice`'s hop, `is_integer`'s recursion, and `Origin::Distinct`.

### The LIR snapshot suite (this session)

Fourteen more `insta` snapshots, in the same shape as the IR and parser ones:
source in, the whole lowered program out. They cover what the earlier fifteen
did not — monomorphization, a bound resolved at the call rather than through a
vtable, `#static` versus `::`, a text pattern reaching `core.bytes_eq`, the
division-by-zero check, `#unsafe`, casts, the four rungs a loop's ladder needs,
an operator that is a call, an `extern` declaration, array constants,
safepoints on a back edge, an allocation that escapes, and a decision tree over
tuples, ranges, an or-pattern and a guard. `design/lir.md` §10 ends with what
the two kinds of test (snapshots, invariants) each guard.

Writing them found three defects, all fixed:

| Defect | Fix |
|---|---|
| `TABLE[1]` on a `::` array constant lowered to `undef` — indexing takes `&TABLE` and a constant has no address, so `place_of` returned `None` and the caller made it undefined | `place_of` materializes a constant into a slot, the same path `(a + b).x` takes |
| `cast.<u16>(7)` stayed a run-time cast between two constants, because the fold was gated on the *source* still being `comptime_int` | Any cast the evaluator can perform is folded |
| `#unsafe` was documented as removing "bounds, init, null" checks but also removes the division-by-zero one | `spec/09` says so, and says the overflow trap is `overflow=`'s decision and stays |

### Added after the suite (same session)

- **A vtable snapshot about the data**, not the dispatch: a three-method trait
  (slot order), two impls (two constants), an impl for `Box.<i32>` (the vtable
  is the instantiation's), and one type coerced twice (one constant, shared).
- **A `match` snapshot for scrutinees that are not enums** — `char`, float,
  `bool`, slice patterns, a struct destructured by member.
- **A snapshot rendered with spans**, since §7c's line table is built from the
  position on every statement and nothing was checking they were there.
- **`one_program_holds_core_and_every_instantiation`** — the snapshots render
  only the entry file's functions, which makes a call to `core.panic` with no
  `core.panic` under it look like a missing definition. It is the renderer
  filtering; the program is one value with `core` in it.
- **fix**: a slice pattern projected `xs[0]` off the header, an `Index` on a
  slice, which `Projection::Index` says never happens. It goes through the
  pointer now, like `xs[i]` always did, and `no_place_indexes_a_slice` type-
  walks every projection so it cannot come back.
- **fix (sema, not LIR)**: `Self` in `impl Trait for Box.<i32>` was the head
  `Box.<?T>`, so a member whose body never mentions `self` was refused with
  "type annotations needed" on its own parameter. Collection binds `Self` to
  the whole target expression now, in a namespace belonging to the impl block.

## Next: LIR still has special cases that should be ordinary data

**Planned, not implemented. This section is the brief.**

The standing requirement, from the user, in their words: *LIR should be as
simple as possible without being platform-specific (aside from pointer size).*
A vtable is the clearest violation — it is a struct of function pointers and
nothing else, and LIR gives it a table of its own, an id type of its own, a
constant form of its own and a line of its own in the dump. Every one of those
is a thing a backend has to learn that it already knows how to do.

What follows is that change and the others of its kind, each with the shape to
move to, the obstacle in the way, and what it costs to leave alone. **A is the
one that was asked for.** B–D are the same mistake in other places. E–G are
cheaper and independent. H is the big one and goes last.

### A. A vtable is a global, not a table beside the program

**Now**: `Program::vtables: Vec<Vtable>`, `VtableId`, `Vtable { trait_def,
concrete, symbol, slots: Vec<Option<VtableSlot>> }`, `Constant::Vtable(id)`,
`AggregateKind::Dyn`, and a dump line `vtable _NV… for Dog as Speak { [0] say =
… }`. A dispatch reads `s.vtable.*.say` — a `Field` projection on a `*void`, so
the offset it means cannot be derived from the place's type; the backend has to
know that slot *n* of a vtable is at `n * pointer_size`.

**Wanted**: one struct type per **trait**, one immutable global per **impl**,
and no other machinery.

```
type VT.Draw = struct {             // one per trait, slots in declaration order
  area:      *func(*void) -> i32    // +0
  perimeter: *func(*void) -> i32    // +8
  sides:     *func(*void) -> i32    // +16
}
global vt.Square.as.Draw: VT.Draw = { Square.area, Square.perimeter, Square.sides }
type *dyn Draw = struct { data: *void, vtable: *VT.Draw }
```

Then `dyn(p, &vtable#0)` is `Aggregate` of the `*dyn Draw` struct over two
operands, the second being the address of a global; and `s.vtable.*.area` is a
`Field` on a real struct type whose offset is in the type table like every
other. Nothing about vtables remains in the instruction set.

**What is in the way**, in the order it has to be cleared:

1. **`Global::init` is `Option<ConstValue>`**, and a function address is not a
   `ConstValue` — it is `Constant::Func`. So `Constant` needs an aggregate case
   and `Global` needs to hold a `Constant`:
   ```rust
   pub enum Constant {
       Value(ConstValue),
       Func { def, name, symbol },
       Aggregate(Vec<Constant>),   // new
       Address(DefId),             // new: the address of a global
       Undef,
   }
   pub struct Global { …, pub init: Option<Constant>, pub mutable: bool }
   ```
   `mutable` is new because today only `#static`s are emitted and immutability
   is implied by there being nothing else. After this there is something else.
2. **A vtable global needs a `DefId`**, since `Base::Global` and the proposed
   `Constant::Address` are keyed by one and a vtable was never written in any
   source. Monomorphization already allocates synthetic defs for the
   instantiations it makes (`mono::run` takes `&mut DefTable` for exactly
   this); do the same here, with the `_NV…` mangling as the symbol so the name
   the linker sees does not change.
3. **The vtable pointer's type must be the trait's, not the impl's.** A `dyn`
   has erased the concrete type, so `*dyn Draw`'s second member is
   `*VT.Draw` — the per-trait struct — and each impl's global is a value of
   that one type. This is what makes the dispatch an ordinary field read; a
   per-impl vtable type would put the backend back to computing `n *
   pointer_size` by hand.
4. **`Ty` has no function-pointer form** worth checking before starting: if
   `Ty::Func` cannot be spelled as a member type here, the slots can stay
   `*void` and the change still pays for itself — the offsets come from the
   struct either way. Do not let this block the rest.

**Also delete**: `Origin::Dyn` can stay as debug metadata (it says what the
struct *was*, which is what `Origin` is for), but `AggregateKind::Dyn` should
go — see D.

**Tests that move**: `lir_snapshot_a_vtable_is_a_constant_per_trait_and_type`,
`lir_snapshot_dynamic_dispatch_goes_through_a_vtable_slot`, and any snapshot
with a `dyn(` in it. The claims they make do not change; the rendering does.

### B. A constant that needs storage should be a global too

`b"yes"` appears as an inline operand (`Constant::Value(ConstValue::Str(…))`),
and so does an array constant (`{ 1, 2, 3 }`). Neither is a value a machine
holds in a register, so every backend has to synthesize a read-only global and
a reference to it — the same work, done three times, differently.

Once A has given LIR immutable globals with constant initializers, the rule to
enforce is: **an operand is a scalar or an address, never a blob.** Materialize
string, byte-string and aggregate constants as globals at lowering, and hand
out `Constant::Address`. An invariant test can then say it, which is worth more
than the paragraph in the design doc.

### C. There are three ways to write arithmetic

`Rvalue::Binary { op, lhs, rhs }`, `Rvalue::Unary { op, operand }` and
`Rvalue::Builtin { op, args, checked }` overlap: the last can express both
others, and `checked` is a flag that changes the *result type* (to `(T, bool)`)
rather than the operation. Three shapes, one idea.

**Wanted**: one `Rvalue::Op { op: Op, args: Vec<Operand> }`, where checked
arithmetic is its own opcode (`AddChecked`, `SubChecked`, …) rather than a
flag. A backend then matches one enum once, and the arity is the opcode's own
business. Keep the operand count validated by an invariant test rather than by
the type.

### D. `AggregateKind` says what the type table already says

`Struct(DefId)`, `Tuple`, `Slice`, `Dyn` are four names for "build a value of
this struct type", and the type is already on the destination place. Collapse
them to one case carrying the type. Keep **`Array`**, because an array does not
flatten (§7b), and keep **`Variant`**, because it is the one aggregate whose
construction needs a value the fields do not carry — the tag.

### E. `Offset` carries a `Ty` where everything else carries a number

Every size in LIR is a number: `TypeDef::layout`, every `TypeMember::offset`.
`Rvalue::Offset { ptr, index, elem: Ty }` is the exception, and it sends the
backend back through the layout engine for a stride the compiler has already
computed. Make it `stride: u64`. This is the pointer-size caveat the
requirement allows, and it is already how the rest of the type table works.

### F. A field's name and its type's member names should agree

`checked_add` yields a `(i32, bool)` whose `TypeDef` members are named `0` and
`1`, and the projections that read it are `.0` and **`.overflowed`** — a name
the type does not have. The index is authoritative so nothing miscompiles, but
a dump that names a member which is not in the type is a trap for the next
reader. Either name the tuple's members `0`/`1` at the projection, or give the
checked result a real `TypeDef` with `{ value, overflowed }`. The second is
better and costs one type per width.

### G. `StmtKind::Intrinsic` is keyed by a `Symbol`

The set that survives to LIR is closed — `new`, `make`, `transmute`, `trap`,
`assert`, `wrapping_add`, `wrapping_sub`, `gc_collect`, `gc_keep_alive`,
`gc_pin`, and `embed_file` if it reaches here at all (check). A `Symbol` makes
a backend's match non-exhaustive, so a name added upstream is a silent
fall-through instead of a compile error. Make it an enum. Confirm the list by
walking `sema::intrinsics` against what `lir::lower` handles before this pass.

### H. `Program` is not standalone: `Ty::Nominal` needs the `DefTable`

Already named in `design/lir.md` §10 and still true. Every local's type is a
`sema::Ty`, and resolving a `Ty::Nominal` to its definition means
`mono::type_key(&DefTable, ty)` — the one place LIR reaches back into the
compiler. Replacing the nominal case with a `TypeId(u32)` index into
`Program::types` makes the program serializable and a backend an independent
consumer.

It is last because it touches every file in `lir/` and every test that spells a
type. Doing A–G first means doing it once, over the simpler shape.

### Ordering, and what each one costs to skip

| | Change | Depends on | Cost of leaving it |
|---|---|---|---|
| A | Vtables become globals | — | Four concepts a backend must learn; a field offset it must compute by hand |
| B | Blob constants become globals | A's `Constant::Address` | Every backend re-implements read-only data emission |
| C | One arithmetic rvalue | — | Three shapes for one idea |
| D | `AggregateKind` collapses | — | Four names for one operation |
| E | `Offset` carries a stride | — | The backend re-runs layout |
| F | Member names agree with the type | — | A dump that lies quietly |
| G | Intrinsics are an enum | — | A non-exhaustive match in every backend |
| H | `TypeId` instead of `Ty::Nominal` | A–G, ideally | `Program` cannot leave the process |

### How to work on any of these

The snapshots are the safety net and the review surface both: make the change,
run `cargo insta test`, and **read every diff before accepting it** — that is
where a shape regression shows up as a shape regression rather than as a
backend bug six months later. The invariant tests in `nestc/src/lir/tests.rs`
under `// ===< LIR well-formedness >===` are the other half; each of A, B, D
and F wants one more of them, stating the rule the change establishes.

Update `design/lir.md` §1, §7b and §10 in the same commit as the code. §10 is
the instruction-set table a backend reads; a change that does not land there
did not happen.

## Deliberately not done

- **Per-function escape summaries.** A change of *precision* (§5); the drop
  machinery does not move.
- **`Drop`, the trait.** The per-type form of cleanup is unbuilt. §5's drops are
  the compiler's own, for memory it proved local.
- **The object-start table** interior pointers need (§6). The collector's to
  build, and there is no collector.
- **Narrowing what counts as a root.** A `*T` is a root whatever `T` is, so a
  vtable pointer (`*void` by this level) is counted. It wants a distinction
  between a managed reference and a machine address that the type system does not
  draw.
- **`#soa`**, a slice literal's storage, ABI classification, dead-code
  elimination, cross-compilation-unit generics: all unchanged from the last
  handoff.
- **The resolution question from last session** — an impl member shadowing the
  enclosing namespace for a bare call — is still open and still depends on
  nothing.

## Files to know

| File | Why it matters |
|---|---|
| `design/lir.md` | The specification. §5 and §6 are now built; §1's instruction set is the contract a backend answers for. |
| `design/roadmap.md` §9, §10 | §9 is what was built; §10 is next. |
| `nestc/src/lir/mod.rs` | `Stmt` (with `safepoint`), `StmtKind::Drop`, `Safepoint`, `Terminator`. |
| `nestc/src/lir/escape.rs` | The escape analysis. `analyze` → block id → the locals that get a drop. |
| `nestc/src/ir/check/dropped.rs` | Use after a written `drop`, dropped twice, dropped in a loop. |
| `packages/core/slice.nest` | `bytes_eq`, the `#lang` item a text pattern calls. |
| `packages/core/mem.nest` | `new`, `make`, `drop`. |
| `nestc/src/lir/safepoint.rs` | Liveness, back edges, and what counts as a root. |
| `nestc/src/lir/lower.rs` | `panic_at` / `location` / `tag_of`; `push_scope` / `rung` / `emit_drops`. |
| `nestc/src/sema/def.rs` | `LangItems`, and the core-is-a-default rule. |
| `packages/core/fail.nest` | `panic`, `panic_handler`, `trap`. |

## Code context

```
func f(c_0: bool) -> i32  // _NC1f
  let _1: *mut Node
  let p_3: *mut Node
bb0:                    // entry
  _1 := $new()
    @safepoint { live: [] }
  p_3 := _1
  switch c_0 { 1 => bb1, _ => bb2 }
bb3:                    // join
  call report(p_3.*.x)
    @safepoint { live: [p_3]
      p_3 := reloc p_3
    }
bb5:                    // cleanup 0 (return)
  call report(0)
    @safepoint { live: [p_3]
      p_3 := reloc p_3
    }
  drop p_3
  goto bb4
```

```rust
// lir/mod.rs — the whole instruction set (see design/lir.md §10)
pub enum StmtKind {
    Assign { place: Place, value: Rvalue },
    Call { dest: Option<Place>, callee: Callee, args: Vec<Operand> },
    Intrinsic { dest: Option<Place>, name: Symbol, args: Vec<Operand> },
    Drop(Operand),                             // the operand is always a pointer
}
pub struct Stmt { kind: StmtKind, span: Option<FileSpan>, safepoint: Option<Safepoint> }
pub struct Safepoint { live: Vec<LocalId> }    // the roots *and* the relocs
```

**The non-obvious bits.**

*`lir::lower` now takes `&LangItems` and `&SourceMap`.* The first is for the one
tag LIR looks up (`panic`, plus `location` to build its argument); the second is
for turning a span into the file/line/column that argument holds.

*The safepoint pass runs last, after `collect_types`.* It needs the flattened
type table to answer whether a named type holds a reference, and that table is
built from the locals the functions ended up with.

*A rung is labelled `cleanup N (kind)`, not `defer N (kind)`.* It carries drops
now as well as defer bodies.

*`Stmt::new` and `Terminator::new` exist* so the two new `safepoint` fields did
not have to be written at seventy literal sites.

## Resume instructions

1. `cd nestc && cargo test` — expect **521 passed**.
2. See the phase working:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
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
   @public main :: func () { let a := f(true) }
   EOF
   ./target/debug/nestc /tmp/e.nest | sed -n '/===< LIR/,$p'
   ```
   Expect: `drop p_3` on the `cleanup 0 (return)` rung after the `defer` body, a
   `@safepoint` on each call with `p_3` live and relocated, and no `$panic`,
   no `discriminant`, no `comptime_int`.
3. **Phase 10 (codegen and the real driver) is next.** `design/roadmap.md` §10,
   and **`design/lir.md` §10 is the brief** — the instruction set (four
   statements, four terminators, seven rvalues), the four things a backend does
   itself, the one external dependency, and the list of things that look like
   gaps and are not. `-C` stays the interface for build settings
   (`nestc/src/common/options.rs`).
4. A small one first, if you want it: the `Drop` trait (`#lang("drop")`), which
   spec §8.4 promises and nothing implements. It attaches to the same ladder §5's
   drops now ride, and it is the per-type form of what `drop(p)` is the per-site
   form of.

## Edge cases and known limits

Everything in the previous handoff's list still holds. New or changed:

- **A `core.Location` type appears in most LIR dumps.** Any function with a
  checked operation has a local for the panic's argument, and the type table is
  the closure of what the functions mention.
- **`core.panic`'s own body is in every program.** It is a concrete function, so
  it is a monomorphization root; dead-code elimination is what would remove it
  from a program that cannot panic, and there is none.
- **An allocation in a loop body is dropped once per iteration**, on the scope's
  own rungs. That is correct and is also why the ordering rule matters: a `break`
  before the `let` builds the rung early.
- **`gc_keep_alive` and `gc_pin` need no special case in the safepoint pass.**
  They are intrinsics whose argument is an operand, so liveness counts them as
  reads, which is exactly what §6 asks `gc_keep_alive` to be.
- **A never-returning call still carries a safepoint** with its arguments live.
  Collection may happen inside `core.panic`, and the callee holds what it was
  given.
- **A slice a program reads from is never dropped.** Every use of one goes
  through `&xs` — `.len()` and `xs[i]` both do — and taking a local's address is
  not on §5's whitelist. `make` allocations are collected, not freed early. There
  is a test asserting exactly this so it is not rediscovered as a bug.
- **`drop` cannot free a slice by hand**: its parameter is `*mut T` and a slice
  is not a pointer. A `free`-shaped intrinsic over `[]mut T` is the missing
  piece, and it is not built.
- **`check::dropped` does not follow aliases.** `let q := p; drop(p); q.*.x` is
  not caught. That wants ownership; the limit is stated in `core`'s declaration
  of `drop` rather than hidden in the pass.
- **`transmute` reads its result type off the destination**, unlike `cast`, which
  carries both. That is what the operation *is* — reinterpret as whatever this
  slot holds — but it is the one intrinsic whose type is not in the instruction.
- **`bytes_eq` and `core.panic` are in every program.** Both are concrete
  functions and therefore monomorphization roots; dead-code elimination is what
  would drop them from a program that needs neither, and there is none.
