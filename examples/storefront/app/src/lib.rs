//! The storefront app: an axum router around a warpline [`Runtime`].
//!
//! Numbered `[warpline N]` comments mark every place the app touches
//! warpline; the README walks through them in the same order.

pub mod checkout;
pub mod fraud;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use warpline_core::pg::{
    self, AdminError, AuthOutcome, Authenticator, PgMeter, PgMeterHandle, PgPool,
};
use warpline_core::{digest, Bytes, Limits, PublishError, Runtime, RuntimeConfig};

use checkout::{HookInput, Order, Verdict, HOOK_FN};

/// A merchant hook is a compiled component; this is generous for one.
const MAX_HOOK_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

/// [warpline 1] Build the runtime: the object the whole app shares.
///
/// - `modules_dir` is where published components live (compiled code, so
///   keep it private to this process; see the `RuntimeConfig` docs).
/// - `allow_private_egress = true` lets hooks reach loopback and private
///   addresses. This example needs it because the fraud service is
///   `127.0.0.1` or a docker-compose name. **Production keeps the default
///   `false`** and allowlists public hosts, so a merchant's hook cannot
///   probe your internal network.
/// - `PgMeter` records every invocation that reached a guest into
///   `warpline.meter` without blocking the request; the returned handle
///   must be shut down to flush it (see `main.rs`).
/// - No `.kv(...)`: guests' `kv` is the default in-memory store, so loyalty
///   counters reset when the app restarts. Implement `KvStore` over your own
///   database to make them durable.
pub fn build_runtime(
    modules_dir: impl Into<PathBuf>,
    pool: &PgPool,
) -> Result<(Runtime, PgMeterHandle), warpline_core::Error> {
    let mut cfg = RuntimeConfig::new(modules_dir);
    cfg.allow_private_egress = true;
    cfg.max_output_bytes = 1024 * 1024; // hooks return small JSON; also vetted at 64 KiB
    let (meter, meter_handle) = PgMeter::spawn(pool.clone(), 1024);
    let runtime = Runtime::builder(cfg).meter(Arc::new(meter)).build()?;
    Ok((runtime, meter_handle))
}

#[derive(Clone)]
pub struct AppState {
    runtime: Runtime,
    pool: PgPool,
    /// [warpline 4] Bearer-token auth for merchant routes.
    auth: Authenticator,
    /// Digest of the admin token; compared as digests so the comparison does
    /// not leak the token's prefix through timing.
    admin_token_digest: Arc<str>,
    fraud_api: Arc<str>,
}

impl AppState {
    pub fn new(runtime: Runtime, pool: PgPool, admin_token: &str, fraud_api: &str) -> Self {
        // The TTL is how long a revoked key or changed limit can go unnoticed
        // by this process; the cache keeps auth off the database hot path.
        let auth = Authenticator::new(pool.clone(), Duration::from_secs(5));
        Self {
            runtime,
            pool,
            auth,
            admin_token_digest: digest(admin_token.as_bytes()).into(),
            fraud_api: fraud_api.into(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/admin/merchants", post(create_merchant))
        .route(
            "/merchant/{name}/hook",
            put(upload_hook).layer(DefaultBodyLimit::max(MAX_HOOK_UPLOAD_BYTES)),
        )
        .route("/merchant/{name}/usage", get(usage))
        .route("/shops/{merchant}/checkout", post(checkout))
        .with_state(state)
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// [warpline 4] Authenticate a merchant key for merchant `name`. 401 for
/// no/unknown key, 403 for another merchant's key. On success `authenticate`
/// also returns the merchant's limits; the routes here do not need them
/// (checkout reads them itself, see below), an app that invokes on behalf of
/// the authenticated caller would use them directly.
async fn authorize(
    state: &AppState,
    name: &str,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    match state.auth.authenticate(name, bearer(headers)).await {
        Ok(AuthOutcome::Authorized(_)) => Ok(()),
        Ok(AuthOutcome::MissingOrUnknownKey) => {
            Err((StatusCode::UNAUTHORIZED, "missing or unknown API key"))
        }
        Ok(_) => Err((StatusCode::FORBIDDEN, "key belongs to another merchant")),
        Err(e) => {
            tracing::error!(error = %e, "auth lookup failed");
            Err((StatusCode::SERVICE_UNAVAILABLE, "auth unavailable"))
        }
    }
}

#[derive(Deserialize)]
struct NewMerchant {
    name: String,
    allowed_hosts: Option<Vec<String>>,
    cpu_budget_ms: Option<u64>,
    mem_cap_bytes: Option<usize>,
}

/// `POST /admin/merchants`: create a merchant and return its API key (shown
/// once; only a hash is stored).
async fn create_merchant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NewMerchant>,
) -> Response {
    let presented = bearer(&headers).map(|t| digest(t.as_bytes()));
    if presented.as_deref() != Some(&*state.admin_token_digest) {
        return error(StatusCode::UNAUTHORIZED, "bad admin token");
    }

    // [warpline 3] A tenant is a row with limits; the key is issued in the
    // same transaction. Start from the defaults and override what was sent;
    // `create_tenant` validates the ranges and host names.
    let mut limits = Limits::default();
    if let Some(v) = req.cpu_budget_ms {
        limits.cpu_budget_ms = v;
    }
    if let Some(v) = req.mem_cap_bytes {
        limits.mem_cap_bytes = v;
    }
    if let Some(hosts) = req.allowed_hosts {
        limits.allowed_hosts = hosts;
    }
    match pg::create_tenant(&state.pool, &req.name, Some(&limits)).await {
        Ok(key) => (
            StatusCode::CREATED,
            Json(json!({ "merchant": key.tenant, "api_key": key.api_key })),
        )
            .into_response(),
        Err(e @ (AdminError::InvalidName | AdminError::Config(_))) => {
            error(StatusCode::BAD_REQUEST, e)
        }
        Err(e) => {
            tracing::error!(error = %e, "create_tenant failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not create merchant",
            )
        }
    }
}

/// `PUT /merchant/{name}/hook`: the raw wasm body becomes the merchant's
/// `checkout` function.
async fn upload_hook(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((status, msg)) = authorize(&state, &name, &headers).await {
        return error(status, msg);
    }
    // [warpline 5] Compile, check it fits the `handler` world, persist, and
    // atomically point (merchant, "checkout") at it. Bad uploads are the
    // client's fault (422); nothing is activated on failure.
    match state.runtime.publish(&name, HOOK_FN, body).await {
        Ok(staged) => Json(json!({ "digest": staged.digest })).into_response(),
        Err(e @ (PublishError::Compile(_) | PublishError::ImportMismatch(_))) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, e)
        }
        Err(PublishError::InvalidName) => error(StatusCode::BAD_REQUEST, "invalid merchant name"),
        Err(e) => {
            tracing::error!(error = %e, "publish failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "could not publish hook")
        }
    }
}

/// `GET /merchant/{name}/usage`: what this merchant's hooks have used.
async fn usage(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err((status, msg)) = authorize(&state, &name, &headers).await {
        return error(status, msg);
    }
    // [warpline 8] Totals over the rows PgMeter wrote. The meter is batched,
    // so the newest invocations may take a moment to show up.
    match pg::usage_summary(&state.pool, &name, chrono::DateTime::UNIX_EPOCH).await {
        Ok(u) => Json(json!({
            "invocations": u.invocations,
            "errors": u.errors,
            "cpu_us": u.cpu_us,
            "wall_us": u.wall_us,
            "max_mem_peak_bytes": u.max_mem_peak_bytes,
        }))
        .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "usage query failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "usage unavailable")
        }
    }
}

/// `POST /shops/{merchant}/checkout`: public. Prices the order, asks the
/// merchant's hook for a decision and applies the policy in [`checkout`].
async fn checkout(
    State(state): State<AppState>,
    Path(merchant): Path<String>,
    Json(order): Json<Order>,
) -> Response {
    let subtotal = match order.subtotal() {
        Ok(s) => s,
        Err(msg) => return error(StatusCode::BAD_REQUEST, msg),
    };
    // The route is public, so there is no key to take limits from: read the
    // merchant's stored limits. (A busy app would cache this.) An unknown
    // merchant is a 404 before any hook runs.
    let limits = match pg::tenant_limits(&state.pool, &merchant).await {
        Ok(Some(l)) => l,
        Ok(None) => return error(StatusCode::NOT_FOUND, "unknown merchant"),
        Err(e) => {
            tracing::error!(error = %e, "tenant_limits failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "merchant lookup failed");
        }
    };
    let input = match serde_json::to_vec(&HookInput {
        customer: &order.customer,
        items: &order.items,
        subtotal_cents: subtotal,
        fraud_api: &state.fraud_api,
    }) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    // [warpline 6] Invoke with the merchant's own limits: CPU budget, memory
    // cap and the http-out allowlist all come from `limits`. `invoke` never
    // queues; it fails fast with Overloaded / TenantBusy.
    let result = state
        .runtime
        .invoke(&merchant, HOOK_FN, input, &limits)
        .await
        .map(|inv| inv.output);

    // [warpline 7] Turn whatever happened into a shopper-facing outcome.
    let (approved, discount, message, hook) = match checkout::resolve(result, subtotal) {
        Verdict::Hook(d) => (d.approved, d.discount_cents, d.message, "ok"),
        Verdict::NoHook => (true, 0, "approved".into(), "none"),
        Verdict::FailOpen(why) => {
            tracing::warn!(%merchant, %why, "checkout hook failed; approving without discount");
            (true, 0, "approved".into(), "failed")
        }
        Verdict::Retry => {
            let mut resp = error(StatusCode::SERVICE_UNAVAILABLE, "busy, retry shortly");
            resp.headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            return resp;
        }
    };
    Json(json!({
        "approved": approved,
        "subtotal_cents": subtotal,
        "discount_cents": discount,
        "total_cents": subtotal - discount, // discount <= subtotal is enforced
        "message": message,
        "hook": hook,
    }))
    .into_response()
}
