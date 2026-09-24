---
title: Memory
description: The GC, new, make, drop, and the gc_* intrinsics for C interop.
---

Nest is garbage collected — pointers are always live while reachable, and a
free is never *required*. Fresh memory comes from two intrinsics; std
containers (`Vector`, `HashMap`, …) are built on top of them.

```nest
new.<T>()               // one zeroed, GC-managed T          -> *mut T
make.<[]T>(len)         // zeroed slice of `len` elements    -> []mut T
make.<[]T>(len, cap)    // as above, with reserved capacity
drop(p)                 // free `p`'s object now             -> void
```

```nest
const cat := new.<CatImage>()          // *mut CatImage, all fields zeroed
const buf := make.<[]u8>(1024)         // []mut u8, zeroed
let   xs  := Vector.<i32>.new()        // std, wraps make internally
```

Memory is zero-initialized unless the element type is `#raw` (see
[Directives and attributes](/language/directives/)), in which case it's
left uninitialized and reads are only permitted in `#unsafe` scopes.

## `drop`

`drop(p)` releases an object before the collector would have. The compiler
already does exactly this wherever it can prove an object doesn't outlive
the scope that made it; `drop` is that same operation written by hand, for
the cases the proof can't reach — a buffer finished with well before its
scope ends.

Writing it takes the question on, and the language holds you to the part
it can check:

```nest
let p := new.<Node>()
let v := p.*.x
drop(p)
return p.*.x   // error: `p` is used after it was dropped
```

- The compiler stops inserting a drop of its own for that value, so the
  object is freed once.
- Using the name afterward is a compile error, along with dropping it
  twice, or dropping — inside a loop — something declared outside it.

What the check can't see is an **alias** made before the drop:

```nest
let q := p
drop(p)
q.*.x   // not caught
```

Catching that needs ownership, which this language doesn't have — `drop`
is the one intrinsic whose correctness is partly the author's, which is
also why its parameter is `*mut T`: the permission to do the most
destructive write there is belongs in the type.

## GC intrinsics for C interop

None of these are needed by ordinary code — they exist because two things
the collector can't see from outside are otherwise unsayable: a pointer
that has escaped to C, and a pointer C will hold for longer than one call.
All four return `void`.

| Intrinsic | Meaning |
|---|---|
| `gc_collect()` | Request a collection now. A hint, not a guarantee. |
| `gc_keep_alive(x)` | A no-op that counts as a use, so `x` stays reachable up to this point. |
| `gc_pin(x)` | Make the object immovable. |
| `gc_leak(p)` | Keep the object alive until `drop(p)`, reachable or not. |

`gc_keep_alive` exists for one specific failure: a value's live range ends
at its last **read**, so this is wrong —

```nest
let buf := make.<[]u8>(1024)
let p   := &buf[0]
c_write(p)   // `buf` is already dead here — nothing reads it again
```

The collector may move or free `buf` during the call even though C is
using its address. `gc_keep_alive(buf)` *after* the call extends the live
range across it.

`gc_pin` is for handing a pointer to C for longer than one call — a
callback registration, a buffer the other side keeps. A pinned object is
never moved, so the address C holds stays the object's address. It is
**not** kept alive by this alone: it's collected once nothing in the
program reaches it, since memory C allocated isn't somewhere the collector
looks. A program handing C a pointer to keep also keeps a reference of its
own, in a `#static` say, for as long as C may use it.

`gc_leak(p)` is that reference when the program has nowhere to put one: the
object stays alive whether anything reaches it or not, until `drop(p)`
releases it. Without the `drop`, it lives as long as the process — a leak,
by request.

## Allocation on the heap for storage — `boxed`

`core/mem`'s `boxed(value)` puts a value whose type has no name onto the
heap, returning a `*dyn Trait` — the way to store several closures (or
other values known only by a bound) behind one uniform pointer type. See
[Closures and Func](/language/closures/) for the full picture.

```nest
{ boxed } :: import <core/mem>

const a: *dyn Func(i32) -> i32 := boxed({ x in x + 1 })
```
