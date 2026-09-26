---
title: Control flow
description: if, match, .match, if match, and the three loop forms.
---

## `if`

`if`/`else` is an expression whose arms are blocks of a common type:

```nest
const label := if port == 80 { "http" } else { "custom" }
```

## `match` and `.match`

```nest
match = expr '.match' '{' arm { ',' arm } '}'
arm   = pattern '=>' ( expr | block )
```

`match` is postfix on the scrutinee, evaluates the first arm whose pattern
matches, and yields that arm's value — all arms share a common type.

```nest
return result.match {
    .ok(cats) => {
        assert(cats.len() > 0, "Cat array was empty")
        Response.redirect(cats[0].url)
    },
    .err(e) => e.match {
        .network_error(msg) => Response.internal_server_error(msg),
        .parse_error        => Response.bad_request("Invalid payload format"),
    },
}
```

A `match` over an enum, `Option`, `Result`, or bounded value must be
**exhaustive**: every case covered, or a wildcard `_`/bare-identifier arm
handles the rest — a non-exhaustive match is a compile error. Guards don't
count toward exhaustiveness. Range and or-patterns *are* considered —
`0..=255` fully covers a `u8`.

### Patterns

```nest
_                                        // wildcard
mut x                                    // (mutable) binding
x @ pattern                              // bind the whole, match inside
.variant(a, b)  .variant { a, b }        // enum variant, tuple or record payload
{ a, b, .. }    Type(a, b)               // struct (also `.{ a, b }`) / tuple struct
(a, b)          [first, .. rest]         // tuple / slice
pattern | pattern                        // or-pattern
&pattern                                 // dereference: match through a pointer
pattern if guard                         // guard (match arms only)
lo..<hi  lo..=hi  ..<hi  ..=hi  lo..     // ranges
```

```nest
// literal + range + or-pattern
code.match {
    200       => "ok",
    301 | 302 => "redirect",
    400..=499 => "client error",
    500..     => "server error",
    _         => "unknown",
}

// @-binding, guard
msg.match {
    m @ .text(s) if s.len() > 280 => truncate(m),
    m                              => m,
}

// dereference pattern
node.match {
    &.leaf(v)      => v,
    &.branch(l, r) => sum(l) + sum(r),
}

// slice patterns
xs.match {
    []            => "empty",
    [only]        => "one",
    [first, .. _] => "many",
}
```

`*` and `self` are for `import`s alone: `* :: import <std>` globs a
namespace's members, and `{ self, Json } :: import <std/http>` binds the
namespace itself beside a member (see
[Namespaces and packages](../namespaces/)). Anywhere else, `self` in a pattern
is an error.

For **bindings** (`::`/`let`/`const`, not `match`), the pattern must be
**irrefutable** — it must match every value of the operand's type. Struct,
tuple, slice-with-rest, and namespace patterns are irrefutable; enum-variant,
literal, and range patterns are refutable and only allowed in `match` (or
`if match`, below).

```nest
const { width, height } := cat         // bind two fields
const { url: u, .. } := cat            // bind `url` as `u`, ignore the rest
const (a, b) := pair                    // tuple
let   [first, .. rest] := xs            // slice: head + remaining
```

A struct pattern may also be written with a leading dot, `.{ width, height }`,
the way an anonymous struct value is: the two spellings are the same pattern.

## `if match`

A single refutable pattern can be tested inline, binding on success:

```nest
if match .some(v) := lookup(key) {
    use(v)
} else {
    handle_missing()
}
```

This is sugar for a two-arm `match`, and the idiomatic way to handle one
case without full exhaustiveness.

## Loops

There are three looping constructs, all expressions; only `loop` yields a
value via `break`:

```nest
loop  block                       // infinite loop; exit with `break [value]`
while cond block                  // pre-tested conditional loop
for pattern in iterable block     // iterate an iterator
```

```nest
const first_even := loop {
    const n := next()
    if n % 2 == 0 { break n }
}

let i := 0
while i < 10 {
    io.println(i)
    i += 1
}
```

`break`/`continue` control the innermost loop. `break value` is valid only
in a `loop`, and makes the whole `loop` expression evaluate to `value`;
`while` and `for` always evaluate to `void`.

### `for` and `Iterator`

`for` doesn't know about arrays or ranges specifically — it's defined
entirely in terms of the std `Iterator` trait:

```nest
Iterator :: #lang("iterator") trait {
    Item :: type
    next :: func (self: *mut Self) -> Option.<Self.Item>
}
```

`for pat in iterable block` obtains an iterator, then drives it:

```nest
for cat in cats {
    io.println(cat.url)
}
```

desugars to roughly:

```nest
{
    let __it := cats.iter()
    loop {
        __it.next().match {
            .some(cat) => { io.println(cat.url) },
            .none      => break,
        }
    }
}
```

Because the loop variable comes from a pattern, destructuring works
directly: `for (i, cat) in cats.iter().enumerate() { ... }`. A container hands
out an iterator through `IntoIterator` (`#lang("into_iterator")`); slices,
`Vec`, ranges, and every iterator implement it.

Ranges are themselves iterators, over a `core` trait `Step`:

```nest
for i in 0..<n          { ... }   // 0, 1, ..., n-1  (half-open)
for i in 0..=n          { ... }   // 0, 1, ..., n    (inclusive)
for i in (0..<n).step(2) { ... }  // every other one — an adapter, below
```

A range with no start (`..`, `..<b`, `..=b`) has no first element, so
iterating one **panics** rather than running zero times.

### Adapters

Because iteration is a trait, ordinary methods compose lazily over any
iterator. An iterator writes only `next`; the rest are default methods on
`Iterator` in `core/iter`:

| Adapters (lazy) | Consumers (run the loop) |
|---|---|
| `map(f)`, `filter(keep)`, `enumerate()`, `zip(other)`, `chain(other)`, `take(n)`, `skip(n)`, `step(n)`, `take_while(p)`, `skip_while(p)`, `flatten()`, `flat_map(f)`, `peekable()` | `each(f)`, `fold(init, f)`, `reduce(f)`, `count()`, `any(p)`, `all(p)`, `find(p)`, `last()`, `max()`, `min()`, `max_by(cmp)`, `min_by(cmp)`, `collect()` |

```nest
{ Vec } :: import <std/collections>

const urls := cats
    .iter()
    .filter({ c in c.width > 0 })
    .map({ c in c.url })
    .collect.<Vec.<str>>()
```

Each adapter wraps the iterator (and the closure it was given) in a small
struct that is an iterator itself, so the chain above is one nested type and
no work happens until a consumer — `for`, `collect`, `fold`, … — pulls
elements through `next`. Being monomorphized, the chain compiles to the loop
it stands for.

`collect` builds whatever collection the turbofish or the context names, as
long as it implements `FromIterator` — in `std`, `Vec`, `HashMap` (from
`(key, value)` pairs) and `String` (from `char`s). An iterator is also its own
`IntoIterator`, so `for` walks a chain directly:
`for x in xs.iter().map(f) { ... }`.

`reduce` is `fold` seeded with the first element, answering `.none` for an
empty iterator. `max`/`min` need an element with an order of its own (`Ord`,
which the integers, `char` and `bool` have); floats go through
`max_by`/`min_by` with a comparison. `peekable()` gives an iterator whose
`peek()` answers what `next` will, without taking it.

```nest
{ Ordering } :: import <core/cmp>

(1..=4).reduce({ a, b in a + b })     // .some(10)
xs.iter().max()                       // the greatest element
fs.iter().max_by({ a, b in            // floats need a comparison
    if a < b { Ordering.less } else if a > b { Ordering.greater } else { Ordering.equal }
})
```

**Writing through an iterator:** `xs.iter_mut()` (on a `[]mut T` or a `Vec`)
hands out a `*mut T` per element — `for p in xs.iter_mut() { p.* = 0 }`.
A `HashMap`'s `iter()` hands out `(key, value)` copies, `keys()` and
`values()` one half of each, and `iter_mut()` a pointer to each value.

**As a trait object:** `*mut dyn Iterator.<Item = i32>` works — `next` goes
through the vtable, and the adapters still apply to the pointer. The adapters
and consumers are bounded `<Self: Sized>`, which is what keeps them out of the
vtable ([traits](../traits/#self-sized--methods-for-implementing-types-only)).
