//! `warpline-control` — control plane HTTP API.
//!
//! - `POST /tenants/{tenant}/functions/{func}` accepts a multipart `.wasm`
//!   upload (field `wasm`/`module`/`file`), compiles it (`spawn_blocking`,
//!   under a process-wide semaphore — compilation is CPU-heavy and
//!   otherwise unbounded concurrency here is a DoS vector), type-checks its
//!   imports/exports against the linker
//!   (`warpline_core::runtime::typecheck_component`) so a bad component is
//!   rejected here rather than at invoke time, and only *then* persists the
//!   source `.wasm` + compiled `.cwasm` and publishes the `(tenant, func)`
//!   pointer — in DB mode, inside one transaction that also enforces the
//!   per-tenant function quota (see [`publish_pointer`]).
//! - `POST /admin/tenants/{tenant}` (guarded by `WARPLINE_ADMIN_TOKEN`)
//!   creates a tenant idempotently, optionally sets its resource config,
//!   and issues a new API key — tenant upsert, config update and key insert
//!   all happen in one transaction.
//! - `GET /healthz`.
//!
//! Split into this lib (state + [`router`]) and a thin `main.rs` so
//! `crates/control/tests/` can drive the whole app through
//! `tower::ServiceExt::oneshot` without a real listening socket.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tokio::sync::Semaphore;
use wasmtime::component::{Component, Linker};
use wasmtime::Engine;

use warpline_core::{
    auth::{authenticate, parse_bearer, AuthOutcome, DbState},
    cache::{self, digest},
    registry,
    runtime::typecheck_component,
    types::{
        valid_name, validate_allowed_hosts, validate_cpu_budget_ms, validate_mem_cap_bytes, HostCtx,
    },
};

/// Multipart upload size cap.
const UPLOAD_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Default per-tenant cap on distinct function names (finding 3) — also
/// bounds the cardinality of the `func` label a tenant can push into
/// metrics/logs. A field on [`AppState`] rather than a bare constant so
/// tests can override it to a small number and exercise the quota boundary
/// without uploading 100 real functions.
const DEFAULT_MAX_FUNCTIONS_PER_TENANT: i64 = 100;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub linker: Linker<HostCtx>,
    pub modules_dir: PathBuf,
    pub db: DbState,
    /// `WARPLINE_ADMIN_TOKEN`, or `None` if unset — the admin route is
    /// disabled (404) in that case.
    pub admin_token: Option<String>,
    /// Bounds concurrent `spawn_blocking` compiles process-wide (finding
    /// 2) — a burst of uploads shouldn't be able to spin up an unbounded
    /// number of full cranelift passes at once.
    pub compile_semaphore: Arc<Semaphore>,
    /// Per-tenant cap on distinct function names, see
    /// [`DEFAULT_MAX_FUNCTIONS_PER_TENANT`]. A field (not that constant
    /// directly) so tests can dial it down and exercise the quota boundary
    /// cheaply.
    pub max_functions_per_tenant: i64,
}

impl AppState {
    pub fn new(engine: Engine, linker: Linker<HostCtx>, modules_dir: PathBuf, db: DbState) -> Self {
        let admin_token = std::env::var("WARPLINE_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let permits = (std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            / 2)
        .max(1);
        Self {
            engine,
            linker,
            modules_dir,
            db,
            admin_token,
            compile_semaphore: Arc::new(Semaphore::new(permits)),
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

    let authed = match authenticate(&state.db, &tenant, bearer_from(&headers)).await {
        Ok(AuthOutcome::Ok(a)) => a,
        Ok(AuthOutcome::Unauthorized) => {
            return (StatusCode::UNAUTHORIZED, "unauthorized".to_string()).into_response()
        }
        Ok(AuthOutcome::Forbidden) => {
            return (StatusCode::FORBIDDEN, "forbidden".to_string()).into_response()
        }
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

    // Bound concurrent compiles process-wide (finding 2): held across the
    // whole `spawn_blocking` call below, not just while acquiring it.
    let Ok(permit) = state.compile_semaphore.clone().acquire_owned().await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
            .into_response();
    };

    let cwasm_dir = registry::cwasm_dir(&state.modules_dir);
    let engine = state.engine.clone();
    let wasm_for_compile = wasm.clone();
    let compiled = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        cache::compile(&engine, &wasm_for_compile, &cwasm_dir)
    })
    .await;
    let (component, wasm_digest): (Component, String) = match compiled {
        // The freshness bool isn't needed here — the component always gets
        // persisted below, unconditionally, once it's passed the typecheck.
        Ok(Ok((component, digest, _freshly_compiled))) => (component, digest),
        Ok(Err(e)) => {
            tracing::warn!(%tenant, %func, error = %e, "component compile/parse failed");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid wasm component".to_string(),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(%tenant, %func, error = %e, "compile task panicked");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
                .into_response();
        }
    };

    if let Err(e) = typecheck_component(&state.linker, &component) {
        tracing::warn!(%tenant, %func, error = %e, "component failed import/export typecheck");
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "component does not match the warpline handler world (missing export or \
             unsatisfiable import)"
                .to_string(),
        )
            .into_response();
    }

    // Only a typechecked component's bytes get persisted (finding 3): an
    // upload that compiles but fails the typecheck never leaves a
    // wasm/cwasm blob behind for the GC pass to have to clean up later.
    let cwasm_dir = registry::cwasm_dir(&state.modules_dir);
    let modules_dir = state.modules_dir.clone();
    let engine = state.engine.clone();
    let wasm_for_persist = wasm.clone();
    let digest_for_persist = wasm_digest.clone();
    let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        cache::persist_cwasm(&component, &engine, &cwasm_dir, &digest_for_persist)?;
        registry::write_wasm_source(&modules_dir, &digest_for_persist, &wasm_for_persist)?;
        Ok(())
    })
    .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!(%tenant, %func, error = %e, "failed to persist compiled module");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to publish module".to_string(),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(%tenant, %func, error = %e, "persist task panicked");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
                .into_response();
        }
    }

    if let Err((status, msg)) =
        publish_pointer(&state, &tenant, &func, authed.id, &wasm_digest).await
    {
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
/// In `InsecureDev` (no database, no tenant id), there is nothing to lock
/// or upsert against, so this just writes the pointer.
async fn publish_pointer(
    state: &AppState,
    tenant: &str,
    func: &str,
    tenant_id: Option<uuid::Uuid>,
    wasm_digest: &str,
) -> Result<(), (StatusCode, String)> {
    let (DbState::Postgres(pool), Some(tenant_id)) = (&state.db, tenant_id) else {
        return registry::write_pointer(&state.modules_dir, tenant, func, wasm_digest).map_err(
            |e| {
                tracing::error!(%tenant, %func, error = %e, "failed to write pointer");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to publish module".to_string(),
                )
            },
        );
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

    let max_functions = state.max_functions_per_tenant;
    let other_functions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM functions WHERE tenant_id = $1 AND name <> $2")
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
        "INSERT INTO functions (tenant_id, name, wasm_hash) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, name) DO UPDATE SET wasm_hash = EXCLUDED.wasm_hash",
    )
    .bind(tenant_id)
    .bind(func)
    .bind(wasm_digest)
    .execute(&mut *tx)
    .await
    .map_err(|e| internal_error(e, "failed to upsert functions row"))?;

    tx.commit()
        .await
        .map_err(|e| internal_error(e, "failed to commit publish transaction"))?;

    // Written after the commit, not before (finding 6) — see doc comment
    // above for why that ordering is the one that can't leave a pointer
    // serving code the `functions` table doesn't know about.
    registry::write_pointer(&state.modules_dir, tenant, func, wasm_digest).map_err(|e| {
        tracing::error!(%tenant, %func, error = %e, "failed to write pointer");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to publish module".to_string(),
        )
    })?;

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
        // No WARPLINE_ADMIN_TOKEN configured: the route doesn't exist.
        return StatusCode::NOT_FOUND.into_response();
    };
    let provided = bearer_from(&headers);
    if !provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected_token.as_bytes())) {
        return (StatusCode::UNAUTHORIZED, "unauthorized".to_string()).into_response();
    }
    if !valid_name(&tenant) {
        return (StatusCode::BAD_REQUEST, "invalid tenant name".to_string()).into_response();
    }

    let DbState::Postgres(pool) = &state.db else {
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
    // Normalized (lowercased, trailing-dot-trimmed) form is what gets
    // stored — see `validate_allowed_hosts` (finding 9): storing the raw
    // input would let an allowlist entry silently never match the
    // already-normalized host `http-out::fetch` compares it against.
    let normalized_hosts: Option<Vec<String>> = match &cfg.allowed_hosts {
        Some(hosts) => match validate_allowed_hosts(hosts) {
            Ok(v) => Some(v),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
        None => None,
    };
    if let Some(ms) = cfg.cpu_budget_ms {
        if let Err(e) = validate_cpu_budget_ms(ms) {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    }
    if let Some(b) = cfg.mem_cap_bytes {
        if let Err(e) = validate_mem_cap_bytes(b) {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    }

    let mut key_bytes = [0u8; 32];
    if let Err(e) = getrandom::fill(&mut key_bytes) {
        tracing::error!(error = %e, "getrandom failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
            .into_response();
    }
    let api_key = format!("wl_{}", hex::encode(key_bytes));
    let key_hash = digest(api_key.as_bytes());

    // Tenant upsert + config update + key insert as one unit (finding 11):
    // a failure partway through must not leave e.g. a tenant row with no
    // usable key, or a key issued against a config update that never
    // landed.
    let result: Result<(), sqlx::Error> = async {
        let mut tx = pool.begin().await?;
        let tenant_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO tenants (name) VALUES ($1) \
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id",
        )
        .bind(&tenant)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE tenants SET \
                allowed_hosts = COALESCE($2, allowed_hosts), \
                cpu_budget_ms = COALESCE($3, cpu_budget_ms), \
                mem_cap_bytes = COALESCE($4, mem_cap_bytes) \
             WHERE id = $1",
        )
        .bind(tenant_id)
        .bind(&normalized_hosts)
        .bind(cfg.cpu_budget_ms.map(|v| v as i32))
        .bind(cfg.mem_cap_bytes)
        .execute(&mut *tx)
        .await?;

        sqlx::query("INSERT INTO api_keys (key_hash, tenant_id) VALUES ($1, $2)")
            .bind(&key_hash)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await
    }
    .await;

    match result {
        Ok(()) => {
            tracing::info!(%tenant, "tenant created/updated, api key issued");
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({ "tenant": tenant, "api_key": api_key })),
            )
                .into_response()
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
