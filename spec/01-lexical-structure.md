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

### Intrinsic identifiers (`$name`)

An identifier may begin with `$`, forming a **compiler intrinsic** name:

```
intrinsic = '$' identifier
```

`$cast`, `$new`, `$make`, `$size_of`, `$assert`, `$panic`, `$transmute`,
`$embed_file`, … are lexed as ordinary identifiers (just spelled with a leading
`$`) and are called, turbofished, and inferred like any function. They are the
only source of compiler-provided *values*; see
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.4. A `$`
identifier always resolves to a compiler intrinsic and is never a namespace
member. User code cannot declare `$` names.

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
`dyn` forms a trait object (`dyn Trait`). Note that `cast` and `assert` are **not** keywords: `$cast` is an
intrinsic and `assert` is a std function (compile-time assertion is `$assert`).

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

### Floating-point literals

```
3.14
1.0e-9
6.022e23
```

Has the type `comptime_float` until context assigns a concrete float type
(`f16`/`f32`/`f64`/`f80`/`f128`); defaults to `f64`.

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
`string` type stores a pointer and a byte length.

**Interpolated strings** are prefixed with `f`. Inside them, `{ expr }` splices
the result of `expr` (which must satisfy the display/format contract). Braces are
escaped by doubling: `{{` and `}}`.

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
@  #  $             // attribute / directive / intrinsic sigils
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
