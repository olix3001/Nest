# 04 — Namespaces and Name Resolution

A **namespace** is a named scope containing declarations. It is the only
scoping-and-grouping construct in the language. **Every source file is itself an
(anonymous) namespace** — its top-level items are that namespace's members. There
is no separate "module" concept, no `module` keyword, and **no file header**: a
file is never named from within itself. Another file's namespace is obtained by
`import`ing it (§4.5), which yields that file's namespace **value**; the importer
then binds or destructures it like any other namespace. The grouping mechanism is
uniform from a whole file down to a trait implementation.

## 4.1 The two namespace forms

Every use of the `namespace` keyword introduces a **block with an explicit
body** — `namespace { ... }`. There is **no** bodyless `namespace name` form and
no way to rename or re-head the enclosing file's namespace from inside it. A
`namespace` value is used in exactly two ways.

### 1. Inline namespaces (bound with `::`)

An inline namespace is an ordinary `::` binding whose value is a `namespace`
block, used to group related declarations (including private helper groupings):

```
config :: namespace {
  @public SERVER_PORT :: $cast.<HttpPort>(8080)
  @public SERVER_ADDR :: "127.0.0.1"
}

internal_helpers :: namespace {
  validate_url :: func (url: string) -> bool { return url.len > 0 }
}
```

An inline namespace may be `@public` to export it, and may itself carry
directives (e.g. `#c` for a C-interop grouping):

```
@public
http :: namespace { ... }
```

### 2. Impl namespaces (the `impl` keyword)

A type's methods and trait implementations are supplied by an **anonymous** impl
namespace introduced by the `impl` keyword. The target type — and, for a trait
impl, the trait named before `for` — are part of the header, so the namespace is
not bound to a name and the target need not live in the current namespace:

```
impl CatImage {                             // inherent methods / associated funcs
  @public
  new :: func (id: CatId, url: string, w: int32, h: int32) -> CatImage {
    return .{ id: id, url: url, width: w, height: h }
  }
}

impl ToJson for CatImage {                  // trait implementation of ToJson
  @public
  render :: func (self: *CatImage) -> string { ... }
}
```

- `impl T { ... }` adds inherent items to type `T`.
- `impl Trait for T { ... }` implements `Trait` for `T`; the compiler checks every
  required method is present with a matching signature (`Self` resolved to `T`).
- Because the target is written in the header (not a name binding), you can
  implement traits for types declared elsewhere (subject to visibility). This also
  rules out the meaningless forms an earlier draft allowed, such as binding an impl
  to an imported value.

The block after the header is a namespace body — `impl` is dedicated sugar for
"a namespace attached to a type", so the `namespace` keyword is not repeated. Both
the target and the trait are ordinary types, so either may be a **generic
instantiation** — `impl Into.<int32> for CatId` implements the single specialized
trait `Into.<int32>`. To implement over a *family* of types instead of one, the
impl takes **generic parameters** (§4.8).

A function whose first parameter is `self: *T` / `self: *mut T` / `self: T` is a
**method** (`value.method(args)`); one without `self` is an **associated
function** (`Type.func(args)`, e.g. `CatImage.new(...)`). See
[05-functions-and-generics.md](05-functions-and-generics.md).

> **Unification note.** A file, an inline `namespace { ... }`, and an `impl ... {
> ... }` body are the same construct — a `namespace` value. A file produces one
> anonymous namespace (yielded by `import`), form 1 binds one to a name with `::`,
> form 2 attaches one to a type via the `impl` keyword.

## 4.2 Nesting

Namespaces nest arbitrarily; members are reached with `.`:

```
@public
http :: namespace {
  @public Response :: enum { ok(string), redirect(string), not_found, ... }
  @public Client   :: struct {}
  impl Client { ... }
  @public Router   :: struct { logging: bool }
  impl Router { ... }
}
```

`http.Client`, `http.Router`, `config.SERVER_PORT`. If this file is imported as
`network :: import "network.nest"`, those become `network.http.Client`, etc. — the
importer chooses the file's name, the file does not.

## 4.3 Namespace merging (same-name unification)

If several inline `namespace` declarations reachable at the same fully qualified
name are unioned (e.g. `foo :: namespace {...}` appearing at one qualified path in
more than one place), they are treated as **one namespace**: their members are
unioned. Consequently there can be **no duplicate members** — two functions,
consts, or types with the same name in same-named namespaces is a conflict error,
exactly as if they were written in one block. Because files are anonymous and
never head-name themselves, whole-file merging by header no longer exists;
assembly is explicit via `import`.

The **sole exception** is impl namespaces. Any number of `impl` blocks may target
the same type, and different trait impls may each define a method of the same name
(e.g. two traits both requiring `render`). This is sound because a trait's methods
are only reachable when that trait is in scope (imported) or accessed through an
explicit `dyn`/`$cast` (see §4.5). Inherent-method conflicts across `impl T`
blocks are still errors.

## 4.4 Visibility and re-export

Every item is **private to its enclosing namespace by default**. Privacy is
lexical: a private item is visible to its declaring namespace and all namespaces
nested inside it, never to the outside.

| Marker | Effect |
|--------|--------|
| *(none)* | private to the enclosing namespace (and its descendants) |
| `@public` | export the item from its namespace |
| `@public(all)` | (struct/enum) export the type **and** all fields/variants |
| `@private` | (field) re-hide one field inside a `@public(all)` aggregate |

```
@public CatId :: distinct string

@public(all)
CatImage :: struct {
  id: CatId,
  url: string,
  width: int32,
  height: int32,
  @private cache: Option.<string>,   // exported struct, but this field stays private
}
```

### Re-export

A namespace re-exports by putting `@public` on a `::` binding whose RHS is an
`import` (see §4.5):

```
@public io :: import <std/io>                       // re-export the whole namespace
@public { Client, Router } :: import "network.nest"  // re-export selected items
@public * :: import <prelude>                        // re-export everything public in prelude
```

`@public` is the only access modifier; there are no protected/internal tiers.
See [09-directives-and-attributes.md](09-directives-and-attributes.md).

## 4.5 `import`

`import` is a compile-time expression that **always evaluates to the namespace
value** of another file or package. It is not a statement and has no glob token of
its own; it must appear as the RHS of a `::` binding, and the **pattern on the
left** decides what happens to the imported namespace:

```
foo :: import <std/io>                             // bind the whole namespace as `foo`
* :: import <prelude>                              // GLOB: bring every public item into scope
{ CatImage, CatId, HttpPort } :: import "models.nest"        // selective
{ http: { Client, Router, Response } } :: import "network.nest" // nested selective
{ http: * } :: import <std>                        // pull member `http`, glob ITS members in
```

- `name :: import …` binds the whole namespace to `name`.
- `* :: import …` **globs**: every `@public` member of the target is brought into
  the current scope (this replaces the old bare-`import` statement).
- `{ ... } :: import …` destructures selected public members (optionally through
  nested public namespaces; see
  [07-patterns-and-matching.md](07-patterns-and-matching.md)).
- `*` may also appear **inside** a destructuring field, `{ member: * }`, to select
  a sub-namespace and glob *its* members into scope in one step.
- Prefix any of these with `@public` to additionally re-export what it brings in.
- `import` only ever grants access to `@public` items of the target; private
  items are invisible.

### Import paths — packages vs. files

The operand is an **import path**, and its *delimiter* says whether it names a
package or a file:

| Form | Meaning |
|------|---------|
| `import <name>` | a **package** — the standard library or a third-party dependency, resolved by name through the package resolver. `<std>`, `<std/http>`, `<core/c>`. The first segment is the package root; `/` walks into its public sub-namespaces. |
| `import "path"` | a **file** — a source file resolved on the file system relative to the importing file. `"hello.nest"`, `"./util.nest"`, `"models.nest"`, `"sub/router.nest"`. |

There is no ambiguity: angle brackets are never a file, quotes are never a
package. A package like `std` may internally be assembled from files, but a
consumer names it as a package (`<std/http>`), never by file path.

The `.nest` extension and `./` prefix are permitted (and conventional) in the
quoted file form; a bare relative path such as `"models"` also resolves as a file.
`import` has no run-time effect; it is resolved during compilation.

## 4.6 Name resolution

To resolve an unqualified name, the compiler searches in order:

1. **Local scope** — `let` / `const` / `::` bindings in the current block,
   innermost first, honoring shadowing.
2. **Enclosing function parameters and generic parameters.**
3. **Enclosing namespaces** — innermost outward to the file namespace, including
   names brought in by `import` (glob or selective) at each level.
4. **The prelude** — a small implicit set of always-in-scope names (`Result`,
   `Option`, `string`, the primitive types, the `$`-intrinsics, reflection
   helpers, …).

The first match wins; there is no cross-scope overloading. Two glob `import`s at
one scope that introduce the same name are a conflict the programmer resolves by
switching one to a selective binding.

A **qualified** name `a.b.c` resolves `a` by the rules above, then `b` as a
member of `a`, then `c` as a member of `a.b`. Each hop must be visible: crossing
into another namespace sees only its `@public` members. Intrinsics (`$name`)
resolve directly and are never namespace members.

An `@using` field does **not** contribute promoted members to `a.b` field lookup:
`@using` grants only an implicit upcast to the field's type, never name promotion.
The embedded field's members are reached through the field name (e.g. `a.t.x`),
not directly on `a`. Method
resolution is the one place the upcast participates — see below. See
[03-types.md](03-types.md) §3.10.

### Methods and trait methods

`value.method(args)` resolves `method` by:

1. searching the inherent `impl T` namespaces of `value`'s type, then
2. searching trait `impl Trait for T` namespaces for traits that are **in scope**
   (imported or declared) and implemented for that type, then
3. if `value`'s type has an `@using` field of type `U` and neither of the above
   matched, retrying the lookup on `U` through the implicit upcast — `value`
   coerces to `U` (or `*U` for a method receiver) and `method` resolves on `U`.
   Because at most one `@using` field is allowed per struct, this step is
   unambiguous. Fields are still never promoted, so only method calls benefit.

A trait method is only callable where that trait is in scope; this is what makes
the merging exception in §4.3 sound. Ambiguity between two in-scope traits
providing the same method name is resolved by dispatching through the trait
explicitly, e.g. `$cast.<*dyn ToJson>(&x).render()`.

## 4.7 `Self`

Inside a `trait` body and inside an `impl` namespace, `Self` refers to the
implementing/target type. It is used in signatures (`self: *Self`) and return
types, letting one trait definition apply uniformly to every implementor.

## 4.8 Generic impls

A plain `impl` names concrete types, so it implements for exactly one type. To
implement over a **family** of types, the impl declares its own generic parameters
in `< >` immediately after `impl` — the same declaration syntax used by `func <T>`
and generic types (§5.4). Every parameter so declared may then appear in the
target, in the trait before `for`, and throughout the body:

```
// inherent methods for EVERY Storage.<T>, not one specialization
impl <T> Storage.<T> {
  @public
  push :: func (self: *mut Storage.<T>, v: T) { ... }
  @public
  get :: func (self: *Storage.<T>, i: uint) -> Option.<T> { ... }
}

// implement a trait for a whole family
impl <T> ToJson for Storage.<T> {                   // ToJson for every Storage.<T>
  @public render :: func (self: *Storage.<T>) -> string { ... }
}
```

The parameters are bound exactly like function generics — `<T>` is an
unconstrained type parameter, and bounds/constraints (§5.4) are allowed. A **conditional** (bounded) impl only
applies when the parameters satisfy their constraints:

```
// Storage.<T> is ToJson only when its elements are
impl <T: ToJson> ToJson for Storage.<T> {
  @public render :: func (self: *Storage.<T>) -> string { ... }
}
```

Because the trait is itself a type, it too may be generic in the impl's
parameters. This covers both a specialized generic trait and a fully generic one:

```
impl <T> Into.<T> for CatId { ... }          // Into.<T> for CatId, for every T
impl <U> Into.<int32> for Storage.<U> { ... } // Into.<int32> for every Storage.<U>
```

A **blanket** impl leaves the target itself a bare parameter, implementing a trait
for *every* type (optionally constrained):

```
impl <T> Describe for T {                     // Describe for absolutely every T
  @public describe :: func (self: *T) -> string { ... }
}

impl <T: ToJson> Loggable for T { ... }       // Loggable for every T that is ToJson
```

Within any generic impl, `Self` is the (parameterized) target — `Storage.<T>`,
`CatId`, `T` — as written in the header.

### Specialization (most specific wins)

Two impls **overlap** when some concrete type is matched by both — for the same
inherent set, or for the same trait. Overlap is allowed **only when one impl is
strictly more specific than the other**; the compiler then selects the most
specific matching impl at each use site. `impl Storage.<int32>` wins over `impl
<T> Storage.<T>` for `int32`, while `Storage.<string>` still uses the generic one.

Impl **A is more specific than B** when every type A matches is also matched by B,
but not the reverse (A's match set is a strict subset of B's). This orders:

- a concrete instantiation under a generic one — `Storage.<int32>` ⊂ `Storage.<T>`;
- a more-constrained impl under a less-constrained one over the same shape —
  `impl <T: ToJson> ... Storage.<T>` ⊂ `impl <T> ... Storage.<T>`;
- a concrete/structured target under a **blanket** parameter — `Storage.<T>` ⊂ `T`.

```
impl <T> Serialize for T { ... }              // fallback for everything
impl <T> Serialize for Storage.<T> { ... }    // more specific: any Storage
impl Serialize for Storage.<int32> { ... }    // most specific: this one
// Storage.<int32> picks the third, Storage.<bool> the second, int the first.
```

If two overlapping impls are **incomparable** — each matches a type the other does
not, so neither match set contains the other — the overlap is **ambiguous** and a
compile error. Resolve it by adding a constraint that makes them disjoint or by
adding a more-specific impl that covers the shared case. This keeps method
resolution (§4.6) unambiguous: for any concrete type and trait there is always a
single most-specific impl, or a diagnosed error.

Generic impls are monomorphized on demand: each concrete type that uses the impl
gets its own specialized code, exactly as generic functions do (§5.4). Which impl
a call resolves to is settled at monomorphization, after the concrete type is
known.
