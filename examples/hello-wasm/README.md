# hello-wasm

Minimal warpline guest module. Returns the bytes `hello from wasm`.

## Build

```bash
# Add the target once.
rustup target add wasm32-wasip2

# From inside this directory.
cargo build --target wasm32-wasip2 --release
```

Output: `target/wasm32-wasip2/release/hello_wasm.wasm` — a Component-Model
binary; `wasm32-wasip2` emits components directly, no adapter step needed.

## Upload to warpline-control

```bash
curl -X POST \
  -F "wasm=@target/wasm32-wasip2/release/hello_wasm.wasm" \
  http://localhost:8081/tenants/demo/functions/hello
```

## Invoke via warpline-host

```bash
curl -X POST \
  --data-binary @/dev/null \
  http://localhost:8080/tenants/demo/functions/hello/invoke
```

Expected response body: `hello from wasm`.

## Why not in the workspace?

This crate targets `wasm32-wasip2`. The warpline workspace targets the host
platform (linux x86_64 / arm64). Mixing them in one workspace forces every
`cargo check` / `cargo build` to think about a target it doesn't want to
build. The workspace root's `Cargo.toml` explicitly excludes this directory
(and `examples/test-guest`) via `exclude`.
