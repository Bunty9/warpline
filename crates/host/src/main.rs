//! `warpline-host` binary entry point. All routing and business logic
//! lives in `warpline_host` (this crate's lib target) so it can be driven
//! from tests via `tower::ServiceExt::oneshot` without a real socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use warpline_core::{
    auth::DbState,
    kv::MemKv,
    meter,
    registry::ComponentCache,
    runtime::{build_engine, build_http_client, build_linker, EpochTicker},
};
use warpline_host::{metrics_handle, metrics_router, router, shutdown_signal, AppState};

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
    let insecure_dev = matches!(db, DbState::InsecureDev);

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

    let (meter_tx, meter_handle) = meter::spawn_writer(db.clone(), meter::METER_CHANNEL_CAPACITY);
    let metrics_handle = metrics_handle();

    let state = AppState {
        engine,
        linker,
        modules_dir,
        component_cache: Arc::new(ComponentCache::new()),
        kv: Arc::new(MemKv::new()),
        http_client,
        allow_private_egress,
        db,
        metrics_handle: metrics_handle.clone(),
        ticker: Arc::new(ticker),
        meter_tx: meter_tx.clone(),
    };

    // `/metrics` on its own, loopback-only-by-default listener (finding 6)
    // — it's never on the router handed to `axum::serve` below.
    let metrics_bind =
        std::env::var("WARPLINE_METRICS_BIND").unwrap_or_else(|_| "127.0.0.1:9090".to_string());
    let metrics_listener = tokio::net::TcpListener::bind(&metrics_bind).await?;
    let metrics_app = metrics_router(metrics_handle);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(metrics_listener, metrics_app).await {
            tracing::error!(error = %e, "metrics listener failed");
        }
    });

    let app = router(state);

    // InsecureDev has no auth in front of it; don't default to a
    // publicly-reachable bind in that mode (finding 8).
    let default_bind = if insecure_dev {
        "127.0.0.1:8080"
    } else {
        "0.0.0.0:8080"
    };
    let bind = std::env::var("WARPLINE_HOST_BIND").unwrap_or_else(|_| default_bind.to_string());
    tracing::info!(%bind, %metrics_bind, "warpline-host starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Drain the metering channel (finding 5): drop this process's own
    // sender clone (every handler's clone should already be gone —
    // graceful shutdown waited for in-flight requests to finish) so the
    // writer's `recv` loop sees the channel close, then give it a bounded
    // window to flush whatever's still buffered.
    drop(meter_tx);
    if tokio::time::timeout(Duration::from_secs(5), meter_handle)
        .await
        .is_err()
    {
        tracing::warn!("meter writer did not drain within timeout");
    }
    Ok(())
}
