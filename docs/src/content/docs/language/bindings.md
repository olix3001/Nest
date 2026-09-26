---
title: Bindings
description: "::, let, const — the three ways to name a value in Nest."
---

Everything a program names is introduced by one of three forms. The choice
between them comes down to one question: **is the bound value known at
compile time?**

| Form | Binds | Mutable? | Known at |
|------|-------|----------|----------|
| `name :: value` | constants, types, functions, traits, namespaces | no | compile time |
| `const name := value` | a local (or scoped) value | no | run time |
| `let name := value` | a local (or scoped) value | yes | run time |

## Compile-time bindings — `::`

`::` binds a name to something the compiler can evaluate at compile time. The
right-hand side may be a value, a type, a function, a trait, a namespace, or
an `import`:

```nest
SERVER_PORT :: 8080
CatId       :: distinct str
main        :: func () { }
ToJson      :: trait { render :: func (self: *Self) -> str }
config      :: namespace { }
io          :: import <std/io>
```

`::` bindings are always immutable and always compile-time-known. They can
appear at the top of a file, inside a namespace, or inside a function body (a
local type or local constant), and at namespace scope they're
**order-independent** — mutually recursive types and functions are fine.

Inside a function body, `::` declares a **local item** — a constant, type,
trait or function — and an `impl` may sit among the statements too. A local
item belongs to its block: it's visible anywhere in that block and nowhere
outside it. It is a definition, not a closure, so it can't use the enclosing
function's locals, parameters or generics.

```nest
main :: func () -> i32 {
    Point :: struct { x: i32, y: i32 }
    impl Point { sum :: func (self: *Self) -> i32 { self.x + self.y } }
    twice :: func (n: i32) -> i32 { n * 2 }
    const p := Point { x: 1, y: 2 }
    twice(p.sum())
}
```

### A constant's type

`SERVER_PORT :: 8080` has no single runtime type: a numeric literal is a
`comptime_int` (see [Types](../types/)), and a constant bound to one
stays untyped — each use site settles it for itself, which is what lets one
`MAX` be an `i8` in one place and an `i64` in another.

To **pin** a constant to one type, write the type *before* the `::`:

```nest
MAX      :: 100          // comptime_int; settles per use site
MAX_BYTE: u8 :: 100      // a u8 everywhere, and only a u8
```

| Form | Meaning |
|------|---------|
| `name :: value` | a constant; a literal keeps its comptime-ness |
| `name: T :: value` | a constant pinned to `T` |
| `name :: T` | a **type alias** — always |

Putting the type first is what keeps the last row unconditional: written
after the `::` it would have to be told from an alias by whether something
followed it.

A pinned constant is range-checked against `T` with the literal's exact value
in hand: `MAX_BYTE: u8 :: 300` is rejected rather than wrapped.

### Evaluation

A `::` binding *is* its value — there's no run-time moment it could be
computed at. Its right-hand side may therefore only name other constants,
`const` generic parameters, and calls to `#const` functions:

```nest
next_pow2 :: #const func (n: u32) -> u32 { ... }
CAPACITY: u32 :: next_pow2(1000)
```

Integer arithmetic in a constant expression is **exact**, and where the
result has a width it must fit: `P: u8 :: 200 * 2` is an error (`400` doesn't
fit `u8`), while `BIG :: 200 * 2` is fine (`400`, untyped). Reach for
`cast.<u8>(200 * 2)` to take the low bits on purpose.

The left-hand side of `::` is a **pattern**, which is why import
destructuring works:

```nest
{ CatImage, CatId, HttpPort } :: import "models.nest"
{ http: { Client, Router } }  :: import "network.nest"
io                             :: import <std/io>
```

## Runtime bindings — `let` and `const`

Inside function bodies, values computed at run time are bound with `:=`,
prefixed by `const` (immutable) or `let` (mutable):

```nest
const router := http.Router { logging: true }   // immutable
let   count  := 0                                 // mutable
count = count + 1
```

`const … :=` and `::` are both immutable; they differ in *when* the value
exists. Use `::` for compile-time constants and type/function definitions;
`const … :=` for run-time values you won't reassign.

An optional `: Type` between the name and `:=` fixes the type and provides
the **contextual type** used by literal defaulting, inferred enum variants,
and inferred composite literals:

```nest
const port: HttpPort := config.SERVER_PORT
const cats: []CatImage := fetch_all()
```

Every `let`/`const` must be initialized at the point of declaration — there's
no uninitialized-then-assigned form. Use `Option.<T>` set to `.none` if a
value genuinely arrives later.

## Assignment

After a `let` binding, plain assignment reassigns it:

```nest
place = expr
place += expr   // and -=, *=, /=, %=
```

`place` is an assignable location: a `let` variable, a field of one, an
element of a `[]mut`/array, or a `.*` through a `*mut` pointer. `const` and
`::` bindings are not assignable, nor are places reached through a read-only
`*T` / `[]T`. Binding mutability (`let` vs `const`) and reference mutability
(`*mut` / `[]mut`) are independent: `const p := &mut x` is an immutable
binding holding a mutable pointer, so `p.* = 1` is legal but `p = &mut y` is
not.

## `distinct` types

`distinct T` creates a new **nominal** type with `T`'s representation but no
implicit interchange with `T` or any other `distinct T`:

```nest
CatId    :: distinct str
HttpPort :: distinct u16
```

Conversion either way is explicit, through `cast`:

```nest
const raw_id: str  := cast(self.id)        // CatId -> str
const id: CatId    := cast.<CatId>("abc")  // str -> CatId
```

A `distinct T` **inherits `T`'s methods** (`T` does not gain the distinct
type's), with `Self` rebound to the distinct type — so an inherited method
that returned `Self` now returns the distinct type, not `T`:

```nest
Meters :: distinct f64

a + b   // Meters, not f64 — Add's Output is Self.Output, and Self is Meters
```

A literal settles directly on a distinct numeric, the same way it settles on
a plain one, so `const p: HttpPort := 80` needs no `cast`. To inherit
*nothing*, use a tuple struct instead: `Opaque :: struct (f64)` is a new type
with a field, not a new name for an old one.

## Shadowing and scope

Bindings are lexically scoped to the enclosing block, namespace, or file. An
inner binding may **shadow** an outer one of the same name (including
shadowing a `let` with a `const`, or vice versa). `let`/`const` bindings
inside a function are order-dependent and must be declared before use.

## Static mutable storage — `#static`

`let`/`const` only appear inside function bodies. A program-lifetime mutable
region is declared with `#static` on a `::` binding:

```nest
#static request_count: usize :: 0
#static scratch: [4096]u8          // no initializer -> zeroed
```

The type is always written first (a region is storage, and storage has a
width), the initializer must be a constant expression, and it may be omitted
(the region is zeroed). The same form inside a function gives that local a
single program-lifetime region that persists across calls, C `static`-local
style:

```nest
tick :: func () {
    #static calls: usize :: 0
    calls = calls + 1
}
```

A `#static` is shared global state with no synchronization performed by the
language — concurrent access is a data race unless mediated by std atomics or
locks — and reading one is a run-time operation, so it may never appear in a
constant expression.
