---
title: Building from source
description: Build nestc, nest-lsp and twig from the repository.
---

Building from the repository is how `nestc`, `nest-lsp` and `twig` are
installed today.

## What you need

- [`just`](https://github.com/casey/just) (`brew install just`, or your
  distribution's package), which runs the build.
- Rust through [rustup](https://rustup.rs). The repository pins a nightly
  toolchain (`nestc/rust-toolchain.toml`), and rustup installs it on the
  first build.
- A C compiler, `cc`. It compiles the runtime, and `nestc` uses it as the
  linker for every program.
- The Boehm collector: `brew install bdw-gc`, or `libgc-dev` on
  Debian/Ubuntu.
- LLVM 21: `brew install llvm@21`, or `llvm-21-dev` (with `libpolly-21-dev`
  and `libzstd-dev`) from [apt.llvm.org](https://apt.llvm.org) on
  Debian/Ubuntu.

Homebrew installs are found on their own. Elsewhere, point the build at them
with `BDW_GC_PREFIX` (the directory holding `include/gc.h`) and
`LLVM_SYS_211_PREFIX` (LLVM's prefix, e.g. `/usr/lib/llvm-21`). `just tools`
checks everything is found.

## Building

```sh
git clone https://github.com/olix3001/Nest
cd Nest
just bootstrap
```

`just bootstrap` builds the compiler and the language server with `cargo`,
then twig. twig is written in Nest, so the first twig is compiled by `nestc`
directly and then rebuilds itself. After that, `just build` rebuilds whatever
changed. Both use the release profile; `just profile=debug build` builds the
debug one.

The build ends by printing where the tools are and a line to put them on your
`PATH`:

```sh
export PATH="$PWD/nestc/target/release:$PWD/twig/build/release:$PATH"
```

`just test` runs every test suite, and `just` alone lists the other recipes.

## Keep the checkout where you built it

A build **records paths into its checkout** when it is compiled:

- where `core` and `std` are (`packages/core`, `packages/std`), which
  `nestc` reads from source whenever it compiles a program;
- the runtime archive, and the collector library, which it links into every
  program.

So a built `nestc` works only while the checkout stays where it was built.
After moving it, run `just build` again. To use another `core` or `std`
without rebuilding, set `NEST_CORE` or `NEST_STD` to its `package.nest`.

## Editors

The Zed extension lives in `editors/zed`, with the tree-sitter grammar beside
it in `editors/tree-sitter-nest`. `editors/README.md` says how to install it as
a dev extension. It finds `nest-lsp`, `twig` and `nestc` on your `PATH`, or
at paths set in Zed's settings.
