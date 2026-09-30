//! Config and wiring. The only file that reads environment variables:
//! warpline-core never does, the embedding app decides.

use std::time::Duration;

use anyhow::{bail, Context};
use storefront::{build_runtime, router, AppState};
use warpline_core::pg;

fn env(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => bail!("{name} must be set"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url = env("DATABASE_URL")?;
    let admin_token = env("ADMIN_TOKEN")?;
    let fraud_api = env("FRAUD_API")?;
    let modules_dir = std::env::var("MODULES_DIR").unwrap_or_else(|_| "./modules".into());
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());

    // [warpline 2] The app owns the pool and decides when to migrate.
    // `migrate` only touches the `warpline` schema, so it can share a
    // database with the app's own tables.
    let pool = pg::connect(&database_url)
        .await
        .context("connecting to postgres")?;
    pg::migrate(&pool).await.context("migrating")?;

    let (runtime, meter) = build_runtime(&modules_dir, &pool).await?;
    let app = router(AppState::new(runtime, pool, &admin_token, &fraud_api));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, %fraud_api, "storefront listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // [warpline 9] In-flight requests have finished; flush queued meter rows
    // before exiting, or the last invocations would never be billed.
    meter.shutdown(Duration::from_secs(5)).await;
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (what `docker stop` sends).
async fn shutdown_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}
