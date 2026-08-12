# 09 — Directives and Attributes

Three annotation-like mechanisms exist, and they do not overlap:

- **Attributes** — `@name` / `@name(args)`. **User-definable** metadata attached
  to a declaration. Attributes never change how code compiles by themselves; they
  annotate items for tooling and for reflection to read back (planned; see
  [12-reflection.md](12-reflection.md)). The built-in visibility attributes
  (`@public`, `@private`, `@using`) are the exception that the compiler acts on
  directly.
- **Directives** — `#name` / `#name(args)`. **Compiler-defined**; they change how
  the compiler treats the item they modify (layout, inlining, dispatch,
  safety). Directives never produce a value.
- **Intrinsics** — `$name(...)`. Compiler-provided **values / operations**, called
  like functions (see
  [06-expressions-and-operators.md](06-expressions-and-operators.md) §6.4).

Rule of thumb: `@` annotates, `#` modifies the construct it precedes, `$`
produces a value. This split is what resolves the "directive as expression"
tension: value-producing compile magic (`$embed_file`, `$size_of`, `$cast`) is an
intrinsic, and a static check inside a struct is the intrinsic statement
`$assert(...)`, not a directive.

## 9.1 Placement

- **Attributes** precede the whole declaration:
  ```
  @public
  @route("/cat")                       // user-defined attribute
  handler :: func () -> Response { ... }
  ```
- **Directives** sit immediately before the construct they modify — a type or
  func literal keyword, a field, or (for `#impl`) a `namespace`:
  ```
  CatImage :: #packed #align(4) struct { ... }
  render   :: #inline func (self: *CatImage) -> string { ... }
  #impl(ToJson, CatImage) namespace { ... }
  data: #raw [4096]uint8,              // directive on a field
  ```

Multiple annotations may stack; order among same-kind annotations is not
significant. Attributes and their arguments are recorded on the declaration for
reflection.

## 9.2 Attributes

### `@public`, `@public(all)`, `@private`

The visibility attributes — the only access control in the language, and the only
attributes the compiler acts on.

- `@public` exports the item from its enclosing namespace.
- `@public(all)` (struct/enum) exports the type **and** every field/variant.
- `@private` (field) re-hides one field inside a `@public(all)` aggregate.

The one further built-in attribute that the compiler acts on is `@using`, which
belongs to the same name-resolution family: on a struct field it grants an
**implicit upcast** from the outer struct to that field's type. It does **not**
promote the field's members onto the outer struct (see
[03-types.md](03-types.md) §3.10). `@using` applies only to struct fields, and
**at most one** field per struct may be `@using`.

```
Entity :: struct {
  @using t: Transform,     // Entity implicitly casts to Transform (no promotion)
  hp: int,
}
```

```
@public CatId :: distinct string

@public(all)
CatImage :: struct {
  id: CatId,
  url: string,
  @private cache: Option.<string>,     // struct is exported; this field is not
}
```

See [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md)
§4.4 for the full visibility model, including `@public` on `import` bindings for
re-export.

### User-defined attributes

Any `@name` that is not a built-in visibility attribute is a **user attribute**:
inert metadata carrying optional arguments, attached to a declaration and later
readable via reflection. They enable data-driven tooling (routers, serializers,
test discovery, doc generators) without compiler changes:

```
@route("/cat", method: "GET")
@deprecated("use get_v2")
get :: func () -> Response { ... }
```

Because attributes do not alter compilation, an unknown attribute is not an error
(subject to tooling policy) — it is simply preserved as metadata.

## 9.3 Directives

Directives modify the construct they precede. The core set (extensible, but this
is what the language model relies on):

### Layout

- **`#packed`** — remove inter-field padding in a struct; fields sit at natural
  byte offsets with no alignment gaps. Changes size and field offsets; matters for
  FFI and wire formats.
- **`#align(N)`** — force the alignment of a struct or field to `N` bytes (`N` a
  power of two). Over-aligns for SIMD-friendly or cache-line layouts.
- **`#soa`** — on a slice/array type, store it **struct-of-arrays**: each field of
  the element type becomes its own contiguous column. Element access presents the
  same `s[i].field` interface; the layout differs. Only valid for record element
  types.

```
Vec3 :: #align(16) struct { x: f32, y: f32, z: f32, _pad: f32 }

Header :: #packed struct { magic: uint32, len: uint32 }

particles: #soa []Particle          // stored column-wise
```

### Code generation

- **`#inline`** — hint that a function be inlined at call sites. Affects codegen
  only, never semantics or visibility; the backend may ignore it.
- **`#const`** — restrict a function to the compile-time-evaluable subset, making
  it usable in constant contexts and at run time (see
  [05-functions-and-generics.md](05-functions-and-generics.md) §5.1). There is no
  `#comptime` directive; `::` already forces compile-time evaluation of its RHS,
  and `#const` marks reusable const-safe functions.

### Storage

- **`#static`** — on a `let` binding, place it in a single **program-lifetime**
  memory region rather than on the stack. At namespace scope this is the only way
  to declare a mutable global (`#static let count: uint := 0`); inside a function
  it makes a local persist across calls. The initializer must be `#const` and may
  be omitted (the region is zeroed, unless the type is `#raw`). `#static let` is
  shared, unsynchronized state — see
  [02-declarations-and-bindings.md](02-declarations-and-bindings.md) §2.6.

### Dispatch and impl

- **`#impl(T)` / `#impl(Trait, T)`** — introduce an anonymous `namespace` of
  inherent items for `T`, or a trait implementation of `Trait` for `T`. The
  target is an argument, allowing impls for out-of-scope types (see
  [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md)
  §4.1).

### Safety (opt-out of default checks)

The language is checked-by-default but is **not** memory-safe like Rust; two
directives trade safety for speed:

- **`#raw`** — on a **type or field**, storage is left **uninitialized** (not
  zeroed) by `$new` / `$make`, and reads are not init-checked. For FFI structs and
  performance-critical buffers.
- **`#unsafe`** — on a **func or block**, disables run-time safety checks in that
  scope: bounds checks, the read-before-write (uninitialized) trap, and null
  checks at C boundaries. Nothing else changes.

```
Scratch :: #raw struct { buf: [4096]uint8 }     // not zeroed on allocation

fast_copy :: #unsafe func (dst: *mut uint8, src: *uint8, n: usize) {
  // no bounds/init checks inside this body
}

hot :: func () {
  #unsafe {
    // scoped escape hatch
  }
}
```

By default, reading a location before it is written is a compile error where
statically provable, otherwise a run-time trap; `#raw` / `#unsafe` remove that
guarantee.

### C interop

- **`extern("abi")`** — a keyword (not a `#`-directive) placed immediately before
  `func`, selecting an ABI / calling convention (currently `"c"`) for external
  declarations and exported symbols. Detailed in [11-c-ffi.md](11-c-ffi.md).

> The layout/codegen/safety directives above are the "basic" set. Deliberately
> out of scope for now: vectorization/SIMD directives and other
> micro-architectural controls.
