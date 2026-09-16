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

- **Varargs.** Decided, and then revisited once `std` had to call `open`.
  Nest has **no variadics of its own** — a function wanting many arguments takes
  a tuple, which is already sayable — but a **C** variadic is declarable with
  `#c_vararg`, because the alternative was a C shim per function and the first
  one was already written. The directive marks a declaration, never a
  definition: accepting a tail costs a flag on a signature, and *reading* one is
  `va_list`, which is per-target and which nothing emits.

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

**Only what `twig` and the language server need**, and nothing more. Decided:
`std` is not a general-purpose library yet — it is the floor those two programs
stand on, and what they turn out to want is what gets added. A library written
speculatively is a library nobody has yet had to use.

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

**The spelling is `#intrinsic`, not a sigil.** `$name` was retired in phase 3 and
an intrinsic is a **bodyless `#intrinsic` function declared in `core`** (§9),
called like any other function — `cast`, `size_of`, `make` and `transmute` all
work this way already. Reflection is the same: a `core/reflect.nest` of bodyless
declarations, and `@` stays what it is, the **attribute** sigil.

```nest
reflect :: import <core/reflect>

// core/reflect.nest — every one of these is compiler-supplied.
@public type_info :: #intrinsic("type_info") func <T> () -> TypeInfo
```

The shape:

- **`type_info.<T>()` returns a value** — ordinary data, and constant-folded
  because `T` is concrete after monomorphization. Its `members` are a slice of
  descriptors, and a descriptor's `kind` is an **enum**. So a loop over
  `type_info.<T>().members` is an ordinary loop over ordinary data, and can run
  at run time like any other: nothing about *reading the description* requires
  unrolling.
- What does require unrolling is **typed access to the value**: an accessor that
  yields a differently-typed value for each member can only be typed if the loop
  around it is unrolled. That is a property of the loop's body, not of the data —
  and reading a field by a **run-time** selector is a separate question, below.
- **User attributes are data on the same descriptors.** An `@attribute`
  declaration defines a struct; writing `@json(rename: "user_id")` on a member
  puts that struct in the member's `attrs`. No expansion pass, no generated
  code, no second program representation — an encoder reads them like any other
  field. This is a **new** §9 addition: today's attributes are a fixed set
  (`@public`, `@link_name`), and letting a program declare its own is the change.

### Reading a field chosen at run time — decided

**`member_ptr` plus an ordinary `cast`. No checked read.**

```nest
@public member_ptr :: #intrinsic("member_ptr") func <T> (v: *T, m: Member) -> *mut void
```

The address is `base + m.offset`, which LIR already computes for every static
field access, so the intrinsic is one instruction. The caller casts it to what
the descriptor said the member is, and `size_of` / `align_of` are already there
for anything that needs them.

**And the read is checked, through a `TypeId`.** The check itself was never the
hard part — it is the bounds check's lowering (§3.2): a comparison, an edge, and
a panic block. What was missing was a stable thing to compare *against*, and the
compiler already has one it does not expose: `ir::mono::type_key` is a globally
unique string per monomorphized type, computed whole-program rather than per
unit, and already what every symbol is mangled from.

```nest
@public type_id :: #intrinsic("type_id") func <T> () -> TypeId
```

- A `TypeId` is a **hash of that key**, folded to a constant at compile time.
  128 bits, as Rust's is, so a collision is not a correctness argument anyone
  has to have.
- A `Member` carries one, so `member_read.<R>(v, m)` is a constant compared
  against a loaded field and a branch — implementable today, over a function
  that already exists.
- **`distinct` survives it.** `type_key` keeps `distinct` and mutability even
  though LIR's `Cx::strip` erases both (§9), so `type_id.<usize>()` and
  `type_id.<u64>()` differ — which is the answer a checked read wants.
- It also gives an **`Any`**, over machinery that is already there: `*dyn Trait`
  is `{ data, vtable }` and a vtable already exists per (trait, concrete type).
  **A blanket `impl <T> Any for T` does not work today**, and has to before
  `Any` can exist: it type-checks and then fails in codegen
  (`Void is not a type a value can have`) — a concrete `impl Named for P` on the
  same trait runs correctly, so it is the blanket form specifically.

**This does not compete with the compile-time path.** A selector known at compile
time is the unrolled loop, statically typed, no check at all. A selector known
only at run time cannot be checked statically by definition — `TypeId` is what
makes *that* case safe rather than a bare `cast`.

**The GC hazard is pre-existing, not introduced here.** A member pointer is an
*interior* pointer — and `&mut p.y` is one too, so the language has had them
since `&` did. Boehm is conservative and traces interior pointers, so this works
today. A precise or moving collector needs an interior address mapped back to
the object it points into, which is exactly the **object-start table** already
open in §5/§6 — the same item, reached from a second direction.

**Unrolling is spelled `#comptime`.** Decided — a **directive**, in the family
`#inline` and `#intrinsic` already belong to, and deliberately not a statement
keyword: `comptime for` was rejected, and this is not that. It says the loop is
evaluated when the program is compiled, which is also what makes its body's
per-member typing possible.

---

## Stage 3 — `twig`: the package tool, written in Nest

**The name is `twig`.** It is a build tool in the cargo sense: a manifest, a dependency graph, a target directory, and
`nestc` invoked once per package.

**It is written in Nest**, which is the point of stages 1 and 2: the first real
program in the language is the one that builds programs in the language, and
whatever is missing from `std` will be discovered by needing it.

### What it does

1. Read `nest.toml` — the manifest, in TOML — naming the package,
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
- **A library format: `.nlib` and `.nmeta`.** Today a dependency is *source*,
  recompiled into every program that uses it. The pieces are already the right
  shape (a self-contained `Unit`, §11; a symbol scheme that does not depend on
  the split), but nothing serializes them.
  - `.nlib` is compiled code; `.nmeta` is metadata alone. The second matters
    most: it is what makes checking a downstream package cheap, and it is what a
    language server reads.
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
  - **It is written in Rust, beside `nestc`** — decided. Everything it answers
    is something only the compiler holds: spans, resolutions, the type on a
    node, the descriptors reflection builds, the diagnostics. A server in Nest
    could only reach them through `nestc` serializing all of it across a process
    boundary, which is a second protocol to design and keep in step with the
    compiler; in Rust it is a function call. The shape is a `nestc lsp`
    subcommand (or a crate using `nestc` as a library): a `Session` over a
    `MemLoader`-style overlay for unsaved buffers, diagnostics on change first,
    then hover from the type on a node and go-to-definition from a resolution's
    `Def.span`, then completion.
  - **It does not wait for `twig` after all.** Without a manifest a workspace is
    a guess about `-L` paths, and that guess is good enough to start: the
    manifest refines what the server loads, not whether it can.

---

## The order of work

Ten steps. **Each one is a whole feature**, each ends in a commit, and **work
stops after each commit** so the step can be reviewed before the next begins. A
step is done when its own tests pass and the full suite still does — no step
leaves a half-built feature behind for the next one to finish.

The numbering is a dependency order, not a wish list: every step needs the one
before it, except where it says otherwise.

**Steps 1–7 are done.** What is left is the library format, `twig` and the
editor — steps 8 through 10 — and the **language server comes first**, ahead of
steps 8 and 9 (see Stage 4). Each step
below that is finished says so and says what it actually turned out to be;
`HANDOFF.md` carries the detail.

### Step 1 — `repeat` and `format`, lowered — **done**

The standing instruction: when something can be simplified in LIR, do it.
`$slice` and `$array` were the first two, these are the last two intrinsics that
are more than "one instruction or one runtime call" (§10). `repeat` is a `make`
and a loop; `format` needs a formatter and an allocation.

**Independent of everything below** — it is here first because it is the
outstanding debt, and because `format` is what `std/io`'s `print` will want.

*Done when*: the intrinsic list is nine, the two variants are gone from the enum,
and the coverage test asserts their absence. **Commit. Stop.**

### Step 2 — a method call on a literal receiver — **done**

Written here as "blanket impls", which it was not: blanket impls worked. The
reproducer had two variables in it, and the one that mattered was the
**receiver** — `(5).tag()` left it an open `comptime_int`, a type no impl is
written for, so every lookup missed and the call lowered to `call (undef)()`
with no diagnostic. A literal receiver now settles on its default before the
lookup, and a lookup that finds nothing is an error.

### Step 3 — `core/c` — **done**

The C boundary as described above: the type aliases, `c.ptr.<T>`, `c.null`, and
the `c"..."` literal with its trailing NUL. **Varargs** arrived later, with
`#c_vararg` (stage 2's `open`). `extern("c")` already works; this is the
vocabulary to use it with.

*Done when*: a Nest program calls `write(1, ...)` through libc and the output
appears. **Commit. Stop.**

### Step 4 — `#comptime` loops — **done**

The directive that makes a loop evaluate at compile time, which is what lets its
body be typed per iteration. Needed by reflection's typed path and by nothing
else yet, which is why it is its own step rather than part of the next one: a
loop that unrolls is a language feature with its own failure modes (a bound that
is not constant, a body that cannot be typed at some iteration) and deserves its
own diagnostics.

*Done when*: `#comptime for` over a constant sequence unrolls, a non-constant
bound is an error at the loop rather than a mystery later, and the LIR shows the
unrolled form.

**Commit. Stop.**

### Step 5 — the whole reflection system — **done**

One step, because the parts are useless apart:

- `core/reflect.nest`: `TypeInfo`, `Member`, the `kind` enum.
- `type_info.<T>()` — the description as a constant-folded value.
- `type_id.<T>()` — a 128-bit hash of `ir::mono::type_key`.
- `member_ptr(v, m)` — `base + m.offset`, one instruction.
- The checked read: a constant `TypeId` against a loaded one, the bounds check's
  lowering.
- `Any`, over the `{ data, vtable }` machinery that exists (needs step 2).
- **User-defined `@attribute` declarations**, and their values on `Member.attrs`.
  This is the §9 change: today's attributes are a fixed set.

*Done when*: a test walks a struct's members at run time, reads each one through
a checked read, and a wrong type traps; and an `@attribute` written on a member
is readable from its descriptor.

**Commit. Stop.**

### Step 6 — the `std` floor — **done**

`packages/std/`, seven namespaces over an internal `sys`: `libc` raw at the
bottom, `sys` as the one file that knows there is a C library, and `io`, `fs`,
`process`, `mem`, `str` and `collections` above it. It **ships with the
compiler and is versioned with it** (decided), so `import <std/io>` resolves
with nothing registered; it is still not linked automatically, because nothing
reaches it without that import.

What it turned out to need, none of which was library code:

- **The entry point had no arguments.** `main` took none, so `argv` existed for
  exactly one frame and was gone. It takes `argc` / `argv` now — as pointer-sized
  *integers*, not `Ptr`, so the collector does not treat C's stack as a root.
  They went to `nest_init` at first, which kept them; they go to whoever claims
  `#lang("start")` now, which is `std/sys.start`, and `nest_init` is back to
  preparing the collector and nothing else. **The entry point is `std`'s** from
  that call on: it is handed the program's own `main` as a function pointer, so
  nothing outside the compiler has to spell a mangled symbol. The environment is
  read from `environ` instead, because `envp` is a snapshot and `setenv`
  replaces the table under it.
- **Two C things Nest cannot say.** `errno` is a macro, and `environ` is a
  symbol macOS hides behind a feature macro. Each is one line in
  `runtime/nest_runtime.c`. **`open` was a third** and is not any more:
  `#c_vararg` declares it as the variadic function it is, and the shim is gone.
  It is the case that paid for the directive — on arm64 a fixed argument arrives
  in a register and a variadic one on the stack, so a three-parameter
  declaration of `open` created files with whatever the stack held.
- **The `open` flags differ per target** — Linux's `O_APPEND` is macOS's
  `O_TRUNC` — so `std/libc` branches on `core/target`'s `OS`, which the compiler
  generated for this build. That is the one place `#when` would have replaced
  real code rather than a comment.

*Done when*: a Nest program reads a file, writes to stdout, spawns a process and
reads its arguments and environment. **It does** —
`the_std_floor_reads_writes_spawns_and_reads_its_arguments` is that program.

**What `std` does not have**, and what each waits on:

- **Metadata and directory listing.** `struct stat` and `struct dirent` have
  layouts that differ between Linux and macOS — `d_name` is 19 bytes in on one
  and 21 on the other — and a wrong layout reads the wrong bytes rather than
  failing to compile. `fs.size` asks a descriptor instead. This wants `#when`,
  or the one declaration generated the way `C_LONG` is.
- **An owned `String`.** `core/fmt`'s `Buf` and `Vec.<u8>` are both already a
  growable byte buffer; a third is a third spelling of one thing.
- **Buffered I/O.** `io.File` writes straight through. A `BufWriter` is the
  first thing `twig` will want that is not here.
- **Iterator adapters** (§10.4): `map`, `filter`, `enumerate`, `collect`. `for`
  works over ranges and slices, and over a `Vec` through its `IntoIterator`.

### Step 7 — `std/serialize`: JSON and TOML — **done**

Parse and serialize, generic over any type, using step 5. It grew from `std/json`
into two formats over one pair of traits, the way serde has them:

- **`Encoder` / `Decoder` are the format, `Encode` / `Decode` the type.** Every
  type already has the second pair through a blanket impl that walks
  `type_info.<T>()`: a struct is a map of its members, a tuple a sequence, and a
  `distinct` its representation. What makes that work without a derive is
  `member_dyn`, which hands a member back as a trait object whose vtable is that
  member's own impl. A concrete impl beats the blanket one.
- **The attributes are format-neutral**: `@rename(name: "...")` and `@skip`.
- **Decoding is strict about what is missing and loose about what is extra**: a
  missing key is an error unless the member is an `Option`, and an unknown key
  is skipped.
- **JSON** is a streaming writer (compact and pretty) and a pull parser. **TOML**
  goes through a tree both ways, because a table's text is not in the order
  its values are: tables, dotted keys, basic and literal strings, integers in
  every base, floats, arrays, inline tables and `[[arrays of tables]]`; no
  multi-line strings or dates. TOML has no null, so a `.none` is left out.
- **Floats go through C** — `snprintf` at rising precision until `strtod` reads
  the same bits back — until `core/fmt` has shortest float formatting.

What it needed from the compiler: every file its own namespace (two `Error`s in
one package were one type), and two monomorphization fixes — a call through a
bound on a `self: *Self` method picked the blanket impl at `T = *X`, and the
`int.<N>` and `uint.<N>` impls of one trait mangled to one symbol.

*Done when*: a struct round-trips through JSON, a renamed member honours its
attribute, and a malformed document is an error rather than a trap. **It does**,
through TOML too — `a_struct_round_trips_through_json` and
`a_struct_round_trips_through_toml`.

### Step 8 — `.nlib` / `.nmeta`, and the rest of `nestc`'s debt to `twig`

- The library format: `.nlib` (archive format that combines pre-generated IR and metadata), and `.nmeta` (metadata alone), so
  a dependency stops being source recompiled into every program.
- `-C opt-level`, running LLVM's pass manager.
- `-C target-cpu`.

*Done when*: a package compiles to a `.nlib`, a second package compiles against
its `.nmeta` without reading its source, and the two link into a program.
**Commit. Stop.**

### Step 9 — `twig`

The package tool, **written in Nest**: `nest.toml`, dependency resolution, the
target directory, and `nestc` invoked per package with `--package`, `-C` and
`--error-format=json`. The first real program in the language.

*Done when*: `twig build` builds a package with a dependency, and `twig run`
runs it.

**Commit. Stop.**

### Step 10 — the editor

Syntax highlighting first (it needs nothing), then the language server, **in
Rust**, and **before steps 8 and 9** (see Stage 4).

*Done when*: a `.nest` file is highlighted in VS Code, and the server reports
diagnostics for an open buffer.

**Commit. Stop.**

### Not scheduled

**The rest of `#comptime`** — step 4 unrolls a range of integer literals, which
is what the parser can read where it rewinds. Two sequences a program will want
are still out of reach, and they are not the same size:

  - `.{ a, b, c }` needs each element's token range recorded and re-parsed. It
    is the small one, and it belongs wherever a program first wants it.
  - `type_info.<T>().members` needs a sequence that is only constant **after**
    monomorphization, which is a compile-time evaluator over the IR and a
    feature of its own. Nothing below needs it: the *run-time* walk is what
    `std/json` uses, and it works today.

**Parallel code generation** — the units are independent and the merge is in
place, so what is left is a backend instance and an LLVM context per thread. It
belongs wherever compile times start to hurt, which is probably during step 9.

**Debug info** — DWARF, from the spans and origins LIR already carries. It
belongs wherever debugging `twig` stops being possible by printing. Debug infos should be disable'able via config.

---

## Open decisions

Nothing below is assumed anywhere in this plan.

| Decision | Why it is open |
|---|---|
| **Whether `std` is versioned with the compiler** | Rust ships one std per compiler; a package tool could resolve it like any dependency |
| **What `.nlib` holds beside the code** | Whether the metadata is duplicated inside it or only in the `.nmeta` |
| **Blanket impls** | `impl <T> Trait for T` type-checks and fails in codegen. Required before `Any` — see stage 2 |
