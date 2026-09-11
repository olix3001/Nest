# Handoff: Phase 6 is done — monomorphization, symbols, and dispatch on a bound

**Generated**: 2026-09-11
**Branch**: `main`
**Status**: **392 tests pass**, `cargo clippy` reports 74 warnings (down from the
84 baseline; the set is the baseline minus the dead-code entries this phase made
live, plus one of the same kind in `mono.rs`). Every file in `examples/*.nest`
compiles.

**Read `design/roadmap.md` §6 first.** It is the plan of record and is now marked
done. This document is the *state* — what was decided, what is built, what will
bite you. **Phase 7 (layout) is next and is unblocked.**

## Goal

Phase 6: a concrete program. A generic function is not code — `func <T> (x: T)`
describes a family, and a machine can be handed only a member of it — so the
family has to become its members before anything below the IR can run.

## What is built (this session)

The pass is **`nestc/src/ir/mono.rs`**, run from `sema::analyze` after the
`ir::check` passes.

- [x] **One function per distinct argument set.** `id.<i32>`, `id.<bool>`,
      `id.<u8>` are three `Function`s with three `DefId`s. The generic
      declaration is **dropped** from `Linked` — there is nothing to emit for a
      family, and keeping it is where a surviving `Dispatch::Generic` would hide.
- [x] **Every function has a symbol**, mangled per `design/lir.md` §7 and stamped
      with the unmangled name beside it as an `Instance` meta fact.
- [x] **No `Dispatch::Generic` survives.** A call on a bound is resolved by
      matching the now-concrete self type against each candidate impl's target.
- [x] **Both carried to-dos are done.** `check::constness::check_only` re-runs
      over exactly the new instantiations; the const **evaluator** binds a
      `<const N>` argument from the call's `Instantiation`, so `twice.<4>()` in a
      `::` binding now evaluates to `8`.
- [x] **`--emit`-less dump**: the driver prints a `===< MONO >===` section, each
      function headed by `// <symbol> = <name>`. That is how everything below was
      eyeballed.

### Where the generic arguments come from (`Instantiation` / `Generics`)

This is the load-bearing decision of the phase.

Inference already works out what each call site instantiates its callee with —
that *is* what inferring a call is. It now **records** the answer:

- `Generics { params, own }` on a function's node: every parameter it is generic
  over, declared ones first, and how many of those it declared itself.
- `Instantiation(Vec<GenericArg>)` on a call's callee node: what this call bound
  them to, in the same order.

Both are built by `Inferer::instantiate_parts`, in one place, so the order cannot
drift. Lowering carries them into the IR; `mono` reads the pair back.

The alternative — unify the callee's declared signature against the type the call
settled on — is a second implementation free to disagree, **and it is wrong**
wherever the signature does not mention a parameter: `func <T> () -> usize {
return $size_of.<T>() }` has no `T` anywhere in `func() -> usize`.

It survives as a deliberate **narrow fallback** (`Mono::args_from_signature`) for
the one call shape that records nothing: an **operator**. `a + b` picks its impl
through trait selection rather than the generic-instantiation path, so nothing
along the way had an argument list to record — but the callee's type was
reconstructed by lowering from already-concrete operands, so matching the
declaration against it is sound *there*.

### The `own` split, and why it exists

`Generics::own` says how many parameters the declaration listed itself; the rest
are the enclosing `impl`'s. A method is generic over `impl <T> Vec.<T>`'s `T`
without declaring it.

The split matters in exactly one place and matters a lot there. When a call on a
bound becomes a call on the impl that satisfied it, the two halves come from
different places: the **method's own** arguments come from the call site (the
trait's declaration and the impl's must list the same ones, so they line up by
position), the **impl's** from matching the impl's target against the concrete
self type. Without the split there is no way to tell which is which.

It is also what makes `core.Vec.<i32>.push` mangle and print with the arguments
on the type rather than trailing off the end.

### Instantiating a body is a renumbering

`Mono::instantiate` clones the `Function` and walks it with a `VisitorMut`,
giving every node a **fresh `IrId`** and copying the facts hung off the old one —
span, type (substituted), directives, `ImplicitCast`, `RangeReported`,
`Instantiation` (substituted) — across.

- Ids **cannot** be shared: the one fact that differs between two instantiations
  is the per-node type, and that is keyed by id.
- A **local's `DefId` is not** renumbered. Two instantiations share one
  declaration per local, which is what a local's def is: the place it was
  written. Their types differ and live on the fresh ids.

### Resolving a call on a bound

`Dispatch::Generic { trait_def, method, self_ty }` → `Dispatch::Static`, with the
callee rewritten to the impl's method.

- Each impl's target is resolved out of syntax **once**, after inference, into
  `infer::ImplTarget` — `ty_from_node` belongs to inference and `mono` has none
  of that machinery. `Session` now holds `impls` and `impl_targets` side by side,
  indexed alike.
- Matching is **one-way** (`match_ty`): the impl's generics are the holes, the
  concrete type is the answer. A hole asked to be two different things fails,
  which is what keeps `impl <T> Pair.<T, T>` off `Pair.<i32, f64>`.
- A concrete impl beats a blanket one, as during inference.
- The self type is **autoderefed on the second try**: `Dispatch::Generic` records
  the `self` **parameter**'s type, so `d.weight()` on a `*D` arrives as
  `*Entity` while the impl is written `impl Describe for Entity`. Exact first, so
  an impl really written for a pointer still wins.

## Failed approaches (don't repeat these)

Everything in the previous handoffs' lists still stands. New this session:

- **Recovering a call's generic arguments by unifying signatures.** Wrong for any
  parameter the signature does not mention (`$size_of.<T>()`), and a second
  implementation of something inference computed once. Kept only as the operator
  fallback above, where both signatures are in hand and every argument is in
  them.
- **Rooting the walk at `main` (plus externs, plus a library's public surface).**
  Built first, and it produces a **broken program**: this pass does not eliminate
  dead code, so an unreached concrete `f` is still emitted — and if the walk
  never visited `f`, the generic `len` it calls was never instantiated *and was
  dropped*, leaving `f` naming a function that does not exist. Since nothing is
  eliminated, every function that will be emitted has to be a root. There is a
  regression test (`every_call_names_a_function_the_program_still_has`).
- **Mangling per `design/lir.md` §7 as written.** The scheme was not injective.
  An integer is a letter followed by digits and a path component *starts* with
  digits, so `i324core3Foo` is `i32`+`core.Foo` or `i324`+something; and
  `Ku167` is `u16` at `7` or `u167` at nothing. Fixed with an `N` before a
  nominal path and a `p`/`n` sign letter before every numeric `const` value —
  `design/lir.md` §7 is amended to match, with the reasoning.
- **Omitting a nominal's `I ... E` when it has no arguments.** Then
  `N4core6OptionN4core6Option` is one four-component path or two two-component
  ones. The empty list is still a list.
- **Keying instances by their arguments.** `Ty` has no `Hash`/`Eq` — a `const`
  argument may hold a float. Keying by the **symbol** is better anyway: a mangled
  name's one job is injectivity, so two argument sets share a symbol exactly when
  they are the same instantiation.
- **Mutating `Linked` and expecting the link tests to hold.** `link.rs` says
  monomorphization rewrites `Linked` and leaves the per-file programs alone —
  that is the intent — but `linking_merges_every_file_into_one_program` asserted
  on `session.linked` after `analyze`. It now measures `ir::link(&session.ir)`
  directly, which is what it was always about.
- **Running monomorphization before the `ir::check` passes.** Every one of them
  wants to report against the program as written: one mistake inside `func <T>`
  is one mistake, not one per call site.
- **Expecting `walk_pattern_mut` to reach a `Binding`.** It visits sub-*patterns*;
  the `Binding` inside `At` and inside a slice `rest` is not one and has its own
  `IrId`. `Cloner::visit_pattern` handles both by hand.

## Key decisions

| Decision | Rationale |
|---|---|
| A call site's generic arguments are **recorded**, not re-derived | Inference computed them once; a second derivation is free to disagree, and is wrong for a parameter the signature never mentions |
| `Generics` records `own` | The only way to tell a method's own arguments from its impl's when a bound is resolved — they come from different places |
| Instances are keyed by their **symbol** | A mangled name's one job is injectivity, so it *is* the key. Also sidesteps `Ty` having no `Hash` |
| A local's `DefId` is **not** renumbered per instance | Two instantiations share one declaration per local; that is what a local's def is. Only the type differs, and that is on the fresh id |
| The roots are every **concrete** function, not `main` | Nothing is dropped, so everything emitted must still have its callees, and a generic declaration does not survive. The set narrows by itself when DCE arrives |
| Monomorphization runs **after** the `ir::check` passes | A mistake inside a generic is one mistake, reported against the function as written |
| The generic declaration is removed from `Linked` | It is not code. Removing it is also what makes "no `Dispatch::Generic` survives" checkable |
| Impl targets are resolved into `Ty` once, after inference | `ty_from_node` belongs to inference; mono holds a concrete type and wants a match, not a unification |
| Impl matching is **one-way** | The right-hand side is settled; the only question is what the impl's generics would have to be |
| The const **evaluator** binds `<const N>` itself, not waiting for mono | A constant's value is needed *during* inference (an array length, a `const` argument); mono runs long afterwards |
| `usize` / `isize` mangle as `us` / `is` | They stand where a primitive stands and are in every other program; `N4core5usizeIE` in every symbol would be noise. Keyed on the `#lang` tag |

## Current state

**Working**: everything. `cd nestc && cargo test` → **392 passed**. `cargo
clippy` → 74 warnings. Every file in `examples/*.nest` compiles clean. The
roadmap, `design/lir.md` §7 and this document all describe what is actually
built.

**Broken**: nothing.

**Uncommitted changes**: none.

**Deliberately not done**:

- **Dead-code elimination.** See the roots decision above.
- **Cross-compilation-unit generics.** A generic declaration is not a root;
  which instantiations exist is a question about a consumer this compilation
  cannot see.
- **Two impls of one trait for one self type differing only in the trait's own
  arguments** (`impl Add.<f64> for Vec3` beside `impl Add.<i32> for Vec3`).
  `Dispatch::Generic` records the trait, not the arguments the bound was written
  with, so there is nothing to match them against. `ImplTarget` would grow a
  `trait_args` field on the day the IR carries them; it was built with one and
  it was removed, because an unused field is worse than a missing one.
- **A computed array length** (`[SIZE * 2]T`). Still rejected by
  `Inferer::const_value_in`, and still **not** a monomorphization question: the
  const evaluator runs on the IR and a length is wanted during inference, before
  there is any IR. Lowering one expression on demand, or an AST-level evaluator,
  is the shape of the fix. The previous handoff filed this under phase 6; it
  belongs to whichever phase decides to build one of those two things.
- **Moving `+`, `-` and the rest out of `sema::builtins` into real `Add` / `Sub`
  impls in `core`.** Unchanged from the last handoff, and still cleanup rather
  than a fix.

## Files to know

| File | Why it matters |
|---|---|
| `design/roadmap.md` §6, §7 | §6 is what was built; §7 is next. Read before starting. |
| `design/lir.md` §7, §7b, §7c | Names and mangling (amended this session), aggregate flattening, debug info. §7b/§7c are phase 8's brief and §7's neighbours. |
| `nestc/src/ir/mono.rs` | The whole phase. `run`, `roots`, `Mono::reach` / `instantiate` / `rewrite_call` / `select`, `match_ty`, `mangle`. |
| `nestc/src/sema/infer.rs` | `Generics`, `GenericArg`, `Instantiation`, `ImplTarget`, `instantiate_parts`, `stamp_generics`, `resolve_impl_targets`. |
| `nestc/src/ir/link.rs` | `Linked::insert` / `remove` — what mono adds and drops. |
| `nestc/src/ir/check/constness.rs` | `check_only`, the deferred re-check. |
| `nestc/src/ir/const_eval.rs` | `call_const_fn` binds `<const N>` from the call's `Instantiation`. |
| `nestc/src/sema/mod.rs` | `analyze`'s ordering, and `monomorphize`. |
| `nestc/src/common/options.rs` | `Target` (`pointer_bits`, `os`, `arch`) and `profile` — phase 7 wants these back. |

## Code context

```
$ nestc x.nest        # the tail of the output
===< MONO >===
// _NC4main = main
func main() -> void {
  let x: i32 = (Box.get: func(*Box.<i32>) -> i32)((&bi: Box.<i32>): *Box.<i32>): i32
  ...
}
// _NC3BoxIi32E3get = Box.<i32>.get
func get(self: *Box.<i32>) -> i32 #self(*) { ... }
// _NC3BoxIbE3get = Box.<bool>.get
func get(self: *Box.<bool>) -> bool #self(*) { ... }
// _NC6sum_itIN7CounterIEE = sum_it.<Counter>
func sum_it(s: *Counter) -> i32 {
  return (Counter.total: func(*Counter) -> i32)(s: *Counter): i32   // was Dispatch::Generic
}
// _NC6repeatIKu16p7E = repeat.<7>
func repeat() -> u16 { return 7: u16 }                              // was ExprKind::ConstParam
```

```rust
// ir/mono.rs
pub struct Instance { pub origin: DefId, pub args: Vec<GenericArg>, pub name: String, pub symbol: Symbol }
pub fn run(defs: &mut DefTable, meta: &Meta, linked: &mut Linked,
           impls: &ImplTable, targets: &[ImplTarget]) -> Vec<Diagnostic>;

// sema/infer.rs
pub struct Generics { pub params: Vec<DefId>, pub own: usize }
pub enum GenericArg { Ty(Ty), Const(Const) }
pub struct Instantiation(pub Vec<GenericArg>);
pub struct ImplTarget { pub self_ty: Ty }
```

**The non-obvious bits.**

*`Instance` is stamped on **every** function, concrete ones included*, with
`origin == def` and no arguments. A concrete function is its own only instance,
and saying so uniformly is what lets a consumer read it without first asking
whether there is one.

*The order of `Generics::params` and of `Instantiation`'s arguments is the same
order, and it is decided in one place.* If you touch `instantiate_parts`, touch
`stamp_generics` in the same edit — they walk the same signature with the same
two halves (declared, then `collect_generic_params` over the signature: types
then consts).

*A `reach_vtable` walk sorts the impl's members before queueing them.* A
`HashMap`'s iteration order varies between runs, and every queued job allocates a
`DefId` in the order it was queued — so without the sort two builds of one
program produce different def numbering.

*Mono does not run on a program that already reported an error.* Its IR describes
something that does not type-check, so the walk would at best find nothing and at
worst report a defect in the pass for a defect in the program.

## Resume instructions

1. `cd nestc && cargo test` — expect **392 passed**.
2. See the phase working:
   ```
   cargo build
   cat > /tmp/e.nest <<'EOF'
   Summing :: trait { total :: func (self: *Self) -> i32 }
   Counter :: struct { n: i32 }
   impl Summing for Counter { total :: func (self: *Counter) -> i32 { return self.n } }
   sum_it :: func <S: Summing> (s: *S) -> i32 { return s.total() }
   Box :: struct <T> { v: T }
   impl <T> Box.<T> { @public get :: func (self: *Box.<T>) -> T { return self.v } }
   repeat :: func <const N: u16> () -> u16 { return N }
   @public main :: func () {
     const c := Counter { n: 1 }
     const p := sum_it(&c)
     const bi := Box.<i32> { v: 1 }
     const bb := Box.<bool> { v: true }
     const x := bi.get()
     const y := bb.get()
     const r := repeat.<7>()
   }
   EOF
   ./target/debug/nestc /tmp/e.nest | sed -n '/MONO/,$p' | grep '^//'
   ```
   Expected: no diagnostics, and instances for `Box.<i32>.get`, `Box.<bool>.get`,
   `sum_it.<Counter>`, `repeat.<7>`, `Counter.total`.
3. **Phase 7 (layout) is next.** `design/roadmap.md` §7 is the plan: sizes,
   alignments and offsets per monomorphized type; `#packed`, `#align(N)`, `#soa`;
   and this is where the **target comes back** — `PTR_BITS` is a `core` constant
   for the type system, but a layout needs a real pointer width. Note phase 5
   removed `Target` from `Inferer` / `ConstEval` / the exhaustiveness checker on
   purpose; do not thread it back through those.
4. Whatever you touch, verify with all three:
   - `cargo test` (392 and rising)
   - `for f in ../examples/*.nest; do ./target/debug/nestc "$f" >/dev/null || echo "FAIL $f"; done`
   - `cargo clippy` — compare the warning **set**, not the count.

## Edge cases and known limits

Everything in the previous handoff's list still holds. New or changed:

- **An unreached concrete function is still emitted**, with a symbol, and is a
  root of the walk. See the roots decision.
- **A generic declaration is gone from `Linked` after this pass.** The per-file
  `Program`s still have it — they are the record of what lowering produced — so
  `--emit=ir` and the IR snapshot tests are unaffected.
- **`Dispatch::Virtual` is untouched.** Which function a vtable slot holds is a
  property of the vtable, and building one is IR → LIR's job (phase 8). The
  *impls* that fill the slots are reached, through the `*dyn` coercion.
- **A trait's own generic arguments do not take part in selection.** See
  "deliberately not done".
- **`match_const` compares widths by value**, so `int.<32>` and `i32` match: they
  are the same type (§3.1) and both normalize to a bare width.
- **A `const` argument that is not a primitive encodes as `Z`.** §5 makes an
  aggregate inadmissible, so reaching that is a defect; `Z` keeps the symbol
  injective among the values that do arrive rather than inventing one.
- **`mono.rs`'s `Instance::origin` / `args` show as "never read" in clippy.** The
  bin does not read them yet; the tests do. Same class as the several dozen
  pre-existing entries of that kind.

## Warnings

- **Run everything from `nestc/`.** `packages/` and `examples/` are one level up.
  A `cd` inside a compound Bash command silently changes the working directory
  for later calls — use absolute paths.
- **`cargo test` does not rebuild `target/debug/nestc`.** Run `cargo build`
  before the examples loop or you are testing a stale binary.
- **Compare the clippy warning *set*, not the count.** A new warning and a fixed
  one cancel out in a count. `cargo clippy --message-format=short 2>&1 | grep -E
  '^src|^warning: ' | sed 's/:[0-9]*:[0-9]*:/:/' | sort`, then `comm` against the
  baseline. Note the redirection: `2>&1 | grep`, **not** `2>&1 > file` — the
  second sends clippy's stderr to the terminal and leaves you an empty file and a
  clean-looking diff.
- **`cargo clippy` caches.** `touch` the file you changed, or you will compare
  against a stale run.
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
  or position.
- **Nest syntax worth restating**: `f :: func () { … }`, not `func f() { … }`;
  `let x: T := v`; a typed constant is `A: u8 :: 5` (the type goes **before** the
  binder); a directive is its own line above the binding (`#const\nf :: func …`);
  an `extern` function is `puts :: extern("c") func (…)`; an array literal is
  `[_]T { a, b }`; match arms are comma-separated.
- **Comments explain *why*, at length, and cite spec sections inline.**

## User notes

- Commits are **title-only**: no body, no co-author trailer, changes bundled as
  `feat: a + feat: b + fix: c`.
- The user redesigned the integer representation three times mid-implementation
  during phase 5. Expect the design to keep moving; commit green states often so
  a redesign costs one commit, not a session.
