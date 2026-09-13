# Handoff: step 6 is done — `std` exists, and five things under it were broken

**Generated**: 2026-09-13
**Branch**: `main` (ahead 37)
**Status**: **583 tests pass** without LLVM, **600** with it. `cargo build`
reports 48 warnings, all dead-code-shaped — unchanged. **A Nest program reads a
file, writes to stdout, spawns a process, and reads its arguments and
environment.**

**Read `design/toolchain.md`** — its ten-step order of work is the plan. **Steps
1–6 are done and committed.** What is left is **`std/json`, the library format,
`twig` and the editor** — steps 7 through 10, in that order. The rest of
`#comptime` is still **deferred and unscheduled**; nothing below needs it.

## What happened this session

One commit: step 6, and the five bugs found inside it. The library is the small
part — most of the work was that `std` is the first program large enough to use
the language rather than test it, and it found things.

### `packages/std/` — seven namespaces over an internal `sys`

| File | What |
|---|---|
| `libc.nest` | The raw C surface, declared with `core/c`'s types and **exposed on purpose** |
| `sys.nest` | The backing. **Not re-exported**, so `<std/sys>` does not resolve |
| `io.nest` | `Error`, `Write`, `Read`, `File`, `write_all`, `read_to_end`, `print`/`println` |
| `fs.nest` | `open`, whole-file `read`/`write`/`append`, `exists`, `remove`, `rename`, `create_dir`, `size` |
| `process.nest` | `args`, `env`/`set_env`/`env_vars`, `exit`, `spawn`/`run`/`wait`, `Status` |
| `mem.nest` | `copy`, `fill`, `clone`, `equal`, `index_of`, `swap`, `reverse` |
| `str.nest` | `from_utf8`, `find`, `split`, `lines`, `trim`, `join`, `parse_int`, `to_string` |
| `collections.nest` | `Vec.<T>`, `HashMap.<K, V>`, and the `Hash` trait they need |

- **`std` ships with the compiler and is versioned with it** (your decision).
  `Session::default_std_path` registers it as a *fallback* exactly as `core` is,
  so `import <std/io>` resolves from a checkout with nothing registered, and
  `-L` / `--package std=` replaces it. It is still **not linked automatically**:
  nothing reaches it without an import naming it.
- **`sys` is the swap point.** Nothing above it names `libc`. A target with no C
  library is a second copy of that one file.
- **`#when` is still not needed** (your decision: wait until blocking). It came
  close once — the `open` flags differ between Linux and the BSDs, and
  `std/libc` branches on `core/target`'s generated `OS` instead. The branch is
  over a constant, so it folds.

### Three things Nest cannot say, all one line of C

`runtime/nest_runtime.c` grew from six functions to ten. Each is there because
the alternative is not writable:

- **`nest_errno`** — `errno` is a *macro*, expanding to `__error()` on macOS and
  `__errno_location()` on Linux. There is no symbol to declare.
- **`nest_open`** — C's `open` is **variadic**, and on arm64 a variadic argument
  is passed on the stack while a fixed one goes in a register. A three-argument
  declaration created files with whatever was on the stack for permissions; the
  first `fs.write` returned `EACCES`. `core/c` already names the remedy: a
  fixed-arity shim in C. **This is the only one.**
- **`nest_envp`** — returns **`environ`**, not `main`'s third parameter. `envp`
  is a snapshot and `setenv` replaces the table under it, so a program could not
  see a variable it had just set.

### The entry point now takes `argc` / `argv`

`main` took none, so `argv` existed for exactly one frame and was gone —
`std/process` had nothing to read. It now takes both and hands them to
`nest_init`, which keeps them.

**The parameters are pointer-sized integers, not `Ty::Ptr`.** `lir::safepoint`
treats every `Ptr` local as a GC root, and `argv` addresses memory the startup
owns; a moving collector would have relocated a pointer into C's stack. It is
the same answer `core/c` gives — a `c.ptr.<T>` is one `usize` in a struct,
untraced by construction.

## Five bugs, all found by writing `std` rather than by a test

1. **A `for` loop ran its body zero times.** `core`'s `Iterator` impls for
   `Range` and `SliceIter` were **stubs returning `.none`**. Every `for` in
   every program silently did nothing. Fixed with a `Step` trait — `step_cmp`
   and `step_up` — and **one** `impl <T: Step> Iterator for Range.<T>`.
   - It has to be one impl. A set per integer family would have to choose
     between them before the body was read, so `for x in 0..<4` would settle the
     literal on `isize` instead of inferring `i32` from its body.
   - It cannot be `Ord` + `Add`: on a primitive those are *instructions*, not
     calls, so an unbounded `T` has no way to ask and a bound naming them is one
     the primitives do not satisfy.
2. **`.?` propagated garbage.** `from_residual` is a static trait call: it
   resolves to the **trait's** declaration and the solver then redirects it to
   the impl member. The recorded `Instantiation` was the trait's — `FromResidual.<R>`
   owns one parameter — while `impl <T, E> FromResidual.<E> for Result.<T, E>`
   has two. `T` was left unbound, and building an `.err` of a type that does not
   exist lowered to **`return undef`**. `sema::lower::carry_instantiation` now
   drops the list when the callee was redirected, and mono re-derives it from
   the signature.
3. **A `never`-typed block expression got a value slot.** A `loop` with no
   `break` produced `let _1: never` and a dead `return _1`. The `never`-returning
   *call* path one function above already did this right; `lower_loop`, the `if`
   and the `match` now do too, and seal the block they leave behind.
4. **A sub-slice of a `[]mut T` was a `[]T`.** `mutable: false` was hardcoded, so
   writing to part of a buffer was unsayable — which is what every reader filling
   the tail of what it has read needs. A sub-slice now inherits the permission;
   an **array**'s does not, because `[N]T` carries no mutability in its type.
5. **`@link_name` did nothing.** Spec §9 documents it and `ir::mono::mangle`
   looks for it in `d.directives` — and `sema::collect` only ever put `#name`
   directives there, never `@` attributes. It is now recorded.

## What `std` needs from `core`, and got

- **`Eq` for the primitives** (`cmp.nest`). `a == b` on two integers stays an
  instruction; these are for a generic function with an `Eq` bound, which has no
  operand types to look at. Without them `T: Eq` is a bound no integer satisfies
  and `HashMap.<usize, V>` cannot be written.
- **`wrapping_mul`** (`num.nest`, plus the intrinsic table and one LIR case).
  A hash function is a multiply that is *meant* to overflow; under
  `-C overflow=trap` there was no way to write one.

## What `std` does not have

Each is a real thing to want, and each says what it waits on:

| Missing | Why, and what it needs |
|---|---|
| **Metadata, directory listing** | `struct stat` / `struct dirent` layouts differ between Linux and macOS — `d_name` is 19 bytes in on one and 21 on the other — and a wrong layout reads the wrong bytes rather than failing to compile. Wants `#when`, or the declaration generated the way `C_LONG` is. `fs.size` asks a descriptor instead |
| **Buffered I/O** | `io.File` writes straight through. The first thing `twig` will want that is not here |
| **An owned `String`** | `core/fmt`'s `Buf` and `Vec.<u8>` are each already a growable byte buffer. A third is a third spelling of one thing, and the `#lang` tag `core` reserves for it is a language feature rather than a container |
| **Iterator adapters** (§10.4) | `map`, `filter`, `enumerate`, `collect`. `for` works over ranges, slices and a `Vec` |
| **Arrays are not `IntoIterator`** | `for x in some_array` has no impl. Slices do |
| **Windows** | Everything here is POSIX. That is what `sys` being a layer is for |

## Still open, and yours to decide

| Decision | Why it is open |
|---|---|
| **What `.nlib` holds beside the code** | An archive of pre-generated IR *and* metadata, with the `.nmeta` alongside — your note in `toolchain.md` |
| **`Drop`** (`#lang("drop")`) | Waiting on a *decision*: with no moves, "this value was returned / stored / passed to a call, so do not drop it" has no settled answer. **`io.File` is the first type that wants it** — a descriptor is not closed for you |
| **Floats in `f"..."`** | `Display` has no float impl. Ryū is a few hundred lines and a table; it belongs with the float work |
| **Two declarations of one `@link_name`** | They collide and the mangler uniquifies one to `strlen.1`, which then does not link. Only reachable by writing two aliases of one C function, so it is noted rather than fixed |

## Not Yet Done (compiler)

- [ ] **A `void` member should be erased from a `TypeDef` too.** §9 erases `void`
      from slots, parameters and arguments but not from a type's members. **Not
      done because `Projection::Field` indices are positional.**
- [ ] **`-C opt-level` and `-C target-cpu`.** The backend hardcodes
      `OptimizationLevel::None`, `RelocMode::PIC`, `CodeModel::Default`.
- [ ] **Parallel code generation.** What is missing is a backend instance and an
      LLVM context per thread.
- [ ] **A library format** (`.nlib` / `.nmeta`). A dependency is source today.
- [ ] **Debug info.** Nothing emits DWARF. **Disableable by config** (your note).
- [ ] **A `defer` captures at registration, and this one does not** (§8.4).
- [ ] **`core.panic` prints nothing useful.** A trap says `nest: trap`. **`std`
      exists now**, so the `#lang("panic_handler")` a program may replace could
      finally print one — but `core` may not import `std`, so this is a decision
      about where the default handler lives rather than a missing function.
- [ ] **A trait must be imported by name for its impls to apply.** `{ Eq } ::
      import <core/cmp>` is needed; `cmp :: import <core/cmp>` is not enough.
      Pre-existing, and it bit again in `std/mem`.
- [ ] **Optimization across units**, per-function escape summaries, the
      object-start table, narrowing what counts as a root (§5/§6).

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs still stands. New this session:

- **Declaring a variadic C function with fixed arity.** On arm64 the arguments
  go in different places. `open` created files with garbage permissions and no
  error until the *next* run could not read them.
- **Reading the environment from `main`'s `envp`.** It is a snapshot; `setenv`
  replaces the table.
- **A field and an inherent method with the same name.** `Vec.len` the field
  hides `len()` the method, and the struct literal then reports "`Vec` has no
  field `len`". The field is `count`.
- **Naming several impls where one blanket impl would do**, when the type
  argument is still open. The choice forces a literal onto its default before
  the body has said what it is — which is what a `Step` bound avoids.
- **Building a C `char **` before the strings it points at are held somewhere
  traced.** The table holds addresses, which are integers, which the collector
  does not follow.
- **Letting a test's `Session` keep the default target.** `Options::default` is
  a 64-bit **Linux**, and `core/target.nest` is generated from it — so a program
  built by a test on macOS opened files with Linux's flag bits, where
  `O_APPEND` *is* `O_TRUNC`. Ask the backend: `session.options.target =
  backend.target_info(None)?.target()`.
- **A snapshot test with `codegen-units` near the file count.** `partition`
  merges the smallest groups until there are at most `n`, so the picture depended
  on how many files `core` happened to have. Pinned to 64, where nothing merges.

## Key Decisions

Everything in the previous handoff still stands. New this session:

| Decision | Rationale |
|---|---|
| `std` ships with the compiler, versioned with it | Your decision. One `std` per `nestc`, so a program never resolves which |
| It is a *fallback* path, not a link | `import <std/io>` resolves out of the box; `-L` replaces it; nothing reaches it unasked |
| `sys` is not re-exported | The backing is `std`'s own business; `<std/libc>` is the exposed raw surface |
| `#when` waits until it blocks | Your decision. The `open` flags branch on the generated `OS` instead |
| The runtime grows only for what Nest cannot say | Three functions: a macro, a variadic call, a hidden symbol |
| The entry point's `argv` is an integer, not a pointer | The collector must not trace C's stack |
| One `Iterator` impl for `Range`, over a `Step` bound | Several would settle the element type before the body was read |
| A sub-slice inherits the slice's permission | It is the same elements; anything else makes a partial write unsayable |
| An array's sub-slice does not | `[N]T` carries no mutability, so permission belongs to whatever holds it |
| `io.Error` carries the operation and the subject | `open "nest.toml": No such file or directory` without the caller assembling it |
| A failed `print` is ignored | A `Result` would put a `.?` on every line of output in every program |
| `spawn`'s `args` exclude `argv[0]` | The alternative is a list whose first element must be repeated |
| `HashMap` is open-addressed with linear probing | No allocation per entry, and the bucket is a mask |
| No owned `String` yet | `Buf` and `Vec.<u8>` are each already one |

## Current State

**Working**: everything. `cd nestc && cargo test` → **583**;
`LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 cargo test --features llvm` →
**600**.

**Broken**: nothing known.

**Uncommitted**: nothing.

## Files to Know

| File | Why it matters |
|---|---|
| `design/toolchain.md` | **The plan.** Step 6 now says what it turned out to be |
| `design/lir.md` | The specification. §7d overflow, §10 the backend's brief, §11 the unit split |
| `packages/std/sys.nest` | **The swap point.** The only file above `libc` that knows there is one |
| `packages/std/libc.nest` | The raw surface, and the four `open` flags that branch on the target |
| `runtime/nest_runtime.c` | Ten functions. `nest_open`, `nest_errno`, `nest_envp` are the new three |
| `nestc/src/lir/entry.rs` | `argc` / `argv`, and why they are integers |
| `nestc/src/sema/lower.rs` | `carry_instantiation` — the `.?` fix |
| `nestc/src/sema/collect.rs` | `link_name` |
| `nestc/src/sema/session.rs` | `default_std_path`, and the `std` registration beside `core`'s |
| `packages/core/range.nest` | `Step`, and the one `Iterator` impl |

## Resume Instructions

1. `cd nestc && cargo test` — expect **583 passed**.
2. With the backend:
   ```
   export LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21
   cargo test --features llvm          # expect 600
   cargo build --features llvm
   ```
3. **See the floor hold**:
   ```
   cat > /tmp/f.nest <<'EOF'
   io :: import <std/io>
   fs :: import <std/fs>
   process :: import <std/process>
   main :: func () -> i32 {
     fs.write("/tmp/f.txt", "alpha\n".as_bytes()).match { .ok(_) => (), .err(e) => { io.eprintln(e.describe())  return 1 } }
     io.print(fs.read_to_string("/tmp/f.txt").match { .ok(t) => t, .err(_) => "" })
     for a in process.args() { io.println(a) }
     return cast.<i32>(process.args().len())
   }
   EOF
   ./target/debug/nestc -o /tmp/f /tmp/f.nest && /tmp/f one two; echo $?   # 3
   ```
4. **Then step 7**, `std/json` — parse and serialize over any type through step
   5's **run-time** walk, with `@json(...)` for renaming. Everything it needs is
   built: `core/reflect` walks the type, `std/fs` reads the document,
   `std/collections` holds the object, and `std/str` reads the numbers.
   *Done when*: a struct round-trips, a renamed member honours its attribute,
   and a malformed document is an error rather than a trap.

## Edge Cases & Error Handling

Everything in the previous handoff still stands. New this session:

- **A range with no start panics when iterated.** `for i in ..<5` has no first
  element; answering `.none` would run the loop zero times and say nothing.
- **An inclusive range does not compute `b + 1`.** `0..=255` over a `u8` would
  trap on the iteration that is supposed to end the loop; reaching the end
  replaces the range with an empty one.
- **`sys` reads `errno` in the same statement that detects the failure.** Any
  later call may overwrite it.
- **A `Res` whose `n` is negative never has a zero `err`.** It would read as a
  success with a nonsense count everywhere above.
- **`fs.read_to_string` does not validate UTF-8**, on purpose — `str.from_utf8`
  is the check, and validating every file read costs a pass over it.
- **A `File` is not closed for you.** No destructors; the whole-file functions
  close their own, on both exits.
- **`process.run` answers 127 for a program that does not exist**, because the
  child `_exit`s with it after a failed `exec` — it must not return.
- **An empty `read`/`write` answers zero** rather than indexing `&s[0]` on a
  zero-length slice.

## User Notes (standing)

- **Ask questions in batches.**
- **If something can be simplified in LIR while building codegen, do it**, and
  pass this instruction on in every future handoff. `embed_file` is still the
  only intrinsic left on that list that is more than one instruction or one
  runtime call.
- **Casts should be explicit about which conversion they are.** Done.
- **The GC is not the priority.** An existing collector behind a thin shim.
- **Codegen is a trait**, and it supplies the target info. Done.
- **LLVM codegen uses inkwell and produces object files.** Done.
- **Everything should be easily usable by the future CLI tool.**
- **`nestc` before `twig`.** Done.
- **`std` is not linked automatically**; that is `twig`'s job. Done — it is a
  resolvable package, not a linked one.
- **`twig` is written in Nest**, so `std` needs fs, io and JSON serialization.
  fs and io exist; JSON is step 7.
- **`nestc` supports structured output as JSON.** Done.
- **There should be `insta` snapshot tests over the generated LLVM**, with no
  target-specific content. *Still not done.*
- **Conditional compilation** is wanted for building `std`. `#when` is not in
  the parser, the spec or the grammar — **deferred until blocking**, your
  decision, and it has not blocked yet.
- **One object file out of `nestc`, always.** Done.
- **A runtime loop over a type's members is fine.** Only typed access must unroll.
- **Debug info should be disableable by config.**
- **Be compact in commit comments; do not edit existing comments.**
- **If you are unsure or need further guidance, ask.**

## User Notes
- **Step 6 was asked for and is done and committed.**
- **`std` is versioned with the compiler** (your answer this session).
- **`#when` waits until it blocks** (your answer this session). It has not yet.
- The focus is now `std/json`, the library format, `twig` and the LSP — steps 7
  through 10.
