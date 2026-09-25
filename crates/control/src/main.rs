//! `warpline-control` — control plane HTTP API.
//!
//! Phase 1 surface:
//! - `POST /tenants/{tenant}/functions/{func}` accepts a multipart `.wasm`
//!   upload, hashes it, runs it through `warpline_core::cache::load_or_compile`,
//!   and persists the resulting `.cwasm` under `./modules/{tenant}/{func}/`.
//!
//! Out of scope for Phase 1: tenant auth, per-function quota enforcement,
//! upload size limits, S3-backed cache. The Phase-1 store is the local
//! filesystem — the spec calls for swapping in S3/MinIO once the host /
//! control split lives across multiple machines.

use std::path::PathBuf;

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use wasmtime::Engine;

use warpline_core::{cache::load_or_compile, runtime::build_engine};

#[derive(Clone)]
struct AppState {
    engine: Engine,
    modules_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let engine = build_engine()?;
    let modules_dir = std::env::var("WARPLINE_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./modules"));
    std::fs::create_dir_all(&modules_dir)?;

    let state = AppState {
        engine,
        modules_dir,
    };

    let app = Router::new()
        .route("/tenants/{tenant}/functions/{func}", post(upload))
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .with_state(state);

    let bind =
        std::env::var("WARPLINE_CONTROL_BIND").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
    tracing::info!(%bind, "warpline-control starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn upload(
    State(state): State<AppState>,
    Path((tenant, func)): Path<(String, String)>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut wasm_bytes: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "wasm" || name == "module" || name == "file" {
            match field.bytes().await {
                Ok(b) => wasm_bytes = Some(b.to_vec()),
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, format!("read field failed: {e}"))
                        .into_response()
                }
            }
        }
    }
    let Some(wasm) = wasm_bytes else {
        return (StatusCode::BAD_REQUEST, "missing wasm field").into_response();
    };

    let cache_dir = state.modules_dir.join(&tenant).join(&func);
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir failed: {e}"),
        )
            .into_response();
    }

    match load_or_compile(&state.engine, &wasm, &cache_dir) {
        Ok(_module) => {
            tracing::info!(%tenant, %func, "module compiled and cached");
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({
                    "tenant": tenant,
                    "func": func,
                    "status": "compiled",
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile failed: {e}"),
        )
            .into_response(),
    }
}
