# Everything in this repository, built and tested.
#
#   nestc      the compiler (Rust, with the LLVM backend)
#   nest-lsp   the language server (Rust)
#   twig       the build tool (Nest), bootstrapped by nestc, then built by itself
#
# Needs: `just` (brew install just), rustup (the toolchain in
# nestc/rust-toolchain.toml installs on first use), a C compiler (`cc`), the
# Boehm collector (bdw-gc), and LLVM 21.
#
#   just bootstrap     a fresh machine: everything, twig included
#   just build         everything, incrementally, once bootstrapped
#   just test          every test suite there is
#
# `profile` is release by default; `just profile=debug build` for the other.

profile := "release"

# The cargo and twig flags each profile wants. A debug build is the default for
# both tools, so only release has a flag to pass.
cargo_profile := if profile == "release" { "--release" } else { "" }
twig_profile := if profile == "release" { "--release" } else { "" }

root := replace(justfile_directory(), '\', '/') # Weird windows stuff...
bin := root / "nestc/target" / profile
nestc := bin / "nestc"
twig := root / "twig/build" / profile / "twig"

export NESTC := nestc

# The collector and LLVM are found the way nestc/build.rs finds them, so that a
# build through `cargo` directly and a build through here agree. Both are
# resolved once, when this file is read, and exported to every recipe.
export BDW_GC_PREFIX := env("BDW_GC_PREFIX", ```
    if command -v brew >/dev/null 2>&1 && brew --prefix bdw-gc >/dev/null 2>&1; then
      brew --prefix bdw-gc
    fi
```)

export LLVM_SYS_211_PREFIX := env("LLVM_SYS_211_PREFIX", ```
    if command -v brew >/dev/null 2>&1 && brew --prefix llvm@21 >/dev/null 2>&1; then
      brew --prefix llvm@21
    else
      for config in llvm-config-21 llvm-config; do
        if command -v "$config" >/dev/null 2>&1 && case "$("$config" --version)" in 21.*) true;; *) false;; esac; then
          "$config" --prefix
          break
        fi
      done
    fi
```)

# What you can run.
default:
    @just --list --unsorted

# ===< Building >===

# A fresh machine: the compiler, the server, and twig from nothing.
#
# The difference from `build` is twig. twig is written in Nest, so the first one
# has to be compiled by nestc directly, from `core` and `std` sources; that twig
# then builds twig the way every other package is built. Once one exists,
# `build` uses it and this is not needed again.
[doc("A fresh machine: the compiler, the server, and twig from nothing.")]
bootstrap: tools build-nestc build-lsp
    @just _step "Bootstrapping twig"
    mkdir -p {{ root }}/twig/build/bootstrap
    {{ nestc }} {{ root }}/twig/src/main.nest -o {{ root }}/twig/build/bootstrap/twig
    @just _step "Building twig with itself ({{ profile }})"
    cd {{ root }}/twig && {{ root }}/twig/build/bootstrap/twig build {{ twig_profile }}
    @{{ twig }} --help >/dev/null || (echo "error: the built twig does not run" >&2; exit 1)
    @just _where

# Everything, incrementally. Falls back to a bootstrap if twig is not built yet.
build: tools build-nestc build-lsp build-twig
    @just _where

# The compiler, with the LLVM backend.
build-nestc: tools
    @just _step "Building nestc ({{ profile }}, with LLVM)"
    cd {{ root }}/nestc && cargo build {{ cargo_profile }} --features llvm -p nestc

# The language server. Built from the same source as nestc, which is not
# optional: the two share the front end, and a server built from another commit
# answers about a language the compiler is not compiling.
[doc("The language server, from the same source as nestc.")]
build-lsp: tools
    @just _step "Building nest-lsp ({{ profile }})"
    cd {{ root }}/nestc && cargo build {{ cargo_profile }} -p nest-lsp

# The build tool, by itself. Bootstraps first if there is no twig yet — which is
# why it needs nestc: the bootstrap twig is compiled by it.
[doc("The build tool. Bootstraps first if there is no twig yet.")]
build-twig: build-nestc
    @just _step "Building twig ({{ profile }})"
    @if [ -x "{{ twig }}" ]; then \
        cd {{ root }}/twig && {{ twig }} build {{ twig_profile }}; \
    elif [ -x "{{ root }}/twig/build/bootstrap/twig" ]; then \
        cd {{ root }}/twig && {{ root }}/twig/build/bootstrap/twig build {{ twig_profile }}; \
    else \
        echo "no twig yet — bootstrapping"; \
        mkdir -p {{ root }}/twig/build/bootstrap; \
        {{ nestc }} {{ root }}/twig/src/main.nest -o {{ root }}/twig/build/bootstrap/twig; \
        cd {{ root }}/twig && {{ root }}/twig/build/bootstrap/twig build {{ twig_profile }}; \
    fi

# ===< Testing >===

# Every suite: the compiler, the server, the grammar, `std` and twig.
#
# One recipe rather than a list a reader has to assemble, because "did I break
# anything" is one question. Each part is also its own recipe, for the times it
# is not.
[doc("Every suite: the compiler, the server, the grammar, `std` and twig.")]
test: test-nestc test-lsp test-grammar test-std test-twig
    @just _step "All suites passed"

# The compiler: inference, lowering, codegen, and the programs in examples/.
test-nestc:
    @just _step "Testing nestc"
    cd {{ root }}/nestc && cargo test {{ cargo_profile }} -p nestc

# The language server, including the editor scenarios that are easy to regress.
test-lsp:
    @just _step "Testing nest-lsp"
    cd {{ root }}/nestc && cargo test {{ cargo_profile }} -p nest-lsp

# The tree-sitter grammar, against its corpus. Skipped where it is not set up,
# since it needs npx and the generated parser.
[doc("The tree-sitter grammar, against its corpus.")]
test-grammar:
    @just _step "Testing the grammar"
    @if command -v npx >/dev/null 2>&1; then \
        cd {{ root }}/editors/tree-sitter-nest && npx tree-sitter test; \
    else \
        echo "npx not found — skipping the grammar corpus"; \
    fi

# `std`'s own tests, run the way any package's are. `core` has none yet: its
# test runner is the thing under test, so a failing one there has nowhere to be
# reported from.
#
# `--build-dir` keeps the build out of `packages/std`: `std` ships with the
# compiler, and a build has no business writing into a package it only reads.
[doc("`std`'s own tests, through `twig test`.")]
test-std:
    @just _step "Testing std"
    @if [ -x "{{ twig }}" ]; then \
        cd {{ root }}/packages/std && {{ twig }} test --build-dir {{ root }}/twig/build/tests/std; \
    else \
        echo "twig is not built — skipping std's tests"; \
    fi

# twig's own tests: `@test` functions in twig's sources, run by `twig test`.
#
# The probe reads the command list rather than trying `twig test --help`: twig
# answers `--help` before it dispatches, so that exits 0 for a command it does
# not have. It stays because this recipe runs before twig is necessarily built.
[doc("twig's own tests, through `twig test`.")]
test-twig:
    @just _step "Testing twig"
    @if [ -x "{{ twig }}" ] && {{ twig }} --help 2>/dev/null | grep -q '^  test\b'; then \
        cd {{ root }}/twig && {{ twig }} test; \
    else \
        echo "twig is not built — skipping its tests"; \
    fi

# ===< Housekeeping >===

# Check the tools a build needs, and say which is missing rather than failing
# somewhere inside cargo.
[doc("Check the tools a build needs, and say which is missing.")]
tools:
    @command -v cargo >/dev/null || (echo "error: cargo not found; install Rust with rustup (https://rustup.rs)" >&2; exit 1)
    @command -v cc >/dev/null || (echo "error: cc not found; install a C compiler (Xcode command line tools, or gcc/clang)" >&2; exit 1)
    @test -n "{{ BDW_GC_PREFIX }}" -o -f /usr/include/gc.h -o -f /usr/local/include/gc.h \
        || (echo "error: the Boehm collector was not found; install it (\`brew install bdw-gc\`, or your distribution's libgc-dev) or set BDW_GC_PREFIX" >&2; exit 1)
    @test -n "{{ LLVM_SYS_211_PREFIX }}" \
        || (echo "error: LLVM 21 not found; install it (\`brew install llvm@21\`, or your distribution's llvm-21) or set LLVM_SYS_211_PREFIX" >&2; exit 1)
    @printf 'LLVM: %s\n' "{{ LLVM_SYS_211_PREFIX }}"

# Formatting. `cargo fmt` over the Rust; there is no formatter for Nest yet.
fmt:
    cd {{ root }}/nestc && cargo fmt

[doc("Check the Rust is formatted, without rewriting it.")]
fmt-check:
    cd {{ root }}/nestc && cargo fmt --check

# Everything built, in both profiles. A serialized-format change needs this —
# see design/library.md.
#
# Two directories, because a build writes to two: `cargo`'s and twig's. Nothing
# under `packages/` is built into any more — `test-std` passes `--build-dir` —
# but a `build/` left there by an older toolchain is still removed.
[doc("Everything built, in both profiles.")]
clean:
    cd {{ root }}/nestc && cargo clean -p nestc && cargo clean -p nest-lsp
    rm -rf {{ root }}/twig/build
    find {{ root }}/packages -type d -name build -prune -exec rm -rf {} +

# ===< Private helpers >===

[private]
_step message:
    @printf '\n\033[1;32m==>\033[0m %s\n' "{{ message }}"

[private]
_where:
    @just _step "Done"
    @printf '  nestc     %s\n  nest-lsp  %s\n  twig      %s\n\n' \
        "{{ nestc }}" "{{ bin }}/nest-lsp" "{{ twig }}"
    @printf 'Put them on PATH, for example in your shell profile:\n\n  export PATH="%s:%s:$PATH"\n\n' \
        "{{ bin }}" "{{ root }}/twig/build/{{ profile }}"
    @printf 'For Zed, install the dev extension from editors/zed (see editors/README.md).\n'
