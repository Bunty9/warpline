//! `warpline-control` binary entry point. All routing and business logic
//! lives in `warpline_control` (this crate's lib target) so it can be
//! driven from tests via `tower::ServiceExt::oneshot` without a real
//! socket.

use std::path::PathBuf;

use warpline_control::{router, AppState};
use warpline_core::{
    auth::DbState,
    registry,
    runtime::{build_engine, build_linker},
};

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
    let modules_dir = std::env::var("WARPLINE_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./modules"));
    std::fs::create_dir_all(&modules_dir)?;

    // Run once, before serving: clean up any wasm/cwasm blob no pointer
    // references (finding 3) — e.g. left behind by a crash between
    // persisting a blob and writing its pointer.
    match registry::gc_unreferenced_blobs(&modules_dir, &engine, registry::GC_GRACE_PERIOD) {
        Ok(removed) => tracing::info!(removed, "startup GC: removed unreferenced module blobs"),
        Err(e) => tracing::warn!(error = %e, "startup GC failed"),
    }

    let state = AppState::new(engine, linker, modules_dir, db);
    let app = router(state);

    // InsecureDev has no auth in front of it; don't default to a
    // publicly-reachable bind in that mode (finding 8).
    let default_bind = if insecure_dev {
        "127.0.0.1:8081"
    } else {
        "0.0.0.0:8081"
    };
    let bind = std::env::var("WARPLINE_CONTROL_BIND").unwrap_or_else(|_| default_bind.to_string());
    tracing::info!(%bind, "warpline-control starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
