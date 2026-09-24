# 05 — Functions and Generics

## 5.1 Function declarations

A function is a `::` binding whose value is a `func` literal. Directives that
modify the function (`#inline`, `#const`, …) sit immediately before the `func`
keyword; attributes (`@public`) precede the whole declaration:

```
func_decl = [ attribute ]* identifier '::' [ directive ]* 'func'
            [ generics ] '(' [ params ] ')' [ '->' type ] block
```

```
main :: func () {
  ...
}

@public
render :: #inline func (self: *CatImage) -> str {
  ...
}
```

- The return type follows `->`. When omitted, the function returns `void`.
- The body is a brace-delimited block. The value of the last expression is
  **not** implicitly returned; use `return` (see
  [08-error-handling-and-defer.md](08-error-handling-and-defer.md) for how
  `return` interacts with `defer`).

### `#const` functions

`#const func` restricts the body to a **compile-time-evaluable subset**: no
run-time I/O, no run-time allocation, and only calls to other `#const` functions
and intrinsics. Such a function may be evaluated at compile time (by the
comptime interpreter) *and* called normally at run time. There is no
whole-program value tracking — enforcement is purely "is every construct
const-safe?". This is what lets a call sit on the right-hand side of `::`:

```
#const
to_port :: func (n: uint16) -> HttpPort {
  assert(n > 0, "port must be non-zero")
  return cast.<HttpPort>(n)
}

SERVER_PORT :: to_port(8080)     // evaluated at compile time
```

## 5.2 Parameters

```
params = param { ',' param }
param  = 'self' [ ':' type ]
       | identifier ':' type [ ':=' expr ]
```

```
new    :: func (id: CatId, url: str, w: int32, h: int32) -> CatImage { ... }
listen :: func (self: *Router, host: str, port: HttpPort) { ... }
```

Parameters are immutable bindings inside the body (rebind locally with `let` if
you need a mutable copy). A parameter's *type* still carries its own mutability:
`s: []mut int` is an immutable binding to a mutable slice, so `s[i] = x` is
allowed but `s = other` is not.

### Default values (`:=`)

A parameter may carry a **default**, which makes it optional at the call site:

```
pad :: func (s: str, width: usize := 8, fill: char := ' ') -> str { ... }

pad("hi")                  // width = 8,  fill = ' '
pad("hi", 4)               // width = 4,  fill = ' '
pad("hi", fill: '-')       // width = 8,  fill = '-'
```

It is `:=` and not `=` because a default *introduces* what the binding holds,
which is the job `:=` already does for a local — every `=` in the grammar is
either assignment to an existing place or an associated-type constraint. It is
not `::` either: a `::` item is fixed once, whereas a default is evaluated **per
call**, so `.{}` as a default builds a fresh value at each site rather than
sharing one.

Rules:

- **Defaulted parameters trail the required ones.** Positional arguments bind
  left to right, so a hole in the middle could only ever be filled by naming the
  arguments after it.
- The default is type-checked **once**, against its own parameter, where it is
  written — not at each call site.
- A default must be **compile-time known at the call site** (§5.1): a literal, a
  path to a `const` item or `const` generic parameter, a composite literal whose
  elements are all constant, a `cast` of a constant, `#caller_location`, or a
  call to a `#const` function. Note that this is not the same as "a single fixed
  value" — `#caller_location` differs at every call site and is still
  admissible, because each site knows its own.

### `#caller_location`

`#caller_location` is the position of the **call site**, as a `Location`. It is
an expression, and the only place it is legal is a default argument:

```nest
{ Location } :: import <core/loc>

report :: func (msg: str, loc: Location := #caller_location) {
  // `loc.file`, `loc.line`, `loc.column` name the line that called `report`
}
```

That restriction is the feature, not a limitation of it. A default is filled in
at the call site, so each call supplies its own position — which is exactly what
makes `panic` name the line that raised it rather than the line inside `core`
that declares it. Written anywhere else it could only mean "the position of this
expression", which is a different thing, so it is refused.

`Location` is an ordinary struct in `core`, found by its `#lang("location")` tag
like everything else the compiler wires syntax to:

```nest
// core/loc.nest
@public(all)
Location :: #lang("location") struct { file: str, line: u32, column: u32 }
```

It is **not** in the prelude: a function only needs to name the type in order to
*declare* such a parameter, which is a deliberate act that can afford a line of
import (§4.6). Line and column are 1-based, and the column counts `char`s rather
than bytes.

It is an ordinary default in every other respect — writing the argument at a call
site overrides it, and a method may declare one.
- An omitted argument is filled in during lowering, so the compiled call is
  ordinary and positional; nothing after that stage knows a default was involved.

### The receiver (`self`)

A parameter named `self` marks the function as a **method** of the type its
`impl` namespace targets. Its type is `*T` (read-only receiver), `*mut T`
(mutating receiver), or `T` (by-value receiver). A bare `self` with no type is
`self: Self`, the by-value receiver; outside an `impl` or a `trait` there is no
`Self` for it to mean, and it is an error:

```
impl Router {
  @public
  get :: func (self: *mut Router, path: str, handler: impl Func() -> Response) { ... }
}
```

A method is invoked with dot syntax, `router.get("/cat", handler)`, binding the
receiver to `self`; the compiler auto-takes `&router` / `&mut router` as the
receiver type requires. A function in an `impl` namespace **without** `self` is
an **associated function**, called on the type: `CatImage.new(id, url, w, h)`.

A trait member may also be named through the **trait** rather than through a
value or a concrete type — `Make.make(3)`, `FromResidual.from_residual(r)`.
There is no receiver to dispatch on, so `Self` is whatever the surrounding
context requires, and the impl is selected once that is known:

```
Make :: trait { make :: func (n: int32) -> Self }
impl Make for Widget { make :: func (n: int32) -> Widget { ... } }

const w: Widget := Make.make(3)     // Self = Widget, from the annotation
func () -> Widget { return Make.make(3) }   // Self = Widget, from the return type
```

If nothing pins `Self` down, that is a "type annotations needed" error, exactly
as for an uninferred type parameter.

## 5.3 Calls and arguments

```
call = callee '(' [ args ] ')'
args = arg { ',' arg }
arg  = [ identifier ':' ] expr        // positional or named
```

Arguments are positional by default; any argument may be passed **by name**:

```
router.listen(host: config.SERVER_ADDR, port: port)
```

Named and positional may be mixed, but once a named argument appears the rest of
the call must also be named. Names must match parameters, each supplied at most
once, and every parameter without a default (§5.2) must be supplied. A call may
therefore pass anywhere from the required count up to the full parameter count.

Naming an argument is what makes a default in the *middle* of the trailing
defaults reachable: `pad("hi", fill: '-')` skips `width`, which no positional
call could do.

### Trailing block sugar

When the **last** parameter takes a closure, its argument may be written as a
closure after the call's `)`:

```
router.get("/cat") {
  const cats := client.get.<[]CatImage>(url).!
  return ...
}

list.reduce(0) { acc, x in acc + x }
spawn() { work() }
```

The parentheses are always written, even when the block is the only argument:
after a bare name, `{` opens a composite literal (`Point { x: 1 }`), and the
two cannot be told apart without them. A trailing block is never taken in the
head of an `if`, `while`, `for` or `match`, for the same reason a composite
literal is not.

A block with no header takes no parameters. Either way the trailing block is
exactly the closure (§5.5) passed as the final argument:

```
router.get("/cat", { in ... })
list.reduce(0, { acc, x in acc + x })
```

This is the idiomatic form for handler / callback APIs.

## 5.4 Generics

Functions and types may be parameterized. Generic parameters are declared in
`< >` after `func` (for functions) or after the type name (for types):

```
generics      = '<' generic_param { ',' generic_param } '>'
generic_param = identifier [ ':' constraint ]     // type param; bare `T` is unconstrained
              | 'const' identifier ':' type        // compile-time value param
constraint    = type { '+' type }                  // trait bounds; a bare param is already a type
```

A bound is any `type`, so a trait carrying an **associated-type equality** —
`Iterator.<Item = int32>` — is a legal bound. Inside the `.<...>` turbofish an
argument may be `name = type` in addition to a positional type or `_`, pinning
that associated type on the bound (see
[03-types.md](03-types.md) §3.7 and [13-grammar.md](13-grammar.md) §13.3):

```
sum :: func <I: Iterator.<Item = int32> + Clone> (it: I) -> int32 { ... }
collect :: func <I: Iterator, C: FromIterator.<Item = I.Item>> (it: I) -> C { ... }
```

A parameter whose type is written `impl Bound` is an anonymous generic
parameter with that bound, and the two spellings below declare the same
function. A call cannot name the anonymous one in a turbofish.

```
apply :: func (f: impl Func(i32) -> i32, x: i32) -> i32 { return f(x) }
apply :: func <F: Func(i32) -> i32> (f: F, x: i32) -> i32 { return f(x) }
```

A call is held to its callee's bounds where it is written: passing a type that
does not implement one is an error at the call.

As a **return type**, `impl Bound` is the other way round: the body decides
what the type is, and a caller knows it only by its bounds. The body is held to
them; a caller may do with the value what the bounds allow and nothing more —
it cannot, for instance, store it where the type the body happens to return is
wanted.

```
make_adder :: func (n: i32) -> impl Func(i32) -> i32 {
  return { x in x + n }
}
const add5 := make_adder(5)
add5(1)                                // 6
```

It is the function's own type parameters that the returned type may mention,
and a caller's instantiation says what they are. The type is known once the
body is typed, and the compiler uses it as it is: the value is returned
directly, with no indirection behind it.

`Item` must be an associated type declared by the named trait; the constraint
requires the implementor's choice for that associated type to equal the given
type. Positional type arguments and `name = type` bindings may be mixed in one
turbofish (`Map.<K, Value = V>`).

```
get :: func <T> (self: *Client, url: str) -> Result.<T, FetchError> {
  ...
}

zeros :: func <const N: usize> () -> [N]u8 { ... }      // value param used in a type
```

- `T` — any type, unconstrained (most permissive). A type parameter needs no kind
  annotation; the `const` keyword is the only thing that marks a *value* parameter,
  so there is no `T: type` form.
- `T: SomeTrait` — constrains `T` to implementors of `SomeTrait`, enabling that
  trait's methods in the body. Multiple bounds: `T: TraitA + TraitB`.
- `const N: Ty` — a **compile-time value** parameter: a constant of the concrete
  type `Ty`, usable in the body and in types such as `[N]u8`. The `const` keyword
  is what distinguishes a value parameter from a type parameter, so
  `<const N: usize>` is never mistaken for a trait bound.

  `Ty` may be **any primitive type** — an integer of any width, `bool`, `char`,
  `f32`. An **array length** is the one slot that fixes it: `[N]T` is a `usize`
  count (§3.2), so a parameter standing in one must be declared `usize`. That is
  a property of the slot, not of `const` parameters. Restricting every parameter
  to `usize` would be an arbitrary line: the parameter is a compile-time value,
  every primitive has compile-time values, and the type system already has to
  compare and substitute them. An **integer width** is the other fixed slot —
  `int.<N>` takes a `u16` (§3.1) — and a `<const B: bool>` or `<const C: char>`
  is an ordinary thing to want besides.

  A parameter fills a slot on the **widening** rule (§3.1), not on equality: a
  `<const N: u8>` is a good integer-width argument, and a `<const N: usize>` is
  not.

  Aggregates are **not** const parameters: a struct or an array as a generic
  argument would put structural equality of arbitrary values into type identity,
  which is a much larger promise than comparing two primitives.
- Generic parameters are compile-time values; the language **monomorphizes**
  (each instantiation generates specialized code), which enables the LLVM backend
  and reflection over `T`. Runtime polymorphism is opt-in via `dyn Trait` (see
  [03-types.md](03-types.md) §3.4).

The same `< >` parameter declaration also heads an **`impl`** block, letting one
impl cover a whole family of types (blanket, generic-trait, and conditional
impls), with the most specific matching impl selected per use site. See
[04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.8.

### Instantiation and inference — `.<...>` and `_`

Type arguments are **usually inferred** and the turbofish omitted:

```
Vector.<int>.new()          // T explicit
Vector.new()                // T inferred from later push/use
cast(self.id)              // target type inferred from context
```

When explicit, supply arguments with `.<...>`; use `_` to leave individual
positions to inference:

```
client.get.<[]CatImage>(url)   // explicit
make.<[]_>(1024)              // element type inferred
collect.<_, str>(iter)      // first inferred, second fixed
```

The `.<` token (not bare `<`) removes the C++ `f<a>(b)` ambiguity. `.<...>` is
**mandatory** for supplying arguments in every position, type and value alike;
bare `<...>` is never a valid argument list. The only place `<` `>` appears is the
*declaration* of generic parameters (`func <T>` and the type-name
equivalent). Whether an omitted turbofish is inferred is settled between the AST
and IR stages.

A `.<...>` applied to a generic function name **without** a following call is
itself an expression: it names the chosen specialization, yielding a value of that
function's type — a reference/pointer that can be stored, passed, or called later:

```
const fn := sth.function.<int32>    // the int32 specialization, as a value
fn(x)                               // call it later
```

## 5.5 Closures

A **closure** is a function written where a value goes, which may name the
locals around it:

```
closure = '{' [ '[' identifier { ',' identifier } ']' ]
              [ param { ',' param } ] [ '->' type ] 'in'
              { statement } [ expr ] '}'
param   = identifier [ ':' type ]
```

```
const double := { x in x * 2 }
const add    := { a: i32, b: i32 -> i32 in a + b }
const now    := { in clock.now() }
const scaled := { [n] x in x * n }
```

A parameter's type and the result type may be left out; they are inferred —
from the parameter the closure is passed to, when there is one, and otherwise
from how the closure is used. `in` ends the header, and a closure that takes
nothing is `{ in body }`. A `{` whose first tokens are not a header — `in`, a
`[` list of names, or a name followed by `,`, `:`, `->` or `in` — is a block.

`func (x: i32) -> i32 { return x * 2 }` written where a value goes is a closure
too, spelled with its types. Only a `::` binding makes a `func` literal a
definition, and a `::` function written inside a body **never** captures: it is
a constant, and naming a local of the function around it is an error.

### Captures

A closure **shares** every local it names from outside: it reads what the
local holds when it is called, and a write through either is seen by both.

```
let mut count := 0
list.each() { x in count += x }        // count is the sum afterwards
```

A shared local lives as long as the closure does, whatever frame bound it, so
each binding is its own: a closure made on one pass of a loop keeps that pass's
`let`, not the next one's.

A name in the **capture list** is copied instead, when the closure is made. The
copy is read-only.

```
let mut n := 1
const f := { [n] x in x + n }
n = 10
f(1)                                   // 2
```

### Types, and calling one

Every closure has a type of its own that no program names. What it and a
function pointer (`*func(...)`, §3.5) have in common is the prelude trait
`Func`:

```
Func :: #lang("func") trait <Args> { Output :: type }
```

`Func(A, B) -> R` is how a bound on it is written, and it means
`Func.<(A, B), Output = R>` — the arguments as one tuple, `()` for none, and a
missing `-> R` is `-> void`. Nothing implements `Func` but the compiler: a
closure implements it with its own signature, and so does a `*func`.

A value whose type implements `Func` is called like a function. Taking a
closure is taking something that implements `Func`, and each closure passed
makes its own instantiation, so the call is a direct one:

```
apply :: func (f: impl Func(i32) -> i32, x: i32) -> i32 { return f(x) }

apply(double, 3)              // a *func
apply({ x in x + n }, 3)      // a closure
```

A closure is an ordinary value: store it, pass it, call it later.

## 5.6 Entry point

`main :: func ()` in the root namespace is the program entry point. It may return
`void` or a `Result.<void, E>`; returning `.err` sets a non-zero process exit
status.
