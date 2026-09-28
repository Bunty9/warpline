//! Per-invocation host context attached to the wasmtime `Store`.
//!
//! One [`HostCtx`] is constructed for every call to [`crate::runtime::invoke`].
//! It carries tenant identity, the KV backend, the outbound-HTTP allowlist +
//! shared `reqwest::Client`, the WASI p2 state, the per-invocation KV/log
//! quota counters, and the [`TenantLimiter`] that enforces the memory/table
//! cap and records peak usage for metering.
//!
//! The hand-written `HttpReq`/`HttpResp` envelopes from Phase 1 are gone —
//! `wasmtime::component::bindgen!` in `runtime.rs` generates `Request` /
//! `Response` types straight from `crates/core/wit/warpline.wit`, so this module no
//! longer needs to mirror them by hand.

use std::sync::Arc;

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::kv::KvStore;

/// Returns true iff `s` matches `^[a-z0-9][a-z0-9_-]{0,62}$` — the shape
/// required of tenant and function names. Hand-rolled rather than pulling
/// in a regex crate for one pattern.
pub fn valid_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let is_head = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let is_tail = |b: u8| is_head(b) || b == b'_' || b == b'-';
    is_head(bytes[0]) && bytes[1..].iter().all(|&b| is_tail(b))
}

/// Valid range for a tenant's `cpu_budget_ms`, milliseconds — mirrored by
/// the `CHECK` constraint on `tenants.cpu_budget_ms` in
/// `crates/core/migrations/0002_auth_config.sql`.
pub const MIN_CPU_BUDGET_MS: i64 = 1;
pub const MAX_CPU_BUDGET_MS: i64 = 10_000;
/// Valid range for a tenant's `mem_cap_bytes` — mirrored by the `CHECK`
/// constraint on `tenants.mem_cap_bytes`.
pub const MIN_MEM_CAP_BYTES: i64 = 1024 * 1024;
pub const MAX_MEM_CAP_BYTES: i64 = 512 * 1024 * 1024;
/// Max `allowed_hosts` entries a tenant config may set.
pub const MAX_ALLOWED_HOSTS: usize = 64;

/// A tenant config value submitted to `warpline-control`'s admin route
/// failed validation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cpu_budget_ms must be between {MIN_CPU_BUDGET_MS} and {MAX_CPU_BUDGET_MS}")]
    CpuBudgetRange,
    #[error("mem_cap_bytes must be between {MIN_MEM_CAP_BYTES} and {MAX_MEM_CAP_BYTES}")]
    MemCapRange,
    #[error("allowed_hosts must have at most {MAX_ALLOWED_HOSTS} entries")]
    TooManyHosts,
    #[error("invalid hostname: {0}")]
    InvalidHost(String),
}

pub fn validate_cpu_budget_ms(v: i64) -> Result<(), ConfigError> {
    if (MIN_CPU_BUDGET_MS..=MAX_CPU_BUDGET_MS).contains(&v) {
        Ok(())
    } else {
        Err(ConfigError::CpuBudgetRange)
    }
}

pub fn validate_mem_cap_bytes(v: i64) -> Result<(), ConfigError> {
    if (MIN_MEM_CAP_BYTES..=MAX_MEM_CAP_BYTES).contains(&v) {
        Ok(())
    } else {
        Err(ConfigError::MemCapRange)
    }
}

/// A "plausible hostname" is anything `url::Host::parse` accepts — a
/// domain name or an IP literal, both of which `http-out::fetch`'s
/// allowlist check (`runtime::http_fetch`) compares against verbatim.
///
/// Returns the *normalized* form of each host (lowercased, trailing
/// root-label dot trimmed) rather than echoing back what was submitted —
/// `runtime::http_fetch` compares a request's already-normalized host
/// against whatever was stored here, so storing the raw, unnormalized
/// input (e.g. `API.Example.com.`) would silently make the allowlist
/// entry never match. Callers (`warpline-control`'s admin route) persist
/// the returned `Vec<String>`, not the input.
pub fn validate_allowed_hosts(hosts: &[String]) -> Result<Vec<String>, ConfigError> {
    if hosts.len() > MAX_ALLOWED_HOSTS {
        return Err(ConfigError::TooManyHosts);
    }
    hosts
        .iter()
        .map(|h| {
            let host = url::Host::parse(h).map_err(|_| ConfigError::InvalidHost(h.clone()))?;
            Ok(host
                .to_string()
                .to_lowercase()
                .trim_end_matches('.')
                .to_string())
        })
        .collect()
}

/// Per-invocation host context. See module docs.
pub struct HostCtx {
    pub tenant_id: String,
    pub fn_name: String,
    pub kv: Arc<dyn KvStore>,
    /// Outbound HTTP host allowlist (deny-by-default — see
    /// `runtime::host_http_out`).
    pub allowed_hosts: Vec<String>,
    /// When `false` (the default outside tests), `http-out::fetch` refuses
    /// to connect to loopback/private/link-local/etc. addresses — see
    /// `runtime::is_blocked_ip`. Tests that stand up a `127.0.0.1` server
    /// set this `true`.
    pub allow_private_egress: bool,
    /// Shared `reqwest::Client` — cheap to clone, expensive to build (each
    /// one owns a connection pool), so callers construct one per process and
    /// pass it into every `HostCtx`.
    pub http_client: reqwest::Client,
    /// Memory/table cap + peak-usage tracker, installed on the `Store` via
    /// `store.limiter(|c| &mut c.limiter)`.
    pub limiter: TenantLimiter,
    /// `kv::put` calls made so far this invocation — capped at
    /// [`crate::runtime::MAX_KV_PUTS_PER_INVOCATION`].
    pub(crate) kv_put_count: usize,
    /// `kv::put` value bytes written so far this invocation — capped at
    /// [`crate::runtime::MAX_KV_PUT_BYTES_PER_INVOCATION`].
    pub(crate) kv_put_bytes: usize,
    /// `log::emit` lines emitted so far this invocation — capped at
    /// [`crate::runtime::MAX_LOG_LINES_PER_INVOCATION`].
    pub(crate) log_line_count: usize,
    /// `log::emit` message bytes emitted so far this invocation — capped at
    /// [`crate::runtime::MAX_LOG_BYTES_PER_INVOCATION`].
    pub(crate) log_bytes: usize,
    /// Set once the log limit has been hit, so the "log output suppressed"
    /// notice is only emitted once per invocation instead of once per
    /// dropped line.
    pub(crate) log_suppressed_notified: bool,
    wasi_ctx: WasiCtx,
    table: ResourceTable,
}

impl HostCtx {
    /// Build a [`HostCtx`] with a deny-by-default WASI p2 context: no
    /// preopens, no env, no args, no network — only what the guest's Rust
    /// std needs to link (clocks, random, a stdio sink).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tenant_id: String,
        fn_name: String,
        kv: Arc<dyn KvStore>,
        allowed_hosts: Vec<String>,
        allow_private_egress: bool,
        http_client: reqwest::Client,
        mem_cap_bytes: usize,
    ) -> Self {
        Self {
            tenant_id,
            fn_name,
            kv,
            allowed_hosts,
            allow_private_egress,
            http_client,
            limiter: TenantLimiter::new(mem_cap_bytes),
            kv_put_count: 0,
            kv_put_bytes: 0,
            log_line_count: 0,
            log_bytes: 0,
            log_suppressed_notified: false,
            wasi_ctx: wasmtime_wasi::WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        }
    }
}

impl WasiView for HostCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

/// Small sane ceiling on core instances a single component's `Store` may
/// create. A `wasm32-wasip2` component built with `wit-bindgen` typically
/// instantiates on the order of a dozen core modules (its own module plus
/// the WASI p2 adapter modules it imports); 32 leaves headroom for that
/// while still catching a component that tries to spin up an unbounded
/// number of sub-instances.
const MAX_INSTANCES: usize = 32;
/// Small sane ceiling on core tables a single `Store` may create.
const MAX_TABLES: usize = 8;
/// Small sane ceiling on core linear memories a single `Store` may create —
/// most components have exactly one; a handful covers multi-memory
/// components without leaving the cap effectively unbounded.
const MAX_MEMORIES: usize = 4;

/// Memory + table-growth limiter installed on the wasmtime `Store` via
/// `Store::limiter`. Each tenant gets its own ceiling — runaway allocators
/// are rejected when `memory.grow`/`table.grow` would push the *combined*
/// size of every memory/table in the store above the cap — and the limiter
/// doubles as the peak-memory recorder for metering.
///
/// A component can have many core memories (its own module plus every
/// dependency it links against), each capable of growing independently;
/// checking `desired` against the cap per-memory (as an earlier version of
/// this limiter did) lets a component with N memories use N times the
/// intended cap. Tracking a running `total_bytes` across every memory this
/// limiter has seen closes that.
pub struct TenantLimiter {
    pub mem_cap_bytes: usize,
    /// Sum of `desired - current` over every accepted `memory_growing` call
    /// — the combined size of every core memory in the store.
    total_bytes: usize,
    /// High-water mark of [`Self::total_bytes`].
    pub peak_bytes: usize,
    /// Set once `memory_growing` rejects a request — lets `invoke`
    /// distinguish "guest hit the memory cap" from any other trap.
    pub cap_hit: bool,
    table_cap_elems: usize,
    /// Sum of `desired - current` over every accepted `table_growing` call,
    /// mirroring `total_bytes` for tables.
    total_table_elems: usize,
}

impl TenantLimiter {
    pub fn new(mem_cap_bytes: usize) -> Self {
        Self {
            mem_cap_bytes,
            total_bytes: 0,
            peak_bytes: 0,
            cap_hit: false,
            table_cap_elems: 10_000,
            total_table_elems: 0,
        }
    }
}

impl wasmtime::ResourceLimiter for TenantLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        max: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // wasmtime consults the limiter before the memory's own declared
        // max; reject those here so a doomed grow isn't charged.
        if max.is_some_and(|m| desired > m) {
            return Ok(false);
        }
        let new_total = self.total_bytes + desired.saturating_sub(current);
        if new_total <= self.mem_cap_bytes {
            self.total_bytes = new_total;
            self.peak_bytes = self.peak_bytes.max(self.total_bytes);
            Ok(true)
        } else {
            self.cap_hit = true;
            Ok(false)
        }
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        max: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if max.is_some_and(|m| desired > m) {
            return Ok(false);
        }
        let new_total = self.total_table_elems + desired.saturating_sub(current);
        if new_total <= self.table_cap_elems {
            self.total_table_elems = new_total;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
}

#[cfg(test)]
mod tests {
    use super::{
        valid_name, validate_allowed_hosts, validate_cpu_budget_ms, validate_mem_cap_bytes,
    };

    #[test]
    fn config_validators_accept_boundary_values_and_reject_outside_them() {
        assert!(validate_cpu_budget_ms(1).is_ok());
        assert!(validate_cpu_budget_ms(10_000).is_ok());
        assert!(validate_cpu_budget_ms(0).is_err());
        assert!(validate_cpu_budget_ms(10_001).is_err());

        assert!(validate_mem_cap_bytes(1024 * 1024).is_ok());
        assert!(validate_mem_cap_bytes(512 * 1024 * 1024).is_ok());
        assert!(validate_mem_cap_bytes(1024 * 1024 - 1).is_err());
        assert!(validate_mem_cap_bytes(512 * 1024 * 1024 + 1).is_err());

        assert!(
            validate_allowed_hosts(&["example.com".to_string(), "127.0.0.1".to_string()]).is_ok()
        );
        assert!(validate_allowed_hosts(&["not a host".to_string()]).is_err());
        assert!(validate_allowed_hosts(&vec!["a.com".to_string(); 65]).is_err());
    }

    #[test]
    fn validate_allowed_hosts_normalizes_case_and_trailing_dot() {
        let normalized = validate_allowed_hosts(&["API.Example.com.".to_string()]).unwrap();
        assert_eq!(normalized, vec!["api.example.com".to_string()]);
    }

    #[test]
    fn valid_name_accepts_expected_shapes() {
        assert!(valid_name("a"));
        assert!(valid_name("tenant-a"));
        assert!(valid_name("fn_1"));
        assert!(valid_name("0abc"));
        assert!(valid_name(&"a".repeat(63)));
    }

    #[test]
    fn valid_name_rejects_bad_shapes() {
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(64)));
        assert!(!valid_name("-leading-dash"));
        assert!(!valid_name("Uppercase"));
        assert!(!valid_name("has space"));
        assert!(!valid_name("has/slash"));
        assert!(!valid_name("has.dot"));
        assert!(!valid_name("../traversal"));
    }
}
