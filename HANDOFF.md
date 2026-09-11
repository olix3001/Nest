# Handoff: Phases 2–4 built, plus a round of language decisions; phase 5 next

**Generated**: 2026-09-11
**Branch**: `main`
**Status**: Ready for review. **Phases 0 through 4 have no items left.**
**363 tests pass**, `cargo clippy` reports 84 warnings (all pre-existing dead
code; the baseline was 88 and nothing was added), every file in `examples/*.nest`
compiles.

**Read `design/roadmap.md` first.** It is the plan of record: ten phases, in
order, each completed one marked ✅ with a "what it actually took" section saying
where reality departed from the plan. This document is the *state* — what was
decided, what is built, what will bite you.

## Goal

Build everything that reads the IR: the validation passes deferred out of
inference, then monomorphization, then **LIR** — a MIR-like low-level IR that is
the last stop before code generation. `design/lir.md` is the LIR design;
`design/roadmap.md` is the order of work; this document is the state.

The user scoped this session to **phases 2, 3 and 4 only**, one commit each.
All three are committed.

## What is built (this session)

### Phase 2 — the prelude split (`effcafa`)

- [x] **`packages/core/prelude.nest`** — re-exports `Option`, `Result`,
      `ControlFlow`, `str`, and (after phase 3) `cast`, `size_of`, `panic`.
      Nothing else in `core` is globbed.
- [x] **`core.nest` re-exports topic *namespaces***, not members, so
      `import <core/ops>` resolves. `str.nest` and `fail.nest` get no namespace
      re-export — a namespace called `str` or `panic` beside the prelude's `str`
      *type* and `panic` *function* would be one name meaning two things.
      (`panic.nest` was renamed **`fail.nest`** for this reason.)
- [x] **`#lang` may tag an `import` binding.** The prelude is a namespace
      assembled by re-export, so the binding that names it is the only thing a
      tag can sit on. The tag rides on `RawImport`/`ImportDecl` and is registered
      by `imports::wire`, because the target namespace is unknown before then.
- [x] **`sema::analyze` globs the `#lang("prelude")` namespace**, looked up
      *after* the wiring loop, never by name or path.
- [x] **A `#lang` trait is selectable without importing its name.** `a + b`
      reaches `Add` by tag; gating that on `import <core/ops>` would make `+`
      require an import. Same rule for the desugars that call a lang trait's
      method by name (`for` → `.into_iter()` / `.next()`, `.?` → `.branch()`).
      `Inferer::lang_traits` is the escape hatch, checked beside
      `in_scope_traits` in `select` and in impl-method lookup.
- [x] **Imports wire in dependency order** (`sema::mod::wire_order`, a post-order
      DFS tolerant of cycles).

### Phase 3 — `#intrinsic`, and retiring `$name` (`cead807`)

- [x] **`$` is not a token.** Dropped from the identifier regex; `parse_intrinsic_call`,
      `NodeKind::IntrinsicCall`, `Resolution::Intrinsic` and `DefKind::Intrinsic`
      are all gone.
- [x] **Every intrinsic is a bodyless `#intrinsic` declaration in `core`** —
      `mem.nest` (`cast`, `transmute`, `size_of`, `align_of`, `new`, `make`,
      `embed_file`), `fail.nest` (`panic`, `assert`), `gc.nest` (the three
      collector intrinsics), `slice.nest` (`len`).
- [x] **`sema/intrinsics.rs`** — the registry. `INTRINSICS` is the tag list;
      `Special` is the *two* rules a signature cannot state.
- [x] **Identity is the tag.** `#intrinsic("size_of")` names it; a bare
      `#intrinsic` defaults to the declared name. `Def::intrinsic_tag()` reads it.
      An unknown tag is an error **at the declaration**.
- [x] **A bodyless `func` must be `#intrinsic`, `extern`, or a trait
      requirement** (`Collector::check_bodyless`). That shape used to parse and
      mean nothing.
- [x] **An intrinsic call is an ordinary call.** Inference has no special path;
      `lower_call` / `lower_method_call` see the callee's tag and build
      `ExprKind::Intrinsic` instead of `ExprKind::Call`.
- [x] **`DIVERGING_INTRINSICS` is gone**: `panic` is declared `-> never`, so
      *any* call typing as `never` ends a block (`Inferer::diverges`).
- [x] **Comptime items need no sigil.** `assert(...)` may stand among a struct's
      fields, a trait's members, or a namespace's items;
      `Parser::at_comptime_item` decides by shape with one token of lookahead.

### Phase 4 — `const` generics over any primitive (`c99deac`)

- [x] **`Const::Value(u64)` → `Const::Value(Box<ConstArg>)`** — a
      `ir::const_eval::ConstValue` **and** the `Ty` it was written at. `Const`
      lost `Copy` and `Eq`.
- [x] **Both halves are identity.** `unify_const` compares the whole `ConstArg`,
      so `3u8` and `3usize` are different arguments.
- [x] **`check_const_param` admits any primitive** (`Ty::is_primitive`) and
      rejects aggregates with §5's reason.
- [x] **`const_value_in`** replaces `const_len_depth`: it reads a value *at* a
      wanted type, carries a slot description for diagnostics (`"an array
      length"` / `"a `const` argument"`), and range-checks integer literals where
      they are written.
- [x] **The turbofish parser takes any primitive literal**, negative numbers
      included (`Parser::const_arg_lit`). Inside `.<...>` a leading `-` is part
      of the literal, not an operator — there is no const-expression arithmetic
      there.
- [x] **An array length must be a `usize`** — see Key Decisions.
- [x] **A `const` parameter is type-checked wherever it appears in a type**, not
      only where it is read as a value. `a_const_parameter_in_a_type_is_checked_as_a_type`
      and `a_const_parameter_is_a_value_of_its_declared_type` pin the matrix:
      `N` solved from a `[N]T` argument and carried into the return type; an
      explicit `.<4>` contradicting the argument; a symbolic `[N]T` refusing a
      fixed-count literal; two distinct parameters being two distinct lengths;
      nesting (`[N][M]T`); two instantiations in one expression; and every
      primitive parameter reading back as its own type in value position.
- [x] A slot mismatch **names the slot**: "an array length is a `usize`, and this
      is not one" rather than "a `const` argument …".

### After the phases — a round of language decisions (`78d2b5e`..`4b041a3`)

The user raised four things and answered a design question on each. All are built.

- **`#caller_location` is a default argument** (`c00961a`). Spec §5.2 already
  specified this and the compiler implemented neither it nor the `#caller_location`
  *directive* it carried on `panic` (which parsed and did nothing). It is now an
  expression, legal **only** as a default argument, evaluating to
  `core.Location { file, line, column }` — a `#lang("location")` struct in
  `core/loc.nest`, not in the prelude. `panic("boom")` reports the caller's line.
  The lowering cannot go through the default cache (`Lowerer::defaults`), which
  lowers each default once and clones it: every call would get the *declaration's*
  position. `lower_args` checks for it and builds the value from the call site.
- **`Default` and the `..` spread** (`8de2d87`). The user chose Rust-style over
  field defaults: a literal never silently omits a field. `core.Default` is a
  `#lang("default")` trait, and `P { x: 5, ..rest }` fills the rest. It is
  **desugared** (`desugar::lower_spread`) into `{ __spread1 :: rest  P { x: 5,
  y: __spread1.y } }`, so inference sees an ordinary complete literal and every
  rule — privacy, field typing, missing-field — applies to the filled-in reads.
  The temporary is **typed** with the literal's own type node, which is what
  makes `P { x: 1, ..q }` a `Q`-is-not-a-`P` error rather than a silent build.
  Cost of desugaring early: the **type must be named**, so `.{ ..rest }` is
  refused — the field list comes from the resolved type and desugaring runs
  before inference.
- **A constant's type goes before the binder** (`ab4ebc6`). `MAX_BYTE: u8 :: 100`,
  and `#static count: usize :: 0` / `#static scratch: [4096]u8`, and a trait's
  `MAX: i32` / `MIN: i32 :: 0`. The point, in the user's words, is that
  `NAME :: type` is a type alias **all the time**. `#static` now *requires* its
  type — a region whose width depended on who read it is not a region. The AST
  did not change: the parser still builds `AssocConst { ty, default }`, so the
  whole typed-constant machinery downstream was already in place.
  `Output :: type` and `Output :: Vec3` keep `::`, because both are
  `name :: <a type>`, which is what that shape means everywhere.
- **Trait conformance is checked** (`01f3977`). `impls::build` checked that an
  impl's members were *present*; nothing checked they had the declared **type** —
  including method signatures, which §4.1 has always promised.
  `Inferer::check_impl_conformance` does it, in inference, because it is a
  question about types. Both sides are substituted into the impl's world first:
  `Self` → the impl's self type (`Self` inside a trait resolves to the trait's
  own def, so substituting that def is what replaces it), the trait's generics →
  what the impl wrote or a **fresh variable** when the impl wrote nothing (a bare
  `impl Add for V` leaves `Rhs` for its own members to choose), and a method's own
  generics aligned positionally onto the trait's.

Two bugs fixed on the way:

- **A struct-field default hung the compiler** (`78d2b5e`), and had since before
  this session. The language has no field defaults, so `parse_field` stopped at
  the type and left `:=` unconsumed; `expect_ident` reports *without* consuming,
  so the struct-body loop never reached `}`. It is now refused by name, and every
  member-body loop calls `Parser::ensure_progress` so the class cannot recur.
- **A function-local `#static` was not lowered as a global** (`4b041a3`) — a
  known limit the user leaned on ("inside functions `#static` is the way to
  create statics"). It resolved to a `DefKind::Local`, so `lower_globals` never
  saw it and the declared type was ignored (`usize` read back as `isize`). It is
  now introduced as a `DefKind::Const` region whose def points at the *binding*,
  with only its visibility coming from the block.

## Failed approaches (don't repeat these)

Everything in the previous handoffs' lists still stands. New this session:

- **Leaving operator traits behind the in-scope filter after the prelude split.**
  `impls` selection consults `in_scope_traits`, which was seeded from the
  whole-of-core glob. Removing the glob made `a + b` report
  ``i32` does not implement `core.Add.<i32>`` in **74 tests**. Traits reached by
  `#lang` tag must bypass that filter; the name is still needed to *write* an
  impl.
- **Wiring imports in `HashMap` order.** `walk_package` looks up a *member* of
  the target namespace, and `<core/ops>` names a member `core.nest` produces by
  re-export — so it does not exist until that file is wired. Symptom:
  ``package has no public member `ops```, intermittently. Fixed by `wire_order`,
  not by retrying.
- **Believing the roadmap's item 4 for phase 3.** It claimed `cast`, `transmute`
  and `panic` need special handling in inference because their signatures cannot
  say what they do. All three are ordinary declarations (see Key Decisions). The
  user flagged this before the work started and was right. The real exceptions
  are `make` and `len`.
- **`Const::Value(l) => l as usize`.** After the retype, `Const::Value` holds a
  box, so every `match` on it that assumed an integer had to go through
  `Const::value()`, which returns `Option<u64>` and is `None` for a `bool`.
- **A non-`usize` `const` parameter in an array-length slot.**
  `func <const N: u32> () -> [N]i32` with a `[4]i32` annotation at the call site
  reported ``type mismatch: expected `[4]i32`, found `[4]i32``` — the lengths
  differ *by type*. Do not "fix" this by coercing at the length slot; that puts a
  hole in the identity rule phase 4 exists to establish.
- **A blunt regex for the `NAME: T :: value` migration.** `([A-Z]\w*) :: (i32|…)`
  also matches `Alias :: i32` (a type alias), `Output :: i32` (an associated-type
  binding) and `Ty::isize()` (Rust). All three were rewritten and had to be
  reverted by hand. The typed-constant form and the alias form are the same shape
  until you know what the RHS *means*, which is the whole reason the syntax
  changed — a migration script cannot know it either.
- **Checking trait conformance in `impls.rs`.** That pass runs before inference,
  so there are no types to compare. Completeness belongs there; matching does not.
- **Comparing a trait method against an impl method without aligning their
  generics.** The trait's `X` and the impl's `X` are different `DefId`s, so two
  identical signatures failed to unify and reported `expected func(*S, X) -> i32,
  found func(*S, X) -> i32`.
- **Assuming `cargo test` rebuilds `target/debug/nestc`.** It does not. The
  examples loop ran against a stale binary twice and reported a phantom failure.
  `cargo build` first.
- **`cd nestc && …` in a compound Bash command.** The working directory persists
  into the *next* call. Use absolute paths.

## Key decisions

| Decision | Rationale |
|---|---|
| The prelude is found by `#lang("prelude")` on an `import` binding in `core.nest` | It is a namespace assembled by re-export; the binding is the only thing a tag can sit on. Same promise `#lang` makes everywhere: `core` stays renameable |
| A `#lang`-tagged trait is always a selection candidate | `a + b` reaches `Add` by tag, so the compiler named it, not the program. Otherwise `+` would need an import |
| `core/panic.nest` → `core/fail.nest`; `str.nest` gets no namespace re-export | The root exports each topic file as a namespace; `panic`/`str` would collide with the prelude's function and type |
| `#intrinsic` takes an **optional** tag | The spec's code blocks show the bare form; the roadmap wanted the tag. Both are accepted, and the tag is what the compiler keys on |
| `cast`, `transmute` and `panic` are **ordinary declarations** | `cast :: func <T, U> (x: U) -> T` — `T` first, so a turbofish pins it and `U` infers; `cast(x)` takes `T` from context; `panic -> never` states divergence. All three already worked |
| The only two `Special`s are `make` (result is `[]mut T`) and `len` (argument must be a sequence) | No bound in the language says "the same type, made mutable" or "one of the two built-in sequences" |
| The IR still prints `$name` for an intrinsic node | The IR is a compiler artifact, not source. Keeping it left 135 snapshot lines untouched |
| `$abort` is gone; `core`'s `.!` bodies call `panic` | `abort` had no spec entry; `panic` does |
| An array length must be a `usize`, checked at the declaration | §3.2 makes it a `usize` count. Typed const identity otherwise produces `expected [4]i32, found [4]i32`. Spec §5's illustration was corrected to match |
| `#caller_location` is a default argument, never anything else | A default is filled in at the call site, which is the whole of why it names the caller. Elsewhere it could only mean "the position of this expression" |
| `Default` + `..` spread, not struct field defaults | The user's call: a literal never silently omits a field, and `..` is the visible token that says "and the rest from here" |
| The spread is desugared before inference | Every rule that applies to a written field then applies to a filled-in one, free. The price is that the type must be named |
| `NAME: T :: value`, `#static NAME: T [:: value]`, `MAX: i32 [:: default]` | The type before the binder is what leaves `NAME :: type` a type alias unconditionally |
| A `#static` must write its type | A region is storage; storage has a width, and the zeroed form has no initializer to infer one from |
| Conformance is checked in inference | It is a question about types, and `impls::build` runs before there are any |
| A `const` argument carries its type | §5: `3u8` and `3usize` are different arguments. Comparing values alone would collapse them, and nothing would catch it until monomorphization |

## Current state

**Working**: everything. `cd nestc && cargo test` → **363 passed**. `cargo
clippy` → 84 warnings, all pre-existing dead code. Every file in
`examples/*.nest` compiles clean.

**Broken**: nothing known.

**Uncommitted changes**: `design/roadmap.md` (the ✅ marks and the three
"what it actually took" sections) and this file.

**A deliberate gap**: `spec/` still describes phase 5, which is not implemented.
Integers are still `Ty::Int { signed, width }` with `IntWidth::Fixed`/`Ptr`;
`i32` is a primitive rather than sugar for `int.<32, true>`; there is no
`wrapping_add`. `design/roadmap.md` §5 is the map of that gap.

## Files to know

| File | Why it matters |
|---|---|
| `design/roadmap.md` | **The plan**, with phases 2–4 marked done and what each actually took. Read before starting anything. |
| `design/lir.md` | The LIR design: mangling, debug info, the flattening table, the settings that change lowering. |
| `nestc/src/sema/intrinsics.rs` | The intrinsic registry. A new intrinsic is a row here **then** a declaration in `core` — the declaration is rejected until the row exists. |
| `nestc/src/sema/ty.rs` | `Ty`, `Const`, `ConstArg`, `InferCtxt`, `is_primitive`, `unify_const`, `primitive_ty`. **Phase 5 lands here hardest.** |
| `nestc/src/sema/infer.rs` | `const_value_in`, `const_from_lit`, `const_of_def`, `check_const_param`, `intrinsic_result`, `lang_traits`, `diverges`. |
| `nestc/src/sema/lower.rs` | `lower_intrinsic` — where a call to an `#intrinsic` def becomes `ExprKind::Intrinsic`. |
| `nestc/src/sema/collect.rs` | `check_bodyless` (the three ways a signature may stand alone) and the `#lang`-on-import tag. |
| `nestc/src/sema/imports.rs` / `mod.rs` | `RawImport.lang`, and `wire_order` — the dependency-ordered wiring. |
| `nestc/src/sema/builtins.rs` | The one place primitive operator impls live. **Phase 5's family impl extends or replaces this.** |
| `packages/core/mem.nest` | The intrinsic declarations, with the `make` caveat written down. |
| `packages/core/slice.nest` | `impl <T, const N: usize> [N]T` — the precedent phase 5 copies, with two parameters instead of one. |

## Code context

**The surface forms this session added or changed:**

```nest
{ Add } :: import <core/ops>        // the prelude no longer globs all of core
a + b                               // still needs no import: `Add` is found by tag

@public cast :: #intrinsic("cast") func <T, U> (x: U) -> T   // core/mem.nest
cast.<u8>(n)                        // `T` pinned, `U` inferred
cast(n)                             // `T` from context
panic("unreachable")                // -> never, so it may end a block owing a value
len(xs)                             // { len } :: import <core/slice>

assert(size_of.<i32>() == 4)        // a comptime item; no sigil, no `::`

pick :: func <const B: bool> (a: i32, b: i32) -> i32 { if B { return a }  return b }
pick.<true>(1, 2)
sep  :: func <const C: char> () -> char { return C }
sep.<','>()
neg  :: func <const N: i8> () -> i8 { return N }
neg.<-3>()
```

**The types:**

```rust
// sema/ty.rs
pub enum Const {
    Value(Box<ConstArg>),   // a value AND the type it was written at
    Param(DefId),
    Var(ConstVar),
    Error,
}
pub struct ConstArg { pub ty: Ty, pub value: ConstValue }
impl Const {
    pub fn known(ty: Ty, value: ConstValue) -> Const;
    pub fn len(n: u64) -> Const;          // `usize`-typed; an array length
    pub fn value(&self) -> Option<u64>;   // None for a bool/char/float
}
impl Ty { pub fn is_primitive(&self) -> bool; }   // Int | Float | Bool | Char

// sema/intrinsics.rs
pub enum Special { MutableArg, SequenceArg }
pub struct IntrinsicRow { pub tag: &'static str, pub special: Option<Special> }
pub const INTRINSICS: &[IntrinsicRow];
pub fn lookup(tag: &str) -> Option<&'static IntrinsicRow>;

// sema/def.rs
impl Def { pub fn intrinsic_tag(&self) -> Option<Symbol>; }  // arg, else the name

// sema/imports.rs
pub struct RawImport { …, pub lang: Option<Symbol>, … }       // `#lang` on an import
```

**The non-obvious bits.**

*An intrinsic leaves no trace in inference.* `f.<T>(x)` on an `#intrinsic` def is
type-checked exactly like any generic call; only `Inferer::intrinsic_result`
runs afterwards, and only for `make` and `len`. If you are tempted to add a third
case there, check whether the *signature* can say it instead — that is the whole
point of declaring them in `core`.

*The IR's intrinsic namespace is not core's.* `ExprKind::Intrinsic` also carries
compiler-internal operations lowering invents (`array`, `repeat`, `slice`,
`full`, `format`), which have no `core` declaration. The registry maps a
declaration's tag into that namespace; it does not enumerate it.

*A `const` parameter takes part in type checking, not just in substitution.*
`[N]u32` is a type, so `N` reaches unification through `Const` — a mismatch is
reported in the **lengths** (``expected `[4]u32`, found `[3]u32```) rather than
in `N`, and two distinct parameters never unify. In value position `N` has the
type it was declared with, so a `<const B: bool>` read where a `usize` is wanted
is an ordinary mismatch.

*A `const` parameter's type is checked against its slot at every use.*
`const_of_def` compares the parameter's declared type with the slot's `want` and
refuses a mismatch. This is the only place a symbolic `Const::Param` can be
checked at all — its *value* is not known until monomorphization.

## Resume instructions

1. `cd nestc && cargo test` — expect **363 passed**.
   - If ``cannot load package `core` ``: `packages/core/` is missing or
     `NEST_CORE` is stale. The default is
     `{CARGO_MANIFEST_DIR}/../packages/core/core.nest`.
2. See this session's work end to end:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
   { Add } :: import <core/ops>
   { len } :: import <core/slice>
   V :: struct { n: i32 }
   impl Add for V { Output :: V  add :: func (self: V, rhs: V) -> V { return self } }
   sum   :: func (a: i32, b: i32) -> i32 { return a + b }
   count :: func (xs: []i32) -> usize { return len(xs) }
   small :: func <const N: u8> () -> u8 { return N }
   pick  :: func <const B: bool> (a: i32, b: i32) -> i32 { if B { return a }  return b }
   main  :: func () { const x := cast.<u8>(1)  const y := pick.<true>(1, 2) }
   EOF
   ./target/debug/nestc /tmp/e.nest | grep -E '^func|\$'
   ```
   - Expected: no diagnostics; `$cast(...)` in `main`'s body.
3. Read `design/roadmap.md` §5, then start **phase 5 (the integer family
   `int.<N, S>`)**. It is the largest phase in the plan and the one with the most
   ways to be subtly wrong; the three named risks are `usize` collapsing into
   `u64`, const-generic recursion, and display churn.
4. Whatever you touch, verify with all three:
   - `cargo test` (363 and rising)
   - `for f in ../examples/*.nest; do ./target/debug/nestc "$f" >/dev/null || echo "FAIL $f"; done`
   - `cargo clippy` — 84 warnings is the baseline; add none.

## Edge cases and known limits

Everything in the previous handoff's list still holds except where noted. New or
changed:

- **A `..` spread needs the type named.** `.{ x: 5, ..rest }` is refused; write
  `P { x: 5, ..rest }`. Lifting this means moving the expansion after inference,
  which needs a synthetic local and so a mutable `DefTable` in lowering.
- **`Location` is not in the prelude**, so declaring a `#caller_location`
  parameter costs `{ Location } :: import <core/loc>`. `panic` needs no import
  because it carries the parameter itself.
- **`make` still takes only a length**, and `#static` composes with other
  directives but none of them (a link section, an offset) is implemented yet.
- **`assert` is not in the prelude.** §6.4 puts only `cast`, `panic` and
  `size_of` there, so a compile-time assertion needs
  `{ assert } :: import <core/fail>` — including the struct-body form §6.10
  shows without one. Worth revisiting if it grates.
- **`core`'s `.!` bodies panic with a fixed string.** There is no formatting, so
  `Result.unwrap` cannot report the error value it had.
- **`make` takes only a length.** Spec §6.9 has `make.<[]T>(len, cap)`; the
  declaration is one-argument until default arguments on a bodyless `#intrinsic`
  are checked.
- **`size_of.<T>()` still loses `T` in the IR.** `ExprKind::Intrinsic` carries no
  type arguments, exactly as before — the result type is stamped on the node and
  `usize` says nothing about `T`. Phase 7 (layout) is where that has to change.
- **A `const` generic parameter on a *type* declaration is still rejected**
  (`collect.rs`): a nominal type's identity is `(def, type-args)` with no slot
  for a value. Unchanged by phase 4.
- **An integer literal does not fill a `char` slot.** `f.<0x61>()` on a
  `<const C: char>` is refused; write `'a'`.
- **Array lengths still do not go through the const evaluator.** `[SIZE * 2]T` is
  rejected. The fix is a `Const::Unevaluated(DefId)` resolved post-link, and it
  belongs with phase 6. **Do not** bolt a second evaluator onto the AST. (The
  user has marked this deferrable in `design/roadmap.md`'s open questions.)
- **`examples/packages/use_packages.nest` does not compile from the CLI** —
  ``unknown package `shapes` ``. Not a regression: that example needs packages
  registered programmatically, which only `tests.rs` does.
- **A generic `#const` function's calls are still not checked.** Phase 6.
- **`#static` inside a function parses, resolves and type-checks, but is not
  lowered as a global.**
- **Exhaustiveness over floats, strings, byte strings and `comptime_int`** is
  never complete except by a wildcard.

## Warnings

- **Run everything from `nestc/`.** `packages/` and `examples/` are one level up.
  A `cd` inside a compound Bash command silently changes the working directory
  for later calls — use absolute paths.
- **`cargo test` does not rebuild `target/debug/nestc`.** Run `cargo build`
  before the examples loop or you are testing a stale binary.
- **`rustfmt --edition 2024 <file>` follows `mod` declarations**, and reformats
  the whole of any file you pass. Check `git diff --stat` afterwards and revert
  what you did not edit. **Never** run `cargo fmt` with no arguments.
- **Regenerating snapshots**: `INSTA_UPDATE=always cargo test`, then
  `rm -f src/*/snapshots/*.snap.new` and
  `sed -i '' '/^assertion_line: /d' src/*/snapshots/*.snap`. Then **read every
  diff** — diff against `git`, not against `.snap.new`.
- **Many tests assert *exactly one* diagnostic.** Deliberate: the regression
  guard against cascades. Fix the cascade rather than loosening the assertion.
- **`messages()` returns errors only; `warnings()` returns warnings.**
- **`#lang` discovery is by tag only.** Never hardcode a core type's name, file
  or position. `core_is_an_ordinary_multi_file_package` and
  `the_prelude_is_found_by_tag_not_by_name_or_path` both exist to fail if you do.
- **A new intrinsic is a row in `sema/intrinsics.rs` first.** The `core`
  declaration is rejected until the tag is known.
- **`is_value_rhs` must stay in step with `collect::def_kind_of`.**
- **Nest syntax worth restating**: `f :: func () { … }`, not `func f() { … }`;
  `let x: T := v`; primitives are `u32`/`i32`; an array literal is
  `[_]T { a, b }`; match arms are comma-separated.
- **Comments explain *why*, at length, and cite spec sections inline.**
- **Cloned default arguments share their `IrId`s with the original.**

## User notes

- Commits are **title-only**: no body, no co-author trailer, changes bundled as
  `feat: a + feat: b + fix: c`.
- The user answered three of `design/roadmap.md`'s open questions in place:
  `PTR_BITS` comes from the target and exists only inside the compiler; array
  lengths through the const evaluator may be deferred; the `str` → `String`
  conversion is decided after codegen works.
