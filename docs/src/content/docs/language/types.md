---
title: Types
description: Primitives, integers of any width, pointers, arrays, slices, tuples, and Option.
---

A type is a compile-time value: anywhere one is expected, any `::`-bound name
whose value is a type may be used, as may the type-forming syntax on this
page.

## Primitives

```nest
Signed integers:   i8  i16  i32  i64   iN    (arbitrary width N in 2..=65535)
Unsigned integers: u8  u16  u32  u64   uN    (arbitrary width N in 1..=65535; u1 is bool)
Pointer-sized:     isize usize
Floating point:    f16  f32  f64  f128  (exactly these widths)
Boolean:           bool   (an alias for u1)
Text:              char   (Unicode scalar, 32-bit)   str   (UTF-8, borrowed)
Unit:              void   (the empty tuple; a function with no `-> T` returns void)
Uninhabited:       never  (the type of an expression that does not return)
Sizeless:          opaque (a pointee this program does not describe)
```

### Integers of any width

Integers are two generic families, and the familiar names are sugar for them:

```nest
int.<N>     // N: u16 — a signed integer N bits wide
uint.<N>    // N: u16 — an unsigned one

i32 == int.<32>
u8  == uint.<8>
```

Any width from the family's minimum up to `65535` is legal — `u7`, `i24`,
`u4096` all name real types. `i1` isn't a type; `u1` is spelled `bool`, the
same type rather than a second one-bit integer. Signedness isn't a parameter
— `int` and `uint` are two separate families, precisely so nothing is ever
generic over signedness.

`i32` and `int.<32>` are **the same type**, not two that convert; the short
spelling is the ordinary one, and the generic form shows up where a width has
to be spoken about — an `impl` header, a bound, a reflection query.

#### `MIN` and `MAX`

Every width has extremes, `Self.MIN` and `Self.MAX` — ordinary associated
constants on the family `impl`s, not compiler-known values:

```nest
impl <const N: u16> int.<N> {
  MAX :: cast.<Self>(~cast.<uint.<N>>(0) / 2)
  MIN :: cast.<Self>(~cast.<uint.<N>>(0) / 2 + 1)
}

impl <const N: u16> uint.<N> {
  MIN :: cast.<Self>(0)
  MAX :: cast.<Self>(~cast.<Self>(0))
}
```

Both are computed in the **unsigned** family of the same width, the one
place every intermediate value fits. They aren't written as `2^(N-1)`
because a shift's operands are linked to one width, and `1 << N` would have
no type wide enough to hold the result without overflowing on the way.

The families are also why the operations on integers are written **once**:
`wrapping_add` isn't compiler syntax, it's an inherent method in `core` on an
`impl` over a whole family — per-width impls couldn't do this, since `u4096`
is legal and there's no finite list to write out.

### Widening

A narrower integer may stand where a wider one is wanted, since no value is
lost; the reverse needs an explicit `cast`.

## Pointers — `*T`, `*mut T`, `&x`

Pointers are Go-style: explicit in type and at the point of taking an
address, automatically dereferenced on use, garbage-collected, and **without
pointer arithmetic**. They're immutable by default; write access is opt-in.

```nest
*T          // read-only pointer to a T (cannot write through it)
*mut T      // pointer through which the pointee may be mutated
&expr       // address-of: yields *T, or *mut T from a mutable place
```

- **Auto-deref.** Field access, method calls, and indexing through a pointer
  need no explicit deref: given `self: *CatImage`, `self.url` reads the
  field. The postfix `p.*` yields the whole pointee when needed (Zig-style —
  there is no prefix `*p`).
- **Never null.** A `*T`/`*mut T` always points at a live value; there's no
  null pointer and no null literal. Absence is `Option.<*T>` (`.none`). A
  genuinely nullable raw pointer only exists for C interop, as `c.ptr.<T>`
  (see [C FFI](../c-ffi/)).
- **GC-managed.** Taking `&x` is always safe; the collector keeps the
  pointee alive while the pointer is reachable. No lifetime annotations, no
  manual free (though see [Memory](../memory/) for `drop`).
- **No arithmetic.** `p + 1` is a type error — iterate slices for sequential
  access.

## Arrays and slices

```nest
[N]T        // fixed-length array of N elements (N a compile-time constant)
[]T         // read-only slice: a (ptr, len) view over a contiguous run of T
[]mut T     // mutable slice: elements may be assigned through it
[N]mut T    // fixed array whose elements may be mutated through this reference
```

`N` is any compile-time constant expression — a literal, a named constant, a
`const` generic parameter, or arithmetic over those. Slices, like pointers,
are immutable by default: `s[i] = x` is legal only when `s: []mut T`.
Indexing is `s[i]`, length is `s.len()`, sub-slicing is `s[lo..<hi]`.

A sub-slice **keeps the permission of what it was cut from**: `s[lo..<hi]`
on a `[]mut T` is itself a `[]mut T`. Cutting a sub-slice of an *array* is
the one exception, and yields a read-only `[]T` — permission over an array's
elements belongs to whoever holds the array, since a `[N]T` carries no
mutability of its own.

A fixed array `[N]T` implicitly coerces to a read-only `[]T` wherever a slice
is expected (never to `[]mut T`) — the coercion *is* taking the whole
sub-slice. The length is part of the type: `[3]i32` and `[4]i32` don't
convert. Out-of-bounds indexing traps at run time, and is a **compile error**
when both the array's length and the index are compile-time known.

```nest
const xs: [3]i32 := .{ 1, 2, 3 }
const view: []i32 := xs             // coerces
```

## Tuples

```nest
(A, B, C)          // tuple type
(a, b, c)          // tuple value
t.0  t.1  t.2       // positional access
```

`void` is the zero-element tuple `()`.

## `Option`

Absence is modeled by the prelude enum `Option` — there is no `?T` sugar and
no `nil`/`null`:

```nest
Option :: enum <T> {
  some(T),
  none,
}
```

`Option.<*CatImage>` is an optional pointer; a bare `T` coerces to
`.some(value)` in an `Option` context. It's consumed by `match`, by methods
like `unwrap_or(default)`, or by the `Try` operators `.?`/`.!` (see
[Errors](../errors/)).

## Generic type application — `.<...>`

A generic type or function is instantiated with the `.<...>` turbofish:

```nest
Result.<T, models.FetchError>
client.get.<[]CatImage>(url)
cast.<HttpPort>(8080)
```

Type arguments are usually inferred from context, so `.<...>` is often
unnecessary. The placeholder `_` requests inference of one position while
fixing others: `make.<[]_>(1024)`, `collect.<_, str>(iter)`.

A turbofish argument may also be an **associated-type equality**, `name =
type`, pinning an associated type of the instantiated trait rather than
supplying a positional parameter — chiefly used to write bounds and `dyn`
types:

```nest
Iterator.<Item = i32>       // the Iterator trait with Item fixed to i32
dyn Iterator.<Item = i32>   // the trait object, same pin
```

## Type identity

- **Named types are nominal.** Identity is the declaration, not the
  structure — two named structs with identical fields are different types,
  as if each were `distinct`.
- **Anonymous types are structural.** `*T`, `[]T`, `[N]T`, `(A, B)`,
  `func(...)`, `Option.<T>`, `dyn Trait`, and an inline `struct { ... }`
  with no name are equal when their components match.
- **No implicit conversion across nominal boundaries** — numeric types,
  `distinct T` and its underlying `T`, and named structs never implicitly
  convert. Everything crosses through an explicit `cast`, except two
  coercions: an `@using` field, and an anonymous struct value coercing to a
  matching named struct.

## Dynamic arrays — `Vector`

There's no built-in growable array; it's the std struct `Vector.<T>`:

```nest
let xs := Vector.<i32>.new()
xs.push(1)
xs.push(2)
const view: []i32 := xs.slice()   // borrow as a read-only slice
```

`HashMap.<K, V>` and other containers follow the same `.new()` pattern.
