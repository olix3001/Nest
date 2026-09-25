---
title: Errors
description: Result, the Try trait, .? and .!, defer, and panics.
---

Errors are **values**, not exceptions — there's no throw/catch and no stack
unwinding for ordinary failures. A fallible function returns a `Result`,
and callers engage with both cases. This is the Rust model, adapted to
Nest's syntax.

## `Result.<T, E>`

```nest
Result :: #lang("result") enum <T, E> {
    ok(T),
    err(E),
}
```

```nest
get :: func <T> (self: *Client, url: str) -> Result.<T, FetchError> {
    return .err(.network_error("Failed to reach endpoint"))
}

FetchError :: enum {
    network_error(str),
    parse_error,
}
```

The error type is usually an enum enumerating failure modes, so callers can
`match` on precisely what went wrong. Construction is context-inferred, so
both the wrapper and the payload use the `.` shorthand:
`.err(.network_error("..."))`.

## Consuming with `match`

```nest
return result.match {
    .ok(cats) => Response.ok(render_all(cats)),
    .err(e)   => e.match {
        .network_error(msg) => Response.internal_server_error(msg),
        .parse_error        => Response.bad_request("Invalid payload format"),
    },
}
```

## The `Try` trait and `.?` / `.!`

Short-circuiting isn't hard-wired to `Result`/`Option` — it's a trait, so
any user type can participate (Rust's `Try`):

```nest
Try :: #lang("try") trait {
    Output   :: type   // the value produced on success
    Residual :: type   // what's carried out on short-circuit

    branch :: func (self: Self) -> ControlFlow.<Self.Residual, Self.Output>
    unwrap :: func (self: Self) -> Self.Output
}
```

Two postfix operators consume a `Try` value:

| Form | Behavior | On failure |
|------|----------|-----------|
| `x.?` | yields `Output` | **returns** from the enclosing function, rebuilding its type from the residual via `FromResidual` |
| `x.!` | yields `Output` | **aborts** — panics |

```nest
load_config :: func () -> Result.<Config, ConfigError> {
    const text   := read_file("config").?   // returns .err(..) on failure
    const parsed := parse(text).?
    return .ok(parsed)
}

const port := to_port(8080).!    // abort if invalid; use when failure is a bug
```

`.?` is the propagation operator (Rust's `?`, spelled postfix); there's no
bare `expr?`. `.!` is the panicking unwrap. `x.?` is only valid inside a
function whose return type implements `FromResidual.<R>` for the residual
`R` the operand short-circuits with — identity, or a declared conversion —
otherwise it's an error naming the residual with nowhere to go. Because
`.?` may return early, it triggers `defer` unwinding.

`Option` short-circuits with nothing to carry (its `Residual` is `void`),
which is also what keeps a `Result`'s error from silently vanishing into an
`Option`-returning function — the two residuals disagree, and no impl says
how they'd convert.

A conversion between error types is an ordinary `FromResidual` impl:

```nest
impl <T> FromResidual.<IoError> for Result.<T, ConfigError> {
    from_residual :: func (r: IoError) -> Result.<T, ConfigError> {
        return .err(ConfigError.io(r))
    }
}
```

## `defer`

`defer` schedules a statement or block to run when the **current function
scope** exits — via `return`, via a `.?` short-circuit, or by falling off
the end. Deferred actions run **LIFO**:

```nest
open_and_use :: func (path: str) -> Result.<void, IoError> {
    const file := open(path).?
    defer file.close()                 // runs however we leave this function

    const data := file.read_all().?    // early return here still runs close()
    process(data)
    return .ok(())
}
```

- A `defer` registers its action when control **reaches** it, capturing the
  values it references then. One never reached does not run.
- Multiple `defer`s unwind in reverse order of registration.
- They run on normal returns and `.?` early returns alike — this replaces
  `try`/`finally`.
- The GC reclaims memory; `defer` releases what the GC doesn't manage
  (files, sockets, locks).

For types whose cleanup should be automatic rather than hand-written at
each use site, the `Drop` trait (`#lang("drop")`) is what the compiler
calls on scope exit, at the same LIFO points `defer` runs at. `defer` is
the explicit, per-site form; `Drop` is the per-type form.

## Panics vs. errors

| Channel | Trigger | Recoverable? | Use for |
|---------|---------|--------------|---------|
| `Result` + `.?` | explicit `.err`, propagated | yes, by the caller | expected, handleable failures |
| abort / trap | `panic`, `.!` on failure, failed `assert`, out-of-bounds index, illegal `cast`, uninitialized read | no | programming errors, invariant violations |

A **compile-time** assertion is `comptime_assert(cond)` — stops the build,
emits nothing. A **run-time** one is `assert(cond, msg)`, an ordinary
`core` function that panics on failure and reports the calling line.

## The panic handler

`panic` is an ordinary function in `core`, not a compiler intrinsic:

```nest
@public panic :: #lang("panic") func (msg: str, loc: Location := #caller_location) -> never {
    panic_handler(msg, loc)
}
```

Every abort — including ones the compiler itself raises, like a trapped
integer overflow — goes through it, with the `Location` filled in from the
failing operation's own position. The handler is replaceable: `core` has no
I/O, so its default just stops. A program claims `#lang("panic_handler")`
itself to replace it, and that claim wins over `core`'s:

```nest
my_handler :: #lang("panic_handler") func (msg: str, loc: Location) -> never {
    write_line(msg)
    os.abort()
}
```
