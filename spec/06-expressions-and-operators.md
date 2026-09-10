# 06 — Expressions and Operators

Almost everything is an expression. Blocks, `match`, and `if`/`else` produce
values; statements are expressions used for effect.

## 6.1 Primary expressions

```
primary =
    literal                     // 42, 3.14, "s", f"...", 'c', true, false
  | identifier
  | intrinsic_call              // $name(...) or $name.<...>(...)   (see 6.4)
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

## 6.4 Intrinsics (`$name`)

Compiler intrinsics are identifiers that begin with `$`. Lexically they are
ordinary identifiers (see [01-lexical-structure.md](01-lexical-structure.md)),
so they are called, take turbofish type arguments, and infer like any function —
they are simply provided by the compiler rather than user code. This is the
**only** mechanism for compiler-provided values; directives (`#name`) never
produce values, they only modify items.

Core intrinsics (extensible; not a closed list):

| Intrinsic | Purpose |
|-----------|---------|
| `$cast.<T>(x)` / `$cast(x)` | explicit type conversion (§6.5) |
| `$transmute.<T>(x)` | reinterpret the bits of `x` as `T` (same size) |
| `$new.<T>()` | allocate one zeroed, GC-managed `T`; yields `*mut T` (§6.9) |
| `$make.<[]T>(len[, cap])` | allocate a zeroed, GC-managed slice (§6.9) |
| `$size_of.<T>()` / `$align_of.<T>()` | layout queries (`usize`), `#const` |
| `$len(x)` | element count of an array or slice (`usize`); the core library's `.len()` method is written in terms of it |
| `$assert(cond[, msg])` | compile-time assertion (§6.10) |
| `$panic(msg)` | abort the program with a message |
| `$embed_file("path")` | splice a file's bytes as a compile-time `[]uint8` |

```
const bits := $transmute.<uint32>(3.14f32)
const p    := $new.<CatImage>()
const n    := $size_of.<CatImage>()
DATA :: $embed_file("logo.png")           // []uint8 baked into the binary
```

Many intrinsics are usable at compile time (they behave as `#const`), which is
why `$cast(8080)` and `$embed_file(...)` may appear on the RHS of `::`.

## 6.5 `$cast`

`$cast` is the sole explicit conversion intrinsic. Two forms:

```
$cast.<T>(expr)     // convert expr to the named target type T
$cast(expr)         // convert expr to the contextually-expected type
```

```
$cast.<HttpPort>(8080)                 // int literal -> distinct uint16
$cast.<*dyn ToJson>(&cat)              // *CatImage   -> ToJson trait object
const raw_id: string := $cast(self.id) // CatId       -> string (target from annotation)
```

`$cast` covers numeric widening/narrowing, `distinct` ↔ underlying, `*mut T` →
`*T`, pointer → `*dyn Trait`, `*T` → `c.ptr.<T>` (see
[11-c-ffi.md](11-c-ffi.md)), and any conversion the type system defines as
legal. It never performs a disallowed conversion — illegal casts are compile
errors, not run-time coercions.

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
explicit `$cast`.

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

## 6.9 Allocation intrinsics (`$new`, `$make`)

The language is garbage-collected, so there is no free. Fresh memory comes from
two intrinsics; std containers (`Vector`, `HashMap`, …) are built on top of them.

```
$new.<T>()               // one zeroed, GC-managed T          -> *mut T
$make.<[]T>(len)         // zeroed slice of `len` elements    -> []mut T
$make.<[]T>(len, cap)    // as above, with reserved capacity
```

```
const cat := $new.<CatImage>()          // *mut CatImage, all fields zeroed
const buf := $make.<[]uint8>(1024)      // []mut uint8, zeroed
let   xs  := Vector.<int>.new()         // std, wraps $make internally
```

Memory is zero-initialized unless the element type is `#raw` (see
[09-directives-and-attributes.md](09-directives-and-attributes.md)), in which
case it is left uninitialized and reads are only permitted in `#unsafe` scopes.

## 6.10 Compile-time statement items (`$assert`)

Intrinsic calls that return `void` can be used as standalone statements — not
only inside function bodies, but also as **items** inside a `struct`, `enum`,
`trait`, or `namespace` body. `$assert(cond[, msg])` is the canonical case: a
compile-time assertion. A body entry is therefore
`field | variant | decl | comptime-statement`.

```
Header :: #packed struct {
  $assert($size_of.<Self>() == 64, "Header must be 64 bytes")   // static check
  magic: uint32,
  len:   uint32,
}
```

`$assert` is checked during compilation and produces no run-time code. A
**run-time** assertion is the ordinary std function `assert(cond, msg)`, not an
intrinsic (see [08-error-handling-and-defer.md](08-error-handling-and-defer.md)).

## 6.11 Interpolated strings

`f"...{expr}..."` desugars to a call to the standard formatting routine that
concatenates the literal segments with each `expr`'s display output. It is an
ordinary expression of type `string`:

```
f"Invalid dimensions: {w}x{h}"
// ==> string.format("Invalid dimensions: {}x{}", w, h)   (illustrative)
```

Every interpolated value must satisfy the formatting/display contract for its
type.

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
