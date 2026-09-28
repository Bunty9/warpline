//! Content-addressed module registry shared by `warpline-control` (writer)
//! and `warpline-host` (reader).
//!
//! Layout under `WARPLINE_MODULES_DIR` (default `./modules`):
//!
//! ```text
//! modules/
//!   cwasm/{digest}-{compat}.cwasm   -- shared, content-addressed (cache.rs)
//!   tenants/{tenant}/{func}         -- pointer file, contents = digest
//! ```
//!
//! One flat `cwasm/` directory means two tenants uploading the same module
//! share the compiled cache entry — see `cache.rs` module docs. The
//! pointer file is the only per-(tenant, func) state; the host re-reads it
//! on every invoke (see [`resolve`]) so a re-upload takes effect
//! immediately, and [`ComponentCache`] is what keeps a warm invoke from
//! re-deserializing the `.cwasm` on every call.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lru::LruCache;
use wasmtime::component::Component;
use wasmtime::Engine;

use crate::cache;
use crate::types::valid_name;

/// Cap on the in-memory [`ComponentCache`] — see [`ComponentCache::new`].
pub const COMPONENT_CACHE_CAP: usize = 256;

/// The shared, content-addressed `.cwasm` cache dir under `modules_dir` —
/// what `cache::load_or_compile`/`cache::load_cwasm` read and write.
pub fn cwasm_dir(modules_dir: &Path) -> PathBuf {
    modules_dir.join("cwasm")
}

fn pointer_path(modules_dir: &Path, tenant: &str, func: &str) -> Option<PathBuf> {
    if !valid_name(tenant) || !valid_name(func) {
        return None;
    }
    Some(modules_dir.join("tenants").join(tenant).join(func))
}

/// Atomically write the pointer file for `(tenant, func)` so it now
/// resolves to `digest`. Same tempfile-in-same-dir-then-rename pattern as
/// `cache::load_or_compile`'s `.cwasm` writes, for the same reason: a
/// reader (`read_pointer`) never observes a partial write.
pub fn write_pointer(
    modules_dir: &Path,
    tenant: &str,
    func: &str,
    digest: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        cache::is_valid_digest(digest),
        "invalid digest: expected 64 lowercase hex characters"
    );
    let path = pointer_path(modules_dir, tenant, func)
        .ok_or_else(|| anyhow::anyhow!("invalid tenant or function name"))?;
    let parent = path.parent().expect("pointer_path always has a parent");
    std::fs::create_dir_all(parent)?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    tmp.write_all(digest.as_bytes())?;
    tmp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    tmp.persist(&path)?;
    Ok(())
}

/// Read the pointer file for `(tenant, func)`, returning `Ok(None)` if no
/// function has been uploaded under that name. Validates both the
/// tenant/func names and the file's contents (must be a 64-hex-char
/// digest) before handing anything back — a pointer file is host-trusted
/// (only `write_pointer` writes it), but the digest still ends up in a
/// cache-directory path, so the same shape check `cache::load_cwasm` does
/// applies here too.
pub fn read_pointer(
    modules_dir: &Path,
    tenant: &str,
    func: &str,
) -> anyhow::Result<Option<String>> {
    let Some(path) = pointer_path(modules_dir, tenant, func) else {
        return Ok(None);
    };
    let digest = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let digest = digest.trim();
    anyhow::ensure!(
        cache::is_valid_digest(digest),
        "corrupt pointer file {}: not a valid digest",
        path.display()
    );
    Ok(Some(digest.to_string()))
}

/// In-memory LRU of loaded [`Component`]s keyed by content digest, so a
/// warm invoke (whose pointer resolves to a digest this cache already
/// holds) skips `Component::deserialize` entirely. A miss falls through to
/// [`cache::load_cwasm`] against `modules_dir`'s shared `cwasm/` directory.
pub struct ComponentCache {
    inner: Mutex<LruCache<String, Arc<Component>>>,
}

impl ComponentCache {
    /// A cache holding up to [`COMPONENT_CACHE_CAP`] components.
    pub fn new() -> Self {
        Self::with_capacity(COMPONENT_CACHE_CAP)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(NonZeroUsize::new(cap.max(1)).unwrap())),
        }
    }

    /// Resolve `digest` to a `Component`, hitting the LRU first.
    pub fn get_or_load(
        &self,
        engine: &Engine,
        modules_dir: &Path,
        digest: &str,
    ) -> anyhow::Result<Arc<Component>> {
        if let Some(hit) = self.inner.lock().unwrap().get(digest) {
            return Ok(hit.clone());
        }
        let component = Arc::new(cache::load_cwasm(engine, &cwasm_dir(modules_dir), digest)?);
        self.inner
            .lock()
            .unwrap()
            .put(digest.to_string(), component.clone());
        Ok(component)
    }
}

impl Default for ComponentCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `(tenant, func)` to a loaded [`Component`], reading the pointer
/// file fresh on every call — see module docs — and going through `cache`
/// for the `Component` itself. Returns `Ok(None)` if no function has been
/// uploaded under that name (the host maps that to 404).
pub fn resolve(
    engine: &Engine,
    cache: &ComponentCache,
    modules_dir: &Path,
    tenant: &str,
    func: &str,
) -> anyhow::Result<Option<Arc<Component>>> {
    let Some(digest) = read_pointer(modules_dir, tenant, func)? else {
        return Ok(None);
    };
    Ok(Some(cache.get_or_load(engine, modules_dir, &digest)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_pointer_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let digest = "a".repeat(64);
        write_pointer(dir.path(), "tenant-a", "fn-a", &digest).unwrap();
        assert_eq!(
            read_pointer(dir.path(), "tenant-a", "fn-a").unwrap(),
            Some(digest)
        );
    }

    #[test]
    fn read_pointer_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pointer(dir.path(), "tenant-a", "fn-a").unwrap(), None);
    }

    #[test]
    fn read_pointer_invalid_names_are_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pointer(dir.path(), "../etc", "fn-a").unwrap(), None);
    }

    #[test]
    fn write_pointer_rejects_bad_digest() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_pointer(dir.path(), "tenant-a", "fn-a", "not-a-digest").is_err());
    }

    #[test]
    fn re_upload_overwrites_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let d1 = "a".repeat(64);
        let d2 = "b".repeat(64);
        write_pointer(dir.path(), "tenant-a", "fn-a", &d1).unwrap();
        write_pointer(dir.path(), "tenant-a", "fn-a", &d2).unwrap();
        assert_eq!(
            read_pointer(dir.path(), "tenant-a", "fn-a").unwrap(),
            Some(d2)
        );
    }
}
