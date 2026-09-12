# LIR — the low-level IR

**Status**: §1, §2, §3, §4, §7, §7b, §7cc, §7c and §7d are **built** —
`nestc/src/lir/` is the representation (`mod.rs`), the lowering (`lower.rs`) and
the dump (`pretty.rs`). §5 (drops) and §6 (GC safepoints) are not; they are the
next phase, and both ride the ladder §3 already builds.

LIR is the last stage before code generation. It is deliberately *not* LLVM IR:
it keeps Nest's type system, its GC model and its `defer` semantics, all of which
LLVM has no notion of. What it gives up is structure — no `if`, no `while`, no
`match`, no nesting. A function is a list of basic blocks connected by explicit
jumps, in the spirit of Rust's MIR.

Three things exist in LIR that exist nowhere earlier in the pipeline:

1. **`defer` bodies placed on every path that leaves a scope**, without being
   duplicated.
2. **Explicit `drop`s** for allocations proven not to outlive their scope.
3. **Explicit GC safepoints**, carrying the set of live pointers.

## 1. Shape

A function is locals plus blocks. Locals are declared up front; blocks are
labelled and end in exactly one terminator.

```
func f(a: i32, b: i32) -> bool {          // examples/f.nest:3:1
  let p: Point                            // 4:7
  let t0: i32
  let t1: bool
bb0:
  t0 := a + b                             // 5:11
  p.x = t0                                // 6:3
  t1 := call g(&p)                        // 7:14
    @safepoint { live: [p]
      p := reloc p
    }
  switch t1 { 1 => bb1, _ => bb2 }
bb1:
  return t1
bb2:
  return false
}
```

There are **four** instructions — an assignment, a call, an intrinsic and a
`drop` (§5) — and four terminators. That is the whole set, and keeping it that
small is the point: a backend for C, for LLVM, or for wasm has to answer for each
of them, so every operation this level invents is a question asked of every
backend that will ever exist. §10 is that list from the backend's side.

`:=` **introduces** a value into a local; `=` **stores** into a place. That is
the same distinction the source language draws, so it costs a reader nothing to
carry across. Every instruction carries the source span it came from, printed as
a trailing comment — codegen needs them for debug metadata, and a span that is
only reconstructed at the end is a span that is wrong.

### Places

A **place** is an lvalue path: a local, plus a chain of projections.

```
p               the local itself
p.x             field `x`
p.*             the pointee (dereference)
p[i]            element `i`
p.*.next.x      chained
(p as Some).0   downcast to a variant, then its first field
```

A place is never a value. `t0 := p.x` *loads* from a place; `p.x = t0` *stores*
into one. Only the store form appears on the left of `=`.

A **base** is a local or a `#static`, and nothing else. A `::` constant is not
one: §2.5 says such a constant *is* its value, so it has no region and nothing
can point at it. Where one is used as a place — `TABLE[1]`, since indexing goes
through `core`'s `Index` impl and that takes `&TABLE` — the value is written
into a slot first and the slot is the place. That is the same materialization
`(a + b).x` gets, and it is why a backend never meets a base it cannot address.

Field projections print **by name**, not by index. The index is what codegen
wants, but a reader debugging a mis-lowered access needs to know which field it
was — and after `#packed` / `#align` / `#soa` have had their say, the index alone
does not tell them.

### Terminators

```
goto bb1
switch t { 0 => bb1, 1 => bb2, _ => bb3 }
return t
unreachable
```

`switch` covers every branch: a two-way `if` is a switch on a `bool`, and a
`match` is a switch on a discriminant (see §4). Calls are *instructions*, not
terminators — a consequence of the panic model below.

## 2. Panics and the `never` type

**A panic does not unwind.** It runs the panic handler and terminates the
process. `defer` bodies do **not** run on the panic path.

This is the single largest simplification in LIR. If panics unwound, every
fallible call would need two successors — a normal one and a cleanup one — and
every pass in this document would have to reason about cleanup blocks. Instead a
call is an ordinary instruction with one successor, and the CFG stays roughly the
size of the source.

What makes that liveable is the **`never` type**: the type of an expression that
does not produce a value because control never reaches past it.

```
panic :: func (msg: string) -> never
abort :: func () -> never

classify :: func (n: i32) -> string {
  return n.match {
    0        => "zero",
    1..=9    => "small",
    _        => panic("out of range"),   // never coerces to string
  }
}
```

`never` coerces implicitly to **every** type, which is sound precisely because
the coercion can never actually run. That is what lets `panic(...)` sit in any
expression position without special-casing it in the type checker.

A function declared `-> never` is checked to genuinely never return: see the
divergence check in the IR-pass plan. In LIR, a call to one is followed by
`unreachable`, and the block ends there.

### A panic is a call, and it is the program's panic

There is no `$panic` operation. `panic` is an ordinary function in `core` (spec
§8.6) found by its `#lang("panic")` tag, and the failures the **compiler** raises
— a trapped overflow (§7d), an index past the end of a sequence (§3.2) — lower to
a call to it:

```
bb6:                    // overflow
  _5 := Location("m.nest", 22, 9)
  call core.panic("integer overflow", _5)
  unreachable
```

Two things follow, and both are the reason. A backend has nothing new to
implement: it already emits calls, and the last instruction of a panic is
`core`'s `trap` intrinsic, one instruction on every target. And a program that
replaces `#lang("panic_handler")` replaces what an overflow does too — a
compiler-private abort beside a library panic would be two ways for a program to
die, reported differently.

The `Location` is built at the lowering, because the source did not write this
call site. It is the same three numbers `#caller_location` fills in for a call
the program *did* write, taken from the span the failing operation already
carries (§7c).

## 3. `defer`, placed once per exit path

`defer` bodies run when the enclosing scope exits — by falling off the end, by
`return`, by `break`, or by `continue`. Ordering is reverse of registration, and
outer scopes run after inner ones.

A deferred body is **not a construct at this level**: it is an ordinary block,
reached by an ordinary jump, and the ladder is ordinary edges. Nothing in the
representation records that it came from a `defer`, because nothing after this
point needs to know.

The requirement is that a `defer` body appears **once** in LIR no matter how many
paths exit through it. Duplicating it inline at each `return` is the obvious
lowering and the wrong one: it multiplies code size, and it means a reader
staring at three copies has to prove to themselves that they are the same copy.

The lowering is a **cleanup ladder** — one block per deferred body, chained
outward, with each exit path jumping into the ladder at the depth it is leaving
from.

```
f :: func () -> i32 {
  defer a()
  {
    defer b()
    if cond { return 1 }
  }
  return 2
}
```

```
func f() -> i32 {
  let ret: i32
bb0:
  t0: bool := cond
  switch t0 { true => bb1, false => bb2 }

bb1:                       // `return 1` — inside both scopes
  ret = 1
  goto cleanup_b

bb2:                       // inner scope ends normally
  goto cleanup_b_fall

cleanup_b:                 // leaving the inner scope, on the return path
  call b()
  goto cleanup_a

cleanup_b_fall:            // leaving the inner scope, falling through
  call b()
  goto bb3

bb3:
  ret = 2
  goto cleanup_a

cleanup_a:                 // leaving the outer scope
  call a()
  return ret
}
```

The rungs are shared per **kind** of exit and not per exit *site*: three
`return`s inside one scope enter the same rung. They cannot be shared across
kinds, because what follows the rung differs — a `return` continues outward to
the function's exit and a `break` only to the loop's — which is why `cleanup_b`
and `cleanup_b_fall` are two blocks above rather than one.

The `defer b()` body is emitted once even though two paths run it. `return`
writes its value into a dedicated `ret` local rather than returning directly,
because the actual `return` has to happen *after* the ladder — this is what makes
"defer runs before the function returns" true in the IR rather than a convention.

A `break` out of a loop enters the ladder at the depth of the loop's scope, not
the function's; `continue` enters at the loop body's depth. The general rule is
that a jump crossing *n* scope boundaries enters the ladder *n* rungs down.

## 4. Match, lowered to a decision tree

IR keeps `match` in source shape — a list of patterns and arms — because that is
what the exhaustiveness pass reads and what diagnostics point at. LIR is where it
becomes control flow.

```
e.match {
  Some(x) if x > 0 => A,
  Some(_)          => B,
  None             => C,
}
```

```
bb0:
  t0: u8 := discriminant(e)
  switch t0 { 1 => bb1, 0 => bb4, _ => unreachable }
bb1:                                 // Some
  x := (e as Some).0
  t1: bool := x > 0
  switch t1 { true => bb2, false => bb3 }
bb2: ... A ... goto join
bb3: ... B ... goto join
bb4: ... C ... goto join
join:
```

Tests are ordered so each is performed **once**: the discriminant is read a single
time even though two arms match `Some`. A guard is a test like any other, except
that a failed guard must fall through to the *next arm*, not to the next test —
which is why the tree is built rather than a naive chain of comparisons emitted.

The tree is built during lowering and nowhere else. Exhaustiveness is decided
earlier, on IR (§ the IR-pass plan), so by the time the tree is built every
scrutinee value is known to be covered and the final `_ => unreachable` is a real
guarantee rather than a hope.

## 5. Drops

An allocation whose object provably does not outlive its scope gets an explicit
`drop`, which frees it without involving the collector.

Escape analysis is **intra-procedural**, and a value escapes if it is:

- returned,
- stored into anything reachable from outside the scope (a parameter's pointee, a
  global, a field of an escaping value),
- passed to **any** call, or
- captured by a closure that itself escapes.

"Passed to any call" is deliberately blunt. Without per-function summaries there
is no way to know whether a callee retains what it is given, and guessing wrong
frees memory that is still referenced. The pass is therefore useful for
short-lived local temporaries and honest about being useful for nothing else.

```
let p := new.<Node>()      // never passed anywhere
p.*.x = 1
t := p.*.x
```

```
bb0:
  _1 := $new()
  p_3 := _1
  p_3.*.x = 1
  _4 := p_3.*.x
  goto bb5
bb5:                       // cleanup 0 (return)
  drop p_3                 // provably dead at scope exit
  goto bb4
```

A `drop` sits on the same cleanup ladder as `defer`, for the same reason: an
allocation in a scope with three exits is freed once, in a block all three reach.
It goes on the rung **after** the `defer` bodies, because a `defer` may still
read the object and the memory has to survive until it has.

Extending this to per-function escape summaries later is a change of *precision*,
not of shape — the drop insertion machinery does not move.

### The same instruction, written by hand

`drop(p)` in the source (spec §6.9) lowers to **this** instruction, not to a
second one. A program writing it has taken on the question the analysis would
otherwise have answered, and two things follow: it is no longer a candidate for
an automatic drop — passing a local to anything disqualifies it, and a call is a
call — and `check::dropped` refuses a later use of the name, because a pointer
whose object was freed is the one thing a collected language exists to make
impossible.

That is why the instruction takes an **operand** rather than a local:
`drop(node.*.next)` frees a pointer no local names. For a backend the two are one
case — a pointer value, and a free. A `make`d slice is `{ ptr, len }` by here, so
the lowering projects the member: the operand is an address in every case, and
not sometimes a struct.

One consequence of the whitelist is worth stating rather than leaving to be
found. **A slice a program reads from is never dropped**, because every use of
one goes through `&xs` — `.len()` and `xs[i]` both do — and taking a local's
address is not on the list. `make` allocations are collected, not freed early,
unless nothing touches them.

### Where the analysis runs

A scope is **lexical**, and once control flow is a graph there are no scopes left
to speak of — the ladder is what remains of them. So the question is asked of the
IR tree, in `lir::escape`, and the answer is handed to the lowering, which
registers a drop the way it registers a `defer` and lets the machinery it already
has place it. Asking it on the CFG instead would mean rediscovering the scopes
the ladder was built from.

The test is written as a **whitelist**, which is the blunt rule above read from
the other side: a candidate survives only if every mention of it is the base of a
place being read or written — `p.*.x`, `p.*`, `p.*.x = 1`. Anything else
disqualifies it, including forms that would be provably fine, because a whitelist
that is wrong leaks and a blacklist that is wrong corrupts memory.

One ordering rule falls out of the ladder being shared. A rung is built at the
first exit that needs it (§3), so a `let` that comes *after* an exit has no slot
yet when that rung is built, and putting a drop there would free a slot the path
never wrote. Such an allocation is not a candidate; it is collected like anything
else.

## 6. GC safepoints, and why pointers get redefined

The collector needs two things: to know when it may run, and to know which stack
slots hold pointers when it does.

A **safepoint** is a point where collection may happen — a call, an allocation, a
loop back-edge. LIR annotates each with the set of live pointer-typed locals.

```
_7 := call g(p)
    @safepoint { live: [p_1, q_2]
      p_1 := reloc p_1
      q_2 := reloc q_2
    }
```

It sits **on** the statement rather than becoming a block, because the
association between "this call" and "the collection that may happen inside it" is
what an LLVM statepoint needs and a separate block loses. A back edge is the same
annotation on the terminator.

Whether that becomes a shadow stack, an LLVM statepoint, or something else is a
**codegen** decision — different collectors want different mechanisms, and the
expensive part (computing precise liveness) is the same for all of them.

`live` is one list, not two. It is the root set the collector traces *and* the
list of relocations, because with a moving collector every one of those locals
holds a different address afterwards — `live: [p]` beside `p := reloc p` would be
the same fact written twice, and two copies of a fact are two things that can
disagree. A dump prints the `reloc` lines because a redefinition is what the list
*means*.

The set is the one live **before** the statement, not after it. Collection
happens while the statement is running — inside the callee, inside the allocator
— and at that moment the destination has not been written, so relocating it would
relocate whatever the slot happened to hold.

### What counts as a root

A local whose type can contain a reference: a pointer, a slice, a `dyn`, and any
aggregate holding one — a `str` is a root because it *is* a `[]u8` by this
level (§9),
and an enum is one when a *variant* holds a pointer, since its flattened payload
member is `[N]u8` and says nothing.

One over-approximation is left, and it is named rather than hidden: a `*T` is a
root whatever `T` is, so a vtable pointer — a `*void` by this level — is counted
with the rest. Narrowing it wants a distinction between a managed reference and a
machine address that the type system does not draw today. Counting a code pointer
is safe and costs a word in a stack map, so it waits for the language to have the
distinction rather than for the pass to guess at it.

### Why `reloc` exists

Assume the collector may **move** objects — copy them to a new address and update
every reference. Now consider:

```
p := alloc Node            // p holds 0x1000
t := call g(x)             // GC runs inside g; the Node is copied to 0x5000
u := p.field               // reads 0x1000
```

`p` is a raw address in a register or a stack slot. The collector *can* find and
update it — that is exactly what the safepoint's live set is for. The problem is
not the collector. The problem is every pass that runs afterwards.

If LIR does not say that `p` changed across the call, then as far as any
optimization is concerned `p` is the same value before and after. Which licenses:

- hoisting `p.field` out of a loop that contains a call,
- common-subexpression-eliminating two loads of `p.field` either side of a call,
- keeping `p` in a callee-saved register across the call and reusing it after.

Each is a correct transformation on the LIR as written, and each produces a read
from a stale address. The failures are non-deterministic, depend on when
collection happens, and surface as corruption far from the cause.

`reloc` fixes this by making the safepoint a **definition site**. After it, `p`
is a *new* definition of `p`. Any pass that respects def-use chains — which is
all of them — stops moving loads across the safepoint automatically, with no
GC-specific knowledge. This is the same reason LLVM's `gc.relocate` returns a new
SSA value per live pointer instead of mutating in place.

```
bb3:
  t7: i32 := call g(p) @safepoint {
    live: [p, q]
    p := reloc p            // p is redefined HERE
    q := reloc q
  }
  t8: i32 := p.field        // provably reads the post-GC address
```

**If the collector turns out to be non-moving**, codegen drops the relocs as
identity and nothing is lost — the cost of assuming "may move" is zero. The cost
of assuming "never moves" and later wanting a copying or generational collector
is a change to safepoint semantics, which means revisiting every pass that reads
them. That asymmetry is the whole argument.

### Interior pointers

A slice may point into the **middle** of an object: `s[2..5]` of a buffer, or a
`str` cut out of a longer one. The collector must therefore resolve an arbitrary
address back to the object that contains it.

This is the most expensive constraint in this design and it should be budgeted as
real work, not a detail. It needs an **object-start table** — per-card offsets
letting any address find its containing object's base — and, because the
collector may move, relocation has to preserve each interior pointer's *offset*
rather than simply rewriting an address.

It is the right call: without it, every subslice would have to carry its owner
separately, which costs a word on the most common data type in the language.

### User control

Three intrinsics:

| Intrinsic | Meaning |
|---|---|
| `$gc_collect()` | Request a collection now. |
| `$gc_keep_alive(x)` | A no-op that **counts as a use**, so `x` stays in the live set up to this point. |
| `$gc_pin(x)` | Make an object immortal and immovable. |

`$gc_keep_alive` exists for a specific failure. Liveness ends at the last *read*,
so this is wrong:

```
let buf := alloc Bytes
let p   := &buf.data
some_c_func(p)          // `buf` is already dead here — nothing reads it again
```

The collector may free or move `buf` during the call even though C is using its
address. `$gc_keep_alive(buf)` after the call extends the live range across it.

`$gc_pin` is for handing a pointer to C for longer than one call. A pinned object
is never moved and never collected, which is a leak by construction — that is the
trade, and it is why the intrinsic is explicit rather than inferred.

### What is not a root

A pointer that is dead after the safepoint is not in the live set, and a
non-pointer local never is. Precision matters here for a reason beyond
performance: an over-approximate live set keeps garbage alive, and with a moving
collector it also means relocating objects nothing will ever read again.

## 7. Names and metadata

**LIR has no concept of an `impl`.** A method is a function with a name, and that
name is flat. Where the IR prints `<impl Wrapper>.own` or
`core.<impl []T>.len`, LIR prints one symbol.

**Every LIR function carries the name it will have in the binary** — the exact
symbol the linker sees. That means mangling happens at or before LIR lowering,
not in codegen, and that `@link_name("...")` has already been applied by the time
a function reaches LIR.

Two names are therefore worth keeping side by side on each function:

| Field | Example | Used for |
|---|---|---|
| `name` | `core.Vec.<i32>.push` | dumps, diagnostics, profiles |
| `symbol` | `_NC4core3VecIi32E4push` | the object file; what LIR *is* keyed by |

`symbol` is the authority. `name` exists because a reader debugging a LIR dump
should not have to demangle by hand, and it is dropped at codegen.

The precedence for `symbol`:

1. `@link_name("...")` if present — verbatim, no mangling.
2. Otherwise the mangled form of the canonical path plus type arguments.

An `extern("c")` function with no `@link_name` mangles to its bare name, because
that is what C expects.

### Mangling, and why monomorphization owns it

A mangled name has one job: **be injective**. Two instantiations that differ in
any way a linker could confuse must produce different symbols, and the same
instantiation reached from two files must produce the same one. Everything else
— readability, brevity, resemblance to another language's scheme — is secondary
to that.

Monomorphization is where the name is decided because monomorphization is where
the *identity* is decided. It walks the call graph from the entry points,
instantiates each generic function for each distinct set of arguments, and keys
the result by those arguments; the symbol is the encoding of that key. Deciding
it later, in codegen, would mean recomputing the key from a type it has already
been given — and any disagreement between the two computations is a duplicate
symbol or a missing one.

The scheme, Itanium-flavoured because the length-prefixed form is easy to demangle
and uses only characters every object format accepts:

```
_NC 4core 3Vec I i32 E 4push
│   └─ length-prefixed path components ─┘ │      └ the member
│                                         └ type arguments, I ... E
└ prefix: Nest, mangled
```

| Argument | Encoding | Example |
|---|---|---|
| integer primitive | `i`/`u` + width, `is`/`us` for pointer-sized | `i32`, `u8`, `us` |
| float primitive | `f` + width | `f64` |
| `bool` / `char` / `void` / `never` | `b` / `c` / `v` / `N` | |
| `*T` / `*mut T` | `P` / `Pm` + inner | `Pi32` |
| `[]T` / `[]mut T` | `S` / `Sm` + inner | `Si32` |
| `[N]T` | `A` + length + inner | `A3i32` |
| tuple | `T` + elements + `E` | `Ti32bE` |
| `func(...) -> R` | `F` + parameters + `E` + result | `Fi32Eb` |
| nominal | `N` + length-prefixed canonical path + args in `I ... E` | `N4core6OptionIi32E` |
| `dyn Trait` | `D` + the trait's path + `E` | `D4core8ToJsonE` |
| `const` argument | `K` + the value's type + sign + the value | `Kusp3`, `Kb1`, `Ki32n5` |

Four details earn their place:

- **A `const` argument carries its type.** Const generic parameters are not
  `usize`-only — a parameter may be any primitive — so `K3` would be ambiguous
  between `3usize` and `3u8`, and those are different instantiations.
- **Every encoding starts with a letter, and a numeric value carries its sign.**
  `n` marks a negative value because `-` is not safe in every object format, and
  `p` marks a non-negative one because *something* has to. Both exist for the
  same reason as the `N` on a nominal: an integer is a letter followed by digits
  and a path component *starts* with digits, so `i324core3Foo` would be either
  `i32` then `core.Foo` or `i324` then something, and `Ku167` would be `u16` at
  `7` or `u167` at nothing. One letter between them settles both.
- **A nominal's arguments are written `I ... E` even when there are none.** The
  list is what tells a reader where the length-prefixed path stops; without it
  `N4core6OptionN4core6Option` could be one four-component path or two
  two-component ones.
- **A primitive mangles as a primitive, even though it is sugar.** `i32` is
  `int.<32>` in the type system, and mangling it that way would make every
  symbol in every program longer to record something no two types disagree
  about. The sugar *is* the canonical spelling here. `usize` / `isize` get the
  same treatment for the same reason, even though since §3.1 they are `distinct`
  declarations in `core` rather than primitives: `us` and `is`, keyed on the
  `#lang` tag, not `N4core5usizeIE`.

The arguments are split where the declaration's are: those belonging to the
enclosing `impl` go on the type the impl is for, the function's own on the
function. That is what makes `core.Vec.<i32>.push` mangle as
`_NC4core3VecIi32E4push` rather than hanging everything off the end. A path with
nowhere to put them — a free function — takes them all on the function.

A **trait impl's** member carries one more thing: `X` followed by the trait it
implements, between the type and the member.

```
_NC 4Vec3 X N4ConvIi32E 2to        // Vec3.<as Conv.<i32>>.to
```

This is not decoration. `impl Conv.<i32> for Vec3` and `impl Conv.<bool> for
Vec3` are two coherent impls (§4.9 — they do not overlap, because the trait's
arguments differ), they declare the same member name, and that member has the
same canonical path in both. Without the trait they would have the same symbol,
which is the one thing a mangled name may not allow. An **inherent** impl's
member takes no qualifier: there is no trait, and the path already names it
uniquely.

Nothing outside monomorphization may construct a symbol. A pass that needs one
asks the instantiation it already holds.

### Directives that survive to LIR

Directives are *carried* through the whole pipeline (`Def` and `ir::Function`
both hold a `Vec<Directive>` and the front end deliberately passes along ones it
has no opinion about). These are the ones LIR and codegen must still see:

| Directive | On | Meaning at this level |
|---|---|---|
| `#packed`, `#align(N)`, `#soa` | types, fields | already consumed by layout, but kept for debug info and for FFI checks |
| `#section("...")` | functions, constants | which object-file section the symbol lands in |
| `#offset(N)` | functions, constants | a fixed position in the generated binary |
| `#inline` | functions | a codegen hint, never semantics |
| `#raw` | fields | no zero-initialization |
| `#unsafe` | functions, blocks | checks suppressed |

`#section` and `#offset` are the two that do not exist yet anywhere — see the
plan.

## 7b. Type definitions, and aggregates flattened to structs

**LIR carries the program's type definitions, and by the time it does there is
only one aggregate shape left: the struct.** A tuple, an enum and a slice are all
lowered into plain structs during IR → LIR.

The IR already carries type definitions — `ir::TypeDef`, with structs, enums and
`distinct`s, member types in the side table — because a `Ty::Nominal` is only a
*name*: it says which type, never what is in it, and layout, exhaustiveness and
codegen all need the contents. What LIR adds is the flattening:

| Source shape | LIR |
|---|---|
| `struct { a: T, b: U }` | itself |
| tuple struct, `(A, B)` | a struct with positional members `0`, `1` |
| `distinct T` | **`T` itself** — the name is gone, see below |
| `[]T` / `[]mut T` | `struct { ptr: *T, len: usize }` |
| `enum { a, b(T) }` | `struct { tag: uN, payload: <union of the variants> }` |
| `dyn Trait` | `struct { data: *void, vtable: *void }` |
| a vtable | a struct of function pointers, and one constant per impl |
| `[N]T` | **stays an array** |

The reason to do it here rather than in codegen is that every LIR pass after this
point asks structural questions — what is at this offset, is this field a
pointer, how big is this local — and each aggregate that keeps its own shape is
one more case every one of those passes has to learn. Flattened, a place
projection is *always* "member `n` of a struct", and the drop, root and layout
passes each have one rule instead of five.

### Vtables are data, and data is a struct

A vtable is not a language construct at this level; it is a **constant**. Each
`impl` that a `dyn Trait` may select gets one, whose type is a struct of function
pointers in the trait's declaration order — the order `ir::TypeDef`'s
`TypeDefKind::Trait` already fixes, for exactly this reason — and whose value is
the addresses of that impl's methods. A `dyn` call is then two ordinary
operations: project the slot, call through the pointer.

Making it a struct rather than a shape of its own is the same argument as the
rest of the table: a vtable has an address, a layout and a member at an offset,
and every pass that already handles those handles it for free. The trait
disappears; the ordering it fixed does not.

### Arrays do not flatten

`[N]T` stays an array in LIR, and this is the one aggregate that keeps its own
shape. The reason is that an array is not "a struct with N members that happen to
match":

- **Its index is a value, not a name.** `xs[i]` for a run-time `i` is address
  arithmetic — `base + i * stride`. A struct projection is a constant offset
  chosen at compile time. Flattening would either lose the dynamic form or
  reintroduce it as a special case on structs, which is the case the flattening
  was meant to remove.
- **Its length is part of its type, not of its contents.** `[3]i32` and `[4]i32`
  differ in a number LIR must keep to bounds-check, to compute a stride, and to
  emit debug info that says "array of 3" rather than "struct of three fields".
- **A thousand-element array is one entry, not a thousand.** A struct of `N`
  members costs `N` member records in the type table and in debug info. `[4096]u8`
  is a normal thing to write.
- **The GC walks it as a run.** A root map for an array of pointers is "this
  many, this far apart", which is one entry; as a struct it is one entry per
  element.

So LIR's aggregates are: **struct, and array**. Everything else — tuple, enum,
slice, trait object, vtable — is a struct by the time LIR sees it, and a
`distinct` is not an aggregate at all: it is whatever it is distinct from. A
slice is the interesting near-miss: `[]T` *does* flatten, because a slice is a
pointer and a length, and neither of those is indexed by a run-time value. The
indexing happens through the pointer it holds.

### Pointer arithmetic exists here and nowhere above

That last sentence is an instruction, not a figure of speech. `s[i]` on a slice
is `s.ptr + i` in elements, and LIR has an `offset` rvalue for it:

```
t0 := s.ptr
t1 := t0 + i * stride(i32)
t2 := t1.*
```

The **source language has no pointer arithmetic**, deliberately: an address you
can move is an address you can move wrongly, and every sequence the language has
carries its own bounds. But the flattening above is what makes a slice's element
unreachable by projection — a struct has members, not elements — so the
arithmetic has to exist somewhere below the point where the bounds stopped being
part of the type. This is that point.

It is in **elements** and carries the element type rather than a byte stride,
for the same reason everything else here carries a type: the stride is layout's
answer (§7cc) and there should be one of it. An array needs none of this: it
kept its own shape, so `a[i]` is an ordinary projection and `&a[i]` an ordinary
address-of.

The enum row is the one with a real decision in it: the tag's width and whether
the payload is laid out as an overlapping union or as the widest variant are
layout's to make, not the lowering's. What the lowering fixes is only the
*shape* — a tag member and a payload member — so `(e as Some).0` becomes an
ordinary two-step projection.

Note this is a change of representation, not of information: the enum's variants
and their names stay reachable through the type's definition, which is what a
LIR dump prints and what debug info is emitted from.

## 7c. Debug info is emitted from LIR, so LIR carries what it needs

By the time codegen runs, the AST is gone, generics are gone, and a function's
source identity is one of `N` instantiations that never appeared in the source at
all. **Everything a debugger needs therefore has to be reachable from LIR**, and
carried deliberately rather than reconstructed.

What that means concretely, per function:

| Fact | Where it comes from | Why a debugger needs it |
|---|---|---|
| unmangled `name`, with concrete arguments (`core.Vec.<i32>.push`) | monomorphization | what a stack frame is labelled |
| `symbol` | monomorphization | tying a frame to an address |
| declaring file, line, column | the def's span | "step into" and breakpoint resolution |
| per-instruction span | carried on every LIR node | the line table: address → source position |
| local and parameter **names**, with their scopes | lowering, from the IR's bindings | printing `xs` rather than `%7` |
| each local's type | LIR types | interpreting the bytes at a slot |
| type definitions: members, offsets, enum variant names, array lengths | `ir::TypeDef` plus layout | rendering a value as a value |

Three consequences that shape the IR above this level:

- **A local's source name must survive lowering.** LIR renumbers everything into
  slots, so the name is metadata on the slot, set when the binding is lowered.
  A temporary that no source name produced simply has none, and a debugger shows
  it as a slot — that is honest, and better than inventing a name.
- **Enum variant names and array lengths must survive flattening.** An enum is a
  struct with a tag at this level (§7b), but a debugger showing `2` instead of
  `.green` is a worse debugger. The variant names stay on the *type definition*,
  which the flattening does not touch; the array length stays because arrays do
  not flatten at all.
- **`#inline` implies an inlining record.** If codegen inlines a call, the
  instructions that came from the callee keep the callee's spans, and gain an
  "inlined at" pointer to the call site. Without it a stack trace names a
  function the programmer never called from there.

How much of this is emitted is a build setting, not a property of LIR: LIR always
carries it, and a build that asks for no debug info simply drops it at the end.
Dropping late is cheap; reconstructing is not possible.

## 7cc. Layout: the rules a size is computed by

§7b fixes an aggregate's *shape* and says explicitly that the tag's width and
whether the payload overlaps are layout's to decide. This is where they are
decided. The rules are in `nestc/src/ir/layout.rs`, and they are **decisions**
rather than derivations — a backend has to agree with them, so they are written
down.

Layout is a **query**, not a pass: it is asked about a concrete type and
memoizes the answer. "Every type" is not a set anyone can enumerate — `[N]T` for
every `N` a program mentions, every tuple, every instantiation — so each arrives
when something needs it. What *is* enumerated is the types a program declares;
each concrete one is laid out once and the result stamped on its `TypeDef`.

| Type | Size | Alignment |
|---|---|---|
| `i<N>` / `u<N>` | `ceil(N/8)` rounded up to the alignment | that byte count rounded up to a power of two, capped at 16 |
| `f16`/`f32`/`f64` | 2 / 4 / 8 | itself |
| `f80` / `f128` | 16 | 16 |
| `bool` | 1 | 1 |
| `char` | 4 | 4 |
| `void`, `never` | 0 | 1 |
| `*T` | the target's pointer width | itself |
| `*dyn Trait` | two words | one word |
| `[]T` / `[]mut T` | two words (`ptr`, `len`) | one word |
| `[N]T` | `N × stride(T)` | `align(T)` |
| `func(...)` | one word | itself |
| tuple, struct | fields in **declaration order** | the widest field's |
| `distinct T` | exactly `T`'s | exactly `T`'s |
| `enum` | tag, then a payload every variant shares | the wider of the two |

Five of these earn a word:

- **An integer's size is a decision.** `u24` is a legal type (§3.1) and no
  machine has a three-byte load, so something has to say whether it occupies
  three bytes or four. Four — because the alternative is that `[N]u24` has a
  stride nothing can load. The alignment cap at 16 is the other half: a `u4096`
  is 512 bytes and a 512-byte alignment would be absurd, since alignment exists
  so that a load can be one instruction.
- **`size` is the stride**, tail padding included. There is no separate "data
  size", because every consumer here wants the stride and carrying two numbers
  would mean every one of them choosing.
- **Fields are never reordered.** That is a promise, not a limitation: §9's
  `#packed` is defined as *removing padding*, which only means something if the
  order is the written one, and an FFI struct that reordered itself would not be
  one.
- **An enum's payload overlaps.** One variant is live at a time, so the space is
  shared; laying them end to end would make an enum as big as all of them
  together, which is not a trade anyone wants for a type whose whole point is
  that it is one of them. The tag is the smallest unsigned integer that tells the
  variants apart — one byte for anything up to 256 of them.
- **A `distinct T` has exactly `T`'s layout.** Not "the same size as": the same
  bytes (§2.4). That is what makes a `usize` and its `uint.<64>` interchangeable
  in memory and different in the type system.

`#packed` sets every field's alignment to 1 — it is the one thing that can lower
an alignment, and that is the point of it. `#align(N)` raises an alignment, on a
type or on a single field, and the size follows: a stride has to be a multiple of
the alignment or the second element of an array would be misaligned.

### A type has a size only if the target can address it

The ceiling is **`isize::MAX` on the target**, and a type that exceeds it has no
layout — `LayoutError::TooLarge`, reported at the declaration by the stamping
pass (`ir/check/layouts.rs`), which is the one layout failure that pass reports
because it is the one no earlier check owns.

Two things make it a rule rather than an implementation limit:

- **`isize`, not `usize`.** The *difference* of two addresses inside one object
  is an `isize`, so an object larger than that has interior addresses whose
  distance apart cannot be expressed — and `&a[n] - &a[0]` is what indexing
  computes.
- **Without a ceiling the arithmetic is unanswerable, not merely unchecked.**
  `[18446744073709551615]u64` is a type a program may write, and `N × stride(T)`
  for it leaves a `u64`. The only two answers available without a rule are a
  panic in a debug compiler and a *silently wrapped* size in a release one, and
  the second is the dangerous one: every later check passes on a number that is
  smaller than the truth.

So every product and every sum in the layout arithmetic is checked as it is
computed, rather than at the end where it would already have wrapped.

### Where the target comes back

Phase 5 took the target out of the type layer on purpose: `usize` is `distinct
uint.<PTR_BITS>` over a constant `core` supplies (§3.1), so a *type* never has to
ask how wide a pointer is. A layout does — not for `usize`, whose width is
already in the type, but for a **pointer**, which is not a `usize` and has no
width written anywhere. `layout` is the one place allowed to ask; everything
else, the const evaluator included, asks `layout`.

## 7d. What a build's settings change here

A setting (`Options` in `nestc/src/common/options.rs`) is decided before the
first file is read and reaches LIR unchanged. Two of them change what gets
*lowered*, not just what gets emitted:

| Setting | What changes at LIR |
|---|---|
| `overflow=trap` | an `add` becomes a checked add plus a branch to a panic block |
| `overflow=wrap` | an `add` is a single wrapping instruction, no extra edge |
| `pointer-width` | the width of `usize`/`isize`, and therefore every layout |
| debug level | how much of §7c survives to the object file |

The overflow choice belongs at **LIR lowering**, not codegen, because the trap
form is not a flag on an instruction — it is a second basic block, an extra edge,
and a call that diverges. Every pass after lowering (drops, safepoints, liveness)
has to see that edge to be correct, so it must exist in the graph rather than
appear underneath it.

The **bounds check** is the same shape and is here for the same reason. `a[i]`
emits a comparison against the length — the constant in a `[N]T`'s type, the
`len` member of a `[]T` (§7b) — and a block that panics:

```
t0 := k < s.len
switch t0 { 1 => bb2, _ => bb1 }
bb1:                    // out of bounds
  call core.panic("index out of bounds", Location(...))
  unreachable
bb2:
  t1 := s.ptr + k * stride(i32)
```

Two things elide it, and neither is an optimization: **`#unsafe`** (§9), whose
whole meaning is that the run-time checks in that scope are off, and an index the
evaluator already worked out to be in range, whose comparison has a known answer
— the out-of-range case having been reported by `check::bounds` rather than
compiled.

**Dividing by zero is not overflow**, and is not this setting's to turn off.
`overflow=wrap` says what `i32::MAX + 1` *means*; there is no wrapped answer for
`x / 0` to have. So an integer `/` or `%` gets a comparison, an edge, and a block
that panics — §3.2's shape again — whatever the setting says, and only `#unsafe`
removes it. A divisor that is a known non-zero constant gets no branch, for the
reason a known-good index gets none.

Two things this setting does **not** change:

- **Constants.** A `::` binding *is* its value (§2.5), and one that overflows is
  refused whatever the setting says — there is no running program for the wrapped
  answer to happen in. A written `$cast` is still how the low bits are asked for.
- **The wrapping intrinsics.** `wrapping_add` wraps in a `trap` build too. That
  is the whole point of it: the program said which behaviour it wanted, and a
  setting that overrode it would make the intrinsic useless.

## 8. What LIR still carries

- **Types**, and the **definitions** behind them. Every local and every
  instruction is typed; every nominal type's contents are reachable from its
  `DefId`. Codegen needs layout, and the drop/root passes need to know what is a
  pointer.
- **Spans.** On every instruction, for debug metadata (§7c).
- **Def ids.** So a diagnostic raised in a LIR pass can name a source item.
- **Names**: the symbol each function will have, and the source names of locals.
- **Arrays**, as arrays (§7b).
- **The build's settings**, already applied to the shape of the graph (§7d).

## 9. What LIR no longer has

Generics and `const` generic parameters (monomorphization runs before lowering,
so LIR is fully concrete), traits and dynamic dispatch as *concepts* (a `dyn`
call is an indirect call through a vtable slot), `defer` as a construct,
structured control flow, the distinction between a `match`, an `if` and a
`while`, **`impl` blocks** — a method is just a function with a name — and every
aggregate shape except the struct (§7b).

**Intrinsics are also gone as calls.** A `#intrinsic` function declared in `core`
has no body to lower; where the IR has a call to one, LIR has the operation it
denotes — an instruction, a constant, or nothing at all. `size_of.<T>()` is the
number layout computed; `wrapping_add(a, b)` is one instruction. What reaches
codegen is never "a call to a function that does not exist".

**The comptime types are gone**, and so is the `$cast` out of them. A literal
written where an `i32` is wanted reaches the IR as `$cast(10: comptime_int)`,
which is bookkeeping about where the literal's type came from and was only ever
read by inference. LIR folds it into the definition — `_4 := 10` — because
`comptime_int` is a type no backend has a register for, and a cast whose source
cannot exist at run time is not a conversion.

The fold is not conditional on the source being a comptime type: **any cast the
evaluator can perform is a value**. `cast.<u16>(7)` is `7` even though the `7`
had already settled on `isize`, because the alternative is asking a backend to
emit a conversion between two constants it will fold anyway — and a rule that
holds only for the types inference happened to leave behind is a rule nobody can
state.

**Reading a discriminant is a member read.** An enum is `{ tag, payload }` by
§7b, so `s.tag` is an ordinary projection and there is no `discriminant`
operation beside it. What §4 requires is that it be read *once*, which is a
property of the decision tree rather than of the instruction set.

**And there is no `$panic`** — see §2.

**`distinct` types are gone, and not by being wrapped.** A `distinct T` *is* a
`T` in memory (§2.4) — the difference between them is a rule about which values
may be given which names, and every pass that enforces it has already run. So
LIR replaces the name with what it names: a `usize` local is a `u64` local, a
`str` parameter is a `[]u8` parameter, and `Meters` is `f64`. Nothing carries
the source name into a backend.

Wrapping it in a one-member struct instead — which is its IR shape, and was its
LIR shape until this was fixed — would be worse than redundant. **A struct of
one scalar is not passed like the scalar** under any C ABI: it can go in memory
where the scalar goes in a register, and the two disagree at exactly the
boundary FFI cares about. The wrapper would also have to be unwrapped by every
backend, at every use, to get back the type it should have had. The peeling goes
through pointers, slices, arrays and tuples for the same reason — `*usize` is
`*u64` — but stops at a nominal type's generic arguments, since `Vec.<usize>` is
the instantiation monomorphization named and renaming it here would name a
function that does not exist.

This holds for every representation, not only the scalar one: a `distinct` over
a struct **is** that struct here, over an enum that enum, and over another
`distinct` whatever that one ends at. No `TypeDef` is emitted for the name and
none is emitted for what it wraps beyond the one the representation already
had.

One consequence worth stating: the `$cast` the IR emits to peel a `distinct` —
when one reaches an inherited method, or where a program writes
`cast.<Point>(h)` — has nothing left to do here, because both sides of it are
the same type. LIR folds a cast whose two types are equal into a move, so the
peel costs an instruction only until this pass runs.

## 10. What a backend has to supply

Everything above says what LIR *is*. This section is the other side of it: the
complete list of what is still left to do when a backend receives one, so that
"is LIR low enough" has an answer somebody can check rather than believe.

### The instruction set, in full

**Four statements**, and one of them is a call:

| Statement | LLVM | C | wasm |
|---|---|---|---|
| `Assign { place, rvalue }` | the rvalue, then `store` (or an SSA def) | `p = e;` | the rvalue, then `local.set` / `store` |
| `Call { dest, callee, args }` | `call` / indirect `call` | a call | `call` / `call_indirect` |
| `Intrinsic { dest, name, args }` | an intrinsic or an instruction | a builtin or a call | an instruction |
| `Drop(operand)` | a call to the runtime's free | a call | a call |

**Four terminators**: `goto`, `switch`, `return`, `unreachable`. `switch` covers
every branch there is, so there is no `br`/`switch` pair to keep in step.

**Eight rvalues**: `Use`, `Ref`, `Builtin`, `Binary`, `Unary`, `Cast`,
`Aggregate`, `Offset`, and that is the set. `Builtin` is the checked arithmetic
`overflow=trap` needs — `checked_add(a, b)` yielding `{ value, overflowed }`
(§7d) — which is one LLVM overflow intrinsic, one C helper, or one wasm sequence. `Binary` and `Unary` are machine operations on **scalars** —
nothing structural ever reaches one, which is why text equality is a call to
`core` (§6.13) rather than an `==` on a `{ ptr, len }`.

`Cast` carries both types, so what the conversion *is* — a truncation, a sign
extension, a rounding, an int-to-float — is a lookup rather than a derivation.
`Offset` is a GEP in elements, carrying the element type rather than a byte
stride, so the stride comes from the same layout everything else does.

**Two shapes of intrinsic** remain by phase 10: the GC ones (`gc_collect`,
`gc_keep_alive`, `gc_pin`), `transmute`, the wrapping arithmetic, and `trap`.
Each is one instruction or one runtime call. `transmute` is the only one whose
result type is read off `dest` rather than carried in the operation, because the
operation *is* "reinterpret as whatever this slot holds".

### The four things a backend genuinely does itself

1. **Materialize constants.** A `Constant::Value` may be a `Str`, a `Bytes` or an
   `Aggregate`. A text constant becomes a private global plus the `{ ptr, len }`
   the type wants; an aggregate becomes an initializer laid out by the same
   `TypeDef` a local of that type uses. This is emission, not analysis — the
   value is fully known.
2. **Turn safepoints into stack maps.** §6 computed the live set; what shape it
   takes — a shadow stack, an LLVM statepoint, a side table — is the backend's,
   and different collectors want different ones. A non-moving collector drops
   the `reloc`s as identity.
3. **ABI classification.** Which arguments go in registers, which are returned
   indirectly, what an `extern("c")` function's signature means on this target
   (§11.3). LIR carries the ABI name and the types; the classification is
   per-target and belongs where the target is.
4. **Register allocation and instruction selection**, which is the backend's
   whole job and is not something an IR can pre-answer.

### The one thing LIR does not carry on its own

**Resolving a `Ty::Nominal` to its `TypeDef` needs the compiler's def table.**
`Program::types` is keyed by `mono::type_key`, and computing that key from a `Ty`
takes a `&DefTable`. A backend living in this compiler has one, so it is not a
blocker — but it does mean `Program` is not a standalone artifact that could be
serialized and handed to another process. Closing that wants a `TypeId` on every
local instead of a `Ty`, which is a change worth making when there is a second
consumer to justify it and not before.

### Known over-approximations, named rather than hidden

- **A `*T` is a GC root whatever `T` is** (§6), so a vtable pointer — a `*void`
  by this level — is traced with the rest. It wants a distinction between a
  managed reference and a machine address that the type system does not draw.
- **Escape analysis is intra-procedural and blunt** (§5). Per-function summaries
  are a change of precision, not of shape.
- **A slice pattern's elements go through the pointer**, the same way `xs[i]`
  does, since a slice has members and not elements (§7b). An earlier lowering
  projected `xs[0]` off the header instead; an invariant test now says no place
  indexes a slice.
- **A `str` pattern longer than a handful of bytes** still calls the same byte
  comparison every other one does. A length-dispatched jump table would be
  faster and is an optimization, not a lowering.

### Things that look like gaps and are not

- **A block with no predecessors.** An exhaustive `match`'s fallback is
  `unreachable` and nothing jumps to it. That is the guarantee, written down.
- **`&p.*` on a pointer.** An identity that any backend folds, produced where the
  source took the address of a dereference.
- **A `switch` on a `bool`.** One-bit switches are fine everywhere; there is no
  separate two-way branch precisely so that there is one form to handle.
- **An `i128` arm value on a narrower switch.** Arm values are widened for
  storage, not for meaning; each fits the operand's own type.
- **Division.** `x / 0` traps before the divide (§7d), so the instruction a
  backend emits has no undefined case left except `INT_MIN / -1`, which the
  checked form catches under `overflow=trap`.

### How the shape above is held in place

`nestc/src/lir/tests.rs`, with the snapshots in `nestc/src/lir/snapshots/`.
They live with the pass rather than with `sema` because what they test is this
pass: source goes in, and the LIR that came out the far end is what is checked.
Two kinds of test, guarding two different things.

**Snapshots, from source to LIR.** Each one compiles a whole program — the entry
file plus `core` — and records the dump of every function the entry file
defined. A graph is the one representation where a reader cannot reconstruct
intent from the shape, since every construct becomes the same jumps, so "did
this `while` become the right three blocks" is a question only the whole
lowering can answer. There is one per construct: the loops, the ladder's four
rungs, the decision tree over enums and tuples and ranges and text, the checks
(§7d), monomorphization, statics, casts, safepoints on the back edge, dynamic
dispatch against a bound resolved at the call, and a declaration with no blocks.

**One program, not one per file.** `link` merges the per-file IR into a single
`Linked`, monomorphization runs over that, and this pass emits one
`lir::Program` holding every function that survives — the entry file's,
`core`'s, and every instantiation neither file wrote. Codegen receives that one
value; there is nothing to link after it. The snapshots render only the entry
file's functions, because a test about `while` should not be a record of the
standard library, so a dump showing `call core.panic` without `core.panic`
under it is the renderer filtering rather than the program being split. A test
says so, and the invariant below — every direct call names a function the
program defines — is checked over the whole thing, `core` included.

**Vtables.** One snapshot is about the dispatch — two projections and an
indirect call — and one about the **data**: a trait with three methods so the
slot order is visible, two impls so there are two constants, an impl for
`Box.<i32>` so the vtable is for the instantiation rather than the generic, and
one type coerced twice, which shares its constant instead of emitting a second.

**Invariants, over any lowering.** A snapshot catches a change; it cannot say
what *any* program may produce. The invariant tests say that: every place and
every live local names a slot that exists, block ids are dense and 0 is the
entry, every direct call names a function the program defines, no `Binary` has
an aggregate operand, no local has a type a machine cannot hold, and every
nominal type a local mentions is in the type table. They run over `core` too —
it is code a backend has to emit, and a shape it alone produces is exactly the
one nothing else would catch.
