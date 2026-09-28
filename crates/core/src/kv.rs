//! Per-tenant key/value store abstraction.
//!
//! The trait is intentionally tiny — `get` / `put`, no batch, no scan — so
//! that any of the downstream candidate backends (in-process [`MemKv`], the
//! P5 `driftdb` LSM, Redis, S3) drop in behind the same surface. Tenant
//! scoping is a constructor argument on every call — `(tenant, key)` is the
//! real key — rather than a host-side string prefix: concatenating
//! `"{tenant}/{key}"` let tenant `a` key `b/x` collide with tenant `a/b` key
//! `x`. A backend that stores by the `(tenant, key)` pair (or an
//! equivalent composite key on a real backend) can't have that collision.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

/// Default per-tenant byte quota for [`MemKv`] when constructed with
/// [`MemKv::new`].
pub const DEFAULT_TENANT_CAP_BYTES: usize = 16 * 1024 * 1024;

/// Fixed bookkeeping cost charged per entry on top of key + value bytes
/// (map node, `String`/`Vec` headers, the tenant string in the tuple key).
pub const ENTRY_OVERHEAD_BYTES: usize = 64;

/// Errors a [`KvStore`] implementation can hand back to the host. The host
/// turns [`KvError::QuotaExceeded`] into a guest trap (see
/// `runtime::warpline::host::kv::Host::put`) rather than a guest-visible
/// `Result` — quota is an operational limit, not something the WIT contract
/// models as guest-recoverable.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    #[error("tenant kv quota exceeded: {used_bytes} used + {added_bytes} new > {cap_bytes} cap")]
    QuotaExceeded {
        used_bytes: usize,
        added_bytes: usize,
        cap_bytes: usize,
    },
}

/// Bytes-in, bytes-out KV interface used by the `warpline:host/kv` capability.
/// `tenant` is supplied by the host on every call — see the module docs for
/// why it's a parameter rather than folded into `key` by string
/// concatenation.
#[async_trait]
pub trait KvStore: Send + Sync {
    /// Look up `key` under `tenant`. Returns `None` if absent.
    async fn get(&self, tenant: &str, key: &str) -> Option<Vec<u8>>;
    /// Write (or overwrite) `tenant`'s `key -> value`.
    async fn put(&self, tenant: &str, key: &str, value: Vec<u8>) -> Result<(), KvError>;
}

#[derive(Default)]
struct MemKvInner {
    entries: HashMap<(String, String), Vec<u8>>,
    /// Running total of value bytes stored per tenant — kept in the same
    /// lock as `entries` so a put's quota check and its write are atomic.
    tenant_bytes: HashMap<String, usize>,
}

/// In-process map backed by `tokio::sync::RwLock<HashMap<...>>`.
///
/// Used in tests, the dev `docker-compose` stack, and the single-tenant demo
/// path. Swap in a persistent backend (driftdb, Postgres, Redis) for any
/// deployment that needs durability or cross-host visibility.
///
/// Enforces a per-tenant byte quota ([`MemKv::with_capacity_bytes`], default
/// [`DEFAULT_TENANT_CAP_BYTES`]) so one runaway tenant can't grow the map
/// without bound — overwriting a key subtracts the old value's size before
/// adding the new one, so repeated overwrites of the same key don't
/// double-count.
pub struct MemKv {
    inner: RwLock<MemKvInner>,
    tenant_cap_bytes: usize,
}

impl MemKv {
    /// A [`MemKv`] with the default per-tenant quota
    /// ([`DEFAULT_TENANT_CAP_BYTES`]).
    pub fn new() -> Self {
        Self::with_capacity_bytes(DEFAULT_TENANT_CAP_BYTES)
    }

    /// A [`MemKv`] with an explicit per-tenant byte quota.
    pub fn with_capacity_bytes(tenant_cap_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(MemKvInner::default()),
            tenant_cap_bytes,
        }
    }
}

impl Default for MemKv {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl KvStore for MemKv {
    async fn get(&self, tenant: &str, key: &str) -> Option<Vec<u8>> {
        let composite = (tenant.to_string(), key.to_string());
        self.inner.read().await.entries.get(&composite).cloned()
    }

    async fn put(&self, tenant: &str, key: &str, value: Vec<u8>) -> Result<(), KvError> {
        let composite = (tenant.to_string(), key.to_string());
        let mut inner = self.inner.write().await;

        // Charge key + value + a fixed per-entry overhead, so a flood of
        // empty values under unique keys still hits the quota.
        let cost = |v_len: usize| ENTRY_OVERHEAD_BYTES + key.len() + v_len;
        let old_cost = inner
            .entries
            .get(&composite)
            .map(|v| cost(v.len()))
            .unwrap_or(0);
        let new_cost = cost(value.len());
        let used = inner.tenant_bytes.get(tenant).copied().unwrap_or(0);
        // `used` always includes `old_cost` (or is 0 if the key is new), so
        // this subtraction never underflows.
        let new_used = used - old_cost + new_cost;

        if new_used > self.tenant_cap_bytes {
            return Err(KvError::QuotaExceeded {
                used_bytes: used,
                added_bytes: new_cost,
                cap_bytes: self.tenant_cap_bytes,
            });
        }

        inner.tenant_bytes.insert(tenant.to_string(), new_used);
        inner.entries.insert(composite, value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cross_tenant_composite_keys_do_not_collide() {
        let kv = MemKv::new();
        // tenant "a" key "b/x" vs tenant "a/b" key "x" — a host that scoped
        // keys by string concatenation (`"{tenant}/{key}"`) would map both
        // to "a/b/x" and let one tenant read the other's value.
        kv.put("a", "b/x", b"from-a-b-x".to_vec()).await.unwrap();
        kv.put("a/b", "x", b"from-a-slash-b-x".to_vec())
            .await
            .unwrap();

        assert_eq!(kv.get("a", "b/x").await.unwrap(), b"from-a-b-x");
        assert_eq!(kv.get("a/b", "x").await.unwrap(), b"from-a-slash-b-x");
    }

    #[tokio::test]
    async fn put_over_tenant_quota_errors() {
        // Room for exactly one 2-byte key + 10-byte value entry.
        let kv = MemKv::with_capacity_bytes(ENTRY_OVERHEAD_BYTES + 12);
        kv.put("t", "k1", vec![0u8; 10]).await.unwrap();
        let err = kv.put("t", "k2", vec![0u8; 10]).await.unwrap_err();
        assert!(matches!(err, KvError::QuotaExceeded { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn overwrite_does_not_double_count_quota() {
        let kv = MemKv::with_capacity_bytes(ENTRY_OVERHEAD_BYTES + 11);
        kv.put("t", "k", vec![0u8; 10]).await.unwrap();
        // Same key, same size — the old entry's cost is freed before the
        // new one is charged, so this fits exactly.
        kv.put("t", "k", vec![1u8; 10]).await.unwrap();
        assert_eq!(kv.get("t", "k").await.unwrap(), vec![1u8; 10]);
    }

    #[tokio::test]
    async fn empty_values_still_consume_quota() {
        let kv = MemKv::with_capacity_bytes(ENTRY_OVERHEAD_BYTES * 3);
        let mut accepted = 0;
        for i in 0..100 {
            if kv.put("t", &format!("k{i}"), Vec::new()).await.is_ok() {
                accepted += 1;
            }
        }
        assert!(accepted < 3, "accepted {accepted} empty-value entries");
    }
}
