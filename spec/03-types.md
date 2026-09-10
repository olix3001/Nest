# 03 — Types

A type is a compile-time value. Anywhere a type is expected, any `::`-bound name
whose value is a type may be used, as may the type-forming syntax below. Types
are first-class at compile time: they can be passed to generic functions, stored
in `::` constants, and inspected by reflection (planned; see
[12-reflection.md](12-reflection.md)).

## 3.1 Primitive types

```
Signed integers:   i8  i16  i32  i64   iN    (arbitrary width N in 2..=65535; i1 is not a type)
Unsigned integers: u8  u16  u32  u64   uN    (arbitrary width N in 1..=65535; u1 is bool)
Pointer-sized:     isize usize                (signed / unsigned integer wide enough to hold any address or index)
Floating point:    f16  f32  f64  f80  f128   (exactly these widths; there is no bare `float`)
Boolean:           bool   (an alias for u1)
Text:              char   (Unicode scalar, 32-bit)   string   (UTF-8, immutable)
Unit:              void   (the empty tuple; a function with no `-> T` returns void)
```

Integers are written `i<N>` / `u<N>` for a bit width `N` up to `65535`: the
common widths `i8`/`i16`/`i32`/`i64` and `u8`/`u16`/`u32`/`u64` are just the
familiar cases of an arbitrary-width family, so `u7`, `i24`, `u4096` are equally
legal type expressions. `i1` is **not** a type; `u1` is spelled `bool`. There is
**no** bare `int`/`uint` — use the pointer-sized `isize`/`usize` for addresses,
lengths, and indices (`.len()`, indexing, C interop sizes), and a fixed width
otherwise. Floats exist only at the widths `f16`/`f32`/`f64`/`f80`/`f128`.

Integer literals have type `comptime_int` and float literals `comptime_float`
until context assigns a concrete type (see
[01-lexical-structure.md](01-lexical-structure.md)); a `comptime_int` implicitly
converts to any integer type whose range holds its value. Between concrete
numeric types there are **no implicit conversions**; widening and narrowing both
go through `$cast`.

## 3.2 Pointers (`*T`, `*mut T`, `&x`)

Pointers are Go-style: explicit in type and at the point of taking an address,
automatically dereferenced on use, garbage-collected, and **without pointer
arithmetic**. They are **immutable by default**; write access is opt-in.

```
*T          read-only pointer to a T (cannot write through it)
*mut T      pointer through which the pointee may be mutated
&expr       address-of: yields *T, or *mut T from a mutable place
```

- **Immutable by default.** `*T` is a read-only view; writing `p.field = x`
  through a `*T` is a compile error. Use `*mut T` to mutate the pointee. `&x` of
  a `let` (mutable) place can yield `*mut T`; `&x` of a `const` place yields
  `*T`. Reference mutability is orthogonal to binding mutability (`let`/`const`;
  see [02-declarations-and-bindings.md](02-declarations-and-bindings.md)).
- **Auto-deref.** Field access, method calls, and indexing through a pointer do
  not require an explicit deref: given `self: *CatImage`, `self.url` reads the
  field. The **postfix** `p.*` yields the whole pointee when needed (Zig-style;
  there is no prefix `*p`).
- **Never null.** A `*T` / `*mut T` always points at a live value; there is no
  null pointer and no null literal. Absence is `Option.<*T>` (`.none`). A
  genuinely nullable raw pointer exists only for C interop as `c.ptr.<T>` (see
  [11-c-ffi.md](11-c-ffi.md)).
- **GC-managed.** Taking `&x` is always safe; the collector keeps the pointee
  alive while the pointer is reachable. No lifetime annotations, no manual free.
- **No arithmetic.** `p + 1` is a type error. Iterate slices for sequential
  access.

## 3.3 Composite types

### Structs

A struct type has three shapes, matching enum-variant syntax:

```
struct = [ directive ]* 'struct' [ struct_body ]
struct_body =
    '{' { field | comptime_item } '}'          // record struct
  | '(' type { ',' type } ')'                   // tuple struct
  | (* nothing *)                               // unit struct
field = [ attribute ]* identifier ':' type ','
```

```
CatImage :: struct {                 // record struct
  id: CatId,
  url: string,
  width: int32,
  height: int32,
}

Pair   :: struct(int, int)           // tuple struct; fields are .0 and .1
Marker :: struct                     // unit struct; a single value `Marker`
```

Fields are private to the struct's namespace unless the struct is `@public(all)`
or the field is individually `@public`. Inside a `@public(all)` struct, an
individual field may be re-hidden with `@private` (see
[09-directives-and-attributes.md](09-directives-and-attributes.md)). A record
body may also contain compile-time items such as `$assert(...)` (see
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.10).

Struct layout is affected by the directives `#packed`, `#align(N)`, and `#raw`
(§9). Struct values are built with a **composite literal**:

```
Type { field: value, ... }        // named record
Type(a, b)                        // named tuple struct
.{ field: value, ... }            // inferred record (from context)
.{ a, b }                         // inferred tuple struct (from context)
```

When the context type is a tuple struct, a positional inferred literal builds it:
`const x: MyType := .{ a, b }` is identical to `const x := MyType(a, b)`.

**Named vs. anonymous.** A `struct { ... }` written inline in a type position —
`x: struct { a: int, b: int }` — is an **anonymous** struct type. Anonymous
struct types are *structural* (two with the same fields are the same type). A
struct bound to a name with `::` is **nominal**: it is its own distinct type even
if another named or anonymous struct has identical fields, and it never
implicitly converts to or from them (use `$cast`, or an `@using` field — §3.8,
§3.10). Only named structs can have `impl` methods; anonymous structs are plain
data.

### Arrays and slices

```
[N]T        fixed-length array of N elements (N is a compile-time constant)
[]T         read-only slice: a (ptr, len) view over a contiguous run of T
[]mut T     mutable slice: elements may be assigned through it
[N]mut T    fixed array whose elements may be mutated through this reference
```

Slices, like pointers, are **immutable by default**: `s[i] = x` is legal only
when `s : []mut T`. Indexing is `s[i]`, length is `s.len()`, sub-slicing is
`s[lo..<hi]` (see the range operators in
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.12).

`.len()` is **not** compiler syntax. It is an ordinary inherent method the core
library declares on the built-in sequences — `impl <T> []T { len :: ... }` and
`impl <T, const N: usize> [N]T { len :: ... }` — whose body is the `$len(s)`
intrinsic (§6.4). Writing it that way is what makes `a.len()`, `s.len()`, and the
std `Vector`'s `.len()` one spelling with one meaning; only `core` can declare it,
because only the defining package may write an inherent impl (§4.9). `$len` may
also be called directly, and on a `[N]T` whose `N` is known it folds to a
compile-time constant.
Out-of-bounds indexing traps at run time (unless in an `#unsafe`
scope, §9). A slice-of-structs may be laid out struct-of-arrays with the `#soa`
directive (§9). Growable sequences are the std `Vector` (§3.9).

Array **values** are written with composite literals — `.{ 1, 2, 3 }` (inferred),
`[_]T { ... }` / `[N]T { ... }` (explicit), `.{ 0; n }` (repeat) — see
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.2.
Brackets themselves never introduce a value; they build the type. A fixed array
`[N]T` **implicitly coerces to a read-only `[]T`** where a slice is expected, but
never to `[]mut T`. The coercion *is* taking the whole sub-slice: it means
exactly what writing `a[..]` means, and compiles to the same thing.

The length is **part of the type**: `[3]int32` and `[4]int32` are different
types and do not convert. `N` may be an integer literal, a constant, or a
`const` generic parameter (§5); `[_]T { ... }` leaves it to be inferred from the
literal that fills it.

### Enums (sum types)

```
enum = [ directive ]* 'enum' [ generics ] '{' { variant } '}'
variant = [ attribute ]* variant_name [ payload ] ','
variant_name = snake_case_identifier
payload =
    '(' type { ',' type } ')'      // tuple payload
  | '{' field { field } '}'        // record payload
```

Variant names are **snake_case**. A variant may be bare, carry a positional
(tuple) payload, or carry a named (record) payload:

```
Response :: enum {
  ok(string),
  redirect(string),
  bad_request(string),
  not_found,
  internal_server_error(string),
}

Shape :: enum {
  circle { radius: f64 },
  rect   { w: f64, h: f64 },
  point,
}
```

Variants are constructed with a leading `.`, the enum type inferred from context:

```
return Response.redirect(cat.url)   // fully qualified
return .redirect(cat.url)           // inferred, in a Response context
return .rect { w: 3.0, h: 4.0 }     // record-payload variant
```

Enums are consumed by `match` (see
[07-patterns-and-matching.md](07-patterns-and-matching.md)). `Result` and
`Option` are ordinary enums provided by the prelude.

### Tuples

```
(A, B, C)          tuple type
(a, b, c)          tuple value
t.0  t.1  t.2      positional access
```

`void` is the zero-element tuple `()`.

## 3.4 Traits and dynamic dispatch

A `trait` is a set of method signatures a type can implement. Traits serve as
**static bounds** on generics (monomorphized, no vtable) and, explicitly, as
**dynamic trait objects** via `dyn`.

```
trait = [ directive ]* 'trait' '{' { trait_member } '}'
trait_member = method_sig | assoc_type
method_sig = identifier '::' 'func' [ generics ] '(' params ')' [ '->' type ]
assoc_type = identifier '::' 'type' [ ':' type { '+' type } ]  // trait bounds on the impl's choice
```

```
ToJson :: trait {
  render :: func (self: *Self) -> string
}
```

- `Self` names the implementing type.
- A type implements a trait through an anonymous impl namespace introduced by the
  `impl` keyword: `impl ToJson for CatImage { ... }`. The target is written in the
  header, so a trait may be implemented for a type not in the current namespace,
  and generic impls (`impl <T> ToJson for Storage.<T>`) cover whole families with
  most-specific-wins selection (see
  [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.1, §4.8).
- **Static bound:** `func <T: ToJson>(...)` accepts any `T` implementing `ToJson`
  and is monomorphized — no vtable.
- **Dynamic dispatch:** the type `dyn ToJson` is a trait object. It is **unsized**
  — its size is the erased type's, which is precisely what the type no longer
  says — so it names a type only **behind a pointer**: `*dyn ToJson` (or
  `*mut dyn ToJson`) is a fat pointer, data + vtable. A bare `dyn ToJson` as a
  variable's type, a field, a parameter, or a slice element is an error; a slice
  *of pointers*, `[]*dyn ToJson`, is fine, because the pointer is what has the
  size. A `*T` coerces to `*dyn Trait` when `T: Trait` (and `*mut T` to
  `*mut dyn Trait`), or explicitly `$cast.<*dyn ToJson>(&cat)` — the same
  unsizing, written out. Method calls on it dispatch through the vtable, with
  `Self` resolved to `dyn ToJson`:

```
const j: *dyn ToJson := &cat
io.println(j.render())              // virtual call

render_all :: func (xs: []*dyn ToJson) { ... }
```

`dyn` is the **only** place a vtable appears; everything else is static.

## 3.5 Function types

```
func_type = 'func' [ generics ] '(' [ param_types ] ')' [ '->' type ]
```

Used for higher-order parameters:

```
handler: func() -> Response
```

Function values (including closures) inhabit function types; see
[05-functions-and-generics.md](05-functions-and-generics.md) and
[06-expressions-and-operators.md](06-expressions-and-operators.md).

## 3.6 The `Option` enum

Absence is modeled by the prelude enum `Option` (there is **no** `?T` sugar):

```
Option :: enum <T> {
  some(T),
  none,
}
```

`Option.<*CatImage>` is an optional pointer; `Option.<string>` an
absent-or-present string. There is no `nil`/`null`; the empty value is `.none`,
and a bare `T` coerces to `.some(value)` in an `Option` context.

An `Option` is consumed by `match` (exhaustive `.some(v)` / `.none`), by methods
(`unwrap_or(default)`), or by the `Try` operators `.?` / `.!` — which are not
special-cased to `Option`/`Result` but work on any type implementing the `Try`
trait (see [08-error-handling-and-defer.md](08-error-handling-and-defer.md)).

## 3.7 Generic type application (`.<...>`)

A generic type or function is instantiated with the `.<...>` turbofish:

```
Result.<T, models.FetchError>
client.get.<[]CatImage>(url)
$cast.<HttpPort>(8080)
```

Type arguments are **usually inferred** from context, so `.<...>` is frequently
unnecessary (`$cast(self.id)` with the target inferred, `Vector.new()` with `T`
inferred from later use). When some arguments should be inferred and others
fixed, the placeholder `_` requests inference of a position:

```
$make.<[]_>(1024)          // element type inferred from context
collect.<_, string>(iter)  // first type-arg inferred, second fixed
```

A turbofish argument may also be an **associated-type equality**, `name = type`,
which pins an associated type of the instantiated trait rather than supplying a
positional parameter:

```
Iterator.<Item = int32>          // the Iterator trait with Item fixed to int32
Trait.<K, Value = V>             // mixed: positional K, plus Value = V
```

This is chiefly used to write bounds (see
[05-functions-and-generics.md](05-functions-and-generics.md) §5.4). `name` names
an associated type declared by the trait, and `name = type` is unambiguous
against a positional type argument because a type is never followed by `=` here.

The `.<` token disambiguates generic application from `<` comparison (see
[01-lexical-structure.md](01-lexical-structure.md)). Whether an omitted turbofish
is filled in by inference is resolved between the AST and IR stages.

## 3.8 Type identity and equivalence

- **Named types are nominal.** Identity is the declaration, not the structure.
  Every `::`-bound named type — including every named `struct` and `enum` — is
  its own type, as if it were `distinct`. Two named structs with identical fields
  are **different types**; a named struct and a structurally-identical anonymous
  struct are **different types**. `distinct T` is the same nominal rule applied to
  a non-aggregate underlying type.
- **Anonymous types are structural.** Types written inline without a name — an
  anonymous `struct { a: int, b: int }`, `*T`, `*mut T`, `[]T`, `[]mut T`,
  `[N]T`, `(A, B)`, `func(...)`, `Option.<T>`, `dyn Trait` — are equal when their
  components (and, for structs, the set of field name→type) match. Two anonymous
  `struct { a: int, b: int }` are the same type.
- **No implicit conversions across nominal boundaries.** A named struct does not
  implicitly convert to another named struct, nor from a named struct to its
  anonymous structural twin; nor do numeric types convert, nor a `distinct T` and
  its underlying `T`. All such conversions are an explicit `$cast` (permitted when
  the layouts are compatible). There are exactly **two** implicit struct→struct
  coercions:
  1. via an `@using` field (§3.10);
  2. **from an anonymous struct value to a matching named struct** — a value whose
     type is an anonymous `struct { … }` implicitly coerces to any named struct
     with the same field name→type set. This is one-way: a *named* struct never
     implicitly becomes anonymous (that direction needs an explicit `$cast`).

  Composite literals `.{ ... }` are a further exempt case: they are untyped until
  context assigns them a type — they *become* the expected named or anonymous
  struct rather than converting from one.

  ```
  P :: struct { x: int, y: int }
  const a := .{ x: 1, y: 2 }       // anonymous struct value
  const p: P := a                  // OK: anonymous -> named P
  const q: struct{x:int,y:int} := $cast.<struct{x:int,y:int}>(p)  // named -> anon: explicit
  ```
- The mutability coercions still hold: `*mut T` coerces to `*T` and `[]mut T` to
  `[]T` (dropping write access), never the reverse.

## 3.9 Dynamic arrays: `Vector`

There is no built-in growable array; it is the std struct `Vector.<T>`, built on
the `$new` / `$make` allocation intrinsics (see
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.9):

```
let xs := Vector.<int>.new()           // empty, T inferred where possible
let ys := Vector.<int>.with_capacity(16)
xs.push(1)
xs.push(2)
const view: []int := xs.slice()        // borrow as a read-only slice
```

Maps and other containers (`HashMap.<K, V>`, …) are likewise std structs with
`.new()` constructors.

## 3.10 `@using` fields (implicit upcast)

An `@using` attribute on a struct field whose type is a struct (or a pointer to
one) marks that field for an **implicit upcast** — a deliberately narrow
Odin-style `using`. `@using` applies only to struct fields; it is not permitted on
function parameters, locals, or imports. **At most one** field per struct may be
`@using`.

```
Transform :: struct { x: f32, y: f32, angle: f32 }

Entity :: struct {
  @using t: Transform,     // Entity upcasts to Transform
  hp: int,
}
```

`@using` has exactly one effect — the **implicit upcast** — and, unlike Odin, it
does **not** promote the embedded type's members onto the outer struct:

1. **Implicit upcast.** The outer struct implicitly coerces to the `@using`
   field's type. An `Entity` value coerces to `Transform` (yielding a copy of
   `t`); a `*Entity` / `*mut Entity` coerces to `*Transform` / `*mut Transform`
   (the address of the sub-object, `&e.t`, zero-cost). Internally the coercion is
   just "take `e.t`":

   ```
   translate :: func (t: *mut Transform, dx: f32, dy: f32) { ... }
   translate(&mut e, 1.0, 0.0)     // &mut Entity coerces to *mut Transform
   ```

2. **No field promotion.** `e.x` is **not** valid — reach the embedded field's
   members through the field name (`e.t.x`, `e.t.angle = 0.0`). The one
   ergonomic exception is **method calls**: `e.method()` where `method` is defined
   on `Transform` (and not on `Entity`) resolves through the upcast, binding the
   receiver to `&e.t` (see [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.6).

**One per struct.** Allowing a single `@using` field keeps both the upcast target
and method resolution unambiguous — there is never a question of *which* embedded
type a value coerces to, or whose method `e.method()` means. Two `@using` fields
in one struct is a compile error. Only struct-typed (or pointer-to-struct) fields
may be `@using`.
