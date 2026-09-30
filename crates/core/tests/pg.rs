//! Postgres-backed tests for `warpline_core::pg`. DB tests skip unless
//! `WARPLINE_TEST_DATABASE_URL` is set and non-empty; each uses its own
//! uniquely named tenant so they can share one database concurrently.

#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use sqlx::types::chrono::{DateTime, Utc};
use sqlx::PgPool;
use warpline_core::pg::{self, AdminError, AuthOutcome, Authenticator, PgMeter};
use warpline_core::{Limits, MeterSink, Usage};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tenant(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "t-{label}-{nanos}-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Pool against the test database, migrated; `None` (skip) if unconfigured.
async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("WARPLINE_TEST_DATABASE_URL").unwrap_or_default();
    if url.is_empty() {
        eprintln!("skipping: WARPLINE_TEST_DATABASE_URL not set");
        return None;
    }
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    pg::migrate(&pool).await.unwrap();
    Some(pool)
}

macro_rules! pool_or_skip {
    () => {
        match test_pool().await {
            Some(p) => p,
            None => return,
        }
    };
}

#[tokio::test]
async fn migrate_is_idempotent_and_leaves_public_schema_alone() {
    let pool = pool_or_skip!();
    // A host app's own tables, same names as ours, in `public`.
    sqlx::query("CREATE TABLE IF NOT EXISTS public.tenants (marker TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE IF NOT EXISTS public._sqlx_migrations (marker TEXT)")
        .execute(&pool)
        .await
        .unwrap();

    pg::migrate(&pool).await.unwrap();
    pg::migrate(&pool).await.unwrap();

    for table in ["tenants", "_sqlx_migrations"] {
        let cols: Vec<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(table)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(cols, ["marker"], "public.{table} was modified");
    }
    let in_warpline: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables \
         WHERE table_schema = 'warpline' AND table_name = '_sqlx_migrations'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(in_warpline, 1);
}

#[tokio::test]
async fn authenticate_outcomes() {
    let pool = pool_or_skip!();
    let auth = Authenticator::new(pool.clone(), Duration::ZERO);
    let (a, b) = (unique_tenant("a"), unique_tenant("b"));
    let limits = Limits::new(7, 2 << 20)
        .unwrap()
        .with_allowed_hosts(&["Example.com".to_string()])
        .unwrap();
    let key_a = pg::create_tenant(&pool, &a, Some(&limits)).await.unwrap();
    pg::create_tenant(&pool, &b, None).await.unwrap();

    assert_eq!(
        auth.authenticate(&a, Some(&key_a.api_key)).await.unwrap(),
        AuthOutcome::Authorized(limits)
    );
    assert_eq!(
        auth.authenticate(&a, Some("wl_nope")).await.unwrap(),
        AuthOutcome::MissingOrUnknownKey
    );
    assert_eq!(
        auth.authenticate(&a, None).await.unwrap(),
        AuthOutcome::MissingOrUnknownKey
    );
    assert_eq!(
        auth.authenticate(&b, Some(&key_a.api_key)).await.unwrap(),
        AuthOutcome::WrongTenant
    );
}

#[tokio::test]
async fn auth_cache_ttl_is_respected() {
    let pool = pool_or_skip!();
    let name = unique_tenant("ttl");
    let key = pg::create_tenant(&pool, &name, None).await.unwrap().api_key;
    let new = Limits::new(9, 3 << 20).unwrap();

    let cached = Authenticator::new(pool.clone(), Duration::from_millis(400));
    let uncached = Authenticator::new(pool.clone(), Duration::ZERO);
    let default = AuthOutcome::Authorized(Limits::default());
    assert_eq!(
        cached.authenticate(&name, Some(&key)).await.unwrap(),
        default
    );

    pg::set_limits(&pool, &name, &new).await.unwrap();
    // Cache still serves the old limits; TTL 0 sees the change at once.
    assert_eq!(
        cached.authenticate(&name, Some(&key)).await.unwrap(),
        default
    );
    assert_eq!(
        uncached.authenticate(&name, Some(&key)).await.unwrap(),
        AuthOutcome::Authorized(new.clone())
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        cached.authenticate(&name, Some(&key)).await.unwrap(),
        AuthOutcome::Authorized(new)
    );
}

#[tokio::test]
async fn create_tenant_is_idempotent_and_issues_new_keys() {
    let pool = pool_or_skip!();
    let name = unique_tenant("idem");
    let auth = Authenticator::new(pool.clone(), Duration::ZERO);
    let limits = Limits::new(5, 4 << 20).unwrap();

    let k1 = pg::create_tenant(&pool, &name, Some(&limits))
        .await
        .unwrap();
    // `None` on an existing tenant keeps its limits.
    let k2 = pg::create_tenant(&pool, &name, None).await.unwrap();
    assert_ne!(k1.api_key, k2.api_key);
    for k in [&k1, &k2] {
        assert_eq!(
            auth.authenticate(&name, Some(&k.api_key)).await.unwrap(),
            AuthOutcome::Authorized(limits.clone())
        );
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM warpline.tenants WHERE name = $1")
        .bind(&name)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);

    assert!(matches!(
        pg::create_tenant(&pool, "Bad Name", None).await,
        Err(AdminError::InvalidName)
    ));
    assert!(matches!(
        pg::set_limits(&pool, &unique_tenant("ghost"), &limits).await,
        Err(AdminError::NoSuchTenant(_))
    ));
}

#[tokio::test]
async fn meter_rows_land_and_summarise() {
    let pool = pool_or_skip!();
    let tenant = unique_tenant("meter");
    let since: DateTime<Utc> = (SystemTime::now() - Duration::from_secs(60)).into();
    let (meter, handle) = PgMeter::spawn(pool.clone(), 100);
    meter.record(&tenant, "f", Usage::new(1500, 2500, 4096), true);
    meter.record(&tenant, "f", Usage::new(10, 20, 8192), false);
    handle.shutdown(Duration::from_secs(5)).await;

    assert_eq!(meter.dropped(), 0);
    let mut rows: Vec<(i64, i64, i64, bool)> = sqlx::query_as(
        "SELECT cpu_us, wall_us, mem_peak_bytes, ok FROM warpline.meter WHERE tenant = $1 ORDER BY id",
    )
    .bind(&tenant)
    .fetch_all(&pool)
    .await
    .unwrap();
    rows.sort();
    assert_eq!(rows, [(10, 20, 8192, false), (1500, 2500, 4096, true)]);

    let s = pg::usage_summary(&pool, &tenant, since).await.unwrap();
    assert_eq!(
        (
            s.invocations,
            s.errors,
            s.cpu_us,
            s.wall_us,
            s.max_mem_peak_bytes
        ),
        (2, 1, 1510, 2520, 8192)
    );
    let none = pg::usage_summary(&pool, &unique_tenant("empty"), since)
        .await
        .unwrap();
    assert_eq!(none.invocations, 0);
}

/// No database needed: a pool pointing nowhere.
fn dead_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://x:x@127.0.0.1:1/x")
        .unwrap()
}

#[tokio::test]
async fn dead_pool_retries_counts_drops_and_does_not_hang() {
    let (meter, handle) = PgMeter::spawn(dead_pool(), 100);
    for _ in 0..3 {
        meter.record("t", "f", Usage::new(1, 1, 1), true);
    }
    let started = std::time::Instant::now();
    handle.shutdown(Duration::from_secs(10)).await;
    assert!(started.elapsed() < Duration::from_secs(10), "shutdown hung");
    assert_eq!(meter.dropped(), 3);
}

#[tokio::test]
async fn full_channel_counts_drops() {
    let (meter, handle) = PgMeter::spawn(dead_pool(), 1);
    // The writer is stuck retrying the first batch, so the 1-slot channel fills.
    for _ in 0..50 {
        meter.record("t", "f", Usage::new(1, 1, 1), true);
    }
    assert!(meter.dropped() > 0);
    handle.shutdown(Duration::from_secs(10)).await;
}
