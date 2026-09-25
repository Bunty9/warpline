//! Host-side request / response envelopes that mirror the records in
//! `wit/warpline.wit`. The wit-bindgen-generated bindings will subsume these
//! in Phase 2; for Phase 1 they keep the trait signatures honest without
//! pulling in the codegen.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wasmtime_wasi::preview1::WasiP1Ctx;

use crate::kv::KvStore;

/// Outbound HTTP request issued by guest code via the `warpline:host/http-out`
/// capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpReq {
    pub url: String,
    pub method: String,
    pub body: Vec<u8>,
}

/// Response returned to the guest from `http-out::fetch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResp {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Per-invocation host context attached to the wasmtime `Store`.
///
/// One of these is constructed for every call to [`crate::runtime::invoke`].
/// It carries:
/// - `tenant_id`, `fn_name` — identity for metering + capability scoping.
/// - `kv` — pluggable per-tenant key/value backend.
/// - `allowed_hosts` — outbound HTTP host allowlist (deny-by-default).
/// - `mem_cap_bytes` — memory ceiling enforced by [`TenantLimiter`].
/// - `wasi` — WASI Preview 1 context wired through `wasmtime-wasi`.
pub struct HostCtx {
    pub tenant_id: String,
    pub fn_name: String,
    pub kv: Arc<dyn KvStore>,
    pub allowed_hosts: Vec<String>,
    pub mem_cap_bytes: usize,
    pub wasi: WasiP1Ctx,
}

/// Memory + table-growth limiter installed on the wasmtime `Store` via
/// `Store::limiter`. Each tenant gets its own ceiling — runaway allocators
/// are trapped when `memory.grow` would push the linear memory above the cap.
pub struct TenantLimiter {
    pub mem_cap_bytes: usize,
}

impl wasmtime::ResourceLimiter for TenantLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _max: Option<usize>,
    ) -> anyhow::Result<bool> {
        Ok(desired <= self.mem_cap_bytes && desired >= current)
    }
    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _max: Option<usize>,
    ) -> anyhow::Result<bool> {
        // NOTE(phase-1): wasmtime 27 widened the `ResourceLimiter` table
        // counters from `u32` to `usize`. The spec snippet predates that
        // change; we follow the trait's current signature.
        Ok(desired <= 10_000)
    }
}
