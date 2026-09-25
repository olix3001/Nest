---
title: Directives and attributes
description: "@attributes, #directives, #when, layout, safety, #lang, and #intrinsic."
---

Three annotation-like mechanisms exist, and they don't overlap:

- **Attributes** — `@name` / `@name(args)`. User-definable metadata attached
  to a declaration; never changes how code compiles by itself, except the
  built-in visibility attributes (`@public`, `@private`, `@using`), which the
  compiler acts on directly.
- **Directives** — `#name` / `#name(args)`. Compiler-defined; changes how
  the compiler treats the item they modify (layout, inlining, dispatch,
  safety). Directives never produce a value.
- **Intrinsics** — ordinary functions declared in `core` with no body,
  marked `#intrinsic`; the compiler supplies the operation (see
  [Memory](../memory/) and [Operators](../operators/)).

Rule of thumb: `@` annotates, `#` modifies the construct it precedes, and a
**value comes from a function**, never from a directive.

## Placement

Attributes precede the whole declaration; directives sit immediately before
the construct they modify — a type or `func` keyword, or a field:

```nest
@public
@route("/cat")                       // user-defined attribute
handler :: func () -> Response { ... }

CatImage :: #packed #align(4) struct { ... }
render   :: #inline func (self: *CatImage) -> str { ... }
data: #raw [4096]u8,                 // directive on a field
```

**Attributes come before directives** — `@public #when(os = .Windows) Handle
:: ...`, not the reverse; the parser reads the `@` run first and doesn't
backtrack to pick up an attribute after a `#` one.

## `@public`, `@public(...)`, `@private`

See [Namespaces and packages](../namespaces/) for the full table and
[Structs](../structs/) for field visibility.

## `@test`

`@test` marks a function as a test — see [Testing](../testing/).

## `@doc` and `///`

`///` lines above a declaration — an item, a field, an enum variant, a trait
member — are its documentation. They are sugar for one attribute, `@doc`,
whose text is the lines without their slashes:

```nest
/// A point in the plane.
///
/// Both coordinates are in pixels.
@public(all)
Point :: struct {
    /// Across.
    x: i32,
    y: i32,
}

@doc("The same, written out.")
origin :: func () -> Point { return Point { x: 0, y: 0 } }
```

`@doc` is `core`'s (`#lang("doc")`, re-exported from `<core/reflect>`), and
`///` means it in every file whether or not the name `doc` is in scope. A
plain `//` line between the doc and the item ends the doc; `////` is a plain
comment. Inside a function body a `///` is only a comment. The language
server shows the text on hover, `nestc --emit metadata` writes it out (see
[the toolchain](../../toolchain/)), and reflection reads it like any attribute:
`attr_of.<doc>(type_info.<Point>().attrs)`.

## Conditional compilation — `#when`

`#when(condition)` on any declaration compiles it only when `condition`
holds. An excluded declaration is **removed before names are resolved** —
it declares nothing, so naming it is an ordinary unresolved name.

A condition isn't a Nest expression (it's read before name resolution): a
small closed language over what the **build** is.

- **`test`** — true in a test build (`nestc --test`) of the package the
  declaration belongs to.
- **`os = .Variant`** — the target OS: `.Linux`, `.Macos`, `.Windows`,
  `.Freebsd`, `.Bare` (a freestanding target: `-C os=none`).
- **`arch = .Variant`** — the target architecture: `.X86_64`, `.Aarch64`,
  `.Riscv64`, `.Wasm32`.
- **`profile = .Variant`** — the build profile: `.Debug`, `.Release`.
- **`all(...)`**, **`any(...)`**, **`not(...)`** — combinators over the
  above.

```nest
tests :: #when(test) namespace {
    @test
    adds :: func () { assert(add(2, 3) == 5) }
}

#when(all(arch = .X86_64, not(os = .Windows)))
sysv_only :: func () { ... }

#when(any(os = .Macos, os = .Linux))
posix :: namespace { ... }
```

A key the compiler doesn't know, or a variant outside its enum, is an
**error** — not a condition that's quietly false.

## Layout

- **`#packed`** — remove inter-field padding in a struct; fields sit at
  natural byte offsets with no alignment gaps.
- **`#align(N)`** — force the alignment of a struct or field to `N` bytes
  (a power of two).
- **`#soa`** — on a slice/array type, store it struct-of-arrays: each field
  of the element type becomes its own contiguous column, while
  `s[i].field` access stays the same. Only valid for record element types.
- **`#repr("C")`** — on a `struct`, `enum` or `distinct` type, guarantee the type's
  representation is what a C declaration of it would have. On a struct this
  is a promise (Nest already lays fields out in declaration order at
  natural alignment); on an enum it changes the tag to C's `int`, whatever
  the discriminants would otherwise have fit in. On a `distinct`, it promises
  that what the type is distinct from is C's — an enum underneath must itself
  be `#repr("C")`.

```nest
Vec3   :: #align(16) struct { x: f32, y: f32, z: f32, _pad: f32 }
Header :: #packed struct { magic: u32, len: u32 }
Errno  :: #repr("C") enum { ok = 0, perm = 1, noent = 2 }   // tag is a C int
particles: #soa []Particle          // stored column-wise
```

## Code generation

- **`#inline`** — hint that a function be inlined at call sites. Codegen
  only; the backend may ignore it.
- **`#const`** — restrict a function to the compile-time-evaluable subset
  (see [Functions](../functions/)).

## Storage

- **`#static`** — a program-lifetime memory region rather than a constant
  (see [Bindings](../bindings/)).
- **`#section("name")`** — on a function or constant, place its symbol in
  the named object-file section.
- **`#offset(N)`** — on a function or constant, fix the symbol at position
  `N` in the generated binary — interrupt vectors, boot headers, anything a
  loader expects at a known address.

Both apply only to things that *become symbols*; on a local, field, or
type — which have no symbol — they're rejected rather than ignored.

## Safety — opt out of default checks

Nest is checked-by-default, but not memory-safe like Rust; two directives
trade safety for speed:

- **`#raw`** — on a type or field, storage is left **uninitialized** (not
  zeroed) by `new`/`make`, and reads aren't init-checked.
- **`#unsafe`** — on a `func` or block, disables run-time safety checks in
  that scope: bounds checks, division-by-zero, integer overflow and
  shift-amount traps, the read-before-write trap, and null checks at C
  boundaries.

```nest
Scratch :: #raw struct { buf: [4096]u8 }   // not zeroed on allocation

fast_copy :: #unsafe func (dst: *mut u8, src: *u8, n: usize) {
    // no bounds, init, division or overflow checks inside this body
}
```

:::note
`#unsafe { ... }` and `#when(...)` written directly on a statement or a
bare block (rather than on a whole `func` or `::`-bound item) are specified
but **not yet accepted by the parser** — only a `func` or a `::`-bound item
may carry `#unsafe` today. The lowerer already understands the scope; only
the surface grammar is missing.
:::

`#unsafe` isn't the same lever as `-C overflow=wrap`: that setting changes
what leaving the width *means*, everywhere, for the whole build; `#unsafe`
changes nothing about meaning, it only says the check isn't worth paying
for here. An operation with a stated behavior — `wrapping_add`,
`checked_add`, `saturating_add` — still means exactly what it says inside
an `#unsafe` body.

## `#lang` — language items

`#lang("tag")` marks a core-library declaration the compiler must find by a
well-known name to wire built-in syntax to it — `+` lowers to whatever
trait carries `#lang("add")`, `.?` uses `#lang("try")`, `for` uses
`#lang("iterator")`. The tag is drawn from a fixed vocabulary the compiler
defines; a `tag` it doesn't recognize, applied twice, or on the wrong kind
of item, is an error.

```nest
Add :: #lang("add") trait <Rhs> {
    Output :: type
    add :: func (self: Self, rhs: Rhs) -> Self.Output
}
```

A tag `core` claims is a default the program may answer over — claimed
once inside `core` and once outside it, the outside claim wins with no
diagnostic. This is what lets a program replace the panic handler:

```nest
#lang("panic_handler")
my_handler :: func (msg: str, loc: Location) -> never { ... }
```

See [Operators](../operators/) for the full `#lang` registry.

## `#intrinsic` — compiler-supplied bodies

`#intrinsic` marks a bodyless function whose body the compiler supplies —
an instruction, a constant, or nothing at all. It's the mirror of `#lang`:

| Directive | Direction | Meaning |
|---|---|---|
| `#lang("add")` | compiler → core | "find the item with this tag and wire syntax to it" |
| `#intrinsic` | core → compiler | "this declaration has no body; you supply it" |

```nest
@public size_of :: #intrinsic func <T> () -> usize

impl <const N: u16> int.<N> {
    wrapping_add :: #intrinsic func (self: Self, rhs: Self) -> Self
}
```

A bare `#intrinsic` means "the tag is the declared name"; `#intrinsic("size_of")`
states the tag explicitly. Ordinary user code never writes `#lang` or
`#intrinsic` — both vocabularies belong to the compiler and the library it
ships with.

## `#caller_location`

An expression, not a decoration — evaluates to a `Location` describing the
call site, legal only as a default argument. See
[Functions](../functions/#default-values).

## `#callconv` and `extern("c")`

`#callconv("name")` sets the calling convention (register/stack protocol);
`extern("abi")` marks an external symbol and selects its ABI's types. See
[C FFI](../c-ffi/).

## `#c_vararg`

Marks an `extern("c")` declaration as a C variadic function. See
[C FFI](../c-ffi/).

## Not a directive

`impl T { ... }` / `impl Trait for T { ... }` were once the `#impl(...)`
directive; they're now the `impl` keyword — see
[Traits and impls](../traits/).
