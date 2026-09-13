# Handoff: `nestc` is a real compiler driver, and a Nest program runs

**Generated**: 2026-09-13
**Branch**: `main`
**Status**: **561 tests pass** without LLVM, **577** with it. `cargo clippy`
reports the same ~80 dead-code-shaped warnings as before. **`nestc prog.nest`
produces `prog`, and running it runs the program.** Everything is committed; the
tree is clean.

**Read `design/toolchain.md` first** — it is the plan for everything after this
session: `core/c`, then `std`, then `twig`, then the editor. Then
`design/lir.md` §10 (the backend's brief) and §7e (the entry point).

## What happened this session

Two commits:

1. `70e9a6b` — **a program runs**: the entry point, linking, JSON diagnostics,
   `-L`.
2. *(this one)* — **one object whatever the split**, `--package`, and the plan.

## Completed

### The entry point (`nestc/src/lir/entry.rs`, `design/lir.md` §7e)

A linker looks for `main`; the program's `main :: func ()` (spec §5.6) has a
mangled symbol. So LIR adds a second function — named `entry`, symbol `main`,
returning C's `int` — whose whole body is `nest_init()`, then the program's
`main`, then its status.

- **Built in LIR, not in a backend**: it is two calls and a return, which LIR
  already says. A backend that built it would build it again, differently, for
  the next target.
- **Built in LIR, not in the C runtime**: a `main` in `nest_runtime.c` would
  have to encode this compiler's mangling, and encode it twice over because
  `main` may return nothing or a status. The shim stays six functions that know
  no names.
- Named `entry` and *symbolled* `main` — the `name` on a `Function` is a source
  name and this function has none. Two `main`s in one dump, one calling the
  other, is a reader's problem for no gain.
- `-> void` exits zero, `-> i32` returns its status, another integer width is
  converted by a `CastKind`, `-> never` ends in `unreachable`.
- **`-C entry=auto|none`.** `auto` adds one when there is a file-scope `main`,
  so a library — having none — is already right without being told.
- **The rule for which `main` that is now lives once**, in `Linked::mains`,
  shared with `ir::check::declarations`. Two of them in one compilation is now
  an error (`a program has one main`) rather than a silent choice.

### Linking (`nestc/src/codegen/link.rs`, `nestc/build.rs`)

- **Not a backend's job.** What to do with several objects is the same question
  on every target, answered by a tool this compiler does not ship — so `nestc`
  invokes the platform's C compiler as the linker driver, which is what knows
  where `crt1.o` is. `-C linker=`, `-C link-arg=` (repeatable, in order),
  `-C runtime=`.
- **The runtime is built beside the compiler.** `build.rs` compiles
  `runtime/nest_runtime.c` into `libnest_runtime.a` (leaking allocator, no
  Boehm, no dependencies) and the path is embedded; every link picks it up. A
  **failure to build it is not a build failure** — the C toolchain is needed to
  link a program, not to compile one — and the driver names `-C runtime=` at the
  point a link is actually attempted.
- **`--emit` defaults to `link`**, which is what `cc foo.c` does. The dumps are
  one flag away and unchanged.
- **`--emit obj` produces one object however many codegen units there were.** A
  unit is a unit of *work* (§11); what comes out should not say how it was
  compiled. Several units are emitted to a scratch directory and merged with a
  partial link (`ld -r`, `-C partial-linker=`), and the parts are deleted.
  **Emission is still sequential** — parallel codegen is a backend instance and
  an LLVM context per thread, and the merge step is already in place for it.
- `-o` names the artifact; when a link also wants that name, other emissions
  grow the extension (`--emit link,obj -o prog` → `prog` and `prog.o`).

### The two things a build tool needs (`--error-format=json`, `-L`, `--package`)

- **`--error-format=json`** — one JSON object per line on stderr (JSON Lines,
  because a tool reads stderr as a stream and a top-level array could not be
  parsed until the compiler exited). The shape mirrors `Diagnostic` and adds
  what only the `SourceMap` knows: the file's name and each span's line and
  column, **beside** the byte offsets. It carries `rendered` — the human text
  verbatim — so a tool forwarding a message never reimplements the renderer.
  The driver's own failures go out the same way.
- **`-L <dir>`** — a package `foo` is `<dir>/foo/foo.nest`. Searched in order.
- **`--package name=path`** — pins a package to its root file. This is what
  `twig` will use: it has already resolved the dependency, and a search would be
  a second, weaker answer.
- **Precedence**: an explicit registration (`--package`) beats a `-L` search,
  and a `-L` search beats the compiled-in `core` path — which is a *fallback*
  pointing at this checkout, and a compiler run elsewhere should use the `core`
  it was pointed at.

### The driver as it stands (`nestc --help`)

```
usage: nestc [options] <file.nest>

  -o <path>              where to write the output
  --target <triple>      the machine to generate code for (default: the host)
  --emit <list>          link (default), ast, ir, mono, lir, obj, asm, backend-ir
  -L <dir>               a directory to search for packages
  --package <name>=<path>  a package pinned to a root file
  --error-format <form>  human (default) or json
  -C <key>=<value>       backend=, codegen-units=, entry=, linker=, link-arg=,
                         partial-linker=, runtime=, overflow=, pointer-width=,
                         os=, arch=, profile=, print=options
```

## Next: the plan

**`design/toolchain.md` is the whole thing.** In order, with the reason each
arrow is real:

1. **`core/c`** (spec §11, nothing implements it). C types as **aliases**, not
   `distinct` — the user's decision, contradicting the spec text deliberately,
   so that §11.4's coercions become nothing at all. `c.ptr.<T>` stays its own
   type. **C strings are the one special case**: `c.str`, a `c"..."` literal
   that emits a trailing NUL, and explicit `c.cstr` / `c.from_cstr` both ways —
   no implicit conversion, because a `str` is `{ ptr, len }` and a C string is
   not. `extern("c")` already works end to end; what is missing is the types.
2. **`std`**, on **libc through `core/c`**, with a `std/libc` namespace exposing
   the raw declarations unwrapped. Written so the backing implementation is
   swappable (an internal `sys` namespace) without writing the syscall backend
   now. Not linked automatically — that is `twig`'s job. `io`, `fs`, `process`,
   `mem`, `str`, `collections`, `json`, `libc`.
3. **`twig`**, written in Nest. The first real program in the language.
4. **The editor** — syntax highlighting (needs nothing), then an LSP, which
   waits for `twig` because a manifest is what tells a server what a workspace
   is. It may be written in Nest itself.

### Reflection, decided

**Compile-time reflection *and* user-defined attributes, with attributes visible
as data on the type information. No `comptime` keyword.**

**The spelling is `#intrinsic`, not a sigil**: `$name` was retired in phase 3, so
reflection is a `core/reflect.nest` of **bodyless `#intrinsic` functions** called
like any other — the way `cast`, `size_of` and `make` already work. `@` stays the
**attribute** sigil.

- `type_info.<T>()` returns a **value** — ordinary data, constant-folded because
  `T` is concrete after monomorphization. Its members are a slice of descriptors
  whose `kind` is an **enum**, so a loop over the descriptions is an ordinary
  loop and can run at run time like any other.
- What must unroll is **typed access to the value**: an accessor yielding a
  different type per member is what forces it — the loop's body, not the data.
- **A field chosen at run time is read with `member_ptr` and an ordinary
  `cast`** — the address is `base + m.offset`, which LIR already computes. A
  *checked* read is **`TypeId`**: `ir::mono::type_key` is already a globally
  unique string per monomorphized type, so `type_id.<T>()` is one bodyless
  `#intrinsic` folding a 128-bit hash of it, a `Member` carries one, and the
  check is the bounds check's lowering. It keeps `distinct` (unlike `Cx::strip`),
  and it is what an `Any` would sit on. The GC
  hazard is pre-existing: `&mut p.y` is already an interior pointer, Boehm
  traces them, and a precise collector wants the **object-start table** that is
  already open in §5/§6.
- An `@attribute` declaration defines a struct; `@json(rename: "user_id")` on a
  member puts that struct in the member's `attrs`. No expansion pass, no
  generated code, no second program representation. This is a **new** §9
  addition — today's attributes are a fixed set (`@public`, `@link_name`).

## Still open, and yours to decide

| Decision | Why it is open |
|---|---|
| **C varargs** (`printf`) | Not in the spec or the parser. `std` can do its own formatting; a program calling C cannot always |
| **How unrolling is spelled** | Implicit (a loop whose body demands it unrolls), or written — `#unroll` |
| **`#unroll` on ordinary loops** | A hint like `#inline` — the same feature as above, or a different one sharing a name |
| **The manifest's name and format** | `twig.toml` and TOML, or something `std/json` can already read |
| **The tool's name** | `twig` or `hatch` |
| **Whether `std` is versioned with the compiler** | Rust ships one per compiler; a package tool could resolve it like any dependency |
| **What an `rlib` contains** | Objects plus metadata in one file, or two as Rust has them |
| **`Drop`** (`#lang("drop")`) | Waiting on a *decision*, not on work: with no moves, "this value was returned / stored / passed to a call, so do not drop it" has no settled answer, and a wrong one either double-releases or never releases |

## Not Yet Done (compiler)

- [ ] **`repeat` and `format` still owe the `$slice` treatment.** Both are more
      than "one instruction or one runtime call", which §10 claims of an
      intrinsic. `repeat` is a `make` and a loop; `format` needs a formatter and
      an allocation. **Standing instruction: when something can be simplified in
      LIR while building codegen, do it, and carry this instruction into the
      next handoff.**
- [ ] **A `void` member should be erased from a `TypeDef` too.** §9 erases
      `void` from slots, parameters and arguments but not from a type's members.
      The backend skips zero-byte members, which is correct on its own terms.
      **Not done because `Projection::Field` indices are positional**, so
      dropping a member shifts every index after it.
- [ ] **`-C opt-level` and `-C target-cpu`.** The backend hardcodes
      `OptimizationLevel::None`, `RelocMode::PIC`, `CodeModel::Default`.
- [ ] **Parallel code generation.** The units are independent and the merge is
      in place; what is missing is a backend instance and an LLVM context per
      thread.
- [ ] **A library format** (`rlib`/`rmeta`). A dependency is source today.
- [ ] **Debug info.** Nothing emits DWARF, though every statement carries a span
      and every `TypeDef` carries its origin and pre-flattening name.
- [ ] **A `defer` captures at registration, and this one does not** (spec §8.4).
      The position half is fixed; this half is not.
- [ ] **Optimization across units**, per-function escape summaries, the
      object-start table, narrowing what counts as a root (§5/§6).

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs still stands. New this session:

- **Putting the entry point in the C runtime.** It would have to encode the
  mangling scheme, and encode it twice for `main`'s two legal return shapes.
- **Calling the synthesized entry `main` as well.** Two functions with one name
  in every dump, distinguishable only by a symbol comment.
- **Finding the entry point by "parent is a namespace".** A file *is* a
  namespace and so is `namespace app { }`, so that test finds `app.main` too.
  The canonical name is what distinguishes them — `Linked::mains`.
- **Letting `-o` name both an object and an executable.** The second write wins,
  silently, and a build system links whichever it finds.
- **One temp directory shared by two tests that assert nothing else is in it.**
  The other test's artifacts read as leftovers.
- **A temp directory that outlives the run.** The same source hashes to the same
  name every time, so the *previous* run's executable was sitting in it and read
  as a part left behind — a test that passed alone and failed in the suite. The
  process id is in the path now.
- **A `--emit` list that adds to the default** rather than replacing it. Asking
  for the LIR is not also asking for a program.
- **Making a missing C toolchain a build failure.** A `nestc` that cannot link
  still compiles, dumps and emits objects.

## Key Decisions

| Decision | Rationale |
|---|---|
| The entry point is synthesized in LIR | It is two calls and a return; a backend would reinvent it per target, and C would have to encode the mangling |
| It is named `entry` and symbolled `main` | `name` is a source name and this function has none |
| `-C entry=auto` keys off a file-scope `main` | A library has none, so the default is already right for one |
| One rule for "which `main`", in `Linked::mains` | Two copies could disagree and nothing would show |
| Two root `main`s is an error | Picking one silently means a program never starts where its author wrote |
| Linking is the driver's, not the trait's | The question is the same on every target |
| `cc` is the linker driver | It is what knows `crt1.o`, the system libraries, and the loader's name |
| The runtime is built by `build.rs` and linked automatically | Every program calls `nest_init`; it is not the program's to remember |
| A missing C toolchain is a link-time message | The compiler is still a compiler without it |
| `--emit` defaults to `link` | What `cc foo.c` does, now that a backend can produce a program |
| One object, whatever the split | A codegen unit is a unit of work, not an artifact |
| Several units merge with `ld -r` | The parts are independent, which is also what parallel codegen needs |
| JSON diagnostics are JSON Lines | A tool reads stderr as a stream; an array parses only at exit |
| The JSON carries `rendered` | So forwarding a message never means reimplementing the renderer |
| Byte offsets travel beside line/column | An editor works in one and a person in the other, and neither can be derived without the file |
| `--package` beats `-L`, `-L` beats the built-in `core` | A resolved path is a fact; a search is a guess; the compiled-in path points at this checkout |
| C types will be aliases, not `distinct` | The point is that a language value goes in; §11.4's coercions become nothing |
| C strings are explicit both ways | A `str` is `{ ptr, len }` and a C string is not; neither has what the other needs |
| `std` sits on libc and also exposes it raw | One implementation across three platforms, and an unwrapped `ioctl` should not need re-declaring |
| Reflection is data, attributes are data on it | A loop over descriptions is an ordinary loop; only typed access to a value must unroll |
| Reflection is `#intrinsic` functions in `core`, not a sigil | `$name` was retired in phase 3; `@` is the attribute sigil |
| A run-time field read is `member_ptr` plus a `TypeId`-checked read | The check is the bounds check's lowering, and the identity it compares is a hash of `mono::type_key`, which already exists whole-program |

## Current State

**Working**: everything. `cd nestc && cargo test` → **561**;
`LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 cargo test --features llvm` →
**577**. All ten examples emit object files at every `-C codegen-units` setting,
a program links and runs, and a four-unit build produces one object that links
and runs.

**Broken**: nothing. **Uncommitted changes**: none.

**Not implemented and blocking `std`**: `core/c`. A program cannot call a C
function today for want of the types to declare one with — `c.ptr`, `c.char`,
`c.int` do not resolve.

## Files to Know

| File | Why it matters |
|---|---|
| `design/toolchain.md` | **The plan**: `core/c` → `std` → `twig` → the editor, and every open decision |
| `design/lir.md` | The specification. §7e is the entry point, §10 the backend's brief, §11 the unit split |
| `design/roadmap.md` | Phase 10 is this work |
| `nestc/src/lir/entry.rs` | The synthesized entry point |
| `nestc/src/codegen/link.rs` | `link` (a program) and `combine` (`ld -r`), and `LinkOptions` |
| `nestc/build.rs` | Builds `libnest_runtime.a`; fails softly |
| `nestc/src/main.rs` | The driver: flags, `write_object`, `write_units`, `link_program` |
| `nestc/src/common/emitter.rs` | `render` and `render_json` |
| `nestc/src/sema/session.rs` | `register_package`, `add_search_path`, and the precedence between them |
| `nestc/src/ir/link.rs` | `Linked::mains` — what an entry point is |
| `nestc/src/codegen/mod.rs` | The `Codegen` trait, `TargetInfo`, `OutputKind`, `backends()` |
| `nestc/src/codegen/llvm/unit.rs` | **The translation.** Its four design notes are the module comment |
| `runtime/nest_runtime.c` | Six functions, and why a shim exists |

## Resume Instructions

1. `cd nestc && cargo test` — expect **561 passed**.
2. With the backend:
   ```
   export LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21
   cargo test --features llvm          # expect 577
   cargo build --features llvm
   ```
3. **See a program run** — no hand-written C driver any more:
   ```
   cat > /tmp/prog.nest <<'EOF'
   add :: func (a: i32, b: i32) -> i32 { return a + b }
   main :: func () -> i32 { return add(2, 3) }
   EOF
   ./target/debug/nestc -o /tmp/prog /tmp/prog.nest && /tmp/prog; echo $?   # 5
   ```
4. See the LLVM IR: `nestc --emit backend-ir -o /tmp/x.ll file.nest`.
   See the diagnostics a tool would read: `nestc --error-format=json file.nest`.
5. **Next: `core/c`** (stage 1 in `design/toolchain.md`). Start with the types
   and `extern("c")` declarations — which already work — and then the `c"..."`
   literal. Ask about varargs before assuming either answer.

## Edge Cases & Error Handling

- **A `char`** is `u32` by this level, so `c == 'a'` is `eq.u32 c_0, 97`.
- **A match on integer literals is a comparison chain** (§4), not a jump table.
- **A zero-length string** is a `[0]u8` global; a backend may emit nothing for it
  as long as the address is valid.
- **A `defer` in an `if` block** belongs to that block's scope. Spec §8.4 says
  "the current function scope"; nothing tests the difference.
- **`Intrinsic::Unknown`** is the safety net for anything lowered away.
- **A backend refuses an output kind it does not produce** rather than writing a
  file of the wrong thing — a file that exists and holds the wrong content is
  worse than no file, because a build system will link it.
- **The `lir` backend cannot produce an object**, so a bare `nestc` against it
  now fails at the link rather than silently doing nothing.
- A slice a program reads from is never dropped, `check::dropped` does not
  follow aliases, `transmute` reads its result type off the destination, and
  `core.panic` and `bytes_eq` are in every program because there is no dead-code
  elimination.

## Warnings

- **`LLVM_SYS_211_PREFIX` must be set** for anything `--features llvm`.
- **`build.rs` needs `cc` and `ar`** to produce the runtime. Without them the
  compiler still builds and every test that does not link still passes — the two
  link tests return early rather than fail.
- **Do not reintroduce a filter in the renderer.** `pretty::unit_to_string`
  prints a unit exactly as it is.
- **`Cx::strip` is load-bearing in three ways at once** — `distinct`,
  mutability, and the key the type table is built on.
- **`-C codegen-units` is not tuning.** At `N > 1` optimization across the cut is
  gone; the default is 1 for that reason.
- **Around 80 clippy warnings are expected**, all dead-code-shaped.

## User Notes (standing)

- **Ask questions in batches.**
- **If something can be simplified in LIR while building codegen, do it**, and
  pass this instruction on in every future handoff. `$slice` and `$array` were
  the first two; **`repeat` and `format` are next**.
- **Casts should be explicit about which conversion they are.** Done —
  `CastKind`.
- **The GC is not the priority.** An existing collector behind a thin shim, so
  it can be swapped cheaply. Done as the C shim + Boehm.
- **Codegen is a trait**, and it supplies the target info. Done.
- **LLVM codegen uses inkwell and produces object files.** Done.
- **Everything should be easily usable by the future CLI tool.**
- **`nestc` before `twig`.** Done — see the flag list above.
- **`std` is not linked automatically**; that is `twig`'s job.
- **`twig` is written in Nest**, so `std` needs fs, io and JSON serialization
  first.
- **`nestc` supports structured output as JSON.** Done.
- **There should be `insta` snapshot tests over the generated LLVM**, with no
  target-specific content (no triples), so they pass on every machine. *Not done
  — the LLVM tests assert properties rather than snapshots.*
- **Conditional compilation** is wanted for building `std`, since different
  targets need different code. `#when` is not in the parser, the spec or the
  grammar.
- **One object file out of `nestc`, always.** Several units are for parallelism;
  they are merged. Done.
- **A runtime loop over a type's members is fine** — the members are data and a
  member's kind is an enum. Only typed access to the value must unroll.
- **If you are unsure or need further guidance, ask.**
