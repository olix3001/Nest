# 11 — C Foreign Function Interface

C interop is a first-class concern. Compiling to LLVM, the language shares C's
object model closely enough that calling into and out of C is direct: no
marshalling layer, C types are named explicitly, and standard-language types
coerce into their C equivalents at the boundary.

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
c.bool
c.size_t c.ssize_t c.ptrdiff_t c.intptr_t c.uintptr_t
c.void                          // the C `void` (as a return type)
c.ptr.<T>                       // a raw, nullable, non-GC C pointer to T
c.func                          // a C function-pointer type constructor
```

These are distinct nominal types, kept separate from the language's own
`i8`/`usize`/… because they carry a guarantee the language types do not.

**`core/c` types are ABI-compatible with the target's C compiler; the language's
own primitives are not guaranteed to be.** A `core/c` type's width, alignment,
and argument-passing convention are *defined* to be whatever the target C ABI
says they are — which is why `c.int` cannot be written as a fixed width at all,
and why `c.long` is 32-bit on Windows and 64-bit elsewhere. A language primitive
means the opposite thing: `i32` is exactly 32 bits on every target, chosen by the
language and owed nothing to the platform. That a language primitive happens to
match a C type on mainstream targets is a property of those targets, not a
promise — so a type crossing the C boundary is spelled with a `core/c` name, and
the coercions of §11.4 are what carry a language value into one.

## 11.2 C pointers (`c.ptr.<T>`)

The language's `*T` / `*mut T` are GC-managed and **never null**. C pointers are
different and quarantined to `core/c`:

- `c.ptr.<T>` is a **raw** pointer: not tracked by the garbage collector, and
  **nullable**. Its null value is `c.null` (equivalently `c.ptr.null.<T>()`).
- A language `*T` / `*mut T` **implicitly coerces** to `c.ptr.<T>` at a C call
  boundary (the address is passed through; the GC is informed so the pointee is
  not collected for the duration of the call).
- Going the other way — `c.ptr.<T>` back to a language `*T` — is **explicit and
  checked**: `$cast.<*T>(p)` traps if `p` is null (or is undefined behavior only
  inside an `#unsafe` scope, where the null check is dropped). This keeps
  nullability from leaking into the non-null language pointer.

```
const p: c.ptr.<c.char> := c.null           // a null C pointer
if p == c.null { ... }

const gp: *mut Buffer := &mut buf
some_c_func(gp)                              // *mut Buffer coerces to c.ptr.<Buffer>
```

## 11.3 Declaring external functions (`extern("c")`)

An external C function is declared with an `extern("c")` **func** literal — the
`extern` modifier sits next to the `func` keyword and carries the ABI as a
string — and no body:

```
c :: import <core/c>

strlen :: extern("c") func (s: c.ptr.<c.char>) -> c.size_t
malloc :: extern("c") func (n: c.size_t) -> c.ptr.<c.void>
qsort  :: extern("c") func (base: c.ptr.<c.void>, n: c.size_t, size: c.size_t,
                            cmp: c.func.<(c.ptr.<c.void>, c.ptr.<c.void>) -> c.int>)
```

- `extern("c")` selects the **C ABI / calling convention** *and* marks the binding
  as an external symbol resolved at link time. The ABI is a string so other
  conventions (`extern("system")`, …) stay expressible later; `"c"` is the only
  one defined now. `extern` is a keyword, not a `#`-directive, and it always sits
  immediately before `func` — never on the left of the binding name.
- A signature may use `c.*` types **or** ordinary language types — any type is
  allowed at the boundary. The `c.*` types exist for when you need an *exact* C
  ABI width/shape; language types coerce to their C counterparts per §11.4.
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

Many externals sharing one ABI may be grouped in an `extern("c") { ... }` block
instead of repeating the modifier. The block is pure surface sugar: each member is
an ordinary bodyless `func` declaration, and the block **desugars** to the same
per-function `extern("c")` bindings — there is no distinct block construct in the
AST or name resolution. Members are function declarations only.

```
extern("c") {
  strlen :: func (s: c.ptr.<c.char>) -> c.size_t
  malloc :: func (n: c.size_t) -> c.ptr.<c.void>
}
// identical to writing `strlen :: extern("c") func ...` on each line
```

## 11.4 Implicit casts at the boundary

To make C calls ergonomic, the standard language types coerce **implicitly** to
their C counterparts when passed to an `extern("c")` function (and only there):

| Language type | Coerces to |
|---------------|-----------|
| `int32` / `uint32` / … | the matching `c.int` / `c.uint` / … of equal width |
| `isize` / `usize` | `c.ssize_t` / `c.size_t` |
| `*T` / `*mut T` | `c.ptr.<T>` |
| `[]T` | `(c.ptr.<T>, c.size_t)` — pointer + length, per the callee's expectation |
| `str` | `c.ptr.<c.char>` (NUL-terminated copy when required) |
| `bool` | `c.bool` |

The reverse direction (C type → language type) is **never** implicit: results
coming back from C are C types and must be converted with `$cast` (checked) so
that null, width, and signedness assumptions are made explicit. Widths that do
not match the target ABI are a compile error rather than a silent truncation.

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

Aggregates handed across the boundary should have a C-compatible layout —
typically `#packed` or `#align(...)` as the C side expects, and built from `c.*`
field types (or language types of matching ABI width). `#raw` structs are useful
here to hand C uninitialized buffers without the zeroing cost.

## 11.6 Safety at the boundary

C interop is where the language's "not memory-safe like Rust" stance is visible:

- Passing a language pointer to C is safe (GC-aware, non-null).
- Receiving a `c.ptr` and using it requires a checked `$cast` (null-trapping) or
  an explicit `#unsafe` scope to skip the check.
- Reading a `#raw` / uninitialized buffer that C is expected to fill is only
  legal in `#unsafe` code; the compiler will not vouch for its contents.

The intent is that ordinary code stays checked, and the unchecked, C-shaped
operations are visibly marked (`c.ptr`, `$cast`, `#unsafe`, `#raw`) rather than
hidden.
