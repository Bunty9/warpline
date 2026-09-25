//! Minimal warpline guest — the simplest possible `handle` export.
//!
//! Built with:
//!
//! ```bash
//! cargo build --target wasm32-wasip1 --release
//! ```
//!
//! The output goes to `target/wasm32-wasip1/release/hello_wasm.wasm` and
//! gets uploaded to `warpline-control` to be compiled into a `.cwasm` cache
//! entry. See `README.md` in this directory.

#![no_std]

// Tiny static payload returned by `handle`. The runtime returns these bytes
// directly to the HTTP caller.
static GREETING: &[u8] = b"hello from wasm";

// TODO(phase-2): wire wit-bindgen so the export matches the
// `warpline:host/handler` world declared in `wit/warpline.wit`. For Phase 1
// we expose a raw C ABI symbol that the host can resolve by name. The two
// returned values (pointer + length) let the host read the static greeting
// bytes out of guest linear memory.

#[no_mangle]
pub extern "C" fn handle_ptr() -> *const u8 {
    GREETING.as_ptr()
}

#[no_mangle]
pub extern "C" fn handle_len() -> usize {
    GREETING.len()
}

#[no_mangle]
pub extern "C" fn handle() -> u32 {
    // Placeholder marker — the real Component-Model export returns
    // `list<u8>`. Phase 2 replaces this with the wit-bindgen-generated glue.
    0
}

// `#![no_std]` requires a panic handler. Abort is the right answer inside a
// sandboxed guest — any panic is a bug in the guest, and the host's trap
// handler will catch the resulting `unreachable`.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
