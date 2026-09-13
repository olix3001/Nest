# 06 — Expressions and Operators

Almost everything is an expression. Blocks, `match`, and `if`/`else` produce
values; statements are expressions used for effect.

## 6.1 Primary expressions

```
primary =
    literal                     // 42, 3.14, "s", f"...", 'c', true, false
  | identifier
  | call                        // an intrinsic is an ordinary call (see 6.4)
  | 'self' | 'Self'
  | '(' expr ')'                // grouping
  | '(' expr { ',' expr } ')'   // tuple
  | composite_literal
  | closure                     // func (...) -> T { ... }   (see 05)
  | if_expr | block
  | 'import' string             // compile-time namespace value (see 04)
```

## 6.2 Composite literals

```
composite_literal =
    type '{' composite_body '}'          // typed: record OR array (chosen by the type)
  | type '(' [ args ] ')'                // typed tuple struct
  | '.{' composite_body '}'              // inferred: record | array | tuple (by context)
  | '.' variant_name [ payload_args ]    // enum variant (see below)

composite_body =
    [ field_init { ',' field_init } ]    // named entries      -> record / struct
  | [ expr { ',' expr } ]                // positional entries -> array / tuple
  | expr ';' expr                        // repeat: value ; count -> array
field_init = identifier ':' expr
```

```
http.Router { logging: true }              // typed record
Client {}
Pair(1, 2)                                 // typed tuple struct
.{ id: id, url: url, width: w, height: h } // inferred record (from context)
```

The `.{` form defers the type to context (a `return` type, a `let` annotation, a
parameter type, an element type). Whether a positional `.{ ... }` is an array, a
tuple, or a **tuple struct** is likewise decided by the expected type: with a
tuple-struct context, `.{ a, b }` is identical to the named form `MyType(a, b)`.
Anonymous struct values built with a named-field `.{ ... }` further coerce into
any matching named struct (one-way; see [03-types.md](03-types.md) §3.8).

### Array and slice literals

Brackets `[ ]` build **types** and index/slice; array **values** use the
composite-literal forms. The inferred literal is `.{ ... }`; the explicit form
prefixes the array/slice type:

```
.{ 1, 2, 3 }             // inferred array (or tuple) -> here [3]int by context
[_]int { 1, 2, 3 }       // explicit element type, length inferred
[3]int { 1, 2, 3 }       // explicit type and length
[]int  { 1, 2, 3 }       // slice literal (backing array allocated)
.{ 0; 16 }               // repeat: sixteen zeros (inferred [16]int)
[16]uint8 { 0; 16 }      // typed repeat
.{ .{1, 2}, .{3, 4} }    // nested -> [2][2]int by context
```

A fixed array `[N]T` **implicitly coerces to a read-only slice `[]T`** wherever a
slice is expected; it never coerces to `[]mut T` (mutation needs an explicitly
mutable source). See [03-types.md](03-types.md) §3.3.

### Inferred enum variants (snake_case)

A variant written with a leading `.` and no enum qualifier takes its enum type
from context. Variant names are snake_case:

```
return .redirect(cat.url)          // Response.redirect, inferred
return .err(.network_error("..."))  // Result.err wrapping FetchError.network_error
return .rect { w: 3.0, h: 4.0 }     // record-payload variant
```

Both the outer and inner variants infer their enums from the expected type.

## 6.3 Postfix expressions

```
postfix =
    postfix '.' identifier         // field / member access
  | postfix '.' integer            // tuple element
  | postfix '.<' type_or_'_' { ',' type_or_'_' } '>'   // generic instantiation
  | postfix '(' [ args ] ')'       // call
  | postfix '[' expr ']'           // index
  | postfix '[' range_expr ']'     // slice (range_expr uses ..< / ..= / ..)
  | postfix '.*'                   // dereference a pointer (Zig-style postfix)
  | postfix '.?'                   // Try: unwrap-or-return           (see 08)
  | postfix '.!'                   // Try: unwrap-or-abort            (see 08)
  | postfix '.match' match_block   // match on the receiver           (see 07)
```

Member access auto-dereferences pointers: `self.url` works whether `self` is
`CatImage`, `*CatImage`, or `*mut CatImage`. When the whole pointee is needed,
`p.*` dereferences explicitly.

`.?` and `.!` are the two `Try` operators (not special-cased to `Option` /
`Result` — they work on any `Try` implementor):

| Form | Behavior | Failure case |
|------|----------|-------------|
| `x.?` | yields the success value | **returns** the failure from the enclosing function |
| `x.!` | yields the success value | **aborts** (calls `unwrap`) |

There is no bare `expr?`; propagation is always the explicit `.?`. See
[08-error-handling-and-defer.md](08-error-handling-and-defer.md) for the `Try`
trait.

`.match` is postfix so a value flows left-to-right into its analysis:

```
result.match { .ok(cats) => ..., .err(e) => ... }
```

## 6.4 Intrinsics (`#intrinsic`)

An intrinsic is an **ordinary function declared in `core`, with no body**, marked
`#intrinsic`. The compiler supplies the body — an instruction, a constant, or
nothing at all — and everything else about it is ordinary: it has a signature,
it takes turbofish type arguments, it infers, it can be passed around, and it is
documented where every other function is documented.

```nest
// core/mem.nest
@public size_of  :: #intrinsic("size_of") func <T> () -> usize
@public align_of :: #intrinsic("align_of") func <T> () -> usize
@public cast     :: #intrinsic("cast") func <T, U> (x: U) -> T
```

The tag in the directive is what identifies the intrinsic; a bare `#intrinsic`
defaults it to the declared name (§9). `cast` takes its **target type first** so
that `cast.<u8>(n)` pins `T` and leaves `U` to be inferred from the argument —
that is ordinary partial-turbofish inference, not a rule about `cast`.

There is no `$name` form. A separate namespace for compiler-provided functions
bought nothing: it split the library in two, meant the signatures lived in the
compiler where no reader of `core` could see them, and made every new intrinsic a
change to the language's lexer rather than a line in a library. `#intrinsic` says
the same thing where it belongs, next to the declaration it describes.

The directive marks the **direction**: `#lang("...")` is the compiler *looking
up* an item core provides; `#intrinsic` is core declaring an item the compiler
*fills in*. A bodyless function that is neither is a trait requirement (§5); one
at namespace scope with no `#intrinsic` and no `extern` is an error.

The intrinsics (extensible; not a closed list):

| Intrinsic | Purpose |
|-----------|---------|
| `cast.<T>(x)` / `cast(x)` | explicit type conversion (§6.5) |
| `transmute.<T>(x)` | reinterpret the bits of `x` as `T` (same size) |
| `new.<T>()` | allocate one zeroed, GC-managed `T`; yields `*mut T` (§6.9) |
| `make.<[]T>(len[, cap])` | allocate a zeroed, GC-managed slice (§6.9) |
| `size_of.<T>()` / `align_of.<T>()` | layout queries (`usize`), `#const` |
| `len(x)` | element count of an array or slice (`usize`); the core library's `.len()` method is written in terms of it |
| `drop(p)` | free `p`'s object now, rather than when the collector next runs (§6.9) |
| `assert(cond[, msg])` | compile-time assertion (§6.10) |
| `trap()` | stop the process immediately, without unwinding — the last instruction of a panic |
| `embed_file("path")` | splice a file's bytes as a compile-time `[]u8` |
| `gc_collect()` | request a collection now (§6.4.1) |
| `gc_keep_alive(x)` | keep `x` reachable up to this point (§6.4.1) |
| `gc_pin(x)` | make `x`'s object immortal and immovable (§6.4.1) |
| `wrapping_add`, `checked_add`, `saturating_add`, … | integer operations with a stated overflow behaviour; inherent methods on `int.<N>` / `uint.<N>` (§3.1) |

```nest
const bits := transmute.<u32>(3.14)
const p    := new.<CatImage>()
const n    := size_of.<CatImage>()
DATA :: embed_file("logo.png")            // []u8 baked into the binary
```

The ones that are **operations on a value** are inherent methods rather than free
functions, so they read like the rest of the language: `x.wrapping_add(y)`, not
`wrapping_add(x, y)`. The ones that are questions about a *type* stay free
functions, because there is no value to hang them off.

`panic(msg)` is **not** in this table, and that is the point: it is an ordinary
function in `core` (§8) that calls the replaceable panic handler, and `trap` is
the one part of it no library can write. Nothing about `panic` needs the compiler
except its `#lang("panic")` tag, which is how a trapped overflow or an index past
the end of a sequence reaches the same function a written `panic(...)` does.

Which of these names are in scope unqualified is the prelude's business (§4.6):
`cast`, `panic` and `size_of` are, the rest are reached through an import.

Many intrinsics are usable at compile time (they behave as `#const`), which is
why `cast(8080)` and `embed_file(...)` may appear on the RHS of `::`.

### 6.4.1 Garbage-collector intrinsics

Memory is collected automatically and none of these is needed by ordinary code.
They exist because two things the collector cannot see from the outside — a
pointer that has escaped to C, and a pointer C will hold for longer than one call
— have no other expression. All three return `void`: what they do is change what
the collector may do next.

| Intrinsic | Meaning |
|---|---|
| `gc_collect()` | Request a collection now. A hint, not a guarantee. |
| `gc_keep_alive(x)` | A no-op that **counts as a use**, so `x` stays reachable up to this point. |
| `gc_pin(x)` | Make the object immortal and immovable. |

`gc_keep_alive` exists for one specific failure. A value's live range ends at
its last **read**, so this is wrong:

```
let buf := make.<[]u8>(1024)
let p   := &buf[0]
c_write(p)                 // `buf` is already dead here — nothing reads it again
```

The collector may move or free `buf` during the call even though C is using its
address. `gc_keep_alive(buf)` **after** the call extends the live range across
it.

`gc_pin` is for handing a pointer to C for longer than one call — a callback
registration, a buffer the other side keeps. A pinned object is never moved and
never collected, which is a leak by construction. That is the trade, and it is
why the intrinsic is explicit rather than something the compiler infers.

## 6.5 `cast`

`cast` is the sole explicit conversion intrinsic. Two forms:

```
cast.<T>(expr)      // convert expr to the named target type T
cast(expr)          // convert expr to the contextually-expected type
```

```
cast.<HttpPort>(8080)                  // int literal -> distinct u16
cast.<*dyn ToJson>(&cat)               // *CatImage   -> ToJson trait object
const raw_id: str := cast(self.id)     // CatId       -> str (target from annotation)
```

`cast` covers numeric widening/narrowing, `distinct` ↔ underlying, `*mut T` →
`*T`, pointer → `*dyn Trait`, `*T` → `c.ptr.<T>` (see
[11-c-ffi.md](11-c-ffi.md)), and any conversion the type system defines as
legal. It never performs a disallowed conversion — illegal casts are compile
errors, not run-time coercions.

### A written `cast` may lose; an inserted one may not

A `cast` **the program writes** is allowed to lose precision. Narrowing an
integer keeps the low bits, and narrowing a float rounds — exactly what the
machine does, and exactly what the program asked for. This holds at compile time
too, so a constant and the same expression at run time are the same number:

```
A :: 400
X: u8 :: cast.<u8>(A)          // 144 — the low 8 bits, as at run time
Y: u8 :: cast.<u8>(300)        // 44
Z: f32 :: cast.<f32>(3.5e40)   // inf
```

The conversion the **compiler inserts** to settle an untyped literal on the type
its use site asked for (§1.5, §2.5) may not. Nothing in the source said `300`
should become `44`, so a literal the target cannot hold is an error:

```
let y: u8 := 300               // error: the literal `300` does not fit in `u8`
let z: f32 := 3.5e40           // error: the literal `3.5e40` does not fit in `f32`
let ok: u8 := cast.<u8>(x)     // fine for any integer `x` — it was written
```

"Cannot hold" is exact for integers: the value keeps its arbitrary precision
until it settles, so the check is exact however the number was written. For
floats it means the width loses the number *entirely* — a finite value that
overflows to infinity, or a non-zero one that underflows to zero. Ordinary
rounding is not an error and cannot be: `0.1` is no more exact in `f64` than in
`f32`, so `const e: f32 := 0.1` is a `f32`'s nearest value to `0.1`.

A string literal has nothing to check: every type a `comptime_str` may settle on
(§1.5) holds all of it.

## 6.6 Prefix and unary operators

```
unary =
    '&' unary        // address-of      -> *T
  | '&' 'mut' unary  // mutable address -> *mut T (place must be mutable)
  | '-' unary        // numeric negation
  | '!' unary        // boolean not   (also spelled `not`)
  | '~' unary        // bitwise not
  | '*' type         // (type position only) pointer type
```

Dereference is **not** a prefix operator; it is the postfix `p.*` (§6.3). The
prefix `*` appears only in type positions (`*T`, `*mut T`).

## 6.7 Binary operators and precedence

Highest to lowest; same-row operators associate left-to-right unless noted.

| Level | Operators | Notes |
|-------|-----------|-------|
| 1 | postfix `.` `.<>` `()` `[]` `.*` `.?` `.!` | tightest |
| 2 | prefix `&` `&mut` `-` `!` `~` | unary |
| 3 | `*` `/` `%` | multiplicative |
| 4 | `+` `-` | additive |
| 5 | `<<` `>>` | shifts |
| 6 | `&` | bitwise and |
| 7 | `^` | bitwise xor |
| 8 | `\|` | bitwise or |
| 9 | `==` `!=` `<` `<=` `>` `>=` | comparison (non-associating) |
| 10 | `&&` / `and` | logical and (short-circuit) |
| 11 | `\|\|` / `or` | logical or (short-circuit) |

Comparison operators do not chain: `a < b < c` is a parse error; write
`a < b && b < c`. `and`/`or` are exact synonyms for `&&`/`||`. Bitwise operators
require integer operands. Mixing signed/unsigned or differing widths requires an
explicit `cast`.

Precedence and associativity are **purely syntactic**: they decide how an
expression parses into a tree, before any meaning is attached. What each operator
*does* is then defined by a core-library trait (§6.13).

The built-in numeric types satisfy those traits too, but not through source
`impl`s: they are width-parameterized (`u4096` is as real as `u8`), so there is
no finite set of impls to write. The compiler instead carries one **builtin row**
per primitive operator, and the trait solver considers those rows *uniformly with
user impls* when selecting. A primitive `a + b` and a `Vec3 + Vec3` therefore go
through the same selection and reach the same call shape; the row simply marks
the result as a machine instruction rather than a function body. Two consequences
are worth knowing: bitwise and shift rows apply to integers only, and a user impl
for a primitive is exactly as specific as the row, so it is an ambiguity rather
than an override (and is refused outright by coherence — §4.9).

## 6.8 Block and `if` as expressions

```
block = '{' { statement } [ expr ] '}'
```

A block evaluates to its trailing expression (or `void`). `if`/`else` is an
expression whose arms are blocks of a common type:

```
const label := if port == 80 { "http" } else { "custom" }
```

The trailing expression is what supplies a value to an enclosing `let`/`const`,
a `match` arm, an `if` arm, or any other expression position:

```
const area := shape.match {
  .rect { w, h } => {
    const scaled := w * dpi
    scaled * h            // trailing expr -> this arm's value
  },
  .point => 0.0,
}
```

**A `func` body is the sole exception:** its trailing expression is **not** an
implicit return — a function returns `void` unless it uses `return` (see
[05-functions-and-generics.md](05-functions-and-generics.md) §5.1). Blocks used
as expressions (arms, `let` initializers, nested `{ … }`) do yield their tail;
only the outermost function-body block discards it.

Loops and iteration have their own chapter (see
[10-loops-and-iteration.md](10-loops-and-iteration.md)); a `loop` exited with
`break value` yields that value.

## 6.9 Allocation intrinsics (`new`, `make`, `drop`)

The language is garbage-collected, so a free is never *required*. Fresh memory
comes from two intrinsics; std containers (`Vector`, `HashMap`, …) are built on
top of them.

```
new.<T>()               // one zeroed, GC-managed T          -> *mut T
make.<[]T>(len)         // zeroed slice of `len` elements    -> []mut T
make.<[]T>(len, cap)    // as above, with reserved capacity
drop(p)                 // free `p`'s object now             -> void
```

```
const cat := new.<CatImage>()          // *mut CatImage, all fields zeroed
const buf := make.<[]uint8>(1024)      // []mut uint8, zeroed
let   xs  := Vector.<int>.new()         // std, wraps make internally
```

Memory is zero-initialized unless the element type is `#raw` (see
[09-directives-and-attributes.md](09-directives-and-attributes.md)), in which
case it is left uninitialized and reads are only permitted in `#unsafe` scopes.

### `drop`, and why it is not `#unsafe`

`drop(p)` releases an object before the collector would have. The compiler
already does exactly this wherever it can prove an object does not outlive the
scope that made it (`design/lir.md` §5); `drop` is that same operation written by
hand, for the cases the proof cannot reach — a buffer finished with well before
its scope ends, say.

Writing it **takes the question on**, and the language holds you to the part it
can check:

- the compiler stops inserting a drop of its own for that value, so the object is
  freed once;
- **using the name afterwards is a compile error**, along with dropping it twice
  and dropping — inside a loop — something declared outside it.

```
let p := new.<Node>()
let v := p.*.x
drop(p)
return p.*.x            // error: `p` is used after it was dropped
```

Assigning to the name gives it an object again and clears the state. What the
check cannot see is an **alias** made before the drop:

```
let q := p
drop(p)
q.*.x                   // not caught
```

Catching that wants ownership, which this language does not have. So `drop` is
the one intrinsic whose correctness is partly the author's, which is also why the
parameter is `*mut T`: freeing an object is the most destructive write there is,
and the permission to do it belongs in the type.

## 6.10 Compile-time statement items (`assert`)

Intrinsic calls that return `void` can be used as standalone statements — not
only inside function bodies, but also as **items** inside a `struct`, `enum`,
`trait`, or `namespace` body. `assert(cond[, msg])` is the canonical case: a
compile-time assertion. A body entry is therefore
`field | variant | decl | comptime-statement`.

```
Header :: #packed struct {
  assert(size_of.<Self>() == 64, "Header must be 64 bytes")   // static check
  magic: uint32,
  len:   uint32,
}
```

`assert` is checked during compilation and produces no run-time code. A
**run-time** assertion is the ordinary std function `assert(cond, msg)`, not an
intrinsic (see [08-error-handling-and-defer.md](08-error-handling-and-defer.md)).

## 6.11 Interpolated strings

`f"...{expr}..."` is sugar for a **format buffer, one `display` call per piece,
and the bytes that came out**. It is an ordinary expression of type `str`:

```
f"Invalid dimensions: {w}x{h}"
// ==>
// {
//   let mut __fmt := format.start()
//   "Invalid dimensions: ".display(&mut __fmt)
//   w.display(&mut __fmt)
//   "x".display(&mut __fmt)
//   h.display(&mut __fmt)
//   format.end(&mut __fmt)
// }
```

The contract is the `#lang("display")` trait in `core` — one method,
`display(self, out: *mut Buf)`, which writes into the buffer rather than
returning a string, so a struct of ten members costs ten appends and one
allocation. **Every interpolated value must implement it**, and a value that does
not is "no impl of `Display`" reported at the `{expr}` that has none.

A literal segment goes through the same call an embedded expression does: `str`
implements `Display` like any other type, so nothing in the desugaring treats
the pieces that were typed as text specially.

Reaching `core` happens by `#lang` tag — `format_start`, `format_end`, `display`
— never by name or path, so a replacement `core` supplies its own formatting
without a compiler change. There is no formatting *intrinsic*: what a value looks
like is a library question, and a compiler that answered it would leave a user's
own type with nowhere to.

Width, alignment and precision (`{x:>8.2}`) are **not** in the syntax: `{ }`
holds an expression and nothing else (§1.5).

## 6.12 Ranges

Range operators are always **explicit** about the upper endpoint. A bare `..`
(no `<` / `=`) denotes an *unbounded* end.

```
range_expr =
    expr '..<' expr     // half-open [a, b)
  | expr '..=' expr     // closed    [a, b]
  | expr '..'           // from a, unbounded above
  | '..<' expr          // up to b, exclusive
  | '..=' expr          // up to b, inclusive
  | '..'                // full / unbounded (also the pattern rest token)
```

```
0 ..< n        // 0, 1, ..., n-1
0 ..= n        // 0, 1, ..., n
lo ..          // lo, lo+1, ...        (unbounded)
```

A range is an ordinary value implementing `Iterator` (see
[10-loops-and-iteration.md](10-loops-and-iteration.md)), so it drives `for`
loops; the same syntax is used to slice (`s[lo..<hi]`, `s[lo..]`, `s[..<n]`,
`s[..]`) and to match numeric ranges (see
[07-patterns-and-matching.md](07-patterns-and-matching.md)). There is no bare
`a..b`; write `a..<b` or `a..=b`.

## 6.13 Operators as trait methods

Operators are **not** built into the compiler; each one is sugar for a call to a
method on a core-library trait. The trait the compiler reaches for is the one
carrying the matching `#lang(...)` directive (see
[09-directives-and-attributes.md](09-directives-and-attributes.md) §9.3). A type
supports an operator by implementing that trait — the same mechanism user code
uses to overload it. `int + int` and `Vec3 + Vec3` are the same construct: both
are `Add.add(a, b)`, differing only in which `impl` is selected.

### The registry

| Syntax | `#lang` tag | Trait | Method (illustrative signature) |
|--------|-------------|-------|----------------------------------|
| `a + b`   | `"add"`    | `Add`    | `add(self: Self, rhs: Rhs) -> Self.Output` |
| `a - b`   | `"sub"`    | `Sub`    | `sub(self, rhs) -> Output` |
| `a * b`   | `"mul"`    | `Mul`    | `mul(self, rhs) -> Output` |
| `a / b`   | `"div"`    | `Div`    | `div(self, rhs) -> Output` |
| `a % b`   | `"rem"`    | `Rem`    | `rem(self, rhs) -> Output` |
| `a & b`   | `"bitand"` | `BitAnd` | `bitand(self, rhs) -> Output` |
| `a \| b`  | `"bitor"`  | `BitOr`  | `bitor(self, rhs) -> Output` |
| `a ^ b`   | `"bitxor"` | `BitXor` | `bitxor(self, rhs) -> Output` |
| `a << b`  | `"shl"`    | `Shl`    | `shl(self, rhs) -> Output` |
| `a >> b`  | `"shr"`    | `Shr`    | `shr(self, rhs) -> Output` |
| `-a`      | `"neg"`    | `Neg`    | `neg(self: Self) -> Self.Output` |
| `~a`      | `"bitnot"` | `BitNot` | `bitnot(self: Self) -> Self.Output` |
| `a == b`, `a != b` | `"eq"`  | `Eq`  | `eq(self: Self, rhs: Self) -> bool` |
| `a < b`, `a <= b`, `a > b`, `a >= b` | `"ord"` | `Ord` | `cmp(self: Self, rhs: Self) -> Ordering` |
| `a[i]` (read)  | `"index"`     | `Index`    | `index(self: *Self, i: Idx) -> *Self.Output` |
| `a[i] = v` (write) | `"index_mut"` | `IndexMut` | `index_mut(self: *mut Self, i: Idx) -> *mut Self.Output` |

Compound assignment is deliberately **absent** from the table: `a += b` is not an
operator of its own but sugar for `a = a + b`, so it needs no `#lang` item and
inherits whatever `Add` the type has. A separate `AddAssign` would let `+=` and
`+` disagree for the same type, which is a difference no reader expects to have
to check.

Two supporting `#lang` enums round out the set: `Ordering` (`#lang("ordering")`,
`enum { less, equal, greater }`) is what `Ord.cmp` returns, and `Result` /
`Option` / `ControlFlow` back the `Try` operators (§6.3, see
[08-error-handling-and-defer.md](08-error-handling-and-defer.md)).

### Desugaring rules

Applied **after** parsing, to the operator tree §6.7 produced:

- Arithmetic, bitwise, and shift binaries lower directly to the method call:
  `a + b` ⇒ `Add.add(a, b)`, `a << b` ⇒ `Shl.shl(a, b)`, and so on.
- Prefix `-a` ⇒ `Neg.neg(a)`; prefix `~a` ⇒ `BitNot.bitnot(a)`.
- Equality: `a == b` ⇒ `Eq.eq(a, b)`; `a != b` ⇒ the negation of that `bool`.
- Ordering: all four relations go through one method, `Ord.cmp`, and test its
  `Ordering` result — `a < b` is "`cmp` answered `.less`", `a >= b` is "`cmp` did
  not answer `.less`", and so on. One `cmp` gives every ordering relation.
- The numeric core is the exception to those last two: `i32 == i32` and
  `i32 < i32` are the machine's own compare, and are *not* routed through a
  three-way `cmp` the hardware would only have to undo. `Eq` and `Ord` are what a
  **user** type is compared by.
- Indexing: `a[i]` in value position ⇒ `Index.index(&a, i).*`; `a[i]` as the
  place of an assignment ⇒ `IndexMut.index_mut(&mut a, i).*`.
  The **built-in sequences are the exception**, and it is a rule rather than a
  carve-out: `[]T`, `[]mut T` and `[N]T` implement `Index` and **not**
  `IndexMut`, so a write to one of them goes through `Index` too. A sequence's
  write permission is in its *type* — a `[]mut T` is writable through however
  immutably the binding holding it was declared (§2.3), and an array's elements
  belong to whatever holds the array — while `IndexMut`'s `self: *mut Self` asks
  for permission over the *container*, which is the right question for a user
  container and the wrong one for these. What the element pointer permits
  follows the receiver, which is the one thing the declared signature cannot
  say; it is the same gap `make.<[]T>(n)` has (§6.9).
  Both impls live in `core` and both members are `#intrinsic` (§6.4): there is
  no body to write, only a compiler operation to name. `a[i]` is therefore one
  construct for every type, with no special case in the compiler for what
  indexing *means*.
- Compound assignment: `a += b` ⇒ `a = a + b` (and likewise for `-=` `*=` `/=`
  `%=` and the bitwise/shift forms), so it dispatches through the same `Add` the
  plain `+` does.

### What is *not* a trait method

These stay built into the compiler and dispatch on nothing:

- `&&` / `and`, `||` / `or` — short-circuiting control flow on `bool` only.
- `!` / `not` — boolean negation on `bool` (bitwise complement is `~`, which
  *is* a trait via `BitNot`).
- `&` / `&mut` (address-of) and postfix `.*` (deref) — the built-in pointer
  operations; nest pointers are not a library type.
- `.?` / `.!` — driven by the `Try` **lang item** rather than by an operator
  trait in this table (§6.3, [08](08-error-handling-and-defer.md) §8.3).

### How a `+` reaches its implementation

Putting it together, when the compiler lowers `a + b`:

1. Parsing yields `Binary { op: Add, a, b }` — precedence/associativity only,
   no meaning yet.
2. Lowering rewrites it to a call to the `add` method of the trait tagged
   `#lang("add")`. The compiler finds that trait by its tag; the core library,
   not the compiler, decides what `Add` looks like.
3. Ordinary trait selection (see
   [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md)
   §4.8) picks the `impl Add for typeof(a)` — a core-library impl for the numeric
   primitives, or a user `impl` for a user type. Missing impl ⇒ the same "no such
   method" error any absent trait method gives; there is no separate "cannot add"
   rule.

The payoff: operator overloading, the built-in numeric operators, and the core
library are one uniform system. Adding an operator to a new type is writing an
`impl`; the compiler needs no change, because the only thing it hard-codes is the
*tag → trait* lookup, and the tag is attached in the core library with `#lang`.
