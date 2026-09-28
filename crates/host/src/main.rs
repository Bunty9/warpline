//! `warpline-host` binary entry point. All routing and business logic
//! lives in `warpline_host` (this crate's lib target) so it can be driven
//! from tests via `tower::ServiceExt::oneshot` without a real socket.

use std::path::PathBuf;
use std::sync::Arc;

use warpline_core::{
    auth::DbState,
    kv::MemKv,
    registry::ComponentCache,
    runtime::{build_engine, build_http_client, build_linker, EpochTicker},
};
use warpline_host::{metrics_handle, router, shutdown_signal, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let db = DbState::connect().await?;

    let engine = build_engine()?;
    let linker = build_linker(&engine)?;
    let ticker = EpochTicker::spawn(engine.clone());
    // Deny-by-default: outbound HTTP refuses loopback/private/link-local/etc.
    // targets unless explicitly opted into (e.g. local dev against a
    // sidecar). See `warpline_core::runtime::is_blocked_ip`.
    let allow_private_egress = std::env::var("WARPLINE_ALLOW_PRIVATE_EGRESS")
        .map(|v| v == "1")
        .unwrap_or(false);
    let http_client = build_http_client(allow_private_egress)?;
    let modules_dir = std::env::var("WARPLINE_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./modules"));

    let state = AppState {
        engine,
        linker,
        modules_dir,
        component_cache: Arc::new(ComponentCache::new()),
        kv: Arc::new(MemKv::new()),
        http_client,
        allow_private_egress,
        db,
        metrics_handle: metrics_handle(),
        ticker: Arc::new(ticker),
    };

    let app = router(state);

    let bind = std::env::var("WARPLINE_HOST_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    tracing::info!(%bind, "warpline-host starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}
