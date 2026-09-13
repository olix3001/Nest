# LIR — the low-level IR

**Status**: **all of it is built.** `nestc/src/lir/` is the representation
(`mod.rs`), the lowering (`lower.rs`), the escape analysis (`escape.rs`), the
safepoint pass (`safepoint.rs`), the codegen-unit split (`unit.rs`) and the dump
(`pretty.rs`). §11 — codegen units — is the newest part, and §10 is the list a
backend answers for.

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
unit f {                                    // one codegen unit (§11)
  type Point = struct {                     // size 8, align 4
    x: i32                                  // +0
    y: i32                                  // +4
  }

  func f(a_0: i32, b_1: i32) -> bool  // _NC1f   // examples/f.nest:3:1
    let p_2: Point                          // 4:7
    let _3: i32
    let _4: bool
  bb0:
    _3 := add.i32 a_0, b_1                  // 5:11
    p_2.x = _3                              // 6:3
    _4 := call g(&p_2)                      // 7:14
      @safepoint { live: [p_2]
        p_2 := reloc p_2
      }
    switch.bool _4 { 1 => bb1, _ => bb2 }
  bb1:
    return _4
  bb2:
    return false

  declare func g(p_0: *Point) -> bool  // _NC1g
}
```

**Everything in a dump is written out in full**: an operation is a word and a
type (`add.i32`, `lt.u64`), a type is named the way the source names it, a
function is its whole path with its symbol beside it, and a global is its name
and never its index. The indices the structures hold are how a *backend* resolves
a reference; a person reading a dump should never have to.

There are **three** instructions — an assignment, a call and a `drop` (§5) — and
four terminators. That is the whole set, and keeping it that small is the point:
a backend for C, for LLVM, or for wasm has to answer for each of them, so every
operation this level invents is a question asked of every backend that will ever
exist. §10 is that list from the backend's side.

A **call** covers three things that differ only in how the code is reached: a
symbol, a pointer, and an intrinsic. `call f(x)`, `call (t0)(x)` and `$new()` are
one statement with three callees, not three statements, because everything else
about them — arguments evaluated into operands, a result that may be discarded, a
block that ends when the operation does not return — is the same question.

`:=` **introduces** a value into a local; `=` **stores** into a place. That is
the same distinction the source language draws, so it costs a reader nothing to
carry across. Every instruction carries the source span it came from, printed as
a trailing comment — codegen needs them for debug metadata, and a span that is
only reconstructed at the end is a span that is wrong.

### Places

A **place** is an lvalue path: a local, plus a chain of projections.

```
p                              the local itself
p.x                            member `x`
p.*                            the pointee (dereference)
p[i]                           element `i`
p.*.next.x                     chained
(p.payload as Shape.circle).0  the payload, read as a variant's own type
```

A place is never a value. `_3 := p_2.x` *loads* from a place; `p_2.x = _3` *stores*
into one. Only the store form appears on the left of `=`.

A **base** is a local or a global, and nothing else. A `::` constant that fits in
a register is not one: §2.5 says such a constant *is* its value. One that does
not fit — a string's bytes, an array constant — **is** a global by the time it
gets here, because a blob is storage and storage has an address (§9, and §10's
note on the data section). So `TABLE[1]` indexes the global holding `TABLE`, and
`(a + b).x` still gets a slot of its own. Either way a backend never meets a base
it cannot address.

Field projections print **by name**, not by index. The index is what codegen
wants, but a reader debugging a mis-lowered access needs to know which field it
was — and after `#packed` / `#align` / `#soa` have had their say, the index alone
does not tell them.

### Terminators

```
goto bb1
switch.u8 t { 0 => bb1, 1 => bb2, _ => bb3 }
return t
unreachable
```

`switch` covers every branch: a two-way `if` is a switch on a `bool`, and a
`match` is a switch on a tag (see §4). It carries the **type** it switches at,
because the arm values are stored widened and the width they are compared at is
the operand's, not the storage's. Calls are *instructions*, not terminators — a
consequence of the panic model below.

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

They are not shared across **registration counts** either. Spec §8.4: *a `defer`
never reached does not run*, so an exit written above a `defer` leaves a scope
whose list is shorter, and it gets a rung of its own:

```
f :: func (n: i32) -> i32 {
  if n > 10 { return 1 }      // registers nothing — returns directly
  defer first()
  if n > 5 { return 2 }       // runs first()
  defer second()
  return 3                    // runs second(), then first()
}
```

That is what decides the `defer` body is a **statement** in the IR
(`ir::StmtKind::Defer`) and not a list hoisted to the block: hoisting loses the
position, and the position is the whole question. The lowering registers one as
it walks past it, and a rung is keyed by `(scope, kind of exit, how many are
registered)`.

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
  _0 := e.tag                        // an ordinary member read (§7b)
  switch.u8 _0 { 1 => bb1, 0 => bb4, _ => bb5 }
bb1:                                 // .some
  x_1 := (e.payload as Option.some).0
  _2 := gt.i32 x_1, 0
  switch.bool _2 { 1 => bb2, _ => bb3 }
bb2: ... A ... goto join
bb3: ... B ... goto join
bb4: ... C ... goto join
bb5: unreachable
join:
```

Tests are ordered so each is performed **once**: the tag is read a single
time even though two arms match `.some`. A guard is a test like any other, except
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

### Directives become decided attributes

Directives are *carried* through the front end (`Def` and `ir::Function` both
hold a `Vec<Directive>`, and the front end deliberately passes along ones it has
no opinion about). A directive is an AST-shaped thing, though — a name and a list
of arguments — and re-reading one is work every backend would do identically and
could do differently. So the ones that still matter are **decided here**, into a
`FunctionAttrs` a backend reads rather than interprets:

| Directive | Becomes | Meaning at this level |
|---|---|---|
| `#section("...")` | `attrs.section` | which object-file section the symbol lands in |
| `#offset(N)` | `attrs.offset` | a fixed position in the generated binary |
| `#inline` | `attrs.inline` | a codegen hint, never semantics |
| `#unsafe` | `attrs.unchecked` | the checks this body was compiled without |
| `@public` | `attrs.public` | whether the symbol must be visible outside the program — everything else may be given internal linkage (§11) |

`#packed`, `#align(N)` and `#soa` do not appear: layout consumed them, and what
they decided is in the offsets the type table already carries. `#raw` is a
field's, and zero-initialization is the global's initializer being absent.

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
| `distinct T` | **`T` itself** — the name is gone, see §9 |
| `[]T` / `[]mut T` | `struct { ptr: *T, len: usize }` — **one** type, not two (§9) |
| `enum { a, b(T) }` | `struct { tag: uN, payload: [N]u8 }`, plus a struct per variant |
| `*dyn Trait` | `struct { data: *void, vtable: *vtable.Trait }` |
| a vtable | `struct` of function pointers per **trait**, and a global per **impl** |
| `[N]T` | **stays an array** |

A type is a [`lir::Ty`], not the front end's: what a machine holds rather than
what a program may say. Generics are gone (monomorphization), `distinct` is gone
(§9), mutability is gone (§9), a `char` is the `u32` it is, and every aggregate
is an **index into the unit's own type table**. That last one is what makes a
unit self-contained (§11): nothing in it needs the compiler's def table to be
understood.

The reason to do it here rather than in codegen is that every LIR pass after this
point asks structural questions — what is at this offset, is this field a
pointer, how big is this local — and each aggregate that keeps its own shape is
one more case every one of those passes has to learn. Flattened, a place
projection is *always* "member `n` of a struct", and the drop, root and layout
passes each have one rule instead of five.

### Vtables are data, and data is an ordinary global

A vtable is not a language construct at this level, and it is not a construct of
LIR's either. It is **one struct type per trait** and **one immutable global per
impl**:

```
type vtable.Draw = struct {           // one per trait, slots in declaration order
  area:      func(*void) -> i32       // +0
  perimeter: func(*void) -> i32       // +8
  sides:     func(*void) -> i32       // +16
}
const vtable.Draw.for.Square: vtable.Draw = { &Square.area, &Square.perimeter, &Square.sides }
type *dyn Draw = struct { data: *void, vtable: *vtable.Draw }
```

The slot order is the trait's declaration order — the order `ir::TypeDef`'s
`TypeDefKind::Trait` already fixes, for exactly this reason — and *which*
function fills each slot was decided by monomorphization, because the
instantiated method that fills one does not exist until that pass makes it.

Two consequences, and both are the point. Building a trait object is an ordinary
aggregate over two operands, the second being the address of a global. And a
dispatch is an ordinary member read: `s.vtable.*.area` is a `Field` on a struct
whose offset is in the type table like every other, so a backend never computes
`n * pointer_size` by hand. Nothing about vtables is left in the instruction set.

The vtable pointer's type is the **trait's**, not the impl's. A `dyn` has erased
the concrete type, so every impl's table has to be a value of one type for the
slot offsets to be knowable at the call; a per-impl vtable type would put the
backend straight back to computing offsets itself.

### A variant is a type, so reading one is a member read

An enum is `{ tag, payload }` with one payload big and aligned enough for every
variant (§7cc). What the payload holds is **also a type** — one struct per
variant, whose members sit at offsets relative to the payload:

```
type Shape = struct { tag: u8, payload: [8]u8 }
  // tag 1 => .circle as Shape.circle(0: i32 +0)
  // tag 2 => .rect   as Shape.rect { w: i32 +0, h: i32 +4 }
```

so `(s.payload as Shape.rect).w` is the whole of what reading a variant is: a
member, a reinterpretation, and a member. The cast is a pointer cast on every
target and is always legal — the shared payload is aligned for the widest
variant, so it is aligned for each of them — and the field offsets under it come
from the table rather than from a rule a backend has to know. The alternative,
handing a backend `[N]u8` and the sentence "read these bytes as the variant's
fields", is the one place where a place's type would not describe the bytes
under it.

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

It is in **elements**, and the stride beside it is a **number of bytes** — the
element layout's size, tail padding included. Every other size in LIR is a number
(a `TypeDef`'s layout, a member's offset), and a type here would send a backend
back through the layout engine for an answer this compiler has already computed.
An array needs none of this: it kept its own shape, so `a[i]` is an ordinary
projection and `&a[i]` an ordinary address-of.

The enum row is the one with a real decision in it: the tag's width and where
the shared payload starts are layout's to make, not the lowering's. What the
lowering fixes is only the *shape* — a tag member, a payload member, and a type
per variant saying how to read it.

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
| `overflow=trap` | `add` becomes `add_checked` plus a branch to a panic block |
| `overflow=wrap` | `add` is a single instruction, no extra edge |
| `pointer-width` | the width of `usize`/`isize`, and therefore every layout |
| `codegen-units=N` | how many units the program is cut into (§11) |
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
_0 := lt.u64 k_1, s_2.*.len
switch.bool _0 { 1 => bb2, _ => bb1 }
bb1:                    // out of bounds
  _3 := []u8(&const.str.0, 19)      // the message is data (§2.5)
  _4 := core.Location(…)
  call core.panic(_3, _4)
  unreachable
bb2:
  _5 := s_2.*.ptr + k_1 * 4
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
- **The wrapping intrinsics.** `wrapping_add` lowers to `add` in a `trap` build
  too — an `add` wraps by definition (§10), and what the setting changes is
  whether a *check* is emitted around it, not what the instruction means. That is
  the whole point of the intrinsic: the program said which behaviour it wanted,
  and a setting that overrode it would make it useless.

## 8. What LIR still carries

- **Types**, and the **definitions** behind them. Every local and every
  instruction is typed, and every aggregate's contents are an index into the
  unit's own table (§7b). Codegen needs layout, and the drop and root passes need
  to know what is a pointer.
- **Spans.** On every instruction, for debug metadata (§7c).
- **Names**: the symbol each function will have, the unmangled name beside it,
  and the source names of locals.
- **Decided attributes**, not directives: the section, the inline hint, the
  offset, whether the symbol is public (§7).
- **Arrays**, as arrays (§7b).
- **The build's settings**, already applied to the shape of the graph (§7d).

What it does **not** carry is a reference to anything outside itself. There are
no `DefId`s in a lowered program: a type is an index into the unit's type table,
a function is an index into its function list, a global is an index into its
globals. That is what makes a unit a thing that could be written to disk and
compiled by another process (§11), and it is why a diagnostic raised this late
has a span and a name rather than a def to look up.

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

**Mutability is gone.** `*T` and `*mut T`, `[]T` and `[]mut T`, `&x` and `&mut x`
are each one thing here. No target distinguishes them — LLVM, C and wasm each
have one kind of address — and the rule that needed the distinction, who may
write through this pointer, was enforced in sema long before now. Keeping it
would put two entries in the type table describing one machine type and would
hand a backend a field it has no use for. The **symbol** is the exception:
`mono::type_key` mangles mutability and monomorphization already decided every
name, so nothing here recomputes one from a stripped type.

**`void` is gone from every slot.** It is a type the *language* has — what a
function with no result returns, and what `Residual :: void` makes an `Option`'s
short-circuit carry — and not one a machine has. So a `void` parameter is not
passed, a `void` binding gets no slot, and a `void` argument is evaluated for its
effects and then dropped. Both sides of a call erase it by the same rule, so a
caller and a callee cannot come out with different arities. `never` goes the same
way and for the same reason: a slot typed "does not return" is a slot no register
file has, which is why a call is a statement with an *optional* destination
rather than an rvalue.

**Blobs are gone from operands.** A string's bytes, a byte string's, and an
aggregate the const evaluator folded are data rather than values: each becomes an
immutable global at lowering, and what an instruction carries is its address
(§2.5). Otherwise every backend would have to invent read-only data emission on
its own, from an operand, differently — and two programs holding the same bytes
would emit two copies.

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

**Three statements**, and one of them is a call:

| Statement | LLVM | C | wasm |
|---|---|---|---|
| `Assign { place, rvalue }` | the rvalue, then `store` (or an SSA def) | `p = e;` | the rvalue, then `local.set` / `store` |
| `Call { dest, callee, args }` | `call` / indirect `call` / an instruction | a call, or a builtin | `call` / `call_indirect` |
| `Drop(operand)` | a call to the runtime's free | a call | a call |

**Three callees**, which is where the intrinsic went: `Static(FuncId)` names a
function this unit holds, `Indirect(operand)` calls through a pointer, and
`Intrinsic(op)` is a machine operation named by the compiler rather than by the
linker. They are one statement because they differ in exactly one way — how the
code is reached — and an intrinsic is an **enum** rather than a name so that a
backend's match is exhaustive: an intrinsic added upstream is then a compile
error in every backend instead of a silent fall-through.

**Four terminators**: `goto`, `switch`, `return`, `unreachable`. `switch` covers
every branch there is, so there is no `br`/`switch` pair to keep in step, and it
carries the type its arms are compared at.

**Six rvalues**: `Use`, `Ref`, `Op`, `Cast`, `Aggregate`, `Offset`.

`Op { op, ty, args }` is every primitive operation there is — unary, binary and
checked alike — because the arity is the opcode's business and a backend that
matches one enum once cannot forget a case. The `ty` is what the operation runs
*at*: `lt.u64` and `lt.i64` are different instructions on every target, and a
constant operand carries no type of its own. The opcodes:

| Group | Opcodes |
|---|---|
| arithmetic | `add` `sub` `mul` `div` `rem` `neg` — **wrapping** on integer overflow, IEEE on floats |
| checked (§7d) | `add_checked` `sub_checked` `mul_checked` — result `(T, bool)` |
| bitwise | `bit_and` `bit_or` `bit_xor` `bit_not` `shl` `shr` |
| logical | `not` |
| comparison | `eq` `ne` `lt` `le` `gt` `ge` — result `bool` |

Overflow is two opcodes rather than one and a flag, deliberately: a flag that
changes the *result type* is not a flag. And the plain one **wraps**, by
definition — `overflow=wrap`, `#unsafe` and `wrapping_add` all emit it, because
all three mean the same instruction and an opcode whose meaning depended on a
build setting a backend cannot see would be the one thing this level exists to
prevent. An operation's operands are **scalars** —
nothing structural ever reaches one, which is why text equality is a call to
`core` (§6.13) rather than an `==` on a `{ ptr, len }`.

`Cast` **names its instruction**, and carries both types beside it. The name is
the point: there are many conversions between two numbers, and which one applies
is a rule about the operands' signedness rather than something the destination
type can answer. So the rule runs once, here, and the instruction is written
down:

| Kind | From → to | What it does |
|---|---|---|
| `trunc` | integer → narrower integer | keep the low bits |
| `zext` / `sext` | integer → wider integer | fill with zeroes / with the sign bit, by the **source's** signedness |
| `fptrunc` / `fpext` | float → narrower / wider float | round to nearest / exact |
| `fptosi` / `fptoui` | float → integer | round toward zero, by the **destination's** signedness |
| `sitofp` / `uitofp` | integer → float | round to nearest, by the **source's** signedness |
| `reinterpret` | same width, different type | nothing — a register is a register |
| `ptrtoint` / `inttoptr` / `ptrcast` | addresses | nothing, on every target this reaches |

The two signedness rows read opposite sides on purpose, and that is exactly the
corner a backend deriving this for itself gets wrong. `reinterpret` is a case
rather than an absence for the same reason `Intrinsic::Unknown` is a case: a
backend should handle it deliberately, not by falling through to one that shifts
bits. And `CastKind::Unknown` is what a pair with no instruction behind it
becomes — a test failure here rather than a guess there.

`Aggregate` builds a value of a struct type, an array, or one variant of an enum;
the four names it used to have for "build a struct" were four names for one
operation, and the type says which struct. `Offset` is a GEP in elements with the
stride in bytes beside it.

**Eleven intrinsics** reach a backend, and each is one instruction or one
runtime call: `new`, `make`, `trap`, `assert`, `transmute`, `repeat`, `format`,
`embed_file`, `gc_collect`, `gc_keep_alive`, `gc_pin`. Everything else a
`#intrinsic` declares is *gone* by this point — `size_of`, `align_of` and `cast`
are constants, `index` and `len` are projections, `wrapping_add` and
`wrapping_sub` are opcodes, `drop` is a statement. A test asserts that mapping is
total, so a row added to `sema::intrinsics` with no case here fails the build
rather than arriving at a backend as a name.

**`slice` and `array` were on that list and are not any more**, and why they left
is the rule the list has to keep earning. `$slice` took a `Range` — the
`#lang("range")` **enum**, six variants — so a backend would have had to switch
on a tag and compute a start and an end. That is not one instruction, and it is
not information a run-time value ever had to carry: the parser builds a slice
node only when the index is *syntactically* a range, so `sema::lower` now emits
the two bounds directly (both present, both exclusive: a missing start is `0`, a
missing end is `$len`, and `..=b` is `b + 1`). With plain numbers arriving,
`lir::lower` finishes the job — the pointer is an `Offset` and the length is a
`sub`, which is an `Aggregate` over two operands and no intrinsic at all.

`$array` on a slice type went the same way: its elements need storage, so it is a
`make` and a store per element. **An intrinsic that needs a branch is not an
intrinsic**, and one built out of instructions a backend already has belongs in
the lowering, where every backend gets it once. `repeat` and `format` are the two
that still owe this treatment.

A test asserts the *absence*: `slice`, `array` and `index_mut` must have no
`lir::Intrinsic` case, so emitting one again makes it an `Unknown` and fails
`no_program_contains_an_unknown_intrinsic` rather than reaching a backend.

`transmute` is the only one whose result type is read off `dest` rather than
carried in the operation, because the operation *is* "reinterpret as whatever
this slot holds".

### The four things a backend genuinely does itself

1. **Emit the data section.** Every global in a unit has a type, a mutability and
   an initializer written out member by member — a scalar, an address of a
   function, an address of another global, an aggregate of those, or bytes. What
   a backend does is lay those out by the same `TypeDef` a local of that type
   uses and write them into a section. It is emission, not analysis: the values
   are fully known, and *which* constants need storage was decided here (§9), so
   two backends cannot disagree about it.
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

### There is nothing LIR does not carry

A unit answers every question it raises. A type is a `TypeId` into its own table,
a callee is a `FuncId` into its own list, a global is a `GlobalId` into its own
globals, and a symbol is what ties one unit's declaration to another's definition
(§11). No `DefId` survives lowering, so nothing here needs the compiler's tables
to be read — a unit could be serialized and handed to another process, which is
what makes parallel code generation a scheduling question rather than a design
one.

### Known over-approximations, named rather than hidden

- **A `*T` is a GC root whatever `T` is** (§6), with two exceptions LIR can now
  name: a pointer to a vtable and a function pointer are pointers to *code*, and
  code is not in the heap. Every other machine address — a `*u8` into a buffer, a
  pointer a `transmute` produced — is still traced, and narrowing that wants a
  distinction between a managed reference and a machine address that the type
  system does not draw.
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

**One lowering, then a split.** `link` merges the per-file IR into a single
`Linked`, monomorphization runs over that, and this pass lowers every function
that survives — the entry file's, `core`'s, and every instantiation neither file
wrote — into one whole-program unit. §11 then cuts that into codegen units. The
snapshots render the entry file's unit, because a test about `while` should not
be a record of the standard library: a dump showing `call core.panic` and a
`declare func core.panic` beside it is the split doing its job, and the function
itself is in `core`'s unit.

**Vtables.** One snapshot is about the dispatch — two projections and an
indirect call — and one about the **data**: a trait with three methods so the
slot order is visible, two impls so there are two constants, an impl for
`Box.<i32>` so the vtable is for the instantiation rather than the generic, and
one type coerced twice, which shares its constant instead of emitting a second.

**Invariants, over any lowering.** A snapshot catches a change; it cannot say
what *any* program may produce. The invariant tests say that: every place and
every live local names a slot that exists, block ids are dense and 0 is the
entry, every index a unit holds resolves inside that unit, no operation has an
aggregate operand or the wrong arity, no operand carries a blob, no local is
typed `void` or `never`, no place indexes a slice, every named type is in the
table, every declared intrinsic has a case here, and every symbol is defined in
exactly one unit at every split. They run over `core` too — it is code a backend
has to emit, and a shape it alone produces is exactly the one nothing else would
catch.

**And over every example.** `every_example_lowers_to_well_formed_units` lowers
each file in `examples/` at four settings of `-C codegen-units` and runs all of
the above over every unit that comes out, plus one more: the dump must not
contain `<unknown …>`, because rendering resolves every index the structures
hold. The failures worth catching are the ones a hand-written test program does
not contain — a type only `core`'s `Result` reaches, a global only one unit
defines, an intrinsic only one example uses.

## 11. Codegen units

The lowering produces **one** whole-program unit; the last thing it does is cut
that into `-C codegen-units=N` of them (default **1**, which is the whole program
and is what a dump reads best). The cut is a **filter**, not a redesign, and §8
is what makes it one: a unit refers to a type by an index into its own table, to
a function by an index into its own list, and to a global the same way, so a unit
is built by walking what its functions reach, copying that, and renumbering.

**The partition is by source file, then merged.** A file is what a person writes
and what a person recompiles, so it is the partition that makes an incremental
build rebuild what changed; `-C codegen-units=N` then merges the smallest units
together until there are no more than `N`. That is the shape rustc uses (one unit
per module, merged down to the requested count) and for the same two reasons: the
merge is what bounds the count, and merging the *smallest* is what keeps the
units within reach of each other in size, which is what decides how long the
slowest thread takes.

**What a unit carries that it does not define** is a declaration: a [`Function`]
with no blocks for every function it calls, and a `Global` with `external` set
for every global it reads. That is the whole of what linking needs from this
side — a name, a signature and a symbol — and it is why the symbol is printed
beside every name in a dump. A definition appears in exactly one unit; a test
says so at five different settings.

```
unit main {
  type Point = struct { x: i32, y: i32 }
  func main() -> void @public  // _NC4main
    …
    _1 := call scale(_0, 3)
  declare func scale(p_0: Point, k_1: i32) -> Point @public  // _NC5scale
}

unit shapes {
  type Point = struct { x: i32, y: i32 }
  func scale(p_0: Point, k_1: i32) -> Point @public  // _NC5scale
    …
}
```

`Point` is in **both** units, because a unit that cannot describe its own
arguments is not self-contained. The two copies are the same type and say so: a
`TypeDef` carries the mangled key it was interned under, which is the one piece
of identity that survives the renumbering and is what a backend merging debug
info across units needs.

**What this buys, and what it costs.** It buys parallel code generation — the
units are independent values, so compiling them is a scheduling question rather
than a design one — and it costs a declaration in one unit for every definition
in another, plus a copy of each type more than one unit names. It also gives up
the whole-program view an optimizer would want, which is exactly why the default
is 1 and the number is the build's to choose.
