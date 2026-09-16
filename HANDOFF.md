# Handoff: after the language server; next, `@test` and `twig test`

**Generated**: 2026-09-16
**Branch**: `main`
**Last commit**: `2047f05`
**Status**: Step 10 (the editor) is done: highlighting, diagnostics, hover, go-to-definition and completion in Zed. The user has tried the diagnostics in Zed. **First handle the open items below, then the test system**; nothing of the test system is started.
**Tests**:
- `cd nestc && rtk proxy cargo test -p nestc` gives **623**.
- `LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 rtk proxy cargo test --features llvm -p nestc` gives **643**.
- `cargo test -p nest-lsp` gives **15**.
- `cd editors/tree-sitter-nest && npx tree-sitter test` gives 7.

## First: open items from the user (before the test system)

1. **The twig path setting in Zed still did not work** for the user. `2047f05` is a guess at the cause: `~/` wasn't expanded, and relative paths weren't resolved against the worktree. It also adds logging: nest-lsp writes its arguments, the resolved twig and nestc, and every twig run to stderr, which Zed's language server log shows. **Ask the user for that log.** Other things to check:
   - does `LspSettings::for_worktree("nest-lsp")` see `lsp.nest-lsp.settings`?
   - was the dev extension rebuilt?
   - Possible extra: handle `workspace/didChangeConfiguration`, and send log lines with `window/logMessage`.
2. Done: **`twig build|run --emit ast,ir,mono,lir,llvm-ir,asm`** writes dumps for the root package's targets to `build/<profile>/obj/{lib,bin}/<name>/<name>.{ast,ir,mono,lir,ll,s}`, and asking for them recompiles the root library. nestc's `--emit` takes `kind=path` (for `ast`, `ir`, `mono`, `lir`, `obj`, `asm`, `backend-ir`).
   - Fixed: a slice constant (`X: []str :: .{ "a", "b" }`) failed in LLVM. LIR typed its storage as the slice itself; it is now an `[N]T` array global plus a `{ ptr, len }` view (`lir/lower.rs` `slice_storage`), tested in `a_program_links_and_runs`. twig's `build.known_emit` workaround could go back to a `[]str`.
3. `2047f05`: `nestc --obj-dir` plus twig keeping objects is done. The user asked that objects stay always; twig now passes `--obj-dir` on every build. Plain `nestc` still uses a temporary directory. Old objects from a changed `codegen-units` aren't cleaned.

## Next Step: a built-in test system

The user asked for it: **`@test` on a function marks it a test, and twig runs the tests of a package.** (`twig run` already exists: `twig run [--bin <name>] [-- <args>]`.)

**Ask the user before designing**, in one batch:

1. **What a test is.** Is it `@test` on a `func () -> void` that fails by panicking or trapping? Or should a `func () -> Result.<void, str>` also be allowed?
2. **What `@test` is.**
   - It could be a fixed attribute the compiler reads, like `@public` and `@link_name`.
   - Or it could be an `@attribute` struct declared in `core` and found by a `#lang` tag.
   - The second fits §9's user attributes and reflection.
3. **How tests run.** A failing test aborts the process (`nest_trap`), so:
   - (a) one process per test, where twig runs the test binary once per test name (`<bin> --run <name>`); or
   - (b) the runtime catches the trap (`setjmp`/`longjmp` or a signal handler) so one process runs them all.
   - (a) is simple and isolates tests.
4. **Where tests live.**
   - Tests could sit anywhere in a package's files, compiled only for `twig test`.
   - Or they could sit in a `tests/` directory as their own targets.
   - Should tests of a library see its private members?
5. **Output.** Should it look like cargo's (`test foo ... ok`, a summary, a non-zero exit on failure)? Should `twig test <filter>` exist?

A likely shape, to propose rather than assume:
- **nestc:** `nestc --test` compiles the entry as a test binary. It collects every function with `@test` (in the entry package's own files, not libraries') and synthesizes a `main` that runs one by name, or lists them all. See how `entry=auto` synthesizes a C `main` (`-C entry`, `lir/entry.rs`).
- **twig:** `twig test [filter] [--release]`
  - builds the dependencies as usual;
  - compiles the root package's library and each binary root with `--test` (`build::command` plus a flag; keep it the single source of flags);
  - runs each test in its own process and prints the results.
- **LSP (later):** code lenses to run a test.

*Done when*: a package with `@test` functions, some passing and some failing, prints each result through `twig test` and exits non-zero on a failure. Commit, stop.

## Completed This Session

- [x] `fa6a515`:
  - **`twig metadata`** prints JSON (root, profile, packages with targets `{name, entry, lib, output, program, args}`); the args are `build::command`'s.
  - **`twig build --deps`** builds only the dependency libraries that are stale.
  - **`nestc` became a library.** `src/lib.rs` holds the modules, `src/driver.rs` everything `main.rs` had, and `main.rs` is only `main` and `requested`.
    - `driver::Invocation::parse(args)` plus `.session(loader)` is how a tool gets a session set up exactly as a command line would.
  - **`nest-lsp`** in `nestc/lsp`, a workspace member of `nestc/Cargo.toml`.
- [x] `4a01ce0`: the Zed extension (`editors/zed`, a Rust `cdylib` on `zed_extension_api` 0.7) starts `nest-lsp`.
- [x] `0a287e2`:
  - **`nest-lsp --twig <path> --nestc <path>`**, which the extension sets from `lsp.nest-lsp.settings.{twig,nestc}` (the user hit "cannot find twig");
  - **hover**, **go-to-definition** and **completion**;
  - `///` lines above a definition are its hover documentation.

## How the Server Works

- **`nestc/lsp/src/workspace.rs`**
  - A file's workspace is its nearest `nest.toml`.
  - `Toolchain::prepare` (for twig: `build --deps`, then `metadata`) runs on a thread; tests use a `Fake`.
  - `candidates` orders a package's targets for a file. A binary's own-package `--extern` is swapped for `--package` so its library is read from source.
- **`analysis.rs`**
  - An `Overlay` `FileLoader` puts buffers over the disk, keyed by `session::resolve_import`.
  - `analyze(args, buffers)` returns an `Outcome { files, diagnostics, session }`.
  - Positions are UTF-16.
- **`server.rs`**
  - A **unit** is one command line: a workspace target, or a lone file with no manifest.
  - Units are re-analyzed when a file they read changes, or when their workspace is re-prepared.
  - After every save, all workspaces are prepared again.
  - Requests call `analyze()` first, so they see the latest edit.
  - Publishing only sends files whose diagnostics changed.
- **`complete.rs`**: completion, auto-import (`importable`: breadth-first over package roots from `libraries` and `pkg_of`) and variant completion.
- **`ide.rs`**
  - `find` picks the smallest node containing the offset that names something: a `Resolution`/`PathRes` on a use, a `MethodRes` on a method `FieldAccess`, or a `DefMeta` but only on the def's **name** (found with `word_in` inside `Def.span`, which covers the whole declaration).
  - Hover shows the declaration source (cut at a function's body, at most 16 lines), a local's or parameter's `name: Ty`, and the container path.
  - Completion re-analyzes with `__nest_lsp_complete` inserted at the cursor. After a `.` it offers a namespace's public `ns.members` or a type's fields plus `ImplTable` members. Otherwise it offers locals (after their `let`, inside their block), the file's names, the prelude and keywords.

## Not Yet Done / Unverified

- [ ] Hover, go-to-definition, completion and the new settings have **not been tried in the Zed GUI** (only through tests and a Python stdio driver against `twig/src/main.nest`). The wasm build of the extension is done by Zed.
- [x] Completion covers primitives, slices, `distinct` types and `.variant`. Names not in scope are offered with an auto-import, and trait methods too (`nestc/lsp/src/complete.rs`).
- [x] Files changed on disk are watched through `workspace/didChangeWatchedFiles`, registered dynamically when the client supports it.
- [x] Completion checks impl bounds (`complete.rs` `applies`/`bind`/`implements`): self types are matched structurally, bound generics are checked against their bounds (at most 4 levels deep), builtin operator rows count for primitives, and `distinct` types fall back to their representation. Trait arguments (`Add.<f64>`) are not checked.
- [x] Compiler fix: from another file, a namespace member is looked up in `ns.members` only, never `ns.imported` (`resolve.rs` `resolve_member`). An `@public x :: import` alias def is now `Visibility::Public`. Test: `a_private_import_is_not_a_member_to_other_files`.
- [ ] A standalone file (no manifest) only offers imports from packages it already loads.
- [ ] std and core document with `//`, not `///`, so their hovers have no documentation. Ask whether to convert them.
- [ ] Speed: completion re-analyzes, about 1.5 s in a debug build on twig. A release build should be much faster, but that isn't measured.
- [ ] Carried over:
  - Rust tests for the library path (an `.nmeta` round trip; an LLVM core → std → program run; an `--indirect` import error; `--up-to-date`).
  - `-C target-cpu=bogus` only warns.

## Failed Approaches (Don't Repeat These)

- **A test helper waiting for an empty `publishDiagnostics` on a correct file times out.** Nothing is published when nothing changed. Send a request instead; it analyzes first.
- **Needles in test drivers**: `"s.\n"` matched `packages.\n` in a comment. Anchor on something unique.
- `lsp_server::Response` has `response_result`, not `result`/`error`; use `Response::new_ok`/`new_err`.
- `lsp-types` 0.97's `Uri` is not `url::Url`; convert through `url::Url::from_file_path`, then `.as_str().parse()`.
- Earlier ones still apply:
  - Piping `cargo test` through the RTK hook shows nothing; use `rtk proxy cargo test > $SCRATCH/log 2>&1` and grep `^test result:`.
  - tree-sitter needs `prec.dynamic(-1)` for `Name {`, and `self` stays an identifier.
  - macOS has no `timeout`.
  - Nest code has no `.is_some()`, and a non-void statement must be bound to a name.

## Key Decisions

| Decision | Rationale |
|---|---|
| lib split + `nest-lsp` crate (user) | `main.rs` stays thin; the server is a client of the library |
| `lsp-server` + `lsp-types`, sync (user) | The compiler is synchronous and single-threaded |
| twig produces metadata and libraries (user: "always use twig") | One source of truth for flags; `build::command` |
| Only the open package from source, dependencies from `.nlib` | Fast; the dependencies are kept fresh by `twig build --deps` on save |
| `///` is documentation (user) | `//` stays a plain comment |
| Completion by re-analysis with a placeholder | A buffer being typed doesn't parse; the placeholder makes `p.` a member access with a typed base |

## Files to Know

| File | Why |
|---|---|
| `nestc/src/driver.rs` | The former `main.rs`: flags, `Invocation`, `load_libraries`, emission, linking |
| `nestc/lsp/src/{server,ide,analysis,workspace}.rs` | The server |
| `twig/src/{main,build,metadata}.nest` | Commands; `build::command` is the single source of flags |
| `nestc/src/lir/entry.rs` | Where a C `main` is synthesized, the model for a test runner |
| `runtime/nest_runtime.c` | `nest_trap`, which is what a failing test hits |
| `editors/zed/src/lib.rs`, `editors/README.md` | Extension and setup |
| `design/toolchain.md` | Step 10 is marked done; its "not scheduled" list has room for the tests |

## Resume Instructions

1. Run the tests as above.
2. Ask how hover, go-to-definition and completion behave in Zed, and fix what they report first.
3. Ask the five test-system questions above, in one batch.
4. Write the design into `design/toolchain.md` as a step, then build nestc's `--test` first (with Rust tests), then `twig test`. Commit, stop.

## Warnings

- **Standing rules**:
  - No `cargo fmt`; format by hand.
  - Don't edit existing comments unless they're now false.
  - Commits are title-only (`feat: a + feat: b`), with **no body and no co-author**.
  - Ask questions in batches, and ask when unsure.
  - Simplify LIR while working on codegen if possible, and pass this rule on in every handoff.
- **Don't push** without asking.
- Every new side-table type needs registering in `library/metas.rs`. A serialized struct change needs a bump of `library::FORMAT`; after one, delete the `build/` directories.
- `nestc` for twig and `nest-lsp` must be built from the same source: the server reads `.nlib`s by `FORMAT` and struct layout, and `compiler_id` is only the version.
- Grammar edits: run `npx tree-sitter generate`, re-parse the corpus, re-copy `highlights.scm`, commit, then bump `rev` in `editors/zed/extension.toml`.
- Language gotchas: a line starting with `.` continues the previous one; no multi-line strings; `return`/`break` in a match arm need a block; no `++`.
- Open questions carried over:
  - implicit `str` → `String`;
  - plain enums in serialize;
  - TOML multi-line strings and dates;
  - trait impls on named types parking members without the trait in scope.
- Stale comments left as-is:
  - `session.rs` `<dir>/foo/foo.nest`;
  - the `std/str.nest` header;
  - `sema/tests.rs` `only_in_scope_traits_are_selection_candidates`;
  - `driver.rs` "A future `twig`".

## User Notes
- I want you to build tests for twig after @test is working,
- Inside design/ directory, write a description of how nlib and nmeta formats work,
