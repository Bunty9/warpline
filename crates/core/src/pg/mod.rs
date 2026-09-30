//! Everything Postgres: migrations, bearer-token auth, tenant admin and the
//! batching meter. Behind the `postgres` feature (on by default).
//!
//! Two rules keep this embeddable in someone else's database and process:
//! every object lives in the `warpline` schema (see [`migrate`]), and
//! nothing here reads environment variables or migrates implicitly — the
//! caller owns the pool, the TTLs and when [`migrate`] runs.

mod admin;
mod auth;
mod meter;

pub use admin::{create_tenant, set_limits, usage_summary, AdminError, IssuedKey, UsageSummary};
pub use auth::{AuthError, AuthOutcome, Authenticator};
pub use meter::{PgMeter, PgMeterHandle};
pub use sqlx::PgPool;

use sqlx::migrate::Migrator;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Arbitrary constant for the advisory lock that serialises concurrent
/// `CREATE SCHEMA` calls (sqlx's own lock only covers the migrations).
const SCHEMA_LOCK_KEY: i64 = 0x7761_7270_6c69_6e65; // "warpline"

/// [`migrate`] failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Create the `warpline` schema if needed and apply pending migrations.
/// Idempotent. Touches nothing outside that schema: the migrator runs on
/// one dedicated connection whose `search_path` is `warpline`, so sqlx's
/// `_sqlx_migrations` bookkeeping table lands there too and cannot collide
/// with the host application's own migrations.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    // Detached so the `search_path` change can never leak back into the pool.
    let mut conn = pool.acquire().await?.detach();
    let res = async {
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SCHEMA_LOCK_KEY)
            .execute(&mut conn)
            .await?;
        sqlx::query("CREATE SCHEMA IF NOT EXISTS warpline")
            .execute(&mut conn)
            .await?;
        sqlx::query("SET search_path TO warpline")
            .execute(&mut conn)
            .await?;
        MIGRATOR.run(&mut conn).await?;
        Ok::<_, MigrateError>(())
    }
    .await;
    // Closing the connection also releases the advisory lock.
    let _ = sqlx::Connection::close(conn).await;
    res
}
