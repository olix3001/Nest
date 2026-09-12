# Handoff: Phase 9 is done, and LIR has been audited against a real backend

**Generated**: 2026-09-12
**Branch**: `main`
**Status**: **499 tests pass**, `cargo clippy` reports 83 warnings (the same
dead-code-shaped set as before — fields codegen will read and nothing does yet).
Every file in `examples/*.nest` compiles.

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

**Working**: everything. `cd nestc && cargo test` → **499 passed**. `cargo
clippy` → 83 warnings. Every file in `examples/*.nest` compiles clean.

**Broken**: nothing.

**Uncommitted changes**: none.

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

1. `cd nestc && cargo test` — expect **499 passed**.
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
