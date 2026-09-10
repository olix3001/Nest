# 02 — Declarations and Bindings

Everything a program names is introduced by one of three forms. The choice
between them is driven by a single question: **is the bound value known at
compile time?**

| Form | Binds | Mutable? | RHS known at |
|------|-------|----------|--------------|
| `name :: value` | constants, types, functions, traits, namespaces | no | compile time |
| `const name := value` | a local (or scoped) value | no | run time |
| `let name := value` | a local (or scoped) value | yes | run time |

## 2.1 Compile-time bindings — `::`

```
binding = pattern '::' expr
```

`::` binds a name to something the compiler can evaluate at compile time. The
right-hand side may be:

- a **value** (`SERVER_PORT :: 8080`, `SERVER_ADDR :: "127.0.0.1"`),
- a **type** (`CatId :: distinct str`, `Point :: struct { ... }`),
- a **function** (`main :: func () { ... }`),
- a **trait** (`ToJson :: trait { ... }`),
- a **namespace** (`config :: namespace { ... }`),
- the result of a **compile-time expression**, including `import`, a `#const`
  function call, and intrinsics of constant operands
  (`SERVER_PORT :: $cast.<HttpPort>(8080)`).

`::` bindings are always immutable and always have a compile-time-known value.
They may appear at the top level of a file, inside a namespace, or inside a
function body (a local type or local constant).

The left-hand side is a **pattern** (see
[07-patterns-and-matching.md](07-patterns-and-matching.md)), which is why import
destructuring works:

```
{ http: { Client, Router, Response } } :: import "network.nest"
{ CatImage, CatId, HttpPort }          :: import "models.nest"
io                                      :: import <std/io>
```

Here the RHS (a namespace value) is destructured by the pattern on the LHS. A
plain identifier is the trivial pattern; a bare `*` globs every public member into
scope (see [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.5).

## 2.2 Runtime bindings — `let` and `const`

```
local = ( 'let' | 'const' ) identifier [ ':' type ] ':=' expr
```

Inside function bodies, values computed at run time are bound with `:=`,
prefixed by `const` (immutable) or `let` (mutable):

```
const router := http.Router { logging: true }   // immutable
let   count  := 0                                // mutable
count = count + 1
```

`const … :=` and `::` are both immutable; they differ in *when* the value
exists. Use `::` for compile-time constants and type/function definitions; use
`const … :=` for run-time values you will not reassign.

### Type annotations

An optional `: Type` between the name and `:=` fixes the type and provides the
**contextual type** used by literal defaulting, inferred enum variants, and
inferred composite literals:

```
const port: HttpPort := config.SERVER_PORT
const cats: []CatImage := fetch_all()
```

When the annotation is omitted, the type is inferred from the initializer.

### Initialization requirement

Every `let`/`const` must be initialized at the point of declaration; there is no
uninitialized-then-assigned form. (Use an `Option.<T>` initialized to `.none` if
a value genuinely arrives later; or, for raw buffers, a `#raw` type in an
`#unsafe` scope — see
[09-directives-and-attributes.md](09-directives-and-attributes.md).)

## 2.3 Assignment

After a `let` binding, plain assignment reassigns it:

```
assign = place ( '=' | '+=' | '-=' | '*=' | '/=' | '%=' ) expr
```

`place` is an assignable location: a `let` variable, a field of one, an element
of a `[]mut`/array, or a `.*` through a `*mut` pointer. `const` and `::`
bindings are not assignable, nor are places reached through a read-only `*T` /
`[]T`. Binding mutability (`let` vs `const`) and reference mutability (`*mut` /
`[]mut`) are independent: `const p := &mut x` is an immutable binding holding a
mutable pointer, so `p.* = 1` is legal but `p = &mut y` is not. Compound
assignments (`+=`, …) desugar to `place = place op expr`.

## 2.4 `distinct` types

`distinct T` creates a new **nominal** type with the same representation and
memory layout as `T`, but which is not interchangeable with `T` or with any
other `distinct T`:

```
CatId    :: distinct str
HttpPort :: distinct u16
```

Conversion in either direction is explicit via `$cast`:

```
const raw_id: str  := $cast(self.id)          // CatId -> str
const id: CatId    := $cast.<CatId>("abc")    // str -> CatId
```

`distinct` exists to make units and identifiers type-safe: an `HttpPort` cannot
be silently passed where a plain `u16` is expected, and vice versa.

### Method inheritance is one-way

A `distinct T` **inherits `T`'s methods**; `T` does not gain the distinct type's.

```
CatId :: distinct str
impl CatId {
  is_valid :: func (self: CatId) -> bool { ... }
}

id.len()        // ok — inherited from `str`
id.is_valid()   // ok — CatId's own
s.is_valid()    // error: no method `is_valid` on `str`
```

The asymmetry is the point. A `distinct T` is `T` plus an invariant and some
extra operations, so everything `T` can do it can do; the operations that assume
the invariant stay off `T`, where the invariant does not hold. Inheriting in both
directions would make the type not distinct at all.

A method the distinct type declares itself **wins** over an inherited one of the
same name, which is how a `distinct` type refines behaviour rather than only
adding to it. Because the representations are identical, reaching an inherited
method is a reinterpretation of the receiver and costs nothing at run time.

A `distinct` type also takes **directives**, which is how one becomes a language
item:

```
str :: #lang("str") distinct []u8
```

## 2.5 Shadowing and scope

Bindings are lexically scoped to the enclosing block (`{ }`), namespace, or
file. An inner binding may **shadow** an outer one of the same name; the inner
name wins for the remainder of its scope. Shadowing a `let` with a `const` (or
vice versa) is allowed.

Within a single scope, redeclaring the same name is an error unless it is an
explicit shadow in a nested block. `::` bindings at namespace scope are
order-independent (mutually recursive types and functions are fine); `let` /
`const` bindings inside a function are order-dependent and must be declared
before use.

## 2.6 Static mutable storage (`#static let`)

`let` / `const` normally appear only inside function bodies. At **namespace
scope**, every ordinary binding is `::` (immutable, compile-time); a bare `let`
there is an error. A program-lifetime **mutable** region is declared instead with
the `#static` directive on a `let`:

```
#static let request_count: uint := 0
#static let scratch: [4096]mut uint8            // no initializer -> zeroed
```

- `#static let` names a single memory region that lives for the whole program and
  is **shared** by all code that can see the name. It is the only namespace-scope
  mutable binding.
- The initializer must be a `#const` expression. It may be **omitted**, in which
  case the region is zero-initialized (the one place a binding needs no
  initializer, since static storage is always zeroed — unless the type is `#raw`,
  which leaves it uninitialized and readable only in an `#unsafe` scope).
- `#static` may also mark a `let` **inside a function**, giving that local a
  single program-lifetime region that persists across calls (C `static`-local
  semantics).
- A `#static let` is **shared global state**: the language performs no
  synchronization, so concurrent access is a data race unless mediated by std
  atomics / locks. Mutability still follows the normal rules (`[]mut`, `*mut`).

See [09-directives-and-attributes.md](09-directives-and-attributes.md) for the
`#static` directive.
