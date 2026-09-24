---
title: Closures and Func
description: Closures, captures, the Func trait, *func pointers, and *dyn Func.
---

A closure is a function written where a value goes, which may name the
locals around it:

```nest
const double := { x in x * 2 }
const add    := { a: i32, b: i32 -> i32 in a + b }
const now    := { in clock.now() }
const scaled := { [n] x in x * n }
```

A parameter's type and the result type may be left out and inferred — from
the parameter the closure is passed to, when there is one, otherwise from
how the closure is used. `in` ends the header; a closure taking nothing is
`{ in body }`. A `{` whose first tokens aren't a header (`in`, a `[...]`
capture list, or `name` followed by `,`/`:`/`->`/`in`) is an ordinary block.

`func (x: i32) -> i32 { return x * 2 }`, written where a value goes, is a
closure too — spelled with its types written out. Only a `::` binding makes
a `func` literal a *definition*: a `::`-bound function written inside a body
never captures, and naming an enclosing local from inside one is an error.

## Captures

A closure **shares** every local it names from outside: it reads what the
local holds when called, and a write through either side is seen by both.

```nest
let mut count := 0
list.iter().each() { x in count += x }   // count is the sum afterwards
```

A shared local lives as long as the closure does, not as long as the frame
that declared it — a closure made on one pass of a loop keeps *that* pass's
`let`, not the next one's.

A name in the **capture list**, `[n]`, is copied instead, at the moment the
closure is made. The copy is read-only:

```nest
let mut n := 1
const f := { [n] x in x + n }
n = 10
f(1)   // 2 — f's copy of n was taken before the reassignment
```

## The `Func` trait

Every closure has a type of its own that no program names. What it and a
function pointer (`*func(...)`) have in common is the prelude trait `Func`:

```nest
Func :: #lang("func") trait {
    Args :: type
    Output :: type
}
```

`Func(A, B) -> R` is sugar for `Func.<Args = (A, B), Output = R>` — the
arguments as one tuple, `()` for none, a missing `-> R` meaning `-> void`.
`Func` declares no method: nothing implements it but the compiler, and
calling a value whose type implements it is a direct call to whatever code
that instantiation reaches.

```nest
apply :: func (f: impl Func(i32) -> i32, x: i32) -> i32 { return f(x) }

apply(double, 3)          // a *func
apply({ x in x + n }, 3)  // a closure
```

A closure is an ordinary value — store it, pass it, call it later. Taking a
closure as a parameter (`impl Func(...)`) makes a fresh instantiation per
closure passed, so the call is direct with no indirection.

## `*func` and `*extern("c") func`

A named function used as a value is a **function pointer**, one word:

```nest
*func(A) -> R                // a Nest-ABI function pointer
*extern("c") func(A) -> R    // a C-callback function pointer
```

These are two distinct types that never convert to each other — the two
calling conventions pass an aggregate differently. A bare `func(...)` type
(with no leading `*`) is a parse error; a function pointer is only ever
spoken about behind a pointer.

## `*dyn Func` — closures behind a pointer

To keep closures of *different* types together, put each on the heap and
hold it as a trait object. `*dyn Func(i32) -> i32` is, like every trait
object, a data pointer plus a vtable — the vtable's one entry is the
closure's body. `core/mem`'s `boxed(value)` puts a value whose type has no
name onto the heap:

```nest
{ boxed } :: import <core/mem>

const a: *dyn Func(i32) -> i32 := boxed({ x in x + n })
const b: *dyn Func(i32) -> i32 := boxed({ x in x * 3 })
const fs: [2]*dyn Func(i32) -> i32 := .{ a, b }
fs[1](2)   // 6
```

Only a closure becomes a `*dyn Func` — a `*func` is already one word and is
passed as-is; coercing one to `*dyn Func` is refused (there's no data half
for the vtable's `self` to point at), so wrap it in a closure first:
`boxed({ x in raw_fn(x) })`.

## Full example

```nest
{ boxed } :: import <core/mem>

// A parameter written `impl Func(...)` takes a closure or a function
// pointer, and each closure passed makes its own instantiation.
apply :: func (f: impl Func(i32) -> i32, x: i32) -> i32 {
    return f(x)
}

// A trailing block is the last argument.
repeat :: func (n: i32, body: impl Func(i32)) {
    let mut i := 0
    while i < n {
        body(i)
        i = i + 1
    }
}

// A closure returned as `impl Func`: the caller knows it by its bound.
make_adder :: func (n: i32) -> impl Func(i32) -> i32 {
    return { x in x + n }
}

// A shared local outlives the call that bound it when the closure does.
counter :: func () -> impl Func() -> i32 {
    let mut count := 0
    return { in
        count = count + 1
        count
    }
}

double :: func (x: i32) -> i32 { return x * 2 }

main :: func () -> i32 {
    const square := { x in x * x }
    let a := apply(square, 4)   // 16
    let b := apply(double, 4)   // 8

    let mut total := 0
    repeat(5) { i in total = total + i }   // total is 10

    let mut n := 1
    const copied := { [n] x in x + n }   // copies n as it is now
    n = 100
    let c := copied(1)   // 2

    const tick := counter()
    tick()
    let d := tick()   // 2

    // Closures of different types, held as one trait object.
    const fs: [2]*dyn Func(i32) -> i32 := .{ boxed(make_adder(3)), boxed({ x in x - 1 }) }
    let e := fs[0](1) + fs[1](1)   // 4 + 0

    return a + b + total + c + d + e - 42   // 0
}
```

## Known limitations

- A closure may **read** its function's `<const N>` (it gets a copy), but
  its parameter and result types can't mention `N` — a closure's type is
  generic over type parameters only.
- No `FnMut`/`FnOnce` split — shared captures are already GC cells, so
  `*Self` suffices for every call.
