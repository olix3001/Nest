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
- **Intrinsics** — ordinary functions declared in `core` with no body, marked
  `#intrinsic`; the compiler supplies the operation (see
  [06-expressions-and-operators.md](06-expressions-and-operators.md) §6.4).

Rule of thumb: `@` annotates, `#` modifies the construct it precedes, and a
**value comes from a function** — never from a directive. This is what resolves
the "directive as expression" tension: value-producing compile magic
(`embed_file`, `size_of`, `cast`) is a function core declares and the compiler
fills in, and a static check inside a struct is the statement `assert(...)`, not
a directive.

## 9.1 Placement

- **Attributes** precede the whole declaration:
  ```
  @public
  @route("/cat")                       // user-defined attribute
  handler :: func () -> Response { ... }
  ```
- **Directives** sit immediately before the construct they modify — a type or
  func literal keyword, or a field:
  ```
  CatImage :: #packed #align(4) struct { ... }
  render   :: #inline func (self: *CatImage) -> str { ... }
  data: #raw [4096]uint8,              // directive on a field
  ```
  Implementations are **not** a directive: they use the `impl` keyword
  (`impl Trait for T { ... }`), see
  [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md).

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

The remaining compiler-acted attribute is `@link_name(str)`, which overrides
the external link symbol of an `extern` declaration: the binding keeps its
in-language name while the compiler emits/links against the string. It is only
meaningful on `extern` functions — see [11-c-ffi.md](11-c-ffi.md) §11.3.

```
@public CatId :: distinct str

@public(all)
CatImage :: struct {
  id: CatId,
  url: str,
  @private cache: Option.<str>,     // struct is exported; this field is not
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

- **`#static`** — on a `::` binding, make it a single **program-lifetime**
  memory region rather than a constant. The RHS is then read as the region's
  **type**, with the initial contents after `:=`
  (`#static count :: uint := 0`); the type is required, and the initializer may
  be omitted, in which case the region is zeroed (unless the type is `#raw`).
  At namespace scope this is the only way to declare a mutable global; inside a
  function the same form makes a local persist across calls. The initializer
  must be a constant expression, and reading a static is a run-time operation,
  so a static may not appear in one. `#static` is shared, unsynchronized state —
  see [02-declarations-and-bindings.md](02-declarations-and-bindings.md) §2.6.

- **`#section("name")`** — on a **function or constant**, place the symbol it
  becomes in the named object-file section rather than the default one. What the
  names mean is the target's business, not the language's (`.text`, `.rodata`,
  `.init_array`, a linker script's own).

- **`#offset(N)`** — on a **function or constant**, fix the symbol at position
  `N` in the generated binary. For interrupt vectors, boot headers, and anything
  else a piece of hardware or a loader expects at a known address.

Both apply only to the things that *become symbols*. On a local, a field or a
type there is no symbol for them to describe, and they are rejected rather than
ignored:

```
@public
#section(".init_array")
register :: func () { ... }

#offset(0x0000)
RESET_VECTOR :: reset_handler
```

### Language items (`#lang`)

- **`#lang("tag")`** — mark the item it precedes as a **language item**: a
  core-library declaration the compiler must be able to find by a well-known name
  in order to wire built-in syntax to it. `#lang` changes dispatch (which trait a
  desugaring targets), so it is a directive, not an attribute; like every
  directive it produces no value and only annotates the construct it modifies.

  The `tag` is drawn from a **fixed vocabulary the compiler defines** (unlike
  user attributes, which accept any name). It marks a `trait`, an `enum`, a
  `struct`, or a `func` — whatever the compiler needs to reach. Each tag names
  exactly one item program-wide: a `tag` the compiler does not recognize, a `tag`
  applied twice, and a `tag` on the wrong kind of item are all errors.

  ```
  Add :: #lang("add") trait <Rhs> {
    Output :: type
    add :: func (self: Self, rhs: Rhs) -> Self.Output
  }

  Ordering :: #lang("ordering") enum { less, equal, greater }
  ```

`#lang` is what lets the language define its operators, `.?` / `.!`, and `for`
in the core library instead of hard-wiring them: the operator `+` lowers to a
call to whatever trait carries `#lang("add")`, `.?` uses `#lang("try")`, and
`for` uses `#lang("iterator")`. The full registry and the desugaring rules are in
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.13; the
operator/method mapping lives there so it sits beside the operators it drives.

Ordinary user code never writes `#lang` — the tags belong to the core library the
compiler is built against. It is listed here because it is a compiler directive,
but its effect is described where operators are (§6.13).

### Compiler-supplied bodies (`#intrinsic`)

- **`#intrinsic`** — mark a **bodyless** function whose body the compiler
  supplies: an instruction, a constant, or nothing at all. It is how `core`
  declares `cast`, `size_of`, `panic`, `wrapping_add` and the rest (§6.4).

  ```nest
  @public size_of :: #intrinsic func <T> () -> usize

  impl <const N: usize, const S: bool> int.<N, S> {
    wrapping_add :: #intrinsic func (self: Self, rhs: Self) -> Self
  }
  ```

  It is the mirror of `#lang`, and the pair is easiest to keep straight by the
  **direction** each points:

  | Directive | Direction | Meaning |
  |---|---|---|
  | `#lang("add")` | compiler → core | "find the item with this tag and wire syntax to it" |
  | `#intrinsic` | core → compiler | "this declaration has no body; you supply it" |

  The directive takes an optional **tag**: `#intrinsic("size_of")` says *which*
  intrinsic the declaration is, and a bare `#intrinsic` means "the tag is the
  declared name". The tag, never the name or the path, is the identity — for the
  same reason `#lang`'s is (§9.3). A compiler that keyed on `core.mem.size_of`
  would turn renaming a library declaration into a compiler change, and `core` is
  meant to be replaceable. The two forms mean the same thing for every
  declaration whose name already matches its tag, which is all of them today;
  writing the tag is what makes a rename possible later.

  A function marked `#intrinsic` **must** have no body, and the compiler must
  recognize it — an `#intrinsic` the compiler has never heard of is an error at
  the declaration, not a link failure later. Conversely a bodyless function that
  is neither `#intrinsic`, `extern`, nor a trait requirement is an error: those
  three are the only ways a signature stands without an implementation.

  Ordinary user code never writes `#intrinsic`, for the same reason it never
  writes `#lang`: the set is the compiler's, and a program that could declare its
  own would be asking for a body no compiler knows how to fill.

### Source positions (`#caller_location`)

- **`#caller_location`** — an **expression**, not a decoration: it evaluates to a
  `Location` describing the **call site**, and it is legal only as a default
  argument. See §5.2, which covers the rule and the reason for it. It is spelled
  with a `#` rather than as a name so that it cannot be shadowed, re-exported or
  passed around; the only place it means anything is the one position that gives
  it a meaning.

### Implementations (not a directive)

Implementations were once the `#impl(...)` directive; they are now the **`impl`
keyword**. `impl T { ... }` adds inherent items to `T`; `impl Trait for T { ... }`
implements `Trait` for `T`; `impl <T> ...` parameterizes over a family with
most-specific-wins selection. The target is in the header, allowing impls for
out-of-scope types. See
[04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) §4.1, §4.8.

### Safety (opt-out of default checks)

The language is checked-by-default but is **not** memory-safe like Rust; two
directives trade safety for speed:

- **`#raw`** — on a **type or field**, storage is left **uninitialized** (not
  zeroed) by `new` / `make`, and reads are not init-checked. For FFI structs and
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
