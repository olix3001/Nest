# Handoff: seven bugs found by fuzzing, none fixed yet; then `@test`

**Generated**: 2026-09-17
**Branch**: `main`
**Last commit**: `77ad6d6`
**Status**: The GC work and the LSP work are done and committed. A bug hunt over
the compiler and the language followed, and **the bugs it found are all still
open** — they are the list below, in the order I would fix them. Nothing else is
in flight.
**Tests**:
- `cd nestc && rtk proxy cargo test -p nestc` gives **651**.
- `cd nestc && rtk proxy cargo test -p nest-lsp` gives **30**.
- `cd editors/tree-sitter-nest && npx tree-sitter test` gives 7.

## The bugs to fix

Each one has a program that shows it. They are small enough to retype; write them
under `/tmp` and compile with `nestc/target/release/nestc <file> -o /tmp/x`.

### 1. Anonymous struct types do not work at all (spec §3.8)

**The largest of these.** An anonymous `struct { ... }` in *any* type position is
rejected, and a `cast` to one reaches code generation as an error type.

```
main :: func () -> i32 {
  let a: struct { x: i32 } := .{ x: 2 }     // error: type annotations needed
  return a.x
}
```

```
main :: func () -> i32 {
  let a := .{ x: 2 }                        // error: type annotations needed
  return a.x                                // spec: this is an anonymous struct value
}
```

```
S :: struct { x: i32 }
main :: func () -> i32 {
  let p: S := .{ x: 2 }
  let q := cast.<struct { x: i32 }>(p)      // error: internal: an error type reached
  return q.x                                //        code generation in `main`
}
```

A parameter typed `struct { x: i32 }` fails the same way, and so does a named
struct with an anonymous struct field once it is *used* (declaring one compiles).
What does work: `.{ ... }` into a named struct, in a `let`, a `return` and an
argument, and the implicit anonymous→named coercion is untestable while the
anonymous side cannot be written. The spec promises all of it in §3.8, including
the two implicit struct→struct coercions and `cast` for the named→anonymous
direction.

### 2. Importing an inherent-impl member is accepted, then fails internally

```
{ wrapping_sub } :: import <core/num>       // accepted, though `wrapping_sub` is
main :: func () -> i32 {                    // a member of `impl uint.<N>`, not of
  let a: u8 := 0                            // the namespace
  let b := wrapping_sub(a, 1)               // error: internal: an error type reached
  return 0                                  //        code generation in `main`
}
```

The diagnostic says it is a compiler defect itself ("nothing was reported about
it, so inference produced an error type without a diagnostic"). Either the import
should be refused where it is written, or the call should be a real error.
`a.wrapping_sub(1)` — the method call — is fine.

### 3. Writing through two levels of indexing

```
main :: func () -> i32 {
  let mut g: [3][3]i32 := .{ .{ 0; 3 }; 3 }
  g[1][2] = 6            // error: `[3]i32` does not implement `core.ops.IndexMut.<?7>`
  return g[1][2]         //        and: cannot assign to this expression
}
```

Reading `g[1][2]` compiles and runs; `g[1] = ...` compiles. Only the nested write
fails, and the unresolved `?7` in the message says the index's type was never
inferred for the inner level.

### 4. A bounded type parameter does not coerce to its trait object

```
T :: trait { v :: func (self: *Self) -> i32 }
call :: func (d: *dyn T) -> i32 { return d.v() }
wrap :: func <X: T> (x: *X) -> i32 { return call(x) }   // error: type mismatch:
                                                        // expected `dyn T`, found `X`
```

`&a` where `a: A` and `impl T for A` coerces fine; the same coercion from a
type parameter that is bounded by the trait does not happen.

### 5. `T.Item` — an associated type through a type parameter

```
Holder :: trait { Item :: type
  get :: func (self: *Self) -> Self.Item }
use :: func <T: Holder> (t: *T) -> T.Item { return t.get() }  // error: cannot
                                                              // resolve name `T.Item`
```

`Self.Item` inside a trait works (`core/iter` uses it), and so does pinning it in
a bound, `<T: Holder.<Item = i32>>`. Only the projection through the parameter
fails — and `spec/05-functions-and-generics.md` §5.4 writes exactly that form
(`C: FromIterator.<Item = I.Item>`).

### 6. A repeat aggregate is written out element by element

`let a: [N]u8 := .{ 0; N }` costs about 40 µs per element: 10 000 takes 0.4 s,
50 000 takes 2 s, and 1 000 000 never finished (killed at 25 s). It should lower
to a zero initializer or a memset for a constant repeat, and to a loop otherwise.

### 7. Deep recursion is a segmentation fault

`f :: func (n: i32) -> i32 { if n == 0 { return 0 } return f(n - 1) }` with
`f(100000)` dies with SIGSEGV rather than a trap. Decide whether that is worth a
stack guard (a probe in the prologue, or a runtime-checked limit) or is the
platform's business; it is the one item here that may be "as intended".

*Done when*: each has a test in the suite that fails without the fix. §1 and §2
are the two that end in `internal:`, which is the compiler calling itself wrong.

## Then: `opaque` (the user asked for this)

A type that has **no size and no values**, usable only behind a pointer, for the
FFI cases where the pointee is not ours to describe: `FILE`, `sqlite3`, a handle
a C library hands back. `*mut opaque` / `*opaque` are the only forms.

The user's preference, verbatim: **`opaque` is the name, with `c.void` an alias
for it if that fits.** So the language gets `opaque`, and `std/c` re-exports it
under the name C programmers will look for. Things to settle while designing it:

- Is it one built-in type (`opaque`), or a declaration (`Handle :: opaque`) that
  makes a fresh nominal one per library handle? The second is what C headers
  mean, and it keeps two libraries' handles from being interchangeable.
- What is refused: `let x: opaque`, a field of it, `size_of`, `new.<opaque>()`,
  dereferencing `*opaque`, `make.<[]opaque>`.
- `*opaque` ↔ `*mut opaque` ↔ `*u8` conversions: which are implicit, which need
  `cast`, and whether a `*T` may `cast` to `*opaque` at all (§11 says a C pointer
  is not nullable, which stays true).
- Where it lives in the spec: §3 as a type, §11 for the FFI rules.

## Then: a built-in test system (unchanged, not started)

`@test` on a function marks it a test, and `twig` runs a package's tests. The
five questions to ask before designing it are in the previous handoff's list and
still stand:

1. What a test *is*: `func () -> void` that fails by trapping, or also
   `func () -> Result.<void, str>`?
2. What `@test` is: a fixed attribute the compiler reads (like `@public`), or a
   `@attribute` struct in `core` found by a `#lang` tag?
3. How tests run: one process per test (`<bin> --run <name>`), or the runtime
   catching the trap?
4. Where tests live: anywhere in the package, compiled only for `twig test`, or a
   `tests/` directory? Do they see private members?
5. Output: cargo's shape (`test foo ... ok`, a summary, non-zero exit)? A filter?

A likely shape: `nestc --test` compiles the entry as a test binary, collecting the
entry package's `@test` functions and synthesizing a `main` the way `entry=auto`
already synthesizes one (`lir/entry.rs`); `twig test [filter]` builds through
`build::command` and runs each test in its own process. Write the design into
`design/toolchain.md` first. Twig's own tests come after `@test` works.

## Completed This Session

- [x] `d383dd1` — **the collector**:
  - The runtime is Boehm and nothing else; the leaking `calloc` build is gone.
    `nestc/build.rs` finds bdwgc (`BDW_GC_PREFIX`, pkg-config, brew, `/usr/local`,
    `/usr`) and fails the build without it; `libgc.a` is linked statically, so a
    built program does not need the collector installed. `build.sh` checks too.
  - `NEST_GC_POISON=1` makes `nest_free` fill an object with `0xDB` and keep it,
    so a read after a wrong free fails every time rather than sometimes.
  - **Escape analysis was freeing memory still in use**: `&p.*.x`, `&mut
    p.*.arr[2]` and `p.*.arr[0..]` each let an address inside the allocation
    leave, and none of them disqualified it. An element used in place
    (`p.*.arr[2] = 7`) still does not, which is the precision worth keeping.
  - **A compiler crash**: `match` on `Option.<*mut Node>` where `Node` reaches
    itself through a pointer overflowed the stack in the exhaustiveness check
    (`ir/check/exhaustive.rs`). A wildcard column whose rows name no constructor
    now goes to the default matrix instead of being expanded.
  - `gc_pin` now promises only that the object does not move (Boehm never moves
    anything, so it compiles to nothing); `gc_leak(p)` is new and keeps an object
    alive until `drop(p)`, through a root set the runtime keeps
    (`nest_gc_leak`/`nest_free`).
  - `examples/gc/{collects,escapes,leak}.nest`, each run by a test in
    `codegen/llvm/tests.rs`.
- [x] `77ad6d6` — **the language server**:
  - `settle()` waits for twig, so the first question of a session is answered
    instead of returning nothing (this was "no completions in Zed").
  - A save that finds the same metadata no longer bumps the workspace's
    generation, which used to mark every unit stale: three saves went from a full
    re-analysis each to 0.01 s.
  - A document is analyzed once it has been quiet for 400 ms (`IDLE`), so typing
    no longer publishes diagnostics about half-written lines.
  - An import an answer would write is clamped into the text the editor has.
  - `NEST_LSP_LOG=<path>` records every message, every analysis, and whether a
    completion came from the analysis already made or ran a new one
    (`nestc/lsp/src/log.rs`).
  - Measured: completion 25–30 ms a keystroke, from the existing analysis, with
    no re-analysis at all.
- [x] `design/library.md` — the `.nlib` and `.nmeta` formats (uncommitted at the
  time of writing this, along with this file).

## How the Bug Hunt Was Run

A directory of small programs, each with `// expect: <status>` or
`// expect: error` on its first line, and a script that compiles and runs them and
reports an ICE, a compile failure, a wrong status, or a compiler that had to be
killed. Roughly 70 programs over: nesting and long expressions, traps (overflow,
division, shifts, index), slices and arrays, unicode in strings and identifiers,
`defer` in every position, generics and monomorphization (including an infinite
one, which is refused properly), `dyn`, enums and match (guards, or-patterns,
nested payloads, tuples of bools), `distinct`, `transmute`, statics, recursion,
`@using`, interpolation, operators, associated types, function pointers, FFI,
`embed_file`, and empty files. Everything not in the list above passed.

Worth keeping in mind while fixing: three of the seven (§1, §2, §5) are inference
or resolution gaps against what the spec already promises, so the spec is the
thing to read first, not the code.

## Warnings

- **Standing rules**:
  - No `cargo fmt`; format by hand.
  - Don't edit existing comments unless they're now false.
  - Commits are title-only (`feat: a + fix: b`), with **no body, no co-author and
    no session trailer**.
  - Ask questions in batches, and ask when unsure.
  - Simplify LIR while working on codegen if possible, and pass this rule on in
    every handoff.
- **Don't push** without asking.
- Boehm is required now: `brew install bdw-gc`, or `BDW_GC_PREFIX`.
- Every new side-table type needs registering in `library/metas.rs` (and must be
  `Send`). A serialized struct change needs a bump of `library::FORMAT`; after
  one, delete the `build/` directories. `design/library.md` says why.
- `nestc` for twig and `nest-lsp` must be built from the same source.
- Grammar edits: run `npx tree-sitter generate`, re-parse the corpus, re-copy
  `highlights.scm`, commit, then bump `rev` in `editors/zed/extension.toml`.
- Language gotchas that cost time while fuzzing: a line starting with `.`
  continues the previous one (so match arms on their own lines need commas or
  braces), ranges are `..<` and `..=` rather than `..`, a binding is `:=` and not
  `=`, `return`/`break` in a match arm need a block, and there is no `++`.
- `nestc/lsp/Cargo.toml` has a `[profile.release]` block that cargo ignores:
  profiles only count in the workspace root manifest.

## Failed Approaches (Don't Repeat These)

- Driving the server with **incremental** `didChange` ranges: it advertises full
  synchronization, so a range change makes its copy of the file the typed text
  alone. Send the whole document.
- Measuring "is the server re-analyzing" by CPU alone: an analysis of `std` in
  release is ~100 ms and hides in the noise. Use `NEST_LSP_LOG`.
- `git stash -- <path>` to check whether a fix is load-bearing, when the file is
  already committed: it stashes nothing and the check silently passes.
- In the escape-analysis tests, a case with no `drop` in its LIR proves nothing
  about freeing too early. Run it under `NEST_GC_POISON=1`.

## User Notes

- ~~LSP completions do not work~~: fixed in `77ad6d6`.
- ~~Ensure the GC works properly~~: done in `d383dd1`.
- ~~Document the `.nlib` and `.nmeta` formats in `design/`~~: `design/library.md`.
- Fix the bugs found by the hunt (the seven above).
- Add `opaque` (with `c.void` as an alias if it fits).
- Build tests for twig after `@test` works.
- LSP tests should keep covering odd editor scenarios.
