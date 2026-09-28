//! Per-invocation metering ledger.
//!
//! Every completed `runtime::invoke` call — success or guest-visible
//! error/trap alike — writes one row into the `meter` table recording
//! `(tenant, func, cpu_us, mem_peak_bytes, ok)` plus a default `now()`
//! timestamp. `ok` (added in `migrations/0002_auth_config.sql`) is `false`
//! for a trapped/timed-out/capped invocation, so the billing rollup job
//! (out of scope for Phase 1/2) can tell a metered failure from a metered
//! success. The schema is otherwise owned in `migrations/0001_init.sql`.
//!
//! This is an append-only ledger for the runtime.

use sqlx::PgPool;

/// Insert one metering row. Returns the sqlx error verbatim if the insert
/// fails; the caller decides whether to retry or drop on the floor.
pub async fn record(
    pool: &PgPool,
    tenant: &str,
    func: &str,
    cpu_us: u64,
    mem_peak: usize,
    ok: bool,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO meter (tenant, func, cpu_us, mem_peak_bytes, ok, ts) \
         VALUES ($1, $2, $3, $4, $5, now())",
    )
    .bind(tenant)
    .bind(func)
    .bind(cpu_us as i64)
    .bind(mem_peak as i64)
    .bind(ok)
    .execute(pool)
    .await?;
    Ok(())
}
