# LIR — the low-level IR

**Status**: design, not yet built. Nothing in `nestc/` implements this document.

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
  t1 := call g(&p) @safepoint(live: [p])  // 7:14
  switch t1 { true => bb1, false => bb2 }
bb1:
  return t1
bb2:
  return false
}
```

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

## 3. `defer`, placed once per exit path

`defer` bodies run when the enclosing scope exits — by falling off the end, by
`return`, by `break`, or by `continue`. Ordering is reverse of registration, and
outer scopes run after inner ones.

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
let p := alloc Node        // never passed anywhere
p.x = 1
t := p.x
```

```
bb0:
  p := alloc Node
  p.x = 1
  t := p.x
  drop p                   // provably dead at scope exit
  ...
```

A `drop` sits on the same cleanup ladder as `defer`, for the same reason: an
allocation in a scope with three exits is freed once, in a block all three reach.

Extending this to per-function escape summaries later is a change of *precision*,
not of shape — the drop insertion machinery does not move.

## 6. GC safepoints, and why pointers get redefined

The collector needs two things: to know when it may run, and to know which stack
slots hold pointers when it does.

A **safepoint** is a point where collection may happen — a call, an allocation, a
loop back-edge. LIR annotates each with the set of live pointer-typed locals.

```
t7: i32 := call g(p) @safepoint {
  live: [p, q]
  p := reloc p
  q := reloc q
}
```

Whether that becomes a shadow stack, an LLVM statepoint, or something else is a
**codegen** decision — different collectors want different mechanisms, and the
expensive part (computing precise liveness) is the same for all of them.

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
| `distinct T` | a struct with one member — already its IR shape |
| `[]T` / `[]mut T` | `struct { ptr: *T, len: usize }` |
| `enum { a, b(T) }` | `struct { tag: uN, payload: <union of the variants> }` |
| `dyn Trait` | `struct { data: *void, vtable: *void }` |

The reason to do it here rather than in codegen is that every LIR pass after this
point asks structural questions — what is at this offset, is this field a
pointer, how big is this local — and each aggregate that keeps its own shape is
one more case every one of those passes has to learn. Flattened, a place
projection is *always* "member `n` of a struct", and the drop, root and layout
passes each have one rule instead of five.

The enum row is the one with a real decision in it: the tag's width and whether
the payload is laid out as an overlapping union or as the widest variant are
layout's to make, not the lowering's. What the lowering fixes is only the
*shape* — a tag member and a payload member — so `(e as Some).0` becomes an
ordinary two-step projection.

Note this is a change of representation, not of information: the enum's variants
and their names stay reachable through the type's definition, which is what a
LIR dump prints and what debug info is emitted from.

## 8. What LIR still carries

- **Types**, and the **definitions** behind them. Every local and every
  instruction is typed; every nominal type's contents are reachable from its
  `DefId`. Codegen needs layout, and the drop/root passes need to know what is a
  pointer.
- **Spans.** On every instruction, for debug metadata.
- **Def ids.** So a diagnostic raised in a LIR pass can name a source item.

## 9. What LIR no longer has

Generics and `const` generic parameters (monomorphization runs before lowering,
so LIR is fully concrete), traits and dynamic dispatch as *concepts* (a `dyn`
call is an indirect call through a vtable slot), `defer` as a construct,
structured control flow, the distinction between a `match`, an `if` and a
`while`, **`impl` blocks** — a method is just a function with a name — and every
aggregate shape except the struct (§7b).

`distinct` types are also gone. A `distinct T` has exactly `T`'s representation,
so the `$cast` the IR emits when a distinct type reaches an inherited method is a
no-op here: by this point the check that the method is *available* has already
happened, and LIR sees two names for one layout. It does not need to know which
was written.
