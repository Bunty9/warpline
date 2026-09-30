//! `warpline-control` — control plane HTTP API.
//!
//! - `POST /tenants/{tenant}/functions/{func}` accepts a multipart `.wasm`
//!   upload (field `wasm`/`module`/`file`) and stages it through
//!   `warpline_core::Runtime::stage` (compile under the runtime's compile
//!   semaphore, type-check against the handler world, persist), so a bad
//!   component is rejected here rather than at invoke time. Only then is
//!   the `(tenant, func)` pointer activated — in DB mode after a
//!   transaction that also enforces the per-tenant function quota (see
//!   `publish_pointer`).
//! - `POST /admin/tenants/{tenant}` (guarded by the admin token) creates a
//!   tenant idempotently, optionally sets its resource limits, and issues a
//!   new API key via `warpline_core::pg::create_tenant` (one transaction).
//!   Fields left out of the body take the default limits when a body is
//!   sent at all.
//! - `GET /healthz`.
//!
//! Split into this lib (state + [`router`]) and a thin `main.rs` so
//! `crates/control/tests/` can drive the whole app through
//! `tower::ServiceExt::oneshot` without a real listening socket.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use warpline_core::{
    pg::{self, AdminError, AuthOutcome, Authenticator},
    types::{parse_bearer, valid_name, validate_cpu_budget_ms, validate_mem_cap_bytes},
    Limits, PublishError, Runtime, Staged,
};

/// Multipart upload size cap.
const UPLOAD_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Default per-tenant cap on distinct function names (finding 3) — also
/// bounds the cardinality of the `func` label a tenant can push into
/// metrics/logs. A field on [`AppState`] rather than a bare constant so
/// tests can override it to a small number and exercise the quota boundary
/// without uploading 100 real functions.
const DEFAULT_MAX_FUNCTIONS_PER_TENANT: i64 = 100;

#[derive(Clone, Debug)]
pub struct AppState {
    /// Stages and activates uploaded modules.
    pub runtime: Runtime,
    /// `None` = dev mode without Postgres: no auth, default limits, no admin
    /// API. The pool is reached through [`Authenticator::pool`].
    pub auth: Option<Authenticator>,
    /// Admin bearer token, or `None` — the admin route is disabled (404) in
    /// that case. Set by the binary (from `WARPLINE_ADMIN_TOKEN`).
    pub admin_token: Option<String>,
    /// Per-tenant cap on distinct function names, see
    /// `DEFAULT_MAX_FUNCTIONS_PER_TENANT`. A field (not that constant
    /// directly) so tests can dial it down and exercise the quota boundary
    /// cheaply.
    pub max_functions_per_tenant: i64,
}

impl AppState {
    pub fn new(runtime: Runtime, auth: Option<Authenticator>) -> Self {
        Self {
            runtime,
            auth,
            admin_token: None,
            max_functions_per_tenant: DEFAULT_MAX_FUNCTIONS_PER_TENANT,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/tenants/{tenant}/functions/{func}",
            post(upload).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT_BYTES)),
        )
        .route("/admin/tenants/{tenant}", post(admin_create_tenant))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

fn bearer_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer)
}

async fn upload(
    State(state): State<AppState>,
    Path((tenant, func)): Path<(String, String)>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if !valid_name(&tenant) || !valid_name(&func) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid tenant or function name".to_string(),
        )
            .into_response();
    }

    let outcome = match &state.auth {
        Some(auth) => auth.authenticate(&tenant, bearer_from(&headers)).await,
        None => Ok(AuthOutcome::Authorized(Limits::default())),
    };
    match outcome {
        Ok(AuthOutcome::Authorized(_)) => {}
        Ok(AuthOutcome::WrongTenant) => {
            return (StatusCode::FORBIDDEN, "forbidden".to_string()).into_response()
        }
        Ok(_) => return (StatusCode::UNAUTHORIZED, "unauthorized".to_string()).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "auth lookup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "auth error".to_string()).into_response();
        }
    };

    let mut wasm_bytes: Option<Bytes> = None;
    loop {
        // `MultipartError::status` already distinguishes a body over the
        // `DefaultBodyLimit` (413) from a malformed multipart body (400) —
        // use it rather than collapsing every field error to 400.
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return (e.status(), format!("multipart error: {e}")).into_response(),
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "wasm" || name == "module" || name == "file" {
            match field.bytes().await {
                // Keep the zero-copy `Bytes` handle rather than `.to_vec()`
                // — cloning it below is a refcount bump, not a memcpy
                // (finding 2).
                Ok(b) => wasm_bytes = Some(b),
                Err(e) => return (e.status(), format!("read field failed: {e}")).into_response(),
            }
        }
    }
    let Some(wasm) = wasm_bytes else {
        return (StatusCode::BAD_REQUEST, "missing wasm field".to_string()).into_response();
    };

    // Compile (bounded by the runtime's compile semaphore), typecheck and
    // persist. A component that fails either check never leaves a blob
    // behind for the GC pass to clean up.
    let staged = match state.runtime.stage(wasm).await {
        Ok(staged) => staged,
        Err(PublishError::Compile(e)) => {
            tracing::warn!(%tenant, %func, error = %e, "component compile/parse failed");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid wasm component".to_string(),
            )
                .into_response();
        }
        Err(PublishError::ImportMismatch(e)) => {
            tracing::warn!(%tenant, %func, error = %e, "component failed import/export typecheck");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "component does not match the warpline handler world (missing export or \
                 unsatisfiable import)"
                    .to_string(),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(%tenant, %func, error = %e, "failed to persist compiled module");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to publish module".to_string(),
            )
                .into_response();
        }
    };
    let wasm_digest = staged.digest.clone();

    if let Err((status, msg)) = publish_pointer(&state, &tenant, &func, &staged).await {
        return (status, msg).into_response();
    }

    tracing::info!(%tenant, %func, digest = %wasm_digest, "module compiled and published");
    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "tenant": tenant,
            "func": func,
            "digest": wasm_digest,
        })),
    )
        .into_response()
}

/// Publish `(tenant, func)` -> `wasm_digest`.
///
/// In DB mode (finding 10): one transaction takes a per-*tenant* advisory
/// lock, enforces [`AppState::max_functions_per_tenant`] (finding 3), and
/// upserts the `functions` row — all before committing, so a quota
/// rejection or any DB failure never leaves a `functions` row behind that
/// the pointer file doesn't back.
///
/// The lock is keyed on `tenant` alone, not `(tenant, func)` (finding 5):
/// two concurrent uploads of two different *new* function names for the
/// same tenant must serialize against each other too, or both can read the
/// same `count(*)` before either inserts and both pass the quota check —
/// only a lock that's shared across every upload for a tenant closes that
/// race.
///
/// The pointer file is written *after* `tx.commit()`, not before (finding
/// 6): the advisory lock is released at commit, so a concurrent upload of
/// the *same* `(tenant, func)` could in principle commit its own
/// `functions` row and then race this one on the pointer write — acceptable,
/// last writer wins, and it's the same outcome a sequential re-upload would
/// have anyway. Writing the pointer post-commit instead means a commit
/// failure (or a crash between the two) never leaves a pointer that serves
/// new code without a `functions` row backing it; the reverse case (a row
/// with no pointer yet) just 404s until the write is retried, which is the
/// harmless direction to fail in.
///
/// Without a database (dev mode), there is nothing to lock or upsert
/// against, so this just writes the pointer.
async fn publish_pointer(
    state: &AppState,
    tenant: &str,
    func: &str,
    staged: &Staged,
) -> Result<(), (StatusCode, String)> {
    let activate = || async {
        state
            .runtime
            .activate(tenant, func, staged)
            .await
            .map_err(|e| {
                tracing::error!(%tenant, %func, error = %e, "failed to write pointer");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to publish module".to_string(),
                )
            })
    };
    let Some(pool) = state.auth.as_ref().map(Authenticator::pool) else {
        return activate().await;
    };

    let internal_error = |e: sqlx::Error, action: &str| {
        tracing::error!(%tenant, %func, error = %e, "{action}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
    };

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| internal_error(e, "failed to start publish transaction"))?;

    // Serialize concurrent uploads for the same *tenant* (not just the same
    // func — see doc comment above) so the quota check below can't race
    // with itself.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .map_err(|e| internal_error(e, "failed to take publish lock"))?;

    // The tenant may have been deleted while its key sat in the auth cache.
    let tenant_id: sqlx::types::Uuid =
        sqlx::query_scalar("SELECT id FROM warpline.tenants WHERE name = $1")
            .bind(tenant)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| internal_error(e, "failed to look up tenant"))?
            .ok_or((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))?;

    let max_functions = state.max_functions_per_tenant;
    let other_functions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM warpline.functions WHERE tenant_id = $1 AND name <> $2",
    )
    .bind(tenant_id)
    .bind(func)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| internal_error(e, "failed to check function quota"))?;
    if other_functions >= max_functions {
        // `tx` drops here without a commit, rolling back the advisory lock
        // release included.
        return Err((
            StatusCode::FORBIDDEN,
            format!("tenant function quota exceeded ({max_functions} max)"),
        ));
    }

    sqlx::query(
        "INSERT INTO warpline.functions (tenant_id, name, wasm_hash) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, name) DO UPDATE SET wasm_hash = EXCLUDED.wasm_hash",
    )
    .bind(tenant_id)
    .bind(func)
    .bind(&staged.digest)
    .execute(&mut *tx)
    .await
    .map_err(|e| internal_error(e, "failed to upsert functions row"))?;

    tx.commit()
        .await
        .map_err(|e| internal_error(e, "failed to commit publish transaction"))?;

    // Written after the commit, not before (finding 6) — see doc comment
    // above for why that ordering is the one that can't leave a pointer
    // serving code the `functions` table doesn't know about.
    activate().await
}

/// `POST /admin/tenants/{tenant}` request body — every field optional, so a
/// bare `POST` with no body just creates the tenant + issues a key.
#[derive(Debug, Default, serde::Deserialize)]
struct AdminConfigBody {
    allowed_hosts: Option<Vec<String>>,
    cpu_budget_ms: Option<i64>,
    mem_cap_bytes: Option<i64>,
}

/// Constant-time byte comparison — timing-safe check of the admin bearer
/// token against `WARPLINE_ADMIN_TOKEN`. Hand-rolled rather than pulling in
/// a crate for one XOR-accumulate loop.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn admin_create_tenant(
    State(state): State<AppState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(expected_token) = &state.admin_token else {
        // No admin token configured: the route doesn't exist.
        return StatusCode::NOT_FOUND.into_response();
    };
    let provided = bearer_from(&headers);
    if !provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected_token.as_bytes())) {
        return (StatusCode::UNAUTHORIZED, "unauthorized".to_string()).into_response();
    }
    if !valid_name(&tenant) {
        return (StatusCode::BAD_REQUEST, "invalid tenant name".to_string()).into_response();
    }

    let Some(pool) = state.auth.as_ref().map(Authenticator::pool) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "admin API requires DATABASE_URL".to_string(),
        )
            .into_response();
    };

    let cfg: AdminConfigBody = if body.is_empty() {
        AdminConfigBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(c) => c,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("invalid json body: {e}")).into_response()
            }
        }
    };
    // No fields -> leave an existing tenant's limits alone. Otherwise the
    // body describes the tenant's limits, unspecified fields at defaults.
    // Range checks run on the raw i64s so negatives get a proper 400;
    // `create_tenant` validates hosts again and stores them normalised.
    let limits = if cfg.allowed_hosts.is_none()
        && cfg.cpu_budget_ms.is_none()
        && cfg.mem_cap_bytes.is_none()
    {
        None
    } else {
        let mut l = Limits::default();
        if let Some(ms) = cfg.cpu_budget_ms {
            if let Err(e) = validate_cpu_budget_ms(ms) {
                return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
            }
            l.cpu_budget_ms = ms as u64;
        }
        if let Some(b) = cfg.mem_cap_bytes {
            if let Err(e) = validate_mem_cap_bytes(b) {
                return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
            }
            l.mem_cap_bytes = b as usize;
        }
        if let Some(hosts) = cfg.allowed_hosts {
            l.allowed_hosts = hosts;
        }
        Some(l)
    };

    match pg::create_tenant(pool, &tenant, limits.as_ref()).await {
        Ok(key) => {
            tracing::info!(%tenant, "tenant created/updated, api key issued");
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({ "tenant": key.tenant, "api_key": key.api_key })),
            )
                .into_response()
        }
        Err(e @ (AdminError::Config(_) | AdminError::InvalidName)) => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => {
            tracing::error!(%tenant, error = %e, "failed to create/update tenant");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_regular_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
