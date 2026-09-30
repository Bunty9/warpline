//! The sandbox: `wasmtime::component::bindgen!` host bindings for
//! `crates/core/wit/warpline.wit`, the host-import implementations, the
//! outbound-HTTP client and SSRF guard, the epoch ticker and the single-call
//! [`run`] machinery. Everything here is crate-private; embedders go through
//! [`crate::Runtime`].
//!
//! ## wasmtime 49 shape
//!
//! - `bindgen!` below generates the `Handler` world (imports `kv`, `log`,
//!   `http-out`; exports `handle`) as **async** on both sides. Imports are
//!   additionally `trappable`: a host fn returning `Err` inside its outer
//!   `wasmtime::Result` traps the guest (used for capability-cap
//!   violations and KV backend errors); the WIT-level `result<_, string>` on
//!   `http-out::fetch` stays a normal guest-visible error.
//! - [`HostCtx`] implements the generated `Host` traits plus
//!   `wasmtime_wasi::WasiView`, so one `Linker` serves both the custom
//!   capability surface and the WASI p2 interfaces a `wasm32-wasip2` guest
//!   implicitly imports through its std lib.
//! - CPU budget is enforced by epoch interruption, driven cooperatively:
//!   each `Store` is configured with an epoch deadline callback returning
//!   `UpdateDeadline::Yield(1)` one tick at a time, so a long-running guest
//!   yields back to the tokio executor on every tick. Every callback
//!   invocation is one tick of guest run time and is counted; the metered
//!   `cpu_us` is that count times 1000. The callback allows `budget` ticks
//!   plus one before raising `CpuBudgetExceededMarker`, so the effective
//!   budget is `(budget, budget + 1]` ms and a 1 ms budget never fails an
//!   echo because the first tick landed early. Instantiation (mostly host
//!   linking and memory setup, about 0.1-0.5 ms) is tick-counted too but is
//!   allowed [`INSTANTIATE_GRACE_TICKS`] on top, not charged to the
//!   budget of the call that follows; otherwise the smallest budgets could
//!   not fit even an echo.
//! - A single background thread ([`EpochTicker`]) bumps the engine-wide
//!   epoch every [`EPOCH_TICK_MS`].
//! - A `tokio::time::timeout` wraps the whole call as a wall-clock backstop
//!   (budget + the http-out timeout + slack) so a guest wedged inside a
//!   slow host call can't hang the request indefinitely.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store};

use crate::types::HostCtx;

wasmtime::component::bindgen!({
    path: "wit",
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
pub(crate) const MAX_KV_PUTS_PER_INVOCATION: usize = 1000;
/// Max total `kv::put` value bytes per invocation — exceeding it traps the
/// guest, independent of the per-call `MAX_KV_VALUE_BYTES` cap.
pub(crate) const MAX_KV_PUT_BYTES_PER_INVOCATION: usize = 8 * 1024 * 1024;
/// `log::emit` messages are truncated (not trapped) at this many bytes.
const MAX_LOG_MSG_BYTES: usize = 4 * 1024;
/// Max `log::emit` lines per invocation. Past this, lines are dropped
/// silently (after one "suppressed" notice) rather than trapping the guest
/// — logging is diagnostic, not something a guest should be killed over.
pub(crate) const MAX_LOG_LINES_PER_INVOCATION: usize = 100;
/// Max total `log::emit` message bytes per invocation, mirroring
/// [`MAX_LOG_LINES_PER_INVOCATION`].
pub(crate) const MAX_LOG_BYTES_PER_INVOCATION: usize = 64 * 1024;
/// `http-out::fetch` response bodies are capped at this many bytes; the
/// host stops reading and returns `Err` to the guest once exceeded.
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
/// Per-request timeout for outbound HTTP, applied on the shared
/// `reqwest::Client` the caller builds and hands to every [`HostCtx`].
pub(crate) const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

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
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
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
///   in `http_fetch` only ever sees the first hop.
/// - a blanket 5 s timeout so a slow upstream can't pin a tenant's request
///   open indefinitely.
/// - a `GuardedResolver` that, when `allow_private` is `false`, refuses
///   to hand back loopback/private/link-local/etc. addresses — see
///   [`is_blocked_ip`]. Callers that need to reach `127.0.0.1` (tests
///   standing up local servers) pass `true`.
pub(crate) fn build_http_client(allow_private: bool) -> reqwest::Result<reqwest::Client> {
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
pub(crate) fn build_engine() -> wasmtime::Result<Engine> {
    let mut cfg = Config::new();
    cfg.epoch_interruption(true)
        .consume_fuel(false)
        .cranelift_opt_level(wasmtime::OptLevel::Speed)
        .parallel_compilation(true);
    Engine::new(&cfg)
}

/// How often [`EpochTicker`] bumps the engine epoch, in milliseconds. Also
/// the unit `invoke` converts `cpu_budget_ms` into epoch ticks with.
pub(crate) const EPOCH_TICK_MS: u64 = 1;

/// Background epoch pump: one `std::thread` per [`Engine`], incrementing
/// its epoch counter every [`EPOCH_TICK_MS`] until dropped.
///
/// Phase 1 spawned a `tokio::spawn` timer *per invocation* that bumped the
/// engine epoch once after that call's budget elapsed — since the epoch is
/// engine-global, that interrupted every other concurrently running store
/// too. One ticker per engine, ticking on a fixed cadence, fixes that: each
/// store just sets its own deadline in ticks and only *that* store's
/// callback (see `invoke`) fires when the ticker carries the epoch past it.
pub(crate) struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    /// Spawn the ticker thread for `engine`.
    pub fn spawn(engine: Engine) -> Result<Self, crate::Error> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("warpline-epoch-ticker".into())
            .spawn(move || {
                // Sleep to an absolute schedule rather than `sleep(tick)`
                // in a loop: each relative sleep overshoots a little, and
                // since budgets are counted in ticks that drift compounds
                // (~+12% at a 100 ms budget before this change).
                let tick = Duration::from_millis(EPOCH_TICK_MS);
                let mut next = std::time::Instant::now();
                while !stop_thread.load(Ordering::Relaxed) {
                    next += tick;
                    let now = std::time::Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else if now - next > tick * 10 {
                        // Badly behind (host suspended, starved thread):
                        // resync instead of bursting ticks.
                        next = now;
                    }
                    engine.increment_epoch();
                }
            })
            .map_err(|e| crate::Error::Internal(format!("spawn epoch ticker thread: {e}")))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

#[cfg(test)]
impl EpochTicker {
    /// The stop flag, set (before the thread is joined) when dropped.
    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
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
pub(crate) fn build_linker(engine: &Engine) -> wasmtime::Result<Linker<HostCtx>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    Handler::add_to_linker::<HostCtx, HasSelf<HostCtx>>(&mut linker, |ctx| ctx)?;
    Ok(linker)
}

/// Build the pre-instantiated [`HandlerPre`] for `component` against
/// `linker` — every import check and the `handle`-export check that
/// `instantiate_async` would otherwise redo on *every* invoke, done once
/// here instead. [`registry::ComponentCache`](crate::registry::ComponentCache) calls this once per loaded
/// component and caches the result; [`invoke`] takes the cached
/// `HandlerPre` rather than a bare `Component` + `Linker` pair so a warm
/// invoke's `instantiate_async` skips straight to instance creation.
///
/// `linker.instantiate_pre` alone already checks every import; wrapping the
/// result in [`HandlerPre::new`] additionally checks the `handle` export,
/// which `instantiate_pre` doesn't look at.
pub(crate) fn instantiate_pre(
    linker: &Linker<HostCtx>,
    component: &Component,
) -> wasmtime::Result<HandlerPre<HostCtx>> {
    let instance_pre = linker.instantiate_pre(component)?;
    HandlerPre::new(instance_pre)
}

/// Type-check `component` against `linker` — every import the component
/// declares must be satisfiable by what `linker` provides (WASI p2 plus the
/// `warpline:host` capability surface) and it must export `handle` with the
/// right signature. Used by `warpline-control` at upload time so a
/// component that imports something we don't provide is rejected with a
/// 422 there, rather than failing to instantiate on its first invoke.
pub(crate) fn typecheck_component(
    linker: &Linker<HostCtx>,
    component: &Component,
) -> wasmtime::Result<()> {
    instantiate_pre(linker, component)?;
    Ok(())
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
        // A backend failure traps the guest: it is an operational fault, not
        // something the WIT contract models as guest-recoverable.
        Ok(self.kv.get(&self.tenant_id, &key).await?)
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

/// Extra ticks instantiation may use before the call's own budget starts. A
/// guest start function that loops is still cut off after this plus the
/// budget.
const INSTANTIATE_GRACE_TICKS: u64 = 2;

/// The callback traps on the tick *after* the limit (`n > limit`), so a limit
/// of `budget` ticks means the guest is cut off in `(budget, budget + 1]` ms.
/// Instantiation is limited to `budget + grace`. Afterwards the call's limit
/// forgives at most the grace that instantiation used, so the whole
/// invocation is cut off within `budget + grace + 1` ticks.
fn call_limit(ticks_used_by_instantiation: u64, budget_ticks: u64) -> u64 {
    ticks_used_by_instantiation
        .min(INSTANTIATE_GRACE_TICKS)
        .saturating_add(budget_ticks)
}

/// Marker error returned by the epoch deadline callback once a store's tick
/// budget is exhausted. [`classify`] downcasts to this to recognise "the CPU
/// budget ran out" independent of wasmtime's own `Trap::Interrupt`.
#[derive(Debug)]
struct CpuBudgetExceededMarker;

impl std::fmt::Display for CpuBudgetExceededMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cpu budget exceeded")
    }
}

impl std::error::Error for CpuBudgetExceededMarker {}

/// How a [`run`] ended badly.
#[derive(Debug)]
pub(crate) enum Failure {
    CpuBudget,
    MemCap { cap_bytes: usize },
    WallClock,
    Trap(wasmtime::Error),
}

/// Outcome of one [`run`]. `mem_peak` is 0 when unknown (wall-clock timeout).
#[derive(Debug)]
pub(crate) struct Run {
    pub result: Result<Vec<u8>, Failure>,
    pub mem_peak: usize,
}

/// Classify a failed instantiate/call.
///
/// CPU-budget exhaustion is checked *before* the memory-cap flag: a rejected
/// `memory.grow` doesn't trap on the spot (wasm just sees `-1`), so a guest
/// that retries allocation in a loop can hit the CPU budget afterwards with
/// `cap_hit` still set. Checking the interrupt first reports the trap that
/// actually ended the call.
fn classify(err: wasmtime::Error, store: &Store<HostCtx>) -> Failure {
    let is_cpu = err.downcast_ref::<CpuBudgetExceededMarker>().is_some()
        || matches!(
            err.downcast_ref::<wasmtime::Trap>(),
            Some(t) if *t == wasmtime::Trap::Interrupt
        );
    if is_cpu {
        return Failure::CpuBudget;
    }
    let limiter = &store.data().limiter;
    if limiter.cap_hit {
        return Failure::MemCap {
            cap_bytes: limiter.mem_cap_bytes,
        };
    }
    Failure::Trap(err)
}

/// Instantiate `pre` in a fresh [`Store`] carrying `ctx` and call its
/// exported `handle(input) -> output`.
///
/// `ticks` is bumped once per epoch tick the guest ran; the caller owns it so
/// the count survives this future being dropped (cancellation, timeout).
/// The whole call is additionally bounded by a wall-clock timeout of
/// `cpu_budget_ms` plus [`HTTP_TIMEOUT`] plus a second of slack.
pub(crate) async fn run(
    pre: &HandlerPre<HostCtx>,
    ctx: HostCtx,
    input: &[u8],
    cpu_budget_ms: u64,
    ticks: Arc<AtomicU64>,
) -> Run {
    let wall_budget = Duration::from_millis(cpu_budget_ms) + HTTP_TIMEOUT + Duration::from_secs(1);
    let budget_ticks = (cpu_budget_ms / EPOCH_TICK_MS).max(1);

    let call = async move {
        let mut store = Store::new(pre.engine(), ctx);
        store.limiter(|c| &mut c.limiter as &mut dyn ResourceLimiter);

        // One tick at a time: count each tick that reaches the deadline,
        // yield to the tokio executor and extend the deadline by one tick.
        // The deadline is re-based after each yield, so time spent waiting to
        // be re-polled is not counted. Limits: see `call_limit`.
        let limit = Arc::new(AtomicU64::new(
            budget_ticks.saturating_add(INSTANTIATE_GRACE_TICKS),
        ));
        let (ticks_cb, limit_cb) = (ticks.clone(), limit.clone());
        store.epoch_deadline_callback(move |_store| {
            let n = ticks_cb.fetch_add(1, Ordering::Relaxed) + 1;
            if n > limit_cb.load(Ordering::Relaxed) {
                Err(wasmtime::Error::new(CpuBudgetExceededMarker))
            } else {
                Ok(wasmtime::UpdateDeadline::Yield(1))
            }
        });
        store.set_epoch_deadline(1);

        let outcome = async {
            let bindings = pre.instantiate_async(&mut store).await?;
            limit.store(
                call_limit(ticks.load(Ordering::Relaxed), budget_ticks),
                Ordering::Relaxed,
            );
            bindings.call_handle(&mut store, input).await
        }
        .await;
        let mem_peak = store.data().limiter.peak_bytes;
        let result = outcome.map_err(|e| classify(e, &store));
        Run { result, mem_peak }
    };

    match tokio::time::timeout(wall_budget, call).await {
        Ok(run) => run,
        Err(_elapsed) => Run {
            result: Err(Failure::WallClock),
            mem_peak: 0,
        },
    }
}

impl std::fmt::Debug for EpochTicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochTicker").finish_non_exhaustive()
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

    #[test]
    fn call_limit_forgives_at_most_the_grace() {
        use super::{call_limit, INSTANTIATE_GRACE_TICKS as G};
        // The limit is an absolute tick count for the whole invocation.
        for budget in [1u64, 10, 100] {
            for used in 0..=(budget + G + 1) {
                let total = call_limit(used, budget);
                assert!(total <= budget + G, "budget {budget} used {used}");
                assert!(total >= budget, "budget {budget} used {used}");
            }
            // Exact values, so an off-by-one in either direction fails.
            assert_eq!(call_limit(0, budget), budget);
            assert_eq!(call_limit(1, budget), budget + 1);
            assert_eq!(call_limit(2, budget), budget + 2);
            assert_eq!(call_limit(50, budget), budget + 2);
        }
        assert_eq!(G, 2, "the exact values above assume a grace of 2 ticks");
    }
}
