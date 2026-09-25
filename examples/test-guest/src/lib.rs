//! warpline-core integration-test fixture.
//!
//! `handle(input)` dispatches on the raw input bytes to exercise every
//! host capability, cap, and trap path `crates/core/tests/runtime.rs`
//! needs to cover:
//!
//! - `loop`            — spins forever (CPU-budget / epoch-interrupt test).
//! - `alloc`            — leaks 1 MiB `Vec`s forever (memory-cap test).
//! - `kv:put:{k}:{v}`   — `kv::put(k, v)`, returns `ok`.
//! - `kv:get:{k}`       — `kv::get(k)`, returns the value or `none`.
//! - `log:{msg}`        — `log::emit("info", msg)`, returns `ok`.
//! - `fetch:{url}`      — `http-out::fetch` a GET, returns
//!                        `status:{n}:{body}` or `err:{msg}`.
//! - `panic`            — panics (guest-trap test).
//! - anything else      — echoed back verbatim.
//!
//! Built with `scripts/build-guests.sh`, which also copies the release
//! output to `crates/core/tests/fixtures/test_guest.wasm` so `cargo test`
//! doesn't need the `wasm32-wasip2` target installed.

wit_bindgen::generate!({
    world: "handler",
    path: "../../wit",
});

use warpline::host::http_out::{fetch, Request};
use warpline::host::{kv, log};

struct TestGuest;

impl Guest for TestGuest {
    fn handle(input: Vec<u8>) -> Vec<u8> {
        if input == b"loop" {
            loop {
                // `black_box` stops the optimiser from proving this loop
                // does nothing and deleting it.
                std::hint::black_box(());
            }
        }

        if input == b"alloc" {
            loop {
                let chunk: Vec<u8> = vec![0xAA; 1024 * 1024];
                std::mem::forget(std::hint::black_box(chunk));
            }
        }

        if input == b"panic" {
            panic!("test-guest: panic requested");
        }

        let Ok(text) = std::str::from_utf8(&input) else {
            return input;
        };

        if let Some(rest) = text.strip_prefix("kv:put:") {
            let mut parts = rest.splitn(2, ':');
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            kv::put(key, value.as_bytes());
            return b"ok".to_vec();
        }

        if let Some(key) = text.strip_prefix("kv:get:") {
            return match kv::get(key) {
                Some(value) => value,
                None => b"none".to_vec(),
            };
        }

        if let Some(msg) = text.strip_prefix("log:") {
            log::emit("info", msg);
            return b"ok".to_vec();
        }

        if let Some(url) = text.strip_prefix("fetch:") {
            let req = Request {
                url: url.to_string(),
                method: "GET".to_string(),
                body: Vec::new(),
            };
            return match fetch(&req) {
                Ok(resp) => {
                    let body = String::from_utf8_lossy(&resp.body);
                    format!("status:{}:{body}", resp.status).into_bytes()
                }
                Err(e) => format!("err:{e}").into_bytes(),
            };
        }

        input
    }
}

export!(TestGuest);
