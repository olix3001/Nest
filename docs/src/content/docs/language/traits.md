---
title: Traits and impls
description: Associated types/consts, default methods, blanket impls, coherence, and dyn.
---

A `trait` is a set of method signatures a type can implement. Traits serve
as **static bounds** on generics (monomorphized, no vtable) and, explicitly,
as **dynamic trait objects** via `dyn`.

```nest
ToJson :: trait {
    render :: func (self: *Self) -> str
}

Bounded :: trait {
    MAX: i32             // every impl must supply a value
    MIN: i32 :: 0        // ...unless the trait supplies one
    clamp :: func (self: *Self, n: i32) -> i32
}

impl Bounded for Volume {
    MAX :: 100            // MIN is inherited
    clamp :: func (self: *Volume, n: i32) -> i32 { ... }
}
```

`Self` names the implementing type. A type implements a trait through an
anonymous impl namespace introduced by `impl`: `impl ToJson for CatImage {
... }`. The target is written in the header, so a trait can be implemented
for a type not in the current namespace.

## Associated constants and types

An **associated constant** is `Name: T` — every impl supplies a value of
type `T`. The trait may give it a default with `:: value`, which an impl may
then omit:

```nest
impl Bounded for Volume {
    MAX :: 100   // type already fixed by the trait; MAX: i32 :: 100 means the same
}
```

An **associated type** keeps the `name :: <a type>` shape everywhere in the
language: `Output :: type` declares one, `Output :: Vec3` binds it.

```nest
Iterator :: trait {
    Item :: type
    next :: func (self: *mut Self) -> Option.<Self.Item>
}
```

A trait method may include a body, which becomes the **default
implementation** any impl may omit — the same optionality an associated
constant's `:: value` gives it.

## Static bounds vs. dynamic dispatch

A **static bound**, `func <T: ToJson>(...)`, accepts any `T` implementing
`ToJson` and is monomorphized — no vtable, no indirection.

**Dynamic dispatch** goes through `dyn ToJson`, a trait object. It's
**unsized** — its size is the erased type's, which is exactly what the type
no longer says — so it only ever names a type behind a pointer: `*dyn
ToJson` (or `*mut dyn ToJson`) is a fat pointer, data + vtable. A bare `dyn
ToJson` as a variable's type, a field, or a slice element is an error; a
slice *of pointers*, `[]*dyn ToJson`, is fine, since the pointer is what has
a size.

```nest
const j: *dyn ToJson := &cat        // *T coerces to *dyn Trait when T: Trait
io.println(j.render())              // virtual call

render_all :: func (xs: []*dyn ToJson) { ... }
```

### Pinning associated types

A trait object's type carries only the erased trait's **associated
bindings**, not the full set of trait arguments a static bound would have.
A trait with associated types, like `Func`'s `Args`/`Output` (see
[Closures and Func](../closures/)), is written `dyn T.<Name = Type,
...>` — the same `.<...>` associated-type-equality syntax a bound uses:

```nest
dyn Iterator.<Item = i32>          // pins Iterator's Item
dyn Func(i32) -> i32               // Func's own call-shaped sugar for the same thing
```

### Object safety

A trait can become a trait object only if a vtable could hold it:

| Not object-safe | Why there is no slot for it |
|---|---|
| a method with no `self` receiver | the table is reached *through* the receiver |
| a generic method | one slot can't stand for every instantiation |
| a method taking or returning `Self` by value | `Self`'s size is erased |
| an associated constant | a vtable holds code, not values |

`self: *Self` and `self: *mut Self` are always fine — a pointer is one word
whatever it points at. Each violation is reported **at the coercion**, not
at the trait's declaration, since a trait nobody erases is under no
obligation.

## Blanket and generic impls

An impl declares its own generic parameters in `< >` right after `impl`, to
cover a whole family of types instead of one:

```nest
// inherent methods for EVERY Storage.<T>
impl <T> Storage.<T> {
    @public push :: func (self: *mut Storage.<T>, v: T) { ... }
}

// Storage.<T> is ToJson only when its elements are (a conditional impl)
impl <T: ToJson> ToJson for Storage.<T> {
    @public render :: func (self: *Storage.<T>) -> str { ... }
}
```

The most-specific matching impl is selected per use site. A **blanket**
impl has a bare parameter as its target (never a local type), so it's only
legal for a trait you define — see coherence below.

## Coherence — where an impl may be written

An impl is found by *matching its target*, not by naming it, so two
libraries writing the same impl would clash with no way to prefer either.
Two rules prevent that:

1. **An inherent `impl T { ... }` may only be written in the package that
   defines `T`.** This is why `.len()` is declared in `core` and can't be
   added to `[]T` by anyone else.
2. **A trait impl must have something of its own in it** — either the
   trait or the target belongs to the package writing the impl. `impl
   ForeignTrait for ForeignType` is refused. The target counts as your own
   when one of your types appears *anywhere* in it, even nested: `impl
   FromResidual.<IoError> for Result.<T, MyError>` is legal because
   `MyError` is yours, even though `Result` is `core`'s.

Files reached by `import "path"` are one program and one unit for this
purpose — they implement each other's traits and types freely. The boundary
is the *package*.
