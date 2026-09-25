---
title: nestc and twig
description: Compiler flags, twig subcommands, and the nest.toml manifest.
---

Two tools: `nestc`, the compiler, which takes a single entry file; and
`twig`, the package/build tool, which drives `nestc` from a `nest.toml`
manifest. See [Getting started](../getting-started/) for the install and the
quick tour — this page is the flag/format reference.

## `nestc`

```
usage: nestc [options] <file.nest>
```

| Flag | Meaning |
|---|---|
| `-o <path>` | where to write the output; with several codegen units, the base each unit's name is appended to |
| `--target <triple>` | the machine to generate code for (default: the host) |
| `--emit <list>` | comma-separated: `link` (default, a linked executable), `ast`/`ir`/`mono`/`lir` (compiler dumps, to stdout), `obj`/`asm`/`backend-ir` (backend output, to files), `nlib` (the package as a library — metadata, IR, objects). `kind=path` sends one to a file. |
| `-L <dir>` | a directory to search for packages; repeatable, in order |
| `-l <name>` | a C library to link against, as the linker names it; repeatable |
| `--link-search <dir>` | a directory to look for those libraries in |
| `--color <when>` | `auto` (default), `always`, `never` |
| `--error-format <form>` | `human` (default) or `json` — one JSON object per line, on stderr |
| `--package <name>=<path>` | a package pinned to a root file, beating any `-L` search |
| `--extern <name>=<path>` | a compiled `.nlib` this compilation depends on and may import; repeatable |
| `--indirect <name>=<path>` | a library a dependency was compiled against: read and linked, but not importable |
| `--obj-dir <dir>` | keep the objects a link/library is made from here, instead of a removed temp dir |
| `--test` | build a test binary: keep the entry package's `@test` functions and run them instead of `main` |
| `--up-to-date` | compile nothing; exit 0 if the library at `-o` is already up to date, 1 otherwise |
| `-C <key>=<value>` | a build setting (below) |
| `-h`, `--help` | this |

Build settings (`-C`):

| Setting | Meaning |
|---|---|
| `backend=<name>` | which code generator (default: the first compiled in) |
| `codegen-units=N` | how many codegen units the program is split into (default: 1) |
| `entry=auto\|none` | synthesize a C `main` calling the program's `main` (default: `auto`) |
| `linker=<path>` | the linker driver for `--emit link` (default: `cc`) |
| `partial-linker=<path>` | merges several codegen units into one object (default: `ld -r`) |
| `link-arg=<arg>` | one more linker argument; repeatable, in order |
| `runtime=<path>` | the runtime archive to link, overriding the one built beside this compiler |
| `overflow=trap\|wrap` | what a run-time integer overflow does (default: `trap`) |
| `opt-level=0\|1\|2\|3\|s\|z` | how hard the backend optimizes (default: `0`) |
| `target-cpu=<name>` | the assumed processor: `generic` (default), `native`, or a backend name |
| `pointer-width=16\|32\|64` | override the target's pointer width |
| `os=<name>` / `arch=<name>` | override the target OS/architecture |
| `profile=debug\|release` | the build profile's name, readable from source |
| `print=options` | print the resolved settings and exit |
| `print=packages` | print the packages shipped with this compiler, `name=root` per line, and exit |

## `twig`

```
usage: twig <command> [options]
```

| Command | Meaning |
|---|---|
| `new <path>` | create a package in a new directory |
| `init` | create a package in the current directory |
| `build` | compile the package's targets |
| `run [-- <args>]` | build a binary, then run it with `<args>` |
| `test [filter]` | build the package's `@test` functions and run them — the ones beside its code, then each file in `tests/` against its library; with a filter, only the tests whose names contain it |
| `metadata` | how each package is compiled, as JSON, for tools |

Options:

| Flag | Meaning |
|---|---|
| `--release` | build with the release profile (default: debug) |
| `--bin <name>` | build or run only this binary |
| `--deps` | build: only the libraries the package depends on |
| `-v`, `--verbose` | print each compiler command before running it |
| `--emit <list>` | build, run: also write the compiler's dumps for the package's own targets beside their objects, in `build/<profile>/obj`: `ast`, `ir`, `mono`, `lir`, `llvm-ir`, `asm` |
| `--build-dir <dir>` | build, run, test, metadata: where the build writes, instead of the package's own `build/` |
| `--nestc-arg <arg>` | build, run, test: one more argument for `nestc`, last on the command line, on the package's own targets only; repeatable. `[build]` in `nest.toml` is the same thing written down |
| `--lib` | `new`, `init`: a library rather than a binary |
| `-h`, `--help` | this |

## `nest.toml`

Targets are explicit — nothing is inferred from which files exist. A
package with no `[lib]` can't be depended on; one with no `[[bin]]` has
nothing to run. Every path is relative to the manifest's directory.

```toml
[package]
name = "hello"
version = "0.1.0"

[lib]
path = "src/package.nest"

[[bin]]
name = "hello"
path = "src/main.nest"

[dependencies]
util = { path = "../util" }

[profile.release]
overflow = "trap"
codegen-units = 4
opt-level = "s"

[build]
link-libs = ["sfml-graphics"]
link-search = ["/opt/homebrew/lib"]
```

A dependency is a `path` for now — the table is a table rather than a
string so `version` and `git` have somewhere to go once there's a
registry.
