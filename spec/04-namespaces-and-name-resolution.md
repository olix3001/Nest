# 04 — Namespaces and Name Resolution

A **namespace** is a named scope containing declarations. It is the only
scoping-and-grouping construct in the language. **Every source file is itself a
namespace** — there is no separate "module" concept and no `module` keyword. A
file's top-level `namespace name` header only *names* the namespace the file
contributes to; the grouping mechanism is uniform from a whole file down to a
trait implementation.

## 4.1 The three namespace forms

### 1. File namespaces and child namespaces

A bare `namespace name` statement at the top of a file declares which namespace
the file's contents belong to. The same statement, referring to a name with no
in-file body, tells the compiler to **load a child namespace from the file
system**:

```
namespace network         // this file's items belong to `network`
```

```
namespace models          // pull in child namespace `models` from a file
```

When such a declaration has no body, the compiler resolves it to a source file by
convention:

```
namespace network         // looks for  src/network(.ext)  or  src/network/mod(.ext)
```

A file that itself begins with `namespace network` *is* the body for the
`namespace network` declaration made in a parent file. A project is assembled
from files this way, with no include mechanism.

### 2. Inline namespaces (bound with `::`)

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

### 3. Impl namespaces (the `#impl` directive)

A type's methods and trait implementations are supplied by an **anonymous** impl
namespace introduced by the `#impl` directive. The target type — and, for a trait
impl, the trait — are passed as **arguments**, so the namespace is not bound to a
name and the target need not live in the current namespace:

```
#impl(CatImage) namespace {                 // inherent methods / associated funcs
  @public
  new :: func (id: CatId, url: string, w: int32, h: int32) -> CatImage {
    return .{ id: id, url: url, width: w, height: h }
  }
}

#impl(ToJson, CatImage) namespace {         // trait implementation of ToJson
  @public
  render :: func (self: *CatImage) -> string { ... }
}
```

- `#impl(T) namespace { ... }` adds inherent items to type `T`.
- `#impl(Trait, T) namespace { ... }` implements `Trait` for `T`; the compiler
  checks every required method is present with a matching signature (`Self`
  resolved to `T`).
- Because the target is an argument, you can implement traits for types declared
  elsewhere (subject to visibility). This also rules out the meaningless forms an
  earlier draft allowed, such as binding an impl to an imported value.

A function whose first parameter is `self: *T` / `self: *mut T` / `self: T` is a
**method** (`value.method(args)`); one without `self` is an **associated
function** (`Type.func(args)`, e.g. `CatImage.new(...)`). See
[05-functions-and-generics.md](05-functions-and-generics.md).

> **Unification note.** Forms 1–3 are the same construct — a `namespace` value.
> Form 1 is produced from a file, form 2 is bound to a name with `::`, form 3 is
> attached to a type by the `#impl` directive.

## 4.2 Nesting

Namespaces nest arbitrarily; members are reached with `.`:

```
namespace network

@public
http :: namespace {
  @public Response :: enum { ok(string), redirect(string), not_found, ... }
  @public Client   :: struct {}
  #impl(Client) namespace { ... }
  @public Router   :: struct { logging: bool }
  #impl(Router) namespace { ... }
}
```

`network.http.Client`, `http.Router`, `config.SERVER_PORT`.

## 4.3 Namespace merging (same-name unification)

If several `namespace` declarations across a project share the same fully
qualified name, they are treated as **one namespace**: their members are unioned.
Consequently there can be **no duplicate members** — two functions, consts, or
types with the same name in same-named namespaces is a conflict error, exactly as
if they were written in one block.

The **sole exception** is impl namespaces. Any number of `#impl(...)` blocks may
target the same type, and different trait impls may each define a method of the
same name (e.g. two traits both requiring `render`). This is sound because a
trait's methods are only reachable when that trait is in scope (imported) or
accessed through an explicit `dyn`/`$cast` (see §4.5). Inherent-method conflicts
across `#impl(T)` blocks are still errors.

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

A namespace re-exports by putting `@public` on a `::` binding that names an
imported item, or on a bare glob `import` (see §4.5):

```
@public io :: import "std/io"                    // re-export the whole namespace
@public { Client, Router } :: import "network"   // re-export selected items
@public import "prelude"                          // re-export everything public in "prelude"
```

`@public` is the only access modifier; there are no protected/internal tiers.
See [09-directives-and-attributes.md](09-directives-and-attributes.md).

## 4.5 `import`

`import` is a compile-time expression evaluating to the **namespace value** of
another file/namespace. Whether it binds a name, destructures, or glob-imports is
determined by the surrounding form — there is no glob token:

```
import "network"                       // GLOB: bring every public item into scope
foo :: import "std/io"                 // bind the whole namespace as `foo`
{ CatImage, CatId, HttpPort } :: import "models"          // selective
{ http: { Client, Router, Response } } :: import "network" // nested selective
```

- A bare `import "x"` statement (no `pattern ::`) glob-imports all of `x`'s
  public items into the current scope.
- `name :: import "x"` binds the whole namespace to `name`.
- `{ ... } :: import "x"` destructures selected public members (optionally through
  nested public namespaces; see
  [07-patterns-and-matching.md](07-patterns-and-matching.md)).
- Prefix any of these with `@public` to additionally re-export what it brings in.
- `import` only ever grants access to `@public` items of the target; private
  items are invisible.

The string operand is an **import path**: `"std/io"` names a standard-library
namespace, `"network"` / `"models"` name project namespaces. Path resolution
(standard library vs. project root vs. relative) is layered on top of this
syntax. `import` has no run-time effect; it is resolved during compilation.

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

1. searching the inherent `#impl(T)` namespaces of `value`'s type, then
2. searching trait `#impl(Trait, T)` namespaces for traits that are **in scope**
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

Inside a `trait` body and inside an `#impl` namespace, `Self` refers to the
implementing/target type. It is used in signatures (`self: *Self`) and return
types, letting one trait definition apply uniformly to every implementor.
