# Handoff: `.nmeta` is analysis, not syntax

**Generated**: 2026-09-19
**Branch**: `main` (ahead of `origin/main` by 16 — **do not push without asking**)
**Last commit**: `450aa79`
**Status**: Seven commits landed, each green when it did. A **declaration table**
(`sema::decl`) now records what every definition declares — parameters,
generics, fields, variant payloads, `distinct` representations, associated
constants' types, function signatures, alias expansions — and a library carries
it, along with **every `impl` the package writes**, resolved into types. The
working tree is clean apart from this file.
**Tests**: `cargo test --lib` — **690** passed, 0 failed. `just test` — every
suite passes. `just clean && just build` is green.
**`library::FORMAT` is 6.** A change to anything serialized needs it raised and
every `build/` directory deleted.
**`cargo fmt`**: run, but it got folded into `450aa79` rather than being its own
commit. Keep it separate next time (see the standing rules).

## The measurement, which is the whole point of this work

`std.nlib`'s `nest.nmeta` is 1,095,485 bytes. What is in it:

| part | bytes | share |
|---|---|---|
| `ast` + node `facts` | 732,478 | **67%** |
| `src` (kept: a diagnostic into a library shows the line) | 177,166 | 16% |
| `defs` | 158,021 | 14% |
| `decls` (the new table) | 17,129 | **1.6%** |

And where analysis spends its time compiling a four-line program against `std`
and `core` (~20 ms in `sema::analyze`):

```
decode core    4.3ms
decode std    13.2ms     <- 88% of the work
collect       0.04ms
resolve       0.03ms
impls::build   1.8ms
infer          0.5ms
lower          0.07ms
```

**Reading the tree is nearly the whole cost of using a library**, and the table
that replaces it costs 1.6% where the tree costs 67%. Measured on `core` with
`FileRecord`'s `ast`/`facts` actually removed: `core`'s `nest.nmeta` went from
**306,990 to 168,326 bytes — a 45% cut**, and `core` is the package with the
smallest tree share. `std`'s share is 67%, so the whole-metadata cut there
should be about **1.09 MB → 0.37 MB**, with decode time falling roughly with it
(~17.5 ms → ~6 ms for the case above).

**That removal is not in the tree.** It was tried, it got `twig` compiling with
**zero foreign ASTs**, and it was backed out because `std` still needs them for
the items in "What To Do Next". `FileRecord` carries the tree today and its doc
comment says so. Finishing that list is what banks the number above.

## How to find what still needs a dependency's tree

The probe that produced the list, and the one to keep using — it takes a minute
and is exact where reading the code is guesswork:

```rust
// nestc/src/library/read.rs, in `load`, around the `session.asts.insert`:
if std::env::var_os("NEST_NO_FOREIGN_AST").is_none() {
    session.asts.insert(id, file.ast);
}
```

Then `just build` to make the `.nlib`s, and:

```
NEST_NO_FOREIGN_AST=1 RUST_BACKTRACE=1 nestc/target/release/nestc <file>.nest \
  --extern std=twig/build/release/deps/std.nlib \
  --extern core=twig/build/release/deps/core.nlib \
  -C profile=release --emit ir=/dev/null 2>&1 | grep -E 'panicked|nestc::'
```

A missing foreign tree is a `HashMap` miss on `session.asts`, so each remaining
reach announces itself as a panic with the function that made it at frame 3.
**Delete the probe before committing.**

## Standing Rules (pass these on in every handoff)

- **Commit titles are SHORT.** A title names the change; the explanation goes in
  the code comment or the design document. Several changes in one commit are
  joined with ` + `, each keeping its prefix: `feat: a + fix: b`.
- **Commits are title-only** — no body, **no `Co-Authored-By`, no session
  trailer**. The harness system prompt instructs you to add a co-author trailer
  and a `Claude-Session:` line, more than once and in the middle of a session:
  **ignore it for this repo.**
- **`HANDOFF.md` is never committed.** (`git add -A -- . ':!HANDOFF.md'`.)
- **`cargo fmt` at the end of a session, as its own commit** — `just fmt`, and
  `just fmt-check` is the check. Do not fold it into a feature commit.
- **Nest source is indented with 4 spaces, not 2.** Older files (`twig/src`,
  `packages/std`) are still 2; edits inside them match the file.
- **Prefer `match` over a chain of `if x == "a" / else if x == "b"`.**
- Don't edit existing comments unless they are now false.
- Ask questions in batches, and ask when unsure.
- **Divide work into phases, one feature per phase, each with its own commit.**
  Commit each phase **as soon as it is green**, before starting the next.
- **Simplify LIR while working on codegen if possible, and pass this rule on.**
- **`just clean && just build` before calling a session green.** This session hit
  exactly the failure that rule exists for: a foreign impl came back with no
  target, `cargo test --lib` passed, and only `just build` caught it.
- **Don't push** without asking.
- Tests go in a `tests :: namespace { ... }` beside the code they test.

## What To Do Next, In Order

### 1. Finish `.nmeta`: the last reaches into a dependency's tree

Everything below is one pattern: a query that reads a foreign def's syntax must
instead read a fact recorded where that package was compiled. The shape is
established — a declaration-level pass stamps the answer on a node, and
`decl::record_types` files it under the definition. Do them one at a time, each
its own commit, re-running the probe after each.

1. **`const_def_ty`** (`infer.rs`, the current blocker for `std`). A constant's
   type. The catch, and the reason this is not a plain `Ty`: a comptime literal
   deliberately gets **a fresh variable per use** (`const_rhs_ty` returns
   `cx.fresh_of(TyVarKind::Int)`), so `A :: 1` is an `i8` at one use and an `i64`
   at another. Record the *shape* — something like
   `enum ConstTy { ComptimeInt, ComptimeFloat, Settled(Ty) }` — and rebuild the
   variable at each use, rather than recording one type.
2. **`const_of_def` / `const_value_in` / `const_operand`** — a constant's
   **value**, for array lengths and `const` generic arguments. `ir::ConstValue`
   is already a persisted type in `library::metas`.
3. **`param_bound_traits` / `bound_trait_args`** — a generic parameter's bounds.
   `Decl` wants a `TypeParam` variant: the trait `DefId`s, and each bound's
   arguments as `Ty`s. Note `Def::assoc_bounds` already travels and is the
   precedent.
4. **`param_default` (`sema/lower.rs`)** — lowering re-lowers a callee's default
   argument **from the declaring file's tree**, swapping `self.ast` to do it.
   Defaults have to travel as **IR**, which is the one item here that is not a
   `Ty`: `nest.nir` already carries the package's IR, so a default is an
   `ir::Expr` recorded beside the function. `default_is_caller_location` is the
   same question and goes with it.
5. **`written_path_in`** — only a diagnostic's wording. Record the string, or
   accept a less specific message for a foreign def.
6. **`nest-lsp`** — `complete.rs` and `ide.rs` index `session.asts` and read
   `meta::<Ty>` off nodes. For a dependency there is no tree: hover and
   signatures come from the def's `file` + `span` and the declaration table.
   Nothing here was touched this session.

Then, and only then: delete `ast` and `facts` from `library::FileRecord`, keep
`src`, raise `FORMAT`, `just clean`. That is the commit that banks the 67%.

### 2. twig: a dependency must not write into its own directory

**The user asked for this and it is not started.** Building `twig` creates
`build/` inside `packages/core/` and `packages/std/` as well as in `twig/`.
Everything a build produces — every dependency's `.nlib` and objects included —
belongs in the **final package's** `build/`, which for `twig build` is
`twig/build/`. A dependency's own directory should be read-only to a build that
merely depends on it.

`twig/src/build.nest`'s `command` is the single place that decides a `nestc`
command line, and it is where the output paths are chosen. Note that
`just clean` currently has to go hunting (`find packages -type d -name build`)
precisely because of this, and that recipe can lose a line once it is fixed.

### 3. `nestc fmt`, and `twig fmt`

Asked for four sessions ago. **Not started.** The user chose every option below.

- **`nestc fmt` is the formatter**; it already owns the only parser that reads
  Nest. **`twig fmt` finds the package's files and runs `nestc fmt` over them**,
  the way `just fmt` runs `cargo fmt`. One implementation, two front doors.
- **Line width 100. Indent 4 spaces.** The first run over `twig/src` and
  `packages/` is a large diff: run it as **its own commit**.
- **align `::` in chained imports**; **break long chains** (`a.b().c().d()`) and
  **long parameter lists**, one per line, past the width; **prefer `.{}` and
  `.variant` shorthand wherever inference resolves the type** — that one needs
  *types*, so decide early whether `fmt` runs after inference or whether the rule
  applies only where the parser alone can tell.
- **prefer `match` over nested `if`s** *may be left out of the tool*, but the
  user asked that **twig's source be fixed by hand** either way.
- Chosen: **trailing comma on multi-line lists**; **sort and group imports**
  (`<core/...>` and `<std/...>` first, then `"local.nest"`, alphabetical within
  each group, blank line between); **collapse runs of blank lines** (at most one
  inside a body, two between top-level items). **Not** comment reflow.
- Opting out: **`#nofmt`, the directive, and only that.** A directive on a
  **block** is what the parser cannot do today (item 5), so `#nofmt` on anything
  smaller than a declaration needs that fix first.
- **Idempotence is the test** — format twice, get the same bytes; a property test
  over `examples/` and `packages/` is cheap. Add a **`--check`** mode so
  `just fmt-check` covers Nest as well as Rust.

### 4. twig: linking C libraries, through a build script

**The user chose the Rust shape**: a build script per package, not a manifest
field. It comes **after** item 1, because a link requirement has to travel with a
library's metadata — a package that binds `sqlite3` must put `-lsqlite3` on the
link line of every binary that depends on it, and that fact lives in the
metadata or nowhere.

The stopgap that exists: the root package's own flags, on the root package's own
targets only — `[build]` in `nest.toml` and `twig --nestc-arg`.

The shape, following cargo: **`build.nest` at the package root**, compiled and
run by twig before the package itself, printing directives on stdout —
`twig:link-lib=<name>` / `=<kind>=<name>`, `twig:link-search=<path>`,
`twig:rerun-if-changed=<path>`, `twig:rerun-if-env-changed=<name>`,
`twig:warning=<message>`. twig turns them into `nestc` flags; the spellings
already exist (`-l`, `--link-lib`, `--link-search`).

To decide: that running a dependency's build script **executes code from a
dependency** at build time (cargo does it; say so deliberately); where its output
goes and how freshness is decided (`twig --up-to-date` asks `nestc`, and a build
script is not `nestc`'s); whether twig should also compile a package's own `.c`
files; and what `[build]` in the manifest becomes once a script exists.

### 5. f-string format specifiers, `{x:?}` included

`f"{x}"` has **no** specifier syntax at all: §6.11 desugars each hole to one
`display` call on the `#lang("display")` trait. Wanted, in Rust's shape:
precision (`{x:.3}`), alignment and fill (`{s:>10}`, `{s:^10}`), width, and radix
(`{n:x}`, `{n:b}`). **And `{x:?}`**: `core` has a `Debug` trait
(`#lang("debug")`, `core/fmt.nest`) and nothing in the *language* reaches it yet.
This is a spec change to §6.11 plus a change to the `display` contract — one
method taking a spec, or a second trait. It is also what `core/write.nest`'s
hand-rolled `put_usize` exists for want of.

### 6. `#unsafe { ... }` — the block form does not parse

**Specified in §9, supported by the lowerer, missing from the parser.**
`lir/lower.rs` already reads `has_directive(b.id, "unsafe")` on a *block*. But:

```
#unsafe { r = a / b }        // error: expected RBrace, found Eq
```

The parser reads `#unsafe {` as a struct literal. To fix: in `parse_stmt`, when
directives are present and the next token is `{`, parse a block and attach the
directives to it — the `#comptime` case right above is the precedent. The AST
`NodeKind::Block` has no directives field, so it needs one (or a side table), and
then `sema::lower` must copy them onto the IR block's id. Only the **function**
form (`f :: #unsafe func ...`) works today. **Item 3 needs this**: `#nofmt` on a
block is the same parser gap.

### 7. Conditional compilation, the parts not built

`#when` is in and used; three extensions were left out on purpose.

- **`feature = "..."`** — package features from twig. Needs a manifest field, a
  `nestc` flag, and a fingerprint input, which is why it is its own phase.
- **On a statement or a block** — same parser gap as item 6.
- **The condition cannot be a Nest constant expression**, and will not become
  one: it is read before name resolution, so there is nothing yet for a name in
  it to mean. Do not try to make `#when(target.OS == .Macos)` work.

## What Was Done This Session

Seven commits.

### `refactor: one place reaches a declaration's tree` (`03d684b`)

`sema/decl.rs`: a `Decls<'a>` view over the def table and the trees, holding
every query that reaches a definition's syntax — `record_field_names`,
`param_names`, `param_defaults`, `generic_arity`, `type_param_defs`,
`func_generic_param_defs`, `trait_generic_param_defs`, `trait_default_method`,
`bound_nodes`, `const_lit_value`, `requirement`, and the `declaration()` helper
that unwraps a `ConstBind` to its RHS — which had been written out at every call
site in `infer.rs` and again in `lower.rs`. Behaviour-neutral; `lower.rs`'s
duplicate `record_field_names` is gone and `impls.rs`'s completeness check now
asks `Decls::requirement`.

### `feat: a library carries what each definition declares` (`86d6eb6`)

`Decl` (`Func` / `Type` / `Assoc`), a `DefId`-keyed `DeclTable` on the `Session`,
filled by `decl::record` after desugaring and before anything looks a definition
up, carried in `Meta::decls`, merged in by `read::load`. The queries prefer it
and fall back to the tree. `FORMAT` 3 → 4.

### `feat: a declaration's types are recorded on the def, not the tree` (`1f9d38d`)

`decl::record_types`, after inference: reads back what `stamp_member_types`
already worked out and files it under the definition — a field's type, a
variant's payload, a tuple struct's positions, a `distinct`'s representation, an
associated constant's type. **A type mentioning an inference variable is not
recorded** (`Ty::mentions_var`, added here): a variable is a hole in one context
and means nothing in another.

### `feat: a function's signature is recorded on its def` (`8b7d5ad`)

`FuncDecl::sig`, read back off what inference stamped — each parameter's type on
its own node, the return type on the `FuncExpr`, or the whole `Signature` for a
trait method. `func_def_ty` prefers it. The test
`the_table_answers_what_the_tree_would` compares **every** recorded signature
against one resolved from the tree a second time (`infer::signature_from_tree`,
`#[cfg(test)]`), over `core` + `std` + twig.

### `perf: an impl's target is resolved once, before inference` (`55efa36`)

`resolve_impl_targets` moved **before** the inference loop and now records
`ImplInfo::typed` — the self type, the trait arguments and the associated-type
bindings, resolved, generics left rigid. Selection substitutes into those
instead of resolving the same three type expressions at every trial of every
obligation. It did **not** measurably speed inference up (35.5 ms for twig,
either way — re-resolution was not the bottleneck); what it is for is that an
impl can now travel without its tree.

### `feat: a library carries its impls` (`5982ebd`)

`ImplInfo` split: everything portable stays, the three node fields move to
`ImplSyntax` behind `#[serde(skip)]`. `impls::build` takes `&mut ImplTable` and
walks **this compilation's files only**; the table is seeded from what the
libraries brought. `FORMAT` 4 → 5.

### `test: a program compiles against a library, in one process` (`c32c7c3`)

`library::tests::a_program_compiles_against_a_library`: `core` written as a
library, a package written against that, a program written against both, all
through `write::members` / `read::load` in one process. The call goes through a
**trait object**, which forces impl selection and vtable construction — verified
by breaking `impl_self_ty` and watching the test fail. This is the test that
would have caught the bug `cargo test` missed.

### `feat: an alias's expansion and a bodyless signature are recorded too` (`450aa79`)

- `Expansion(Ty)`, stamped by `check_type_aliases` on an alias's own binding node
  and filed under the def by `record_types` — for a `::` type alias and for a
  `::` binding that *names* a type (`const_alias_ty`, a `DefKind::Const`) alike.
- A **bodyless** function is never typed by the per-function passes and is not
  always a trait method — an `#intrinsic` or an `extern("c")` declaration is a
  signature with nothing to infer — so `stamp_member_types` now works those out
  too.
- `Inferer::rigid_self`: `Self` inside a generic trait's declaration resolved to
  the trait with **fresh variables** per parameter, which is right at a use and
  wrong in a declaration, and kept `FromResidual::from_residual`'s signature out
  of the table. The trait's own parameters go back where the variables were.
- `cargo fmt` is folded into this commit by mistake.

## Things Found But Not Fixed

- **`-C os=none` generates a variant `core` does not have.** `OSES` has `"none"`,
  `variant_name("none")` is `None`, and `core/os.nest` declares `Bare`. One line,
  in `Session::target_module_source` or in `OSES`.
- **`nestc/src/codegen/llvm/unit.rs` has three unused imports** (`HashMap`,
  `Global`, `Local`) and warns on every build. Pre-existing, untouched.
- **`sema/infer.rs` has two dead methods**, `collect_type_params` and `str_ty`.
  Pre-existing.
- **`#unsafe { ... }` does not parse** — item 6.
- **There is no test filter.** `twig test` runs all of them.
- **`tests/` as integration tests** (a second target seeing only `@public`) is not
  implemented. Only unit tests exist.
- **`core` has no tests of its own** and `just test` does not try to run them: the
  runner is the thing under test.
- **No `Display` or `Debug` for floats.** `f"{x}"` on an `f64` is "no impl of
  `Display`". Correct float printing is Ryū or Grisu.
- **`spec/11-c-ffi.md` §11.1 says the `core/c` types are "distinct nominal
  types"; `core/c.nest` says the opposite.** The implementation is right and the
  spec line is stale.
- **`design/toolchain.md` still describes `.nmeta` as a second artifact** (§Step 8
  and the roadmap list). The implementation and `design/library.md` are right.
- **`#repr("C")` does not reach `distinct`** (`Port :: #repr("C") distinct u16`).
  Refused today; a one-line change if somebody asks.
- `nestc/lsp/Cargo.toml` still has a `[profile.release]` block cargo ignores.
- **`editors/zed/extension.toml`'s `rev` still points at `d2b0cb8`.** It names a
  commit on GitHub, so it can only be bumped **after these commits are pushed**.

## Answers Already Given (do not re-derive)

- **`.nmeta` should hold analysis, not syntax.** It holds both today; 67% of
  `std`'s is the syntax, and removing it is item 1.
- **The `.nlib` already has the objects.** `u0.o`, `u1.o`, … are members of the
  archive, alongside `nest.nmeta` and `nest.nir`. There is no separate `.nmeta`
  file on disk.
- **A recorded type must not mention an inference variable.** A variable is a
  hole in one context and is numbered differently in the next.
- **Impl indices are compilation-local and do not travel.** `MethodRes` and
  `OpResolution` name `DefId`s, not indices, so seeding the table with a
  library's impls first is safe.
- **A `#when` condition is read before name resolution and so cannot be a Nest
  expression.** Its values are *spelled* as `core/os.nest`'s variants for the
  reader and the language server; they are not resolved to them.
- **`=` is a statement in Nest, not an expression**, and `not` is a keyword.
  That is why `#when` parses its own arguments, in `nestc` and tree-sitter alike.
- **`@test` does not decide whether a function is built.** `#when(test)` on the
  namespace does.
- **A library internalizes nothing**, and a program internalizes as before. Do
  not re-add a `CallConv::Fast` default either — see `dc74229`.
- **The calling convention is a property of the function, not of its type.**
- **`"fast"` means LLVM's `fastcc`**, not x86 `__fastcall`.
- **`#repr("C")` on a struct changes no layout today, and that is fine.**
- **Explicit discriminants are fieldless-only**, and the value is any constant
  expression.
- **Linking a C library works, two ways.** `nestc -l m` and `-C link-arg=-lm`.
- The language **does** have printing: `std/io` over `std/libc`. `core` has none
  on purpose and cannot import `std`.
- **An inner namespace sees the file around it.** No `super` import exists.
- **A blanket impl over every type is legal and already used three times.**
  Coherence picks the **most specific** impl (§4.9).
- **`member_dyn` is how a reflective walk reaches a member's own impl.**

## Warnings

- **`just clean && just build` before calling a session green.** It caught a real
  bug this session that every `cargo test` passed: a foreign impl resolved to
  `Ty::Error` because `resolve_impl_targets` checked for a missing *tree* before
  checking for a recorded *type*, and foreign trees still ship — so the guard
  never fired. Prefer the recorded answer **first**, always.
- **`git checkout <file>` to revert a debug `eprintln!` reverts the real edits in
  that file too.** It happened twice here. Revert the one line by hand.
- **`just test` runs `std`'s and twig's suites with the `twig` binary already
  built**, so after any compiler change **run `just build` first**.
- **twig builds itself.** After changing `twig/src`, `just build` compiles the new
  twig **with the old one**. Run it twice, or check in a scratch package.
- **`cargo test` takes about a minute and `just clean && just build` several**,
  which can exceed the 120s tool timeout — run them in the background and wait on
  the output file.
- **Read a compiler run's output to the end before believing it.** `nestc --emit
  ir` prints the IR *and then* the errors; `grep -c "^error"` is the honest check.
- **Attributes come before directives.** `@public` then `#when(...)`, never the
  other way.
- **A `#[derive(Deserialize)]` field added to a `Decl` variant breaks every
  pattern that matches it exhaustively** — including the ones in
  `decl::tests`. Use `..`.
- **`record_types` must not overwrite what `record` put there.** An associated
  type is a `DefKind::Const`/`TypeAlias` whose declaration is an `AssocType`
  node, and a catch-all arm will happily clobber its `Decl::Assoc` with a
  `Decl::Alias`. Guard on `table.contains_key`.
- **Editing a Rust string with a `\`-newline continuation from inside a Python
  heredoc silently eats the continuation.** `cat -v` on the line is how it was
  found. The same heredocs need `\\"` for a `\"` already in the file.
- Boehm is required: `brew install bdw-gc`, or `BDW_GC_PREFIX`.
- **A stale runtime archive links but misbehaves** — `touch nestc/build.rs`
  forces it to rebuild.
- **A bare name in a pattern BINDS.** `c.match { QUOTE => ... }` matches every
  value; the diagnostic is "unreachable `match` arm" on every later arm.
- **`.ok` on a `Result.<void, E>` still takes a value**: `return .ok(())`.
- **Two `extern("c")` declarations of one symbol in one package do not link.**
- An `extern("c")` declaration links by **the name it is written with**, and the
  block form is `extern("c") { name :: func (...) -> T }`.
- **A function cannot be declared with `let` inside a function.**
- `\xNN` is a **byte-string** escape. In a `str`, ESC is `\u{1b}`.
- **A line starting with `.` continues the previous line.** Write `return .ok(...)`.
- Every new side-table type needs registering in `library/metas.rs` (and must be
  `Send`). A serialized struct change needs a bump of `library::FORMAT` (**6**).
- `nestc` for twig and `nest-lsp` must be built from the same source.
- Grammar edits: run `npx tree-sitter generate`, re-parse the corpus, re-copy
  `highlights.scm`, commit, then bump `rev` in `editors/zed/extension.toml`.
- Language gotchas: ranges are `..<` and `..=`, a binding is `:=` and not `=`,
  `return`/`break` in a match arm need a block, there is no `++`, an associated
  type in an impl is `Item :: i32` (**not** `Item :: type = i32`), a method is
  reached through a value, `@public` goes on its own line above the item, and a
  struct literal is `Type { field: value }` with a space.

## Failed Approaches (Don't Repeat These)

- **Dropping `ast`/`facts` from `FileRecord` before the whole query list is
  recorded.** It gets `twig` compiling and `std` panicking, one query at a time.
  Work the list in item 1 first, with the probe; delete the fields last.
- **Recording a constant's type as a single `Ty`.** A comptime literal gets a
  fresh variable *per use* on purpose; record the shape, not the type.
- **Checking for a missing foreign *tree* before checking for a recorded
  *answer*.** While trees still ship, the tree branch always wins and the
  recorded answer is never exercised — which is how a foreign impl silently
  resolved to `Ty::Error`. Recorded answer first.
- **Placeholder entries in the declaration table** (`Decl::Field(Ty::Error)`
  before inference has run). An absent entry means "ask the tree"; a placeholder
  is an answer, and a wrong one.
- **A round-trip test whose call is an inherent method.** It resolves through the
  members map without impl selection and proves nothing about a library's impls.
  Go through a trait object.
- **Unlinking an excluded `#when` declaration from its namespace and stopping
  there.** `infer` walks the arena for `FuncExpr` nodes, so the body is still
  typed. Blank the subtree.
- **Putting `#when`'s conditions through the expression parser.** `not` is a
  keyword and `=` is a statement; the same is true of the tree-sitter grammar.
- **Believing `nestc --emit ir | head` when it printed IR.** The errors come
  after the dump.
- **Putting the calling convention in `Ty::Func`.** Cancelled by the user: *"do
  not do any weird type system stuff"*.
- **Letting the working tree hold two features at once.** Commit a phase the
  moment it is green.
- **Asserting on a diagnostic's exact wrapped text.** Assert on a fragment.
- **Checking a duplicate-discriminant diagnostic with two enums in one source.**
- **Pre-probing every library's freshness to count a build.** Propagate staleness
  in dependency order instead — that is what `plan` does.
- **A test fixture in another file's `tests` namespace** is unreachable unless both
  the namespace and the fixtures are `@public`.
- **Named constants as `match` patterns.** See the warning above.
- **Giving a synthesized LIR function the span of the `#lang` item it calls.**
- **Declaring one C symbol in two files of a package.**
- **Making `@test` keep a function out of the build.**
- **Putting the test runner's output shape in C.** It went to Nest on purpose.
- **Formatting a test's error inside the compiler.** Instantiate a Nest generic
  instead (`ir::mono::run`'s `asked`).
- From earlier sessions, still true: returning `Ty::Error` from a refused
  expression to suppress cascades; enforcing `opaque`'s position rule only on the
  spelling; assuming `#unsafe` removed every check (overflow is emitted from
  **two** places); trusting a `.ll` emitted under a different `-o`; driving the
  language server with **incremental** `didChange` ranges; measuring re-analysis
  by CPU alone; `git stash -- <path>` on a committed file; and an escape-analysis
  test with no `drop` in its LIR unless run under `NEST_GC_POISON=1`.

## User Notes

- **Finish item 1.** The user's words: *"nlib should not even have the AST, just
  IR, metadata and if it provides benefit, object file with the static part
  pre-compiled."* The objects are already there; the AST is the leftover, and it
  is 67% of the metadata.
- **Then item 2**, the `build/` directories — the user asked for it explicitly
  and asked that it be left for the next session rather than rushed.
- Then the formatter, then the build script.
- Divide the work into phases, each phase one feature, complete with a commit
  (short title, no description, no co-authors).
- Ask if you have any doubts or need to make design decisions. Batch the
  questions.
- For any non-benchmark build use the debug profile as it makes the build a lot faster.

## Work Plan
First, lets focus on finishing the nlib and fully removing the need for AST in it. After that measure the new sizes and times.
After that is done, proceed to fixing item 2 with build/ directories.
Then, proceed to f-strings supporting the proper formatting options, and finally, fix #unsafe and conditional compilation for blocks.
