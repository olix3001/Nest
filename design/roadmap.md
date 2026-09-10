# Roadmap — what gets built, in what order, and why that order

**Status**: the plan of record. `HANDOFF.md` is the *state* (what is done, what
broke, what to run); this file is the *plan* (what is next, and what each step
involves). When they disagree, the handoff is right about the past and this file
is right about the future.

Phases 0 and 1 are **complete**: the front end parses, resolves, infers, lowers
to IR, validates the IR, and evaluates constants. 334 tests pass. Everything
below is unbuilt.

## The order, at a glance

| # | Phase | Depends on | Size | Why here |
|---|---|---|---|---|
| 2 | The prelude split | — | small | Self-contained, and every later phase adds names that have to land on one side of it |
| 3 | `#intrinsic`, and retiring `$name` | 2 | medium | Cheaper before core grows; every later phase declares intrinsics |
| 4 | `const` generics over any primitive | — | medium | Prerequisite for 5, useful alone |
| 5 | The integer family `int.<N, S>` | 3, 4 | large | The deepest type-system change; everything after it is easier |
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

## Phase 2 — The prelude split

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

---

## Phase 3 — `#intrinsic`, and retiring `$name`

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

---

## Phase 4 — `const` generics over any primitive type

**Goal.** `const N: Ty` accepts any primitive `Ty`, not only `usize`. Spec §5
already states this.

**Why it is here.** Phase 5 needs `const S: bool`. It is also independently
useful and can be tested on its own, which is why it is a separate phase rather
than the first half of phase 5.

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

---

## Phase 5 — The integer family `int.<N, S>`

**Goal.** Integers are one generic family: `int.<N, S>` with `N: usize` the width
and `S: bool` the signedness. `i32`, `u8`, `usize` remain as sugar. Spec §3.1
already states this.

**Why it exists.** So that the operations on integers can be written **once**, in
`core`, as an inherent impl over the family:

```nest
impl <const N: usize, const S: bool> int.<N, S> {
  wrapping_add :: #intrinsic("wrapping_add") func (self: Self, rhs: Self) -> Self
}
```

Per-width impls cannot do this: `u4096` is a legal type, so there is no finite
list. This is the whole reason the phase exists — `.wrapping_add` is not
reachable any other way.

**Work.**

1. **`Ty::Int`.** `{ signed: bool, width: IntWidth }` becomes a form carrying two
   `Const`s. `IntWidth::Fixed(u16)` and `IntWidth::Ptr` collapse into the width
   `Const`, with a new opaque case for pointer-sized: `usize` is
   `int.<PTR_BITS, false>`, where `PTR_BITS` is target-supplied and **opaque to
   type identity**, so `usize` and `u64` stay different types on a 64-bit target.
   Layout resolves it; the type system never does.
2. **Resolution.** `int` is a builtin generic type constructor in type position.
   `primitive_ty("i32")` returns the same type `int.<32, true>` builds, so the
   two spellings are one type with no conversion.
3. **Display.** Print the sugar when both arguments are literal (`i32`), the
   generic form when either is symbolic (`int.<N, S>` inside the impl, `usize`
   for the pointer case). Diagnostics get worse in exactly the places this is
   done carelessly.
4. **Every `Ty::Int { .. }` match site.** Roughly forty, all compiler-guided.
   `int_fits`, `int_truncate`, `exhaustive::int_bounds` and the numeric-distinct
   machinery each read the width through the `Const` instead of an enum.
5. **Impl indexing and selection.** `impls::build` accepts a family self type;
   method lookup on a concrete integer selects it with `N` and `S` bound. The
   `[N]T` impl already proves the shape works — this is the same thing with two
   parameters instead of one.
6. **`core/num.nest`.** The family impl, with `wrapping_*`, `checked_*`,
   `saturating_*`, `count_ones`, `leading_zeros`, and the rest, each
   `#intrinsic`.

**Risks.** The largest phase in the plan, and the one with the most ways to be
subtly wrong:

- **`usize` collapsing into `u64`.** If `PTR_BITS` is ever folded to a number in
  the type system, `impl usize` and `impl u64` collide and a target change
  silently alters type identity. The opacity is the design; do not "simplify" it.
- **Const-generic recursion.** `int.<N, S>` is a type whose arguments are
  constants, and constants have types, which are integers. The evaluator must not
  need `int.<32, true>`'s definition to evaluate `32`. Keep the width a plain
  `usize` value, not an `int.<…>`-typed one.
- **Display churn.** Every snapshot with an integer type in it changes if the
  sugar rule is wrong. Get rule 3 right before regenerating anything.

**Done when.** `x.wrapping_add(y)` works for `u8`, `i64` *and* `u7`; `i32` and
`int.<32, true>` are the same type; `usize` is not `u64`; snapshots show the
sugar.

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
| Whether `PTR_BITS` is spelled in the source, or only exists inside the compiler | the user | phase 5, cosmetically |
| Array lengths through the const evaluator (`[SIZE * 2]T`) | design | phase 6 — a `Const::Unevaluated(DefId)` resolved post-link is the shape; **do not** bolt a second evaluator onto the AST |
| The `str` → `String` / `[]T` → `Vec.<T>` conversion: a `From`-style trait or a `#lang` coercion | the user | `std`, which does not exist yet |
| `#when` (conditional compilation) | the user | nothing yet; not in the parser, spec, or grammar |
