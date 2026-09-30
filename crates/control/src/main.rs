//! `warpline-control` binary entry point. All routing and business logic
//! lives in `warpline_control` (this crate's lib target) so it can be
//! driven from tests via `tower::ServiceExt::oneshot` without a real
//! socket.

use std::path::PathBuf;
use std::time::Duration;

use warpline_control::{router, AppState};
use warpline_core::{
    pg::{self, Authenticator},
    Runtime, RuntimeConfig, GC_GRACE_PERIOD,
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

    let auth = connect_auth().await?;
    let insecure_dev = auth.is_none();
    let modules_dir = env_nonempty("WARPLINE_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./modules"));
    let runtime = Runtime::new(RuntimeConfig::new(modules_dir))?;

    // Run once, before serving: clean up any wasm/cwasm blob no pointer
    // references (finding 3) — e.g. left behind by a crash between
    // persisting a blob and writing its pointer.
    match runtime.gc(GC_GRACE_PERIOD).await {
        Ok(removed) => tracing::info!(removed, "startup GC: removed unreferenced module blobs"),
        Err(e) => tracing::warn!(error = %e, "startup GC failed"),
    }

    let mut state = AppState::new(runtime, auth);
    state.admin_token = env_nonempty("WARPLINE_ADMIN_TOKEN");
    let app = router(state);

    // InsecureDev has no auth in front of it; don't default to a
    // publicly-reachable bind in that mode (finding 8).
    let default_bind = if insecure_dev {
        "127.0.0.1:8081"
    } else {
        "0.0.0.0:8081"
    };
    let bind = env_nonempty("WARPLINE_CONTROL_BIND").unwrap_or_else(|| default_bind.to_string());
    tracing::info!(%bind, "warpline-control starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// An environment variable, treating empty as unset.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `DATABASE_URL` set: connect, migrate and build the authenticator.
/// Unset: only allowed with `WARPLINE_INSECURE_DEV=1` (returns `None`).
async fn connect_auth() -> anyhow::Result<Option<Authenticator>> {
    let Some(url) = env_nonempty("DATABASE_URL") else {
        anyhow::ensure!(
            env_nonempty("WARPLINE_INSECURE_DEV").as_deref() == Some("1"),
            "DATABASE_URL is not set. Refusing to start without a database \
             (no auth would be enforced and no invocations would be metered). \
             Set WARPLINE_INSECURE_DEV=1 to run without one in local dev."
        );
        tracing::warn!(
            "WARPLINE_INSECURE_DEV=1: starting without Postgres — no auth is \
             enforced and every tenant gets the default resource caps. Do not \
             run this in production."
        );
        return Ok(None);
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        // Fail fast rather than queueing every request behind a saturated pool.
        .acquire_timeout(Duration::from_secs(2))
        .connect(&url)
        .await?;
    pg::migrate(&pool).await?;
    let ttl = env_nonempty("WARPLINE_AUTH_CACHE_TTL_SECS")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    Ok(Some(Authenticator::new(pool, Duration::from_secs(ttl))))
}
