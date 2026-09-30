//! `warpline-host` — multi-tenant WASM function invocation server.
//!
//! Listens for `POST /tenants/{tenant}/functions/{func}/invoke`: auths the
//! bearer token against `warpline_core::pg::Authenticator`, resolves `(tenant, func)` to
//! a `Component` through `warpline_core::registry` (a shared content-hash
//! cache the control plane writes into — see `crates/control`), invokes it
//! via `warpline_core::runtime::invoke`, writes a metering row off the
//! response path, and returns the guest's raw response bytes. Also serves
//! `/healthz` and a Prometheus `/metrics` scrape endpoint.
//!
//! Split into this lib (state + [`router`]) and a thin `main.rs` so
//! `crates/host/tests/` can drive the whole app through
//! `tower::ServiceExt::oneshot` without a real listening socket.
//!
//! `/metrics` is deliberately not on [`router`] — see [`metrics_router`] —
//! since it exposes per-tenant series and shouldn't share a listener with
//! whatever's publicly reachable.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use wasmtime::component::Linker;
use wasmtime::Engine;

use warpline_core::{
    kv::KvStore,
    pg::{AuthOutcome, Authenticator},
    registry::{self, ComponentCache},
    runtime::{invoke, EpochTicker, InvokeError, InvokeOutcome},
    types::{parse_bearer, valid_name, HostCtx},
    Limits, MeterSink, Usage,
};

/// Request body size cap for `/invoke`.
const INVOKE_BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// Process-wide state shared across axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub linker: Linker<HostCtx>,
    pub modules_dir: PathBuf,
    pub component_cache: Arc<ComponentCache>,
    pub kv: Arc<dyn KvStore>,
    pub http_client: reqwest::Client,
    pub allow_private_egress: bool,
    /// `None` = dev mode without Postgres: no auth, default limits.
    pub auth: Option<Authenticator>,
    pub metrics_handle: PrometheusHandle,
    /// Keeps the engine's epoch-ticker thread alive for the process
    /// lifetime; never read, only held.
    pub ticker: Arc<EpochTicker>,
    /// Where completed invocations are reported — `pg::PgMeter` with a
    /// database, [`LogMeter`] without.
    pub meter: Arc<dyn MeterSink>,
}

/// Dev-mode [`MeterSink`]: logs at debug level instead of storing.
#[derive(Debug, Default)]
pub struct LogMeter;

impl MeterSink for LogMeter {
    fn record(&self, tenant: &str, func: &str, usage: Usage, ok: bool) {
        tracing::debug!(%tenant, %func, ?usage, ok, "invoke completed (metering disabled)");
    }
}

static METRICS: OnceLock<PrometheusHandle> = OnceLock::new();

/// Installs the global Prometheus recorder exactly once per process and
/// returns its handle. `metrics::set_global_recorder` can only succeed
/// once, so every test that builds its own [`AppState`] calls this rather
/// than reaching for `PrometheusBuilder` directly.
pub fn metrics_handle() -> PrometheusHandle {
    METRICS
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("install prometheus recorder")
        })
        .clone()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/tenants/{tenant}/functions/{func}/invoke",
            post(invoke_handler).layer(DefaultBodyLimit::max(INVOKE_BODY_LIMIT_BYTES)),
        )
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

async fn metrics_handler(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    handle.render()
}

/// `/metrics` on its own router, with its own `PrometheusHandle` state
/// rather than the full [`AppState`] — meant to be served on a separate
/// listener (`WARPLINE_METRICS_BIND`, default loopback-only; see
/// `main.rs`) so a scrape endpoint reachable from wherever `/invoke` is
/// doesn't also leak tenant/function names (finding 6).
pub fn metrics_router(metrics_handle: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics_handle)
}

/// Outcome label for the `warpline_invoke_total` counter.
fn outcome_label(result: &Result<InvokeOutcome, InvokeError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(InvokeError::CpuBudgetExceeded { .. }) => "cpu_budget_exceeded",
        Err(InvokeError::MemoryCapExceeded { .. }) => "memory_cap_exceeded",
        Err(InvokeError::WallClockTimeout(_)) => "wall_clock_timeout",
        Err(InvokeError::GuestTrap(_)) => "guest_trap",
        Err(InvokeError::Instantiate(_)) => "instantiate_error",
    }
}

async fn invoke_handler(
    State(state): State<AppState>,
    Path((tenant, func)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !valid_name(&tenant) || !valid_name(&func) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid tenant or function name".to_string(),
        )
            .into_response();
    }

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer);
    let outcome = match &state.auth {
        Some(auth) => auth.authenticate(&tenant, bearer).await,
        None => Ok(AuthOutcome::Authorized(Limits::default())),
    };
    let limits = match outcome {
        Ok(AuthOutcome::Authorized(l)) => l,
        Ok(AuthOutcome::WrongTenant) => {
            return (StatusCode::FORBIDDEN, "forbidden".to_string()).into_response()
        }
        Ok(_) => return (StatusCode::UNAUTHORIZED, "unauthorized".to_string()).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "auth lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "auth error".to_string()).into_response();
        }
    };

    let pre = match registry::resolve(
        state.engine.clone(),
        state.linker.clone(),
        state.component_cache.clone(),
        state.modules_dir.clone(),
        tenant.clone(),
        func.clone(),
    )
    .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("no module for {tenant}/{func}"),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(%tenant, %func, error = %e, "failed to resolve module");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load module".to_string(),
            )
                .into_response();
        }
    };

    let ctx = HostCtx::new(
        tenant.clone(),
        func.clone(),
        state.kv.clone(),
        limits.allowed_hosts.clone(),
        state.allow_private_egress,
        state.http_client.clone(),
        limits.mem_cap_bytes,
    );

    let wall_started = Instant::now();
    let result = invoke(
        &state.engine,
        &pre,
        ctx,
        body.to_vec(),
        limits.cpu_budget_ms,
    )
    .await;
    let wall_us = wall_started.elapsed().as_micros() as u64;

    // `func` dropped from the histogram's labels (finding 7): the
    // per-tenant function quota (finding 3, `warpline-control`) bounds how
    // many distinct `func` values a tenant can create, but not how many
    // tenants there are, so keeping `func` off a metric every tenant
    // contributes to avoids multiplying that cardinality further.
    metrics::histogram!(
        "warpline_invoke_duration_us",
        "tenant" => tenant.clone()
    )
    .record(match &result {
        Ok(o) => o.cpu_us as f64,
        Err(_) => wall_us as f64,
    });
    metrics::counter!(
        "warpline_invoke_total",
        "tenant" => tenant.clone(),
        "func" => func.clone(),
        "outcome" => outcome_label(&result)
    )
    .increment(1);

    // Best-effort metering fields for the error path: `cpu_us` falls back
    // to wall time (no epoch-derived figure survives a failed call) and
    // `mem_peak_bytes` is only known when the limiter is what tripped.
    // `cpu_us` is still a wall-clock measurement everywhere for now.
    let (cpu_us, mem_peak_bytes, ok) = match &result {
        Ok(o) => (o.cpu_us, o.mem_peak_bytes, true),
        Err(InvokeError::MemoryCapExceeded { peak_bytes, .. }) => (wall_us, *peak_bytes, false),
        Err(_) => (wall_us, 0, false),
    };
    // `record` never blocks; a sink that cannot keep up counts its own drops
    // (`PgMeter::dropped`).
    state.meter.record(
        &tenant,
        &func,
        Usage::new(cpu_us, wall_us, mem_peak_bytes),
        ok,
    );

    match result {
        Ok(outcome) => (StatusCode::OK, outcome.output).into_response(),
        Err(e) => {
            tracing::warn!(%tenant, %func, error = %e, "invoke failed");
            // Budget/timeout is the guest's own doing (408); a memory-cap
            // trip means the guest asked for more than its tenant is
            // allotted (507); anything else is a host-side trap or bug
            // (500). Bodies stay free of host paths/internals.
            let (status, body) = match e {
                InvokeError::CpuBudgetExceeded { .. } | InvokeError::WallClockTimeout(_) => {
                    (StatusCode::REQUEST_TIMEOUT, e.to_string())
                }
                InvokeError::MemoryCapExceeded { .. } => {
                    (StatusCode::INSUFFICIENT_STORAGE, e.to_string())
                }
                InvokeError::GuestTrap(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "guest trapped".to_string(),
                ),
                InvokeError::Instantiate(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to instantiate component".to_string(),
                ),
            };
            (status, body).into_response()
        }
    }
}

/// Waits for either ctrl-c or SIGTERM (the latter is what container
/// orchestrators send) so in-flight invokes get a chance to finish before
/// `axum::serve` stops accepting new ones.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
