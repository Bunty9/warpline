//! Bearer-token auth against `warpline.api_keys`.
//!
//! [`Authenticator::authenticate`] is on the hot path of every request, so
//! each lookup goes through a bounded per-instance TTL cache; both a valid
//! key and an unknown key are cached, so a client hammering a bad token
//! can't become a query storm. **Consequence: a limits change or key
//! revocation can take up to the TTL to be seen by this instance.** A TTL
//! of zero disables the cache.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use sqlx::PgPool;

use crate::cache::digest;
use crate::Limits;

/// Bounds worst-case memory from a flood of distinct bogus tokens.
const CACHE_CAP: usize = 10_000;

/// Result of [`Authenticator::authenticate`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthOutcome {
    /// Valid key for the requested tenant, with that tenant's limits.
    Authorized(Limits),
    /// No token, or one matching no stored key (HTTP 401).
    MissingOrUnknownKey,
    /// A valid key that belongs to a different tenant (HTTP 403).
    WrongTenant,
}

/// [`Authenticator::authenticate`] failure (the lookup itself, not a denial).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("auth lookup failed: {0}")]
    Db(#[from] sqlx::Error),
}

/// What is cached per key hash. Also what the DB row decodes to.
#[derive(Clone)]
enum Cached {
    Found { tenant: String, limits: Limits },
    Unknown,
}

struct Inner {
    pool: PgPool,
    ttl: Duration,
    cache: Mutex<LruCache<String, (Instant, Cached)>>,
}

/// Cheap to clone; clones share the pool and the cache.
#[derive(Clone)]
pub struct Authenticator(Arc<Inner>);

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("ttl", &self.0.ttl)
            .finish_non_exhaustive()
    }
}

impl Authenticator {
    pub fn new(pool: PgPool, ttl: Duration) -> Self {
        Self(Arc::new(Inner {
            pool,
            ttl,
            // ponytail: one lock around a cache lookup is far cheaper than
            // the round-trip it saves; shard it only if profiling says so.
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_CAP).unwrap())),
        }))
    }

    /// The pool this authenticator queries (handy for the admin functions).
    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    /// Authenticate `bearer` as a caller acting on `tenant`.
    pub async fn authenticate(
        &self,
        tenant: &str,
        bearer: Option<&str>,
    ) -> Result<AuthOutcome, AuthError> {
        let Some(token) = bearer else {
            return Ok(AuthOutcome::MissingOrUnknownKey);
        };
        let inner = &*self.0;
        let key_hash = digest(token.as_bytes());

        if !inner.ttl.is_zero() {
            let hit = inner.cache.lock().unwrap().get(&key_hash).cloned();
            if let Some((at, cached)) = hit {
                if at.elapsed() < inner.ttl {
                    return Ok(outcome(cached, tenant));
                }
            }
        }

        let row: Option<(String, Vec<String>, i32, i64)> = sqlx::query_as(
            "SELECT t.name, t.allowed_hosts, t.cpu_budget_ms, t.mem_cap_bytes \
             FROM warpline.api_keys k JOIN warpline.tenants t ON t.id = k.tenant_id \
             WHERE k.key_hash = $1",
        )
        .bind(&key_hash)
        .fetch_optional(&inner.pool)
        .await?;
        let cached = match row {
            None => Cached::Unknown,
            Some((tenant, allowed_hosts, cpu, mem)) => {
                let limits = Limits {
                    cpu_budget_ms: cpu as u64,
                    mem_cap_bytes: mem as usize,
                    allowed_hosts,
                };
                Cached::Found { tenant, limits }
            }
        };
        if !inner.ttl.is_zero() {
            inner
                .cache
                .lock()
                .unwrap()
                .put(key_hash, (Instant::now(), cached.clone()));
        }
        Ok(outcome(cached, tenant))
    }
}

fn outcome(cached: Cached, requested: &str) -> AuthOutcome {
    match cached {
        Cached::Unknown => AuthOutcome::MissingOrUnknownKey,
        Cached::Found { tenant, .. } if tenant != requested => AuthOutcome::WrongTenant,
        Cached::Found { limits, .. } => AuthOutcome::Authorized(limits),
    }
}
