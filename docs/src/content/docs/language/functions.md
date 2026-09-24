---
title: Functions
description: Declarations, defaults, named arguments, overload sets, generics, bounds, and impl types.
---

A function is a `::` binding whose value is a `func` literal:

```nest
main :: func () {
    ...
}

@public
render :: #inline func (self: *CatImage) -> str {
    ...
}
```

The return type follows `->`; omitted, it's `void`. The body's last
expression is **not** implicitly returned — use `return` (see
[Errors](/language/errors/) for how it interacts with `defer`).

## Parameters

```nest
new    :: func (id: CatId, url: str, w: i32, h: i32) -> CatImage { ... }
listen :: func (self: *Router, host: str, port: HttpPort) { ... }
```

Parameters are immutable bindings inside the body — rebind locally with
`let` for a mutable copy. A parameter's *type* still carries its own
mutability: `s: []mut i32` is an immutable binding to a mutable slice, so
`s[i] = x` is allowed but `s = other` is not.

### Default values

```nest
pad :: func (s: str, width: usize := 8, fill: char := ' ') -> str { ... }

pad("hi")               // width = 8,  fill = ' '
pad("hi", 4)             // width = 4,  fill = ' '
pad("hi", fill: '-')     // width = 8,  fill = '-'
```

Defaulted parameters trail the required ones. The default is type-checked
once, where it's written, not at each call site — but it's *evaluated* per
call, so `.{}` as a default builds a fresh value each time. It must be
compile-time known at the call site: a literal, a path to a `const` item or
`const` generic parameter, a constant composite literal, a `cast` of a
constant, `#caller_location`, or a call to a `#const` function.

`#caller_location` is the one expression legal only in a default — the
position of the *call site*, which is what lets `panic` name the line that
called it:

```nest
{ Location } :: import <core/loc>

report :: func (msg: str, loc: Location := #caller_location) {
    // loc.file / loc.line / loc.column name the caller
}
```

### The receiver — `self`

A parameter named `self` marks the function as a method of the type its
`impl` namespace targets: `*T` (read-only), `*mut T` (mutating), or `T`
(by-value; bare `self` means `self: Self`).

```nest
impl Router {
    @public
    get :: func (self: *mut Router, path: str, handler: impl Func() -> Response) { ... }
}
```

`router.get("/cat", handler)` auto-takes `&router`/`&mut router` as the
receiver type requires. A function in an `impl` namespace *without* `self`
is an **associated function**, called on the type: `CatImage.new(...)`.

## Calls and named arguments

```nest
router.listen(host: config.SERVER_ADDR, port: port)
```

Arguments are positional by default; any may be passed by name. Named and
positional may be mixed, but once a named argument appears the rest of the
call must be named too. Naming an argument is what makes a default in the
*middle* of the trailing defaults reachable — `pad("hi", fill: '-')` skips
`width`, which no positional call could do.

### Trailing block sugar

When the **last** parameter takes a closure, its argument may follow the
call's `)` as a block:

```nest
router.get("/cat") {
    const cats := client.get.<[]CatImage>(url).!
    return ...
}

list.reduce(0) { acc, x in acc + x }
spawn() { work() }
```

The parentheses are always written, even as the sole argument — after a
bare name, `{` opens a composite literal, and the two can't be told apart
without them. A trailing block is never taken in the head of `if`, `while`,
`for`, or `match`. It desugars to exactly the closure passed as the final
argument.

## Overload sets

`func { a, b, m.c }` names a set of several functions as one name — the `{`
right after `func` (no parameter list) tells it apart from a function
literal:

```nest
process :: func { handle_int, handle_str, m.handle_other }
```

## Generics

```nest
sum     :: func <I: Iterator.<Item = i32> + Clone> (it: I) -> i32 { ... }
zeros   :: func <const N: usize> () -> [N]u8 { ... }
```

- `T` — any type, unconstrained. No kind annotation is needed for a type
  parameter; `const` is what marks a *value* parameter, so there's no `T:
  type` form.
- `T: SomeTrait` — constrains `T` to implementors of `SomeTrait`, enabling
  that trait's methods in the body. Multiple bounds: `T: TraitA + TraitB`.
- `const N: Ty` — a compile-time value parameter, usable in the body and in
  types such as `[N]u8`. `Ty` may be any primitive type; an array length is
  specifically a `usize`, and an integer width (`int.<N>`) specifically a
  `u16` — a parameter fills either slot on the *widening* rule, not on
  equality.

A bound may carry an **associated-type equality** — `Iterator.<Item =
i32>` — inside the turbofish, pinning that associated type on the bound.
Positional and `name = type` arguments mix freely: `Map.<K, Value = V>`.

A call is held to its callee's bounds where it's written: passing a type
that doesn't implement one is an error at the call site.

### `.<...>` and inference

Type arguments are usually inferred and the turbofish omitted. When
explicit, `_` leaves individual positions to inference:

```nest
Vector.<i32>.new()             // T explicit
Vector.new()                    // T inferred from later use
client.get.<[]CatImage>(url)    // explicit
make.<[]_>(1024)                // element type inferred
collect.<_, str>(iter)          // first inferred, second fixed
```

`.<...>` (not bare `<...>`) is mandatory for supplying any argument; bare
`<...>` is never a valid argument list — the only place `<` `>` appears is a
generic-parameter *declaration*.

## `impl` — anonymous generics and opaque returns

A parameter whose type is `impl Bound` is an anonymous generic parameter;
these two declarations are the same function, and a call can't name the
anonymous one in a turbofish:

```nest
apply :: func (f: impl Func(i32) -> i32, x: i32) -> i32 { return f(x) }
apply :: func <F: Func(i32) -> i32> (f: F, x: i32) -> i32 { return f(x) }
```

As a **return type**, `impl Bound` is the other way round: the body decides
the concrete type, and a caller knows it only by its bounds — it can do
with the value what the bounds allow, and nothing more.

```nest
make_adder :: func (n: i32) -> impl Func(i32) -> i32 {
    return { x in x + n }
}
const add5 := make_adder(5)
add5(1)   // 6
```

Only the function's own type parameters may appear in the returned type —
not an enclosing `impl <T>`'s.

## `#const` functions

`#const func` restricts the body to a compile-time-evaluable subset: no
run-time I/O, no run-time allocation, and only calls to other `#const`
functions and intrinsics. Such a function may run at compile time *or* at
run time, which is what lets a call sit on the right of `::`:

```nest
#const
to_port :: func (n: u16) -> HttpPort {
    assert(n > 0, "port must be non-zero")
    return cast.<HttpPort>(n)
}

SERVER_PORT :: to_port(8080)   // evaluated at compile time
```

## Entry point

`main :: func ()` at file scope, outside `core`, is the program entry
point. It takes no parameters — read the command line through `core`
instead. Its return type must be `void`, an integer status, or `never`; a
`Result` is **not** among them, so a fallible `main` reports its own error
and returns a status directly rather than propagating one. A compilation
may define at most one `main`; a library, having none, is simply not asked
to.
