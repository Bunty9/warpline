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
//!   new API key via `warpline_core::pg::patch_tenant` (one transaction).
//!   PATCH semantics: fields left out of the body keep the tenant's current
//!   values (defaults for a new tenant); they are never reset.
//! - `GET /healthz`.
//!
//! This library exists to serve the `warpline-control` binary and its tests; its
//! API is not covered by semver guarantees beyond the binary's behaviour.
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
use sqlx::PgPool;
use warpline_core::{
    pg::{self, AdminError, AuthOutcome, Authenticator, LimitsPatch},
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
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "auth unavailable".to_string(),
            )
                .into_response();
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

/// Publish `(tenant, func)` -> `staged`.
///
/// In DB mode (finding 10): one transaction takes a per-*tenant* advisory
/// lock, enforces [`AppState::max_functions_per_tenant`] (finding 3), and
/// upserts the `functions` row.
///
/// The lock is keyed on `tenant` alone, not `(tenant, func)` (finding 5):
/// two concurrent uploads of two different *new* function names for the
/// same tenant must serialize against each other too, or both can read the
/// same `count(*)` before either inserts and both pass the quota check.
///
/// The pointer is switched *inside* the transaction, before the commit, so
/// the lock is still held: two uploads of the same `(tenant, func)` cannot
/// interleave. If the commit fails, [`reconcile_pointer`] takes the lock
/// again and points the pointer back at whatever the `functions` row says
/// (the database is the source of truth), so it cannot clobber a concurrent
/// upload that committed in between. A crash between activate and commit can
/// still leave the pointer ahead of the row; the next upload repairs it.
///
/// The transactional part runs in its own spawned task: if the client
/// disconnects, axum drops the handler future, and a dropped future would
/// roll back after `activate` with nothing to restore the pointer (which
/// would then serve code no row backs, and let timed disconnects dodge the
/// quota). The task always runs to completion.
///
/// Without a database (dev mode), there is nothing to lock or upsert
/// against, so this just writes the pointer.
async fn publish_pointer(
    state: &AppState,
    tenant: &str,
    func: &str,
    staged: &Staged,
) -> Result<(), (StatusCode, String)> {
    let pointer_error = |what: &str, e: &dyn std::fmt::Display| {
        tracing::error!(%tenant, %func, error = %e, "{what}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to publish module".to_string(),
        )
    };
    if state.auth.is_none() {
        return state
            .runtime
            .activate(tenant, func, staged)
            .await
            .map_err(|e| pointer_error("failed to write pointer", &e));
    };

    let (state, tenant, func, staged) = (
        state.clone(),
        tenant.to_owned(),
        func.to_owned(),
        staged.clone(),
    );
    tokio::spawn(async move { publish_in_db(&state, &tenant, &func, &staged).await })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "publish task failed");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ))
        })
}

/// The lock -> quota -> upsert -> activate -> commit section of
/// [`publish_pointer`]; see there for why it is spawned.
async fn publish_in_db(
    state: &AppState,
    tenant: &str,
    func: &str,
    staged: &Staged,
) -> Result<(), (StatusCode, String)> {
    let pool = state
        .auth
        .as_ref()
        .map(Authenticator::pool)
        .expect("only called in DB mode");
    let pointer_error = |what: &str, e: &dyn std::fmt::Display| {
        tracing::error!(%tenant, %func, error = %e, "{what}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to publish module".to_string(),
        )
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
    // Bound every wait so a DB partition cannot hang the request forever.
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .map_err(|e| internal_error(e, "failed to set publish timeouts"))?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .map_err(|e| internal_error(e, "failed to set publish timeouts"))?;
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

    state
        .runtime
        .activate(tenant, func, staged)
        .await
        .map_err(|e| pointer_error("failed to write pointer", &e))?;

    if let Err(e) = tx.commit().await {
        reconcile_pointer(pool, &state.runtime, tenant, func).await;
        return Err(internal_error(e, "failed to commit publish transaction"));
    }
    Ok(())
}

/// After a failed commit: under the tenant lock, point `(tenant, func)` at
/// the digest its `functions` row holds, or remove the pointer if there is
/// no row. Failures are logged; there is nothing further to fall back on.
async fn reconcile_pointer(pool: &PgPool, runtime: &Runtime, tenant: &str, func: &str) {
    if let Err(e) = try_reconcile(pool, runtime, tenant, func).await {
        tracing::error!(%tenant, %func, error = %e, "could not reconcile pointer with database");
    }
}

async fn try_reconcile(
    pool: &PgPool,
    runtime: &Runtime,
    tenant: &str,
    func: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = pool.begin().await?;
    // Bound every wait so a DB partition cannot hang the request forever.
    sqlx::query("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    let hash: Option<String> = sqlx::query_scalar(
        "SELECT f.wasm_hash FROM warpline.functions f \
         JOIN warpline.tenants t ON t.id = f.tenant_id WHERE t.name = $1 AND f.name = $2",
    )
    .bind(tenant)
    .bind(func)
    .fetch_optional(&mut *tx)
    .await?;
    match hash {
        Some(h) => runtime.activate_digest(tenant, func, &h).await?,
        None => runtime.deactivate(tenant, func).await?,
    }
    // Hold the lock until the pointer is settled.
    tx.commit().await?;
    Ok(())
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
    // PATCH semantics: only the fields present in the body change. Range
    // checks run on the raw i64s so negatives get a proper 400;
    // `patch_tenant` validates again and stores hosts normalised.
    let mut patch = LimitsPatch::default();
    if let Some(ms) = cfg.cpu_budget_ms {
        if let Err(e) = validate_cpu_budget_ms(ms) {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
        patch.cpu_budget_ms = Some(ms as u64);
    }
    if let Some(b) = cfg.mem_cap_bytes {
        if let Err(e) = validate_mem_cap_bytes(b) {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
        patch.mem_cap_bytes = Some(b as usize);
    }
    patch.allowed_hosts = cfg.allowed_hosts;

    match pg::patch_tenant(pool, &tenant, &patch).await {
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
    use super::{constant_time_eq, reconcile_pointer};
    use warpline_core::{pg, Bytes, Runtime, RuntimeConfig};

    const GUEST: &[u8] = include_bytes!("../../core/tests/fixtures/test_guest.wasm");

    /// The commit-failure path: the pointer follows the `functions` row (the
    /// source of truth), not whatever was activated before the failure, and
    /// disappears when there is no row.
    #[tokio::test]
    async fn reconcile_pointer_follows_the_database() {
        let url = std::env::var("WARPLINE_TEST_DATABASE_URL").unwrap_or_default();
        if url.is_empty() {
            eprintln!("skipping: WARPLINE_TEST_DATABASE_URL not set");
            return;
        }
        let pool = pg::connect(&url).await.unwrap();
        pg::migrate(&pool).await.unwrap();
        let tenant = format!("t-recon-{}", std::process::id());
        pg::create_tenant(&pool, &tenant, None).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rt = Runtime::new(RuntimeConfig::new(dir.path())).unwrap();
        let committed = rt.stage(Bytes::from_static(GUEST)).await.unwrap();
        // A different component: "activated before the failed commit".
        let mut other = GUEST.to_vec();
        other.extend_from_slice(&[0, 3, 1, b'x', 0]); // custom section: new digest
        let uncommitted = rt.stage(Bytes::from(other)).await.unwrap();
        assert_ne!(committed, uncommitted);

        // No row yet: reconcile removes the stray pointer.
        rt.activate(&tenant, "f", &uncommitted).await.unwrap();
        reconcile_pointer(&pool, &rt, &tenant, "f").await;
        assert_eq!(rt.active(&tenant, "f").await.unwrap(), None);

        // A committed row: reconcile restores exactly that digest.
        sqlx::query(
            "INSERT INTO warpline.functions (tenant_id, name, wasm_hash) \
             SELECT id, 'f', $2 FROM warpline.tenants WHERE name = $1",
        )
        .bind(&tenant)
        .bind(&committed.digest)
        .execute(&pool)
        .await
        .unwrap();
        rt.activate(&tenant, "f", &uncommitted).await.unwrap();
        reconcile_pointer(&pool, &rt, &tenant, "f").await;
        assert_eq!(rt.active(&tenant, "f").await.unwrap(), Some(committed));
    }

    #[test]
    fn constant_time_eq_matches_regular_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
