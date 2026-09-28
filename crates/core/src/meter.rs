//! Per-invocation metering ledger.
//!
//! Every completed `runtime::invoke` call — success or guest-visible
//! error/trap alike — is queued as one [`MeterMsg`] and eventually lands as
//! one row in the `meter` table recording `(tenant, func, cpu_us,
//! mem_peak_bytes, ok)` plus a default `now()` timestamp. `ok` (added in
//! `migrations/0002_auth_config.sql`) is `false` for a
//! trapped/timed-out/capped invocation, so the billing rollup job (out of
//! scope for Phase 1/2) can tell a metered failure from a metered success.
//! The schema is otherwise owned in `migrations/0001_init.sql`.
//!
//! This is an append-only ledger for the runtime. Rows are queued rather
//! than written inline on the invoke path: a per-invoke `tokio::spawn` +
//! `INSERT` would let a burst of invokes spawn one Postgres write per
//! request with no backpressure. Instead callers `try_send` onto a bounded
//! channel (see [`spawn_writer`]) to a single writer task that batches
//! inserts; a full channel means dropping the row rather than blocking the
//! request or growing the task count without bound.

use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::auth::DbState;

/// Capacity of the channel [`spawn_writer`] hands back — see module docs.
/// Sized so a multi-second Postgres hiccup can be absorbed without dropping
/// rows under normal invoke rates, without letting an unbounded backlog
/// build up memory.
pub const METER_CHANNEL_CAPACITY: usize = 10_000;

/// Max rows the writer batches into one `INSERT`.
const BATCH_MAX: usize = 200;

/// One completed invocation, queued for the writer task.
#[derive(Debug, Clone)]
pub struct MeterMsg {
    pub tenant: String,
    pub func: String,
    pub cpu_us: u64,
    pub mem_peak_bytes: usize,
    pub ok: bool,
}

/// The sender half handed to callers by [`spawn_writer`].
pub type MeterSender = mpsc::Sender<MeterMsg>;

/// Insert one metering row directly, bypassing the batching writer.
/// Returns the sqlx error verbatim if the insert fails. Used by tests that
/// want a synchronous write.
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

/// Multi-row `INSERT` for `rows` via `QueryBuilder`'s `push_values` (one
/// round trip regardless of batch size).
async fn insert_batch(pool: &PgPool, rows: &[MeterMsg]) -> sqlx::Result<()> {
    let mut qb = sqlx::QueryBuilder::new(
        "INSERT INTO meter (tenant, func, cpu_us, mem_peak_bytes, ok, ts) ",
    );
    qb.push_values(rows, |mut b, row| {
        b.push_bind(&row.tenant)
            .push_bind(&row.func)
            .push_bind(row.cpu_us as i64)
            .push_bind(row.mem_peak_bytes as i64)
            .push_bind(row.ok)
            .push("now()");
    });
    qb.build().execute(pool).await?;
    Ok(())
}

/// Drain `rx`, writing batches to Postgres (or just logging, under
/// `DbState::InsecureDev`) until every [`MeterSender`] clone has been
/// dropped and the channel is empty — i.e. until graceful shutdown. One
/// task per process; see [`spawn_writer`].
async fn run(mut rx: mpsc::Receiver<MeterMsg>, db: DbState) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(row) => batch.push(row),
                Err(_) => break,
            }
        }
        match &db {
            DbState::Postgres(pool) => {
                if let Err(e) = insert_batch(pool, &batch).await {
                    tracing::warn!(error = %e, n = batch.len(), "failed to write meter batch");
                }
            }
            DbState::InsecureDev => {
                for row in &batch {
                    tracing::debug!(
                        tenant = %row.tenant, func = %row.func, cpu_us = row.cpu_us,
                        mem_peak_bytes = row.mem_peak_bytes, ok = row.ok,
                        "invoke completed (metering disabled: WARPLINE_INSECURE_DEV)"
                    );
                }
            }
        }
    }
}

/// Spawn the single metering writer task for the process. Returns the
/// sender side (cheap to clone into every request handler) and the task's
/// `JoinHandle`, so the caller can drain it on graceful shutdown:
///
/// ```ignore
/// let (tx, handle) = spawn_writer(db, METER_CHANNEL_CAPACITY);
/// // ... serve requests, cloning `tx` into handler state ...
/// drop(tx); // drop this and every clone handed to handlers
/// let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
/// ```
pub fn spawn_writer(db: DbState, capacity: usize) -> (MeterSender, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(capacity);
    let handle = tokio::spawn(run(rx, db));
    (tx, handle)
}
