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
//! - CPU budget is enforced by epoch interruption, driven cooperatively
//!   rather than as a hard interrupt: each `Store` is configured with
//!   [`wasmtime::Store::epoch_deadline_callback`] returning
//!   `UpdateDeadline::Yield(1)` one tick at a time, so a long-running guest
//!   yields back to the tokio executor on every tick instead of blocking
//!   the worker thread — other tasks (other tenants' invocations, the
//!   ticker itself) keep making progress. Once the callback has been
//!   called `budget_ticks` times it returns a distinguishable
//!   [`CpuBudgetExceededMarker`] error instead of extending the deadline
//!   again, which [`classify_trap`] downcasts to `InvokeError::CpuBudgetExceeded`.
//!   A single background thread ([`EpochTicker`]) bumps the engine-wide
//!   epoch every [`EPOCH_TICK_MS`]; each store's deadline is set in ticks.
//!   This replaced a Phase-1 per-call `tokio::spawn` ticker that bumped the
//!   epoch for *every* concurrent store, not just the one whose budget had
//!   actually elapsed.
//! - A `tokio::time::timeout` wraps the whole call as a wall-clock backstop
//!   (budget + the http-out timeout + slack) so a guest wedged inside a
//!   slow host call can't hang the request indefinitely.

use std::net::IpAddr;
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
/// Max `kv::put` calls per invocation — exceeding it traps the guest. Caps
/// unbounded host-memory growth from a guest that puts in a tight loop.
pub const MAX_KV_PUTS_PER_INVOCATION: usize = 1000;
/// Max total `kv::put` value bytes per invocation — exceeding it traps the
/// guest, independent of the per-call [`MAX_KV_VALUE_BYTES`] cap.
pub const MAX_KV_PUT_BYTES_PER_INVOCATION: usize = 8 * 1024 * 1024;
/// `log::emit` messages are truncated (not trapped) at this many bytes.
const MAX_LOG_MSG_BYTES: usize = 4 * 1024;
/// Max `log::emit` lines per invocation. Past this, lines are dropped
/// silently (after one "suppressed" notice) rather than trapping the guest
/// — logging is diagnostic, not something a guest should be killed over.
pub const MAX_LOG_LINES_PER_INVOCATION: usize = 100;
/// Max total `log::emit` message bytes per invocation, mirroring
/// [`MAX_LOG_LINES_PER_INVOCATION`].
pub const MAX_LOG_BYTES_PER_INVOCATION: usize = 64 * 1024;
/// `http-out::fetch` response bodies are capped at this many bytes; the
/// host stops reading and returns `Err` to the guest once exceeded.
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
/// Per-request timeout for outbound HTTP, applied on the shared
/// `reqwest::Client` the caller builds and hands to every [`HostCtx`].
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Returns true iff `ip` is not part of the public, routable Internet:
/// loopback, RFC 1918 private, link-local (169.254/16, fe80::/10),
/// unspecified, broadcast, CGNAT (100.64.0.0/10), unique-local (fc00::/7),
/// multicast, or an IPv4-mapped IPv6 address whose embedded v4 address is
/// itself one of those.
///
/// Used to block SSRF via `http-out::fetch` reaching the host's own
/// network — both for DNS answers (see `GuardedResolver`) and for URL IP
/// literals, which reqwest hands straight to the connector without ever
/// calling the configured resolver.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ipv4(mapped);
            }
            let seg = v6.segments();
            let low32 = std::net::Ipv4Addr::new(
                (seg[6] >> 8) as u8,
                seg[6] as u8,
                (seg[7] >> 8) as u8,
                seg[7] as u8,
            );
            // Prefixes that embed an IPv4 address: judge by the embedded one.
            // `::a.b.c.d` (IPv4-compatible) and `64:ff9b::/96` (NAT64).
            if seg[..6] == [0; 6] && !v6.is_loopback() && !v6.is_unspecified()
                || seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0]
            {
                return is_blocked_ipv4(low32);
            }
            // 6to4 `2002::/16` carries the IPv4 address in segments 1..3.
            if seg[0] == 0x2002 {
                let v4 = std::net::Ipv4Addr::new(
                    (seg[1] >> 8) as u8,
                    seg[1] as u8,
                    (seg[2] >> 8) as u8,
                    seg[2] as u8,
                );
                return is_blocked_ipv4(v4);
            }
            let seg0 = seg[0];
            (seg0 == 0x64 && seg[1] == 0xff9b && seg[2] == 1) // 64:ff9b:1::/48 local NAT64
                || v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

fn is_blocked_ipv4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
        || v4.octets()[0] == 0 // 0.0.0.0/8 "this network"
        || v4.octets()[..3] == [192, 0, 0] // 192.0.0.0/24 IETF protocol assignments
        || (v4.octets()[0] == 198 && (v4.octets()[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || v4.octets()[0] >= 240 // 240.0.0.0/4 reserved (incl. broadcast)
}

/// `reqwest::dns::Resolve` impl that resolves via `tokio::net::lookup_host`
/// and, unless `allow_private` is set, filters every blocked address
/// ([`is_blocked_ip`]) out of the answer — erroring if nothing public is
/// left. Installed on the shared client by [`build_http_client`].
///
/// This only covers hostnames: reqwest never calls the configured resolver
/// for a URL whose host is already an IP literal, so [`http_fetch`]
/// separately checks that case before it ever opens a connection.
struct GuardedResolver {
    allow_private: bool,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if allow_private {
                return Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs);
            }
            let public: Vec<std::net::SocketAddr> = addrs
                .into_iter()
                .filter(|a| !is_blocked_ip(a.ip()))
                .collect();
            if public.is_empty() {
                return Err(Box::from(format!(
                    "{host}: dns resolution returned only blocked/private addresses"
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Build the single shared `reqwest::Client` every [`HostCtx`] should be
/// constructed with for `http-out::fetch`.
///
/// - `redirect::Policy::none()` — redirects could otherwise walk a request
///   from an allowlisted host to a non-allowlisted one; the allowlist check
///   in [`http_fetch`] only ever sees the first hop.
/// - a blanket 5 s timeout so a slow upstream can't pin a tenant's request
///   open indefinitely.
/// - a [`GuardedResolver`] that, when `allow_private` is `false`, refuses
///   to hand back loopback/private/link-local/etc. addresses — see
///   [`is_blocked_ip`]. Callers that need to reach `127.0.0.1` (tests
///   standing up local servers) pass `true`.
pub fn build_http_client(allow_private: bool) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .dns_resolver(Arc::new(GuardedResolver { allow_private }))
        // A proxy resolves the target itself, bypassing `GuardedResolver`.
        .no_proxy()
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
/// store just sets its own deadline in ticks and only *that* store's
/// callback (see `invoke`) fires when the ticker carries the epoch past it.
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
        Ok(self.kv.get(&self.tenant_id, &key).await)
    }

    async fn put(&mut self, key: String, value: Vec<u8>) -> wasmtime::Result<()> {
        if key.len() > MAX_KV_KEY_BYTES {
            wasmtime::bail!("kv key exceeds {MAX_KV_KEY_BYTES} byte cap");
        }
        if value.len() > MAX_KV_VALUE_BYTES {
            wasmtime::bail!("kv value exceeds {MAX_KV_VALUE_BYTES} byte cap");
        }
        self.kv_put_count += 1;
        if self.kv_put_count > MAX_KV_PUTS_PER_INVOCATION {
            wasmtime::bail!("kv put count exceeds {MAX_KV_PUTS_PER_INVOCATION} per invocation");
        }
        // Keys count too: empty values under unique keys still cost memory.
        self.kv_put_bytes += key.len() + value.len();
        if self.kv_put_bytes > MAX_KV_PUT_BYTES_PER_INVOCATION {
            wasmtime::bail!(
                "kv put bytes exceeds {MAX_KV_PUT_BYTES_PER_INVOCATION} per invocation"
            );
        }
        self.kv.put(&self.tenant_id, &key, value).await?;
        Ok(())
    }
}

impl warpline::host::log::Host for HostCtx {
    async fn emit(&mut self, level: String, msg: String) -> wasmtime::Result<()> {
        if self.log_line_count >= MAX_LOG_LINES_PER_INVOCATION
            || self.log_bytes >= MAX_LOG_BYTES_PER_INVOCATION
        {
            if !self.log_suppressed_notified {
                self.log_suppressed_notified = true;
                tracing::warn!(
                    tenant = self.tenant_id.as_str(),
                    func = self.fn_name.as_str(),
                    "log output suppressed: per-invocation limit exceeded"
                );
            }
            return Ok(());
        }

        let msg = truncate_utf8(&msg, MAX_LOG_MSG_BYTES);
        self.log_line_count += 1;
        self.log_bytes += msg.len();

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
        // Clone the things `http_fetch` needs out of `self` up front:
        // `WasiCtx` holds `Box<dyn ... + Send>` trait objects that are not
        // `Sync`, so holding a `&HostCtx` across an `.await` would make
        // this whole async fn's future non-`Send` — which `add_to_linker_async`
        // requires. Owned clones (small `Vec`s, bools, and an Arc-backed
        // Client) sidestep that entirely.
        let allowed_hosts = self.allowed_hosts.clone();
        let allow_private = self.allow_private_egress;
        let client = self.http_client.clone();
        Ok(http_fetch(allowed_hosts, allow_private, client, req).await)
    }
}

/// The actual `http-out::fetch` implementation, split out of the trait impl
/// so its guest-visible error path (`Result<Response, String>`) stays
/// separate from the trap path (reserved for host bugs, not guest input).
async fn http_fetch(
    allowed_hosts: Vec<String>,
    allow_private: bool,
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

    // reqwest never consults the configured DNS resolver when the URL's
    // host is already an IP literal — it hands that straight to the
    // connector — so the private/blocked-range check has to happen here
    // too, not just inside `GuardedResolver`.
    if !allow_private {
        let literal_ip = match url.host() {
            Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
            Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
            _ => None,
        };
        if let Some(ip) = literal_ip {
            if is_blocked_ip(ip) {
                return Err(format!("host {ip} is a blocked/private address"));
            }
        }
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
/// codes (see `warpline-host`'s `status_for` — budget/wall-timeout -> 408,
/// memory cap -> 507, guest trap/instantiate failure -> 500).
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

/// Marker error returned by the `epoch_deadline_callback` installed in
/// [`invoke`] once a store's tick budget is exhausted. [`classify_trap`]
/// downcasts to this to recognise "the CPU budget ran out" independent of
/// wasmtime's own `Trap::Interrupt` (which the callback-based, yielding
/// deadline never actually raises, but which is kept as a fallback below).
#[derive(Debug)]
struct CpuBudgetExceededMarker;

impl std::fmt::Display for CpuBudgetExceededMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cpu budget exceeded")
    }
}

impl std::error::Error for CpuBudgetExceededMarker {}

/// Classify a failed `call_handle` into an [`InvokeError`], using the
/// [`crate::types::TenantLimiter`] state left behind in `store` and the
/// error's downcast to [`CpuBudgetExceededMarker`] / [`wasmtime::Trap`].
///
/// CPU-budget exhaustion is checked *before* the memory-cap flag: a store
/// whose `memory.grow` was rejected doesn't trap on the spot (wasm just
/// sees `-1` from the failed grow and keeps running), so a guest that
/// retries allocation in a loop can hit the CPU budget afterwards with
/// `cap_hit` still set from the earlier rejection. Checking the interrupt
/// first reports the trap that actually ended the call.
fn classify_trap(err: wasmtime::Error, store: &Store<HostCtx>, budget_ms: u64) -> InvokeError {
    let is_cpu_budget = err.downcast_ref::<CpuBudgetExceededMarker>().is_some()
        || matches!(
            err.downcast_ref::<wasmtime::Trap>(),
            Some(t) if *t == wasmtime::Trap::Interrupt
        );
    if is_cpu_budget {
        return InvokeError::CpuBudgetExceeded { budget_ms };
    }
    let limiter = &store.data().limiter;
    if limiter.cap_hit {
        return InvokeError::MemoryCapExceeded {
            peak_bytes: limiter.peak_bytes,
            cap_bytes: limiter.mem_cap_bytes,
        };
    }
    InvokeError::GuestTrap(err.to_string())
}

/// Invoke `component`'s exported `handle(input: list<u8>) -> list<u8>`
/// inside a fresh [`Store`] carrying `ctx`.
///
/// `cpu_budget_ms` is enforced via a cooperative-yield epoch deadline
/// callback (see module docs); the whole call is additionally bounded by a
/// wall-clock `tokio::time::timeout` of `cpu_budget_ms` plus [`HTTP_TIMEOUT`]
/// plus a second of slack, so a guest stuck making slow host calls can't hang
/// the caller even if epoch ticks can't reach it (e.g. blocked inside a
/// host import awaiting I/O).
pub async fn invoke(
    engine: &Engine,
    linker: &Linker<HostCtx>,
    component: &Component,
    ctx: HostCtx,
    input: Vec<u8>,
    cpu_budget_ms: u64,
) -> Result<InvokeOutcome, InvokeError> {
    let wall_budget = Duration::from_millis(cpu_budget_ms) + HTTP_TIMEOUT + Duration::from_secs(1);
    let budget_ticks = (cpu_budget_ms / EPOCH_TICK_MS).max(1);

    let call = async move {
        let mut store = Store::new(engine, ctx);
        store.limiter(|c| &mut c.limiter as &mut dyn ResourceLimiter);

        // One tick at a time: on every epoch tick that reaches the
        // deadline, yield to the tokio executor and extend the deadline by
        // one more tick, until `budget_ticks` ticks have elapsed — at
        // which point return a distinguishable error instead. This is what
        // lets a long-running guest cooperate with other work on the same
        // executor instead of pinning the worker thread until it traps.
        let mut ticks_elapsed: u64 = 0;
        store.epoch_deadline_callback(move |_store| {
            ticks_elapsed += 1;
            if ticks_elapsed >= budget_ticks {
                Err(wasmtime::Error::new(CpuBudgetExceededMarker))
            } else {
                Ok(wasmtime::UpdateDeadline::Yield(1))
            }
        });
        store.set_epoch_deadline(1);

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

#[cfg(test)]
mod tests {
    use super::is_blocked_ip;
    use std::net::IpAddr;

    #[test]
    fn is_blocked_ip_classification_table() {
        let cases: &[(&str, bool)] = &[
            // loopback
            ("127.0.0.1", true),
            ("127.255.255.255", true),
            ("::1", true),
            // RFC 1918 private
            ("10.0.0.1", true),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("192.168.1.1", true),
            // link-local
            ("169.254.1.1", true),
            ("fe80::1", true),
            // unspecified
            ("0.0.0.0", true),
            ("::", true),
            // broadcast
            ("255.255.255.255", true),
            // CGNAT 100.64.0.0/10
            ("100.64.0.1", true),
            ("100.127.255.255", true),
            ("100.63.255.255", false),
            ("100.128.0.0", false),
            // unique-local fc00::/7
            ("fc00::1", true),
            ("fd12:3456::1", true),
            // multicast
            ("224.0.0.1", true),
            ("ff02::1", true),
            // IPv4-mapped IPv6 of a blocked address
            ("::ffff:127.0.0.1", true),
            ("::ffff:10.0.0.1", true),
            // embedded-IPv4 forms: IPv4-compatible, NAT64, 6to4
            ("::10.0.0.1", true),
            ("::8.8.8.8", false),
            ("64:ff9b::a00:1", true),
            ("64:ff9b::808:808", false),
            ("64:ff9b:1::1", true),
            ("2002:a00:1::", true),
            ("2002:808:808::", false),
            // other reserved IPv4
            ("0.1.2.3", true),
            ("192.0.0.8", true),
            ("198.18.0.1", true),
            ("198.20.0.1", false),
            ("240.0.0.1", true),
            // public addresses
            ("8.8.8.8", false),
            ("1.1.1.1", false),
            ("93.184.216.34", false),
            ("2001:4860:4860::8888", false),
            ("::ffff:8.8.8.8", false),
            // outside private ranges but adjacent
            ("172.15.255.255", false),
            ("172.32.0.0", false),
        ];
        for (ip, expected) in cases {
            let parsed: IpAddr = ip.parse().expect("valid ip literal in test table");
            assert_eq!(
                is_blocked_ip(parsed),
                *expected,
                "is_blocked_ip({ip}) expected {expected}"
            );
        }
    }
}
