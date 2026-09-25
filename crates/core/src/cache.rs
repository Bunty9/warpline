//! `.cwasm` content-hashed component cache.
//!
//! Warm path: hash the source `.wasm` component with SHA-256, look up
//! `cache_dir/{hex_digest}.cwasm`, deserialise it back into a
//! `wasmtime::component::Component` without a cranelift pass. The hex
//! digest doubles as a tamper-evident name — any rebuild of the same
//! component hits the same file.
//!
//! Cold path: compile via `Component::new` (full cranelift), then
//! `serialize` the result to disk for the next call. The write is atomic —
//! a temp file in the same directory, renamed into place — so a reader
//! never observes a partially-written `.cwasm`, and concurrent writers of
//! the same digest converge on identical bytes rather than corrupting each
//! other's file.
//!
//! The `Component::deserialize` call is `unsafe` because wasmtime cannot
//! verify that a `.cwasm` blob was produced by the same engine version +
//! config it is being loaded into. The control-plane is the only writer to
//! this directory, the file name is the SHA-256 of the source wasm, and the
//! `cache_dir` is treated as trusted local storage — that's the standard
//! `cwasm` trust model (see wasmtime docs).

use std::path::Path;

use sha2::{Digest, Sha256};
use wasmtime::component::Component;
use wasmtime::Engine;

/// Hex-encoded SHA-256 of `bytes` — the cache key and `.cwasm` file stem.
pub fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Deserialise `cache_dir/{digest}.cwasm` into a [`Component`].
pub fn load_cwasm(engine: &Engine, cache_dir: &Path, digest: &str) -> anyhow::Result<Component> {
    let bytes = std::fs::read(cache_dir.join(format!("{digest}.cwasm")))?;
    // SAFETY: see module-level docs — the cache_dir is host-trusted and the
    // file name is the SHA-256 of the wasm source.
    unsafe { Ok(Component::deserialize(engine, &bytes)?) }
}

/// Compute the content hash of `wasm_bytes` and either deserialise the
/// cached `.cwasm` for it, or compile + persist a fresh one.
///
/// Returns the loaded [`Component`] in both cases.
pub fn load_or_compile(
    engine: &Engine,
    wasm_bytes: &[u8],
    cache_dir: &Path,
) -> anyhow::Result<Component> {
    let digest = digest(wasm_bytes);
    let cached = cache_dir.join(format!("{digest}.cwasm"));

    if cached.exists() {
        return load_cwasm(engine, cache_dir, &digest);
    }

    std::fs::create_dir_all(cache_dir)?;
    let component = Component::new(engine, wasm_bytes)?;
    let serialized = component.serialize()?;

    // Atomic write: same-directory temp file + rename, so a concurrent
    // reader never sees a truncated `.cwasm`.
    let tmp = cache_dir.join(format!("{digest}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &serialized)?;
    std::fs::rename(&tmp, &cached)?;

    Ok(component)
}
