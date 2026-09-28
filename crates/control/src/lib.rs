//! `warpline-control` — control plane HTTP API.
//!
//! - `POST /tenants/{tenant}/functions/{func}` accepts a multipart `.wasm`
//!   upload (field `wasm`/`module`/`file`), compiles it to `.cwasm` via
//!   `warpline_core::cache::load_or_compile` under a shared content-hash
//!   cache dir (`spawn_blocking`, since compilation is CPU-heavy), type-
//!   checks its imports/exports against the linker
//!   (`warpline_core::runtime::typecheck_component`) so a bad component is
//!   rejected here rather than at invoke time, writes the `(tenant, func)`
//!   pointer file (`warpline_core::registry::write_pointer`), and upserts
//!   the `functions` row when a database is configured.
//! - `POST /admin/tenants/{tenant}` (guarded by `WARPLINE_ADMIN_TOKEN`)
//!   creates a tenant idempotently, optionally sets its resource config,
//!   and issues a new API key.
//! - `GET /healthz`.
//!
//! Split into this lib (state + [`router`]) and a thin `main.rs` so
//! `crates/control/tests/` can drive the whole app through
//! `tower::ServiceExt::oneshot` without a real listening socket.

use std::path::PathBuf;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use wasmtime::component::Linker;
use wasmtime::Engine;

use warpline_core::{
    auth::{authenticate, parse_bearer, AuthOutcome, DbState},
    cache::{digest, load_or_compile},
    registry,
    runtime::typecheck_component,
    types::{
        valid_name, validate_allowed_hosts, validate_cpu_budget_ms, validate_mem_cap_bytes, HostCtx,
    },
};

/// Multipart upload size cap.
const UPLOAD_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub linker: Linker<HostCtx>,
    pub modules_dir: PathBuf,
    pub db: DbState,
    /// `WARPLINE_ADMIN_TOKEN`, or `None` if unset — the admin route is
    /// disabled (404) in that case.
    pub admin_token: Option<String>,
}

impl AppState {
    pub fn new(engine: Engine, linker: Linker<HostCtx>, modules_dir: PathBuf, db: DbState) -> Self {
        let admin_token = std::env::var("WARPLINE_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        Self {
            engine,
            linker,
            modules_dir,
            db,
            admin_token,
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

    let mut wasm_bytes: Option<Vec<u8>> = None;
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
                Ok(b) => wasm_bytes = Some(b.to_vec()),
                Err(e) => return (e.status(), format!("read field failed: {e}")).into_response(),
            }
        }
    }
    let Some(wasm) = wasm_bytes else {
        return (StatusCode::BAD_REQUEST, "missing wasm field".to_string()).into_response();
    };

    let cwasm_dir = registry::cwasm_dir(&state.modules_dir);
    let engine = state.engine.clone();
    let wasm_for_compile = wasm.clone();
    // Compilation is CPU-heavy (a full cranelift pass on a cache miss) —
    // keep it off the async runtime's worker threads.
    let compiled = tokio::task::spawn_blocking(move || {
        load_or_compile(&engine, &wasm_for_compile, &cwasm_dir)
    })
    .await;
    let component = match compiled {
        Ok(Ok(c)) => c,
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

    let wasm_digest = digest(&wasm);
    if let Err(e) = registry::write_pointer(&state.modules_dir, &tenant, &func, &wasm_digest) {
        tracing::error!(%tenant, %func, error = %e, "failed to write pointer");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to publish module".to_string(),
        )
            .into_response();
    }

    if let (DbState::Postgres(pool), Some(tenant_id)) = (&state.db, authed.id) {
        if let Err(e) = sqlx::query(
            "INSERT INTO functions (tenant_id, name, wasm_hash) VALUES ($1, $2, $3) \
             ON CONFLICT (tenant_id, name) DO UPDATE SET wasm_hash = EXCLUDED.wasm_hash",
        )
        .bind(tenant_id)
        .bind(&func)
        .bind(&wasm_digest)
        .execute(pool)
        .await
        {
            tracing::warn!(%tenant, %func, error = %e, "failed to upsert functions row");
        }
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
    if let Some(hosts) = &cfg.allowed_hosts {
        if let Err(e) = validate_allowed_hosts(hosts) {
            return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
        }
    }
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

    // Idempotent create: `RETURNING id` fires whether this insert created
    // the row or the ON CONFLICT arm did.
    let tenant_id: uuid::Uuid = match sqlx::query_scalar(
        "INSERT INTO tenants (name) VALUES ($1) \
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name \
         RETURNING id",
    )
    .bind(&tenant)
    .fetch_one(pool)
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(%tenant, error = %e, "failed to upsert tenant");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
                .into_response();
        }
    };

    if let Err(e) = sqlx::query(
        "UPDATE tenants SET \
            allowed_hosts = COALESCE($2, allowed_hosts), \
            cpu_budget_ms = COALESCE($3, cpu_budget_ms), \
            mem_cap_bytes = COALESCE($4, mem_cap_bytes) \
         WHERE id = $1",
    )
    .bind(tenant_id)
    .bind(cfg.allowed_hosts)
    .bind(cfg.cpu_budget_ms.map(|v| v as i32))
    .bind(cfg.mem_cap_bytes)
    .execute(pool)
    .await
    {
        tracing::error!(%tenant, error = %e, "failed to update tenant config");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
            .into_response();
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

    if let Err(e) = sqlx::query("INSERT INTO api_keys (key_hash, tenant_id) VALUES ($1, $2)")
        .bind(&key_hash)
        .bind(tenant_id)
        .execute(pool)
        .await
    {
        tracing::error!(%tenant, error = %e, "failed to insert api key");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
            .into_response();
    }

    tracing::info!(%tenant, "tenant created/updated, api key issued");
    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({ "tenant": tenant, "api_key": api_key })),
    )
        .into_response()
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
