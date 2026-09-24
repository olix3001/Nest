---
title: C FFI
description: core/c, C pointers, extern("c"), crossing the boundary, and #c_vararg.
---

C interop is a first-class concern. Compiling to LLVM, Nest shares C's
object model closely enough that calling into and out of C is direct: no
marshalling layer, and C types cross with no conversion, since most of them
are just aliases for Nest's own.

## The `core/c` namespace

```nest
c :: import <core/c>

c.char   c.schar  c.uchar
c.short  c.ushort c.int    c.uint   c.long  c.ulong  c.longlong c.ulonglong
c.float  c.double
c.size_t c.ssize_t c.uintptr_t c.intptr_t
c.cstr                     // c.ptr.<c.char> — a pointer to NUL-terminated bytes
c.void                     // the unit type — what a C function returning void returns
c.anyopaque                // an alias for `opaque` — the pointee of a void*
c.ptr.<T>                  // a raw, nullable, non-GC C pointer to T
```

**These are aliases, not `distinct` types**: `c.int` is another name for
this target's `i32`, so a language value goes straight into a C call and
comes straight back out with no conversion inserted at either end. `c.long`
is a target-specific alias `nestc` generates (32-bit on Windows, 64-bit
elsewhere) for the same reason — what a C `long` *is* varies by target, and
the target already decides it.

`c.ptr.<T>` is the one exception, and is its own type (below): a language
`*T` is non-null and traced, a C pointer is neither, and erasing that
difference is exactly how a null would reach code written on the promise
that it can't see one.

There's no `c.bool` and no C function-pointer type constructor — a C
callback is written `*extern("c") func(...)`, Nest's own function-pointer
syntax, spelled with the C ABI.

### `c.void` and `c.anyopaque`

C spells two different things `void`, and Nest spells them apart.

A C function that *returns* `void` returns nothing — Nest's unit type.
`c.void` is that type, under the name a C programmer looks for:

```nest
free :: extern("c") func (p: *c.anyopaque) -> c.void
```

A C `void *`, and every handle a C library hands back without publishing
the struct behind it — `FILE`, `sqlite3`, an `SDL_Window` — is a pointer to
something the program doesn't describe. `c.anyopaque` is the name for that
pointee, an alias for the `opaque` primitive, which has no size and no
values:

```nest
fopen  :: extern("c") func (path: c.cstr, mode: c.cstr) -> *c.anyopaque
fclose :: extern("c") func (f: *c.anyopaque) -> c.int
```

`opaque`'s rules hold unchanged: it's a type only behind a pointer, nothing
reads through it, and `*T` ↔ `*c.anyopaque` is an explicit `cast` in both
directions. A program can't accidentally acquire a `c.anyopaque` by value,
because there's no such value — the difference from `c.void`, which has
one.

A binding that wants each handle kept apart — so a `*Sqlite` can't be
passed where a `*SDL_Window` is expected — declares its own nominal handle,
a `distinct` over the same type:

```nest
Sqlite :: distinct opaque
sqlite3_close :: extern("c") func (db: *Sqlite) -> c.int
```

Both spellings compile to the same pointer; the difference is entirely
what the type checker will let the program confuse with what.

## C pointers — `c.ptr.<T>`

Nest's `*T`/`*mut T` are GC-managed and never null; C pointers are
different and quarantined to `core/c`:

- `c.ptr.<T>` is a **raw** pointer: untracked by the collector, and
  **nullable**. Its null value is `c.null.<T>()`.
- A language `*T`/`*mut T` becomes a `c.ptr.<T>` through `c.from_ptr(p)`,
  written explicitly at every call site — there's no implicit coercion at
  the C boundary. `from_ptr` is always sound (a `*T` is always a valid
  address) and always a loss of information.
- Going back — `c.ptr.<T>` to a language `*T` — is `p.to_ptr()` (or
  `p.to_mut()` for `*mut T`), and is **checked**: it panics if `p` is null.

```nest
const p: c.ptr.<c.char> := c.null.<c.char>()   // a null C pointer
if p.is_null() { ... }

const gp: *mut Buffer := &mut buf
some_c_func(c.from_ptr(gp))   // *mut Buffer -> c.ptr.<Buffer>, explicit
```

## Declaring external functions — `extern("c")`

```nest
strlen :: extern("c") func (s: c.ptr.<c.char>) -> c.size_t
malloc :: extern("c") func (n: c.size_t) -> *c.anyopaque
qsort  :: extern("c") func (base: *c.anyopaque, n: c.size_t, size: c.size_t,
                            cmp: *extern("c") func (*c.anyopaque, *c.anyopaque) -> c.int)
```

A callback parameter like `cmp` is `*extern("c") func(...)` — a different
type from a Nest `*func(...)` of the same signature, since the two
conventions pass an aggregate differently; neither converts to the other.

- `extern("c")` selects the C ABI/calling convention *and* marks the
  binding as an external symbol resolved at link time. It's a keyword, not
  a `#`-directive, and always sits immediately before `func`.
- A signature may use `c.*` types or ordinary language types with the
  matching representation — an `i32` parameter and a `c.int` one compile to
  the same thing.
- The **link symbol** defaults to the binding's own name; `@link_name(str)`
  binds a differently-named external symbol:

  ```nest
  @link_name("LLVMSomeFunction")
  some_function :: extern("c") func (m: c.ptr.<Module>) -> c.int
  ```

- The **calling convention** is C's unless `#callconv("name")` says
  otherwise — Win32's `__stdcall` is where that shows:

  ```nest
  @link_name("MessageBoxW")
  message_box :: #callconv("stdcall") extern("c") func (
    owner: *c.anyopaque, text: c.ptr.<u16>, caption: c.ptr.<u16>, flags: c.uint
  ) -> c.int
  ```

Externals sharing one ABI may be grouped in an `extern("c") { ... }` block —
pure surface sugar that desugars to the same per-function bindings:

```nest
extern("c") {
    strlen :: func (s: c.ptr.<c.char>) -> c.size_t
    malloc :: func (n: c.size_t) -> *c.anyopaque
}
```

## Crossing the boundary

There's **no implicit coercion** at the C boundary, either direction:

- Scalar `core/c` types are aliases, so a language value of the matching
  type crosses with no conversion at all — `c.int` *is* `i32`.
- A pointer is **never** passed directly: a `*T`/`*mut T` crosses through
  `c.from_ptr(p)`, and a `c.ptr.<T>` coming back is read with
  `.to_ptr()`/`.to_mut()`. There's no `[]T` or `str` parameter form at the
  boundary — a caller passing a slice or string writes out the pointer and
  length (or NUL-terminated bytes via `c.to_cstr`/`c.from_cstr`) itself.
- Aggregates cross by the shape their type declares — `#repr("C")` is what
  makes that shape match a C declaration's.

The one place the compiler inserts a conversion is a `#c_vararg`
function's variadic tail: each argument past the fixed parameters is
widened to C's own default argument promotion (narrower-than-`int` to
`int`, `float` to `double`) — the shape `va_arg` will read on the C side,
not a convenience for the caller. See
[Directives and attributes](/language/directives/) for `#c_vararg`.

## Exporting to C

A language function is exposed to C callers by defining it as an
`extern("c")` func **with a body**:

```nest
@public
add :: extern("c") func (a: c.int, b: c.int) -> c.int {
    return a + b
}
```

Aggregates crossing the boundary should be `#repr("C")`, which on an
`enum` is also what makes the tag a C `int`:

```nest
// enum SDL_EventType { SDL_QUIT = 0x100, SDL_KEYDOWN = 0x300, SDL_KEYUP };
EventType :: #repr("C") enum {
    quit    = 0x100,
    keydown = 0x300,
    keyup,   // 0x301, as in C
}
```

Neither half — `#repr("C")` or the explicit discriminants — is optional
for a binding that has to agree with a header.

## Safety at the boundary

- Passing a language pointer to C is safe: `c.from_ptr` can't fail, since
  a `*T`/`*mut T` is always a valid address.
- Receiving a `c.ptr` and using it requires a checked `.to_ptr()`/`.to_mut()`
  (null-trapping), or an explicit `#unsafe` scope to skip the check.
- Reading a `#raw`/uninitialized buffer C is expected to fill is only legal
  in `#unsafe` code.

Ordinary code stays checked; the unchecked, C-shaped operations are
visibly marked (`c.ptr`, `cast`, `#unsafe`, `#raw`) rather than hidden.
