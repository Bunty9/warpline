//! `warpline-host` — multi-tenant WASM function invocation server.
//!
//! Listens on `:8080` for `POST /tenants/{tenant}/functions/{fn}/invoke`.
//! Looks the (tenant, fn) tuple up in an in-process module registry
//! (stub-populated for the Phase-1 demo), invokes the module via
//! `warpline_core::runtime::invoke`, writes a metering row, and returns the
//! guest's raw response bytes.
//!
//! The control-plane lives in `warpline-control` (port :8081) — uploads
//! land there, are compiled to `.cwasm`, and the host loads them by content
//! hash when invoked.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
    Router,
};
use tokio::sync::RwLock;
use wasmtime::{Engine, Module};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::WasiCtxBuilder;

use warpline_core::{
    kv::MemKv,
    runtime::{build_engine, invoke},
    types::HostCtx,
};

/// Key into the in-process module registry. (tenant, fn-name) -> compiled
/// module. In Phase 2 this becomes a content-addressed `.cwasm` loader that
/// pulls from the control-plane's cache directory on demand; Phase 1 just
/// keeps everything resident.
type ModuleKey = (String, String);

/// Process-wide state shared across axum handlers.
#[derive(Clone)]
struct AppState {
    engine: Engine,
    modules: Arc<RwLock<HashMap<ModuleKey, Arc<Module>>>>,
    kv: Arc<MemKv>,
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
    let state = AppState {
        engine,
        modules: Arc::new(RwLock::new(HashMap::new())),
        kv: Arc::new(MemKv::new()),
    };

    let app = Router::new()
        .route(
            "/tenants/{tenant}/functions/{func}/invoke",
            post(invoke_handler),
        )
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .with_state(state);

    let bind = std::env::var("WARPLINE_HOST_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    tracing::info!(%bind, "warpline-host starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn invoke_handler(
    State(state): State<AppState>,
    Path((tenant, func)): Path<(String, String)>,
    body: Bytes,
) -> impl IntoResponse {
    let key = (tenant.clone(), func.clone());
    let module = {
        let guard = state.modules.read().await;
        match guard.get(&key) {
            Some(m) => m.clone(),
            None => {
                return (StatusCode::NOT_FOUND, format!("no module for {tenant}/{func}"))
                    .into_response()
            }
        }
    };

    let wasi: WasiP1Ctx = WasiCtxBuilder::new().build_p1();
    let ctx = HostCtx {
        tenant_id: tenant.clone(),
        fn_name: func.clone(),
        kv: state.kv.clone(),
        allowed_hosts: Vec::new(),
        mem_cap_bytes: 64 * 1024 * 1024,
        wasi,
    };

    let started = std::time::Instant::now();
    match invoke(&state.engine, &module, ctx, body.to_vec(), 100).await {
        Ok(out) => {
            let elapsed_us = started.elapsed().as_micros() as u64;
            metrics::histogram!(
                "warpline_invoke_duration_us",
                "tenant" => tenant,
                "func" => func
            )
            .record(elapsed_us as f64);
            (StatusCode::OK, out).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("invoke failed: {e}"))
            .into_response(),
    }
}
