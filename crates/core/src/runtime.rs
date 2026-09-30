//! The [`Runtime`] facade: the one type an embedding app holds.
//!
//! A `Runtime` owns the wasmtime engine and linker, the epoch ticker that
//! enforces CPU budgets, the on-disk module registry (content-addressed
//! `.wasm`/`.cwasm` blobs plus one pointer file per `(tenant, func)`), the
//! in-memory component cache, and admission control. It is cheap to clone
//! (an `Arc`); clones share everything, and the ticker stops only when the
//! last clone, including those held by in-flight invocations, is gone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Semaphore;
use wasmtime::component::Linker;
use wasmtime::Engine;

use crate::cache;
use crate::kv::{KvStore, MemKv};
use crate::registry::{self, ComponentCache, Loaded, Lookup};
use crate::sandbox::{self, EpochTicker, Failure};
use crate::types::{valid_name, HostCtx};
use crate::{Error, InvokeError, Limits, MeterSink, PublishError, Usage};

const MIB: usize = 1024 * 1024;

/// Configuration for a [`Runtime`]. Build with [`RuntimeConfig::new`] and
/// adjust the public fields; new fields may be added in minor releases.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RuntimeConfig {
    /// Root of the module registry. Created if missing.
    ///
    /// # Trust
    ///
    /// Everything under `modules_dir/cwasm` is deserialized as native
    /// machine code and executed. Anyone who can write there can run
    /// arbitrary code as the host process, so the directory (and its
    /// parents) must be writable only by the host process and trusted
    /// operators, never by tenants or shared with less-trusted services.
    pub modules_dir: PathBuf,
    /// Let guests' `http-out` reach loopback/private/link-local addresses.
    /// One flag drives both the DNS resolver filter and the IP-literal check.
    /// Default `false`; enable only for local development and tests.
    pub allow_private_egress: bool,
    /// Total guest memory admitted concurrently, in bytes (default 1 GiB).
    /// Each invocation weighs its `Limits::mem_cap_bytes` rounded up to whole
    /// MiB; when the running total would exceed this, the invocation fails
    /// fast with [`InvokeError::Overloaded`]. At least 1 MiB.
    pub memory_budget_bytes: usize,
    /// Invocations one tenant may have in flight (default 32); beyond that
    /// [`InvokeError::TenantBusy`]. At least 1.
    pub max_in_flight_per_tenant: usize,
    /// Largest output an invocation may return, bytes (default 8 MiB).
    pub max_output_bytes: usize,
    /// Components kept loaded in memory (default 256). At least 1.
    pub component_cache_entries: usize,
    /// Serialized bytes of loaded components kept in memory (default 512 MiB).
    pub component_cache_bytes: usize,
    /// Concurrent compilations in [`Runtime::stage`] (default half the cores,
    /// at least 1).
    pub compile_concurrency: usize,
}

impl RuntimeConfig {
    /// Defaults everywhere except the registry location.
    pub fn new(modules_dir: impl Into<PathBuf>) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            modules_dir: modules_dir.into(),
            allow_private_egress: false,
            memory_budget_bytes: 1024 * MIB,
            max_in_flight_per_tenant: 32,
            max_output_bytes: 8 * MIB,
            component_cache_entries: 256,
            component_cache_bytes: 512 * MIB,
            compile_concurrency: (cores / 2).max(1),
        }
    }
}

/// Builder for a [`Runtime`] with a custom KV store and/or meter.
#[must_use]
pub struct RuntimeBuilder {
    cfg: RuntimeConfig,
    kv: Option<Arc<dyn KvStore>>,
    meter: Option<Arc<dyn MeterSink>>,
}

impl RuntimeBuilder {
    /// Back guests' `kv` capability with `kv` (default: a fresh [`MemKv`]).
    pub fn kv(mut self, kv: Arc<dyn KvStore>) -> Self {
        self.kv = Some(kv);
        self
    }

    /// Report every invocation that reached the guest to `sink` (default:
    /// none).
    pub fn meter(mut self, sink: Arc<dyn MeterSink>) -> Self {
        self.meter = Some(sink);
        self
    }

    /// Validate the config, create the registry directory and start the
    /// epoch ticker. Synchronous; only the async methods need a tokio runtime.
    pub fn build(self) -> Result<Runtime, Error> {
        let cfg = self.cfg;
        if cfg.memory_budget_bytes < MIB {
            return Err(Error::Config("memory_budget_bytes must be at least 1 MiB"));
        }
        if cfg.max_in_flight_per_tenant == 0 {
            return Err(Error::Config("max_in_flight_per_tenant must be at least 1"));
        }
        if cfg.component_cache_entries == 0 {
            return Err(Error::Config("component_cache_entries must be at least 1"));
        }
        if cfg.compile_concurrency == 0 {
            return Err(Error::Config("compile_concurrency must be at least 1"));
        }
        std::fs::create_dir_all(&cfg.modules_dir)?;

        let engine = sandbox::build_engine()?;
        let linker = Arc::new(sandbox::build_linker(&engine)?);
        let http_client = sandbox::build_http_client(cfg.allow_private_egress)
            .map_err(|e| Error::Internal(format!("failed to build http client: {e}")))?;
        let components = ComponentCache::new(
            engine.clone(),
            linker.clone(),
            cfg.modules_dir.clone(),
            cfg.component_cache_entries,
            cfg.component_cache_bytes,
        );
        let ticker = EpochTicker::spawn(engine.clone());
        Ok(Runtime(Arc::new(Inner {
            admission: Arc::new(Semaphore::new(cfg.memory_budget_bytes / MIB)),
            compile_slots: Arc::new(Semaphore::new(cfg.compile_concurrency)),
            in_flight: Mutex::new(HashMap::new()),
            kv: self.kv.unwrap_or_else(|| Arc::new(MemKv::new())),
            meter: self.meter,
            http_client,
            components,
            linker,
            engine,
            cfg,
            _ticker: ticker,
        })))
    }
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeBuilder")
            .field("cfg", &self.cfg)
            .field("custom_kv", &self.kv.is_some())
            .field("meter", &self.meter.is_some())
            .finish()
    }
}

struct Inner {
    cfg: RuntimeConfig,
    engine: Engine,
    linker: Arc<Linker<HostCtx>>,
    kv: Arc<dyn KvStore>,
    meter: Option<Arc<dyn MeterSink>>,
    http_client: reqwest::Client,
    components: ComponentCache,
    /// Permits are MiB of guest memory.
    admission: Arc<Semaphore>,
    in_flight: Mutex<HashMap<String, usize>>,
    compile_slots: Arc<Semaphore>,
    /// Stops (and joins) the epoch thread when the last `Arc<Inner>` goes.
    _ticker: EpochTicker,
}

/// A multi-tenant WebAssembly function runtime.
///
/// Publish a component under `(tenant, function)`, then invoke it with
/// per-call [`Limits`]. Cheap to clone; clones share all state.
///
/// ```
/// use std::sync::Arc;
/// use warpline_core::{Bytes, Limits, MemKv, Runtime, RuntimeConfig};
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let dir = tempfile::tempdir()?;
/// let runtime = Runtime::builder(RuntimeConfig::new(dir.path()))
///     .kv(Arc::new(MemKv::new()))
///     .build()?;
///
/// // A component implementing the `handler` world (see `wit/warpline.wit`);
/// // this one, from the test suite, echoes its input.
/// let wasm = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test_guest.wasm"));
/// runtime.publish("acme", "hello", Bytes::from_static(wasm)).await?;
///
/// let limits = Limits::new(50, 32 << 20)?;
/// let out = runtime.invoke("acme", "hello", b"hi".to_vec(), &limits).await?;
/// assert_eq!(out.output, b"hi");
/// println!("{} bytes back, {} us cpu", out.output.len(), out.usage.cpu_us);
/// # Ok(())
/// # }
/// ```
///
/// # Trust
///
/// The registry directory ([`RuntimeConfig::modules_dir`]) holds native code
/// (`cwasm/`) that is loaded without further checks. Keep it writable only
/// by the host process and trusted operators.
///
/// # Capacity
///
/// `invoke` is fail-fast: it never queues. A tenant beyond its in-flight
/// cap gets [`InvokeError::TenantBusy`] and an invocation the memory budget
/// cannot admit gets [`InvokeError::Overloaded`]; map those to 429/503 (see
/// [`InvokeError::http_status`]) and let the caller retry.
#[derive(Clone)]
pub struct Runtime(Arc<Inner>);

/// A component that has been compiled, type-checked and persisted, but is not
/// yet reachable by any function name. See [`Runtime::stage`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Staged {
    /// Hex SHA-256 of the component bytes; also [`digest`](crate::digest).
    pub digest: String,
}

/// A successful invocation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Invocation {
    /// The bytes the guest returned.
    pub output: Vec<u8>,
    /// What the call used. `cpu_us` has 1 ms granularity.
    pub usage: Usage,
}

impl Runtime {
    /// A runtime with an in-memory KV store and no meter.
    pub fn new(cfg: RuntimeConfig) -> Result<Self, Error> {
        Self::builder(cfg).build()
    }

    /// Start building a runtime with a custom KV store and/or meter.
    pub fn builder(cfg: RuntimeConfig) -> RuntimeBuilder {
        RuntimeBuilder {
            cfg,
            kv: None,
            meter: None,
        }
    }

    /// The wasmtime engine (the same version as [`crate::wasmtime`]), e.g.
    /// to compile components ahead of time with matching settings.
    pub fn engine(&self) -> &Engine {
        &self.0.engine
    }

    /// Compile `wasm`, check it fits the handler world and persist it, without
    /// making it reachable. Compilation runs on the blocking pool behind the
    /// `compile_concurrency` semaphore. Idempotent: re-staging identical bytes
    /// refreshes their timestamps so a concurrent [`gc`](Self::gc) leaves them
    /// for the [`activate`](Self::activate) that follows.
    ///
    /// Split from [`activate`](Self::activate) so a caller can do its own
    /// bookkeeping (a quota check, a database row) between the two.
    pub async fn stage(&self, wasm: Bytes) -> Result<Staged, PublishError> {
        let inner = self.0.clone();
        let permit = inner
            .compile_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                PublishError::Registry(Error::Internal("compile semaphore closed".into()))
            })?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let modules_dir = &inner.cfg.modules_dir;
            let cwasm_dir = registry::cwasm_dir(modules_dir);
            let (component, digest, fresh) = cache::compile(&inner.engine, &wasm, &cwasm_dir)
                .map_err(|e| match e {
                    Error::Wasmtime(e) => PublishError::Compile(e),
                    other => into_publish_err(other),
                })?;
            sandbox::typecheck_component(&inner.linker, &component)
                .map_err(PublishError::ImportMismatch)?;
            // Only a typechecked component is persisted.
            if fresh {
                cache::persist_cwasm(&component, &inner.engine, &cwasm_dir, &digest)
                    .map_err(into_publish_err)?;
            } else if let Err(e) =
                registry::touch(&cwasm_dir.join(cache::cache_file_name(&inner.engine, &digest)))
            {
                tracing::warn!(%digest, error = %e, "could not refresh .cwasm mtime");
            }
            registry::write_wasm_source(modules_dir, &digest, &wasm).map_err(into_publish_err)?;
            Ok(Staged { digest })
        })
        .await
        .map_err(|e| PublishError::Registry(Error::Internal(format!("stage task failed: {e}"))))?
    }

    /// Atomically point `(tenant, func)` at `staged`. Invocations that start
    /// afterwards run the new component; ones already running are unaffected.
    pub async fn activate(
        &self,
        tenant: &str,
        func: &str,
        staged: &Staged,
    ) -> Result<(), PublishError> {
        if !valid_name(tenant) || !valid_name(func) {
            return Err(PublishError::InvalidName);
        }
        let inner = self.0.clone();
        let (tenant, func, digest) = (tenant.to_owned(), func.to_owned(), staged.digest.clone());
        tokio::task::spawn_blocking(move || {
            registry::write_pointer(&inner.cfg.modules_dir, &tenant, &func, &digest)
        })
        .await
        .map_err(|e| PublishError::Registry(Error::Internal(format!("activate task failed: {e}"))))?
        .map_err(into_publish_err)
    }

    /// The component `(tenant, func)` currently points at, if any. Lets a
    /// caller that activates inside its own transaction remember what to
    /// [`activate`](Self::activate) again (or [`deactivate`](Self::deactivate)
    /// if `None`) when the transaction fails to commit.
    pub async fn active(&self, tenant: &str, func: &str) -> Result<Option<Staged>, Error> {
        let inner = self.0.clone();
        let (tenant, func) = (tenant.to_owned(), func.to_owned());
        let digest = tokio::task::spawn_blocking(move || {
            registry::read_pointer(&inner.cfg.modules_dir, &tenant, &func)
        })
        .await
        .map_err(|e| Error::Internal(format!("active task failed: {e}")))??;
        Ok(digest.map(|digest| Staged { digest }))
    }

    /// Remove the `(tenant, func)` pointer, so invocations get
    /// [`NotFound`](InvokeError::NotFound). Idempotent. The blobs stay until
    /// [`gc`](Self::gc) collects them.
    pub async fn deactivate(&self, tenant: &str, func: &str) -> Result<(), Error> {
        let inner = self.0.clone();
        let (tenant, func) = (tenant.to_owned(), func.to_owned());
        tokio::task::spawn_blocking(move || {
            registry::remove_pointer(&inner.cfg.modules_dir, &tenant, &func)
        })
        .await
        .map_err(|e| Error::Internal(format!("deactivate task failed: {e}")))?
    }

    /// Point `(tenant, func)` at a component that was staged earlier, given
    /// only its digest (e.g. read back from a database row). Errors if no
    /// source blob for `digest` exists.
    pub async fn activate_digest(
        &self,
        tenant: &str,
        func: &str,
        digest: &str,
    ) -> Result<(), PublishError> {
        if !registry::wasm_exists(&self.0.cfg.modules_dir, digest) {
            return Err(PublishError::Registry(Error::Corrupt(format!(
                "no staged component with digest {digest}"
            ))));
        }
        self.activate(
            tenant,
            func,
            &Staged {
                digest: digest.to_owned(),
            },
        )
        .await
    }

    /// [`stage`](Self::stage) then [`activate`](Self::activate).
    pub async fn publish(
        &self,
        tenant: &str,
        func: &str,
        wasm: Bytes,
    ) -> Result<Staged, PublishError> {
        if !valid_name(tenant) || !valid_name(func) {
            return Err(PublishError::InvalidName);
        }
        let staged = self.stage(wasm).await?;
        self.activate(tenant, func, &staged).await?;
        Ok(staged)
    }

    /// Run `(tenant, func)` on `input` under `limits`.
    ///
    /// Fails fast, in this order: [`InvalidName`](InvokeError::InvalidName),
    /// [`TenantBusy`](InvokeError::TenantBusy),
    /// [`Overloaded`](InvokeError::Overloaded),
    /// [`NotFound`](InvokeError::NotFound). Once the guest has started, the
    /// configured [`MeterSink`] is called exactly once with the outcome,
    /// including when the returned future is dropped mid-call (then as a
    /// failure). The tenant's in-flight slot and the memory admission are
    /// released on every exit path.
    ///
    /// Admission is taken *before* the module is loaded and held through it,
    /// so a burst of cold invocations (first use, or after an engine upgrade
    /// forces recompiles) occupies budget while loading and can see
    /// [`Overloaded`](InvokeError::Overloaded).
    pub async fn invoke(
        &self,
        tenant: &str,
        func: &str,
        input: Vec<u8>,
        limits: &Limits,
    ) -> Result<Invocation, InvokeError> {
        if !valid_name(tenant) || !valid_name(func) {
            return Err(InvokeError::InvalidName);
        }
        // Held for the whole call: keeps the ticker alive even if every
        // other `Runtime` handle is dropped meanwhile.
        let inner = self.0.clone();

        let _slot = TenantSlot::acquire(&inner, tenant).ok_or(InvokeError::TenantBusy)?;

        let weight = limits.mem_cap_bytes.div_ceil(MIB).max(1);
        let weight = u32::try_from(weight)
            .ok()
            .filter(|w| (*w as usize) <= inner.cfg.memory_budget_bytes / MIB)
            .ok_or(InvokeError::Overloaded)?;
        let _admission = inner
            .admission
            .clone()
            .try_acquire_many_owned(weight)
            .map_err(|_| InvokeError::Overloaded)?;

        let pre = {
            let lookup = {
                let inner = inner.clone();
                let (t, f) = (tenant.to_owned(), func.to_owned());
                tokio::task::spawn_blocking(move || inner.components.lookup(&t, &f))
                    .await
                    .map_err(|e| {
                        InvokeError::Load(Error::Internal(format!("resolve task failed: {e}")))
                    })?
                    .map_err(InvokeError::Load)?
            };
            match lookup {
                Lookup::NotFound => return Err(InvokeError::NotFound),
                Lookup::Hit(pre) => pre,
                Lookup::Miss(digest) => {
                    // Cheap path first, with no permit: deserialise the
                    // `.cwasm` on disk. Only when it needs a real compile do
                    // we queue for one, so a restart does not serialise cold
                    // loads behind in-flight stages.
                    let loaded = {
                        let (inner, digest) = (inner.clone(), digest.clone());
                        tokio::task::spawn_blocking(move || inner.components.load_existing(&digest))
                            .await
                            .map_err(|e| {
                                InvokeError::Load(Error::Internal(format!("load task failed: {e}")))
                            })?
                            .map_err(InvokeError::Load)?
                    };
                    match loaded {
                        Loaded::Ready(pre) => pre,
                        Loaded::NeedsCompile => {
                            // The permit is taken here, asynchronously, and
                            // moved into the blocking task: a blocking thread
                            // must never wait for a permit, or a burst of
                            // misses could park the whole blocking pool while
                            // the permit holders (stages) wait for a thread.
                            let permit =
                                inner.compile_slots.clone().acquire_owned().await.map_err(
                                    |_| {
                                        InvokeError::Load(Error::Internal(
                                            "compile semaphore closed".into(),
                                        ))
                                    },
                                )?;
                            let inner = inner.clone();
                            tokio::task::spawn_blocking(move || {
                                let _permit = permit;
                                inner.components.get_or_load(&digest)
                            })
                            .await
                            .map_err(|e| {
                                InvokeError::Load(Error::Internal(format!(
                                    "recompile task failed: {e}"
                                )))
                            })?
                            .map_err(InvokeError::Load)?
                        }
                    }
                }
            }
        };

        let ctx = HostCtx::new(
            tenant.to_owned(),
            func.to_owned(),
            inner.kv.clone(),
            limits.allowed_hosts.clone(),
            inner.cfg.allow_private_egress,
            inner.http_client.clone(),
            limits.mem_cap_bytes,
        );

        let ticks = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let mut meter = MeterGuard {
            sink: inner.meter.as_deref(),
            tenant,
            func,
            ticks: &ticks,
            started,
            done: false,
        };

        let run = sandbox::run(&pre, ctx, &input, limits.cpu_budget_ms, ticks.clone()).await;
        let usage = Usage::new(
            ticks.load(Ordering::Relaxed) * (sandbox::EPOCH_TICK_MS * 1000),
            started.elapsed().as_micros() as u64,
            run.mem_peak,
        );
        let limit = inner.cfg.max_output_bytes;
        let result = match run.result {
            Ok(output) if output.len() > limit => Err(InvokeError::OutputTooLarge { usage, limit }),
            Ok(output) => Ok(Invocation { output, usage }),
            Err(Failure::CpuBudget) => Err(InvokeError::CpuBudgetExceeded {
                usage,
                budget_ms: limits.cpu_budget_ms,
            }),
            Err(Failure::MemCap { cap_bytes }) => {
                Err(InvokeError::MemoryCapExceeded { usage, cap_bytes })
            }
            Err(Failure::WallClock) => Err(InvokeError::WallClockTimeout { usage }),
            Err(Failure::Trap(source)) => Err(InvokeError::GuestTrap { usage, source }),
        };
        meter.finish(usage, result.is_ok());
        result
    }

    /// Delete blobs no pointer references (and `.cwasm` files compiled by a
    /// different engine), skipping anything modified within `grace`. Returns
    /// the number of files removed. Errors out before deleting anything if a
    /// pointer file cannot be read. Run it at startup, with
    /// [`GC_GRACE_PERIOD`](crate::GC_GRACE_PERIOD) as `grace`.
    pub async fn gc(&self, grace: Duration) -> Result<usize, Error> {
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            registry::gc_unreferenced_blobs(&inner.cfg.modules_dir, &inner.engine, grace)
        })
        .await
        .map_err(|e| Error::Internal(format!("gc task failed: {e}")))?
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("modules_dir", &self.0.cfg.modules_dir)
            .finish_non_exhaustive()
    }
}

fn into_publish_err(e: Error) -> PublishError {
    match e {
        Error::Io(e) => PublishError::Io(e),
        other => PublishError::Registry(other),
    }
}

/// RAII in-flight counter for one tenant; released on every exit path.
struct TenantSlot {
    inner: Arc<Inner>,
    tenant: String,
}

impl TenantSlot {
    fn acquire(inner: &Arc<Inner>, tenant: &str) -> Option<Self> {
        let mut map = inner.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        match map.get_mut(tenant) {
            Some(n) if *n >= inner.cfg.max_in_flight_per_tenant => return None,
            Some(n) => *n += 1,
            None => {
                map.insert(tenant.to_owned(), 1);
            }
        }
        Some(Self {
            inner: inner.clone(),
            tenant: tenant.to_owned(),
        })
    }
}

impl Drop for TenantSlot {
    fn drop(&mut self) {
        let mut map = self
            .inner
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(n) = map.get_mut(&self.tenant) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.tenant);
            }
        }
    }
}

/// Reports to the meter exactly once: explicitly via [`finish`](Self::finish),
/// or from `Drop` (as a failure, with the ticks counted so far) if the
/// invocation future was cancelled mid-call, so cancelling cannot make CPU
/// use free.
struct MeterGuard<'a> {
    sink: Option<&'a dyn MeterSink>,
    tenant: &'a str,
    func: &'a str,
    ticks: &'a AtomicU64,
    started: Instant,
    done: bool,
}

impl MeterGuard<'_> {
    fn finish(&mut self, usage: Usage, ok: bool) {
        self.done = true;
        if let Some(sink) = self.sink {
            sink.record(self.tenant, self.func, usage, ok);
        }
    }
}

impl Drop for MeterGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(sink) = self.sink {
            let usage = Usage::new(
                self.ticks.load(Ordering::Relaxed) * (sandbox::EPOCH_TICK_MS * 1000),
                self.started.elapsed().as_micros() as u64,
                0,
            );
            sink.record(self.tenant, self.func, usage, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test_guest.wasm"
    ));

    /// The epoch thread lives exactly as long as the shared state: it keeps
    /// running while any clone (or in-flight invoke) exists and is stopped
    /// and joined once the last one is dropped.
    #[tokio::test]
    async fn ticker_stops_after_last_clone_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::new(RuntimeConfig::new(dir.path())).unwrap();
        let stopped = rt.0._ticker.stop_flag();
        let weak = Arc::downgrade(&rt.0);

        let clone = rt.clone();
        drop(rt);
        assert!(!stopped.load(Ordering::Relaxed), "a clone is still alive");
        assert!(weak.upgrade().is_some());

        // Also after real use: invoke, then drop the last handle.
        clone
            .publish("t", "f", Bytes::from_static(TEST_GUEST_WASM))
            .await
            .unwrap();
        clone
            .invoke("t", "f", b"x".to_vec(), &Limits::default())
            .await
            .unwrap();
        drop(clone);
        assert!(stopped.load(Ordering::Relaxed), "ticker must stop");
        assert!(weak.upgrade().is_none(), "no leaked Arc<Inner>");
    }

    /// A cold LRU miss whose `.cwasm` is on disk only deserialises: it must
    /// not queue for a compile permit, even while a stage holds the only one.
    #[tokio::test]
    async fn cold_load_from_cwasm_does_not_wait_for_compile_permit() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = RuntimeConfig::new(dir.path());
        cfg.compile_concurrency = 1;
        let first = Runtime::new(cfg.clone()).unwrap();
        first
            .publish("t", "f", Bytes::from_static(TEST_GUEST_WASM))
            .await
            .unwrap();
        drop(first);

        // Fresh runtime: empty LRU, cwasm on disk. Hold the only permit, as
        // a long-running stage would.
        let rt = Runtime::new(cfg).unwrap();
        let _held = rt.0.compile_slots.clone().acquire_owned().await.unwrap();
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            rt.invoke("t", "f", b"x".to_vec(), &Limits::default()),
        )
        .await
        .expect("cold load queued behind the compile permit")
        .unwrap();
        assert_eq!(out.output, b"x");
    }
}
