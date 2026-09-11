# Roadmap — what gets built, in what order, and why that order

**Status**: the plan of record. `HANDOFF.md` is the *state* (what is done, what
broke, what to run); this file is the *plan* (what is next, and what each step
involves). When they disagree, the handoff is right about the past and this file
is right about the future.

Phases 0 through 4 are **complete**: the front end parses, resolves, infers,
lowers to IR, validates the IR, evaluates constants, splits the prelude, declares
every intrinsic in `core`, and takes a `const` generic of any primitive type. A
round of language decisions on top of that — `#caller_location`, `Default`, the
binding syntax, trait conformance — is recorded below. Phase 5 built the integer
families and implicit widening on top of that; what remains of it is moving
`usize` into `core`. **371 tests pass.** Phase 6 and everything after it is
unbuilt.

## The order, at a glance

| # | Phase | Depends on | Size | Why here |
|---|---|---|---|---|
| 2 | The prelude split ✅ | — | small | Self-contained, and every later phase adds names that have to land on one side of it |
| 3 | `#intrinsic`, and retiring `$name` ✅ | 2 | medium | Cheaper before core grows; every later phase declares intrinsics |
| 4 | `const` generics over any primitive ✅ | — | medium | Prerequisite for 5, useful alone |
| 5 | The integer families `int.<N>` / `uint.<N>` | 3, 4 | large | The deepest type-system change; everything after it is easier |
| 6 | Monomorphization | 5 | large | Was the old "Phase 2"; 5 changes what it must substitute |
| 7 | Layout | 6 | medium | A generic type has no layout until its arguments are known |
| 8 | LIR: shape and control flow | 7 | large | `design/lir.md` §1–4 |
| 9 | LIR: defer, drops, safepoints | 8 | large | `design/lir.md` §3, 5, 6 |
| 10 | Codegen and the real driver | 9 | large | The first executable |

Phases 2, 3 and 4 are independent of each other in principle. The order above is
chosen so that each one lands on a tree the previous one has already tidied: 2
decides where a name goes, 3 adds a great many names, 4 and 5 add more.

Two rules hold for every phase below:

- **`cargo test`, the examples loop, and `cargo clippy` all stay green at every
  commit.** No phase is allowed to leave the tree red "until the next one".
- **The spec is part of the change, not a follow-up.** `spec/` already describes
  the end state of phases 2–5; a phase is done when the compiler agrees with what
  is already written there.

---

## Phase 2 — The prelude split ✅ **done** (`effcafa`)

**Goal.** `core.prelude` is globbed into every file; the rest of `core` needs an
explicit `import`. Spec §4.6 already states this.

**Why it is first.** It is the smallest phase, it touches nothing structural, and
it decides a question every later phase would otherwise have to answer again:
when phase 3 adds `cast`, `size_of` and `panic`, "is this in the prelude?" should
already have an answer.

**Work.**

1. `packages/core/prelude.nest` — a new file that re-exports the prelude set and
   nothing else:
   ```nest
   @public { Option } :: import "option.nest"
   @public { Result } :: import "result.nest"
   @public { ControlFlow } :: import "control.nest"
   @public { str } :: import "str.nest"
   ```
   It carries `#lang("prelude")` so the compiler finds it by tag, never by path.
2. `packages/core/core.nest` stops re-exporting everything; it re-exports the
   topic namespaces (`ops`, `cmp`, `iter`, …) so `import <core/ops>` resolves.
3. `sema::analyze` globs the namespace tagged `#lang("prelude")` rather than the
   core root's. One line, plus the tag lookup.
4. Everything that relied on the old whole-of-core glob gets an import: the
   examples, the tests that name `Add`/`Iterator`/`Ordering` directly, and the
   `core` files themselves.

**Risks.** The failure mode is a wall of `cannot resolve name` in tests, which is
mechanical to fix but noisy. `core_is_an_ordinary_multi_file_package` must keep
passing — the prelude is found by tag, so a `core` laid out differently still
works.

**Done when.** A program using `Option`, `Result` and `str` compiles with no
import; a program writing `impl Add.<Vec3> for Vec3` needs `import <core/ops>`;
the tag, not the path, is what the compiler looks for.

**What it actually took.** Three things the plan did not list:

- **`#lang` had to work on an `import` binding.** The prelude is a *namespace*
  assembled by re-export, so the binding that names it is the only thing a tag
  can sit on. The tag travels with the `RawImport` and is registered when the
  import is wired, because the target namespace is not known before that.
- **A `#lang` trait is selectable without importing its name.** `a + b` reaches
  `Add` by tag, and gating that on `import <core/ops>` would have made `+`
  require an import. The same rule covers the desugars that call a lang trait's
  method by name — `for` calls `.into_iter()` / `.next()`, `.?` calls
  `.branch()`. `in_scope_traits` no longer contains `Add`; `lang_traits` does.
- **Imports have to be wired in dependency order.** Walking `<core/ops>` looks up
  a *member* of the root, and that member is itself produced by re-export — so it
  does not exist until `core.nest` is wired. An arbitrary `HashMap` order found
  it about half the time. `wire_order` is a post-order DFS over the import graph,
  tolerant of the cycles `core` already has.

`core/str.nest` gets no namespace re-export from the root, and `panic.nest` was
renamed `fail.nest`: the root exports each topic file as a namespace, and a
namespace called `str` or `panic` beside the prelude's `str` *type* and `panic`
*function* would be one name meaning two things.

---

## Phase 3 — `#intrinsic`, and retiring `$name` ✅ **done** (`cead807`)

**Goal.** Every compiler-provided function is an ordinary bodyless declaration in
`core` marked `#intrinsic`. The `$` sigil is removed from the language. Spec §6.4
and §9 already describe this.

**Why it is here.** Every later phase declares intrinsics (phase 5 declares one
per integer operation), and converting them once, before there are many, is
cheaper than converting them twice.

**Work.**

1. **Parser / lexer.** Drop `$` from the identifier regex; delete the
   `intrinsic_call` production; accept `#intrinsic` as a directive on a `func`
   with no body.
2. **Collection / resolution.** A bodyless `func` at namespace or impl scope is
   an error unless it is `#intrinsic`, `extern`, or a trait requirement — the
   rule that today is silent (a bodyless namespace `func` currently parses and
   means nothing). `Resolution::Intrinsic` and `DefKind::Intrinsic` go away; an
   intrinsic resolves to an ordinary `DefId`.
3. **The registry.** One table mapping an intrinsic's identity to what the
   compiler does with it. Identity comes from the directive argument —
   `#intrinsic("size_of")` — not from the function's name or path, for the same
   reason `#lang` tags do not: core must stay renameable and replaceable. An
   unrecognized tag is an error at the declaration.
4. **Inference.** `IntrinsicResult` and the special-cased signatures disappear:
   an intrinsic call is an ordinary call against a declared signature. The
   exceptions that keep special handling are the ones whose *signature cannot say
   what they do*: `cast` (target type from context), `transmute`, and `panic`
   (`#caller_location`).
5. **`core`.** New `mem.nest` (`size_of`, `align_of`, `transmute`, `new`,
   `make`), `panic.nest`, and the `len` declaration `slice.nest` already needs.
   `prelude.nest` re-exports `cast`, `panic`, `size_of`.
6. **Const evaluation.** `ExprKind::Intrinsic { name }` keys on the registry tag
   rather than a bare string. `ImplicitCast` (already built) keeps its meaning:
   the compiler's own conversion must be exact, a written `cast` may lose.
7. **Everything that says `$`.** Core, examples, tests, snapshots, and the four
   spec files that still show `$` in a code block. The user chose to do this in
   one commit rather than leaving an alias.

**Risks.** This is the widest-touching phase by file count, and almost all of it
is mechanical. The one real design point is item 4: resist the urge to give every
intrinsic a special case in inference — three of them need one, the rest are
ordinary calls, and the whole point of the phase is that the signature is written
down in core where a reader can see it.

**Done when.** `$` is not a token; `packages/core` declares every intrinsic;
`nestc examples/*.nest` is green; the spec's code blocks match what compiles.

**What it actually took.** The plan's item 4 was **wrong**, and the user said so
before the work started: it claimed `cast`, `transmute` and `panic` need special
handling because their *signatures cannot say what they do*. All three are
perfectly ordinary declarations:

```nest
@public cast      :: #intrinsic("cast") func <T, U> (x: U) -> T
@public transmute :: #intrinsic("transmute") func <T, U> (x: U) -> T
@public panic     :: #caller_location #intrinsic("panic") func (msg: str) -> never
```

`cast.<u8>(n)` pins `T` and infers `U` because `T` is written first — ordinary
partial-turbofish inference, which already worked. `cast(n)` takes `T` from
context — ordinary return-position inference. `panic` diverging is `-> never` in
the signature, so `DIVERGING_INTRINSICS` is gone and *any* function returning
`never` now ends a block. `#caller_location` was already a directive.

What those three actually need is a **check** the signature cannot express, and
that is a different claim: `transmute`'s same-size rule (phase 7, once there is
layout), and `cast`'s legality rule. Neither is a signature exception.

The two genuine exceptions are elsewhere, and both are in
`sema::intrinsics::Special`:

- **`make.<[]T>(n)` yields `[]mut T`.** The *mutability* is what the allocation
  adds and no bound says "the same type, made mutable".
- **`len(x)` takes an array or a slice.** No bound means "one of the two built-in
  sequences".

Other departures from the plan:

- `#intrinsic` takes an **optional tag**: `#intrinsic("size_of")` names the
  intrinsic, a bare `#intrinsic` defaults it to the declared name. The spec's
  code blocks showed the bare form and the roadmap wanted the tag; both are
  accepted, and the tag is what the compiler keys on, so `core` stays renameable.
- Retiring `$` took the **comptime-item marker** with it. `$assert(...)` was how
  the parser knew a call could stand among a struct's fields or a trait's
  members. The shape decides it now: every declaration in those positions is
  `name :: rhs` (or `name: ty`), so an identifier followed by `(`, `.`, `.<` or
  `[` is a call. One token of lookahead — `Parser::at_comptime_item`.
- The IR still prints `$name` for an [`ExprKind::Intrinsic`]. The IR is a compiler
  artifact, not source: the sigil reads as "compiler primitive" there, and
  keeping it left 135 snapshot lines untouched.
- `$abort` had no spec entry and is gone; `core`'s two `.!` bodies call `panic`.

---

## Phase 4 — `const` generics over any primitive type ✅ **done** (`c99deac`)

**Goal.** `const N: Ty` accepts any primitive `Ty`, not only `usize`. Spec §5
already states this.

**Why it is here.** Phase 5 needs a `const` parameter at a non-`usize` type —
`int.<N>` takes a `u16` width. It is also independently useful and can be tested
on its own, which is why it is a separate phase rather than the first half of
phase 5.

(The original reason given here was `const S: bool`, for a one-family
`int.<N, S>`. Phase 5 split the signedness into two constructors instead — see
§5 — so `bool` is no longer the motivating case, though `<const B: bool>` still
works and is still tested.)

**Work.**

1. **`sema::ty::Const`.** `Const::Value(u64)` becomes a typed value — an integer
   at arbitrary precision, a `bool`, a `char`, a float — reusing
   `ir::const_eval::ConstValue` rather than inventing a second value type. The
   const evaluator already produces exactly these.
2. **Identity.** Two const arguments are equal when their *types and values* are
   equal: `3u8` and `3usize` are different arguments. This is the one place the
   change can silently do the wrong thing, so it gets its own tests.
3. **Declaration checking.** `check_const_param` verifies the declared type is a
   primitive and rejects an aggregate with the reason spec §5 gives.
4. **Inference.** `Const::Var` unification is unchanged in shape; it gains the
   type check. Defaulting does not apply — a const argument is never inferred
   from nothing.
5. **Array lengths** keep working: `[N]T` is the `usize` case of the same
   machinery.

**Risks.** Type identity is the load-bearing part. A `Const` comparison that
ignores the type would make `Foo.<3u8>` and `Foo.<3usize>` the same type, which
is wrong and would only show up in monomorphization, much later.

**Done when.** `func <const B: bool> ()` type-checks and instantiates, `3u8` and
`3usize` are different arguments, and every array-length test still passes.

**What it actually took.** `Const::Value(u64)` became
`Const::Value(Box<ConstArg>)`, a `ConstValue` **and** the `Ty` it was written at,
which cost `Const` its `Copy`. Two things the plan did not list:

- **The turbofish parser only accepted integer literals.** A `const` parameter
  may be a `bool`, a `char` or a float, so `parse_generic_arg` had to accept any
  primitive literal — and a leading `-`, which inside `.<...>` is part of the
  literal rather than an operator, because there is no const-expression
  arithmetic there.
- **An array length is a `usize`, and that is a property of the slot.** Spec §5's
  illustration wrote `func <const N: uint32> () -> [N]byte`, which under typed
  identity produces `expected [4]i32, found [4]i32` at the call site — the length
  argument is a `u32` and the annotation's is a `usize`. A `const` parameter
  standing in a length slot is now required to be `usize`, reported at the
  declaration, and the spec's illustration was corrected. The alternative — a
  silent coercion at the length slot — would have put a hole in the identity rule
  the phase exists to establish.

Range is checked where the argument is written: `small.<300>()` on a
`<const N: u8>` is refused with the arbitrary-precision literal still in hand,
the same rule §2.5 applies to a typed constant.

A `const` parameter is not only a value the body reads — it appears **in types**,
so it takes part in type checking everywhere a type does. That is pinned by
`a_const_parameter_in_a_type_is_checked_as_a_type`: `N` solved from a `[N]T`
argument and carried into the return type, an explicit `.<4>` contradicting the
call's own argument, a symbolic `[N]T` refusing a fixed-count literal, two
distinct parameters being two distinct lengths, `[N][M]T`, and two
instantiations in one expression. A mismatch is reported in the **lengths**
(``expected `[4]u32`, found `[3]u32```), never in `N`.

---

## Between 4 and 5 — language decisions, all built

Not a phase: four questions the user answered and the work that followed, in
`78d2b5e`..`4b041a3`. `HANDOFF.md` has the detail; the decisions are:

| Decision | Shape |
|---|---|
| `#caller_location` is an expression, legal only as a **default argument** | `func (loc: Location := #caller_location)`, `Location` a `#lang("location")` struct in `core/loc.nest` |
| No struct field defaults; `Default` and `..` instead | `P { x: 5, ..Default.default() }` and `.{ x: 5, ..rest }`; the temporary is bound in desugaring, the field reads expanded in lowering |
| A constant's type goes **before** the binder | `NAME: T :: value`, `#static NAME: T [:: value]`, `MAX: i32 [:: default]` — so `NAME :: type` is a type alias always |
| An impl's members are checked against the trait's **types**, not just their presence | `Inferer::check_impl_conformance` |

Two bugs fixed on the way: a struct-field default **hung the parser** (it had,
since before phase 2), and a function-local `#static` was never lowered as a
global.

---

## Phase 5 — The integer families `int.<N>` / `uint.<N>` 🟡 **mostly done**

**Goal.** Integers are two generic families: `int.<N>` (signed) and `uint.<N>`
(unsigned), with `N: u16` the width in bits. `i32`, `u8` are sugar. Spec §3.1.

**Why it exists.** So that the operations on integers can be written **once**, in
`core`, as an inherent impl over a whole family:

```nest
impl <const N: u16> int.<N> {
  wrapping_add :: #intrinsic("wrapping_add") func (self: Self, rhs: Self) -> Self
}
```

Per-width impls cannot do this: `u4096` is a legal type, so there is no finite
list. This is the whole reason the phase exists — `.wrapping_add` is not
reachable any other way.

### What is done (`6248d22`, `13ffe2d`, `cb85833`)

- [x] **`Ty::Int { signed: bool, width: Const }`.** `IntWidth` is gone.
      Unification solves the width through `unify_const`, so `uint.<N>` meeting
      `u8` learns `N = 8`; the signedness is compared, never solved.
- [x] **`int` / `uint` are builtin type constructors** in type position, and
      `int.<32>` *is* `i32` — the same `Ty`, not a conversion. `uint.<1>` is
      `bool`, matching `u1`.
- [x] **A family impl works.** `core/num.nest` has `wrapping_add` /
      `wrapping_sub` as `#intrinsic` members of `impl <const N: u16> int.<N>`
      and its `uint.<N>` twin, verified on `u8`, `i64`, `i7`, `u4096` and inside
      a caller's own `<const N>` generic.
- [x] **Display prints the sugar** when the width is known and the family form
      when it is not. Zero snapshot churn.
- [x] **Implicit widening** (§3.1): a narrower integer stands where a wider one
      is wanted, by range containment — signed→unsigned never, unsigned→signed
      only when strictly wider. A `const` parameter fills a slot on the same
      rule. Lowers to the existing exact implicit `$cast`.

### What is left — `usize` into `core`

The user's design, settled but **not landed**. WIP lives on the branch
`phase5-usize-in-core` (`90d57b1`), which does **not** build clean.

`usize` and `isize` stop being primitives and become `core` declarations:

```nest
usize :: distinct uint.<PTR_BITS>
isize :: distinct  int.<PTR_BITS>
```

with `PTR_BITS` coming from a compiler-generated `core/target.nest` that also
supplies `OS`, `ARCH` and `PROFILE` as values of `Os` / `Arch` / `Profile` —
enums declared by hand in `core/os.nest`. The generated file is a **member of
core** rather than a package of its own, so that it can name those enums: the
coupling is then between two files of one package instead of between the compiler
and a library's vocabulary.

Three things make it work, and one of them is not obvious:

1. **A width is a bare `u16`** (`Const::Width(u16)`), not a `Const::Value` at
   type `u16`. A `Value` carries the `Ty` it was written at, and that `Ty` for a
   width would be `u16` — itself `uint.<16>`, whose width carries a `u16`,
   without end. Holding the number bare is what makes the representation finite,
   and it is what lets `usize` be defined in terms of a width at all.
2. `Ty::usize()` can no longer be built context-free, so it is injected into
   `InferCtxt` by `#lang` tag exactly as `str` already is
   (`set_ptr_int_tys`), and `Const::len` takes the type as an argument.
3. `numeric_distincts` has to *read* `distinct uint.<PTR_BITS>` — one literal or
   one hop to a constant — or core's declaration would be decorative.

**The gaps that stopped it**, each independent and each real:

- **`Ty::is_primitive` is false for a nominal `distinct`**, so
  `impl <T, const N: usize> [N]T` in `slice.nest` is rejected with "a `const`
  generic parameter must have a primitive type". `check_const_param` has to
  resolve the numeric-distinct representation first.
- **The const evaluator cannot evaluate an enum variant**, so
  `OS: Os :: Os.Linux` in the generated file fails with "`Linux` has no
  compile-time value". This is the largest of the three and is not really about
  integers at all.
- **A width read from a constant** (`uint.<PTR_BITS>`) reports "an integer width
  must be a literal, a constant, or a `const` generic parameter" — an ordering
  problem between the import binding and `const_of_def`, not yet diagnosed.

**Risks** (the two that remain from the original three; display churn is
settled):

- **`usize` collapsing into `u64`.** It is a `distinct` now, so the protection is
  §2.4 nominal identity rather than an opaque width — but the invariant is the
  same and `a_pointer_sized_integer_neither_widens_nor_is_widened_into` is the
  guard.
- **Const-generic recursion.** See point 1 above: the bare width is the fix, and
  it must not be "simplified" back into a typed value.

**Done when.** `x.wrapping_add(y)` works for `u8`, `i64` *and* `u7` ✅; `i32` and
`int.<32>` are the same type ✅; snapshots show the sugar ✅; `usize` is not `u64`
✅ (as a primitive today, as a `distinct` once the above lands); `usize` is
declared in `core` ❌.

---

## Phase 6 — Monomorphization

**Goal.** A concrete program: every generic function instantiated per distinct
argument set, every instantiation named. This was the old "Phase 2"; phase 5
moved ahead of it because it changes what has to be substituted.

**Work.** Walk the call graph from the entry points; instantiate through
`Linked::insert`; key each instantiation by its arguments; assign the symbol per
`design/lir.md` §7. Two carried to-dos: a generic `#const` function's calls must
be **re-checked** once concrete (`Dispatch::Generic` defers today, in both
`check::constness` and the evaluator), and a `const` argument substituted into a
`#const` body turns a `ConstParam` into a value the evaluator can finally read.

**Done when.** No `Dispatch::Generic` survives; every function has a symbol; the
deferred const checks run.

---

## Phase 7 — Layout

Sizes, alignments and offsets, per monomorphized type. Consumes `#packed`,
`#align(N)`, `#soa`, and resolves `PTR_BITS` from the target. Recursive-layout
checking already exists (`616cd3b`).

## Phase 8 — LIR: shape and control flow

`design/lir.md` §1, 2, 4, 7, 7b, 7c: basic blocks, places, terminators, the
`match` decision tree, mangled names, flattened aggregates, debug info. The
overflow setting becomes real here — `overflow=trap` is a checked instruction and
a panic edge, which is why it is lowering's decision and not codegen's (§7d).

## Phase 9 — LIR: defer, drops, safepoints

`design/lir.md` §3, 5, 6: `defer` bodies placed once per exit path, explicit
drops, GC safepoints with their live-pointer sets, and the `reloc` discipline.

## Phase 10 — Codegen and the real driver

The first executable. Also where `nestc`'s scaffold CLI is replaced: `-C` stays
the interface for build settings (`nestc/src/common/options.rs`), because a build
tool translating a profile should keep talking to the compiler the same way.

---

## Open questions, and who owns them

| Question | Owner | Blocks |
|---|---|---|
| What `overflow=` does in a **release** profile | the user | nothing — nestc takes it as input either way |
| Whether `PTR_BITS` is spelled in the source, or only exists inside the compiler | the user | phase 5, cosmetically, **USER NOTE:** It should be defined by the target info, exists only inside the compiler |
| Array lengths through the const evaluator (`[SIZE * 2]T`) | design | phase 6 — a `Const::Unevaluated(DefId)` resolved post-link is the shape; **do not** bolt a second evaluator onto the AST; This step can be deferred for the future |
| The `str` → `String` / `[]T` → `Vec.<T>` conversion: a `From`-style trait or a `#lang` coercion | the user | `std`, which does not exist yet; This decision is left for after the codegen works. |
| `#when` (conditional compilation) | the user | nothing yet; not in the parser, spec, or grammar |
