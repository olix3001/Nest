---
title: Testing
description: "@test, the tests namespace convention, and twig test."
---

`@test` marks a function as a test. It takes no arguments:

```nest
add :: func (a: i32, b: i32) -> i32 { return a + b }

tests :: namespace {
    @test
    adds :: func () {
        assert(add(2, 3) == 5)
    }

    @test
    reads :: func () -> Result.<void, str> {
        return .ok(())
    }
}
```

A test takes **no parameters**, is **not generic**, and returns either
`void` or `Result.<void, E>` for any `E`. It fails by failing — a trap, a
failed `assert`, a panic — or, in the second shape, by returning `.err`. A
returned `.err` is reported with the error itself, through `core`'s
`Debug`, naming the `@test` function's own line rather than the line inside
`core` that raised the panic.

**A program may not name a `@test` function** — not call it, not take its
address. A test is run by `nestc --test` and by `twig test`, each of which
guards the call so a failing test is reported and the next one still runs.

## Keeping tests out of release builds

Tests are compiled like any other function — `@test` says what a function
is *for*, not whether it's built. Keeping them out of a release binary is
[`#when(test)`](../directives/#conditional-compilation---when)'s
job, applied to the namespace they live in. The recommended arrangement is
a `tests` namespace beside the code it tests, which sees the file's private
names and is the unit `#when` applies to:

```nest
tests :: #when(test) namespace {
    @test
    adds :: func () { assert(add(2, 3) == 5) }
}
```

## Running tests

```sh
twig test            # every test
twig test parse      # only the tests whose names contain `parse`
```

builds the package's `@test` functions and runs them. Directly through the
compiler:

```sh
nestc --test entry.nest -o test_binary
./test_binary
NEST_TEST_FILTER=parse ./test_binary
```

`nestc --test` builds a test binary: it keeps the entry package's `@test`
functions and runs them instead of its `main`. The filter reaches a test
binary through the `NEST_TEST_FILTER` environment variable, which `twig test
<filter>` sets; a test runs when its name (`path.tests.joining`) contains it,
and the summary counts the rest as filtered out.

## Integration tests

A package's `tests/` directory holds **integration tests**: each `.nest` file
directly in it is a program of its own, built with `--test` against the
package's library — so it sees only what the library makes `@public`, which
is the point of writing a test there rather than beside the code.

```text
mylib/
  nest.toml
  src/package.nest
  tests/api.nest      # one test binary
```

```nest
// tests/api.nest
{ double } :: import <mylib>

main :: func () {}

tests :: #when(test) namespace {
    @test
    doubles_through_the_api :: func () { assert(double(21) == 42) }
}
```

`twig test` runs the package's own tests first, then each integration test in
name order, and fails if any of them did. A subdirectory of `tests/` is not a
test itself — it is where several of them keep code they share.
