---
title: Operators
description: Precedence, cast, and how every operator is sugar for a trait method call.
---

Operators are **not** built into the compiler beyond parsing: each one is
sugar for a method call on a core-library trait, the one carrying the
matching `#lang(...)` tag. A type supports an operator by implementing that
trait — the same mechanism user code uses to overload it. `int + int` and
`Vec3 + Vec3` are the same construct, both `Add.add(a, b)`, differing only
in which `impl` is selected.

## Precedence

Highest to lowest; same-row operators associate left-to-right unless noted:

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

Comparison operators don't chain — `a < b < c` is a parse error, write `a <
b && b < c`. `and`/`or` are exact synonyms for `&&`/`||`. Bitwise operators
require integer operands; mixing signed/unsigned or differing widths needs
an explicit `cast`. Dereference is the *postfix* `p.*`, not a prefix `*p`
(§3.2) — prefix `*` appears only in type positions.

The built-in numeric types satisfy the operator traits too, but not through
source `impl`s — they're width-parameterized (`u4096` is as real as `u8`),
so there's no finite set to write. The compiler carries one **builtin row**
per primitive operator instead, considered uniformly with user impls during
selection: a primitive `a + b` and a `Vec3 + Vec3` go through the same
selection and reach the same call shape, the row just marks the result as a
machine instruction rather than a function body.

## The trait registry

| Syntax | `#lang` tag | Trait | Method |
|--------|-------------|-------|--------|
| `a + b` | `"add"` | `Add` | `add(self, rhs) -> Output` |
| `a - b` | `"sub"` | `Sub` | `sub(self, rhs) -> Output` |
| `a * b` | `"mul"` | `Mul` | `mul(self, rhs) -> Output` |
| `a / b` | `"div"` | `Div` | `div(self, rhs) -> Output` |
| `a % b` | `"rem"` | `Rem` | `rem(self, rhs) -> Output` |
| `a & b` | `"bitand"` | `BitAnd` | `bitand(self, rhs) -> Output` |
| `a \| b` | `"bitor"` | `BitOr` | `bitor(self, rhs) -> Output` |
| `a ^ b` | `"bitxor"` | `BitXor` | `bitxor(self, rhs) -> Output` |
| `a << b` | `"shl"` | `Shl` | `shl(self, rhs) -> Output` |
| `a >> b` | `"shr"` | `Shr` | `shr(self, rhs) -> Output` |
| `-a` | `"neg"` | `Neg` | `neg(self) -> Output` |
| `~a` | `"bitnot"` | `BitNot` | `bitnot(self) -> Output` |
| `a == b`, `a != b` | `"eq"` | `Eq` | `eq(self, rhs) -> bool` |
| `a < b`, `<=`, `>`, `>=` | `"ord"` | `Ord` | `cmp(self, rhs) -> Ordering` |
| `a[i]` (read) | `"index"` | `Index` | `index(self: *Self, i) -> *Output` |
| `a[i] = v` (write) | `"index_mut"` | `IndexMut` | `index_mut(self: *mut Self, i) -> *mut Output` |

Compound assignment is deliberately absent — `a += b` is sugar for `a = a +
b`, not an operator of its own, so it needs no `#lang` item and inherits
whatever `Add` the type has.

### Desugaring

- Arithmetic/bitwise/shift binaries lower directly: `a + b` ⇒
  `Add.add(a, b)`, and so on.
- Equality: `a == b` ⇒ `Eq.eq(a, b)`; `a != b` ⇒ its negation.
- Ordering: all four relations go through **one** method, `Ord.cmp`, tested
  against its `Ordering` result — `a < b` is "`cmp` answered `.less`", `a >=
  b` is "`cmp` did not answer `.less`".
- The numeric core is the exception to the last two: `i32 == i32` and `i32
  < i32` are the machine's own compare, not routed through `cmp`. `Eq` and
  `Ord` are what a *user* type is compared by.
- Indexing: `a[i]` in value position ⇒ `Index.index(&a, i).*`; as an
  assignment's place ⇒ `IndexMut.index_mut(&mut a, i).*`. The built-in
  sequences (`[]T`, `[]mut T`, `[N]T`) are the one exception — they
  implement `Index` and **not** `IndexMut`, since a sequence's write
  permission already lives in its type (`[]mut T`), not in the container's
  own mutability.

### Not a trait method

Assignment, `&&`/`||` (short-circuiting needs to *not* evaluate the right
operand), the `.?`/`.!` `Try` operators (their own trait, see
[Errors](/language/errors/)), and `.match` stay built into the compiler and
dispatch on nothing in this table.

## `cast`

`cast` is the sole explicit conversion intrinsic, in two forms:

```nest
cast.<T>(expr)   // convert expr to the named target type T
cast(expr)       // convert expr to the contextually-expected type
```

```nest
cast.<HttpPort>(8080)                // int literal -> distinct u16
cast.<*dyn ToJson>(&cat)             // *CatImage   -> ToJson trait object
const raw_id: str := cast(self.id)   // CatId       -> str (target from annotation)
```

It covers numeric widening/narrowing, `distinct` ↔ underlying, `*mut T` →
`*T`, pointer → `*dyn Trait`, and `*T` → `c.ptr.<T>` (see
[C FFI](/language/c-ffi/)). An illegal conversion is a compile error, never
a run-time coercion.

A `cast` **the program writes** may lose precision — narrowing keeps the low
bits, exactly what the machine does:

```nest
X: u8 :: cast.<u8>(400)   // 144 — the low 8 bits
```

The conversion the **compiler inserts** to settle an untyped literal may
not: `let y: u8 := 300` is an error, because nothing in the source said 300
should become 44. Write `cast.<u8>(x)` when that's what you mean.

## Blocks and `if` as expressions

A block evaluates to its trailing expression (or `void`); `if`/`else` is an
expression whose arms are blocks of a common type:

```nest
const label := if port == 80 { "http" } else { "custom" }
```

**A `func` body is the one exception** — its trailing expression is not an
implicit return. A function returns `void` unless it uses `return`; nested
blocks (arms, initializers) do yield their tail, only the outermost
function-body block discards it.
