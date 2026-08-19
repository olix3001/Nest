# 10 — Loops and Iteration

There are three looping constructs. All are expressions; a `loop` (and only a
`loop`) can yield a value via `break`.

```
loop  block                       // infinite loop; exit with `break [value]`
while cond block                  // pre-tested conditional loop
for pattern in iterable block     // iterate an iterator
```

`break` and `continue` control the innermost loop. `break value` is valid only in
a `loop` and makes the whole `loop` expression evaluate to `value`; `while` and
`for` always evaluate to `void`.

```
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

## 10.1 The `Iterator` trait

`for` does not know about arrays or ranges specifically — it is defined entirely
in terms of the std `Iterator` trait. Anything implementing `Iterator` is
iterable.

```
Iterator :: #lang("iterator") trait {
  Item :: type                                // associated element type

  // Advance and produce the next element, or `.none` when exhausted.
  next :: func (self: *mut Self) -> Option.<Self.Item>
}
```

`Iterator` carries `#lang("iterator")` so the `for` desugaring below can name it
without hard-wiring iteration into the compiler (see
[09-directives-and-attributes.md](09-directives-and-attributes.md) §9.3); the
`IntoIterator` convenience is `#lang("into_iterator")`. `for` is defined against
those lang items, so any type implementing them is iterable.

`next` returns `.some(item)` until the sequence ends, then `.none` forever after.
The associated type `Item` is what the loop pattern binds.

## 10.2 `for` desugaring

`for pat in iterable block` is sugar built on two steps: obtain an iterator, then
drive it. A type is iterable if it *is* an `Iterator`, or if it provides `iter`
(the `IntoIterator` convenience below) producing one.

```
for cat in cats {
  io.println(cat.url)
}
```

desugars to roughly:

```
{
  let __it := cats.iter()              // or `cats` itself if already an Iterator
  loop {
    __it.next().match {
      .some(cat) => { io.println(cat.url) },   // `cat` is the `for` pattern
      .none      => break,
    }
  }
}
```

Because the loop variable comes from a **pattern**, destructuring in `for` works
directly:

```
for (i, cat) in cats.enumerate() { ... }        // tuple pattern
for .rect { w, h } in shapes { ... }            // (refutable patterns need a filter/guard)
```

An `IntoIterator`-style convenience trait lets containers hand out an iterator:

```
IntoIterator :: #lang("into_iterator") trait {
  Iter :: type                                  // must implement Iterator
  iter :: func (self: *Self) -> Self.Iter
}
```

Slices, arrays, `Vector`, `HashMap`, and ranges implement these in std. Iterating
a `[]mut T` can yield `*mut T` items for in-place mutation; iterating `[]T`
yields read-only elements.

## 10.3 Ranges as iterators

The range expressions from pattern syntax are also values that implement
`Iterator`, so they drive `for` loops:

```
for i in 0..<n     { ... }        // 0, 1, ..., n-1   (half-open)
for i in 0..=n     { ... }        // 0, 1, ..., n     (inclusive)
for i in (0..<n).step(2) { ... }  // std adapter
```

## 10.4 Iterator adapters

Because iteration is a trait, ordinary methods compose lazily over any iterator —
`map`, `filter`, `take`, `enumerate`, `zip`, `step`, terminating in a consumer
like `collect`, `sum`, or a `for` loop:

```
const names := cats
  .iter()
  .filter(func (c: *CatImage) -> bool { return c.width > 0 })
  .map(func (c: *CatImage) -> string { return c.url })
  .collect.<Vector.<string>>()
```

Adapters are lazy: no work happens until a consumer (`for`, `collect`, `sum`, …)
pulls elements through `next`. Being monomorphized, an adapter chain compiles to
the same code a hand-written loop would.
