# Handoff: LIR is ordinary now, and it comes out in codegen units

**Generated**: 2026-09-12
**Branch**: `main`
**Status**: **532 tests pass**, `cargo clippy` reports 78 warnings (the same
dead-code-shaped set as before — fields codegen will read and nothing does yet).
Every file in `examples/*.nest` compiles.

**Read `design/lir.md` §10 and §11 first.** §10 is the whole instruction set a
backend answers for, and it is now a short list with nothing reserved. §11 is
new: the codegen-unit split. This document is the *state*. **Phase 10 (codegen
and the real driver) is next and is unblocked.**

## Goal

The standing requirement, in the user's words: *LIR should be as simple as
possible without being platform-specific (aside from pointer size)*, and
*instructions should not be codegen-specific — a backend for C, LLVM or wasm
should be able to translate each one*. Plus, from this session: **everything in a
dump written out in full names**, and **split LIR into codegen units, each
carrying declarations for what it calls, so codegen can link them**.

The previous handoff's A–N change list was the plan. All of it is done, and the
split is built on top of it.

## Completed

### The A–N list, all of it

- [x] **A. A vtable is a global.** One struct type per *trait*
      (`type vtable.Draw = struct { area: func(*void) -> i32, … }`), one
      immutable global per *impl*, and `*dyn Draw = { data: *void, vtable:
      *vtable.Draw }`. A dispatch is `d.vtable.*.area` — an ordinary member read
      at an offset the type table knows. `Program::vtables`, `VtableId`,
      `Constant::Vtable`, `AggregateKind::Dyn` and the `vtable …` dump line are
      all gone.
- [x] **B. A blob constant is a global.** A string's bytes, a byte string's and
      a folded aggregate each become an immutable global; an operand is a scalar,
      an address or `undef`. A `str` value is then `[]u8(&const.str.0, 3)` — an
      aggregate over an address and a length.
- [x] **C. One arithmetic rvalue.** `Rvalue::Op { op, ty, args }` covers unary,
      binary and checked alike. Checked arithmetic is its own opcode
      (`add_checked`), not a flag that changes the result type. The `ty` is the
      type the operation *runs at*, because `lt.u64` and `lt.i64` are different
      instructions and a constant operand carries no type. A plain `add`
      **wraps** — `overflow=wrap`, `#unsafe` and `wrapping_add` all emit it,
      because an opcode whose meaning depends on a build setting a backend
      cannot see is the one thing this level exists to prevent.
- [x] **D. `AggregateKind` collapsed** to `Struct(TypeId)`, `Array`, `Variant`.
- [x] **E/F.** Were already done.
- [x] **G + I. One call statement, three callees.** `Callee::Static(FuncId)`,
      `Callee::Indirect(operand)`, `Callee::Intrinsic(Intrinsic)` — the last an
      **enum**, so a backend's match is exhaustive. `StmtKind::Intrinsic` is
      gone; three statement kinds remain (`Assign`, `Call`, `Drop`).
- [x] **H. LIR has its own type.** `lir::Ty` with `Named(TypeId)` indexing the
      unit's own table. **No `DefId` survives lowering.**
- [x] **J. Mutability is erased.** `*T`/`*mut T` and `[]T`/`[]mut T` are one
      type; `Rvalue::Ref` has no flag. `Cx::strip` does it beside `distinct`.
- [x] **K. `Constant` is LIR's own**: `Int` (arbitrary precision — `u4096` is a
      legal type), `Float`, `Bool`, `Func`, `Global`, `Aggregate`, `Bytes`,
      `Variant`, `Undef`. `ir::ConstValue` no longer appears in LIR.
- [x] **L. A callee carries one name and one symbol**, both for people; identity
      is the index.
- [x] **M. Directives became `FunctionAttrs`**: section, inline, offset, public,
      unchecked. A backend reads decided facts, not AST-shaped arguments.
- [x] **N. A variant is a type.** Each enum variant gets a `TypeDef` whose
      members sit at payload-relative offsets, and `Projection::Variant` became
      `Projection::Cast(TypeId)`: `(s.payload as Shape.rect).w`. The offsets come
      from the table like every other member's.

### Two more the list did not have

- [x] **`void` is erased from every slot.** A `void` parameter is not passed, a
      `void` binding gets no slot, a `void` argument is evaluated and dropped,
      and a function-pointer type drops it too. This was found by an invariant
      test over `examples/errors.nest`: `Option`'s `Residual :: void` produced
      `let _7: void`, a slot no machine has.
- [x] **`()` is not an aggregate.** `.stop(())` built an `Aggregate` of nothing
      and reached the error type. It is `undef` now.

### Codegen units (§11) — `nestc/src/lir/unit.rs`, new

- [x] **`-C codegen-units=N`**, default **1**. One unit per source file, then the
      smallest merged until there are at most `N` — the shape rustc uses, for the
      same two reasons (the merge bounds the count; merging the smallest keeps
      them comparable in size, which decides the slowest thread).
- [x] **A unit is self-contained.** It carries the functions it defines, a
      **declaration** (no blocks) for every function it calls, its own type table
      with every index renumbered, and the globals it names.
- [x] **Linkage is explicit.** `External` (a `#static`: one definition, imported
      elsewhere), `Internal` (a string's bytes, a vtable: nothing can name it, so
      each unit gets a private copy), `Imported` (another unit defines it).
- [x] **Dumps are per unit**, headed `unit <name> { … }`.

### The dump says everything in full

`add_checked.i32 _16, _18`, `switch.bool _19.1`, `call core.panic(_20, _22)`,
`_2 := *dyn Draw(sq_0, &vtable.Draw.for.Square)`, `private const const.str.0:
[3]u8 = b"yes"`, `(s_0.payload as Shape.rect).w`, `declare func core.panic(msg_0:
[]u8, loc_1: core.Location) -> never @public  // _NC4core5panic`. Every index the
structures hold is resolved to a name before it is printed; a test asserts no
dump contains `<unknown …>`.

## Not Yet Done

- [ ] **Phase 10: codegen and the real driver.** `design/lir.md` §10 is the
      brief and there is nothing left in it marked "the backend re-derives this".
- [ ] **The `Drop` trait** (`#lang("drop")`), which spec §8.4 promises and
      nothing implements. It attaches to the same ladder §5's drops ride.
- [ ] **Optimization across units** — the cost of the split, and the reason the
      default is 1.
- [ ] The three §5/§6 items still open: per-function escape summaries, the
      object-start table, narrowing what counts as a root.

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs' lists still stands. New this session:

- **Keying the type table by the front end's `mono::type_key` and leaving
  mutability in it.** `[]i32` and `[]mut i32` then sat in the table as two
  entries with identical members and identical layout. The fix is to erase
  mutability *inside `Cx::strip`*, so the key, the display name and the members
  all agree. The **symbol** must keep using the unstripped type —
  `mono::type_key` mangles mutability and monomorphization already decided every
  name — which is why nothing in `lir` recomputes a symbol.
- **Letting a synthesized data global be defined by every unit that uses it.**
  The first split had `home_unit` return true for any global with no span, so
  `_NK1` (`b"integer overflow"`) was *defined* in three units at once — three
  definitions of one symbol, which is what a linker refuses. The fix is
  `Linkage::Internal`: private to each unit, and the linker never sees the name.
  A test now asserts every `External` symbol is defined exactly once at five
  settings of `-C codegen-units`.
- **Naming a variant's payload members by position.** `(s.payload as
  Shape.rect).0` — the index is right and the name is a lie, since the variant's
  type calls that member `w`. `Lowerer::variant_member` reads the name off the
  variant's `TypeDef` now. This is item F's rule, one level deeper than item F
  found it.
- **Filtering `core` out of a snapshot in the renderer.** That is what the old
  harness did, and it made a dump showing `call core.panic` with no `core.panic`
  under it look like a missing definition. The split does it properly now: the
  test harness lowers with one unit per file and renders the entry file's, so
  `core` appears as `declare func` — which is the truth about what that object
  file contains.
- **A `Ty::Func` that keeps its `void` parameters.** The signature erases them
  and the pointer type did not, so an indirect call would have had a type with
  one more parameter than the callee takes. Both erase now, and
  `every_call_agrees_with_its_callees_signature` is the test that says so.
- **Three wrapping opcodes beside the plain ones.** `add_wrap` was emitted for
  `wrapping_add` while `overflow=wrap` emitted `add` — two spellings of one
  instruction, and `add`'s meaning then depended on a build setting. The plain
  opcodes are defined to wrap and the `*_wrap` ones are gone.
- **Trusting `mono`-style mangling for a global's symbol.** A `#static` written
  inside a function body has no canonical path — two functions each declaring
  `n` both mangled to `_NG1n`, which is two definitions of one symbol. The
  uniqueness is the lowering's now (`fn unique`, a `Z<k>` suffix), because
  `lir::lower` is the one place every global in the program passes through.
- **`Constant::Zeroed` beside `init: None`.** Two spellings of "all zero" —
  exactly the overlap item K was about. Removed; `None` means zeroed.

## Key Decisions

| Decision | Rationale |
|---|---|
| LIR gets its own `Ty` | A unit that cannot answer "what is this type" without the compiler's tables is not a unit anyone can hand to another process |
| A vtable is a global, and its type is the **trait's** | A `dyn` erased the concrete type, so one struct type per trait is what makes a slot's offset knowable at the call |
| A variant is a `TypeDef`, reached by `Projection::Cast` | The payload stays `[N]u8` and is aligned for every variant, so the cast is always legal — and member offsets then come from the table |
| One `Op` with explicit opcodes | The arity is the opcode's business; a flag that changes the result type is not a flag |
| A plain `add` is defined to wrap | Otherwise one opcode means two things depending on `-C overflow`, which a backend cannot see |
| An intrinsic is an enum, and a **callee** | A backend's match is exhaustive, and "run this thing, put the answer here" is one statement |
| Mutability is erased | No target has two kinds of address, and the rule that needed it ran in sema |
| `void` is erased from slots, parameters and arguments | Both sides of a call erase by the same rule, so arities cannot disagree |
| A blob constant is a global | Otherwise every backend invents read-only data emission, differently |
| Units are one per file, merged smallest-first | A file is what a person recompiles; the merge is what bounds the count |
| Private linkage for data nothing can name | A unit should not depend on a neighbour's anonymous bytes |
| `-C codegen-units` defaults to 1 | There is no backend yet, and one unit is the dump a person reads |
| A unit refers to everything by index into itself | Makes the split a filter, and makes every dangling reference a test failure |

## Current State

**Working**: everything. `cd nestc && cargo test` → **532 passed**. `cargo
clippy` → 78 warnings, all dead-code-shaped. Every file in `examples/*.nest`
compiles clean, at four settings of `-C codegen-units`.

**Broken**: nothing.

**Uncommitted changes**: this work, unstaged. 45 files: `nestc/src/lir/*` (the
representation, the lowering, the split, the dump, the tests), 34 snapshots,
`nestc/src/ir/mono.rs` (a new `global_symbol`), `nestc/src/common/options.rs`
(the new setting), `nestc/src/main.rs`, `design/lir.md`, `design/roadmap.md`.

## Files to Know

| File | Why it matters |
|---|---|
| `design/lir.md` | The specification. §7b (types), §9 (what is gone), §10 (the backend's list), §11 (codegen units) are the ones that changed. |
| `design/roadmap.md` | Phase 9b is this work; phase 10 is next. |
| `nestc/src/lir/mod.rs` | The whole data model: `Ty`, `TypeDef`, `Unit`, `StmtKind`, `Callee`, `Intrinsic`, `Op`, `Constant`, `Linkage`. |
| `nestc/src/lir/lower.rs` | `Cx::lir` (front-end type → LIR type, interning as it goes), `Cx::intern` / `flatten` / `variant_type` / `vtable_type` / `vtable`, `Cx::const_data` and `Lowerer::const_rvalue` (where a blob becomes a global), `Lowerer::passed` (the `void` erasure at a call). |
| `nestc/src/lir/unit.rs` | The split: `partition` (per file, merged), `build` (collect, then renumber), `home_unit` (which unit defines a global). |
| `nestc/src/lir/safepoint.rs` | Liveness, back edges, and what counts as a root — now able to say a vtable pointer and a function pointer are *code*. |
| `nestc/src/lir/tests.rs` | The invariants are under `// ===< LIR well-formedness >===` and `// ===< The codegen-unit split (§11) >===`. |
| `nestc/src/common/options.rs` | `codegen_units`, and the `-C` parsing. |

## Code Context

```
unit main {
  type Point = struct {           // was a struct; size 8, align 4
    x: i32                    // +0
    y: i32                    // +4
  }

  func main() -> void @public  // _NC4main
    let _0: Point
  bb0:                    // entry
    _0 := Point(1, 2)
    _1 := call scale(_0, 3)
    return

  declare func scale(p_0: Point, k_1: i32) -> Point @public  // _NC5scale
}
```

```rust
// lir/mod.rs — the whole instruction set (see design/lir.md §10)
pub enum StmtKind {
    Assign { place: Place, value: Rvalue },
    Call { dest: Option<Place>, callee: Callee, args: Vec<Operand> },
    Drop(Operand),                                  // always a pointer
}
pub enum Callee { Static(FuncId), Indirect(Operand), Intrinsic(Intrinsic) }
pub enum Rvalue {
    Use(Operand),
    Ref(Place),
    Op { op: Op, ty: Ty, args: Vec<Operand> },      // the type it runs *at*
    Cast { value: Operand, from: Ty, to: Ty },
    Aggregate { kind: Aggregate, fields: Vec<Operand> },
    Offset { ptr: Operand, index: Operand, stride: u64 },   // stride in bytes
}
pub enum Ty {
    Int { bits: u16, signed: bool }, Float { bits: u16 }, Bool,
    Ptr(Box<Ty>), Array { len: u64, elem: Box<Ty> },
    Func { params: Vec<Ty>, ret: Box<Ty> },
    Named(TypeId),                                  // into Unit::types
    Void, Never,                                    // return types only
}
pub enum Projection { Field { index, name }, Index(Operand), Deref, Cast(TypeId) }
```

**The non-obvious bits.**

*Interning happens during lowering, not after it.* `Cx::lir` converts a front-end
type and records any aggregate it meets, reserving the `TypeId` **before**
computing the members so a struct holding a pointer to itself terminates.

*The lowering keeps both types.* `Lowerer::locals` holds the machine type;
`Lowerer::local_tys` holds the front-end one beside it, because the questions
still ahead (which member is `len`, what a variant's payload contains) are asked
of a type the layout engine understands.

*The split runs last, inside `lower()`.* Safepoints are annotated on the
whole-program unit first — what is live at a call is a property of the finished
graph — and `unit::split` then filters and renumbers.

*A monomorphized instance belongs to the unit of the file that **defined the
generic**, not the one that instantiated it,* because that is where its span
points. Deterministic, and it keeps every instantiation of one generic together.

*`Maps::ty/func/global` in `unit.rs` fall back to index 0* for an id the
collection pass missed. That would be a silent alias, which is why
`check_unit_is_closed` runs over every example at four settings: the fallback is
unreachable, and the test is what says so.

## Resume Instructions

1. `cd nestc && cargo test` — expect **532 passed**.
2. See the split working:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
   { scale, Point } :: import "shapes.nest"
   @public main :: func () { let q := scale(Point { x: 1, y: 2 }, 3) }
   EOF
   cat > /tmp/shapes.nest <<'EOF'
   @public Point :: struct { x: i32, y: i32 }
   @public scale :: func (p: Point, k: i32) -> Point { return Point { x: p.x * k, y: p.y * k } }
   EOF
   ./target/debug/nestc -C codegen-units=8 /tmp/e.nest | sed -n '/===< LIR/,$p'
   ```
   Expect: a `unit e` holding `main` and a `declare func scale(…)`, a
   `unit shapes` holding `scale`, and `Point` defined in both.
3. **Phase 10 (codegen and the real driver) is next.** `design/lir.md` §10 is the
   brief: three statements, three callees, four terminators, six rvalues,
   twenty-four opcodes, thirteen intrinsics, and four things a backend does
   itself. `-C` stays the interface for build settings.
4. A small one first, if you want it: the `Drop` trait (`#lang("drop")`).

## Edge Cases & Error Handling

- **A file with data and no code.** Its `#static`s have no unit of their own, so
  unit 0 defines them (`home_unit`'s last clause). Exactly one definition either
  way; a test covers it.
- **Two units needing the same string.** Each gets its own private copy. The
  names collide by design and never reach the linker.
- **A zero-length string constant** becomes a `[0]u8` global. A backend may emit
  nothing for it as long as the address is valid.
- **A `char`** is `u32` by this level, so `c == 'a'` prints as `eq.u32 c_0, 97`.
  The block label still says `arm 'a'`, which is where the spelling survives.
- **`Intrinsic::Unknown`** exists so that adding a row to `sema::intrinsics` with
  no case here cannot silently reach a backend as a name.
  `every_declared_intrinsic_has_a_lir_case` fails the build instead, and
  `no_program_contains_an_unknown_intrinsic` says no program holds one.
- **The error type.** `Cx::error_type` is a struct with no members, for an
  aggregate that is not one — a program that did not type-check. A test asserts
  no program that *did* type-check mentions it.
- Everything in the previous handoff's list still holds: a slice a program reads
  from is never dropped, `check::dropped` does not follow aliases, `transmute`
  reads its result type off the destination, `core.panic` and `bytes_eq` are in
  every program because there is no dead-code elimination.

## Warnings

- **Do not reintroduce a filter in the renderer.** `pretty::unit_to_string`
  prints a unit exactly as it is; if a snapshot shows too much, the split is what
  to change.
- **`Cx::strip` is load-bearing in three ways at once** — `distinct`, mutability,
  and the key the type table is built on. A fourth erasure added there changes
  every type name in every snapshot; that is the intended cost, but read the
  diffs.
- **`-C codegen-units` is not tuning.** At `N > 1` the program is cut and
  optimization across the cut is gone; the default is 1 for that reason.
- **78 clippy warnings are expected.** They are fields and methods codegen will
  read and nothing does yet (`Op::arity`, `Unit::func`, `TypeDef::key`).
