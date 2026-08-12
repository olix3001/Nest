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
       | identifier ':' type
```

```
new    :: func (id: CatId, url: string, w: int32, h: int32) -> CatImage { ... }
listen :: func (self: *Router, host: string, port: HttpPort) { ... }
```

Parameters are immutable bindings inside the body (rebind locally with `let` if
you need a mutable copy). A parameter's *type* still carries its own mutability:
`s: []mut int` is an immutable binding to a mutable slice, so `s[i] = x` is
allowed but `s = other` is not. There are no default parameter values in this
version.

### The receiver (`self`)

A parameter named `self` marks the function as a **method** of the type its
`#impl` namespace targets. Its type is `*T` (read-only receiver), `*mut T`
(mutating receiver), or `T` (by-value receiver):

```
#impl(Router) namespace {
  @public
  get :: func (self: *mut Router, path: string, handler: func() -> Response) { ... }
}
```

A method is invoked with dot syntax, `router.get("/cat", handler)`, binding the
receiver to `self`; the compiler auto-takes `&router` / `&mut router` as the
receiver type requires. A function in an `#impl` namespace **without** `self` is
an **associated function**, called on the type: `CatImage.new(id, url, w, h)`.

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
the call must also be named. Names must match parameters, each supplied exactly
once.

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
generic_param = identifier [ ':' constraint ]     // type param; bare `T` == `T: type`
              | 'const' identifier ':' type        // compile-time value param
constraint    = 'type' | type { '+' type }         // `type` = any type; else trait bounds
```

```
get :: func <T: type> (self: *Client, url: string) -> Result.<T, FetchError> {
  ...
}

zeros :: func <const N: uint32> () -> [N]byte { ... }   // value param used in a type
```

- `T` / `T: type` — `T` is any type (most permissive); the bare form is shorthand
  for `T: type`.
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
*declaration* of generic parameters (`func <T: type>` and the type-name
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
