# Handoff: Phase 5 — the integer families are built; `usize` into `core` is not

**Generated**: 2026-09-11
**Branch**: `main`
**Status**: **371 tests pass**, `cargo clippy` reports 84 warnings (all
pre-existing dead code; identical to the baseline set, not merely the same
count), every file in `examples/*.nest` compiles.

**Read `design/roadmap.md` §5 first.** It is the plan of record and now carries a
"what is left" section naming the three gaps precisely. This document is the
*state* — what was decided, what is built, what will bite you.

## Goal

Phase 5: integers are generic families, so that `wrapping_add` and its
neighbours can be written **once** in `core` instead of once per width. `u4096`
is a legal type, so there is no finite list of widths — a generic self type is
the only way to reach them all.

## What is built (this session)

### The families (`6248d22`, `13ffe2d`)

- [x] **`Ty::Int { signed: bool, width: Const }`.** `IntWidth` is gone.
- [x] **Two families, not one.** `int.<N>` signed and `uint.<N>` unsigned, with
      `N` the width. The first cut had one family `int.<N, S>` with a `bool`
      signedness; the user changed it and the two-constructor form is better —
      see Key Decisions.
- [x] **`int` / `uint` are builtin type constructors** in type position.
      `int.<32>` **is** `i32` — the same `Ty`, not a conversion. `uint.<1>` is
      `bool`, matching `u1`. A bare `int` is refused: a family needs its width.
- [x] **Unification solves the width** through `unify_const`, so `uint.<N>`
      meeting `u8` learns `N = 8`. The signedness is *compared*, never solved.
- [x] **A family impl works.** `packages/core/num.nest` declares
      `wrapping_add` / `wrapping_sub` as `#intrinsic` members of
      `impl <const N: u16> int.<N>` and its `uint.<N>` twin. Verified on `u8`,
      `i64`, `i7`, `u4096`, and inside a caller's own `<const N>` generic.
- [x] **Display prints the sugar** when the width is known, the family form when
      it is not. **Zero snapshot churn** — all 135 lines stayed put.

### Implicit widening (`cb85833`)

- [x] **A narrower integer stands where a wider one is wanted.** The rule is
      **range containment**, not "more bits", so the signed cases fall out of it:
      same signedness wider always; unsigned→signed only when *strictly* wider
      (`u8`→`i16` yes, `u8`→`i8` no); signed→unsigned never.
- [x] **A `const` parameter fills a slot on the same rule.** `<const N: u16>` is
      a good `u32` argument.
- [x] **Pointer-sized types take no part**, either direction. See Key Decisions.
- [x] Lowers to the **existing** exact implicit `$cast` (`Coercion { to }`), so
      nothing new appears in the IR.

## What is NOT built — `usize` into `core`

The design is settled. The code is **on the branch `phase5-usize-in-core`
(`90d57b1`), which does not build clean.** `main` does not contain it.

Intended shape:

```nest
// packages/core/num.nest
usize :: distinct uint.<PTR_BITS>
isize :: distinct  int.<PTR_BITS>
```

`PTR_BITS` comes from a compiler-**generated** `core/target.nest`, which also
supplies `OS`, `ARCH`, `PROFILE` as values of `Os` / `Arch` / `Profile` — enums
written by hand in `core/os.nest`. The generated file is a **member of core**,
not a package of its own, precisely so it can name those enums.

The branch has all of that written, plus the three mechanisms it needs (see Key
Decisions: the bare width, the `#lang` injection, the distinct-walker change).
**Three independent gaps stop it**, and they are why it is not on `main`:

1. **`Ty::is_primitive` is false for a nominal `distinct`.** `slice.nest`'s
   `impl <T, const N: usize> [N]T` is then rejected with "a `const` generic
   parameter must have a primitive type, but `N` is `core.usize`".
   `check_const_param` must resolve the numeric-distinct representation first.
2. **The const evaluator cannot evaluate an enum variant.** The generated
   `OS: Os :: Os.Linux` fails with "`Linux` has no compile-time value". This is
   the biggest of the three and is not about integers at all — it is a missing
   const-eval case.
3. **A width read from a constant fails.** `uint.<PTR_BITS>` reports "an integer
   width must be a literal, a constant, or a `const` generic parameter".
   Undiagnosed; suspect the `{ PTR_BITS } :: import "target.nest"` binding is a
   `DefKind::Local` (a destructuring field pattern) rather than something
   `const_of_def` follows.

Resume by fixing those three **on the branch**, then rebasing onto `main`.

## Failed approaches (don't repeat these)

Everything in the previous handoffs' lists still stands. New this session:

- **One family `int.<N, S>` with a `bool` signedness.** Built, then replaced.
  Nothing is ever generic over signedness, so the `S` argument buys an inference
  variable no program can solve, and it lets a signed and an unsigned type unify
  *through* it. Two constructors cost one extra impl in `core` and remove a whole
  class of wrong.
- **A width as a `Const::Value` at type `u16`.** Infinite regress: a `Value`
  carries the `Ty` it was written at; that `Ty` is `u16`, which is `uint.<16>`,
  whose width carries a `u16`… The branch's `Const::Width(u16)` holds the number
  **bare**, which is what makes the representation finite. Do not "simplify" it
  back into a typed value.
- **Typing an integer width at `usize` once `usize` is a `core` distinct.** A
  definitional cycle, not an implementation wrinkle: reading `PTR_BITS` at
  `usize` requires `usize`, which is defined as `uint.<PTR_BITS>`. The `u16`
  width is what breaks it.
- **`Const::PtrBits` (an opaque width) to keep `usize` from being `u64`.** Built
  and committed, then removed on the branch in favour of `distinct`. It worked,
  but it is a special case in the const lattice where §2.4 already has a
  mechanism. Named here because the *invariant* it protected is still the one
  that matters.
- **Forgetting `unify_const` needs an arm for a new `Const` variant.** Adding
  `PtrBits` without one made `usize` fail to unify with **itself** — 240 tests,
  all reporting ``expected `usize`, found `usize```. Any new variant needs its
  reflexive case.
- **Assuming `subst_type_params` / `collect_generic_params` / `resolve` /
  `finalize` reach a new field.** `Ty::Int` gained a `Const`, and every one of
  those four walkers needed an arm. The one that actually bit: without the
  `collect_generic_params` arm, the family impl's `N` is never freshened, so
  `a.wrapping_add(b)` on a `u8` returns `uint.<N>` with `N` still symbolic.
- **`Target::HOST` in `Ty::display`.** Rendering a type must not consult the
  target — that is the fold the whole design forbids. Use `Const::value()` /
  `Const::bits()`, which need no target.
- **A generated module as its own package.** It cannot name `core`'s types, so
  `OS` could only be a string. Making it a *member of core* is what lets the
  enums live in core, which is what the user asked for.
- **Naming a generated file `<core/target.nest>`.** A relative
  `import "os.nest"` inside it resolves against the file's own name, so the
  placeholder name broke the import. Give it core's real directory.

## Key decisions

| Decision | Rationale |
|---|---|
| Two families `int.<N>` / `uint.<N>`, not one `int.<N, S>` | Signedness selects the family; nothing is generic over it. An argument for it buys an unsolvable inference variable and lets signed and unsigned unify through it |
| A width is a `u16` | §3.1 caps a width at 65535, so `u16` holds every legal one. It is also what breaks the `usize` definitional cycle |
| A width is a **bare** number (`Const::Width`), not a value at a type | A typed width regresses infinitely: `u16` is `uint.<16>` whose width is a `u16`. Bare is finite — and is what a width *is*, since nothing stores one at run time |
| A written width is normalized to `Const::Width` | Otherwise `int.<32>` carries a `u16`-typed `Const::Value` and `i32` carries a bare width, and the two compare unequal — the "same type" promise would be a lie |
| Widening is **range containment**, not "more bits" | The signed cases fall out instead of being special-cased: unsigned→signed needs the extra bit, signed→unsigned has nowhere to put a negative |
| Pointer-sized types never widen, either direction | Whether a `u64` fits a `usize` is the target's business. A coercion that appears on one machine and not another is worse than one that never happens |
| A widening reuses `Coercion { to }` | It is exact, which is exactly what the existing implicit `$cast` promises. A new node kind would say nothing new |
| `usize` is a `distinct`, not a primitive (branch) | §2.4 already means "same representation, different type". That is the whole requirement, so the language's own mechanism should carry it |
| The generated target module is a **member of core** (branch) | Only then can it name `core`'s `Os` / `Arch` / `Profile`. The coupling becomes two files of one package rather than compiler-to-library |
| `usize` / `isize` are injected into `InferCtxt` by `#lang` tag (branch) | Exactly the existing `set_str_ty` precedent: unification has no def table and cannot do a `#lang` lookup |

## Current state

**Working**: everything on `main`. `cd nestc && cargo test` → **371 passed**.
`cargo clippy` → 84 warnings, the same *set* as before this session. Every file
in `examples/*.nest` compiles clean.

**Broken**: nothing on `main`. The branch `phase5-usize-in-core` does not build.

**Uncommitted changes**: none.

**A deliberate gap**: `spec/03-types.md` §3.1 already describes `usize` as
`distinct uint.<PTR_BITS>` and documents the target constants, because that is
the settled design — the spec is ahead of `main` on exactly that one point, and
`design/roadmap.md` §5 says so.

## Files to know

| File | Why it matters |
|---|---|
| `design/roadmap.md` §5 | The plan, with the three remaining gaps named. Read before starting. |
| `nestc/src/sema/ty.rs` | `Ty::Int`, `Const`, `int_widens`, `int_parts`, `unify`, display. The phase lives here. |
| `nestc/src/sema/infer.rs` | `int_family_ty` (the `int.<N>` reader), `try_int_widen`, `const_ty_widens`, `is_ptr_sized`, `collect_generic_params`, `subst_const`. |
| `packages/core/num.nest` | The two family impls, and where `usize` / `isize` are meant to land. |
| `packages/core/slice.nest` | The `[N]T` precedent the family impls copy. |
| `nestc/src/sema/intrinsics.rs` | A new intrinsic is a row here **first**; the `core` declaration is rejected until it exists. |
| `nestc/src/common/options.rs` | `Target` (now `pointer_bits`, `os`, `arch`) and `profile`. `-C os=`, `-C arch=`, `-C profile=`. |

## Code context

```nest
// The families, and the sugar that names their members.
f :: func (a: int.<32>) -> i32 { return a }     // the same type
g :: func (b: uint.<8>) -> u8  { return b }
h :: func (c: uint.<1>) -> bool { return c }    // `u1` is `bool`

// Written once, reaching every width.
impl <const N: u16> uint.<N> {
  @public wrapping_add :: #intrinsic("wrapping_add") func (self: Self, rhs: Self) -> Self
}
x :: func (a: u4096, b: u4096) -> u4096 { return a.wrapping_add(b) }

// Widening: narrower into wider, never the reverse.
w :: func (v: u16) -> u32 { return v }          // fine
n :: func (v: u32) -> u16 { return v }          // error
s :: func (v: u8)  -> i16 { return v }          // fine: strictly wider
e :: func (v: i16) -> u32 { return v }          // error: signed into unsigned
```

```rust
// sema/ty.rs
pub enum Ty { Int { signed: bool, width: Const }, /* … */ }
pub fn int_widens(from: (bool, u32), to: (bool, u32)) -> bool;
impl Ty {
    pub fn int(bits: u16, signed: bool) -> Ty;   // `int.<bits>` / `uint.<bits>`
    pub fn u8() -> Ty;
    pub fn int_parts(&self, target: Target) -> Option<(bool, u32)>;  // None when symbolic
}
```

**The non-obvious bits.**

*A width is not a type argument like any other.* It is normalized, range-checked
at `u16`, and compared bare. `int_family_ty` is the only place that builds one
from source, and `primitive_ty` is the only place that builds one from a name;
those two **must** agree or `int.<32>` and `i32` stop being one type.

*The signedness is compared, never unified.* `unify` on two integers runs
`s1 != s2 || unify_const(w1, w2)`. If you ever make signedness a `Const`, this is
the line that silently starts letting `i32` unify with `u32`.

*Widening happens in `expect`, not `unify`.* `unify` is symmetric and most of its
callers are joins; the direction only exists where one side is the value and the
other is the demand. Same reason `never` is one-way there.

*`int_parts` returns `None` for a symbolic width.* Every caller has to decide
what "not known until monomorphization" means for it. None of them may invent a
number — that is how `usize` would silently become `u64`.

## Resume instructions

1. `cd nestc && cargo test` — expect **371 passed**.
2. See the phase working:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
   wide :: func (a: u4096, b: u4096) -> u4096 { return a.wrapping_add(b) }
   same :: func (a: int.<32>) -> i32 { return a }
   widen :: func (x: u16) -> u32 { return x }
   fam :: func <const N: u16> (a: int.<N>, b: int.<N>) -> int.<N> { return a.wrapping_add(b) }
   EOF
   ./target/debug/nestc /tmp/e.nest | grep -E '\$wrapping|\$cast'
   ```
   Expected: no diagnostics; `$wrapping_add(a: u4096, b: u4096): u4096` and an
   implicit `$cast(x: u16): u32`.
3. **The remaining work**: `git checkout phase5-usize-in-core`, fix the three
   gaps listed above, rebase onto `main`. Gap 2 (const-eval of an enum variant)
   is the one to size first — it is the largest and the least about integers.
4. Whatever you touch, verify with all three:
   - `cargo test` (371 and rising)
   - `for f in ../examples/*.nest; do ./target/debug/nestc "$f" >/dev/null || echo "FAIL $f"; done`
   - `cargo clippy` — compare the warning **set**, not the count.

## Edge cases and known limits

Everything in the previous handoff's list still holds except where noted. New or
changed:

- **`usize` / `isize` are still compiler primitives on `main`**, with an opaque
  pointer-sized width (`Const::PtrBits`). The spec says otherwise; see "A
  deliberate gap".
- **`int.<0>` is refused**, and a width over `65535` is caught by the `u16` slot
  as ``70000` does not fit in `u16``, reported against the literal.
- **A bare `int` / `uint` is not a type.** Unlike `Box` for `Box.<T>`, a family
  name must carry its width — otherwise a forgotten `.<32>` becomes an inference
  error somewhere else entirely.
- **Widening does not apply to `distinct` numerics.** It is an integer rule; a
  `distinct u8` is not a `u8` for this purpose, which is the point of `distinct`.
- **`a_constant_is_comptime_unless_its_type_is_written`** and
  **`a_constants_type_goes_before_the_binder`** were retargeted from `i32` to
  `i8`, because a `u8` constant *does* now reach an `i32` by widening. Their
  intent survives: a bare `5` reaches an `i8` and a pinned `u8` does not.
- **A `const` generic parameter on a *type* declaration is still rejected.**
  Unchanged by phase 5; the families are builtin constructors, not user types.
- **Array lengths still do not go through the const evaluator** (`[SIZE * 2]T`).
  Phase 6.

## Warnings

- **Run everything from `nestc/`.** `packages/` and `examples/` are one level up.
  A `cd` inside a compound Bash command silently changes the working directory
  for later calls — use absolute paths.
- **`cargo test` does not rebuild `target/debug/nestc`.** Run `cargo build`
  before the examples loop or you are testing a stale binary.
- **Compare the clippy warning *set*, not the count.** A new warning and a fixed
  one cancel out in a count. `cargo clippy --message-format=short`, strip line
  numbers, `sort`, `diff` against the baseline.
- **`rustfmt --edition 2024 <file>` follows `mod` declarations** and reformats
  every file it reaches. **Never** run `cargo fmt` with no arguments.
- **Regenerating snapshots**: `INSTA_UPDATE=always cargo test`, then
  `rm -f src/*/snapshots/*.snap.new` and
  `sed -i '' '/^assertion_line: /d' src/*/snapshots/*.snap`. Then **read every
  diff** — against `git`, not against `.snap.new`.
- **Many tests assert *exactly one* diagnostic.** Deliberate: the regression
  guard against cascades. Fix the cascade rather than loosening the assertion.
- **A new intrinsic is a row in `sema/intrinsics.rs` first.**
- **`#lang` discovery is by tag only.** Never hardcode a core type's name, file
  or position. The one place this was nearly broken this session was pinning
  `DefId::USIZE` to a position in `PRIMITIVES` — that approach was abandoned.
- **Nest syntax worth restating**: `f :: func () { … }`, not `func f() { … }`;
  `let x: T := v`; an array literal is `[_]T { a, b }`; match arms are
  comma-separated.
- **Comments explain *why*, at length, and cite spec sections inline.**

## User notes

- Commits are **title-only**: no body, no co-author trailer, changes bundled as
  `feat: a + feat: b + fix: c`.
- The user redesigned the integer representation three times mid-implementation
  (one family → two families → `usize` as a core `distinct`). Each change was an
  improvement and each is recorded above with its reason. Expect the design to
  keep moving; commit green states often so a redesign costs one commit, not a
  session.
