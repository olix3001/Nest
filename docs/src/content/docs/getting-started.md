---
title: Installation & first program
description: Get nestc and twig, and compile your first Nest program.
---

Nest is highly experimental — no syntax, IR format, manifest format, or CLI
flag here should be considered stable yet.

## Installing

:::note
For now, `nestc` and `twig` have to be **built from source**. A build records
paths into the checkout it was built from: where `core` and `std` are, and
where the runtime and the collector it links into every program are. So a
built compiler can't simply be copied to another machine. There is no
installer and no package manager entry yet.
:::

Follow [Building from source](../building-from-source/), then put `nestc` and
`twig` on your `PATH`. Officially tested targets are `x86_64 Linux` and
`arm64 macOS`. Other LLVM-supported targets with libc and Boehm GC should work
but aren't CI-checked yet.

Check both are found:

```sh
nestc --help
twig --help
```

## A single file, with `nestc`

`nestc` compiles one file (and whatever it imports) straight to a linked
executable — no project structure required:

```nest title="hello.nest"
io :: import <std/io>

main :: func () {
    io.println("Hello, Nest!")
}
```

```sh
nestc hello.nest -o hello
./hello
```

`nestc` takes a single entry file plus flags (`--target`, `--emit`, `-C
<key>=<value>`, …) — see `nestc --help` for the full list. This is the layer
`twig` builds on; reach for it directly when you don't need a package, a
manifest, or dependencies.

## A package, with `twig`

`twig` is the build tool: it reads a `nest.toml` manifest, resolves
dependencies, and drives `nestc` for you.

```sh
twig new hello
cd hello
twig build
twig run
```

`twig new <path>` scaffolds a binary package in a new directory (`--lib` for a
library instead); `twig init` does the same in the current directory. The
result is a `nest.toml` manifest and a `src/` directory:

```toml title="nest.toml"
[package]
name = "hello"
version = "0.1.0"

[[bin]]
name = "hello"
path = "src/main.nest"

[dependencies]
# util = { path = "../util" }
```

Targets are explicit — nothing is inferred from which files exist. A package
with no `[lib]` table can't be depended on; one with no `[[bin]]` has nothing
to run. A `[lib]` table names a library target the same way:

```toml
[lib]
path = "src/package.nest"
```

- `twig build` compiles the package's targets (add `--release` for the
  release profile; default is debug).
- `twig run [-- <args>]` builds a binary and runs it, forwarding `<args>`.
- `twig test` builds the package's `@test` functions and runs them.
- `twig build --bin <name>` builds (or runs) only that binary, when a package
  declares more than one.

See [Testing](../language/testing/) for `@test`, and
[Directives and attributes](../language/directives/) for `#when(test)`, which is
how test code stays out of a release binary.
