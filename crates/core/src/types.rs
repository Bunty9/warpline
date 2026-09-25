//! Per-invocation host context attached to the wasmtime `Store`.
//!
//! One [`HostCtx`] is constructed for every call to [`crate::runtime::invoke`].
//! It carries tenant identity, the KV backend, the outbound-HTTP allowlist +
//! shared `reqwest::Client`, the WASI p2 state, and the [`TenantLimiter`]
//! that enforces the memory cap and records peak usage for metering.
//!
//! The hand-written `HttpReq`/`HttpResp` envelopes from Phase 1 are gone —
//! `wasmtime::component::bindgen!` in `runtime.rs` generates `Request` /
//! `Response` types straight from `wit/warpline.wit`, so this module no
//! longer needs to mirror them by hand.

use std::sync::Arc;

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::kv::KvStore;

/// Per-invocation host context. See module docs.
pub struct HostCtx {
    pub tenant_id: String,
    pub fn_name: String,
    pub kv: Arc<dyn KvStore>,
    /// Outbound HTTP host allowlist (deny-by-default — see
    /// `runtime::host_http_out`).
    pub allowed_hosts: Vec<String>,
    /// Shared `reqwest::Client` — cheap to clone, expensive to build (each
    /// one owns a connection pool), so callers construct one per process and
    /// pass it into every `HostCtx`.
    pub http_client: reqwest::Client,
    /// Memory cap + peak-usage tracker, installed on the `Store` via
    /// `store.limiter(|c| &mut c.limiter)`.
    pub limiter: TenantLimiter,
    wasi_ctx: WasiCtx,
    table: ResourceTable,
}

impl HostCtx {
    /// Build a [`HostCtx`] with a deny-by-default WASI p2 context: no
    /// preopens, no env, no args, no network — only what the guest's Rust
    /// std needs to link (clocks, random, a stdio sink).
    pub fn new(
        tenant_id: String,
        fn_name: String,
        kv: Arc<dyn KvStore>,
        allowed_hosts: Vec<String>,
        http_client: reqwest::Client,
        mem_cap_bytes: usize,
    ) -> Self {
        Self {
            tenant_id,
            fn_name,
            kv,
            allowed_hosts,
            http_client,
            limiter: TenantLimiter::new(mem_cap_bytes),
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

/// Memory + table-growth limiter installed on the wasmtime `Store` via
/// `Store::limiter`. Each tenant gets its own ceiling — runaway allocators
/// are rejected when `memory.grow` would push the linear memory above the
/// cap — and the limiter doubles as the peak-memory recorder for metering.
pub struct TenantLimiter {
    pub mem_cap_bytes: usize,
    /// High-water mark of every `desired` size this limiter has accepted.
    pub peak_bytes: usize,
    /// Set once `memory_growing` rejects a request — lets `invoke`
    /// distinguish "guest hit the memory cap" from any other trap.
    pub cap_hit: bool,
    table_cap: usize,
}

impl TenantLimiter {
    pub fn new(mem_cap_bytes: usize) -> Self {
        Self {
            mem_cap_bytes,
            peak_bytes: 0,
            cap_hit: false,
            table_cap: 10_000,
        }
    }
}

impl wasmtime::ResourceLimiter for TenantLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _max: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired <= self.mem_cap_bytes {
            self.peak_bytes = self.peak_bytes.max(desired);
            Ok(true)
        } else {
            self.cap_hit = true;
            Ok(false)
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _max: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.table_cap)
    }
}
