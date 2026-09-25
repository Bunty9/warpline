//! Minimal warpline guest — the simplest possible `handle` export.
//!
//! Built with:
//!
//! ```bash
//! rustup target add wasm32-wasip2
//! cargo build --target wasm32-wasip2 --release
//! ```
//!
//! (or `scripts/build-guests.sh` from the repo root, which builds this and
//! `examples/test-guest` and copies the latter's output into
//! `crates/core/tests/fixtures/`). The output goes to
//! `target/wasm32-wasip2/release/hello_wasm.wasm` — a Component-Model
//! binary, ready to upload to `warpline-control` as-is (no adapter step,
//! unlike `wasm32-wasip1`).

wit_bindgen::generate!({
    world: "handler",
    path: "../../wit",
});

struct HelloGuest;

impl Guest for HelloGuest {
    fn handle(_input: Vec<u8>) -> Vec<u8> {
        b"hello from wasm".to_vec()
    }
}

export!(HelloGuest);
