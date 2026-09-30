//! The crate's error types.

use crate::Usage;

/// General error for [`Runtime`](crate::Runtime) construction, GC and module
/// loading. Non-exhaustive.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wasmtime error: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    /// On-disk module state is missing or malformed (bad pointer file, no
    /// `.cwasm` and no source `.wasm` for a digest, ...).
    #[error("corrupt or missing module state: {0}")]
    Corrupt(String),
    /// A [`RuntimeConfig`](crate::RuntimeConfig) value is out of range.
    #[error("invalid configuration: {0}")]
    Config(&'static str),
    /// A background task panicked or a client could not be built.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Why [`Runtime::stage`](crate::Runtime::stage) or
/// [`Runtime::activate`](crate::Runtime::activate) failed. Non-exhaustive.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// Tenant or function name fails [`valid_name`](crate::valid_name).
    #[error("invalid tenant or function name")]
    InvalidName,
    /// The bytes are not a valid WebAssembly component.
    #[error("not a valid wasm component")]
    Compile(#[source] wasmtime::Error),
    /// The component compiles but does not fit the warpline handler world:
    /// a missing `handle` export or an import the host does not provide.
    #[error("component does not match the warpline handler world")]
    ImportMismatch(#[source] wasmtime::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Why [`Runtime::invoke`](crate::Runtime::invoke) failed. Non-exhaustive.
///
/// Variants that carry a [`Usage`] failed after the guest started running;
/// those invocations were reported to the configured
/// [`MeterSink`](crate::MeterSink). The others were rejected before the
/// guest ran and were not metered.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InvokeError {
    #[error("invalid tenant or function name")]
    InvalidName,
    #[error("no module published for this function")]
    NotFound,
    /// The host's memory budget cannot admit this invocation right now (or
    /// the tenant's memory cap alone exceeds the whole budget).
    #[error("host is at capacity")]
    Overloaded,
    /// The tenant already has `max_in_flight_per_tenant` invocations running.
    #[error("tenant has too many invocations in flight")]
    TenantBusy,
    #[error("cpu budget exceeded ({budget_ms} ms)")]
    CpuBudgetExceeded { usage: Usage, budget_ms: u64 },
    #[error("memory cap exceeded (peak {} bytes, cap {cap_bytes} bytes)", usage.mem_peak_bytes)]
    MemoryCapExceeded { usage: Usage, cap_bytes: usize },
    #[error("wall-clock timeout")]
    WallClockTimeout { usage: Usage },
    #[error("output exceeds the {limit} byte limit")]
    OutputTooLarge { usage: Usage, limit: usize },
    #[error("guest trapped: {source}")]
    GuestTrap {
        usage: Usage,
        #[source]
        source: wasmtime::Error,
    },
    /// The module could not be loaded (corrupt registry state, compile
    /// failure of the stored source, ...).
    #[error("failed to load module: {0}")]
    Load(#[source] Error),
}

impl InvokeError {
    /// What the guest used before failing; `None` when it never ran.
    pub fn usage(&self) -> Option<Usage> {
        match self {
            Self::CpuBudgetExceeded { usage, .. }
            | Self::MemoryCapExceeded { usage, .. }
            | Self::WallClockTimeout { usage }
            | Self::OutputTooLarge { usage, .. }
            | Self::GuestTrap { usage, .. } => Some(*usage),
            _ => None,
        }
    }

    /// A suitable HTTP status code: 400 invalid name, 404 not found, 503
    /// overloaded, 429 tenant busy, 408 cpu budget / wall clock, 507 memory
    /// cap, 502 output too large, 500 guest trap / load failure.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::InvalidName => 400,
            Self::NotFound => 404,
            Self::Overloaded => 503,
            Self::TenantBusy => 429,
            Self::CpuBudgetExceeded { .. } | Self::WallClockTimeout { .. } => 408,
            Self::MemoryCapExceeded { .. } => 507,
            Self::OutputTooLarge { .. } => 502,
            Self::GuestTrap { .. } | Self::Load(_) => 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_table() {
        let u = Usage::new(0, 0, 0);
        let trap = || wasmtime::Error::msg("trap");
        let cases: Vec<(InvokeError, u16)> = vec![
            (InvokeError::InvalidName, 400),
            (InvokeError::NotFound, 404),
            (InvokeError::Overloaded, 503),
            (InvokeError::TenantBusy, 429),
            (
                InvokeError::CpuBudgetExceeded {
                    usage: u,
                    budget_ms: 1,
                },
                408,
            ),
            (InvokeError::WallClockTimeout { usage: u }, 408),
            (
                InvokeError::MemoryCapExceeded {
                    usage: u,
                    cap_bytes: 1,
                },
                507,
            ),
            (InvokeError::OutputTooLarge { usage: u, limit: 1 }, 502),
            (
                InvokeError::GuestTrap {
                    usage: u,
                    source: trap(),
                },
                500,
            ),
            (InvokeError::Load(Error::Corrupt("x".into())), 500),
        ];
        for (err, status) in cases {
            assert_eq!(err.http_status(), status, "{err}");
        }
    }
}
