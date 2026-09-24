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
{ a, b, .. }    Type(a, b)               // struct / tuple-struct destructure
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

For **bindings** (`::`/`let`/`const`, not `match`), the pattern must be
**irrefutable** — it must match every value of the operand's type. Struct,
tuple, slice-with-rest, and namespace patterns are irrefutable; enum-variant,
literal, and range patterns are refutable and only allowed in `match` (or
`if match`, below).

```nest
const .{ width, height } := cat        // bind two fields
const (a, b) := pair                    // tuple
let   [first, .. rest] := xs            // slice: head + remaining
```

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
| `map(f)`, `filter(keep)`, `enumerate()`, `zip(other)`, `chain(other)`, `take(n)`, `skip(n)`, `step(n)` | `each(f)`, `fold(init, f)`, `count()`, `any(p)`, `all(p)`, `find(p)`, `collect()` |

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
long as it implements `FromIterator` (`Vec` does, in `std`). An iterator is
also its own `IntoIterator`, so `for` walks a chain directly:
`for x in xs.iter().map(f) { ... }`.
