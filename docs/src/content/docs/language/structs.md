---
title: Structs
description: Record, tuple, and unit structs, field visibility, Default, spreads, and @using.
---

A struct type has three shapes, matching enum-variant syntax:

```nest
CatImage :: struct {               // record struct
  id: CatId,
  url: str,
  width: i32,
  height: i32,
}

Pair   :: struct(i32, i32)         // tuple struct; fields are .0 and .1
Marker :: struct                   // unit struct; a single value `Marker`
```

Struct values are built with a composite literal:

```nest
CatImage { id: ..., url: "...", width: 640, height: 480 }   // named record
Pair(1, 2)                                                    // named tuple
.{ id: ..., url: "...", width: 640, height: 480 }             // inferred record
.{ 1, 2 }                                                      // inferred tuple
```

When the context type is a tuple struct, a positional inferred literal
builds it: `const x: MyType := .{ a, b }` is identical to `MyType(a, b)`.

## Every field, or a spread

A record literal must supply **every** field — there are no field defaults.
`P :: struct { x: i32 := 1 }` is rejected outright, because a default there
would let `P { }` build something a reader can't see from the literal.

What a type gets instead is a `Default` impl plus the `..` spread, which put
the same convenience behind one visible token:

```nest
{ Default } :: import <core/default>

P :: struct { x: i32, y: i32, z: i32 }
impl Default for P {
  default :: func () -> P { return P { x: 0, y: 0, z: 0 } }
}

P { x: 5, ..Default.default() }   // y and z come from the default
P { x: 5, ..base }                // ...or from any other P
.{ x: 5, ..base }                 // and the inferred form spreads too
```

- The spread is last, and at most one — nothing could follow it and mean
  anything.
- It must be a value of the type being built; a different struct with the
  same remaining field names is a type error, not a conversion.
- It's evaluated once, then read once per field it fills.

## Field visibility

Fields are private unless the struct says otherwise:

| Marker | Effect |
|--------|--------|
| *(none)* | private to the enclosing namespace (and its descendants) |
| `@public(all)` | export the type **and** all its fields |
| `@public(fields: L)` | fields are `L` — `public`, `package`, or `private` |
| `@private` (on a field) | re-hide one field inside an aggregate that opened the rest |

```nest
@public(all)
CatImage :: struct {
  id: CatId,
  url: str,
  width: i32,
  height: i32,
  @private cache: Option.<str>,   // exported struct, but this field stays private
}
```

A private field is still reachable from the namespace the struct is
declared in and from namespaces nested inside it — which is where its impls
and neighboring functions live. See
[Namespaces and packages](/language/namespaces/) for the full visibility
picture.

## Named vs. anonymous

A `struct { ... }` written inline in a type position is **anonymous** —
structural, so two anonymous structs with the same fields are the same type.
A struct bound to a name with `::` is **nominal**: its own distinct type
even against an identical anonymous or named twin, with no implicit
conversion either way (`cast`, or an `@using` field, cross the boundary).
Only named structs get `impl` methods; anonymous structs are plain data.

## `@using` fields — implicit upcast

`@using` on a struct field whose type is a struct (or a pointer to one)
marks that field for an implicit upcast — deliberately narrow, unlike Odin's
`using`: it does **not** promote the embedded type's members onto the outer
struct. At most one field per struct may be `@using`.

```nest
Transform :: struct { x: f32, y: f32, angle: f32 }

Entity :: struct {
  @using t: Transform,   // Entity upcasts to Transform
  hp: i32,
}

translate :: func (t: *mut Transform, dx: f32, dy: f32) { ... }
translate(&mut e, 1.0, 0.0)   // &mut Entity coerces to *mut Transform
```

An `Entity` value coerces to `Transform` (a copy of `t`); a `*Entity`/`*mut
Entity` coerces to `*Transform`/`*mut Transform` — the address of the
sub-object, `&e.t`, at zero cost.

## Layout

`#packed`, `#align(N)`, and `#raw` affect struct layout — see
[Directives and attributes](/language/directives/). A record body may also
contain compile-time items, most usefully `comptime_assert(...)`:

```nest
Header :: #packed struct {
  comptime_assert(size_of.<Self>() == 64)
  magic: u32,
  len:   u32,
}
```
