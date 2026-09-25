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
for (i, cat) in cats.iter().enumerate() { ... } // tuple pattern
for .rect { w, h } in shapes { ... }            // (refutable patterns need a filter/guard)
```

`IntoIterator` lets a container hand out an iterator:

```
IntoIterator :: #lang("into_iterator") trait {
  Iter :: type                                  // must implement Iterator
  into_iter :: func (self: Self) -> Self.Iter
}
```

Slices, `Vec`, and ranges implement it, and so does every iterator — it is its
own `into_iter` (`impl <I: Iterator> IntoIterator for I`), which is what lets
`for` walk an adapter chain directly. Iterating
a `[]mut T` can yield `*mut T` items for in-place mutation; iterating `[]T`
yields read-only elements.

## 10.3 Ranges as iterators

The range expressions from pattern syntax are also values that implement
`Iterator`, so they drive `for` loops:

```
for i in 0..<n     { ... }        // 0, 1, ..., n-1   (half-open)
for i in 0..=n     { ... }        // 0, 1, ..., n     (inclusive)
for i in (0..<n).step(2) { ... }  // every other one (§10.4)
```

There is **one** such impl, over a `core` trait called `Step` that says what
stepping needs of an element type — how one bound stands to another, and what
the next value up is:

```
Step :: trait {
  step_cmp :: func (self: Self, rhs: Self) -> Ordering
  step_up  :: func (self: Self) -> Self
}

impl <T: Step> Iterator for Range.<T> { ... }
```

It is a trait of its own rather than `Ord` + `Add` because on a primitive those
operators are *instructions* rather than calls (§6.13), so an unbounded `T` has
no way to ask and a bound naming them is one the primitives do not satisfy.
`core` implements `Step` for both integer families and for `usize` / `isize`.

One impl rather than several is also what keeps inference working: in
`for x in 0..<4` the element type is decided by the **body**, and a single
blanket impl matches while it is still open, where a set of impls per integer
family would have to choose between them before the body was read.

A range with no start — `..`, `..<b`, `..=b` — has no first element, so
iterating one **panics** rather than running zero times.

## 10.4 Iterator adapters

An iterator writes only `next`. Everything else is a **default method** on
`Iterator` (`core/iter`), so every iterator has it:

- **Adapters**, lazy: `map(f)`, `filter(keep)`, `enumerate()`, `zip(other)`,
  `chain(other)`, `take(n)`, `skip(n)`, `step(n)`, `take_while(p)`,
  `skip_while(p)`, `flatten()`, `flat_map(f)`, `peekable()` (whose `peek()`
  answers what `next` will, by value, without taking it).
- **Consumers**, which run the loop: `each(f)`, `fold(init, f)`,
  `reduce(f)` (the first element seeds it; `.none` when empty), `count()`,
  `any(p)`, `all(p)`, `find(p)`, `last()`, `max()`/`min()` (for an `Ord`
  element; of equals, the last and the first), `max_by(cmp)`/`min_by(cmp)`
  (with a comparison, which is how floats are ordered), `collect()`.

Every one of them is bounded `<Self: Sized>` (§3.4). The integers, `usize`/
`isize`, `char` and `bool` implement `Ord` for generic callers; floats do not.

```
const urls := cats
  .iter()
  .filter({ c in c.width > 0 })
  .map({ c in c.url })
  .collect.<Vec.<str>>()
```

Each adapter is a small generic struct that is an iterator itself — `map`
returns `Map.<Self, F>`, whose impl is

```
impl <I: Iterator, B, F: Func(I.Item) -> B> Iterator for Map.<I, F> {
  Item :: B
  ...
}
```

— so a chain is one nested type, no work happens until a consumer pulls
elements through `next`, and, being monomorphized, the chain compiles to the
loop it stands for. The impl's `B` appears only in a bound: selecting the impl
solves it from the closure's `Output` (§5.4).

`collect` builds any collection that implements `FromIterator`:

```
FromIterator :: trait {
  Item :: type
  from_iter :: func <I: Iterator.<Item = Self.Item>> (it: I) -> Self
}
```

The collection is named by a turbofish (`.collect.<Vec.<i32>>()`) or by the
context (`const v: Vec.<i32> := xs.iter().collect()`). `core` declares the
trait and `std` implements it — for `Vec.<T>`, for `HashMap.<K, V>` from
`(K, V)` pairs (a later pair replaces an earlier one's value), and for `String`
from `char`s — so `core` knows nothing about `std`'s types.

**Writing through an iterator.** `xs.iter_mut()` on a `[]mut T` (and
`v.iter_mut()` on a `Vec`) hands out a `*mut T` per element:
`for p in xs.iter_mut() { p.* = 0 }`.

**Maps.** `m.iter()` hands out `(K, V)` copies, in no particular order;
`m.keys()` and `m.values()` one half of each; `m.iter_mut()` `(K, *mut V)`; and
`for (k, v) in m` walks the entries the map holds when the loop starts.

**Iterators as trait objects.** The adapters and consumers have no vtable
slots (`<Self: Sized>`), so `Iterator` is object-safe: `*mut dyn
Iterator.<Item = T>` calls `next` through its vtable, and the adapters still
apply through `impl <I: Iterator> Iterator for *mut I`.
