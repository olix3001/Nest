# Handoff: steps 2–5 are done, and reflection runs

**Generated**: 2026-09-13
**Branch**: `main` (ahead 36)
**Status**: **581 tests pass** without LLVM, **597** with it. `cargo build`
reports 48 warnings, all dead-code-shaped. **A program reflects over a struct at
run time, reads each member through a checked read, and reads back an
`@attribute` a program declared itself.**

**Read `design/toolchain.md`** — its ten-step order of work is the plan. **Steps
1–5 are done and committed.** What is left is **`std`, `twig` and the editor** —
steps 6 through 10, in that order, and they are the whole of the focus now. The
rest of `#comptime` is **deferred and unscheduled**; nothing below needs it.

## What happened this session

Five commits, one per step plus the bugs found inside them.

| Commit | Step |
|---|---|
| `b65c4f3` | 2 — a method call on a literal receiver |
| `ad68f90` | 3 — `core/c` |
| `5e1417d` | 5a — reflection at run time |
| `bda7434` | 5b — user-declared `@attribute`s |
| `665776a` | 4 — `#comptime for` |

**Steps 4 and 5 were done in that order on your instruction** — reflection first,
`#comptime` after, because the reflective *read* is the part `std/json` needs and
unrolling is only needed by the typed path.

### Step 2 — a method call on a literal receiver

The previous handoff had this right: it was never "blanket impls". `(5).tag()`
left the receiver an open `comptime_int`, **a type no impl is written for**, so
every lookup in the chain missed and the call lowered to `call (undef)()` into a
`void` slot with no diagnostic at all.

- **`Cx::pin_numeric`**, beside `pin_str` and for the same reason: a method call
  asks a question only a concrete type can answer, so the literal settles on its
  default (`isize` / `f64`) *before* the lookup. A literal receiver now reaches
  exactly the impls a `let x: isize` would — through a concrete impl, a family
  impl (`int.<N>`) and a blanket impl alike.
- **A lookup that finds nothing is an error**, carrying the note that the type in
  the message is the one inference chose and not one the source wrote. That note
  is the difference between a puzzle and a fix: `impl Named for i32` with
  `(5).tag()` says "no method `tag` on `isize`", which is correct and would
  otherwise be baffling.

### Step 3 — `core/c`

`packages/core/c.nest`, re-exported from `core.nest`.

- **The scalar types are aliases**, not `distinct` types, on your decision: a
  language value goes into a C call with no conversion on either end.
- **Three of them the target decides**, and they are *generated* into
  `target.nest`: `C_LONG` / `C_ULONG` (LP64 against Windows' LLP64) and `C_CHAR`
  (signed on x86 and on Apple/Microsoft ARM64, unsigned on the ARM and RISC-V
  psABIs). They are generated rather than written in `c.nest` because **`c.nest`
  declares `int`**, which shadows the family inside that file — so it cannot
  spell `int.<C_LONG_BITS>` at all.
- **`c.ptr.<T>` is its own type**: an address and nothing else, so the collector
  never traces one and it crosses the ABI exactly as a pointer does. `to_ptr` /
  `to_mut` **check the null** a `*T` promises its holder cannot see.
- **The C string type is `cstr`, not `str`** — a file that declared `str` would
  shadow the language's own inside itself, and `from_cstr` returns one.
- **`c"..."`** lexes like any string (same escapes, same UTF-8), the **parser**
  puts the NUL on, and desugaring turns it into one `#lang("cstr_of")` call
  *before inference*. So a C string literal costs bytes in read-only data and
  nothing at run time, and the compiler knows nothing about what it becomes.
- **No varargs.** Decided, and unchanged.

### Step 5 — the whole reflection system

`packages/core/reflect.nest`. Everything in it is either a type the compiler
fills in **positionally** (the way it already fills a `Location`) or a bodyless
`#intrinsic`.

- **`type_info.<T>()`** is a read-only global and the call is a copy of it: `T`
  is concrete once monomorphization has run, so there is nothing left to compute.
  `TypeInfo` carries the name, size, align, `Kind`, `TypeId`, `members` and the
  attributes written on the type itself.
- **`type_id.<T>()`** is a 128-bit FNV-1a of `ir::mono::type_key` — the same
  whole-program string every symbol is mangled from, which is what makes it
  stable across units. **The key keeps `distinct` and mutability where `Cx::strip`
  erases both**, so `type_id.<usize>()` and `type_id.<u64>()` differ, and so do
  `*i32` and `*mut i32`. That is exactly the answer a checked read wants.
- **`member_ptr(v, m)`** is `Rvalue::Offset` with a stride of one — the same
  instruction a slice index is. The difference from `p.y` is only that the
  selector is a value.
- **`member_read` / `member_write`** are ordinary Nest: a comparison against a
  constant `TypeId`, a `panic`, and a `transmute`. Reading an `i32` member as an
  `i64` traps.
- **`Any`** is a trait with a blanket impl and `downcast` is the same comparison
  over the `{ data, vtable }` pair that already existed. Nothing new was built
  for it.
- **`@attribute`** marks a struct a program may write on a declaration.
  `@Json(rename: "user_id")` on a member is resolved, its literal arguments are
  recorded on the def, and `type_info` emits each value as a global with the
  `TypeId` that says how to read it beside the address. **No expansion pass, no
  generated code, no second program representation** — `attr_of.<Json>(m.attrs)`
  reads it the way `member_read` reads anything else.
  - It is a plain `Attr`, **not** a `*dyn Any`: a vtable would buy dynamic
    dispatch on a value nothing dispatches on. It would also have to be built
    after monomorphization had finished deciding what to instantiate.

### Step 4 — `#comptime for`

**Unrolled in the parser**, and that is the whole design. An unrolled body has to
be typed once per iteration, so each copy must be an *independent piece of
program*; every stage after parsing has already resolved names and stamped defs
onto the body's declarations, so copying a body there is copying its bindings.
The parser has the tokens and an index into them, so a copy is a rewind.

- The loop variable is bound `#comptime`, which the resolver introduces as a
  **compile-time constant** the way it already introduces a `#static` region. So
  the body may be *typed* with it: `[i]u8` is a different type each iteration,
  which a run-time local could never be.
- **The sequence is a range of integer literals.** A bound this stage cannot read
  is an error **at the loop**, which is the point of the step.
- `#comptime` on anything but a `for` says what it applies to.

## Five bugs found on the way, all fixed

1. **A `::` binding to a type was not an alias.** A `::`-RHS is parsed as an
   *expression*, so `K :: P.<u8>` came back as a `GenericApply` and `C :: u8` as
   a bare `Path`; collection filed both under `Const`, and a use of either in
   type position was a **silent `Ty::Error`** — which unifies with everything, so
   the program type-checked against nothing. `Inferer::const_alias_ty` expands a
   `Const` whose RHS names a type, following a chain of them.
2. **A method call on a trait object went to a blanket impl.** `impl <T> Trait
   for T` matches `T = dyn Trait` as happily as anything else, and the impl
   search ran *before* the trait-object one — so every call on a `*dyn Trait` was
   a static call instantiated at the **erased** type and the vtable built beside
   it went unused. The order is now dyn first.
3. **A field read off a base whose type is not known yet answered `Ty::Error`.**
   `ms[i].name` reads a field of `Index.Output`. The lookup is now an
   `Obligation::Field` discharged when the base has a type, and the question
   "what does `==` mean here" is an `Obligation::Comparison` for the same reason:
   answering it early compared a `str` **as two machine words** instead of by its
   bytes, silently.
4. **A receiver decided which method is called before anything solved it.**
   `Inferer::settle` runs the solver when a receiver is still a variable — the
   same work, done when the answer is wanted rather than at the end of the body.
5. **An empty slice constant held an integer where an address belongs.** A
   backend builds a global's initializer with no builder to hand, so there is
   nowhere to convert one. An empty attribute table is a `[0]Attr` global, as a
   zero-length string is a `[0]u8` one.

## Next: `std`, then `twig`, then the editor

Steps 6 through 10, in that order, and **that is the whole of the focus now**.
Everything they sit on is built: `core/c` gives `std` its `open`/`read`/`write`,
`core/fmt` gives it `Display`, and `core/reflect` is what `std/json` walks a type
with.

| Step | What |
|---|---|
| 6 | **The `std` floor** — `io`, `fs`, `process`, `mem`, `str`, `collections`, over an internal `sys` namespace that keeps the backing swappable. *Done when* a program reads a file, writes to stdout, spawns a process and reads its arguments and environment |
| 7 | **`std/json`** — parse and serialize over any type, through step 5's **run-time** walk, with `@json(...)` for renaming. The step that proves reflection was worth building |
| 8 | **`.nlib` / `.nmeta`**, plus `-C opt-level` and `-C target-cpu` |
| 9 | **`twig`** — the package tool, written in Nest. The first real program in the language |
| 10 | **The editor** — syntax first, then the language server on the manifest |

**The rest of `#comptime` is deferred and unscheduled**, on your instruction —
see "Deferred" below. Nothing in steps 6–10 needs it: `std/json` walks a type at
run time, which is the path that works today.

## Still open, and yours to decide

| Decision | Why it is open |
|---|---|
| **Whether `std` is versioned with the compiler** | Rust ships one per compiler; a package tool could resolve it like any dependency |
| **What `.nlib` holds beside the code** | Your note in `toolchain.md` now says an archive of pre-generated IR *and* metadata, with the `.nmeta` alongside |
| **`Drop`** (`#lang("drop")`) | Waiting on a *decision*: with no moves, "this value was returned / stored / passed to a call, so do not drop it" has no settled answer |
| **Floats in `f"..."`** | `Display` has no float impl. Ryū is a few hundred lines and a table; it belongs with the float work |

## Deferred — the rest of `#comptime`

**Not scheduled, and nothing in steps 6–10 waits on it.** Step 4 unrolls a range
of integer literals, which is what the parser can read at the point it rewinds.
Two sequences a program will eventually want are still out of reach, and they are
not the same size:

- **`.{ a, b, c }`** needs each element's token range recorded and re-parsed.
  Small; it belongs wherever a program first wants it.
- **`type_info.<T>().members`** needs a sequence that is only constant *after*
  monomorphization — a compile-time evaluator over the IR, which is a feature of
  its own and should be scheduled as one. The **run-time** walk is what
  `std/json` uses and it works today, so this buys the typed path and nothing
  else yet.
- **A `#comptime` loop variable is a constant, not a literal.** `[i]u8` works
  because an array length reads a constant; a tuple index `t.i` does not, because
  the parser wants a literal there.

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
- [ ] **`core.panic` prints nothing useful.** A trap says `nest: trap`. `std`
      (step 6) is what fixes it.
- [ ] **A trait must be imported by name for its impls to apply.** `r :: import
      <core/reflect>` is not enough to write `*dyn r.Any`; `{ Any } :: import
      <core/reflect>` is. Pre-existing, and it bit twice this session.
- [ ] **Optimization across units**, per-function escape summaries, the
      object-start table, narrowing what counts as a root (§5/§6).

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs still stands. New this session:

- **`cargo fmt`.** The repo is not rustfmt-clean: one run reformatted 37 files.
  Format the lines you wrote, by hand.
- **Naming a `core` declaration after a primitive and then using that
  primitive in the same file.** `c.nest` declaring `int` means `int.<N>` in
  `c.nest` resolves to `c.int`. The generated `target.nest` is where the
  width-parameterized C types had to go.
- **Answering a question about a receiver before the receiver has a type.**
  Three separate bugs, all the same shape: the field lookup, the `==` decision,
  and the method lookup. `Ty::Error` unifies with everything, so "not known" and
  "not there" have to be different answers.
- **Assuming a blanket impl does not match `dyn Trait`.** It does.
- **Putting an integer in a slice constant's pointer half.** A global's
  initializer is built with no builder.
- **Copying a resolved AST to unroll a loop.** The copies share the defs stamped
  on the original. Re-parsing is what makes them independent.
- **A vtable built from the reflection lowering.** It runs after
  monomorphization, which has finished deciding what to instantiate.

## Key Decisions

Everything in the previous handoff still stands. New this session:

| Decision | Rationale |
|---|---|
| A literal receiver settles on its default before the lookup | An open `comptime_int` is a type no impl is written for |
| "No method on `isize`" carries a note about where `isize` came from | The type in the message is one inference chose |
| The C scalar types are aliases | The point is that a language value goes in |
| `C_LONG` / `C_CHAR` are generated into `target.nest` | The target decides them, and `c.nest` cannot spell them |
| `c.ptr.<T>` is one `usize` in a struct | Untraced by construction, and a pointer over the ABI |
| The C string type is `cstr` | A file declaring `str` shadows the language's own |
| `c"..."` gets its NUL in the parser | Everything downstream sees an ordinary `str` |
| The reflection structs are filled positionally, found by `#lang` | The same contract `Location` has |
| `TypeId` is a hash of `type_key` | Already globally unique, already what symbols are mangled from |
| `type_info` is a global, not an aggregate built at the call | It is a constant; a copy is the whole lowering |
| An attribute is an `Attr`, not a `*dyn Any` | A vtable would buy dispatch nothing uses, after mono has finished |
| An attribute's arguments are literals | A declaration is not a place an expression runs |
| `@attribute` is required | A typo that silently means nothing is the alternative |
| `#comptime for` unrolls in the parser | A copy of a body has to be an independent piece of program |
| Its variable is a compile-time constant | Otherwise the body cannot be *typed* per iteration |
| Its sequence is a range of integer literals | The error belongs at the loop, and that is what the parser can read |
| A not-yet-known receiver defers rather than answering | `Ty::Error` unifies with everything and says nothing |

## Current State

**Working**: everything. `cd nestc && cargo test` → **581**;
`LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 cargo test --features llvm` →
**597**. A program links and runs; it calls libc through `core/c`, walks its own
types at run time, downcasts an `Any`, reads an attribute it declared, and
unrolls a `#comptime for`.

**Broken**: nothing known.

**Uncommitted**: `design/toolchain.md` — your own two edits (the `.nlib` line and
the debug-info note).

## Files to Know

| File | Why it matters |
|---|---|
| `design/toolchain.md` | **The plan.** Step 2's description there is still the old, wrong one; the work done matches this handoff |
| `design/lir.md` | The specification. §7d overflow, §10 the backend's brief, §11 the unit split |
| `packages/core/c.nest` | **New.** The C boundary |
| `packages/core/reflect.nest` | **New.** `TypeInfo`, `Member`, `Attr`, `Any`, and the checked read |
| `nestc/src/lir/lower.rs` | `type_info_global`, `attrs_const`, `type_id_const`, `kind_name`, `fnv1a_128` |
| `nestc/src/sema/infer.rs` | `pin_numeric`, `settle`, `const_alias_ty`, the `Field` / `Comparison` obligations |
| `nestc/src/sema/resolve.rs` | `resolve_attribute`, `introduce_comptime` |
| `nestc/src/parser/expr.rs` | `parse_comptime_for`, `const_range` |
| `nestc/src/sema/session.rs` | The generated `target.nest`, `c_long_bits`, `c_char_signed` |

## Resume Instructions

1. `cd nestc && cargo test` — expect **581 passed**.
2. With the backend:
   ```
   export LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21
   cargo test --features llvm          # expect 597
   cargo build --features llvm
   ```
3. **See reflection run**:
   ```
   cat > /tmp/r.nest <<'EOF'
   r :: import <core/reflect>
   @attribute Json :: struct { rename: str, skip: bool }
   P :: struct { @Json(rename: "user_id", skip: false) id: i32, n: i64 }
   main :: func () -> i32 {
     let p: P := P { id: 7, n: 11 }
     let info: r.TypeInfo := r.type_info.<P>()
     let m: r.Member := info.members[0]
     let j: Json := r.attr_of.<Json>(m.attrs).!
     if j.rename == "user_id" { return r.member_read.<P, i32>(&p, m) }
     return 0
   }
   EOF
   ./target/debug/nestc -o /tmp/r /tmp/r.nest && /tmp/r; echo $?   # 7
   ```
4. **See a `#comptime for` unroll**: `./target/debug/nestc --emit lir` on a body
   with `#comptime for i in 0..<4 { … }` — four copies, four `i_N` slots.
5. **Then step 6**, the `std` floor — and on through `twig` and the editor. The
   rest of `#comptime` is deferred; do not pick it up on the way.

## Edge Cases & Error Handling

Everything in the previous handoff still stands. New this session:

- **A trait object's method is a vtable lookup, always** — the dyn lookup runs
  before the impl search.
- **An empty reflective table is a zero-length global**, not a null pointer.
- **An attribute whose arguments resolution could not order is dropped** rather
  than emitted half-written; resolution already reported it.
- **`Kind::Other`** is the tail for a type the reflection vocabulary does not
  name. It is not a panic on purpose.
- **`core` now instantiates `impl Eq for TypeId` in every program**, beside the
  `Display for usize` the previous handoff noted.

## Warnings

- **`LLVM_SYS_211_PREFIX` must be set** for anything `--features llvm`.
- **`build.rs` needs `cc` and `ar`** to produce the runtime. Without them the
  link tests return early rather than fail.
- **Do not run `cargo fmt`.** See Failed Approaches.
- **Do not reintroduce a filter in the renderer.** `pretty::unit_to_string`
  prints a unit exactly as it is.
- **`Cx::strip` is load-bearing in three ways at once** — `distinct`, mutability,
  and the key the type table is built on. **`type_key` deliberately is not**:
  reflection depends on it keeping what `strip` throws away.
- **`-C codegen-units` is not tuning.** At `N > 1` optimization across the cut is
  gone; the default is 1 for that reason.
- **48 build warnings are expected**, all dead-code-shaped.
- **A trap prints `nest: trap`, not its message.** Reduce with `--emit lir`.

## User Notes (standing)

- **Ask questions in batches.**
- **If something can be simplified in LIR while building codegen, do it**, and
  pass this instruction on in every future handoff. `$slice`, `$array`, `repeat`
  and now the three reflection intrinsics all became an instruction or a
  constant; **`embed_file` is the only one left on the list that is more than one
  instruction or one runtime call.**
- **Casts should be explicit about which conversion they are.** Done.
- **The GC is not the priority.** An existing collector behind a thin shim.
- **Codegen is a trait**, and it supplies the target info. Done.
- **LLVM codegen uses inkwell and produces object files.** Done.
- **Everything should be easily usable by the future CLI tool.**
- **`nestc` before `twig`.** Done.
- **`std` is not linked automatically**; that is `twig`'s job.
- **`twig` is written in Nest**, so `std` needs fs, io and JSON serialization.
- **`nestc` supports structured output as JSON.** Done.
- **There should be `insta` snapshot tests over the generated LLVM**, with no
  target-specific content. *Still not done.*
- **Conditional compilation** is wanted for building `std`. `#when` is not in the
  parser, the spec or the grammar.
- **One object file out of `nestc`, always.** Done.
- **A runtime loop over a type's members is fine** — done, and it is what step 5
  delivered. Only typed access to the value must unroll.
- **Debug info should be disableable by config.**
- **Be compact in commit comments; do not edit existing comments.**
- **If you are unsure or need further guidance, ask.**

## User Notes
- Steps 1–5 were asked for and **all five are done and committed**.
- **Reflection was prioritized over `#comptime`** on your instruction, and both
  landed.
- **The rest of `#comptime` is deferred.** The focus is `std`, `twig` and the
  LSP — steps 6 through 10 — and nothing in them needs it.
