#!/usr/bin/env bash
# Build the wasm32-wasip2 guest examples and refresh the committed test
# fixture `crates/core/tests/fixtures/test_guest.wasm`.
#
# `crates/core`'s test suite reads that fixture with `include_bytes!` so
# `cargo test -p warpline-core` never needs the wasm32-wasip2 target
# installed — only re-run this script when `examples/test-guest` changes.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! rustup target list --installed | grep -q '^wasm32-wasip2$'; then
    rustup target add wasm32-wasip2
fi

for guest in hello-wasm test-guest; do
    echo "building examples/${guest}..."
    (cd "$repo_root/examples/$guest" && cargo build --release --target wasm32-wasip2)
done

fixtures_dir="$repo_root/crates/core/tests/fixtures"
mkdir -p "$fixtures_dir"
cp \
    "$repo_root/examples/test-guest/target/wasm32-wasip2/release/test_guest.wasm" \
    "$fixtures_dir/test_guest.wasm"

echo "updated $fixtures_dir/test_guest.wasm"
