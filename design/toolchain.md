# The toolchain, planned

**Status**: a plan, not a record. Everything here is unbuilt unless it says
otherwise. `design/roadmap.md` is what *has* been built; this is the order the
rest of it goes in and the decisions each step rests on.

The shape of the whole thing, in one line:

```
core/c  →  std  →  twig  →  the editor (syntax, then an LSP)
```

Each arrow is a real dependency, not a preference. `std` cannot open a file
without C declarations; `twig` is written in Nest and cannot be written without
`std`; the language server needs a manifest to know what a workspace *is*, and
the manifest is `twig`'s.

---

## Stage 1 — `core/c`: the C boundary

Spec §11 describes this and nothing implements it. It is the smallest thing that
unblocks everything else: without it a Nest program cannot call `open`, and
`std` is a library with nothing underneath it.

### What it is

`packages/core/c.nest`, imported as `c :: import <core/c>` — a file of the
`core` package, so it needs no new package machinery.

- **The C types are aliases, not `distinct` types.** `c.int` is a name for the
  target's `i32`, `c.long` for its `i64` or `i32`, `c.size_t` for `u64` or
  `u32`. They are aliases because the point is that a language value *goes in* —
  a `distinct` type would make every C call a cast, and the width question is
  answered by the target, which the compiler already knows (`Options::target`,
  and `core`'s generated `target.nest`).
  - The spec text says these are "distinct nominal types". **This plan
    contradicts it deliberately**, on the user's decision: aliases now, and the
    distinctness revisited if a real ABI difference turns up that the alias
    cannot express. §11.4's "coercions" then become nothing at all, which is the
    simplest form a coercion can take.
- **`c.ptr.<T>` is the raw, nullable, non-GC pointer.** This one *is* its own
  type: `*T` is non-null and traced, and erasing that difference is how a null
  reaches code that cannot see one. `c.null`, and `cast.<*T>(p)` trapping on
  null, are §11.2 as written.
- **C strings are the one special case.** A `c.str` is a pointer to
  NUL-terminated bytes; a Nest `str` is `{ ptr, len }` and is **not**
  NUL-terminated. The two do not convert implicitly in either direction, because
  one of them is missing information the other has:
  - `c"hello"` is a literal whose bytes end in a NUL and whose type is `c.str`.
    A constant C string therefore costs nothing at run time.
  - `c.cstr(s)` turns a `str` into a `c.str` — an allocation and a copy, because
    the NUL has to go somewhere.
  - `c.from_cstr(p)` turns a `c.str` into a `str` — a scan for the NUL.

### What has to change in the compiler

- A `c"..."` literal: one lexer token, one AST literal, and a lowering that emits
  the bytes plus a trailing NUL as a read-only global (the machinery a `str`
  literal already uses — `Cx::data_global`).
- `extern("c")` **already works** end to end: the parser takes it, LIR carries
  `extern_abi`, and the LLVM backend emits a declaration. What is missing is only
  the *types* to declare a C function with.

### Open questions for this stage

- **Varargs** (`printf`). Not in the spec, not in the parser. A `printf` is the
  first thing anyone tries; the alternative is that `std` only ever declares
  fixed-arity C functions and does its own formatting, which it will do anyway.
  Recorded as a decision, not assumed either way.

---

## Stage 2 — `std`: a library worth writing a program against

A package, `packages/std/`, imported as `std :: import <std>`. **Not linked
automatically**: `twig` is what puts it on the command line, and a program that
does not ask for it does not get it.

### What it sits on

**libc**, through `core/c`. Decided: the runtime already links libc (`cc` is the
linker driver), it is one implementation across Linux, macOS and the BSDs, and
it makes stage 1 the only thing standing between here and a useful library.

**And `std` exposes the raw libc surface too**, in a namespace of its own —
`std/libc` — rather than hiding it. Two reasons, and the second is the one that
matters: a program that needs an `ioctl` this library never wrapped should not
have to re-declare it, and the wrappers above are then demonstrably ordinary
Nest code rather than compiler magic.

The public API is written so the backing implementation is **swappable**: an
internal `sys` namespace that everything else goes through, so that a syscall
backend for a target with no libc is a file rather than a rewrite. Only the libc
backend is written now.

### What is in it

The list is what `twig` needs, because `twig` is the first real program:

| Namespace | What for |
|---|---|
| `std/io` | readers and writers, stdout/stderr, buffering, `print` |
| `std/fs` | open, read, write, metadata, directories, paths |
| `std/process` | spawn, wait, exit status, environment, arguments |
| `std/mem` | allocation beyond `new`/`make`, copying, comparing |
| `std/str` | the operations `core`'s `str` does not carry |
| `std/collections` | `Vec`, `HashMap` — whatever `core` does not already define |
| `std/json` | parse and serialize, the thing reflection is *for* |
| `std/libc` | the raw declarations, unwrapped |

### What `std/json` needs, and what that needs

Serializing a `T` means walking `T`'s members without knowing them, which is
what **reflection** is. The decision is: **compile-time reflection *and*
user-defined attributes, with attributes visible as data on the type
information**. There is no `comptime` keyword.

The shape:

- `@typeinfo(T)` is a **value** — ordinary data, available at compile time
  because `T` is known after monomorphization. Its members are a slice of
  descriptors, and a descriptor's `kind` is an **enum**. So a loop over
  `@typeinfo(T).members` is an ordinary loop over ordinary data, and can happen
  at run time like any other: nothing about *reading the description* requires
  unrolling.
- What does require unrolling is **typed access to the value**: `@member(v, m)`
  has a different type for each `m`, so the loop that reads a struct's fields
  out of a real value has to be unrolled to be typed at all. That is a property
  of the loop's body, not of the data.
- **User attributes are data on the same descriptors.** An `@attribute`
  declaration defines a struct; writing `@json(rename: "user_id")` on a member
  puts that struct in the member's `attrs`. No expansion pass, no generated
  code, no second program representation — an encoder reads them like any other
  field.

**Decisions still open here** (see the bottom of this file): how the unrolling is
spelled, and whether a `#unroll` directive on ordinary loops is the same feature
or a different one.

---

## Stage 3 — `twig`: the package tool, written in Nest

The name is not settled (`hatch` is the other candidate). It is a build tool in
the cargo sense: a manifest, a dependency graph, a target directory, and
`nestc` invoked once per package.

**It is written in Nest**, which is the point of stages 1 and 2: the first real
program in the language is the one that builds programs in the language, and
whatever is missing from `std` will be discovered by needing it.

### What it does

1. Read a manifest (`twig.toml`? — see the open decisions) naming the package,
   its version, its dependencies and its targets.
2. Resolve dependencies to paths on disk.
3. Compile each package with `nestc`, in dependency order:
   - `--package name=path` for each dependency — never `-L`, because twig has
     already resolved them and a search would be a second, weaker answer.
   - `-C` for everything the profile decided: `overflow`, `codegen-units`,
     `opt-level`, `profile`.
   - `--error-format=json`, and render the diagnostics itself (or forward
     `rendered` verbatim, which is why that field exists).
   - `--emit obj` for a library, `--emit link` for a program.
4. Link, or hand `nestc` the objects to link, with `-C link-arg=` for whatever
   the manifest said about native libraries.

### What `nestc` still owes it

- **`-C opt-level`**, and LLVM's pass manager run behind it. The backend
  currently hardcodes `OptimizationLevel::None`.
- **`-C target-cpu`**.
- **A library format** — the `rlib`/`rmeta` equivalent. Today a dependency is
  *source*, recompiled into every program that uses it. The pieces are already
  the right shape (a self-contained `Unit`, §11; a symbol scheme that does not
  depend on the split), but nothing serializes them.
  - `rmeta` — metadata alone — is the one that matters most, because it is what
    makes checking a downstream package cheap, and it is what a language server
    wants to read.
- **Incremental compilation**, eventually. Not before the above.

---

## Stage 4 — the editor: syntax, then a language server

**This waits for `twig`**, on the user's decision, and the reason is the
manifest: a language server's first question is "what is this workspace, and
what does this file belong to", and without a manifest every answer is a guess
about `-L` paths.

Two pieces, and only the second one waits:

- **Syntax highlighting** — a TextMate grammar for VS Code, or a tree-sitter
  grammar. Needs nothing from the compiler. It is worth doing as soon as there
  is a `std` to read, because everything after this point involves reading a lot
  of Nest.
- **The language server.** Diagnostics, hover, go-to-definition, completion. The
  compiler's shape is already most of the way there: `Session` takes a
  `FileLoader`, so an editor's unsaved buffers are an overlay rather than a
  special case; `parse_cached` is keyed by a stable source key; and
  `--error-format=json` is a protocol a server can consume without a linker.
  - **It may be written in Nest itself**, which would make it the second real
    program in the language and the first one with a reason to be long-running.
    That is a decision to make when the time comes, and it is a real one: an LSP
    in Nest cannot call into `nestc`'s own data structures, so it either drives
    `nestc` as a process (fine for diagnostics, poor for hover) or grows a
    parser of its own.

---

## Open decisions

Nothing below is assumed anywhere in this plan.

| Decision | Why it is open |
|---|---|
| **C varargs** (`printf`) | Not in the spec or the parser. `std` can avoid needing them; a program calling C cannot always |
| **How unrolling is spelled** | Iterating a struct's *values* by member must unroll to be typed. Is that implicit (a loop over a compile-time-known sequence unrolls when its body demands it), or written (`#unroll`)? |
| **`#unroll` on ordinary loops** | A hint, like `#inline` — the same directive as above or a different feature with the same name? |
| **The manifest's name and format** | `twig.toml` and TOML, or something the language can read with `std/json`? A tool written in Nest needs a parser for whatever this is |
| **The tool's name** | `twig` or `hatch` |
| **Whether `std` is versioned with the compiler** | Rust ships one std per compiler; a package tool could resolve it like any dependency |
| **What an `rlib` contains** | Objects plus metadata in one file, or two files as Rust has them |
