# 08 — Error Handling and `defer`

Errors are **values**, not exceptions. There is no throw/catch and no stack
unwinding for ordinary failures. A fallible function returns a `Result`, and
callers must engage with both cases. This is the Rust model, adapted to the
language's syntax.

## 8.1 `Result.<T, E>`

`Result` is a prelude enum with **snake_case** variants:

```
Result :: enum <T, E> {
  ok(T),
  err(E),
}
```

A fallible function declares its success and error types:

```
get :: func <T> (self: *Client, url: string) -> Result.<T, FetchError> {
  return .err(.network_error("Failed to reach endpoint"))
}
```

The error type is usually an enum enumerating failure modes, so callers can
`match` on precisely what went wrong:

```
FetchError :: enum {
  network_error(string),
  parse_error,
}
```

Construction is context-inferred, so both the wrapper and the payload use the `.`
shorthand: `.err(.network_error("..."))`.

## 8.2 Consuming a `Result` with `match`

Exhaustive `match` (see [07-patterns-and-matching.md](07-patterns-and-matching.md))
is the explicit way to handle a result — both cases in one place:

```
return result.match {
  .ok(cats) => Response.ok(render_all(cats)),
  .err(e)   => e.match {
    .network_error(msg) => Response.internal_server_error(msg),
    .parse_error        => Response.bad_request("Invalid payload format"),
  },
}
```

## 8.3 The `Try` trait and the `.?` / `.!` operators

Short-circuiting is not hard-wired to `Result`/`Option`; it is a trait, so any
user type can participate (like Rust's `Try`):

```
Try :: trait {
  Output   :: type          // the value produced on success
  Residual :: type          // the "failure" carried out on short-circuit

  // Split self into "keep going with Output" or "stop with Residual".
  branch :: func (self: Self) -> ControlFlow.<Self.Residual, Self.Output>

  // Rebuild a Self from a residual (used when propagating across types).
  from_residual :: func (r: Self.Residual) -> Self

  // Panic-unwrap: return Output or abort.
  unwrap :: func (self: Self) -> Self.Output
}
```

`Result` and `Option` implement `Try` in the prelude. Two postfix operators
consume a `Try` value:

| Form | Behavior | On failure |
|------|----------|-----------|
| `x.?` | yields `Output` | **returns** from the enclosing function, converting the residual (via `from_residual`) to the function's return type |
| `x.!` | yields `Output` | **aborts** — literally `Try.unwrap(x)` |

`.?` is the propagation operator (Rust's `?`, spelled as a postfix `.?` here);
there is **no** bare `expr?`. `.!` is the panicking unwrap.

```
load_config :: func () -> Result.<Config, ConfigError> {
  const text   := read_file("config").?   // returns .err(..) on failure, residual converted
  const parsed := parse(text).?
  return .ok(parsed)
}

const port := to_port(8080).!             // abort if invalid; use when failure is a bug
const first := maybe_first.!              // Option: abort on .none
```

`x.?` is only valid inside a function whose return type is itself a `Try` whose
`Residual` the current residual converts into (identity, or a defined
conversion). Because `.?` may return early, it triggers `defer` unwinding
(§8.4).

## 8.4 `defer`

`defer` schedules a statement or block to run when the **current function scope**
exits — via `return`, via a `.?` short-circuit, or by falling off the end.
Deferred actions run **LIFO**.

```
open_and_use :: func (path: string) -> Result.<void, IoError> {
  const file := open(path).?
  defer file.close()                 // runs however we leave this function

  const data := file.read_all().?    // early return here still runs close()
  process(data)
  return .ok(())
}
```

Semantics:

- A `defer` registers its action when control **reaches** it, capturing the
  current values it references. A `defer` never reached does not run.
- Multiple `defer`s unwind in reverse order of registration.
- Deferred actions run on normal returns and on `.?` early returns alike. They
  replace `try/finally`.
- The GC reclaims memory; `defer` releases what the GC does not manage (files,
  sockets, locks).

## 8.5 Panics vs. errors

| Channel | Trigger | Recoverable? | Use for |
|---------|---------|--------------|---------|
| `Result` + `.?` | explicit `.err`, propagated by `.?` | yes, by the caller | expected, handleable failures |
| abort / trap | `$panic`, `.!` on failure, failed `assert`, out-of-bounds index, illegal `$cast`, uninitialized read | no (aborts) | programming errors, invariant violations |

Use `Result` for anything a caller could respond to. Reserve aborts for bugs. A
**compile-time** assertion is `$assert(...)` (see
[06-expressions-and-operators.md](06-expressions-and-operators.md) §6.10); a
**run-time** assertion is the std function `assert(cond, msg)`, which aborts on
failure and may be compiled out in release builds.
