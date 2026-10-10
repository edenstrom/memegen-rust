#!/bin/sh
# Build command for wrangler. Cloudflare's Workers Builds image has no Rust
# toolchain, so install one with rustup when cargo isn't on the PATH.
set -eu

# Workers Builds' build cache only keeps package manager directories such as
# ~/.npm (package.json makes it detect npm), so keep the toolchain, the
# registry, worker-build (and the wasm-bindgen, wasm-opt and esbuild binaries
# it downloads into XDG_CACHE_HOME) and the target directory there.
if [ "${WORKERS_CI:-}" = 1 ]; then
  cache="$HOME/.npm/_rust"
  export RUSTUP_HOME="$cache/rustup" CARGO_HOME="$cache/cargo"
  export CARGO_TARGET_DIR="$cache/target" XDG_CACHE_HOME="$cache/xdg"
  export PATH="$CARGO_HOME/bin:$PATH"
  if [ -d "$CARGO_TARGET_DIR" ]; then
    echo "Reusing cached Rust build ($(du -sh "$cache" | cut -f1))"
    # The target directory only grows; start over before it nears the
    # cache's 10 GB limit.
    if [ "$(du -sm "$CARGO_TARGET_DIR" | cut -f1)" -gt 4000 ]; then
      rm -rf "$CARGO_TARGET_DIR"
    fi
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  if [ ! -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y --profile minimal --no-modify-path
  fi
  export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
fi

if command -v rustup >/dev/null 2>&1; then
  rustup target add wasm32-unknown-unknown
fi

# Skips the build when the installed worker-build already matches.
cargo install -q worker-build@^0.8.7
worker-build --release
