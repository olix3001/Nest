# Handoff: Nest compiles to native object files, and they run

**Generated**: 2026-09-13
**Branch**: `main`
**Status**: **544 tests pass** without LLVM, **558** with it. `cargo clippy`
reports the same dead-code-shaped set as before. **All ten files in
`examples/*.nest` emit object files.** A linked program returns the right answer
and `overflow=trap` produces a real run-time trap. Everything is committed; the
tree is clean.

**Read `design/lir.md` §10 first** — it is the whole brief a backend answers, and
it shrank this session (eleven intrinsics, not thirteen). Then
`nestc/src/codegen/mod.rs` for what a backend *is*.

## What happened this session

Four commits, in order:

1. `609d320` — **a conversion names its instruction** (`CastKind`).
2. `596e5a4` — **a backend is a trait** (`Codegen`), and the driver that drives
   one (`-o`, `--target`, `--emit`, `-C backend=`).
3. `4a6318b` — **the LLVM backend**, the C runtime, and the first Nest program
   that runs.
4. `636503f` — **a slice does not take a range**, and `$slice`/`$array` are
   lowered away entirely.

### The proof it works

```
$ nestc --emit obj -o prog.o prog.nest
$ cc -c runtime/nest_runtime.c -o rt.o
$ cc drive.c prog.o rt.o -o prog && ./prog
sum(1..5) = 10
```

and with `overflow=trap` (the default), an overflowing add really traps:
`nest: trap`, exit 134. Structs, loops, matches, dynamic dispatch, slices, slice
literals and `core`'s `Option`/`Result` all compile.

## Completed

### `CastKind` — a conversion names its instruction (`609d320`)

`Rvalue::Cast` carries `kind` beside `from`/`to`: `trunc`, `zext`/`sext` by the
**source's** signedness, `fptrunc`/`fpext`, `fptosi`/`fptoui` by the
**destination's**, `sitofp`/`uitofp`, `reinterpret`, `ptrtoint`, `inttoptr`,
`ptrcast`. Decided by `CastKind::of` at lowering and nowhere else, because a
backend re-deriving it from the type pair is a second copy of a rule with two
easy corners to get wrong. `CastKind::Unknown` is a pair with no case, and two
tests say no program contains one and that every recorded kind is the rule's.

### `Codegen` — a backend is a trait (`596e5a4`)

```rust
pub trait Codegen {
    fn name(&self) -> &'static str;
    fn target_info(&mut self, triple: Option<&str>) -> Result<TargetInfo, CodegenError>;
    fn emit_unit(&mut self, unit: &Unit, kind: OutputKind, out: &Path) -> Result<(), CodegenError>;
    fn extension(&self, kind: OutputKind) -> &'static str { … }
}
```

- **The backend answers what machine this is, first.** `ir::layout` and `core`'s
  generated `target.nest` both read the target and both run long before code is
  generated, so the driver resolves `--target` (or the host) *through the
  backend* and writes it into `Options` before analysis. `-C pointer-width` /
  `os` / `arch` stay as **overrides** applied on top. Builds are no longer laid
  out for a hardcoded `x86_64-linux`.
- `target_info` takes `&mut self` deliberately: resolving a target **configures**
  the backend, and one that could not remember the triple would emit for the host
  whenever `--target` was given.
- **One unit at a time**, with the path the driver's, so parallel emission stays
  a scheduling question.
- **A second backend exists**, `lir`, which writes the LIR dump. Not because
  anyone wants a `.lir` file — a trait with one implementation is shaped like
  that implementation, and this is what a build with no system LLVM has.

### The LLVM backend (`4a6318b`) — `nestc/src/codegen/llvm/`

`--features llvm`, **off by default**. LLVM 21 via inkwell 0.10, needs
`LLVM_SYS_211_PREFIX`. On this machine: `/opt/homebrew/opt/llvm@21` (21.1.8).

Four decisions, each in the module comment of `unit.rs`:

- **A `bool` is an `i8` everywhere.** LLVM's `i1` is a register type whose
  in-memory size is a byte, and a member that is sometimes one bit and sometimes
  one byte gives a wrong offset rather than an error. The `i1` a comparison
  produces is widened at once and never stored.
- **Every access is a byte offset.** LIR decided the layout, so a projection is a
  `getelementptr i8`. Aggregates are still *declared* as packed structs with
  their padding written out, so the IR reads and a global's initializer is an
  ordinary constant — but nothing indexes them by member number. Two layout
  engines that have to agree is the failure this avoids, and LLVM's would be the
  one that won silently.
- **An aggregate is never an LLVM value.** A struct, an array, a variant and a
  checked result are each written member by member to their offsets.
- **No GC roots are emitted**, because the collector is conservative (below).

`module.verify()` runs on **every** emission, so a module LLVM would reject is a
build failure carrying LLVM's own message rather than a bad object file. That is
what found two of the four bugs below.

### The runtime — `runtime/nest_runtime.c`

Six functions: `nest_alloc`, `nest_free`, `nest_gc_collect`, `nest_init`,
`nest_trap`, `nest_assert`. Boehm with `-DNEST_GC_BOEHM`, `calloc` without.

**The shim is thin on purpose.** `new` and `make` do call the collector's
allocator — the indirection exists so that *which* collector is linked is a
**link-time** choice: the Boehm build and the leaking build produce the same
object files from the same compiler. Go and OCaml both put a shim here.

**Boehm is conservative**, which is why §6's precise live sets are computed and
not emitted: it scans the stack itself and would ignore a shadow stack. Those
live sets are what a precise or moving collector needs, and this shim is what
makes that swap cheap.

### `$slice` and `$array` are lowered away (`636503f`)

The user's instruction was *"If you feel like taking a slice should take
something that is not a range, then change it. LIR should not have too much
abstraction."*

- **`sema::lower` decomposes the range.** `parse_index_or_slice` builds a `Slice`
  node only when the index is *syntactically* a range, so which of the six forms
  was written is known at lowering. All six become the same two bounds, **both
  present and both exclusive**: a missing start is `0`, a missing end is `$len`,
  and `..=b` is `b + 1`. One convention downstream instead of a tag and a flag.
- **`lir::lower` finishes it.** Three plain numbers in, so the pointer is an
  `Offset` and the length is a `sub` — a slice is an `Aggregate` over two
  operands and no intrinsic at all:
  ```
  _6 := &a_2
  _7 := _6 + 0 * 4
  _8 := sub.u64 3, 0
  _9 := []i32(_7, _8)
  ```
- **A slice literal is a `make` and a store per element.**
- **The intrinsic list went from thirteen to eleven**, and `Intrinsic::Slice` /
  `Intrinsic::Array` are gone from the enum. The `#lang("range")` enum still
  exists and is still what `for i in 1..<3` iterates — it was only ever ceremony
  for *slicing*.

### Four bugs the backend found that nothing else could

- **A dynamic dispatch passed the whole fat pointer.** A `*dyn Trait` is
  `{ data, vtable }` and the vtable slot's type is `func(*void, …)` — `slot_ty`
  erases the receiver precisely because every implementation takes the data
  pointer — but the call passed the pair. Two words against a one-word parameter.
  It reads fine in a LIR dump; LLVM's verifier rejected it immediately.
- **A `-> void` function returned `undef`.** §9 erases `void` from every slot,
  parameter and argument; the terminator was the one place it had not reached.
- **A zero-byte member was stored.** A `ControlFlow.<void, T>`'s `stop` payload
  is typed `void`, so building the variant handed `undef` to a type no register
  holds. A member of no size is written by writing nothing — which is correct for
  any zero-sized type, not a special case for `void`.
- **A test helper raced two emissions onto one temp file** (two tests compiling
  the same source for different reasons).

## Not Yet Done

- [ ] **`repeat` and `format` still owe the `$slice` treatment.** Both are more
      than "one instruction or one runtime call", which §10 claims of an
      intrinsic. `repeat` is a `make` and a loop; `format` needs a formatter and
      an allocation. **This is the standing instruction: when something can be
      simplified in LIR while building codegen, do it, and carry that instruction
      into the next handoff.**
- [ ] **A `void` member should probably be erased from a `TypeDef` too.** §9
      erases `void` from slots, parameters and arguments but not from a type's
      members, so `ControlFlow.<void, i32>.stop` has a `0: void` member of no
      size. The backend now skips zero-byte members, which is correct on its own
      terms — but erasing them in LIR is the principled fix. **It was not done
      because `Projection::Field` indices are positional**, so dropping a member
      shifts every index after it; that needs care and its own test pass.
- [ ] **No `main` wrapper, so nothing calls `nest_init()`.** A program is linked
      by hand against a C driver today (see *Resume Instructions*). The CLI phase
      is what generates an entry point.
- [ ] **Linking.** `nestc` emits objects; it does not invoke a linker. That is
      the CLI phase too.
- [ ] **`-C opt-level`.** The backend hardcodes `OptimizationLevel::None`,
      `RelocMode::PIC`, `CodeModel::Default`.
- [ ] **Debug info.** Nothing emits DWARF, though every statement carries a span
      and every `TypeDef` carries its origin and its pre-flattening name.
- [ ] **A `defer` captures at registration, and this one does not.** Spec §8.4:
      `let mut i := 1; defer side(i); i = 2` calls `side(2)`. The position half is
      fixed; this half is not.
- [ ] **The `Drop` trait** (`#lang("drop")`). **Waiting on the user's decision,
      not on work**: with no moves, "this value was returned / stored / passed to
      a call, so do not drop it" has no settled answer, and a wrong one either
      double-releases or never releases.
- [ ] **Optimization across units**, the cost of the split and why the default is
      1. The three §5/§6 items also still open: per-function escape summaries,
      the object-start table, narrowing what counts as a root.

## The CLIs, recorded and deliberately not built

The user specified these and asked that they **not** be built yet, and that the
information be carried forward in every handoff.

**Two tools.** `nestc` is the compiler. `twig` (name not settled — `hatch` is the
other candidate) is the cargo-like package tool that drives it.

**What `twig` needs from `nestc`, and what therefore has to exist:**

- **Structured output.** Errors and diagnostics as **JSON**, so `twig` can
  consume them rather than parse text. Today `common::emitter::render` produces
  human text only.
- **`-L <path>`** — search paths, specified manually.
- **`-C` for codegen settings**, which already exists and stays the interface:
  `codegen-units` (built), `overflow` (built), plus `opt-level`, `target-cpu`,
  and the rest.
- **A target triple**, which `--target` now does.
- **A library format**: the equivalent of Rust's `rlib` (compiled code) and
  `rmeta` (metadata alone), so packages can be pre-compiled. **Do not implement
  this yet** — but everything built should be usable by it. The codegen-unit
  split (§11) and the self-contained `Unit` are already the right shape.

**The driver as it stands** (`nestc --help`):

```
usage: nestc [options] <file.nest>

  -o <path>              output; with several units, the base each name is appended to
  --target <triple>      the machine to generate code for (default: the host)
  --emit <list>          ast, ir, mono, lir  (dumps, to stdout)
                         obj, asm, backend-ir (backend files)
                         default: ast,ir,mono,lir
  -C <key>=<value>       backend=, codegen-units=, overflow=, pointer-width=,
                         os=, arch=, profile=, print=options
```

**`--emit` still defaults to the four dumps**, which is today's behaviour and not
a decision. The line to change when an object should be the default is
`Emit::default` in `nestc/src/main.rs`.

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs still stands. New this session:

- **Deriving a cast's instruction from its type pair in the backend.** A widening
  reads the **source's** signedness and a float-to-integer reads the
  **destination's** — opposite sides, and every backend would get one of them
  wrong eventually. The rule runs once, in `CastKind::of`.
- **Comparing LIR widths to pick a cast instruction.** A `bool` is one bit to LIR
  and one byte here, so `bool -> u8` is recorded as a zero extension and must
  emit *nothing*. The backend compares the **LLVM** widths, which turns that into
  an identity instead of an illegal `zext i8 to i8`.
- **Letting the backend re-derive layout by building LLVM struct types and
  indexing them by member number.** Two layout engines that have to agree, and
  LLVM's wins silently. Every access is a byte offset LIR already computed.
- **An `i1` anywhere but as a comparison's immediate result.**
- **`target_info(&self, …)`.** The backend has to *remember* the triple it
  resolved, or `emit_unit` silently emits for the host whenever `--target` was
  given. It takes `&mut self`.
- **An intrinsic that needs a branch.** `$slice` took a six-variant enum. If a
  new intrinsic cannot be one instruction or one runtime call, it belongs in the
  lowering — which is also the *cheap* place, because every backend gets it once.
- **Leaving an enum variant behind for something that is now lowered away.**
  `Intrinsic::Slice` would be a case every backend must match and nothing can
  produce. The coverage test asserts their **absence**.
- **Hashing only the source for a test's temp file name.** Two tests compile the
  same source to check two different things, and the harness runs them on
  different threads.
- **`cfg` on an expression inside a `vec![]`** — not stable; `backends()` pushes.

## Key Decisions

| Decision | Rationale |
|---|---|
| A cast names its instruction | Many conversions between two numbers, and the two signedness rules read opposite sides |
| A backend is a trait, and the driver never names a concrete one | Adding a backend should touch one module's `select`, not the driver |
| LLVM decides the machine, `-C` overrides it | A table here mapping triples to pointer widths is a second source of truth able to drift from the one that matters |
| A triple LLVM knows but this compiler has no *name* for is refused | The names reach source through `core`'s `target.nest`; guessing produces a `core` that does not compile |
| Code generation is behind a feature flag | A compiler whose test suite needs a system LLVM is one most people cannot build |
| A second backend exists that is not LLVM | A trait with one implementation is shaped like that implementation |
| A `bool` is an `i8` | An `i1`'s in-memory size is a byte; a member that is sometimes one and sometimes the other gives a wrong offset, not an error |
| Every access is a byte offset | LIR already ran the layout engine |
| Aggregates are packed with explicit padding | So the declared type *is* LIR's layout, and a global's initializer is an ordinary constant |
| No aggregate is ever an LLVM value | Three rvalues produce something with more than one part; a detour through a form no target has helps none of them |
| `module.verify()` on every emission | LLVM's message naming the instruction is the whole diagnostic |
| Every *defined* function gets External linkage | The split is free to put a private function's definition in a different unit from its caller; what keeps it sound is that every symbol is defined exactly once |
| The runtime is C, and thin | Swapping the collector becomes a link-time choice; the object files do not change |
| No GC roots are emitted | Boehm is conservative and scans the stack itself. The live sets are for the collector after it |
| A slice takes two bounds, not a range | The syntax already knew which form it was; a tag would make every backend rediscover it |
| Both bounds are present and exclusive | One convention downstream instead of a tag plus a flag |
| A zero-byte member is written by writing nothing | Correct for any zero-sized type, not a special case for `void` |

## Current State

**Working**: everything. `cd nestc && cargo test` → **544**;
`LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 cargo test --features llvm` →
**558**. All ten examples emit object files at every `-C codegen-units` setting.

**Broken**: nothing. The open divergences are in *Not Yet Done* and are all known.

**Uncommitted changes**: none.

## Files to Know

| File | Why it matters |
|---|---|
| `design/lir.md` | The specification. §10 is the backend's brief — eleven intrinsics now, and the rule they keep earning. §11 is the unit split. |
| `design/roadmap.md` | Phase 10 is this work. |
| `nestc/src/codegen/mod.rs` | The `Codegen` trait, `TargetInfo`, `OutputKind`, `CodegenError`, `backends()`, `select()`. |
| `nestc/src/codegen/llvm/mod.rs` | Target resolution, the `TargetMachine`, and writing the file. |
| `nestc/src/codegen/llvm/unit.rs` | **The translation.** The four design notes are its module comment. |
| `nestc/src/codegen/text.rs` | The non-LLVM backend, and the shape test for the trait. |
| `nestc/src/main.rs` | The driver: `-o`, `--target`, `--emit`, `-C`, and `write_units`. |
| `runtime/nest_runtime.c` | Six functions, and the comment explaining why a shim exists. |
| `nestc/src/lir/mod.rs` | `CastKind` and its `of`; `Intrinsic` and why two members left. |
| `nestc/src/lir/lower.rs` | The `"slice"` and `"array"` cases; `vtable_slot` and the `.data` receiver; `returned`. |
| `nestc/src/sema/lower.rs` | `lower_slice` and `bound_ty` — where a range stops existing. |

## Resume Instructions

1. `cd nestc && cargo test` — expect **544 passed**.
2. With the backend:
   ```
   export LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21
   cargo test --features llvm          # expect 558
   cargo build --features llvm
   ```
3. **See a program run**:
   ```
   cat > /tmp/prog.nest <<'EOF'
   @public nest_main :: func (a: i32, b: i32) -> i32 {
     let mut total := 0
     let mut i := a
     while i < b { total = total + i; i = i + 1 }
     return total
   }
   EOF
   cat > /tmp/drive.c <<'EOF'
   #include <stdio.h>
   int _NC9nest_main(int a, int b);
   int main(void) { printf("%d\n", _NC9nest_main(1, 5)); return 0; }
   EOF
   ./target/debug/nestc --emit obj -o /tmp/prog.o /tmp/prog.nest
   cc -c ../runtime/nest_runtime.c -o /tmp/rt.o
   cc /tmp/drive.c /tmp/prog.o /tmp/rt.o -o /tmp/prog && /tmp/prog      # 10
   ```
   The symbol is mangled (`_NC9nest_main`) because nothing here is
   `extern("c")` — that, and a generated entry point, is the CLI phase.
4. See the LLVM IR for anything: `nestc --emit backend-ir -o /tmp/x.ll file.nest`.
5. **Next, in rough order of value:**
   - `repeat` and `format`, lowered the way `slice` and `array` were.
   - Erasing `void` members from `TypeDef` (read *Not Yet Done* first — the
     index shift is the hazard).
   - `-C opt-level`, and running LLVM's pass manager.
   - Then the CLI phase: a generated entry point, invoking a linker, JSON
     diagnostics, `-L`, and the `rlib`/`rmeta` equivalent.
   - The §8.4 capture half is still a small, self-contained job.
   - **`Drop` is waiting on a decision, not on work.**

## Edge Cases & Error Handling

- **A `char`** is `u32` by this level, so `c == 'a'` is `eq.u32 c_0, 97`.
- **A match on integer literals is a comparison chain** (§4), not a jump table,
  so what LLVM sees is `icmp eq i32` and then `switch i8` on the `bool`.
- **A zero-length string** is a `[0]u8` global; a backend may emit nothing for it
  as long as the address is valid.
- **A `defer` in an `if` block** belongs to that block's scope. Spec §8.4 says
  "the current function scope"; nothing tests the difference.
- **`Intrinsic::Unknown`** is the safety net for anything lowered away: a `$slice`
  emitted again becomes one, and `no_program_contains_an_unknown_intrinsic`
  fails rather than a backend receiving a name.
- **A backend refuses an output kind it does not produce** rather than writing a
  file of the wrong thing — a file that exists and holds the wrong content is
  worse than no file, because a build system will link it.
- Everything in the previous handoff's list still holds: a slice a program reads
  from is never dropped, `check::dropped` does not follow aliases, `transmute`
  reads its result type off the destination, and `core.panic` and `bytes_eq` are
  in every program because there is no dead-code elimination.

## Warnings

- **`LLVM_SYS_211_PREFIX` must be set** for anything `--features llvm`. Without
  it the build fails in `llvm-sys`'s build script, not in this code.
- **Do not reintroduce a filter in the renderer.** `pretty::unit_to_string`
  prints a unit exactly as it is.
- **`Cx::strip` is load-bearing in three ways at once** — `distinct`, mutability,
  and the key the type table is built on.
- **`-C codegen-units` is not tuning.** At `N > 1` optimization across the cut is
  gone; the default is 1 for that reason.
- **Around 80 clippy warnings are expected**, all dead-code-shaped — fields and
  methods codegen will read and nothing does yet.

## User Notes (standing)

- **Casts should be explicit about which conversion they are.** *Done* —
  `CastKind`. Keep it that way.
- **If something can be simplified in LIR while building codegen, do it**, and
  pass this instruction on in future handoffs. `$slice` and `$array` were the
  first two; `repeat` and `format` are next.
- **The GC is not the priority.** Use LLVM's built-in GC support plus an existing
  collector, so it can be swapped cheaply later. *Done* as the C shim + Boehm;
  the shim is the swap point.
- **Codegen is a trait**, and it supplies the target info. *Done*.
- **LLVM codegen uses inkwell and produces object files.** *Done*. The CLI and
  the library format come next and were deliberately not built.
- **Everything should be easily usable by the future CLI tool.**
- **Ask questions in batches.**

### Questions still open (asked, not yet answered)

1. **LLVM pin**: `llvm@21` + inkwell `llvm21-1` was assumed and works. Keep, or
   pin to an older, more-trodden LLVM?
2. **Feature gate**: `--features llvm`, off by default, was assumed. Confirm.
3. **`--emit` default**: still the four dumps. Flip to `obj` now that a real
   backend exists?
4. **`-C pointer-width`/`os`/`arch`**: kept as overrides beside `--target`. Drop
   them in favour of `--target` alone?
