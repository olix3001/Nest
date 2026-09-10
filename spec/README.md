# Language Specification

> **Status:** Draft. This document specifies the syntax and name-resolution
> model of the language. Semantics are described only where they constrain
> syntax.

The language is statically typed, garbage-collected, and compiled to LLVM IR.
Its design combines:

- **Go-style pointers** — explicit `*T` / `&x`, automatic dereferencing, no
  pointer arithmetic, lifetimes managed by a garbage collector; immutable by
  default (`*mut T` to write), never null.
- **A Rust-style error model** — errors are ordinary values carried in a
  `Result.<T, E>` sum type, propagated with the explicit `.?` operator (backed by
  a `Try` trait), never thrown.
- **`defer`** — scope-exit cleanup that runs in LIFO order.
- **`$`-intrinsics and `#`-directives** — compiler-provided values are intrinsics
  (`cast`, `new`, `make`, `size_of`, `embed_file`, …); compiler behavior is
  changed by directives (`#packed`, `#align`, `#soa`, `#inline`, `#const`,
  `#raw`, `#unsafe`). Implementations use the `impl` keyword, not a directive.
- **Operators defined in the core library** — `+`, `-`, `*`, comparison, indexing,
  and the rest are sugar for trait methods (`Add`, `Sub`, `Ord`, `Index`, …), not
  compiler built-ins; the compiler reaches those traits — and `Try`, `Iterator`,
  `Drop`, `Result`/`Option` — through the `#lang("…")` **language-item** directive
  (see [06](06-expressions-and-operators.md) §6.13, [09](09-directives-and-attributes.md) §9.3).
- **Strong compile-time reflection** — *planned*; the design reserves the
  attribute/layout machinery it needs (see
  [12-reflection.md](12-reflection.md)).
- **A single uniform declaration operator (`::`)** for everything known at
  compile time: constants, types, functions, traits, and namespaces.

## Design principles

1. **One binding operator for compile-time things.** `name :: value` binds any
   compile-time-known entity. There is no separate keyword for "type alias",
   "function definition", or "constant" — they are all `::` bindings whose
   right-hand side happens to be a type, a `func`, or a value.

2. **Everything is a namespace.** There is no "module" concept and no `module`
   keyword: a source file is a namespace, an inline `name :: namespace { ... }`
   is a namespace, and a type's methods live in an `impl` namespace. Same
   construct throughout. See
   [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md).

3. **Private by default.** Every item is private to its enclosing namespace
   unless marked `@public`. Visibility is the only access concept.

4. **Explicit over implicit, except where inference is unambiguous.** `cast`,
   error propagation (`.?`), and taking a reference (`&` / `&mut`) are always
   visible in source. Types of locals, enum variants, struct literals, and most
   generic arguments may be inferred from context.

5. **Checked by default, not memory-safe.** Bounds and uninitialized-read checks
   are on by default; `#raw` / `#unsafe` opt out for speed, and C interop
   (`c.ptr`) is where nullability and unchecked pointers are quarantined.

## Reading order

| File | Topic |
|------|-------|
| [01-lexical-structure.md](01-lexical-structure.md) | Source encoding, tokens, comments, literals, keywords |
| [02-declarations-and-bindings.md](02-declarations-and-bindings.md) | `::` vs `:=`, `let` / `const`, assignment, `distinct` |
| [03-types.md](03-types.md) | Primitives, `*mut`/`[]mut`, structs, enums, traits, `dyn`, `Option`, `Vector` |
| [04-namespaces-and-name-resolution.md](04-namespaces-and-name-resolution.md) | Namespaces, `import`, visibility, `impl`, generic impls, merging, lookup rules |
| [05-functions-and-generics.md](05-functions-and-generics.md) | `func`, `#const`, parameters, named arguments, generics, `.<T>` / `.<_>` |
| [06-expressions-and-operators.md](06-expressions-and-operators.md) | Literals, calls, `$`-intrinsics, `cast`, `new`/`make`, precedence |
| [07-patterns-and-matching.md](07-patterns-and-matching.md) | Patterns (ranges, deref, slices, or-patterns, guards), `match` |
| [08-error-handling-and-defer.md](08-error-handling-and-defer.md) | `Result`, the `Try` trait, `.?` / `.!`, `defer` |
| [09-directives-and-attributes.md](09-directives-and-attributes.md) | `@public`/`@private`, custom attributes, `#packed`/`#align`/`#soa`/`#inline`/`#const`/`#raw`/`#unsafe` |
| [10-loops-and-iteration.md](10-loops-and-iteration.md) | `loop` / `while` / `for`, the `Iterator` trait, ranges, adapters |
| [11-c-ffi.md](11-c-ffi.md) | `core/c`, `c.ptr`, `extern`/`#c`, implicit boundary casts |
| [12-reflection.md](12-reflection.md) | Compile-time reflection — **planned**, design placeholder |
| [13-grammar.md](13-grammar.md) | Consolidated EBNF grammar |

## A note on notation

Grammar fragments use EBNF: `[x]` optional, `{x}` zero-or-more, `x | y`
alternation, `'x'` literal terminal. Non-normative examples are shown as code
blocks. Source-file extensions in path examples are shown as `.ext`; the actual
extension is not fixed by this document.
