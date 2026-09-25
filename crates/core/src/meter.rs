//! Per-invocation metering ledger.
//!
//! Every successful `runtime::invoke` writes one row into the `meter` table
//! recording `(tenant, func, cpu_us, mem_peak_bytes)` plus a default
//! `now()` timestamp. The schema is owned in `migrations/0001_init.sql`.
//!
//! This is the input table for the billing rollup job (out of scope for
//! Phase 1) — for the runtime it's append-only.

use sqlx::PgPool;

/// Insert one metering row. Returns the sqlx error verbatim if the insert
/// fails; the caller decides whether to retry or drop on the floor.
pub async fn record(
    pool: &PgPool,
    tenant: &str,
    func: &str,
    cpu_us: u64,
    mem_peak: usize,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO meter (tenant, func, cpu_us, mem_peak_bytes, ts) \
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(tenant)
    .bind(func)
    .bind(cpu_us as i64)
    .bind(mem_peak as i64)
    .execute(pool)
    .await?;
    Ok(())
}
