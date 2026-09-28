//! `.cwasm` content-hashed component cache.
//!
//! Warm path: hash the source `.wasm` component with SHA-256, look up
//! `cache_dir/{hex_digest}-{compat_hash}.cwasm`, deserialise it back into a
//! `wasmtime::component::Component` without a cranelift pass. `compat_hash`
//! is derived from `Engine::precompile_compatibility_hash()`, which changes
//! whenever the engine's build/config would produce a `.cwasm` the current
//! engine can't load — folding it into the file name means a binary
//! upgrade (or a config change) can never load a `.cwasm` compiled by a
//! different, incompatible engine; it just misses the cache and recompiles.
//! As a second line of defence, if `Component::deserialize` still fails
//! (corrupt file, partial write that predates the atomic-rename fix, disk
//! bitrot), `load_or_compile` logs a warning, recompiles from source, and
//! overwrites the stale entry rather than propagating the error.
//!
//! Cold path: compile via `Component::new` (full cranelift), then
//! `serialize` the result to disk for the next call. The write goes to a
//! `tempfile::NamedTempFile` created directly in `cache_dir` (so the rename
//! that publishes it is same-filesystem and atomic) and `sync_all`'d before
//! the rename, so a reader never observes a partially-written `.cwasm` and
//! concurrent writers of the same digest converge on identical bytes
//! instead of corrupting each other's file. Using the crate's own
//! collision-resistant naming (rather than a hand-rolled
//! `{digest}.{pid}.tmp`) also means two processes on the same host never
//! pick the same temp path.
//!
//! The `Component::deserialize` call is `unsafe` because wasmtime cannot
//! fully verify that a `.cwasm` blob was produced by the same engine
//! version + config it is being loaded into — the compat hash in the file
//! name narrows that gap but doesn't close it, so `load_cwasm` also
//! rejects any digest that isn't exactly 64 lowercase hex characters before
//! it ever builds a path, so a digest can't be used to escape `cache_dir`
//! (e.g. `../../etc/passwd`) on its way to that `unsafe` call. The
//! control-plane is the only writer to this directory and `cache_dir` is
//! otherwise treated as trusted local storage — that's the standard
//! `cwasm` trust model (see wasmtime docs).

use std::hash::{Hash, Hasher};
use std::io::Write;
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

/// A [`Hasher`] that feeds every byte it's given into a SHA-256 digest,
/// instead of folding them into a `u64`. `precompile_compatibility_hash`
/// only gives us `impl Hash`, and `std::hash::Hasher` is the API surface
/// for consuming one; `finish()` (a `u64`) would throw most of that entropy
/// away and isn't guaranteed stable the way SHA-256 is, so we drive our own
/// hasher instead of `DefaultHasher`.
struct Sha256Hasher(Sha256);

impl Hasher for Sha256Hasher {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        unreachable!("Sha256Hasher is write-only — read the digest via finalize_hex")
    }
}

impl Sha256Hasher {
    fn finalize_hex(self) -> String {
        hex::encode(self.0.finalize())
    }
}

/// Hex-encoded SHA-256 over `engine`'s `precompile_compatibility_hash()` —
/// changes whenever a `.cwasm` compiled by `engine` would no longer be
/// guaranteed to load in it (wasmtime version, target, codegen config, ...).
fn compat_hash(engine: &Engine) -> String {
    let mut hasher = Sha256Hasher(Sha256::new());
    engine.precompile_compatibility_hash().hash(&mut hasher);
    hasher.finalize_hex()
}

/// Returns true iff `s` is exactly 64 lowercase hex characters — the shape
/// of a SHA-256 digest as produced by [`digest`]. See module docs for why
/// [`load_cwasm`] checks this before touching the filesystem. `pub(crate)`
/// so `crate::registry` can apply the same check to pointer-file contents
/// before it ever builds a path out of them.
pub(crate) fn is_valid_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `.cwasm` file name [`load_or_compile`]/[`load_cwasm`] use for a given
/// source `digest` under `engine`'s current compatibility hash. Exposed so
/// callers (tests, ops tooling) can locate a cache entry on disk without
/// duplicating the naming scheme.
pub fn cache_file_name(engine: &Engine, digest: &str) -> String {
    format!("{digest}-{}.cwasm", compat_hash(engine))
}

/// Deserialise `cache_dir/{digest}-{compat}.cwasm` into a [`Component`].
pub fn load_cwasm(engine: &Engine, cache_dir: &Path, digest: &str) -> anyhow::Result<Component> {
    anyhow::ensure!(
        is_valid_digest(digest),
        "invalid cache digest: expected 64 lowercase hex characters"
    );
    let bytes = std::fs::read(cache_dir.join(cache_file_name(engine, digest)))?;
    // SAFETY: see module-level docs — `digest` has just been validated as a
    // 64-hex-char SHA-256, the file name additionally carries the engine's
    // compatibility hash, and `cache_dir` is host-trusted storage.
    unsafe { Ok(Component::deserialize(engine, &bytes)?) }
}

/// Compute the content hash of `wasm_bytes` and either deserialise the
/// cached `.cwasm` for it, or compile + persist a fresh one.
///
/// If a cached entry exists but fails to deserialise (corrupt or stale),
/// this logs a warning and falls through to recompiling from `wasm_bytes`,
/// overwriting the bad entry — it never propagates the deserialize error.
///
/// Returns the loaded [`Component`] in both cases.
pub fn load_or_compile(
    engine: &Engine,
    wasm_bytes: &[u8],
    cache_dir: &Path,
) -> anyhow::Result<Component> {
    let digest = digest(wasm_bytes);
    let cached = cache_dir.join(cache_file_name(engine, &digest));

    if cached.exists() {
        match load_cwasm(engine, cache_dir, &digest) {
            Ok(component) => return Ok(component),
            Err(e) => {
                tracing::warn!(
                    path = %cached.display(),
                    error = %e,
                    "stale or corrupt .cwasm cache entry, recompiling"
                );
            }
        }
    }

    std::fs::create_dir_all(cache_dir)?;
    let component = Component::new(engine, wasm_bytes)?;
    let serialized = component.serialize()?;

    // Same-directory temp file (so the rename below is same-filesystem and
    // atomic) + `sync_all` before publishing, so a concurrent reader never
    // sees a truncated `.cwasm` and a crash between write and rename never
    // leaves a corrupt file at the published path.
    let mut tmp = tempfile::NamedTempFile::new_in(cache_dir)?;
    tmp.write_all(&serialized)?;
    tmp.as_file().sync_all()?;
    // NamedTempFile is 0600; the host may run as a different user than the
    // control plane, so publish world-readable like `fs::write` would.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    tmp.persist(&cached)?;

    Ok(component)
}
