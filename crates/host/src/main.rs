//! `warpline-host` binary entry point. All routing and business logic
//! lives in `warpline_host` (this crate's lib target) so it can be driven
//! from tests via `tower::ServiceExt::oneshot` without a real socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use warpline_core::{
    kv::MemKv,
    pg::{self, Authenticator, PgMeter},
    registry::{self, ComponentCache},
    runtime::{build_engine, build_http_client, build_linker, EpochTicker},
    MeterSink,
};
use warpline_host::{metrics_handle, metrics_router, router, shutdown_signal, AppState, LogMeter};

/// Rows the meter queue holds before it starts dropping (and counting).
const METER_CHANNEL_CAPACITY: usize = 10_000;

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

    let engine = build_engine()?;
    let linker = build_linker(&engine)?;
    let ticker = EpochTicker::spawn(engine.clone());
    // Deny-by-default: outbound HTTP refuses loopback/private/link-local/etc.
    // targets unless explicitly opted into (e.g. local dev against a
    // sidecar). See `warpline_core::runtime::is_blocked_ip`.
    let allow_private_egress =
        env_nonempty("WARPLINE_ALLOW_PRIVATE_EGRESS").is_some_and(|v| v == "1");
    let http_client = build_http_client(allow_private_egress)?;
    let modules_dir = env_nonempty("WARPLINE_MODULES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./modules"));

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
    let metrics_handle = metrics_handle();

    let state = AppState {
        engine,
        linker,
        modules_dir,
        component_cache: Arc::new(ComponentCache::new()),
        kv: Arc::new(MemKv::new()),
        http_client,
        allow_private_egress,
        auth,
        metrics_handle: metrics_handle.clone(),
        ticker: Arc::new(ticker),
        meter,
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
    if env_nonempty("WARPLINE_EMBED_CONTROL").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(&state.modules_dir)?;
        match registry::gc_unreferenced_blobs(
            &state.modules_dir,
            &state.engine,
            registry::GC_GRACE_PERIOD,
        ) {
            Ok(removed) => tracing::info!(removed, "startup GC: removed unreferenced module blobs"),
            Err(e) => tracing::warn!(error = %e, "startup GC failed"),
        }
        let mut control_state = warpline_control::AppState::new(
            state.engine.clone(),
            state.linker.clone(),
            state.modules_dir.clone(),
            state.auth.clone(),
        );
        control_state.admin_token = env_nonempty("WARPLINE_ADMIN_TOKEN");
        let default_control_bind = if insecure_dev {
            "127.0.0.1:8081"
        } else {
            "0.0.0.0:8081"
        };
        let control_bind = env_nonempty("WARPLINE_CONTROL_BIND")
            .unwrap_or_else(|| default_control_bind.to_string());
        let control_listener = tokio::net::TcpListener::bind(&control_bind).await?;
        tracing::info!(%control_bind, "embedded warpline-control starting");
        let control_app = warpline_control::router(control_state);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(control_listener, control_app)
                .with_graceful_shutdown(shutdown_signal())
                .await
            {
                tracing::error!(error = %e, "embedded control listener failed");
            }
        });
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

    // Graceful shutdown waited for in-flight requests, so everything they
    // recorded is queued: flush it, bounded.
    if let Some(h) = meter_handle {
        h.shutdown(Duration::from_secs(5)).await;
    }
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
             enforced, every tenant gets the default resource caps, and \
             invocations are logged instead of metered. Do not run this in \
             production."
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
