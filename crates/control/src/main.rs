//! `warpline-control` binary entry point. All routing and business logic
//! lives in `warpline_control` (this crate's lib target) so it can be
//! driven from tests via `tower::ServiceExt::oneshot` without a real
//! socket.

use std::path::PathBuf;

use warpline_control::{router, AppState};
use warpline_core::{
    auth::DbState,
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
    let engine = build_engine()?;
    let linker = build_linker(&engine)?;
    let modules_dir = std::env::var("WARPLINE_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./modules"));
    std::fs::create_dir_all(&modules_dir)?;

    let state = AppState::new(engine, linker, modules_dir, db);
    let app = router(state);

    let bind =
        std::env::var("WARPLINE_CONTROL_BIND").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
    tracing::info!(%bind, "warpline-control starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
