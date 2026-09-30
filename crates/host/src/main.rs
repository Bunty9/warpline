//! `warpline-host` binary entry point. All routing and business logic
//! lives in `warpline_host` (this crate's lib target) so it can be driven
//! from tests via `tower::ServiceExt::oneshot` without a real socket.
//!
//! Environment handling lives only here; empty variables count as unset.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use warpline_core::{
    pg::{self, Authenticator, PgMeter},
    MeterSink, Runtime, RuntimeConfig,
};
use warpline_host::{metrics_handle, metrics_router, router, shutdown_signal, AppState, LogMeter};

/// Rows the meter queue holds before it starts dropping (and counting).
const METER_CHANNEL_CAPACITY: usize = 10_000;

/// How long shutdown waits for the embedded control plane to finish.
#[cfg(feature = "embed-control")]
const CONTROL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    // Before anything records a metric (including the meter export below).
    let metrics_handle = metrics_handle();

    let auth = connect_auth().await?;
    let insecure_dev = auth.is_none();

    // Deny-by-default: outbound HTTP refuses loopback/private/link-local/etc.
    // targets unless explicitly opted into (e.g. local dev against a
    // sidecar).
    let mut cfg = RuntimeConfig::new(
        env_nonempty("WARPLINE_MODULES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./modules")),
    );
    cfg.allow_private_egress =
        env_nonempty("WARPLINE_ALLOW_PRIVATE_EGRESS").is_some_and(|v| v == "1");

    let (meter, meter_handle): (Arc<dyn MeterSink>, _) = match &auth {
        Some(a) => {
            let (m, h) = PgMeter::spawn(a.pool().clone(), METER_CHANNEL_CAPACITY);
            let m = Arc::new(m);
            // Export the meter's drop count as a Prometheus counter.
            let exported = m.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tick.tick().await;
                    metrics::counter!("warpline_meter_dropped_total").absolute(exported.dropped());
                }
            });
            (m, Some(h))
        }
        None => (Arc::new(LogMeter), None),
    };

    let runtime = Runtime::builder(cfg).meter(meter).build()?;
    let state = AppState {
        runtime,
        auth,
        metrics_handle: metrics_handle.clone(),
    };

    // `/metrics` on its own, loopback-only-by-default listener (finding 6)
    // — it's never on the router handed to `axum::serve` below.
    let metrics_bind =
        env_nonempty("WARPLINE_METRICS_BIND").unwrap_or_else(|| "127.0.0.1:9090".to_string());
    let metrics_listener = tokio::net::TcpListener::bind(&metrics_bind).await?;
    let metrics_app = metrics_router(metrics_handle);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(metrics_listener, metrics_app).await {
            tracing::error!(error = %e, "metrics listener failed");
        }
    });

    let app = router(state.clone());

    // Single-machine deploys (Fly volumes attach to one machine) can run
    // the control plane in this process, sharing the engine so both sides
    // agree on the `.cwasm` compatibility hash.
    let embed = env_nonempty("WARPLINE_EMBED_CONTROL").is_some_and(|v| v == "1");
    #[cfg(feature = "embed-control")]
    let control_task = if embed {
        Some(spawn_embedded_control(&state, insecure_dev).await?)
    } else {
        None
    };
    #[cfg(not(feature = "embed-control"))]
    if embed {
        tracing::warn!("WARPLINE_EMBED_CONTROL=1 ignored: built without the embed-control feature");
    }

    // InsecureDev has no auth in front of it; don't default to a
    // publicly-reachable bind in that mode (finding 8).
    let default_bind = if insecure_dev {
        "127.0.0.1:8080"
    } else {
        "0.0.0.0:8080"
    };
    let bind = env_nonempty("WARPLINE_HOST_BIND").unwrap_or_else(|| default_bind.to_string());
    tracing::info!(%bind, %metrics_bind, "warpline-host starting");
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // The control task got the same signal; give it a bounded time to drain
    // (an upload may be mid-flight) rather than dropping it with the runtime.
    #[cfg(feature = "embed-control")]
    if let Some(task) = control_task {
        if tokio::time::timeout(CONTROL_SHUTDOWN_TIMEOUT, task)
            .await
            .is_err()
        {
            tracing::warn!("embedded control did not stop within {CONTROL_SHUTDOWN_TIMEOUT:?}");
        }
    }

    // Graceful shutdown waited for in-flight requests, so everything they
    // recorded is queued: flush it, bounded.
    if let Some(h) = meter_handle {
        h.shutdown(Duration::from_secs(5)).await;
    }
    Ok(())
}

/// Run the control plane on `WARPLINE_CONTROL_BIND`, sharing `state`'s
/// runtime. The returned task ends when the shutdown signal arrives and
/// in-flight requests finish.
#[cfg(feature = "embed-control")]
async fn spawn_embedded_control(
    state: &AppState,
    insecure_dev: bool,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    match state.runtime.gc(warpline_core::GC_GRACE_PERIOD).await {
        Ok(removed) => tracing::info!(removed, "startup GC: removed unreferenced module blobs"),
        Err(e) => tracing::warn!(error = %e, "startup GC failed"),
    }
    let mut control_state =
        warpline_control::AppState::new(state.runtime.clone(), state.auth.clone());
    control_state.admin_token = env_nonempty("WARPLINE_ADMIN_TOKEN");
    let default_bind = if insecure_dev {
        "127.0.0.1:8081"
    } else {
        "0.0.0.0:8081"
    };
    let bind = env_nonempty("WARPLINE_CONTROL_BIND").unwrap_or_else(|| default_bind.to_string());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(control_bind = %bind, "embedded warpline-control starting");
    let app = warpline_control::router(control_state);
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            tracing::error!(error = %e, "embedded control listener failed");
        }
    }))
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
             enforced, every tenant gets the default resource caps, and \
             invocations are logged instead of metered. Do not run this in \
             production."
        );
        return Ok(None);
    };
    let pool = pg::connect(&url).await?;
    pg::migrate(&pool).await?;
    let ttl = match env_nonempty("WARPLINE_AUTH_CACHE_TTL_SECS") {
        Some(v) => v.parse::<u64>().map_err(|_| {
            anyhow::anyhow!(
                "WARPLINE_AUTH_CACHE_TTL_SECS must be a whole number of seconds, got {v:?}"
            )
        })?,
        None => 30,
    };
    Ok(Some(Authenticator::new(pool, Duration::from_secs(ttl))))
}
