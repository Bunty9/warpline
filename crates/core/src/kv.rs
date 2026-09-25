//! Per-tenant key/value store abstraction.
//!
//! The trait is intentionally tiny — `get` / `put`, no batch, no scan — so
//! that any of the downstream candidate backends (in-process [`MemKv`], the
//! P5 `driftdb` LSM, Redis, S3) drop in behind the same surface. Keys are
//! pre-scoped by the host before they reach the implementation (`HostCtx`
//! prefixes them with `t/{tenant_id}/`); the trait sees only opaque strings.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

/// Bytes-in, bytes-out KV interface used by the `warpline:host/kv` capability.
#[async_trait]
pub trait KvStore: Send + Sync {
    /// Look up `k`. Returns `None` if the key is absent.
    async fn get(&self, k: &str) -> Option<Vec<u8>>;
    /// Write (or overwrite) `k -> v`.
    async fn put(&self, k: &str, v: Vec<u8>);
}

/// In-process map backed by `tokio::sync::RwLock<HashMap<...>>`.
///
/// Used in tests, the dev `docker-compose` stack, and the single-tenant demo
/// path. Swap in a persistent backend (driftdb, Postgres, Redis) for any
/// deployment that needs durability or cross-host visibility.
#[derive(Default)]
pub struct MemKv {
    inner: RwLock<HashMap<String, Vec<u8>>>,
}

impl MemKv {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl KvStore for MemKv {
    async fn get(&self, k: &str) -> Option<Vec<u8>> {
        self.inner.read().await.get(k).cloned()
    }
    async fn put(&self, k: &str, v: Vec<u8>) {
        self.inner.write().await.insert(k.to_string(), v);
    }
}
