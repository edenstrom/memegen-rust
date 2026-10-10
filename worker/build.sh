#!/bin/sh
# Build command for wrangler. Cloudflare's Workers Builds image has no Rust
# toolchain, so install one with rustup when cargo isn't on the PATH.
set -eu

if ! command -v cargo >/dev/null 2>&1; then
  if [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y --profile minimal --no-modify-path
  fi
  . "$HOME/.cargo/env"
fi

if command -v rustup >/dev/null 2>&1; then
  rustup target add wasm32-unknown-unknown
fi

cargo install -q worker-build@^0.8.7
worker-build --release
