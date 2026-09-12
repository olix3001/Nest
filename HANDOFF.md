# Handoff: Phase 9 is done — drops, safepoints, and a panic that is a call

**Generated**: 2026-09-12
**Branch**: `main`
**Status**: **461 tests pass**, `cargo clippy` reports 83 warnings (the same
dead-code-shaped set as before — fields codegen will read and nothing does yet).
Every file in `examples/*.nest` compiles.

**Read `design/roadmap.md` §9 first**, then `design/lir.md` §5 and §6. This
document is the *state*. **Phase 10 (codegen and the real driver) is next and is
unblocked.**

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
      rung. A new `StmtKind::Drop(LocalId)`.
- [x] **Safepoints** (§6) at a call, an allocation and a loop back edge, each
      carrying the roots live **before** the statement.
- [x] **Real liveness** — backward dataflow to a fixed point over the locals
      whose type can hold a reference.
- [x] **12 new tests** and a 13th snapshot.

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

## Current state

**Working**: everything. `cd nestc && cargo test` → **461 passed**. `cargo
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
// lir/mod.rs — the whole instruction set
pub enum StmtKind {
    Assign { place: Place, value: Rvalue },
    Call { dest: Option<Place>, callee: Callee, args: Vec<Operand> },
    Drop(LocalId),
}
pub struct Stmt { kind: StmtKind, span: Option<FileSpan>, safepoint: Option<Safepoint> }
pub struct Safepoint { live: Vec<LocalId> }   // the roots *and* the relocs
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

1. `cd nestc && cargo test` — expect **461 passed**.
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
3. **Phase 10 (codegen and the real driver) is next.** `design/roadmap.md` §10.
   The instruction set it has to answer for is three statements and four
   terminators (`design/lir.md` §1). `-C` stays the interface for build settings
   (`nestc/src/common/options.rs`).
4. A small one first, if you want it: the `Drop` trait (`#lang("drop")`), which
   spec §8.4 promises and nothing implements. It attaches to the same ladder §5's
   drops now ride.

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
