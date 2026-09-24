---
title: Enums
description: Sum types, tuple/record payloads, and explicit discriminants.
---

```nest
Response :: enum {
  ok(str),
  redirect(str),
  bad_request(str),
  not_found,
  internal_server_error(str),
}

Shape :: enum {
  circle { radius: f64 },
  rect   { w: f64, h: f64 },
  point,
}
```

Variant names are **snake_case**. A variant may be bare, carry a positional
(tuple) payload, or carry a named (record) payload. Variants are constructed
with a leading `.`, the enum type inferred from context:

```nest
return Response.redirect(cat.url)   // fully qualified
return .redirect(cat.url)           // inferred, in a Response context
return .rect { w: 3.0, h: 4.0 }     // record-payload variant
```

Enums are consumed by `.match` — see [Control flow](/language/control-flow/).
`Result` and `Option` are ordinary enums provided by the prelude (see
[Errors](/language/errors/) and [Types](/language/types/)).

## Explicit discriminants

Every enum value stores a discriminant — the number saying which variant it
holds. By default it's the variant's position, counting from zero; a
variant may state one instead:

```nest
Errno :: enum {
  ok    = 0,
  perm  = 1,
  noent = 2,
  io    = 5,
  again = 11,
}

Signed :: enum {
  invalid = -1,
  ready,   // 0
  done,    // 1
}

Bits :: enum {
  read  = 1 << 0,
  write = 1 << 1,
  exec  = 1 << 2,
}
```

- The value is a constant expression: a literal, a named `::` constant, or
  arithmetic over them, evaluated at compile time exactly as an array
  length is.
- A variant with no `= value` takes one more than the variant before it,
  the first being zero — so `a = 3, b` makes `b` four.
- Two variants may not share a discriminant.
- Only a variant with **no payload** may be given one — a discriminant
  exists to give an enum a C enumeration's numbering, and a C enumeration
  has no payload to number.

The tag's type follows from the discriminants: the narrowest integer that
holds every one of them, signed exactly when some variant's is negative. To
fix the tag at the type a C enumeration has instead, write `#repr("C")` (see
[Directives and attributes](/language/directives/)).
