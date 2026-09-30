//! What a completed invocation cost, and where that gets reported.
//!
//! The runtime hands every finished invocation (success or guest-visible
//! failure alike) to a [`MeterSink`]. The trait is always compiled so an
//! embedding app can plug in its own sink; the Postgres-backed one is
//! `pg::PgMeter` (feature `postgres`).

/// Resources one invocation used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// Guest CPU time in microseconds.
    // ponytail: for now this is the same wall-clock measurement as `wall_us`;
    // the runtime facade switches it to epoch-tick counting.
    pub cpu_us: u64,
    /// Wall-clock time in microseconds.
    pub wall_us: u64,
    /// Peak guest linear memory, bytes (0 when unknown, e.g. after a trap).
    pub mem_peak_bytes: usize,
}

impl Usage {
    pub fn new(cpu_us: u64, wall_us: u64, mem_peak_bytes: usize) -> Self {
        Self {
            cpu_us,
            wall_us,
            mem_peak_bytes,
        }
    }
}

/// Receives one call per completed invocation.
pub trait MeterSink: Send + Sync + 'static {
    /// Must not block: it runs on the invoke path. Sinks that do I/O queue
    /// internally and drop (and count) rows when they cannot keep up.
    fn record(&self, tenant: &str, func: &str, usage: Usage, ok: bool);
}
