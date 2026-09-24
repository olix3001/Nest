# 11 — C Foreign Function Interface

C interop is a first-class concern. Compiling to LLVM, the language shares C's
object model closely enough that calling into and out of C is direct: no
marshalling layer, and C types are named explicitly and cross with no
conversion, because most of them are just aliases for the language's own
(§11.1, §11.4).

## 11.1 The `core/c` namespace

All C types live in `core/c` and are conventionally imported as `c`:

```
c :: import <core/c>
```

`core/c` provides the C primitive and derived types, named in lowercase to mirror
C:

```
c.char   c.schar  c.uchar
c.short  c.ushort c.int    c.uint   c.long  c.ulong  c.longlong c.ulonglong
c.float  c.double
c.size_t c.ssize_t c.uintptr_t c.intptr_t
c.cstr                           // ptr.<c.char> — a pointer to NUL-terminated bytes
c.void                           // the unit type — what a C function returning `void` returns
c.anyopaque                      // an alias for `opaque` — the pointee of a `void *`
c.ptr.<T>                        // a raw, nullable, non-GC C pointer to T
```

**These are aliases, not `distinct` types (§3):** `c.int` is another name for
this target's `i32` and nothing more, so a language value goes straight into a
C call and comes straight back out, with no conversion inserted at either end
(§11.4). A `distinct` type would put a written conversion on both ends of every
call, and would be buying a distinction the language cannot use: what a C
`long` *is* varies by target, and the target is already what decides it —
`c.long` is `C_LONG`, a target-specific alias `nestc` generates
(32-bit on Windows, 64-bit elsewhere), and `c.int`, being always `i32`, cannot
be written any other way.

**`c.ptr.<T>` is the one exception, and it is its own type (§11.2).** A
language `*T` is non-null and traced; a C pointer is neither. Erasing that
difference is exactly how a null reaches code written on the promise that it
cannot see one, so the two are kept apart and every crossing between them is a
written call, never an implicit conversion.

There is no `c.bool` and no C function-pointer type constructor in `core/c`: a
C callback is written `*extern("c") func(...)`, using the language's own
function-pointer syntax (§3.5) rather than a `core/c` type.

### `c.void` and `c.anyopaque`

C spells two different things `void`, and the language spells them apart.

A C function that *returns* `void` returns nothing, which is the language's unit
type (§3.1). `c.void` is that type — the same one `void` names, under the name a
C programmer looks for — so a declaration reads the way the header does and the
call is an ordinary expression.

```
c :: import <core/c>

free :: extern("c") func (p: *c.anyopaque) -> c.void
```

A C `void *`, and every handle a C library hands back without publishing the
struct behind it — `FILE`, `sqlite3`, an `SDL_Window` — is a pointer to
something this program does not describe. `c.anyopaque` is the name for that
pointee: an alias for the `opaque` primitive (§3.1), which has no size and no
values.

```
fopen  :: extern("c") func (path: c.cstr, mode: c.cstr) -> *c.anyopaque
fclose :: extern("c") func (f: *c.anyopaque) -> c.int
```

`opaque`'s rules hold unchanged here, and they are what make the declaration
honest: it is a type only behind a pointer, nothing reads through it, and
`*T` <-> `*c.anyopaque` is an explicit `cast` in both directions. A program
cannot accidentally acquire a `c.anyopaque` by value, because there is no such
value — which is exactly the difference from `c.void`, which has one.

A binding that wants each handle kept apart — so that a `*Sqlite` cannot be
passed where a `*SDL_Window` is expected — declares its own nominal handle
instead, which is a `distinct` over the same type:

```
Sqlite :: distinct opaque
sqlite3_close :: extern("c") func (db: *Sqlite) -> c.int
```

Both spellings compile to the same pointer; the difference is entirely in what
the type checker will let the program confuse with what.

## 11.2 C pointers (`c.ptr.<T>`)

The language's `*T` / `*mut T` are GC-managed and **never null**. C pointers are
different and quarantined to `core/c`:

- `c.ptr.<T>` is a **raw** pointer: not tracked by the garbage collector, and
  **nullable**. Its null value is `c.null.<T>()`.
- A language `*T` / `*mut T` becomes a `c.ptr.<T>` through `c.from_ptr(p)`, an
  ordinary function call, **written explicitly at every call site** — there is
  no implicit coercion at the C boundary (§11.4). `from_ptr` is always sound (a
  `*T` is always a valid address) and always a loss of information, which is
  what makes the way back checked.
- Going the other way — `c.ptr.<T>` back to a language `*T` — is a method on
  the pointer itself, and is **checked**: `p.to_ptr()` (or `p.to_mut()` for a
  `*mut T`) panics if `p` is null. This keeps nullability from leaking into the
  non-null language pointer.

```
const p: c.ptr.<c.char> := c.null.<c.char>()   // a null C pointer
if p.is_null() { ... }

const gp: *mut Buffer := &mut buf
some_c_func(c.from_ptr(gp))                    // *mut Buffer -> c.ptr.<Buffer>, explicit
```

## 11.3 Declaring external functions (`extern("c")`)

An external C function is declared with an `extern("c")` **func** literal — the
`extern` modifier sits next to the `func` keyword and carries the ABI as a
string — and no body:

```
c :: import <core/c>

strlen :: extern("c") func (s: c.ptr.<c.char>) -> c.size_t
malloc :: extern("c") func (n: c.size_t) -> *c.anyopaque
qsort  :: extern("c") func (base: *c.anyopaque, n: c.size_t, size: c.size_t,
                            cmp: *extern("c") func (*c.anyopaque, *c.anyopaque) -> c.int)
```

A callback parameter like `cmp` is a **C function pointer**, `*extern("c")
func(...)` — the language's own function-pointer type (§3.5), spelled with the
C ABI. It is a different type from a Nest `*func(...)` of the same signature,
because the two conventions pass an aggregate differently, so neither converts
to the other: only an `extern("c")` function's name is a
`*extern("c") func(...)` value. `core/c` has no function-pointer type of its
own — `*extern("c") func(...)` is it.

- `extern("c")` selects the **C ABI / calling convention** *and* marks the binding
  as an external symbol resolved at link time. The ABI is a string so other
  conventions (`extern("system")`, …) stay expressible later; `"c"` is the only
  one defined now. `extern` is a keyword, not a `#`-directive, and it always sits
  immediately before `func` — never on the left of the binding name.
- A signature may use `c.*` types **or** ordinary language types with the
  matching representation — an `i32` parameter and a `c.int` one compile to the
  same thing, since `c.int` is `i32`. The `c.*` names exist so a declaration
  reads the way the C header does and states the *exact* C ABI width/shape
  without the reader having to know what it is on this target (`c.long`,
  `c.size_t`); nothing here is converted (§11.4).
- The **link symbol** defaults to the binding's own name. To bind a differently
  named external symbol, attach the `@link_name(str)` attribute: the identifier
  you write is what the rest of the program calls, while the compiler emits and
  links against the string. This lets a C-ugly name be renamed to house style:

  ```
  @link_name("LLVMSomeFunction")
  some_function :: extern("c") func (m: c.ptr.<Module>) -> c.int
  // callers write `some_function(...)`; the linker resolves `LLVMSomeFunction`
  ```

  `@link_name` is an ordinary attribute (`@`), so it sits before the binding like
  `@public` and works the same on a member inside an `extern("c") { ... }` block.
- Which library provides the symbol (link flags, header association) is a
  build-system concern layered on this syntax.
- The **calling convention** is C's unless `#callconv("name")` says otherwise
  (§9). The two are different questions and Win32 is where that shows: a
  `__stdcall` entry point is an `extern("c")` declaration with C types, called by
  a protocol in which the callee pops the arguments.

  ```
  @link_name("MessageBoxW")
  message_box :: #callconv("stdcall") extern("c") func (
    owner: *c.anyopaque, text: c.ptr.<u16>, caption: c.ptr.<u16>, flags: c.uint
  ) -> c.int
  ```

Many externals sharing one ABI may be grouped in an `extern("c") { ... }` block
instead of repeating the modifier. The block is pure surface sugar: each member is
an ordinary bodyless `func` declaration, and the block **desugars** to the same
per-function `extern("c")` bindings — there is no distinct block construct in the
AST or name resolution. Members are function declarations only.

```
extern("c") {
  strlen :: func (s: c.ptr.<c.char>) -> c.size_t
  malloc :: func (n: c.size_t) -> *c.anyopaque
}
// identical to writing `strlen :: extern("c") func ...` on each line
```

## 11.4 Crossing the boundary

There is **no implicit coercion** at the C boundary, in either direction. An
`extern("c")` signature is a promise that its types already are C's (§11.1), so
a call site writes the `core/c` type the declaration asks for:

- The scalar `core/c` types (`c.int`, `c.size_t`, …) are aliases (§11.1), so a
  language value of the matching type crosses with **no conversion at all** —
  `c.int` *is* `i32`, not a value that becomes one.
- A pointer is **never** passed directly: a `*T` / `*mut T` crosses through
  `c.from_ptr(p)`, written at the call site, and a `c.ptr.<T>` coming back is
  read with `.to_ptr()` / `.to_mut()`, both checked (§11.2). There is no `[]T`
  or `str` parameter form at the boundary — a caller passing a slice or a
  string writes out the pointer and length (or NUL-terminated bytes, via
  `c.to_cstr` / `c.from_cstr`) itself.
- Aggregates cross by the shape their type declares (§11.5): `#repr("C")` is
  what makes that shape match a C declaration's.

The **one** place a conversion is inserted for the program is a `#c_vararg`
function's variadic tail (§11.3): each argument past the declared, fixed
parameters is widened to C's own default argument promotion — anything
narrower than an `int` to an `int`, a `float` to a `double` — because that is
the shape `va_arg` on the C side will read, not a convenience for the caller.
This is the only coercion `extern("c")` performs; everything else above is
written by hand.

## 11.5 Exporting to C (`extern("c")` + `@public`)

A language function is exposed to C callers by defining it as an `extern("c")`
func **with a body** and giving it external linkage. It then uses the C ABI and
appears as an ordinary C symbol:

```
@public
add :: extern("c") func (a: c.int, b: c.int) -> c.int {
  return a + b
}
```

Aggregates handed across the boundary should be written `#repr("C")` (§9), which
is the promise that the type's representation is the one a C declaration of it
has — and, on an `enum`, the thing that makes the tag a C `int`. `#packed` and
`#align(...)` say the rest where the C side expects them, and `#raw` structs are
useful here to hand C uninitialized buffers without the zeroing cost.

### A C enumeration

A C enumeration is an `int` whose values the header states, so binding one needs
both halves: `#repr("C")` for the type, and explicit discriminants (§3.3) for the
values.

```
// enum SDL_EventType { SDL_QUIT = 0x100, SDL_KEYDOWN = 0x300, SDL_KEYUP };
EventType :: #repr("C") enum {
    quit     = 0x100,
    keydown  = 0x300,
    keyup,                  // 0x301, as in C
}
```

Without `#repr("C")` the tag would be the narrowest integer that holds those
values — two bytes here — which is not what the C declaration passes or returns.
Without the discriminants the values would be 0, 1, 2. Neither half is optional
for a binding that has to agree with a header.

## 11.6 Safety at the boundary

C interop is where the language's "not memory-safe like Rust" stance is visible:

- Passing a language pointer to C is safe: `c.from_ptr` cannot fail, since a
  `*T` / `*mut T` is always a valid address.
- Receiving a `c.ptr` and using it requires a checked `.to_ptr()` / `.to_mut()`
  (null-trapping) or an explicit `#unsafe` scope to skip the check.
- Reading a `#raw` / uninitialized buffer that C is expected to fill is only
  legal in `#unsafe` code; the compiler will not vouch for its contents.

The intent is that ordinary code stays checked, and the unchecked, C-shaped
operations are visibly marked (`c.ptr`, `cast`, `#unsafe`, `#raw`) rather than
hidden.
