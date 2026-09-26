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
[Directives and attributes](../directives/)), in which case it's
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

## Where `&` points: escape and promotion

`&` works on anything — a local, a parameter, a field, an array element, or
a temporary (`&P { x: 1 }`, `&{ x in x + 1 }`). A pointer is valid for as
long as it is reachable, so the compiler decides where the thing it points
at lives:

- If the address **doesn't escape** the function, the value stays in the
  function's frame. `v.push(1)`, `bump(&mut n)`, `const p := &pt; p.x` — the
  common case, and free.
- If it **escapes**, the value is **promoted**: placed in a garbage-collected
  object instead, and `&` gives that object's address. A promoted local is
  read and written through the object from then on, so nothing can tell the
  difference except that the pointer stays valid.

```nest
Holder :: struct { p: *P }

stays :: func () -> i32 {
    const h := Holder { p: &P { x: 1 } }   // stays in the frame
    return h.p.x
}
goes :: func () -> Holder {
    return Holder { p: &P { x: 2 } }       // promoted: it leaves inside a Holder
}
counter :: func () -> *dyn Func() -> i32 {
    let n := 0
    return &{ in n += 1; n }               // promoted: the closure outlives the call
}
```

An address **escapes** when it — or a pointer derived from it — can still be
reached after the function returns. It escapes when it is:

1. **returned** (or given to `break`);
2. **stored through a pointer**, into memory that isn't one of the
   function's own locals;
3. **passed to a call that keeps it**. Each function is analyzed for which
   of its parameters it keeps — stores, returns out of reach, or passes on to
   a call that keeps them — so `list.push(x)` doesn't promote `list`. A call
   the compiler can't see into (through a `*dyn`, a function pointer, or to
   a C function) keeps everything. A function that only *returns* its
   argument doesn't keep it: its result is followed instead.

*Derived* covers `&x.field`, `&arr[i]`, sub-slices, casts, a `*dyn` made
from the pointer, and any value **holding** one: a struct, tuple or enum
built with it, a closure that copies it, a field read out of such a value,
and a local any of these is stored in. That is why `h` above keeps its
temporary in the frame while `goes` promotes its own: the question is
whether the *holder* escapes.

The analysis doesn't follow control flow: a pointer that escapes on one path
promotes its target on every path. It can promote something that could have
stayed on the stack — an extra allocation — but never the other way round. A
promoted value is an ordinary collected object: the collector finds it from
any live pointer and frees it once nothing reaches it.
