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
binding = pattern '::' ( expr | type ':=' expr )
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

### A constant's type

`SERVER_PORT :: 8080` has **no single runtime type**. A numeric literal is a
`comptime_int` (§3.1), and a constant bound to one stays untyped: each use site
settles it for itself, which is what lets the same `MAX` be an `i8` in one place
and an `i64` in another. A string literal behaves the same way — `GREETING ::
"hi"` is a `comptime_str`, and one use of it may be a `str` and another a `[]u8`
(§1.5).

To **pin** a constant to one type, write the type and give the value after `:=`:

```
MAX      :: 100            // comptime_int; settles per use site
MAX_BYTE :: u8 := 100      // a u8 everywhere, and only a u8
```

This is the same `T := value` shape an associated constant (§3.4) and a
`#static` region (§2.6) use. Writing the type is what tells `name :: u8` (a type
alias) from `name :: u8 := 5` (a `u8` constant): a `::` RHS holds either a value
or a type, and a bare type is an alias.

A pinned constant is range-checked against its type with the literal's exact
value in hand, so `MAX_BYTE :: u8 := 300` is rejected rather than wrapped.

### Evaluation

A `::` binding **is** its value: there is no run-time moment at which it could be
computed. Its right-hand side is therefore evaluated at compile time, which means
it may only name other constants, `const` generic parameters, and calls to
`#const` functions (§5.1) — the last being the point of `#const`:

```
#const next_pow2 :: func (n: u32) -> u32 { ... }
CAPACITY :: u32 := next_pow2(1000)
```

Evaluation runs an interpreter over the same subset `#const` admits: integers,
floats, booleans, characters, string and byte-string literals, and composites of
those, with locals, branches, `match` and loops. It holds no pointers and no heap
values, so taking an address is not a constant expression — a string literal is
not an exception, being read-only data the compiled program contains rather than
something allocated. Integer arithmetic is exact and division by zero is
an error rather than a trap, since there is no running program to trap. A
computation that does not terminate is reported against a step budget.

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

### Trait impls carry too, with `Self` rebound

Trait impls are inherited on the same terms, and an impl written for the distinct
type wins over an inherited one.

What is *not* substituted matters as much as what is: **`Self` stays bound to the
distinct type.** Only the matching is done against the representation.

```
Meters :: distinct f64

a + b       // Meters — not f64
```

The operator traits declare `Output` as `Self.Output`, and the primitive impls
give `Output = Self`. With `Self` bound to `Meters`, the result is `Meters`. Had
`Self` been rebound to `f64` the distinction would evaporate on the first
arithmetic operation, which is exactly what `distinct` exists to prevent.

An inherited operator stays **homogeneous** — `Rhs = Self` — so mixing the
distinct type with its representation is still an error:

```
mix :: func (m: Meters, r: f64) -> Meters { return m + r }
// error: `Meters` does not implement `core.Add.<f64>`
```

Note this only applies to `Self`. An inherited method that returns the
*representation* still returns it: `impl Base { twin :: func (self: *Base) -> Base }`
inherited by `Wrapper` yields a `Base`, because nothing has established that the
result satisfies whatever invariant `Wrapper` carries.

### Literals settle on a distinct numeric

A `comptime_int` may become a `distinct` type over an integer, exactly as it
becomes the integer itself; likewise `comptime_float`:

```
HttpPort :: distinct u16

const p: HttpPort := 80        // no `$cast` needed
q :: func (p: HttpPort) -> HttpPort { return p + 1 }   // `1` is a HttpPort
```

Without this every literal reaching a distinct numeric would need a `$cast`,
which is the ceremony the type exists to buy back.

**To inherit nothing**, use a tuple struct instead — it is a new type with a
field, not a new name for one:

```
Opaque :: struct (f64)     // no methods, no operators, no traits
```

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

## 2.6 Static mutable storage (`#static`)

`let` / `const` appear only inside function bodies. At **namespace scope** every
binding is `::`, and a `let` / `const` there is an error: `let` binds a name to a
stack slot belonging to an enclosing call, and at namespace scope there is no
call.

A program-lifetime **mutable region** is declared by putting the `#static`
directive on a `::` binding:

```
#static request_count :: uint := 0
#static scratch :: [4096]u8                    // no initializer -> zeroed
```

The directive decides how the right-hand side reads. Without it, `name :: 0` is a
value and `name :: [4096]u8` is a *type alias* — a `::` RHS holds either, and
only its shape says which. A static declares neither: it declares a **region**,
so its RHS is the region's **type**, and the initial contents, if any, follow
`:=`. That is the same `T := value` shape a typed constant (§2.5) and an
associated constant (§3.4) use, so there is one rule for where a written type
goes.

- `#static` names a single memory region that lives for the whole program and is
  **shared** by all code that can see the name. It is the only namespace-scope
  mutable binding.
- The **type is required**. A region is storage, and storage has a width; the
  zeroed form has no initializer to infer one from.
- The initializer must be a constant expression — it is written into the
  program's initialized data, so it has to be computable at compile time. It may
  be **omitted**, in which case the region is zeroed (the one binding form that
  needs no initializer, since static storage is zeroed anyway — unless the type
  is `#raw`, which leaves it uninitialized and readable only in an `#unsafe`
  scope).
- The same form inside a function gives that local a single program-lifetime
  region that persists across calls (C `static`-local semantics):

```
tick :: func () {
  #static calls :: uint := 0
  calls = calls + 1
}
```

- A `#static` is **shared global state**: the language performs no
  synchronization, so concurrent access is a data race unless mediated by std
  atomics / locks. Mutability still follows the normal rules (`[]mut`, `*mut`).
- Reading a static is a **run-time** operation, so a static may not appear in a
  constant expression: its contents are whatever the running program last wrote.

A static's initializer is not a place to build a heap value. Nest is garbage
collected and a static is not a region the collector traces, so a global that
needed to allocate could not be represented at all — which is why the constant
requirement is a rule rather than a convenience. A lazily initialized global
belongs in the standard library, as a `Lazy.<T>` over a static, not in the
language.

See [09-directives-and-attributes.md](09-directives-and-attributes.md) for the
`#static` directive.
