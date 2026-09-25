//! WASM runtime — Component-Model engine construction, host-import
//! registration (`wasmtime::component::bindgen!` against
//! `wit/warpline.wit`), the epoch-based CPU ticker, and the `invoke` entry
//! point used by `warpline-host`.
//!
//! ## wasmtime 49 shape
//!
//! - `bindgen!` below generates the `Handler` world (imports `kv`, `log`,
//!   `http-out`; exports `handle`) as **async** on both sides. Imports are
//!   additionally `trappable`: a host fn returning `Err` inside its outer
//!   `wasmtime::Result` traps the guest (used for capability-cap
//!   violations); the WIT-level `result<_, string>` on `http-out::fetch`
//!   stays a normal guest-visible error.
//! - [`HostCtx`] (in `types.rs`) implements the generated `Host` traits plus
//!   `wasmtime_wasi::WasiView`, so one `Linker` serves both the custom
//!   capability surface and the WASI p2 interfaces a `wasm32-wasip2` guest
//!   implicitly imports through its std lib.
//! - CPU budget is enforced by epoch interruption. A single background
//!   thread ([`EpochTicker`]) bumps the engine-wide epoch every
//!   [`EPOCH_TICK_MS`]; each store's deadline is set in ticks. This
//!   replaced a Phase-1 per-call `tokio::spawn` ticker that bumped the
//!   epoch for *every* concurrent store, not just the one whose budget had
//!   actually elapsed.
//! - A `tokio::time::timeout` wraps the whole call as a wall-clock backstop
//!   (budget + the http-out timeout + slack) so a guest wedged inside a
//!   slow host call can't hang the request indefinitely.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store};

use crate::types::HostCtx;

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "handler",
    imports: { default: async | trappable },
    exports: { default: async },
});

/// Key length cap for `kv::put`/`kv::get` — exceeding it traps the guest.
const MAX_KV_KEY_BYTES: usize = 512;
/// Value length cap for `kv::put` — exceeding it traps the guest (no
/// silent truncation).
const MAX_KV_VALUE_BYTES: usize = 1024 * 1024;
/// `log::emit` messages are truncated (not trapped) at this many bytes.
const MAX_LOG_MSG_BYTES: usize = 4 * 1024;
/// `http-out::fetch` response bodies are capped at this many bytes; the
/// host stops reading and returns `Err` to the guest once exceeded.
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
/// Per-request timeout for outbound HTTP, applied on the shared
/// `reqwest::Client` the caller builds and hands to every [`HostCtx`].
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the single shared `reqwest::Client` every [`HostCtx`] should be
/// constructed with for `http-out::fetch`.
///
/// - `redirect::Policy::none()` — redirects could otherwise walk a request
///   from an allowlisted host to a non-allowlisted one; the allowlist check
///   in [`http_fetch`] only ever sees the first hop.
/// - a blanket 5 s timeout so a slow upstream can't pin a tenant's request
///   open indefinitely.
pub fn build_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .build()
}

/// Build the shared wasmtime [`Engine`] used by every invocation.
///
/// - `epoch_interruption(true)` for cheap CPU-budget enforcement — see
///   [`EpochTicker`] and README "Design tradeoffs" §
///   "epoch_interruption over fuel metering".
/// - `consume_fuel(false)` explicitly off — we picked epochs.
/// - `cranelift_opt_level(Speed)` and `parallel_compilation(true)` because
///   the control plane compiles modules out-of-band on upload.
///
/// Async component instantiation/calls are available unconditionally once
/// the `async` cargo feature is enabled (wasmtime 49 dropped the
/// `Config::async_support` toggle — it's a no-op kept only for source
/// compat).
pub fn build_engine() -> anyhow::Result<Engine> {
    let mut cfg = Config::new();
    cfg.epoch_interruption(true)
        .consume_fuel(false)
        .cranelift_opt_level(wasmtime::OptLevel::Speed)
        .parallel_compilation(true);
    Engine::new(&cfg).map_err(Into::into)
}

/// How often [`EpochTicker`] bumps the engine epoch, in milliseconds. Also
/// the unit `invoke` converts `cpu_budget_ms` into epoch ticks with.
pub const EPOCH_TICK_MS: u64 = 1;

/// Background epoch pump: one `std::thread` per [`Engine`], incrementing
/// its epoch counter every [`EPOCH_TICK_MS`] until dropped.
///
/// Phase 1 spawned a `tokio::spawn` timer *per invocation* that bumped the
/// engine epoch once after that call's budget elapsed — since the epoch is
/// engine-global, that interrupted every other concurrently running store
/// too. One ticker per engine, ticking on a fixed cadence, fixes that: each
/// store just sets its own deadline in ticks and only *that* store traps
/// when the ticker carries the epoch past it.
pub struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    /// Spawn the ticker thread for `engine`.
    pub fn spawn(engine: Engine) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("warpline-epoch-ticker".into())
            .spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                    engine.increment_epoch();
                }
            })
            .expect("spawn epoch ticker thread");
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Build the [`Linker`] once per process (or per test) and reuse it across
/// every `invoke` call — registering host imports is not free and none of
/// them close over per-invocation state (that lives in [`HostCtx`], read
/// out of the `Store` at call time).
pub fn build_linker(engine: &Engine) -> anyhow::Result<Linker<HostCtx>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    Handler::add_to_linker::<HostCtx, HasSelf<HostCtx>>(&mut linker, |ctx| ctx)?;
    Ok(linker)
}

/// Scope a guest-supplied key under the calling tenant: `t/{tenant_id}/{key}`.
fn scoped_key(tenant_id: &str, key: &str) -> String {
    format!("t/{tenant_id}/{key}")
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8
/// character.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

impl warpline::host::kv::Host for HostCtx {
    async fn get(&mut self, key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        if key.len() > MAX_KV_KEY_BYTES {
            wasmtime::bail!("kv key exceeds {MAX_KV_KEY_BYTES} byte cap");
        }
        let scoped = scoped_key(&self.tenant_id, &key);
        Ok(self.kv.get(&scoped).await)
    }

    async fn put(&mut self, key: String, value: Vec<u8>) -> wasmtime::Result<()> {
        if key.len() > MAX_KV_KEY_BYTES {
            wasmtime::bail!("kv key exceeds {MAX_KV_KEY_BYTES} byte cap");
        }
        if value.len() > MAX_KV_VALUE_BYTES {
            wasmtime::bail!("kv value exceeds {MAX_KV_VALUE_BYTES} byte cap");
        }
        let scoped = scoped_key(&self.tenant_id, &key);
        self.kv.put(&scoped, value).await;
        Ok(())
    }
}

impl warpline::host::log::Host for HostCtx {
    async fn emit(&mut self, level: String, msg: String) -> wasmtime::Result<()> {
        let msg = truncate_utf8(&msg, MAX_LOG_MSG_BYTES);
        let tenant = self.tenant_id.as_str();
        let func = self.fn_name.as_str();
        match level.to_ascii_lowercase().as_str() {
            "trace" => tracing::trace!(tenant, func, msg, "guest log"),
            "debug" => tracing::debug!(tenant, func, msg, "guest log"),
            "warn" | "warning" => tracing::warn!(tenant, func, msg, "guest log"),
            "error" => tracing::error!(tenant, func, msg, "guest log"),
            // Unknown levels (and "info") map to info — never drop a guest
            // log line just because it used a level we don't recognise.
            _ => tracing::info!(tenant, func, msg, "guest log"),
        }
        Ok(())
    }
}

impl warpline::host::http_out::Host for HostCtx {
    async fn fetch(
        &mut self,
        req: warpline::host::http_out::Request,
    ) -> wasmtime::Result<Result<warpline::host::http_out::Response, String>> {
        // Clone the two things `http_fetch` needs out of `self` up front:
        // `WasiCtx` holds `Box<dyn ... + Send>` trait objects that are not
        // `Sync`, so holding a `&HostCtx` across an `.await` would make
        // this whole async fn's future non-`Send` — which `add_to_linker_async`
        // requires. Owned clones (a small Vec<String> and an Arc-backed
        // Client) sidestep that entirely.
        let allowed_hosts = self.allowed_hosts.clone();
        let client = self.http_client.clone();
        Ok(http_fetch(allowed_hosts, client, req).await)
    }
}

/// The actual `http-out::fetch` implementation, split out of the trait impl
/// so its guest-visible error path (`Result<Response, String>`) stays
/// separate from the trap path (reserved for host bugs, not guest input).
async fn http_fetch(
    allowed_hosts: Vec<String>,
    client: reqwest::Client,
    req: warpline::host::http_out::Request,
) -> Result<warpline::host::http_out::Response, String> {
    let url = url::Url::parse(&req.url).map_err(|e| format!("invalid url: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("scheme {other} not allowed")),
    }
    // `url` already lowercases the host; also strip a trailing root-label
    // dot ("allowed.com." is the same host as "allowed.com") so neither an
    // allowlist entry nor a guest-supplied URL can dodge the comparison by
    // way of it.
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    let allowed = allowed_hosts
        .iter()
        .any(|allowed| allowed.trim_end_matches('.') == host);
    if !allowed {
        return Err(format!("host {host} not allowed"));
    }
    let method = reqwest::Method::from_bytes(req.method.as_bytes())
        .map_err(|e| format!("invalid method: {e}"))?;

    let mut resp = client
        .request(method, url)
        .body(req.body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("body read failed: {e}"))?
    {
        if body.len() + chunk.len() > MAX_HTTP_BODY_BYTES {
            return Err(format!(
                "response body exceeds {MAX_HTTP_BODY_BYTES} byte cap"
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(warpline::host::http_out::Response { status, body })
}

/// Successful outcome of [`invoke`].
#[derive(Debug)]
pub struct InvokeOutcome {
    pub output: Vec<u8>,
    /// Wall-clock duration of the guest's `handle` call, in microseconds.
    /// Not real CPU time — wasmtime has no cheap per-store CPU-time counter
    /// — but for the short, mostly-CPU-bound handlers this host targets,
    /// wall time during the call is a reasonable proxy and is documented as
    /// such wherever it's surfaced (metering rows, API responses).
    pub cpu_us: u64,
    pub mem_peak_bytes: usize,
}

/// Failure outcome of [`invoke`]. The host crate maps these to HTTP status
/// codes (e.g. budget/memory/timeout -> 4xx "your function did this to
/// itself", other trap/instantiate errors -> 5xx).
#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error("cpu budget exceeded ({budget_ms} ms)")]
    CpuBudgetExceeded { budget_ms: u64 },
    #[error("memory cap exceeded (peak {peak_bytes} bytes, cap {cap_bytes} bytes)")]
    MemoryCapExceeded { peak_bytes: usize, cap_bytes: usize },
    #[error("wall-clock timeout after {0:?}")]
    WallClockTimeout(Duration),
    #[error("guest trapped: {0}")]
    GuestTrap(String),
    #[error("failed to instantiate component: {0}")]
    Instantiate(String),
}

/// Classify a failed `call_handle` into an [`InvokeError`], using the
/// [`crate::types::TenantLimiter`] state left behind in `store` and the
/// error's downcast to [`wasmtime::Trap`].
fn classify_trap(err: wasmtime::Error, store: &Store<HostCtx>, budget_ms: u64) -> InvokeError {
    let limiter = &store.data().limiter;
    if limiter.cap_hit {
        return InvokeError::MemoryCapExceeded {
            peak_bytes: limiter.peak_bytes,
            cap_bytes: limiter.mem_cap_bytes,
        };
    }
    if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
        if *trap == wasmtime::Trap::Interrupt {
            return InvokeError::CpuBudgetExceeded { budget_ms };
        }
    }
    InvokeError::GuestTrap(err.to_string())
}

/// Invoke `component`'s exported `handle(input: list<u8>) -> list<u8>`
/// inside a fresh [`Store`] carrying `ctx`.
///
/// `cpu_budget_ms` is enforced via epoch interruption (see module docs);
/// the whole call is additionally bounded by a wall-clock
/// `tokio::time::timeout` of `cpu_budget_ms` + [`HTTP_TIMEOUT`] + a second
/// of slack, so a guest stuck making slow host calls can't hang the
/// caller even if epoch ticks can't reach it (e.g. blocked inside a host
/// import awaiting I/O).
pub async fn invoke(
    engine: &Engine,
    linker: &Linker<HostCtx>,
    component: &Component,
    ctx: HostCtx,
    input: Vec<u8>,
    cpu_budget_ms: u64,
) -> Result<InvokeOutcome, InvokeError> {
    let wall_budget = Duration::from_millis(cpu_budget_ms) + HTTP_TIMEOUT + Duration::from_secs(1);

    let call = async move {
        let mut store = Store::new(engine, ctx);
        store.limiter(|c| &mut c.limiter as &mut dyn ResourceLimiter);
        store.set_epoch_deadline((cpu_budget_ms / EPOCH_TICK_MS).max(1));

        let bindings = match Handler::instantiate_async(&mut store, component, linker).await {
            Ok(bindings) => bindings,
            Err(e) => return Err(InvokeError::Instantiate(e.to_string())),
        };

        let started = Instant::now();
        let result = bindings.call_handle(&mut store, &input).await;
        let cpu_us = started.elapsed().as_micros() as u64;

        match result {
            Ok(output) => Ok(InvokeOutcome {
                output,
                cpu_us,
                mem_peak_bytes: store.data().limiter.peak_bytes,
            }),
            Err(e) => Err(classify_trap(e, &store, cpu_budget_ms)),
        }
    };

    match tokio::time::timeout(wall_budget, call).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => Err(InvokeError::WallClockTimeout(wall_budget)),
    }
}
