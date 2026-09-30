//! [`MeterSink`] that batches rows into `warpline.meter`.
//!
//! `record` only `try_send`s onto a bounded channel; a single writer task
//! drains it in batches of up to 200 rows, one multi-row `INSERT` each. A
//! failing batch is retried with bounded exponential backoff and every query
//! is under a timeout, so a sick database can neither block invocations nor
//! hang shutdown. Rows lost to a full channel or exhausted retries are
//! counted in [`PgMeter::dropped`] rather than silently vanishing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::{mpsc, Notify};

use crate::{MeterSink, Usage};

const BATCH_MAX: usize = 200;
const ATTEMPTS: u32 = 3;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_BASE: Duration = Duration::from_millis(100);

struct Row {
    tenant: String,
    func: String,
    usage: Usage,
    ok: bool,
}

/// Non-blocking, batching Postgres meter. Share it as `Arc<dyn MeterSink>`.
pub struct PgMeter {
    tx: mpsc::Sender<Row>,
    dropped: Arc<AtomicU64>,
}

/// Lets the owner flush and stop the writer task. Dropping it without
/// calling [`shutdown`](Self::shutdown) leaves the writer running until
/// every [`PgMeter`] is gone.
pub struct PgMeterHandle {
    task: tokio::task::JoinHandle<()>,
    stop: Arc<Notify>,
}

impl PgMeter {
    /// Start the writer task. `capacity` bounds queued rows. Needs a tokio
    /// runtime.
    pub fn spawn(pool: PgPool, capacity: usize) -> (PgMeter, PgMeterHandle) {
        let (tx, rx) = mpsc::channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(Notify::new());
        let task = tokio::spawn(run(rx, pool, dropped.clone(), stop.clone()));
        (PgMeter { tx, dropped }, PgMeterHandle { task, stop })
    }

    /// Rows lost so far: channel full, or every write attempt failed.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl MeterSink for PgMeter {
    fn record(&self, tenant: &str, func: &str, usage: Usage, ok: bool) {
        let row = Row {
            tenant: tenant.to_string(),
            func: func.to_string(),
            usage,
            ok,
        };
        if self.tx.try_send(row).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl PgMeterHandle {
    /// Stop accepting rows, flush what is queued, and wait up to `timeout`
    /// for the writer to finish. Rows still unwritten at the deadline are
    /// abandoned.
    pub async fn shutdown(self, timeout: Duration) {
        self.stop.notify_one();
        if tokio::time::timeout(timeout, self.task).await.is_err() {
            tracing::warn!("meter writer did not drain within {timeout:?}");
        }
    }
}

async fn run(
    mut rx: mpsc::Receiver<Row>,
    pool: PgPool,
    dropped: Arc<AtomicU64>,
    stop: Arc<Notify>,
) {
    let mut stopping = false;
    loop {
        let first = if stopping {
            rx.try_recv().ok()
        } else {
            tokio::select! {
                row = rx.recv() => row,
                _ = stop.notified() => {
                    // Later `try_send`s now fail (and are counted); drain the rest.
                    rx.close();
                    stopping = true;
                    continue;
                }
            }
        };
        let Some(first) = first else { break };
        let mut batch = vec![first];
        while batch.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(row) => batch.push(row),
                Err(_) => break,
            }
        }
        if let Err(e) = write_with_retry(&pool, &batch).await {
            tracing::warn!(error = %e, rows = batch.len(), "dropping meter batch");
            dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
        }
    }
}

async fn write_with_retry(pool: &PgPool, batch: &[Row]) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(BACKOFF_BASE * 2u32.pow(attempt - 1)).await;
        }
        match tokio::time::timeout(QUERY_TIMEOUT, insert(pool, batch)).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "timed out".to_string(),
        }
    }
    Err(last)
}

fn i64_of<T: TryInto<i64>>(v: T) -> i64 {
    v.try_into().unwrap_or(i64::MAX)
}

async fn insert(pool: &PgPool, rows: &[Row]) -> sqlx::Result<()> {
    let mut qb = sqlx::QueryBuilder::new(
        "INSERT INTO warpline.meter (tenant, func, cpu_us, wall_us, mem_peak_bytes, ok) ",
    );
    qb.push_values(rows, |mut b, r| {
        b.push_bind(&r.tenant)
            .push_bind(&r.func)
            .push_bind(i64_of(r.usage.cpu_us))
            .push_bind(i64_of(r.usage.wall_us))
            .push_bind(i64_of(r.usage.mem_peak_bytes))
            .push_bind(r.ok);
    });
    qb.build().execute(pool).await?;
    Ok(())
}
