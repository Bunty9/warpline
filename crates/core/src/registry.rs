//! Content-addressed module registry shared by `warpline-control` (writer)
//! and `warpline-host` (reader).
//!
//! Layout under `WARPLINE_MODULES_DIR` (default `./modules`):
//!
//! ```text
//! modules/
//!   wasm/{digest}.wasm              -- shared, content-addressed source
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
//!
//! The source `wasm/` copy exists because the `.cwasm` cache key folds in
//! `Engine::precompile_compatibility_hash()` (see `cache.rs`): an engine
//! upgrade or config change makes every existing `.cwasm` file unloadable
//! by the new binary, and a `.cwasm` can also simply go missing (deleted,
//! disk issue). Without the source on disk too, every pointer into that
//! stale cache would 500 forever; with it, [`ComponentCache::get_or_load`]
//! recompiles on a miss instead.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lru::LruCache;
use wasmtime::component::Linker;
use wasmtime::Engine;

use crate::cache;
use crate::runtime::HandlerPre;
use crate::types::{valid_name, HostCtx};

/// Cap on the in-memory [`ComponentCache`] entry count — see
/// [`ComponentCache::new`].
pub const COMPONENT_CACHE_CAP: usize = 256;
/// Cap on the in-memory [`ComponentCache`]'s total serialized-cwasm bytes —
/// see [`ComponentCache::new`].
pub const COMPONENT_CACHE_BYTE_BUDGET: usize = 512 * 1024 * 1024;

/// The shared, content-addressed `.cwasm` cache dir under `modules_dir` —
/// what `cache::load_or_compile`/`cache::load_cwasm` read and write.
pub fn cwasm_dir(modules_dir: &Path) -> PathBuf {
    modules_dir.join("cwasm")
}

/// The shared, content-addressed source-`.wasm` dir under `modules_dir` —
/// see module docs.
pub fn wasm_dir(modules_dir: &Path) -> PathBuf {
    modules_dir.join("wasm")
}

fn wasm_path(modules_dir: &Path, digest: &str) -> PathBuf {
    wasm_dir(modules_dir).join(format!("{digest}.wasm"))
}

fn pointer_path(modules_dir: &Path, tenant: &str, func: &str) -> Option<PathBuf> {
    if !valid_name(tenant) || !valid_name(func) {
        return None;
    }
    Some(modules_dir.join("tenants").join(tenant).join(func))
}

/// Atomically write the pointer file for `(tenant, func)` so it now
/// resolves to `digest`. Same tempfile-in-same-dir-then-rename pattern as
/// `cache`'s writes, for the same reason: a reader (`read_pointer`) never
/// observes a partial write.
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
    cache::atomic_write(parent, &path, digest.as_bytes())
}

/// Atomically persist the uploaded source `.wasm` bytes for `digest` under
/// `modules_dir/wasm/` — see module docs. A no-op if the content-addressed
/// file already exists (re-upload of identical bytes, or two tenants
/// uploading the same module).
pub fn write_wasm_source(
    modules_dir: &Path,
    digest: &str,
    wasm_bytes: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        cache::is_valid_digest(digest),
        "invalid digest: expected 64 lowercase hex characters"
    );
    let dest = wasm_path(modules_dir, digest);
    if dest.exists() {
        return Ok(());
    }
    cache::atomic_write(&wasm_dir(modules_dir), &dest, wasm_bytes)
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

/// One [`ComponentCache`] entry: the pre-instantiated [`HandlerPre`] (built
/// once from the loaded `Component` — see [`ComponentCache::get_or_load`])
/// plus the serialized `.cwasm` byte length it cost to load — the weight
/// the cache bounds its total footprint by (see [`ComponentCache::insert`]).
struct CacheEntry {
    pre: Arc<HandlerPre<HostCtx>>,
    weight: usize,
}

struct ComponentCacheInner {
    lru: LruCache<String, CacheEntry>,
    total_bytes: usize,
}

/// In-memory LRU of loaded [`Component`](wasmtime::component::Component)s keyed by content digest, so a
/// warm invoke (whose pointer resolves to a digest this cache already
/// holds) skips `Component::deserialize` entirely. A miss falls through to
/// [`cache::load_cwasm`] against `modules_dir`'s shared `cwasm/` directory,
/// and (on top of that failing) to recompiling from the shared `wasm/`
/// source directory — see [`Self::get_or_load`].
///
/// Bounded by both entry count and total serialized-cwasm bytes: a handful
/// of huge components hitting the byte budget evicts before the count cap
/// would ever be reached, and vice versa for many small ones.
pub struct ComponentCache {
    inner: Mutex<ComponentCacheInner>,
    byte_budget: usize,
}

impl ComponentCache {
    /// A cache holding up to [`COMPONENT_CACHE_CAP`] components and
    /// [`COMPONENT_CACHE_BYTE_BUDGET`] total serialized bytes.
    pub fn new() -> Self {
        Self::with_capacity(COMPONENT_CACHE_CAP)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self::with_capacity_and_byte_budget(cap, COMPONENT_CACHE_BYTE_BUDGET)
    }

    pub fn with_capacity_and_byte_budget(cap: usize, byte_budget: usize) -> Self {
        Self {
            inner: Mutex::new(ComponentCacheInner {
                lru: LruCache::new(NonZeroUsize::new(cap.max(1)).unwrap()),
                total_bytes: 0,
            }),
            byte_budget,
        }
    }

    /// Resolve `digest` to a pre-instantiated [`HandlerPre`], hitting the
    /// LRU first, then the on-disk `.cwasm` cache, then — if that's missing
    /// or fails to deserialize (deleted, corrupt, or invalidated by an
    /// engine/config upgrade — see module docs) — recompiling from the
    /// persisted source `wasm/{digest}.wasm` and re-publishing a fresh
    /// `.cwasm` for next time. A cache miss additionally pays for
    /// `linker.instantiate_pre`'s import/export type-check exactly once per
    /// digest (not per invoke) — see [`crate::runtime::instantiate_pre`].
    ///
    /// Concurrent misses on the same digest each recompile independently.
    // ponytail: no single-flight dedupe on a cold digest — a burst of
    // concurrent first-invokes for one freshly-uploaded function each pay
    // for their own recompile. Add a per-digest in-flight map if that shows
    // up as real load.
    pub fn get_or_load(
        &self,
        engine: &Engine,
        linker: &Linker<HostCtx>,
        modules_dir: &Path,
        digest: &str,
    ) -> anyhow::Result<Arc<HandlerPre<HostCtx>>> {
        if let Some(hit) = self
            .inner
            .lock()
            .unwrap()
            .lru
            .get(digest)
            .map(|e| e.pre.clone())
        {
            return Ok(hit);
        }

        let cwasm_dir = cwasm_dir(modules_dir);
        let component = match cache::load_cwasm(engine, &cwasm_dir, digest) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    digest,
                    error = %e,
                    "cwasm cache miss/corrupt, recompiling from stored source"
                );
                let wasm_bytes = std::fs::read(wasm_path(modules_dir, digest)).map_err(|_| {
                    anyhow::anyhow!("no cwasm and no source wasm on disk for digest {digest}")
                })?;
                cache::load_or_compile(engine, &wasm_bytes, &cwasm_dir)?
            }
        };
        let weight = std::fs::metadata(cwasm_dir.join(cache::cache_file_name(engine, digest)))
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let pre = Arc::new(crate::runtime::instantiate_pre(linker, &component)?);
        self.insert(digest, pre.clone(), weight);
        Ok(pre)
    }

    /// Insert `(digest, pre)` weighted at `weight` bytes, evicting
    /// least-recently-used entries (by count, via `LruCache::put`, and by
    /// `weight` via `pop_lru`) until back under both caps.
    fn insert(&self, digest: &str, pre: Arc<HandlerPre<HostCtx>>, weight: usize) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(evicted) = inner
            .lru
            .put(digest.to_string(), CacheEntry { pre, weight })
        {
            inner.total_bytes = inner.total_bytes.saturating_sub(evicted.weight);
        }
        inner.total_bytes += weight;
        while inner.total_bytes > self.byte_budget {
            match inner.lru.pop_lru() {
                Some((_, evicted)) => {
                    inner.total_bytes = inner.total_bytes.saturating_sub(evicted.weight)
                }
                None => break,
            }
        }
    }
}

impl Default for ComponentCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `(tenant, func)` to a loaded, pre-instantiated [`HandlerPre`].
/// The pointer read and (on a cwasm miss) the recompile-from-source
/// fallback are both blocking filesystem/CPU work, so this runs on a
/// blocking-pool thread rather than an async worker — see
/// `warpline-host`'s call site.
pub async fn resolve(
    engine: Engine,
    linker: Linker<HostCtx>,
    cache: Arc<ComponentCache>,
    modules_dir: PathBuf,
    tenant: String,
    func: String,
) -> anyhow::Result<Option<Arc<HandlerPre<HostCtx>>>> {
    tokio::task::spawn_blocking(move || {
        let Some(digest) = read_pointer(&modules_dir, &tenant, &func)? else {
            return Ok(None);
        };
        cache
            .get_or_load(&engine, &linker, &modules_dir, &digest)
            .map(Some)
    })
    .await
    .map_err(|e| anyhow::anyhow!("resolve task panicked: {e}"))?
}

/// Default `grace_period` for [`gc_unreferenced_blobs`] — an hour is
/// comfortably longer than any single upload's compile-then-persist-then
/// point window could plausibly take, so nothing an in-flight upload has
/// touched in the last hour is ever a GC candidate.
pub const GC_GRACE_PERIOD: Duration = Duration::from_secs(60 * 60);

/// `true` iff `path`'s mtime is at least `grace_period` old. A path this
/// can't `stat` (already gone, race with another remover, permissions)
/// counts as "not eligible" — the caller's own `remove_file` will no-op or
/// surface its own error on this or the next pass, there's nothing to
/// double-check here.
fn older_than(path: &Path, grace_period: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| mtime.elapsed().unwrap_or(Duration::ZERO) >= grace_period)
        .unwrap_or(false)
}

/// Delete `dir`'s direct `tempfile`-crate leftovers (`.tmp*` — see
/// `cache::atomic_write`, the only writer under `modules_dir`) that are
/// older than `grace_period`. A process that crashes between creating a
/// `NamedTempFile` and the rename that publishes it leaves one of these
/// behind forever otherwise; a fresh in-flight write is left alone by the
/// grace period the same way a fresh blob is.
fn sweep_stale_temp_files(dir: &Path, grace_period: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(".tmp")
                && older_than(&entry.path(), grace_period)
        })
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Delete every `wasm/{digest}.wasm` blob, `cwasm/{digest}-{compat}.cwasm`
/// blob, and stray `tempfile` leftover under `modules_dir` that's both
/// older than `grace_period` and either unreferenced by any `(tenant,
/// func)` pointer, or — `.cwasm` files only — compiled for an engine other
/// than `engine`'s current `precompile_compatibility_hash()` (it can never
/// be loaded again; `ComponentCache::get_or_load`'s recompile-from-source
/// fallback would just replace it with a fresh one on next access, so there
/// is no reason to keep it around).
///
/// `grace_period` exists so a blob an in-flight upload just wrote (persisted
/// before its pointer, or between an engine upgrade and the next request
/// that would recompile it) is never collected out from under it — pass
/// [`GC_GRACE_PERIOD`] in production, `Duration::ZERO` in a test that wants
/// to observe collection immediately.
///
/// Meant to run once, at control-plane startup before serving, so a crash
/// between persisting a blob and writing its pointer (or an old upload that
/// never made it past typecheck, back when the cwasm was written before
/// that check) doesn't leak disk forever. Best-effort: a file that can't be
/// removed is skipped rather than aborting the whole pass. Returns the
/// number of files removed.
pub fn gc_unreferenced_blobs(
    modules_dir: &Path,
    engine: &Engine,
    grace_period: Duration,
) -> anyhow::Result<usize> {
    let mut referenced: HashSet<String> = HashSet::new();
    let mut removed = 0usize;
    let tenants_dir = modules_dir.join("tenants");
    if tenants_dir.exists() {
        for tenant_entry in std::fs::read_dir(&tenants_dir)? {
            let tenant_entry = tenant_entry?;
            if !tenant_entry.file_type()?.is_dir() {
                continue;
            }
            removed += sweep_stale_temp_files(&tenant_entry.path(), grace_period);
            for func_entry in std::fs::read_dir(tenant_entry.path())? {
                let func_entry = func_entry?;
                if let Ok(digest) = std::fs::read_to_string(func_entry.path()) {
                    let digest = digest.trim();
                    if cache::is_valid_digest(digest) {
                        referenced.insert(digest.to_string());
                    }
                }
            }
        }
    }

    let wdir = wasm_dir(modules_dir);
    if wdir.exists() {
        removed += sweep_stale_temp_files(&wdir, grace_period);
        for entry in std::fs::read_dir(&wdir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(digest) = name.strip_suffix(".wasm") {
                if !referenced.contains(digest)
                    && older_than(&entry.path(), grace_period)
                    && std::fs::remove_file(entry.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
    }

    let current_compat = cache::compat_hash(engine);
    let cdir = cwasm_dir(modules_dir);
    if cdir.exists() {
        removed += sweep_stale_temp_files(&cdir, grace_period);
        for entry in std::fs::read_dir(&cdir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_suffix(".cwasm") {
                if let Some((digest, compat)) = rest.split_once('-') {
                    let collectible = !referenced.contains(digest) || compat != current_compat;
                    if collectible
                        && older_than(&entry.path(), grace_period)
                        && std::fs::remove_file(entry.path()).is_ok()
                    {
                        removed += 1;
                    }
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{build_engine, build_linker};

    const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test_guest.wasm"
    ));

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

    /// Finding 1: deleting the `.cwasm` (e.g. after an engine/config change
    /// invalidates the compatibility hash) must not permanently break an
    /// invoke — `get_or_load` should recompile from the persisted source
    /// and re-publish a fresh `.cwasm`.
    #[test]
    fn get_or_load_recompiles_from_source_after_cwasm_deleted() {
        let engine = build_engine().unwrap();
        let linker = build_linker(&engine).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let digest = cache::digest(TEST_GUEST_WASM);

        write_wasm_source(dir.path(), &digest, TEST_GUEST_WASM).unwrap();
        let component = wasmtime::component::Component::new(&engine, TEST_GUEST_WASM).unwrap();
        cache::persist_cwasm(&component, &engine, &cwasm_dir(dir.path()), &digest).unwrap();

        // Simulate the cwasm going missing entirely.
        std::fs::remove_dir_all(cwasm_dir(dir.path())).unwrap();
        assert!(!cwasm_dir(dir.path()).exists());

        let cache_obj = ComponentCache::new();
        let _loaded = cache_obj
            .get_or_load(&engine, &linker, dir.path(), &digest)
            .expect("should recompile from the persisted source wasm");

        // Recompiling should have re-published a cwasm for next time.
        assert!(cwasm_dir(dir.path())
            .join(cache::cache_file_name(&engine, &digest))
            .exists());
    }

    #[test]
    fn get_or_load_fails_cleanly_with_no_cwasm_and_no_source() {
        let engine = build_engine().unwrap();
        let linker = build_linker(&engine).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache_obj = ComponentCache::new();
        assert!(cache_obj
            .get_or_load(&engine, &linker, dir.path(), &"c".repeat(64))
            .is_err());
    }

    #[test]
    fn gc_removes_unreferenced_blobs_but_keeps_referenced() {
        let engine = build_engine().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let kept = "a".repeat(64);
        let orphan = "b".repeat(64);
        write_pointer(dir.path(), "tenant-a", "fn-a", &kept).unwrap();
        std::fs::create_dir_all(wasm_dir(dir.path())).unwrap();
        std::fs::write(wasm_dir(dir.path()).join(format!("{kept}.wasm")), b"x").unwrap();
        std::fs::write(wasm_dir(dir.path()).join(format!("{orphan}.wasm")), b"y").unwrap();

        let removed =
            gc_unreferenced_blobs(dir.path(), &engine, std::time::Duration::ZERO).unwrap();
        assert_eq!(removed, 1);
        assert!(wasm_dir(dir.path()).join(format!("{kept}.wasm")).exists());
        assert!(!wasm_dir(dir.path()).join(format!("{orphan}.wasm")).exists());
    }

    /// Finding 7: an unreferenced blob younger than `grace_period` must
    /// survive the pass — it could be mid-upload, persisted just before its
    /// pointer.
    #[test]
    fn gc_leaves_young_unreferenced_blobs_alone() {
        let engine = build_engine().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let orphan = "b".repeat(64);
        std::fs::create_dir_all(wasm_dir(dir.path())).unwrap();
        std::fs::write(wasm_dir(dir.path()).join(format!("{orphan}.wasm")), b"y").unwrap();

        let removed =
            gc_unreferenced_blobs(dir.path(), &engine, Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, 0);
        assert!(wasm_dir(dir.path()).join(format!("{orphan}.wasm")).exists());
    }

    /// Finding 7: a `.cwasm` compiled for a different engine than the
    /// current compat hash is collected even though its digest is still
    /// referenced — it can never be loaded again, and the recompile-from-
    /// source fallback will just replace it on next access.
    #[test]
    fn gc_removes_cwasm_with_stale_compat_hash_even_if_referenced() {
        let engine = build_engine().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let digest = "c".repeat(64);
        write_pointer(dir.path(), "tenant-a", "fn-a", &digest).unwrap();
        std::fs::create_dir_all(cwasm_dir(dir.path())).unwrap();
        let stale_path = cwasm_dir(dir.path()).join(format!("{digest}-deadbeef.cwasm"));
        std::fs::write(&stale_path, b"stale").unwrap();
        let current_path = cwasm_dir(dir.path()).join(cache::cache_file_name(&engine, &digest));
        std::fs::write(&current_path, b"current").unwrap();

        let removed =
            gc_unreferenced_blobs(dir.path(), &engine, std::time::Duration::ZERO).unwrap();
        assert_eq!(removed, 1);
        assert!(!stale_path.exists());
        assert!(current_path.exists());
    }

    /// Finding 7: a leftover `tempfile` crate temp file (`.tmp*`, see
    /// `cache::atomic_write`) older than the grace period is swept from the
    /// content-addressed dirs.
    #[test]
    fn gc_sweeps_stale_temp_files() {
        let engine = build_engine().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(wasm_dir(dir.path())).unwrap();
        let tmp = wasm_dir(dir.path()).join(".tmpABCDEF");
        std::fs::write(&tmp, b"partial").unwrap();

        let removed =
            gc_unreferenced_blobs(dir.path(), &engine, std::time::Duration::ZERO).unwrap();
        assert_eq!(removed, 1);
        assert!(!tmp.exists());
    }

    #[test]
    fn component_cache_evicts_by_byte_budget() {
        let engine = build_engine().unwrap();
        let linker = build_linker(&engine).unwrap();
        let cache = ComponentCache::with_capacity_and_byte_budget(256, 1);
        let component = wasmtime::component::Component::new(&engine, TEST_GUEST_WASM).unwrap();
        let pre = Arc::new(crate::runtime::instantiate_pre(&linker, &component).unwrap());
        cache.insert("d1", pre, 100);
        // Weight (100) far exceeds the 1-byte budget, so the entry must
        // have been evicted immediately after insertion.
        assert_eq!(cache.inner.lock().unwrap().total_bytes, 0);
        assert!(cache.inner.lock().unwrap().lru.is_empty());
    }
}
