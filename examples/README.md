# Example programs

Small, single-file Nest programs used as parser/compiler test fixtures. They use
only basic language features and **no** libraries (no `import <std/...>`), so they
exercise the front end without depending on a standard library that does not yet
exist.

| File | Exercises |
|------|-----------|
| `recursion.nest` | functions, `if`/`return`, `while`, recursion |
| `loops.nest` | `for … in` ranges, `loop`/`break value`, compound assignment |
| `shapes.nest` | `struct`, `enum` with record variants, `impl`, `.match` |
| `math.nest` | a small library of `@public` functions, imported by the next file |
| `use_math.nest` | imports `math.nest` (a sibling file) and calls into it |
| `operators.nest` | operator traits (`Add`, `Mul`, …) and their impls |
| `inference.nest` | generics, trait bounds, associated types, projection |
| `errors.nest` | `.?` propagation on `Result` **and** `Option`, `.!` abort, `for` over a slice |
| `arrays.nest` | `[N]T` lengths, `<const N: usize>` value generics, `.len` / `len`, array→slice |
| `dispatch.nest` | `@using` upcasts, `dyn` trait objects, bound-directed calls |

`gc/` holds three programs about the collector, each run by a test in
`nestc/src/codegen/llvm/tests.rs`. They import `core`, and their exit status is
the answer: `collects.nest` (memory nothing reaches is collected),
`escapes.nest` (escape analysis frees nothing still reachable; run it with
`NEST_GC_POISON=1`) and `leak.nest` (`gc_leak` keeps an object alive until
`drop`).

`use_math.nest` is the cross-file example: `math :: import "math.nest"` binds the
sibling file's namespace, and its `@public` functions are reached as
`math.add(...)`, `math.factorial(...)`.

Parse any of them with the bootstrap compiler:

```
cargo run --manifest-path nestc/Cargo.toml -- examples/recursion.nest
```
