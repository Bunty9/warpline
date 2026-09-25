//! `warpline-host` — multi-tenant WASM function invocation server.
//!
//! Listens on `:8080` for `POST /tenants/{tenant}/functions/{fn}/invoke`.
//! Looks the (tenant, fn) tuple up in an in-process module registry
//! (stub-populated for the Phase-1/2 demo — the content-hash pointer-file
//! registry described in the Phase-2 plan is Task 2), invokes the
//! component via `warpline_core::runtime::invoke`, writes a metering row,
//! and returns the guest's raw response bytes.
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
use wasmtime::component::{Component, Linker};
use wasmtime::Engine;

use warpline_core::{
    kv::{KvStore, MemKv},
    runtime::{build_engine, build_http_client, build_linker, invoke, EpochTicker, InvokeError},
    types::HostCtx,
};

/// Key into the in-process module registry. (tenant, fn-name) -> compiled
/// component. Task 2 replaces this with a content-addressed `.cwasm`
/// loader that pulls from the control-plane's cache directory on demand;
/// today everything the process has seen just stays resident.
type ModuleKey = (String, String);

/// Process-wide state shared across axum handlers.
#[derive(Clone)]
struct AppState {
    engine: Engine,
    linker: Linker<HostCtx>,
    modules: Arc<RwLock<HashMap<ModuleKey, Arc<Component>>>>,
    kv: Arc<dyn KvStore>,
    http_client: reqwest::Client,
    /// Keeps the engine's epoch-ticker thread alive for the process
    /// lifetime; never read, only held.
    _ticker: Arc<EpochTicker>,
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
    let linker = build_linker(&engine)?;
    let ticker = EpochTicker::spawn(engine.clone());
    let http_client = build_http_client()?;

    let state = AppState {
        engine,
        linker,
        modules: Arc::new(RwLock::new(HashMap::new())),
        kv: Arc::new(MemKv::new()),
        http_client,
        _ticker: Arc::new(ticker),
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
    let component = {
        let guard = state.modules.read().await;
        match guard.get(&key) {
            Some(m) => m.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    format!("no module for {tenant}/{func}"),
                )
                    .into_response()
            }
        }
    };

    let ctx = HostCtx::new(
        tenant.clone(),
        func.clone(),
        state.kv.clone(),
        Vec::new(),
        state.http_client.clone(),
        64 * 1024 * 1024,
    );

    let started = std::time::Instant::now();
    match invoke(
        &state.engine,
        &state.linker,
        &component,
        ctx,
        body.to_vec(),
        100,
    )
    .await
    {
        Ok(outcome) => {
            let elapsed_us = started.elapsed().as_micros() as u64;
            metrics::histogram!(
                "warpline_invoke_duration_us",
                "tenant" => tenant,
                "func" => func
            )
            .record(elapsed_us as f64);
            (StatusCode::OK, outcome.output).into_response()
        }
        Err(e) => {
            // Budget/cap/timeout failures are the guest's own doing —
            // 408/413-ish; anything else is a host-side trap or bug.
            let status = match e {
                InvokeError::CpuBudgetExceeded { .. }
                | InvokeError::MemoryCapExceeded { .. }
                | InvokeError::WallClockTimeout(_) => StatusCode::REQUEST_TIMEOUT,
                InvokeError::GuestTrap(_) => StatusCode::UNPROCESSABLE_ENTITY,
                InvokeError::Instantiate(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, format!("invoke failed: {e}")).into_response()
        }
    }
}
