# Handoff: the C boundary closed, the entry point moved to `std`, `twig` still not started

**Generated**: 2026-09-16
**Branch**: `main` (ahead 44)
**Status**: **602 tests pass** without LLVM, **619** with it. `cargo build`
reports 48 warnings (46 with `--features llvm`), all dead-code-shaped —
unchanged. **Nothing is uncommitted.**

**Read `design/toolchain.md`** — its ten-step order of work is the plan, and it
is **unchanged this session**: steps 1–6 are done, 7 through 10 are not. You
asked for **step 9, `twig`**, two handoffs ago, and it is **still not started**.
What this session produced is the five commits below, which are the four items
you left in the previous handoff's User Notes plus one you raised while the work
was running.

## What happened this session

Five commits, no `twig`. Every one of them came from a note you wrote rather
than from probing.

| Commit | What |
|---|---|
| `c42ad23` | **`#c_vararg`** — a C variadic function is declarable, so `nest_open` is gone |
| `5aebdec` | **`@no_mangle`** |
| `18993ff` | **The entry point is `std`'s**, by way of `#lang("start")` |
| `19a4480` | **`::` is comptime, `:=` is runtime** — a `#static` takes `:=` |
| `a02419b` | **`reflect.TypeId` is one `distinct u128`** |

### `#c_vararg` (`c42ad23`)

A declaration's written parameters are C's **fixed** ones, and a call may pass a
tail past them:

```nest
extern("c") {
  @public printf :: #c_vararg func (fmt: c.cstr) -> c.int
  @public open   :: #c_vararg func (path: c.cstr, flags: c.int) -> c.int
}
```

**Nest itself has no variadics** and is not getting any — your decision, and the
Zig answer is the one the language already has: a function wanting many
arguments takes a tuple, and `.{ … }` already parses as an inferred composite
literal. `#comptime for` and `core/reflect` are what walk it.

Four things about the tail are decided and none of them is arbitrary:

- **C's default argument promotions are applied**, as an explicit `$cast`:
  anything narrower than an `int` becomes one, an `f32` becomes an `f64`. Not a
  convenience — the callee reads the tail with `va_arg`, which can only be asked
  for a promoted type, so a `u8` passed as a `u8` is read out of a slot nothing
  filled.
- **An unconstrained integer literal settles on `c.int`, not `isize`.** The
  language's own default would put eight bytes where `%d` reads four. A literal
  too large for an `int` keeps the ordinary default, which is what C does with
  one too (`Infer::fits_c_int`).
- **No tail argument reaches a backend as a bare constant.** `Constant::Int` is
  a `BigInt` and no width, and a fixed argument takes its width from the
  parameter it fills. So `lir::lower::spill_variadic_tail` writes it to a slot
  and passes the slot.
- **`Ty::Func` is untouched.** A variadic signature has no function-pointer type
  — the tail lives in the calling convention and a `Ty::Func` says nothing about
  it — so taking the address of one is refused rather than being silently
  indistinguishable from the fixed-arity function beside it. That is also why
  the flag is on the **def** and on `FunctionAttrs`, and why 65 `Ty::Func` match
  sites did not have to change.

**Declaration only.** Reading a tail is `va_list`, whose layout differs per
target and which nothing here emits, so a `#c_vararg` with a body is refused.
That is the `va_list` item on the list below.

**What it was for**: `std/libc` declares `open` as the variadic function it is,
and `runtime/nest_runtime.c` lost `nest_open`. On arm64 that is the difference
between `0644` and whatever the stack held — a fixed argument arrives in a
register and a variadic one on the stack, which is why the shim existed.

Also fixed underneath it: **an `extern` block's members took attributes but not
directives.** The block is sugar for a run of bindings and the desugaring was
dropping half the decoration, so `#c_vararg` was unwritable in the item position.

### `@no_mangle` (`5aebdec`)

`@link_name` with the name left out, which is the usual case: a function a C
caller reaches is looked up by a name somebody outside this program has to
write, and repeating it in an attribute is a second place for it to be wrong. It
works on a `#static` too, because `global_symbol` is the same decision about a
different kind of definition. Written beside an `@link_name` it is **refused**
rather than resolved — both name the symbol, they name different ones.

### The entry point is `std`'s (`18993ff`)

```
extern("c") func entry(_0: i32, _1: u64) -> i32      // main
bb0:
  call nest_init()
  _2 := call start(&entry.status, _0, _1)
  return _2
```

`nest_init` prepares the collector, which is a fact about the machine.
Everything after it is a **decision** — what a program keeps from `argv`, what a
status means — so it goes to whoever claims `#lang("start")`. `std/sys.start` is
what claims it, and it holds the two `#static`s the arguments live in.

- **The function pointer is what makes this possible at all.** A `main` written
  in `std` would have to spell the program's mangled symbol; being *handed* it
  costs nothing and encodes nothing.
- **`#lang("start")` takes one shape**, `func () -> i32`. §5.6 allows three, so
  the other two get `entry.status`, a wrapper holding exactly the conversion the
  entry used to perform inline.
- **With no claimant the entry calls `main` directly**, as before. Not a
  fallback so much as the only thing left: a program without `std` has no way to
  ask what its arguments were.

`nest_argc` and `nest_argv` left the runtime. **`nest_envp` did not**, and it is
not an oversight: it reads `environ`, which macOS hides behind a feature macro
that keeps a dynamic executable from linking against it directly.

### `::` is comptime, `:=` is runtime (`19a4480`)

Your observation, and it was right: `#static` was the **only** place `::` bound
something that is not a value the compiler knows.

```nest
#static count: i32 := 0        // a region
#static scratch: [4]u8         // absent initializer = zeroed
K: i32 :: 0                    // a constant
```

Each wrong operator is reported with the one the declaration wanted, rather than
"unexpected token". `#static` stays **required** in both positions, because
inside a function body it is what separates program lifetime from frame lifetime
and one spelling working in both places is worth more than the word saved.

**`:=` marks the storage as mutable, not the initializer as run-time computed.**
A region's initializer is still baked into the program's data. That constraint is
the whole reason this is a spelling change rather than a feature: license
`#static n: i32 := compute()` and you have static-initialization order, an
ordering rule, and an answer for cycles.

### `reflect.TypeId` is one `distinct u128` (`a02419b`)

Also yours, also right. It was `struct { lo: u64, hi: u64 }`, which is what a
language without arbitrary integer widths has to do — and `ir::mono::fnv1a_128`
was computing a `u128` and then `type_id_const` was taking it apart. Now:

- the constant is one 128-bit integer,
- `eq` is one `eq.u128` instead of two comparisons and the branches an `&&`
  needs,
- `core.TypeId` leaves the type table altogether.

`distinct` rather than a bare `u128` because an identity is not a number.

Two small things fell out: a diagnostic about a parameter following a defaulted
one rendered with thirty stray spaces mid-sentence, and the `bool` codegen test
asserted on `alloca i1` — which is a **prefix of `alloca i128`**, and `core` now
has a 128-bit local in it.

## Next: two things, in this order

### 1. `self` should not need an explicit `: Self`

**The parser already accepts it and sema does not.** `parse_param` documents the
receiver as "just the parameter named `self`; its type is optional (defaulting to
`Self`)" — and nothing implements the defaulting:

```nest
impl P {
  bare :: func (self) -> i32 { return self.x }
}
```

```
error: type annotations needed
  |   bare :: func (self) -> i32 { return self.x }
  |                 ^^^^
```

**It is syntax sugar for the first parameter named `self`, and the explicit
form stays legal** — `func (self: Self)`, `func (self: *Self)` and
`func (self: *mut Self)` all keep working and keep meaning what they mean. A
bare `self` is `self: Self`, by value, and nothing else changes.

**Where it lives — two sites, and both fall back to `self.cx.fresh()`:**

| Site | What it types |
|---|---|
| `sema/infer.rs:963` | the parameter's entry in the body's environment |
| `sema/infer.rs:4733` | `func_def_ty` — the *signature*, which is what a call site sees |

They have to agree, which is the trap: fixing only the first types the body and
leaves every call unable to bind the receiver. `Self` in an `impl` is already
resolvable — the explicit `func (self: Self)` form goes through
`ty_from_node` and works today — so what is missing is the substitution, not the
type.

**A bare `self` outside an `impl` or a `trait` has no `Self` to mean**, and that
is a diagnostic rather than a fresh variable: a closure parameter named `self`
is the only other thing this shape can be, and it is not a receiver.

### 2. Reflection: make sure it works, and that it costs nothing unasked

**`type_info` is already demand-driven, and the mechanism is
`Cx::data_global`'s keyed dedupe** (`lir/lower.rs:956`), not the generic
machinery. A `#intrinsic` is not a function — `type_info.<T>()` lowers at
`lir/lower.rs:3020` to a copy of a global that `type_info_global` builds *when
that call site is lowered* — so a type nothing asks about produces nothing.
Verified: a program declaring `Point` and `Unused` and calling
`reflect.type_info.<Point>()` mentions `Unused` **zero** times in its LIR.

What the next agent should actually do is **confirm the whole surface works**,
because most of it has never been exercised by a program:

- `type_info` on a **generic** type, an **enum**, a **tuple**, a slice, an array
  and a primitive — `kind_const` has an arm for each and only the struct arm is
  known-good.
- `member_ptr` / `member_read` / `member_write` round-tripping a value.
- `Any` and `downcast` through a `*dyn Any`, which is the only part with a
  vtable in it.
- `attr_of` and `member_of`, which no test reaches.
- And the thing to watch for: **a `TypeInfo` emitted for a type nothing asked
  about**. The dedupe is by `ir::mono::type_key`, so the failure mode is not a
  duplicate but a *spurious* one — some path calling `type_info_global` for a
  type it is merely describing. Check the attribute path in particular
  (`attrs_const` walks member defs).

### And then `twig`

Still step 9, still not started, and **the design in the previous handoff still
stands** — flattened TOML, one `nestc` invocation per build until `.nlib`
exists, inherited stdio until `std` has pipes. It is now written against the
final spelling of `#static`, which is why that change went first.

## Not Yet Done (compiler)

- [ ] **A bare `self`** — above. The first thing to do.
- [ ] **Reflection's untested surface** — above.
- [ ] **`va_list`, and a `#c_vararg` with a body.** Accepting a tail costs a
      flag on a signature; *reading* one is a per-target struct (x86-64 SysV's
      is four fields with a register save area, AArch64's is different) and
      LLVM's `va_arg` instruction is not usable — clang lowers it in the front
      end per target. `core/c` would grow `va_list` plus `va_start` / `va_arg` /
      `va_end` intrinsics. **Nothing needs it today.**
- [ ] **`nest_errno`.** The last runtime function that is a *language* gap
      rather than a machine fact. `errno` is a macro expanding to `__error()` on
      macOS and `__errno_location()` on Linux — the **symbol's name** differs, so
      it needs `#when` or an `@link_name` that can branch on the target.
- [ ] **A `void` member should be erased from a `TypeDef` too.** §9 erases
      `void` from slots, parameters and arguments but not from a type's members.
      **Not done because `Projection::Field` indices are positional.**
- [ ] **`-C opt-level` and `-C target-cpu`.** The backend hardcodes
      `OptimizationLevel::None`, `RelocMode::PIC`, `CodeModel::Default`.
- [ ] **Parallel code generation.** What is missing is a backend instance and an
      LLVM context per thread.
- [ ] **A library format** (`.nlib` / `.nmeta`). A dependency is source today.
- [ ] **Debug info.** Nothing emits DWARF. **Disableable by config** (your note).
- [ ] **`core.panic` prints nothing useful.** A trap says `nest: trap`. **`std`
      exists**, and now so does `#lang("start")` — which is where a default
      handler could be installed, since `core` may not import `std` but `std`
      runs before `main` does. That is a new option this session opened.
- [ ] **`str.to_string(7)` where `str` is the *type*** reports "type annotations
      needed", twice. A bad message for a program that is genuinely wrong.
- [ ] **Two declarations of one `@link_name`.** Still not reproduced: both
      declarations were eliminated as dead code before the mangler saw them.
      **`@no_mangle` makes this easier to hit now**, and the answer is still that
      the linker reports it, which is where C reports it.
- [ ] **`defer` does not capture its receiver.** It needs the capture to move
      into sema lowering, which needs a mutable `DefTable` there.
- [ ] **`insta` snapshot tests over the generated LLVM**, with no
      target-specific content. *Still not done.*
- [ ] **Optimization across units**, per-function escape summaries, the
      object-start table, narrowing what counts as a root (§5/§6).

## Failed Approaches (Don't Repeat These)

Everything in the previous handoffs still stands. New this session:

- **`cargo fmt`.** Unchanged and still true: one run rewrites 34 unrelated files
  (`+732 / −401`). The hand-wrapped `assert_eq!`s and the long inline Nest source
  strings in `src/sema/tests.rs` are deliberate. **Format by hand.**
- **A regex sweep over `#static` spellings in Rust string literals.** A Rust
  string's newline is the two characters `\` and `n`, not a real newline, so a
  `[^\n"]*?` guard does **not** stop the match crossing a line of Nest source —
  it turned `f :: func () {}` into `f := func () {}` in one test. Read the diff
  of any such sweep line by line.
- **Writing a multi-line Rust string through a Python heredoc.** A trailing `\`
  inside a Python `"""…"""` is Python's line continuation and never reaches the
  file, so the Rust string keeps the indentation as literal spaces. One
  diagnostic shipped with thirty of them in it (fixed in `a02419b`, and it had
  been there since `2dda8a8`). Write `\\` or use the `Edit` tool.
- **Asserting on an LLVM IR substring without its punctuation.** `alloca i1` is
  a prefix of `alloca i128`. The assertion survived only because nothing in
  `core` had a 128-bit local until this session.
- **Putting `#lang("start")` in `core`.** Considered and rejected: it would make
  every program route through it and would put "what a program keeps from
  `argv`" in the package that may not know what a process is. `std` claims it,
  and a program without `std` keeps the old entry — which costs one `Option` in
  `lir::entry::synthesize`.

## Key Decisions

Everything in the previous handoffs still stands. New this session:

| Decision | Rationale |
|---|---|
| Nest gets **no variadics of its own** | A function wanting many arguments takes a tuple, which is already sayable — `.{ … }` parses, `#comptime for` and `core/reflect` walk it |
| A **C** variadic is declarable, with `#c_vararg` | The alternative was a C shim per function, and the first one was already written |
| The flag is on the **def** and `FunctionAttrs`, not on `Ty::Func` | A variadic signature has no function-pointer type, so the type does not need to carry one — and 65 match sites did not have to change |
| …so **address-of is refused** | A pointer that carried no convention would be indistinguishable from one to the fixed-arity function of the same parameters |
| The tail's promotions are **recorded**, not required of the program | C's rule is the *callee's*, not something the call site chose |
| A tail literal settles on **`c.int`** | `1` is an `int` to everyone who reads it and to the `va_arg` that picks it up |
| A **`str` in a tail is refused**, with the two spellings that work | It is a pointer and a length, and C reads one argument |
| `@no_mangle` is an **attribute** | `@link_name` already is one, and this is that with the name left out |
| …and beside an `@link_name` it is **refused** | Preferring either silently makes one of the two a thing the program wrote and the compiler ignored |
| The entry point's **decisions** go to `#lang("start")` | `nest_init` prepares the collector; everything else was a policy in the wrong file |
| …and the program's `main` is passed as a **function pointer** | Nothing outside the compiler can spell a mangled symbol |
| …and it takes one shape, `func () -> i32` | The other two §5.6 allows get a wrapper, which is where the conversion the entry used to do inline now lives |
| `#static` takes **`:=`** | It is storage, and `::` binds a value the compiler knows |
| …but its initializer stays **const-evaluable** | `:=` marks the storage mutable, not the initializer run-time computed — otherwise this is static-initialization order and not a spelling |
| `TypeId` is one **`distinct u128`** | The language has arbitrary integer widths; the hash was computed as a `u128` and then taken apart for no reason |

## Current State

**Working**: everything. `cd nestc && cargo test` → **602**;
`LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21 cargo test --features llvm` →
**619**.

**Broken**: nothing known.

**Uncommitted**: nothing.

**The runtime is down to eight functions**, and every survivor is something Nest
genuinely cannot say: `nest_alloc`, `nest_free`, `nest_gc_collect`, `nest_init`,
`nest_envp`, `nest_errno`, `nest_trap`, `nest_assert`.

## Files to Know

| File | Why it matters |
|---|---|
| `design/toolchain.md` | **The plan.** Unchanged this session; step 9 is `twig` |
| `design/lir.md` | The specification. §7e the entry point, §9 the directive table, §10 the backend's brief |
| `design/roadmap.md` | Where the `::` / `:=` rule is written down |
| `nestc/src/sema/infer.rs` | `check_vararg_arg`, `c_promotion`, `fits_c_int`, `apply_call_with` — and **`:963` / `:4733`**, the two places a bare `self` has to be given `Self` |
| `nestc/src/sema/collect.rs` | `check_c_vararg`, `no_mangle` |
| `nestc/src/lir/entry.rs` | The entry point, and `status_fn` — the wrapper that makes `main`'s three shapes one |
| `nestc/src/lir/lower.rs` | `spill_variadic_tail`, `type_info_global`, `type_id_const`, `data_global` (the dedupe) |
| `nestc/src/ir/mono.rs` | `mangle` and `global_symbol` — where `@link_name` and `@no_mangle` win |
| `nestc/src/parser/item.rs` | `parse_const_bind` — the `::` / `:=` split, and `parse_extern_block` |
| `packages/std/sys.nest` | `start`, and the two `#static`s the arguments live in |
| `packages/core/reflect.nest` | `TypeId`, `TypeInfo`, `Member`, `Any` — the surface to prove out |

## Resume Instructions

1. `cd nestc && cargo test` — expect **602 passed**.
2. With the backend:
   ```
   export LLVM_SYS_211_PREFIX=/opt/homebrew/opt/llvm@21
   cargo test --features llvm          # expect 619
   cargo build --features llvm
   ```
3. **See this session's four features at once.** Varargs with promotions, a
   `#static` with `:=`, `@no_mangle`, the arguments through `#lang("start")`,
   and a `TypeId` comparison:
   ```
   cat > /tmp/session.nest <<'EOF'
   c       :: import <core/c>
   io      :: import <std/io>
   process :: import <std/process>
   reflect :: import <core/reflect>

   extern("c") {
     @public printf :: #c_vararg func (fmt: c.cstr) -> c.int
   }

   #static calls: i32 := 0

   @no_mangle
   @public nest_probe :: func (n: i32) -> i32 { calls = calls + 1  return n + calls }

   P :: struct { x: i32, y: f64 }

   main :: func () -> i32 {
     let b: u8 := 7
     let f: f32 := 1.5
     printf(c"b=%d f=%.1f n=%d\n", b, f, nest_probe(1))
     let t: reflect.TypeInfo := reflect.type_info.<P>()
     printf(c"P: size=%d members=%d\n", cast.<i32>(t.size), cast.<i32>(t.members.len()))
     let same: bool := reflect.type_id.<P>().eq(reflect.type_id.<P>())
     io.println(same.match { true => "ids agree", false => "ids differ" })
     io.println(process.program())
     return cast.<i32>(process.args().len())
   }
   EOF
   ./target/debug/nestc -L ../packages -o /tmp/session /tmp/session.nest && /tmp/session one; echo $?
   ```
   Expect `b=7 f=1.5 n=2`, `P: size=16 members=2`, `ids agree`, the program's
   own path, then `2`. And `nm /tmp/session | grep nest_probe` finds
   `_nest_probe`, unmangled.

   **The lines do not come out in source order**, and that is not a bug: C's
   `stdio` buffers `printf` until exit, and `std/io` writes straight to the
   descriptor. Two output paths in one program is exactly what a `#c_vararg`
   `printf` beside `io.println` buys, and it is worth seeing once.
4. **Then a bare `self`**, which is §1 above and is the smallest of the three.
   *Done when*: `func (self) -> i32` inside an `impl` types as `func (self: Self)`
   does, the explicit forms still work, and a bare `self` outside an `impl` says
   so.
5. **Then reflection**, which is §2 above. *Done when*: each `Kind` arm has a
   program behind it, `downcast` round-trips, and a type nothing asks about
   still produces no `TypeInfo`.
6. **Then `twig`.** The first file to write is the TOML reader, because it is
   the only part with nothing to copy from. *Done when*: `twig build` builds a
   package with a dependency, and `twig run` runs it.

## Edge Cases & Error Handling

Everything in the previous handoffs still stands. New this session:

- **A `#c_vararg` call may not name an argument.** The tail has no parameter
  names to bind against, and the head moving while the tail kept its position
  would be silent.
- **A `#c_vararg` declaration needs a fixed parameter**, because `va_start`
  names the last one — so a variadic function with none is a thing C cannot
  express either.
- **A struct is *not* refused in a tail.** `c.ptr.<T>` is one, and an
  `extern("c")` signature is already a promise that its types are C's. What is
  refused is what this language owns the representation of: a `str`, a slice, an
  array, a tuple, a `dyn`.
- **A `#static` with no initializer is still zeroed**, and takes no operator at
  all — `#static scratch: [4]u8`.
- **`entry.status` is only synthesized when `main`'s shape differs.** A `main`
  that already returns `i32` *is* the function `#lang("start")` takes a pointer
  to, and is passed as it stands.
- **`nest_init` reuses a declaration the program already has**, as it always
  did — but it now takes **no arguments**, so a program that declared the old
  two-parameter form has declared a different function under one symbol. That is
  the ordinary C hazard and not one this can see.

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
- **`std` is not linked automatically**; that is `twig`'s job. Done — and the
  entry point is careful about it: `#lang("start")` is `std`'s, and a program
  without `std` still gets an entry.
- **`twig` is written in Nest**, so `std` needs fs, io and JSON serialization.
  fs and io exist; JSON is step 7 and is still not written.
- **`nestc` supports structured output as JSON.** Done.
- **There should be `insta` snapshot tests over the generated LLVM**, with no
  target-specific content. *Still not done.*
- **Conditional compilation** is wanted for building `std`. `#when` is not in
  the parser, the spec or the grammar — **deferred until blocking**, your
  decision. It has now been named twice as the thing `nest_errno` wants.
- **One object file out of `nestc`, always.** Done.
- **A runtime loop over a type's members is fine.** Only typed access must unroll.
- **Debug info should be disableable by config.**
- **Be compact in commit comments; do not edit existing comments.**
- **If you are unsure or need further guidance, ask.**
- **`std/libc` should not require custom runtime functions.** Done for
  `nest_open`. `nest_errno` and `nest_envp` remain and are argued for above.
- **All reflection types should be part of `core/reflect`.** They already are —
  `TypeId`, `Kind`, `Attr`, `Member`, `TypeInfo` and `Any` are all declared
  there. What is left is proving the surface works, which is §2 above.
