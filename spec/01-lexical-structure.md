# 01 — Lexical Structure

## 1.1 Whitespace and line structure

Whitespace (spaces, tabs) separates tokens and is otherwise
insignificant. The language is **not** whitespace-sensitive: blocks are
delimited by `{ }`, and statements are separated by newlines or `;`.

A statement ends at a newline **unless** the line ends in a position where the
statement is syntactically incomplete (an open bracket, a binary operator, `,`,
`->`, `=>`, `::`, `:=`, or `.`), or the next line begins such that the statement
is syntactically incomplete (`+`, `::`, `.`), in which case the statement continues
on the next line. An explicit `;` may always be used to terminate a statement.

```
const x := a +
           b        // one statement; line continues after `+`

const y := a
         + b        // also one statement
```

## 1.2 Comments

```
// line comment — runs to end of line

/* block comment
   /* may nest */
   still inside the outer comment */
```

Block comments nest. There are no doc-comment semantics baked into the lexer;
tooling may treat a `//` comment immediately preceding a declaration as
documentation, `///` may be used by external tools, as lexer treats this
like a normal comment. Custom attributes may be used for auto-generating docs.

## 1.3 Identifiers

```
identifier   = ident_start { ident_continue }
ident_start  = letter | '_'
ident_continue = letter | digit | '_'
letter       = any Unicode letter (category L*)
digit        = '0'..'9'
```

Identifiers are case-sensitive. There is no enforced casing convention, but the
standard style is `snake_case` for values, functions, and **enum variants**,
`PascalCase` for types and namespaces bound to types, and `SCREAMING_CASE` for
compile-time constants.

A leading `_` marks an identifier as intentionally unused; the compiler will not
warn about an unused binding whose name begins with `_`.

### Intrinsics are ordinary identifiers

There is **no sigil for compiler-provided functions**. `cast`, `new`, `make`,
`size_of`, `assert`, `panic`, `transmute`, `embed_file` are ordinary names,
declared in `core` as bodyless `#intrinsic` functions and called, turbofished and
inferred like any other — see
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.4. Which of
them are in scope without an import is the prelude's business
([04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md)
§4.6), and a program may shadow any of them the way it may shadow any other
name.

## 1.4 Keywords

Reserved words that cannot be used as identifiers:

```
func    struct   enum    trait    namespace   distinct
let     const    mut     return   defer       match     import
if      else     for     while    loop        break     continue
dyn     true     false   impl
and     or       not     extern
```

There is no `nil` / `null` keyword: pointers are never null, and the absence of
a value is the `Option` enum's `.none` case (see
[03-types.md](03-types.md) §3.6).

`self` and `Self` are **not** keywords — they are ordinary identifiers, reserved
by name resolution rather than the lexer. `self` is the receiver **parameter
binding** (the parameter literally named `self`), and `Self` is a name **bound to
the implementing type** inside a `trait` or `impl` (as if by an implicit `::`
constant), so `Self`, `Self.Residual`, and `*Self` are plain paths. Both names are
reserved: user code may not rebind them, and using either outside a
trait/impl/method is a name-resolution error. `impl` introduces a trait/inherent implementation
attached to a type (`impl T { ... }`, `impl Trait for T { ... }`); `for` doubles
as the loop keyword and the trait-impl separator (unambiguous by position). `mut`
marks a mutable reference (`*mut T`, `[]mut T`) or a mutable binding in a pattern;
`dyn` forms a trait object (`dyn Trait`). Note that `cast` and `assert` are **not** keywords: both are ordinary functions
declared in `core` (§6.4).

`type` is **not** a reserved word: it is a contextual keyword, recognised only as
the RHS of an associated-type binding (`Item :: type`). Elsewhere it is an
ordinary identifier, so `type` may name values, fields, and parameters. (There is
no `T: type` kind bound — a bare generic parameter is already a type; only `const`
marks a value parameter.) `extern` selects an ABI on a `func` literal
(`extern("c") func ...`) or heads an `extern("c") { ... }` block; it is a full
keyword and sits immediately before `func` or the block, never left of a binding
name.

Note: `module` is **not** a keyword — namespaces subsume it (see
[04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md)).

## 1.5 Literals

### Integer literals

```
123          decimal
1_000_000    underscores allowed as digit separators
0xFF         hexadecimal
0o17         octal
0b1010       binary
```

An integer literal has the type `comptime_int` — an arbitrary-precision compile-time
integer — until it is used in a typed context, where it implicitly converts to
any integer type whose range holds its value (a value that does not fit is a
compile error). A literal that no context constrains defaults to `isize`.

The conversion the compiler inserts must be exact; a `cast` the program writes
narrows the way the machine does. `let y: u8 := 300` is an error and
`cast.<u8>(300)` is `44` — see §6.5.

### Floating-point literals

```
3.14
1.0e-9
6.022e23
```

Has the type `comptime_float` until context assigns a concrete float type
(`f16`/`f32`/`f64`/`f80`/`f128`); defaults to `f64`. The width it settles on must
be able to hold it: a literal that would overflow to infinity, or a non-zero one
that would underflow to zero, is a compile error. Rounding is not — no decimal
fraction is exactly a binary float — and a written `cast` may lose either way
(§6.5).

### Boolean literals

```
true   false        // bool
```

There is no null/nil literal. The empty case of an `Option.<T>` is the variant
`.none` (see [03-types.md](03-types.md) §3.6).

### String literals

```
"hello"                       // ordinary string
"line1\nline2"                // escapes: \n \t \r \\ \" \0 \u{1F600}
f"port is {port}"             // interpolated string (see below)
```

Strings are UTF-8, immutable, and length-prefixed (not NUL-terminated). The
`str` type stores a pointer and a byte length.

A string literal has the type `comptime_str` until its use site settles it, the
way an integer literal is a `comptime_int`. It may become any of three types:

```
str        the default: what a literal is when nothing else pins it
[]u8       the same bytes, viewed as a byte slice
[]char     the same text, transcoded to code points at compile time
```

Those three and no others: they are exactly the types the compiler can produce
from the literal's own bytes. A library string type (`String`, a rope, a small
buffer) is reached by a conversion the library defines, not by the literal
changing type. The slices are the read-only ones — a literal lives in read-only
data, so `[]mut u8` is not among them.

A literal is `str` for every purpose other than being *passed to* one of the
other two: a method call, an operator or a field access settles it on `str`
immediately, because that is the type whose impls a string has.

### Byte-string literals

```
b"GET "                       // []u8
b"\x00\x01\xff"               // any octet, whether or not it is UTF-8
```

`b"..."` is a **byte** string: its type is `[]u8` and only `[]u8`, and it carries
no UTF-8 promise. It exists for the data that is not text — a magic number, a
protocol frame, a lookup table — where writing `"\u{...}"` would mean something
other than the bytes intended.

Its escapes are `\n \t \r \\ \" \0` and `\xNN`, where `NN` is two hex digits
naming one byte. `\u{...}` is **not** allowed: a code point above 127 is more
than one byte, so the escape would silently mean something other than it says.
The literal's own characters must be ASCII for the same reason; write anything
else as `\xNN`.

**Interpolated strings** are prefixed with `f`. Inside them, `{ expr }` splices
the result of `expr` (which must implement `core`'s `Display`; see
[06-expressions-and-operators.md](06-expressions-and-operators.md)). Braces are
escaped by doubling: `{{` and `}}`.

A **lone** `}` is an error rather than a literal brace. Accepting it would mean a
program that gains a `{` earlier in the same literal silently changes what the
`}` means.

The braces are matched by lexing what is between them, not by scanning for the
next `}`, so the expression may contain a string holding a brace, a struct
literal, or another interpolated string. It may **not** contain a newline: the
literal is one line, and so is everything spliced into it.

```
f"{w}x{h}"
f"Invalid dimensions: {w}x{h}"
f"literal brace: {{"
```

An interpolated string is syntactic sugar for a formatting call; see
[06-expressions-and-operators.md](06-expressions-and-operators.md).

### Character literals

```
'a'    '\n'    '\u{1F600}'
```

A character literal has type `char` (a Unicode scalar value, 32-bit).

## 1.6 Operators and punctuation

Tokens recognized by the lexer:

```
::  :=  :   ->  =>  ..  ..<  ..=  .<  .{
.*  .?  .!                       // postfix: deref / Try-return / Try-abort
+  -  *  /  %
==  !=  <  <=  >  >=
&&  ||  !            // also spelled `and` `or` `not`
&  |  ^  ~  <<  >>   // bitwise
=  +=  -=  *=  /=  %=
&                   // address-of (prefix) / bitwise-and (infix)   (& mut for a mutable ref)
*                   // pointer type (prefix) / multiply (infix)
@  #                // attribute / directive sigils
.  ,  ;  (  )  {  }  [  ]
```

The multi-character tokens are single lexical units: `.<` (generic-argument
open), `.{` (inferred composite literal open), `.*` (dereference), `.?`
(Try: unwrap-or-return), `.!` (Try: unwrap-or-abort), and the range tokens `..<`
(half-open) / `..=` (closed) / `..` (unbounded). Range operators are always
explicit: `a..<b` excludes `b`, `a..=b` includes `b`; a bare `..` (no `<`/`=`)
is the unbounded / rest token (`a..`, `..<b`, `..`; also the pattern rest). See
[06-expressions-and-operators.md](06-expressions-and-operators.md) and
[08-error-handling-and-defer.md](08-error-handling-and-defer.md). Note that
dereference is the Zig-style **postfix** `p.*`, not a prefix `*p`; the bare `*`
token is only the pointer-type prefix and the multiplication infix. There is no
standalone `?` operator — optionals are `Option.<T>` and propagation is `.?`.

## 1.7 The two "colon" tokens

Because they anchor the whole declaration model, note the three distinct
colon-family tokens up front:

| Token | Meaning | Detail |
|-------|---------|--------|
| `::`  | compile-time binding | `name :: value` — see [02](02-declarations-and-bindings.md) |
| `:=`  | runtime variable init | `let name := value` |
| `:`   | type annotation | `name: Type` |

They never overlap: `::` and `:=` are used where a declaration is expected, and
`:` only inside a type-annotation position.
