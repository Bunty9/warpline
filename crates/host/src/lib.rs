//! `warpline-host` — multi-tenant WASM function invocation server.
//!
//! Listens for `POST /tenants/{tenant}/functions/{func}/invoke`: auths the
//! bearer token against `warpline_core::pg::Authenticator`, invokes the
//! function through `warpline_core::Runtime` (which resolves the module the
//! control plane published, enforces limits and admission, and reports usage
//! to the configured `MeterSink`), and returns the guest's raw response
//! bytes. Also serves
//! `/healthz` and a Prometheus `/metrics` scrape endpoint.
//!
//! This library exists to serve the `warpline-host` binary and its tests; its
//! API is not covered by semver guarantees beyond the binary's behaviour.
//!
//! Split into this lib (state + [`router`]) and a thin `main.rs` so
//! `crates/host/tests/` can drive the whole app through
//! `tower::ServiceExt::oneshot` without a real listening socket.
//!
//! `/metrics` is deliberately not on [`router`] — see [`metrics_router`] —
//! since it exposes per-tenant series and shouldn't share a listener with
//! whatever's publicly reachable.

use std::sync::OnceLock;
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

use warpline_core::{
    pg::{AuthOutcome, Authenticator},
    types::{parse_bearer, valid_name},
    Invocation, InvokeError, Limits, MeterSink, Runtime, Usage,
};

/// Request body size cap for `/invoke`.
const INVOKE_BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// Process-wide state shared across axum handlers.
#[derive(Clone, Debug)]
pub struct AppState {
    /// Built with the meter that should receive completed invocations —
    /// `pg::PgMeter` with a database, [`LogMeter`] without.
    pub runtime: Runtime,
    /// `None` = dev mode without Postgres: no auth, default limits.
    pub auth: Option<Authenticator>,
    pub metrics_handle: PrometheusHandle,
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
/// than reaching for `PrometheusBuilder` directly. If some other recorder
/// is already installed (an embedding app's), this logs a warning and
/// returns a handle that renders nothing rather than panicking.
pub fn metrics_handle() -> PrometheusHandle {
    METRICS
        .get_or_init(|| match PrometheusBuilder::new().install_recorder() {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!(error = %e, "a metrics recorder is already installed; /metrics will be empty");
                PrometheusBuilder::new().build_recorder().handle()
            }
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
fn outcome_label(result: &Result<Invocation, InvokeError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(InvokeError::InvalidName) => "invalid_name",
        Err(InvokeError::NotFound) => "not_found",
        Err(InvokeError::Overloaded) => "overloaded",
        Err(InvokeError::TenantBusy) => "tenant_busy",
        Err(InvokeError::CpuBudgetExceeded { .. }) => "cpu_budget_exceeded",
        Err(InvokeError::MemoryCapExceeded { .. }) => "memory_cap_exceeded",
        Err(InvokeError::WallClockTimeout { .. }) => "wall_clock_timeout",
        Err(InvokeError::OutputTooLarge { .. }) => "output_too_large",
        Err(InvokeError::GuestTrap { .. }) => "guest_trap",
        Err(InvokeError::Load(_)) => "load_error",
        Err(_) => "error",
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
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "auth unavailable".to_string(),
            )
                .into_response();
        }
    };

    let started = Instant::now();
    let result = state
        .runtime
        .invoke(&tenant, &func, body.to_vec(), &limits)
        .await;
    let wall_us = started.elapsed().as_micros() as u64;

    // `func` dropped from the histogram's labels (finding 7): the
    // per-tenant function quota (finding 3, `warpline-control`) bounds how
    // many distinct `func` values a tenant can create, but not how many
    // tenants there are, so keeping `func` off a metric every tenant
    // contributes to avoids multiplying that cardinality further.
    metrics::histogram!(
        "warpline_invoke_duration_us",
        "tenant" => tenant.clone()
    )
    .record(wall_us as f64);
    metrics::counter!(
        "warpline_invoke_total",
        "tenant" => tenant.clone(),
        "func" => func.clone(),
        "outcome" => outcome_label(&result)
    )
    .increment(1);

    match result {
        Ok(outcome) => (StatusCode::OK, outcome.output).into_response(),
        Err(e) => {
            tracing::warn!(%tenant, %func, error = %e, "invoke failed");
            // Bodies stay free of host paths/internals.
            let status =
                StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body = match &e {
                InvokeError::NotFound => format!("no module for {tenant}/{func}"),
                InvokeError::GuestTrap { .. } => "guest trapped".to_string(),
                InvokeError::Load(_) => "failed to load module".to_string(),
                _ => e.to_string(),
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
