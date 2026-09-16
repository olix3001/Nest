#!/usr/bin/env bash
# Builds everything in this repository on a fresh machine:
#
#   nestc      the compiler (Rust, with the LLVM backend)
#   nest-lsp   the language server (Rust)
#   twig       the build tool (Nest), bootstrapped with nestc, then rebuilt by itself
#
# Needs: rustup (the toolchain in nestc/rust-toolchain.toml is installed on
# first use), a C compiler (`cc`), and LLVM 21. LLVM is found through
# LLVM_SYS_211_PREFIX, then `brew --prefix llvm@21`, then `llvm-config-21` or
# `llvm-config` reporting version 21.
#
# usage: ./build.sh [--debug]    (default: release)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE=release
for arg in "$@"; do
  case "$arg" in
    --debug) PROFILE=debug ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "build.sh: unknown argument \`$arg\`" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1;32m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

step "Checking tools"
command -v cargo >/dev/null || fail "cargo not found; install Rust with rustup (https://rustup.rs)"
command -v cc >/dev/null || fail "cc not found; install a C compiler (Xcode command line tools, or gcc/clang)"

if [[ -z "${LLVM_SYS_211_PREFIX:-}" ]]; then
  if command -v brew >/dev/null && brew --prefix llvm@21 >/dev/null 2>&1 && [[ -d "$(brew --prefix llvm@21)" ]]; then
    LLVM_SYS_211_PREFIX="$(brew --prefix llvm@21)"
  else
    for config in llvm-config-21 llvm-config; do
      if command -v "$config" >/dev/null && [[ "$("$config" --version)" == 21.* ]]; then
        LLVM_SYS_211_PREFIX="$("$config" --prefix)"
        break
      fi
    done
  fi
fi
[[ -n "${LLVM_SYS_211_PREFIX:-}" ]] || fail "LLVM 21 not found; install it (\`brew install llvm@21\`, or your distribution's llvm-21) or set LLVM_SYS_211_PREFIX"
export LLVM_SYS_211_PREFIX
echo "LLVM: $LLVM_SYS_211_PREFIX"

CARGO_FLAGS=()
[[ "$PROFILE" == release ]] && CARGO_FLAGS+=(--release)

step "Building nestc ($PROFILE, with LLVM)"
(cd "$ROOT/nestc" && cargo build "${CARGO_FLAGS[@]}" --features llvm -p nestc)

step "Building nest-lsp ($PROFILE)"
(cd "$ROOT/nestc" && cargo build "${CARGO_FLAGS[@]}" -p nest-lsp)

NESTC="$ROOT/nestc/target/$PROFILE/nestc"
NEST_LSP="$ROOT/nestc/target/$PROFILE/nest-lsp"
export NESTC

# twig is written in Nest, so the first one is compiled by nestc directly, with
# `core` and `std` from source; that twig then builds twig the usual way.
step "Bootstrapping twig"
BOOT="$ROOT/twig/build/bootstrap"
mkdir -p "$BOOT"
"$NESTC" "$ROOT/twig/src/main.nest" -o "$BOOT/twig"

step "Building twig with itself ($PROFILE)"
TWIG_FLAGS=()
[[ "$PROFILE" == release ]] && TWIG_FLAGS+=(--release)
(cd "$ROOT/twig" && "$BOOT/twig" build "${TWIG_FLAGS[@]}")
TWIG="$ROOT/twig/build/$PROFILE/twig"
"$TWIG" --help >/dev/null || fail "the built twig does not run"

step "Done"
cat <<EOF
  nestc     $NESTC
  nest-lsp  $NEST_LSP
  twig      $TWIG

Put them on PATH, for example in your shell profile:

  export PATH="$ROOT/nestc/target/$PROFILE:$ROOT/twig/build/$PROFILE:\$PATH"

For Zed, install the dev extension from editors/zed (see editors/README.md).
EOF
