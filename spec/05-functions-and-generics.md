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
render :: #inline func (self: *CatImage) -> string {
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
  $assert(n > 0, "port must be non-zero")
  return $cast.<HttpPort>(n)
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
new    :: func (id: CatId, url: string, w: int32, h: int32) -> CatImage { ... }
listen :: func (self: *Router, host: string, port: HttpPort) { ... }
```

Parameters are immutable bindings inside the body (rebind locally with `let` if
you need a mutable copy). A parameter's *type* still carries its own mutability:
`s: []mut int` is an immutable binding to a mutable slice, so `s[i] = x` is
allowed but `s = other` is not.

### Default values (`:=`)

A parameter may carry a **default**, which makes it optional at the call site:

```
pad :: func (s: string, width: usize := 8, fill: char := ' ') -> string { ... }

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
  elements are all constant, a `$cast` of a constant, a location directive
  (`#caller_location`), or a call to a `#const` function. Note that this is not the same as
  "a single fixed value" — `#caller_location` differs at every call site and is
  still admissible, because each site knows its own.
- An omitted argument is filled in during lowering, so the compiled call is
  ordinary and positional; nothing after that stage knows a default was involved.

### The receiver (`self`)

A parameter named `self` marks the function as a **method** of the type its
`impl` namespace targets. Its type is `*T` (read-only receiver), `*mut T`
(mutating receiver), or `T` (by-value receiver):

```
impl Router {
  @public
  get :: func (self: *mut Router, path: string, handler: func() -> Response) { ... }
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

When the **last** parameter has a function type, its argument may be written as a
trailing brace block after the `)`, and the `()` dropped if it is the only
argument:

```
router.get("/cat") {
  const cats := client.get.<[]CatImage>(url).!
  return ...
}
```

If the closure takes parameters, they are listed in a **header** terminated by
`=>`. Parameter types are optional and inferred from the parameter's function
type when omitted:

```
list.reduce(0) { acc, x =>              // types inferred
  return acc + x
}

list.reduce(0) { acc: int, x: int =>   // types explicit
  return acc + x
}
```

A block with no `=>` header takes no parameters. Either way the trailing block is
equivalent to passing a closure as the final argument:

```
router.get("/cat", func() -> Response { ... })
list.reduce(0, func(acc: int, x: int) -> int { return acc + x })
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

`Item` must be an associated type declared by the named trait; the constraint
requires the implementor's choice for that associated type to equal the given
type. Positional type arguments and `name = type` bindings may be mixed in one
turbofish (`Map.<K, Value = V>`).

```
get :: func <T> (self: *Client, url: string) -> Result.<T, FetchError> {
  ...
}

zeros :: func <const N: uint32> () -> [N]byte { ... }   // value param used in a type
```

- `T` — any type, unconstrained (most permissive). A type parameter needs no kind
  annotation; the `const` keyword is the only thing that marks a *value* parameter,
  so there is no `T: type` form.
- `T: SomeTrait` — constrains `T` to implementors of `SomeTrait`, enabling that
  trait's methods in the body. Multiple bounds: `T: TraitA + TraitB`.
- `const N: Ty` — a **compile-time value** parameter: a constant of the concrete
  type `Ty` (e.g. `uint32`), usable in the body and in types such as `[N]byte`.
  The `const` keyword is what distinguishes a value parameter from a type
  parameter, so `<const N: uint32>` is never mistaken for a trait bound.
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
$cast(self.id)              // target type inferred from context
```

When explicit, supply arguments with `.<...>`; use `_` to leave individual
positions to inference:

```
client.get.<[]CatImage>(url)   // explicit
$make.<[]_>(1024)              // element type inferred
collect.<_, string>(iter)      // first inferred, second fixed
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

An anonymous `func` literal that captures its environment is a closure, inhabiting
the matching function type:

```
const make_adder := func (n: int) -> func(int) -> int {
  return func (x: int) -> int { return x + n }   // captures `n`
}
```

Captures are by reference to the captured binding, kept alive by the garbage
collector. Closures are ordinary values: store, pass, and return them.

## 5.6 Entry point

`main :: func ()` in the root namespace is the program entry point. It may return
`void` or a `Result.<void, E>`; returning `.err` sets a non-zero process exit
status.
