# Roadmap — what gets built, in what order, and why that order

**Status**: the plan of record. `HANDOFF.md` is the *state* (what is done, what
broke, what to run); this file is the *plan* (what is next, and what each step
involves). When they disagree, the handoff is right about the past and this file
is right about the future.

Phases 0 through 9 are **complete**: the front end parses, resolves, infers,
lowers to IR, validates the IR and evaluates constants; monomorphization,
layout, and the LIR lowering turn that into a concrete control-flow graph with
its drops and safepoints placed. **532 tests pass.** Phase 10 — codegen and the
real driver — is what is left.

## The order, at a glance

| # | Phase | Depends on | Size | Why here |
|---|---|---|---|---|
| 2 | The prelude split ✅ | — | small | Self-contained, and every later phase adds names that have to land on one side of it |
| 3 | `#intrinsic`, and retiring `$name` ✅ | 2 | medium | Cheaper before core grows; every later phase declares intrinsics |
| 4 | `const` generics over any primitive ✅ | — | medium | Prerequisite for 5, useful alone |
| 5 | The integer families `int.<N>` / `uint.<N>` ✅ | 3, 4 | large | The deepest type-system change; everything after it is easier |
| 6 | Monomorphization ✅ | 5 | large | Was the old "Phase 2"; 5 changes what it must substitute |
| 7 | Layout ✅ | 6 | medium | A generic type has no layout until its arguments are known |
| 8 | LIR: shape and control flow ✅ | 7 | large | `design/lir.md` §1–4 |
| 9 | LIR: defer, drops, safepoints ✅ | 8 | large | `design/lir.md` §3, 5, 6 |
| 9b | LIR made ordinary, and codegen units ✅ | 9 | large | `design/lir.md` §7b, §10, §11 — every special case a backend would have to learn |
| 10 | Codegen and the real driver | 9b | large | The first executable |

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

## Phase 5 — The integer families `int.<N>` / `uint.<N>` ✅ **done** (`6248d22`, `13ffe2d`, `cb85833`, `bac8491`)

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

### `usize` into `core` (`bac8491`)

`usize` and `isize` are no longer primitives. They are `core` declarations:

```nest
usize :: #lang("usize") distinct uint.<PTR_BITS>
isize :: #lang("isize") distinct int.<PTR_BITS>
```

`PTR_BITS` comes from `core/target.nest`, which the **compiler generates** from
`Options` and which also supplies `OS`, `ARCH` and `PROFILE` as values of `Os` /
`Arch` / `Profile` — enums written by hand in `core/os.nest`. The generated file
is a *member of core* rather than a package of its own, precisely so it can name
those enums: the coupling is then between two files of one package instead of
between the compiler and a library's vocabulary.

Four things made it work:

1. **A width is a bare `u16`** (`Const::Width`), not a `Const::Value` at type
   `u16`. A `Value` carries the `Ty` it was written at, and that `Ty` would be
   `u16` — itself `uint.<16>`, whose width carries a `u16`, without end. Bare is
   what makes the representation finite, and what lets `usize` be defined in
   terms of a width at all.
2. `Ty::usize()` can no longer be built context-free, so the two types are
   injected into `InferCtxt` by `#lang` tag exactly as `str` already was
   (`set_ptr_int_tys`), and `Const::len` takes the type as an argument.
3. `numeric_distincts` **reads** `distinct uint.<PTR_BITS>` — one literal or one
   hop to a constant — so core's declaration is real rather than decorative.
4. **`Self` is rebound for an inherited method.** See below; this is the one
   that mattered most.

**The target no longer enters the type system at all.** `Target` was threaded
through `Inferer`, `ConstEval` and the exhaustiveness checker purely to resolve a
pointer width; with the width arriving as an ordinary constant through `core`,
every one of those fields became dead and was removed. Layout (phase 7) will want
the target again, but the *type* layer no longer does.

### `Self` rebinding, which had been missing all along

A `distinct D :: T` inherits `T`'s methods, and those methods are written in terms
of `T`. Before this, `d.dup()` on a `func (self: Self) -> Self` returned **`T`** —
the distinction evaporated on the first inherited call. Only the *builtin
operator rows* avoided it, by computing `Output` from the self type as written,
which is why `Meters + Meters -> Meters` worked while a real trait impl did not.

`Inferer::infer_method_call_rebound` fixes it: the signature is instantiated and
the `self` parameter unified **in the representation's terms** — that is what
solves a generic representation's own parameters, as when `str` inherits from
`impl <T> []T` — and only then is every occurrence of the representation replaced
by the distinct type. The callee keeps its real signature in the IR, because the
function being called really is the one declared over the representation.

This is what gives `usize` `wrapping_add` returning a `usize` with **no impl of
its own**, and it is a fix to `distinct` in general, not to integers.

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
✅; `usize` is declared in `core` ✅. **376 tests pass.**

**Still open, and deliberately not done**: moving `+`, `-` and the rest out of
`sema::builtins` into real `Add` / `Sub` trait impls in `core` with `#intrinsic`
bodies. It was proposed as the way to make operators return `Self` for `usize` —
but the `Self` rebinding above already does that, so the move is now pure
cleanup rather than a correctness fix. Its cost is not small: floats are not a
family, so it is ~50 impl blocks, plus a selection path that rebinds `Self` for
obligations (a different path from method calls) and a lowering change to keep
`i32 + i32` a primitive `Binary` rather than a call.

---

## Phase 6 — Monomorphization ✅ **done**

**Goal.** A concrete program: every generic function instantiated per distinct
argument set, every instantiation named. This was the old "Phase 2"; phase 5
moved ahead of it because it changes what has to be substituted.

**Done when.** No `Dispatch::Generic` survives; every function has a symbol; the
deferred const checks run. All three hold.

### What is built

The pass is `nestc/src/ir/mono.rs`, and it runs after the `ir::check` passes —
deliberately, so that a mistake inside `func <T>` is reported once, against the
function as it was written, rather than once per instantiation.

- **Arguments are read, not re-derived.** Inference already works out what each
  call site instantiates its callee with; it now records that as an
  `Instantiation` on the call, against a `Generics` list stamped on the callee.
  The pair is built by one piece of code (`instantiate_parts`), so the order
  cannot drift. Re-deriving the arguments by unifying signatures would be a
  second implementation free to disagree, and it is *wrong* wherever the
  signature does not mention a parameter — `func <T> () -> usize { return
  $size_of.<T>() }` has no `T` in `func() -> usize`.
- **`Generics` records the split** between the parameters a declaration lists
  itself and the ones it inherits from its enclosing `impl`. A method is generic
  over `impl <T> Vec.<T>`'s `T` without declaring it, and the two halves come
  from different places when a bound is resolved.
- **Instantiation is a renumbering.** The body is cloned with fresh `IrId`s and
  the facts hung off the old ones copied across with their types substituted;
  ids cannot be shared, because the one fact that differs between two
  instantiations is the per-node type and that is keyed by id. A **local's
  `DefId` is not** renumbered: two instantiations share one declaration per
  local, which is what a local's def is.
- **`Dispatch::Generic` is resolved** by matching the now-concrete self type
  against each candidate impl's target, which is a one-way match (holes are the
  impl's generics) rather than a unification. The targets are resolved out of
  syntax once, after inference, into `infer::ImplTarget`, because `ty_from_node`
  belongs to inference and monomorphization has none of that machinery.
- **A `*dyn Trait` coercion reaches the impls its vtable will hold.** Nothing
  else in the program names them, so without it the vtable would have holes.
- **Symbols** follow `design/lir.md` §7, which this phase amended in two places
  to make the scheme actually injective (an `N` before a nominal path, a sign
  letter before a numeric `const` value).
- **Both carried to-dos are done.** `check::constness::check_only` re-runs over
  exactly the new instantiations, and the const **evaluator** binds a `<const
  N>` argument from the call's `Instantiation` — which it does without waiting
  for this pass, because a constant's value is wanted *during* inference and
  this pass runs long afterwards.

### Fixed afterwards

Four things the first cut of this phase got wrong or left out, all with tests:

- **A computed array length** (`[SIZE * 2]T`) now works. §3.2 says a length is a
  compile-time constant, and an expression over constants is one. It is folded
  during inference — where a length is wanted — by the **same** arithmetic the
  const evaluator runs on the IR: `ir::const_eval::binary_values` and its
  neighbours are free functions over `ConstValue`s with no tree behind them, and
  the two walks share them. A *call* still cannot stand there, and that is
  stated rather than worked around: a body is not compiled until its types are
  known, and this is inference asking.
- **A generic with no finite set of instantiations** (`grow.<T>` calling
  `grow.<Box.<T>>`) used to run the compiler out of stack. It is now reported,
  once per declaration, against a depth budget.
- **A bound's trait arguments** are carried to monomorphization.
  `impl Conv.<i32> for Vec3` and `impl Conv.<bool> for Vec3` are coherent (§4.9)
  and both apply to `Vec3`; with only the trait to go on, selection picked one
  silently.
- **Two impls of one trait for one type shared a symbol.** Their members have
  the same name and the same canonical path, so the implemented trait is now
  part of the symbol (`_NC4Vec3XN4ConvIi32E2to`) and of the name
  (`Vec3.<as Conv.<i32>>.to`).

### What it does not do

- **Dead-code elimination.** An unreached concrete function is still emitted and
  still gets a symbol; dropping it is a decision about the artifact being built,
  not about types. That is also why the walk's roots are every *concrete*
  function rather than `main`: since nothing is dropped, everything emitted must
  still have the callees it names, and a generic declaration does not survive the
  pass. When dead-code elimination arrives the set narrows by itself.
- **Cross-compilation-unit generics.** A generic declaration is not a root: which
  instantiations of it exist is a question about its callers, and for a `@public`
  generic in a library, about a consumer this compilation cannot see.
- **Cross-file instantiation sharing.** Two files asking for `id.<i32>` produce
  one function, because the key is the symbol; two *compilations* do not.

---

## Phase 7 — Layout ✅ **done**

**Goal.** Sizes, alignments and offsets, per monomorphized type. Consumes
`#packed`, `#align(N)`, `#soa`, and resolves the pointer width from the target.
Recursive-layout checking already existed (`616cd3b`).

**Done when.** Every concrete type has a size and an alignment, every aggregate
has field offsets, and `size_of` / `align_of` are constants.

### What is built

`nestc/src/ir/layout.rs` is the computation and `nestc/src/ir/check/layouts.rs`
is the pass that asks it about every type a program declares. The rules — and
they are *rules*, not derivations, since a backend has to agree with them — are
written down in `design/lir.md` §7cc.

- **A query, not a pass.** Layout is asked about a concrete `Ty` and memoizes
  the answer, keyed by the type's mangled encoding (`mono::type_key`), which is
  the right key for the same reason it is the right key for an instantiation: its
  one job is injectivity. "Every type" is not a set anyone can enumerate — `[N]T`
  for every `N`, every tuple, every instantiation — so each arrives when
  something needs it.
- **Type definitions stay definition-relative.** A field of `Pair.<T>` is a `T`,
  as it was after lowering; substituting the use site's arguments is layout's
  job. That is what keeps one `Pair` in the program instead of one per
  instantiation, and it is why monomorphization instantiates *functions* and not
  types.
- **`#packed`, `#align(N)` on a type and on a field.** A member now carries its
  own directives into the IR, which it did not before — `#align(8)` on a field
  was silently ignored.
- **`size_of` / `align_of` fold to constants**, including inside a generic
  `#const` function: the type travels on the call's `Instantiation` (the
  signature `func <T> () -> usize` mentions `T` nowhere), and the evaluator keeps
  a type frame beside its value frame so a nested call resolves `T` against where
  it came from.
- **The target comes back here and only here.** A *pointer's* width is not
  written anywhere in a type — `usize`'s is, since phase 5 — so `layout` asks the
  target and everything else, the const evaluator included, asks `layout`.
- **Layouts show in the IR dump** (`}  // size 12, align 4`), which is what the
  snapshots now assert.

### What it does not do

- **`#soa`.** It is recognized, checked for where it may be written, and
  **warned about** rather than silently ignored. What it waits on is not layout
  arithmetic: storing a `[N]Particle` column-wise means `&a[i]` no longer names a
  contiguous `Particle`, so a place projection through it is a different
  operation — and what a place projection *is* belongs to the LIR lowering
  (§1), which is phase 8.
- **Reporting.** The pass stamps and does not complain, because every concrete
  type it cannot lay out has already been reported by the check that owns the
  question (an unsized member by §3.4's rule, a cycle by `recursive_layouts`, an
  errored member by inference). A second diagnostic for one mistake is the thing
  the diagnostic discipline here is arranged to avoid.
- **ABI classification.** How a struct is *passed* — in registers, on the stack,
  by hidden pointer — is a different question from how it is stored, and it
  belongs with the C FFI work (§11.3) and codegen.

## Phase 8 — LIR: shape and control flow ✅ **done**

`design/lir.md` §1, 2, 4, 7, 7b, 7c: basic blocks, places, terminators, the
`match` decision tree, mangled names, flattened aggregates, debug info. The
overflow setting becomes real here — `overflow=trap` is a checked instruction and
a panic edge, which is why it is lowering's decision and not codegen's (§7d).

### What is built

`nestc/src/lir/mod.rs` is the representation, `lower.rs` the IR → LIR walk, and
`pretty.rs` the dump §1 writes. A function is locals declared up front, basic
blocks, one terminator each.

- **Control flow becomes a graph.** `if` is a switch on a `bool`, `loop` is a
  back edge, `break` / `continue` / `return` are jumps, `&&` and `||` are edges
  rather than operations, and a call to a `-> never` function is an ordinary
  instruction followed by `unreachable` (§2 — a panic does not unwind, so a call
  never needs two successors).
- **`match` is a decision tree** (§4). The discriminant is read **once**; one
  switch chooses the variant and each group projects only the payload its arm
  named. A guard failure falls through to the next *arm*, which is why the
  candidates in a group are a chain. A catch-all arm joins every group, and one
  with a guard sends the whole match down the linear route — its failure would
  mean a different next arm in each group, and a test that means different things
  in different places is not one test.
- **Places are paths** (§1): a local (or a `#static`) plus field, index, deref
  and variant-downcast projections, printed by name. Everything that is not an
  lvalue gets a slot, so `(a + b).x` needs no special case downstream.
- **Pointer arithmetic exists here and nowhere above** (§7b). The source
  language has none on purpose; a slice flattened into `{ ptr, len }` has no
  element to project, so reaching one is `s.ptr + i * stride(T)` — an `offset`
  rvalue, in elements, carrying the element type rather than a byte stride.
- **Aggregates are flattened to structs** (§7b) and arrays are not. The type
  table is the closure of what the lowered program mentions — a tuple, an enum
  (`{ tag, payload }`), a slice (`{ ptr, len }`), a `distinct`, and a `*dyn
  Trait` (`{ data, vtable }`) — each with its layout and its members' offsets,
  and each remembering what it was, because a debugger showing `2` instead of
  `.green` is a worse debugger.
- **Vtables are constants** (§7b). Which function fills which slot is recorded by
  **monomorphization** (`mono::VtableSlots`, stamped on the `*T` → `*dyn Trait`
  coercion), because the instantiated method that fills a slot does not exist
  until that pass makes it. A `dyn` call is two projections and an indirect call.
- **`defer` is placed** (§3), as ordinary blocks on a cleanup ladder, one rung
  per kind of exit rather than per exit site — which is why `return` writes a
  slot rather than returning directly.
- **`overflow=trap` is an edge** (§7d): a checked operation, a switch on the
  flag, and a block that panics and does not come back. Integers reached through
  a `distinct` count — `usize` is one (§3.1).
- **Bounds checks are the same shape** (§3.2): a comparison against the length,
  an edge, a block that panics. `#unsafe` turns them off (§9), and an index the
  evaluator proved in range needs none. The other half is `check::bounds`, which
  refuses `a[7]` on a `[3]i32` where it is written — a fixed array's length is in
  its type, so when the index is known too the answer cannot change.
- **Intrinsics are gone as calls** (§9): `size_of` / `align_of` are the numbers
  layout computed, `cast` is a cast carrying both types, `len` is a constant or a
  member, `index` is a projection or pointer arithmetic, an array literal is an
  aggregate.
- **Everything a debugger needs is carried** (§7c): a span on every statement, a
  source name on every local that had one, both names on every function.

### What it does not do

- **Drops (§5) and GC safepoints (§6).** Phase 9. Both ride the ladder §3 now
  builds, which is why that section came with this phase rather than after it —
  a lowering that dropped `defer` bodies would produce wrong programs, and the
  ladder is what the next two passes attach to.
- **ABI classification and register allocation.** Codegen's, with §11.3.
- **A slice literal's storage.** `[]T { a, b }` still reaches LIR as the
  composite intrinsic: where its elements live is an allocation question, not a
  shape one.

## Phase 9 — LIR: defer, drops, safepoints ✅ **done**

`design/lir.md` §3, 5, 6: `defer` bodies placed once per exit path (which came
with phase 8, because a lowering that dropped them was a wrong program that
looked right), explicit drops, GC safepoints with their live-pointer sets, and
the `reloc` discipline.

### What is built

- **Escape analysis** (`nestc/src/lir/escape.rs`), intra-procedural and
  deliberately blunt: an allocation escapes if it is returned, stored anywhere
  reachable from outside, or **passed to any call**. It runs on the IR **tree**,
  because a scope is lexical and once control flow is a graph there are no scopes
  left to ask about — the ladder is what remains of them. The answer is handed to
  the lowering, which registers a `drop` the way it registers a `defer`.
- **`drop` on the ladder** (§5), after the `defer` bodies on the same rung: a
  `defer` may still read the object, and the memory has to survive until it has.
- **Safepoints** (`nestc/src/lir/safepoint.rs`), at the three places §6 names — a
  call, an allocation, a loop's back edge — each carrying the set of roots live
  **before** the statement, because collection happens while it is running and
  the destination has not been written yet.
- **`live` is one list, not two.** It is the root set and the `reloc`
  redefinitions at once, since with a moving collector every root holds a
  different address afterwards; two copies of that fact are two things that can
  disagree.
- **Real liveness**, a backward dataflow to a fixed point, rather than "every
  pointer in the frame". §6 is explicit that precision is not only a performance
  question: an over-approximate live set relocates objects nothing will read.
- **Back edges without dominators**: a DFS with the path marked, which is the
  same answer dominators give on the graphs this lowering produces.

### What it does not do

- **Per-function escape summaries.** A change of *precision*; the drop machinery
  does not move.
- **`Drop` (the trait).** The per-type form of cleanup is still unbuilt; §5's
  drops are the compiler's own, for memory it proved local.
- **The object-start table** interior pointers need (§6). That is the collector's
  to build, and there is no collector yet.

### Fixed alongside it

- **No `$panic` in LIR.** `panic` is an ordinary function in `core` found by
  `#lang("panic")`, and the compiler's own failures call it; the only intrinsic
  left is `trap`, one machine instruction. The handler behind it is
  `#lang("panic_handler")`, and a program may **replace** it — a `#lang` tag
  claimed outside `core` now wins over `core`'s claim of the same tag, which is
  what makes `core`'s answers defaults rather than fixed points.
- **No `comptime_int` in LIR.** The `$cast` out of a literal folds into the
  literal's definition; `comptime_int` is a type no backend has a register for.
- **No `discriminant` operation.** An enum is `{ tag, payload }`, so reading the
  tag is an ordinary member read. §4's "read once" is a property of the decision
  tree, not of the instruction set.

## Phase 9b — LIR made ordinary, and cut into codegen units ✅ **done**

The special cases LIR still had, removed — the A–N list the previous handoff
planned — and the split that makes code generation parallelizable.

### What is built

- **A vtable is a global** (`design/lir.md` §7b): one struct type per *trait*,
  one immutable global per *impl*, and a dispatch that is an ordinary member read
  at an offset the type table knows. The table, the id type, the constant form
  and the aggregate kind it used to have are all gone.
- **A blob constant is a global too.** A string's bytes, a byte string's and a
  folded aggregate are data with an address; an operand is a scalar, an address
  or `undef`, and an invariant test says so.
- **One arithmetic rvalue.** `Op { op, ty, args }` covers unary, binary, checked
  and wrapping alike; checked arithmetic is its own opcode rather than a flag
  that changes the result type.
- **One call statement.** A symbol, a pointer and an intrinsic are three
  *callees*; the intrinsic set is an **enum**, so a backend's match is
  exhaustive and a row added upstream with no case here fails a test.
- **A type is LIR's own** (`lir::Ty`), with `Named(TypeId)` indexing the unit's
  own table. No `DefId` survives lowering: a unit answers every question it
  raises, which is what makes it a thing another process could compile.
- **A variant is a type.** An enum's payload is read as the variant's own struct,
  so a member's offset comes from the table instead of from a rule.
- **Mutability and `void` are erased.** No target has two kinds of address, and a
  slot that holds nothing is a slot no machine has.
- **Directives become decided attributes**: the section, the inline hint, the
  offset, whether the symbol is public.
- **Codegen units** (§11, `nestc/src/lir/unit.rs`): one unit per source file,
  merged smallest-first until there are at most `-C codegen-units=N` (default 1),
  each carrying a declaration for every function it calls and every global it
  reads.

### What it does not do

- **Emit anything.** The split is the shape codegen will consume; there is no
  backend yet, so `-C codegen-units` buys nothing but a well-tested boundary.
- **Optimize across units**, which is the cost of the split and the reason the
  default is one.
- **Answer the `Drop` trait, per-function escape summaries, or the object-start
  table** — the same three §5 and §6 left open.

## Phase 10 — Codegen and the real driver

The first executable. Also where `nestc`'s scaffold CLI is replaced: `-C` stays
the interface for build settings (`nestc/src/common/options.rs`), because a build
tool translating a profile should keep talking to the compiler the same way.

**`design/lir.md` §10 is the brief.** It lists the whole instruction set a
backend answers for — three statements, three callees, four terminators, six
rvalues, twenty-four opcodes and thirteen intrinsics — and the four things a
backend genuinely does itself (emit the data section, turn safepoints into stack
maps, classify the ABI, select and allocate). There is no longer a list of things
LIR reaches back into the compiler for: a unit is self-contained (§11).

The invariant tests in `lir::tests` assert the shape rather than describing it:
every place names a slot, block ids are dense, every index a unit holds resolves
inside it, no operation has an aggregate operand, no operand carries a blob, no
local is typed `void` or `never`, no place indexes a slice, every declared
intrinsic has a case, and every symbol is defined in exactly one unit at every
split — checked over every file in `examples/` at four settings of
`-C codegen-units`.

---

## Open questions, and who owns them

| Question | Owner | Blocks |
|---|---|---|
| What `overflow=` does in a **release** profile | the user | nothing — nestc takes it as input either way |
| Whether `PTR_BITS` is spelled in the source, or only exists inside the compiler | the user | phase 5, cosmetically, **USER NOTE:** It should be defined by the target info, exists only inside the compiler |
| ~~Array lengths through the const evaluator (`[SIZE * 2]T`)~~ — **built, differently; see below** | the user, to confirm | nothing |
| A call or `size_of` inside a *type* (`[double(4)]T`, `[size_of.<H>()]u8`) | design | nothing today; wants dependency-ordered analysis, which is a phase of its own |
| The `str` → `String` / `[]T` → `Vec.<T>` conversion: a `From`-style trait or a `#lang` coercion | the user | `std`, which does not exist yet; This decision is left for after the codegen works. |
| `#when` (conditional compilation) | the user | nothing yet; not in the parser, spec, or grammar |

**On `[SIZE * 2]T`.** The note here asked for a `Const::Unevaluated(DefId)`
resolved post-link, and said *do not bolt a second evaluator onto the AST*. What
was built is neither quite one nor quite the other, and the difference is worth
confirming.

`Const::Unevaluated` does not work, for a reason that is about types rather than
about effort: **a length is part of a type's identity** (§3.2), so `[SIZE * 2]T`
has to unify with `[8]T` *during* inference. An unevaluated length would unify
with nothing until after linking, so every such unification would have to defer
— and "these two types are the same" is the promise inference exists to check.

What was built instead keeps one implementation of the **semantics** and admits
two **walks**. `ir::const_eval::binary_values` / `int_binary` / `float_binary` /
`unary_op` are free functions over `ConstValue`s with no tree behind them; the IR
evaluator calls them, and so does inference's fold over the AST. There is no
second arithmetic — `SIZE * 2` cannot mean one thing in a type and another in a
value — but there are two tree walks, because the trees genuinely differ and one
runs before the other exists.

The part of the note that stands unchanged is the ordering limit: a **call** in a
type still cannot be evaluated, and is reported as such rather than worked
around. That is the row above.
