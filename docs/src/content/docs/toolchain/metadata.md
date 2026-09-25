---
title: Metadata format
description: The JSON `--emit metadata` writes, field by field, for documentation generators.
---

`nestc --emit metadata[=path]` (or `twig build --emit metadata --emit-only`,
which writes `build/<profile>/obj/lib/<name>/<name>.json`) describes a
package as JSON: every public item a **user** of the package can name,
walked from its root through its public namespaces in declaration order. It
is the input a documentation site is generated from. This page is the
reference for writing such a generator.

The shape is versioned by `format` (currently **2**). A field may be
**added** without a bump; a field that changes meaning or goes away bumps it.
A generator should ignore fields it does not know.

## Top level

```json
{
  "format": 2,
  "package": "std",
  "root": { "kind": "namespace", "name": "std", "path": "std", "members": [ ... ] },
  "impls": [ ... ]
}
```

| Field | Meaning |
|-------|---------|
| `format` | The version of this shape. |
| `package` | The package's name, or `null` for a program. |
| `root` | The package's root namespace, described as an [item](#items). |
| `impls` | Trait [impls](#impls) this package wrote that no described type lists: blanket impls (`impl <T: Default> Fill for T`), impls for primitives, impls for another package's types. Together with each type's own `impls`, this is every trait impl in the package. |

## Items

Every entry in a `members` list is either a **description** or a
**re-export**.

### Re-exports

A name reachable through several public paths is described **once**, where
the walk first meets it, and everywhere else as:

```json
{ "name": "Vec", "path": "std.collections.Vec", "reexport": "std.collections.vec.Vec" }
```

`reexport` is the `path` the full description is under. The walk is in
declaration order, so which path that is stays the same from build to build.

### Fields every description has

| Field | Meaning |
|-------|---------|
| `name` | The name it is bound to at this place. |
| `path` | The public path it is described under: `std.di.Scope`. |
| `kind` | One of the [kinds](#kinds) below. |
| `visibility` | `public`, `package` or `private`. Only `public` items are walked, so this is `public` except on trait members. |
| `defined_at` | The **canonical** path, when it differs from `path`. Present on anything re-exported from a file deeper in the package. |
| `declaration` | The source text of the declaration up to its body or opening brace: `get :: func <T> (self: *mut Self) -> Result.<*T, Error>`. |
| `location` | `{ "package", "file", "line", "column" }`. `file` is relative to the package's directory (the program's, for a program). `line` and `column` are 1-based. |
| `doc` | The item's `@doc`, which is what a `///` comment is, as Markdown. Absent when undocumented. |
| `summary` | The doc's first paragraph, on one line, for index pages and member lists. |
| `attributes` | The other attributes written on it: `[{ "name": "<canonical path>", "args": [{ "name": <string or null>, "value": <value> }] }]`. |
| `directives` | The directives on it: `[{ "name": "inline", "args": [] }]`. An argument is a number, a string, `{ "name": "c" }` for a bare name, or `null` for anything else. |

### Kinds

| `kind` | What it is | Extra fields |
|--------|------------|--------------|
| `namespace` | A namespace, or a file imported as one | `members` |
| `struct` | A struct | `generics`, `fields`, `methods`, `impls` |
| `enum` | An enum | `generics`, `variants`, `methods`, `impls` |
| `trait` | A trait | `generics`, `members`, `methods`, `impls` |
| `type` | A type alias | `expands_to` |
| `distinct` | A `distinct` type | `repr`, `methods`, `impls` |
| `func` | A function or method | see [functions](#functions) |
| `const` | A `::` constant naming a value | `type`, `value` |
| `overload` | An overload set | `functions`: the canonical paths of its members |
| `assoc_type` | A trait's associated type | `bounds`, `required` |
| `assoc_const` | A trait's associated constant | `type`, `value`, `required` |

A namespace's `doc` comes from the namespace itself or, for a file imported
as one, from the `///` on the binding that imports it:

```nest
/// Dependency injection over `core/reflect`.
@public di :: import "di/di.nest"
```

### Fields and variants

`fields` lists **public** fields only. A field visible to the package alone
is not something a reader can name:

```json
{ "name": "x", "type": "i32", "visibility": "public", "doc": "Across.", "summary": "Across." }
```

`variants`:

```json
{ "name": "bad", "payload": [{ "name": null, "type": "i32" }], "value": 7 }
```

`payload` is absent for a bare variant. A payload entry's `name` is `null`
for a positional payload (`.b(T)`) and the member's name for a record one.
`value`, the discriminant, is present only when the enum's discriminants are
not just the variants' positions, meaning the program wrote some.

### Functions

| Field | Meaning |
|-------|---------|
| `method` | Whether it takes `self`. |
| `receiver` | The type of `self`, for a method: `*mut std.di.Services`. |
| `generics` | [Generic parameters](#generics). |
| `self_bounds` | `[{ "on": "Self", "bound": "core.ops.Sized" }, { "on": "Self.Item", "bound": "core.cmp.Ord" }]`: the `Self` bounds a trait method wrote. |
| `params` | The value parameters, `self` excluded: `[{ "name", "type", "default": <bool> }]`. |
| `returns` | The return type. Absent for `void`. |
| `required` | On a trait's method only: `true` when it has no body (every impl writes it), `false` when it is a default an impl may inherit. |

### Generics

```json
[
  { "name": "T" },
  { "name": "N", "const": true },
  { "name": "F", "bounds": ["core.ops.Func.<Args = (*mut std.di.Scope), Output = T>"] }
]
```

A bound is written the way a type is: the trait's canonical path, then its
arguments and pinned associated types, the pins sorted by name.

### Methods and impls

`methods` lists the public methods of the type's inherent impls, each
described as a [function](#functions). `impls` lists the traits the type
implements, **including impls other packages wrote for it**. Each entry
carries the `package` that wrote it:

```json
{
  "trait": "core.iter.FromIterator",
  "for": "std.collections.vec.Vec.<T>",
  "generics": [{ "name": "T" }],
  "assoc": { "Item": "T" },
  "package": "std",
  "location": { "package": "std", "file": "collections/vec.nest", "line": 185, "column": 27 }
}
```

`trait_args` (the trait's own arguments, `impl Add.<f64> for V`) and
`assoc` are absent when empty. A trait's implementors are
every impl, in this package's metadata and in its dependents', whose `trait`
is that trait's canonical path.

## Types and links

Types are strings, written with **canonical** paths:
`core.types.result.Result.<*T, std.di.Error>`. To turn one into links, map
each dotted path in it to the page that describes it. Every description's
canonical path is its `defined_at`, or its `path` when there is no
`defined_at`. The metadata of each dependency (`core`, `std`, …) answers for
its own items.
