//! `.cwasm` content-hashed module cache.
//!
//! Warm path: hash the source `.wasm` with SHA-256, look up
//! `cache_dir/{hex_digest}.cwasm`, deserialise it back into a `wasmtime::Module`
//! without a cranelift pass. The hex digest doubles as a tamper-evident name —
//! any rebuild of the same module hits the same file.
//!
//! Cold path: compile via `Module::new` (full cranelift), then `serialize`
//! the result to disk for the next call.
//!
//! The `Module::deserialize` call is `unsafe` because wasmtime cannot verify
//! that a `.cwasm` blob was produced by the same engine version + config it
//! is being loaded into. The control-plane is the only writer to this
//! directory, the file name is the SHA-256 of the source wasm, and the
//! `cache_dir` is treated as trusted local storage — that's the standard
//! `cwasm` trust model (see wasmtime docs).

use std::path::Path;

use sha2::{Digest, Sha256};
use wasmtime::{Engine, Module};

/// Compute the content hash of `wasm_bytes` and either deserialise the cached
/// `.cwasm` for it, or compile + persist a fresh one.
///
/// Returns the loaded [`Module`] in both cases.
pub fn load_or_compile(
    engine: &Engine,
    wasm_bytes: &[u8],
    cache_dir: &Path,
) -> anyhow::Result<Module> {
    let mut hasher = Sha256::new();
    hasher.update(wasm_bytes);
    let digest = hex::encode(hasher.finalize());
    let cached = cache_dir.join(format!("{digest}.cwasm"));

    if cached.exists() {
        let bytes = std::fs::read(&cached)?;
        // SAFETY: see module-level docs — the cache_dir is host-trusted and
        // the file name is the SHA-256 of the wasm source.
        let module = unsafe { Module::deserialize(engine, &bytes)? };
        Ok(module)
    } else {
        std::fs::create_dir_all(cache_dir)?;
        let module = Module::new(engine, wasm_bytes)?;
        let bytes = module.serialize()?;
        std::fs::write(&cached, &bytes)?;
        Ok(module)
    }
}
