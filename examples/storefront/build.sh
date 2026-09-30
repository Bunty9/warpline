#!/usr/bin/env bash
# Build the checkout hook with the pinned guest toolchain and refresh the
# committed fixture the e2e test uploads (so `cargo test` in app/ does not
# need the wasm32-wasip2 target).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# examples/rust-toolchain.toml (parent dir) pins the toolchain and target.
(cd "$here/hook" && rustup target add wasm32-wasip2 >/dev/null 2>&1 || true)
# --locked: the committed Cargo.lock is part of what makes the hash stable.
(cd "$here/hook" && cargo build --release --locked --target wasm32-wasip2)

out="$here/app/tests/fixtures/checkout_hook.wasm"
cp "$here/hook/target/wasm32-wasip2/release/checkout_hook.wasm" "$out"
echo "updated $out"
sha256sum "$out"
