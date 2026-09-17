# Handoff: the seven bugs are fixed; next is `opaque`, `.nmeta`, then `@test`

**Generated**: 2026-09-18
**Branch**: `main`
**Last commit**: `7933984` (the seven fixes), plus one more for the justfile.
**Status**: All seven bugs the fuzzing pass found are **fixed, each with a
regression test in the suite**. Nothing from that list is left. What is in
flight is the `justfile` — written, **never executed**, because `just` is not
installed on this machine (see Warnings). Everything below it is not started.
**Tests**:
- `cd nestc && rtk proxy cargo test -p nestc` gives **653** (was 651).
- `cd nestc && rtk proxy cargo test -p nest-lsp` gives **30**.
- `cd editors/tree-sitter-nest && npx tree-sitter test` gives 7.

## What To Do Next, In Order

### 1. Verify the `justfile` (start here — it is the one unverified thing)

`build.sh` is **deleted** and replaced by `justfile` at the repository root. The
recipes are a faithful translation of what `build.sh` did, plus the test half
the user asked for, but **not one of them has been run**: `just` is not
installed here, so even the syntax is unchecked.

```
brew install just
just --list          # parses the file at all
just tools           # the tool checks, nothing built
just build           # nestc, nest-lsp, twig
just test            # every suite
just profile=debug build
```

What it provides: `bootstrap` (a fresh machine, twig compiled by nestc then by
itself), `build` (incremental, bootstraps twig if there is none), `test` (nestc,
nest-lsp, the grammar corpus, and twig — the last a stub that says so until
`@test` exists), plus `build-nestc`, `build-lsp`, `build-twig`, `test-nestc`,
`test-lsp`, `test-grammar`, `test-twig`, `tools`, `fmt`, `fmt-check`, `clean`.

Things to look at closely when you run it:
- The two `export ... := env_var_or_default(..., ```…```)` blocks that find
  bdw-gc and LLVM 21. Multi-line backticks and the `case` in the `llvm-config`
  loop are the parts most likely to be wrong.
- `test-twig` calls `twig test --help` to decide whether twig has tests yet.
  That will start returning true once `@test` lands, which is deliberate.
- `clean`'s `find … -exec rm -rf {} +` — check it deletes only package `build/`
  directories.

The user also asked for **`cargo fmt` as its own commit at the end**. It has not
been run. `just fmt` is the recipe. Do it last, alone, so the formatting diff
never sits on top of a change anyone has to read.

### 2. `opaque` (the design questions are answered; nothing is built)

A type with **no size and no values**, usable only behind a pointer, for the FFI
cases where the pointee is not ours to describe: `FILE`, `sqlite3`, a handle a C
library hands back.

**The user has decided all three open questions.** Do not re-ask them:

- **One built-in type**, not a declaration form. Their words: *"One builtin, as
  distinct opaque solves the declaration possibility."* So `opaque` is a
  primitive, and a library that wants a nominal handle writes
  `Handle :: distinct opaque` — the existing `distinct` already mints a fresh
  nominal type, so there is nothing new to design for that case. Check early
  that `distinct` over a sizeless type works, since every other `distinct` today
  stands over something with a layout.
- **`cast` in both directions.** `*T` ↔ `*opaque` always needs an explicit
  `cast`; there is no implicit conversion into or out of it. The one implicit
  move is the ordinary mutability one, `*mut opaque` → `*opaque`.
- `c.void` is an alias for it in `std/c`, which is the name C programmers will
  look for.

Still to settle while building it:
- What is refused: `let x: opaque`, a field of it, `size_of`/`align_of`,
  `new.<opaque>()`, dereferencing `*opaque`, `make.<[]opaque>`. Each of these
  wants a diagnostic that says *why* rather than a layout failure deeper in.
- Where it lives in the spec: §3 as a type, §11 for the FFI rules. §11 says a C
  pointer is not nullable, and that stays true.
- `Ty` has no case for it. Adding one is the same shape of change as
  `Ty::Struct` was this session (below): the compiler names every exhaustive
  match for you, and there were only three.

### 3. `.nmeta` should carry IR, not AST (the user's correction)

From the user, verbatim in the previous handoff: *"nmeta format was supposed to
save IR, not AST, and the design/ says it saves AST. The plan was for nmeta to
save analysis and type data, and nlib to save the IR with that metadata, so it
can compile with skipping much of unnecessary work, just like in rust."*

So `design/library.md` describes the wrong thing, and the format follows the
document. `.nmeta` should hold the analysis and type data; `.nlib` should hold
the IR alongside it, so a dependent package skips re-analysis the way rustc's
metadata lets it. Read `design/library.md` first — it is one commit old
(`4186079`) and is otherwise accurate about the container.

Nothing here was touched this session. A serialized-struct change needs a bump
of `library::FORMAT` and a delete of every `build/` directory afterwards (`just
clean` now does the second half).

### 4. `@test`, then twig's own tests

Unchanged and not started. `@test` on a function marks it a test; `twig` runs a
package's tests. The five questions to answer before designing it:

1. What a test *is*: `func () -> void` that fails by trapping, or also
   `func () -> Result.<void, str>`?
2. What `@test` is: a fixed attribute the compiler reads (like `@public`), or a
   `@attribute` struct in `core` found by a `#lang` tag?
3. How tests run: one process per test (`<bin> --run <name>`), or the runtime
   catching the trap?
4. Where tests live: anywhere in the package, compiled only for `twig test`, or
   a `tests/` directory? Do they see private members?
5. Output: cargo's shape (`test foo ... ok`, a summary, non-zero exit)? A filter?

A likely shape: `nestc --test` compiles the entry as a test binary, collecting
the entry package's `@test` functions and synthesizing a `main` the way
`entry=auto` already synthesizes one (`lir/entry.rs`); `twig test [filter]`
builds through `build::command` and runs each test in its own process. Write the
design into `design/toolchain.md` first. `just test-twig` is already wired to
call `twig test` the moment it works.

## The Seven Bugs — What Each Fix Actually Was

Each has a test that fails without it. The run-and-check-the-status ones are
entries in the table in `codegen/llvm/tests.rs::a_program_links_and_runs`.

### 1. Anonymous struct types (spec §3.8)

There was **no case for them in `Ty` at all**, which is why every position
failed. Added `Ty::Struct(Vec<(Symbol, Ty)>)`, fields kept **sorted by name** by
`Ty::anon_struct` — §3.8 makes the identity the field *set*, so sorting is what
makes the derived `PartialEq` agree and fixes one layout order for the two
spellings at once.

Reaching all of §3.8 took, beyond the variant: `ty_from_node_in` handling
`StructType` in a type position (`anon_struct_ty`, which refuses generics and
the tuple form); `field_ty` reading fields off the type; `check_record_body`
accepting a target with no def; `ExprKind::Construct`'s `def` becoming
`Option<DefId>`; layout, LIR flattening and the symbol mangling (`X` … `E`).

Two pieces are worth knowing about because they are not obvious:

- **`.{ ... }` with no context defaults to an anonymous struct**, in
  `default_anon_structs`, run when the obligation sweep stalls. Only a *named*
  body defaults — a positional or repeat body is an array or a tuple and the
  context was the thing that would have said which.
- **It is pinned eagerly at an un-annotated `let`.** Without that,
  `let a := .{ x: 1 }` followed by `let p: P := a` solved `a` to `P` outright
  and the anonymous value the spec's own example describes never existed.

`try_anon_to_named` is the implicit coercion; the named→anonymous `cast` is a
`Projection::Cast` in LIR — same bytes, same offsets, no instruction.

### 2. Importing a name a namespace does not publish

Root cause was **wider than the bug report**: `bind_field` fell back to an
`external` stand-in for *any* name it could not find, so importing anything
nonexistent was silently accepted and only surfaced as "an error type reached
code generation" from the use site. The stand-in is now only for a namespace
that failed to *load*; a namespace that loaded and has no such member is
reported where the import is written. `impl_member_owner` names the `impl` the
method actually lives in, so `wrapping_sub` says `<impl int>` rather than
leaving the reader guessing.

This found a real stale import in the suite (`gc_collect` from `core/mem`).

### 3. Writing through two levels of indexing

`infer_index_place` read the base's type before its `Index.Output` projection
had solved, so a nested index looked like a variable, the array case did not
recognize it, and the write went to `IndexMut` — which the built-in sequences
deliberately do not implement. One `self.settle(&bty)` before the check.

### 4. A bounded type parameter → its trait object

`try_dyn_coerce` searched for an *impl*, and a type parameter has none. The
bound is the promise that stands in for one; `param_has_bound` is the test, and
monomorphization builds the vtable per instantiation. A parameter bounded by the
*wrong* trait is still a plain type mismatch.

### 5. `T.Item` through a type parameter (§5.4)

The largest of the seven, and the design is the thing to understand before
touching it again.

**A bound's associated types become type parameters of the function.**
`introduce_bound_projections` (in `resolve`) walks each generic parameter's
bounds and mints one synthetic `DefKind::TypeParam` per (parameter, associated
type), as a member of the parameter's own namespace — so `T.Item` resolves
through the ordinary member hop with no special case. `Def::projection` records
the equation `<base as trait>.assoc` that solves it, and `instantiate` registers
that as an ordinary projection obligation once the call site has a `T`.

Supporting pieces:
- `Def::assoc_bounds` — a trait's abstract associated type records what its own
  bounds resolved to, because the file *using* a trait has no access to the
  syntax tree of the file that wrote it. It doubles as the test for "is this an
  abstract associated type".
- **Nested projections work**: `project_bounds` recurses, so `Item :: type:
  Holder` gives `T.Item` an `Item` of its own and `N.Inner.Item` resolves. Capped
  at depth 4, since a trait whose associated type is bounded by itself is a legal
  declaration and an infinite family of names.
- `close_over_projections` extends the generics list with every associated-type
  parameter reachable from it, **in both places that build that list**
  (`stamp_generics` and `instantiate`) — a call site lines up with a declaration
  *by position*, so the two must not drift. `N.Inner` appears nowhere in `deep`'s
  signature, only in its body, and without the closure monomorphization had no
  binding for it.
- `self_assoc_ty` keeps `Self.Item` written inside its own trait as the
  associated def rather than a fresh variable, so `subst_trait_self` has
  something to rewrite. That in turn is why the impl-conformance check had to
  learn to substitute the impl's `Item :: i32` — a variable cannot be
  substituted into, and without it every signature mentioning one disagreed.
- `Projection::pinned` is `<T: Holder.<Item = i32>>`, which has an answer without
  waiting for a call site, and needs it *inside* the generic body.

A bound is written `TypePath { generic_args }` in a generic list and
`GenericApply` in expression position. `assoc_binding` handles both; that cost
an hour.

### 6. A repeat aggregate

`.{ v; N }` over an array built `vec![v; n]` — one operand per element, carried
through every later stage, so the cost was the *compiler's* and grew without
bound. Now:

- A repeated value with a **uniform byte pattern** is a `memset`. Uniform means
  zero (at any element size) or a one-byte element; anything else would need the
  target's byte order to decide, and the loop is correct without asking.
- Anything else is a **loop** (`fill_loop`), a fixed amount of LIR whatever `N`.
- Under `REPEAT_UNROLL` (32) the old element-wise form stays: it folds, and the
  backend emits it whole.

Measured: a 200 000-element repeat of a non-constant value went from **13.6 s to
0.15 s**. A million-element one compiles at all now.

Two things came out of this that were not in the bug report:

- **Copying an aggregate is a `memcpy`** (`codegen/llvm/unit.rs`, the `assign`
  arm). It was a `load`/`store` of the whole thing, which asks LLVM to build a
  first-class value of every element. This is what made the fix look like it had
  not worked.
- **`core/mem` now declares `copy_nonoverlapping` and `set_bytes`** (`#intrinsic`
  `memcpy` / `memset`), over **slices** rather than a pointer and a count, so the
  byte count is derived from a length the value already carries. `std/mem`'s
  `copy` uses the first; `std/mem` gained `zero`; `Vec`'s `from_slice`, `reserve`
  and `extend` no longer copy element by element, and `Vec` gained `fill` and
  `zero`.

Note on naming: the spec calls the growable array `Vector` (§3.9) but `std` calls
it **`Vec`**. The user asked for "vector should have a fill method"; `Vec.fill`
is what was added. One of the two names should give.

### 7. Deep recursion

**The user chose the stack probe, not "the platform's business".** Every
function that can reach itself now compares its own frame address against
`nest_stack_floor` — a global the runtime writes once at startup from
`getrlimit(RLIMIT_STACK)`, less a 64 KiB margin for the handler itself. A zero
floor disables the check, so a program whose limit could not be read still runs.

`lir/recursion.rs` decides who gets one: Tarjan's SCCs over the call graph, so
**mutual** recursion is covered, plus a self-edge, plus any function that calls
through a pointer (nothing to draw an edge to, so no bound can be claimed). The
SCC search is iterative on purpose — a compiler that overflowed its own stack
looking for recursion would be a poor joke.

`f(100000)` now prints `nest: stack overflow` and aborts; `f(1000)` is untouched.

## Key Decisions (follow these unless the user changes them)

- **`opaque` is one built-in type**; `distinct opaque` is how a library gets a
  nominal handle. `*T` ↔ `*opaque` needs an explicit `cast` both ways.
- **Only infinite recursive *types* error**; deep type nesting must not. It
  already does not — a 60-level struct chain compiles, and `R :: struct { r: R }`
  reports "recursive type has no size". This was checked, not assumed.
- **`cargo fmt` is now allowed**, as a separate commit at the end. The old
  standing rule against it no longer applies.

## Warnings

- **The `justfile` has never been run.** `just` is not installed. See item 1.
- **Standing rules**:
  - Don't edit existing comments unless they're now false.
  - Commits are title-only (`feat: a + fix: b`), with **no body, no co-author and
    no session trailer**.
  - Ask questions in batches, and ask when unsure.
  - Simplify LIR while working on codegen if possible, and pass this rule on in
    every handoff.
- **Don't push** without asking.
- Boehm is required: `brew install bdw-gc`, or `BDW_GC_PREFIX`.
- The runtime (`runtime/nest_runtime.c`) changed this session — it now needs
  `<stdint.h>` and `<sys/resource.h>`, and exports `nest_stack_floor` and
  `nest_stack_overflow`. A stale runtime archive will fail to link against the
  probe.
- Every new side-table type needs registering in `library/metas.rs` (and must be
  `Send`). A serialized struct change needs a bump of `library::FORMAT`; after
  one, delete the `build/` directories (`just clean`).
- `nestc` for twig and `nest-lsp` must be built from the same source.
- Grammar edits: run `npx tree-sitter generate`, re-parse the corpus, re-copy
  `highlights.scm`, commit, then bump `rev` in `editors/zed/extension.toml`.
- Language gotchas: a line starting with `.` continues the previous one, ranges
  are `..<` and `..=`, a binding is `:=` and not `=`, `return`/`break` in a match
  arm need a block, there is no `++`, and an associated type in an impl is
  `Item :: i32` — **not** `Item :: type = i32`, which is a parse error.
- `nestc/lsp/Cargo.toml` has a `[profile.release]` block that cargo ignores:
  profiles only count in the workspace root manifest.

## Failed Approaches (Don't Repeat These)

- **For §5**: registering the projection obligation while *reading* the
  signature. The signature is definition-relative and the call site substitutes
  it afterwards, so the obligation captured the unsubstituted `T` and failed
  with "`T` does not implement `Holder`". The answer has to be part of the type
  so substitution reaches it — hence the synthetic parameter.
- **For §5**: a `Ty::Projection` variant normalized in monomorphization. Mono
  has the `ImplTable` but an impl's associated type is a *syntax node*, and
  turning one into a `Ty` is the Inferer's job. It cannot be done from there.
- **For §6**: fixing only the LIR and stopping. The element-wise aggregate was
  half of it; the whole-array `load`/`store` in the LLVM backend was the other
  half, and measuring before checking the backend made the first fix look inert.
- From the previous session, still true: driving the language server with
  **incremental** `didChange` ranges (it advertises full synchronization);
  measuring re-analysis by CPU alone (use `NEST_LSP_LOG`); `git stash -- <path>`
  on a committed file to test whether a fix is load-bearing (it stashes nothing
  and the check silently passes); and an escape-analysis test with no `drop` in
  its LIR, which proves nothing unless run under `NEST_GC_POISON=1`.

## Completed This Session

- [x] `7933984` — the seven bugs, each with a regression test. 651 → 653 tests
      (two new named tests; the rest are rows in the run-and-check-status table).
- [x] `core/mem`'s `copy_nonoverlapping` and `set_bytes`; `std/mem`'s `zero`;
      `Vec.fill` and `Vec.zero`; the element-wise loops in `Vec` replaced.
- [x] Aggregate copies lower to `memcpy`; uniform repeats to `memset`.
- [x] `runtime/nest_runtime.c` records a stack floor and reports an overflow.
- [x] `justfile` replaces `build.sh` — **written, not verified**.

## User Notes

- ~~Fix the bugs found by the hunt~~: all seven, done.
- ~~Remove `build.sh`, move it to a justfile with build, bootstrap and test~~:
  written; **run it**.
- Add `opaque` (decisions above; `c.void` as an alias).
- Change `.nlib` / `.nmeta` so metadata is IR and analysis data, not AST.
- Design `@test`, then build tests for twig on top of it.
- `cargo fmt` at the very end, as its own commit.
- LSP tests should keep covering odd editor scenarios.
