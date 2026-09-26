---
title: Namespaces and packages
description: namespace, impl, visibility, import, name resolution, and overload sets.
---

A **namespace** is a named scope containing declarations — the only
scoping-and-grouping construct in the language. Every source file is itself
an anonymous namespace; there's no separate "module" concept and no file
header. Another file's namespace is obtained by `import`ing it, which
yields that file's namespace *value* — the importer binds or destructures
it like any other namespace.

## The two namespace forms

### Inline namespaces

An ordinary `::` binding whose value is a `namespace` block:

```nest
config :: namespace {
    @public SERVER_PORT :: cast.<HttpPort>(8080)
    @public SERVER_ADDR :: "127.0.0.1"
}

@public
http :: namespace { ... }
```

### Impl namespaces

A type's methods and trait implementations are supplied by an **anonymous**
impl namespace introduced by `impl`:

```nest
impl CatImage {   // inherent methods / associated funcs
    @public
    new :: func (id: CatId, url: str, w: i32, h: i32) -> CatImage {
        return .{ id: id, url: url, width: w, height: h }
    }
}

impl ToJson for CatImage {   // trait implementation
    @public
    render :: func (self: *CatImage) -> str { ... }
}
```

`impl T { ... }` adds inherent items to `T`; `impl Trait for T { ... }`
implements `Trait` for `T` (the compiler checks every required method is
present with a matching signature, `Self` resolved to `T`). Because the
target is written in the header rather than bound to a name, you can
implement traits for types declared elsewhere — see
[Traits and impls](../traits/) for coherence rules. A function whose
first parameter is `self` is a **method**; one without is an **associated
function** (`CatImage.new(...)`).

A file, an inline `namespace { ... }`, and an `impl { ... }` body are the
same underlying construct — a namespace value. A file produces one
anonymous namespace (yielded by `import`); an inline namespace binds one to
a name; `impl` attaches one to a type.

## Nesting

Namespaces nest arbitrarily; members are reached with `.`:

```nest
@public
http :: namespace {
    @public Client :: struct {}
    impl Client { ... }
}
```

`http.Client`. If this file is imported as `network :: import "network.nest"`,
it becomes `network.http.Client` — the importer chooses the file's name, the
file never names itself.

## Merging and overload sets

Inline `namespace` declarations reachable at the same fully qualified name
are unioned into one namespace — so there can be no duplicate members: two
functions, consts, or types with the same name at the same path is a
conflict error, as if written in one block.

One name reaching *several* functions is written down explicitly, as an
**overload set**:

```nest
add_i32 :: func (a: i32, b: i32) -> i32 { ... }
add_f64 :: func (a: f64, b: f64) -> f64 { ... }

@public
add :: func { add_i32, add_f64 }
```

The set is an ordinary member — it has a name, a visibility, a place. Its
members keep their own names and remain callable as themselves; the set
adds a name, it doesn't take any away. A call through a set picks one
member by what it passes: argument count, then parameter names, then
argument types.

- Exactly one member must be left — a call no member takes is an error
  listing what the set has; a call two members take is ambiguous.
- A **concrete** signature beats a generic one that would also take the
  arguments.
- Two **generic** members differing only in a bound are told apart by the
  bound; an argument meeting both is ambiguous.
- A return type is never part of the choice — a call is read from its
  arguments inward.

A set is a name for several functions, not a value of its own: it can be
called, but not bound, passed, or stored — name the member for that.

## Visibility and re-export

Every item is **private to its enclosing namespace by default**, and
lexically — visible to its declaring namespace and every namespace nested
inside it, never to the outside.

| Marker | Effect |
|--------|--------|
| *(none)* | private to the enclosing namespace (and its descendants) |
| `@public` | export the item from its namespace |
| `@public(package)` | export to its own **package**, and no further |
| `@public(all)` | (struct/enum) export the type **and** all its fields |
| `@public(fields: L)` | (struct) fields are `L` — `public`, `package`, `private` |
| `@private` | (field) re-hide one field inside an aggregate that opened the rest |

The **package** is the unit `@public(package)` means: every file of one
package, and — for code belonging to no package — the program's own files
between them. `@public` is the only access modifier; there's no
protected/internal tier.

```nest
@public io :: import <std/io>                       // re-export the whole namespace
@public { Client, Router } :: import "network.nest"  // re-export selected items
@public * :: import <prelude>                        // re-export everything public
```

A namespace re-exports by putting `@public` on a `::` binding whose right
side is an `import`.

## `import`

`import` always evaluates to the namespace value of another file or
package. It's not a statement, has no glob token of its own, and must
appear as the right side of a `::` binding — the **pattern on the left**
decides what happens to it:

```nest
foo :: import <std/io>                                       // bind the whole namespace
* :: import <prelude>                                         // glob every public item into scope
{ CatImage, CatId, HttpPort } :: import "models.nest"          // selective
{ http: { Client, Router } }  :: import "network.nest"         // nested selective
{ http: * } :: import <std>                                    // pull member, glob its members
{ self, Json } :: import <std/http>                            // `http` itself, and `Json`
```

`self` in a destructuring binds the namespace itself next to the members it
picks — under its own name (`http` above; a file's name without `.nest`), or
under another with `{ self: h, Json }`.

`import` only ever grants access to `@public` items of the target; private
items are invisible.

| Form | Meaning |
|------|---------|
| `import <name>` | a **package**, resolved by name (`<std>`, `<std/http>`, `<core/c>`). `/` walks into public sub-namespaces. |
| `import "path"` | a **file**, resolved on the filesystem relative to the importing file (`"hello.nest"`, `"./util.nest"`). |

Angle brackets are never a file, quotes are never a package. `import` has
no run-time effect; it's resolved during compilation.

A **cycle between packages** — `alpha` importing `<beta>` while `beta`
imports `<alpha>` — is refused, since neither could be built first. Files
*within* one package are a different question: they may import each other
freely, cyclically or not.

## Name resolution

To resolve an unqualified name, the compiler searches, in order:

1. **Local scope** — `let`/`const`/`::` bindings in the current block,
   innermost first.
2. **Enclosing function parameters and generic parameters.**
3. **Enclosing namespaces** — innermost outward to the file namespace,
   including names brought in by `import` at each level.
4. **The prelude** — the `@public` members of `core.prelude`, globbed into
   every file. Primitive types aren't part of it; they're built in and
   always nameable.

The prelude is small and deliberate: `Option`, `Result`, `ControlFlow`, `str`,
`usize`/`isize`, `Func`, `Sized`, `cast`, `panic`, `assert`, `size_of` — names that appear in signatures and bodies everywhere,
with nothing else globbed. `.?`/`.!` desugar to prelude names, but operator
traits like `Add` are found by `#lang` tag rather than by name, so writing
an `impl` for one is the deliberate act that affords a line of import:

```nest
// no import needed — the prelude
let x: Option.<i32> := .none

// everything else in core is imported like any other package
{ Add } :: import <core/ops>
```

The first match wins; there's no cross-scope overloading. A **qualified**
name `a.b.c` resolves `a`, then `b` as a member of `a`, then `c` as a
member of `a.b` — each hop sees only `@public` members.

### Method resolution

`value.method(args)` resolves by:

1. the inherent `impl T` namespaces of `value`'s type,
2. trait `impl Trait for T` namespaces for traits **in scope**, implemented
   for that type,
3. if `value`'s type has an `@using` field of type `U` and neither matched,
   retrying on `U` through the implicit upcast.

A trait method is only callable where that trait is in scope. Ambiguity
between two in-scope traits providing the same method name is resolved by
naming the trait explicitly: `cast.<*dyn ToJson>(&x).render()`.

## `Self`

Inside a `trait` body and an `impl` namespace, `Self` refers to the
implementing/target type — used in signatures (`self: *Self`) and return
types, so one trait definition applies uniformly to every implementor.
