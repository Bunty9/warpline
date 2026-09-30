//! [warpline 10] A Postgres-backed `KvStore`: what guests' `kv` capability
//! reads and writes, made durable.
//!
//! The trait is two methods, `get` and `put`, each handed the tenant on every
//! call. Rows live in this app's own `storefront` schema, never in
//! warpline's. Semantics match the in-memory `MemKv`: a per-tenant byte quota
//! charging key + value + 64 bytes per entry, with an overwrite freeing the
//! old entry first. Backend failures become `KvError::Backend`, which traps
//! the calling guest (warpline treats storage as infrastructure, not
//! something a guest can recover from).

use async_trait::async_trait;
use sqlx::PgPool;
use warpline_core::{KvError, KvStore};

/// Same per-entry overhead `MemKv` charges.
const ENTRY_OVERHEAD_BYTES: i64 = 64;

pub struct PgKv {
    pool: PgPool,
    tenant_cap_bytes: i64,
}

fn backend(e: sqlx::Error) -> KvError {
    KvError::Backend(Box::new(e))
}

impl PgKv {
    pub fn new(pool: PgPool, tenant_cap_bytes: usize) -> Self {
        Self {
            pool,
            tenant_cap_bytes: i64::try_from(tenant_cap_bytes).unwrap_or(i64::MAX),
        }
    }

    /// Create the table (idempotent). The app owns this schema, so it is
    /// created here rather than by `warpline_core::pg::migrate`.
    pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::Error> {
        sqlx::query("CREATE SCHEMA IF NOT EXISTS storefront")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS storefront.hook_kv (\
                tenant text NOT NULL, key text NOT NULL, value bytea NOT NULL, \
                PRIMARY KEY (tenant, key))",
        )
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[async_trait]
impl KvStore for PgKv {
    async fn get(&self, tenant: &str, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        sqlx::query_scalar("SELECT value FROM storefront.hook_kv WHERE tenant = $1 AND key = $2")
            .bind(tenant)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)
    }

    async fn put(&self, tenant: &str, key: &str, value: Vec<u8>) -> Result<(), KvError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // Serialise writers of one tenant so the quota check and the write
        // are atomic (released at commit/rollback).
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        // ponytail: sums the tenant's rows on every put, which is fine for
        // small counters; keep a running total column if tenants store a lot.
        let (used, old): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(sum($3 + octet_length(key) + octet_length(value)), 0)::bigint, \
                    COALESCE(sum($3 + octet_length(key) + octet_length(value)) \
                             FILTER (WHERE key = $2), 0)::bigint \
             FROM storefront.hook_kv WHERE tenant = $1",
        )
        .bind(tenant)
        .bind(key)
        .bind(ENTRY_OVERHEAD_BYTES)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        let cost = ENTRY_OVERHEAD_BYTES
            .saturating_add(key.len() as i64)
            .saturating_add(value.len() as i64);
        if used - old + cost > self.tenant_cap_bytes {
            return Err(KvError::QuotaExceeded {
                used_bytes: used as usize,
                added_bytes: cost as usize,
                cap_bytes: self.tenant_cap_bytes as usize,
            });
        }
        sqlx::query(
            "INSERT INTO storefront.hook_kv (tenant, key, value) VALUES ($1, $2, $3) \
             ON CONFLICT (tenant, key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(tenant)
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }
}
