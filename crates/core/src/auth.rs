//! Bearer-token auth + tenant config resolution, and the optional-Postgres
//! bootstrap that backs both. Shared by `warpline-host` and
//! `warpline-control` so "is there a database, and if not are we allowed to
//! run anyway" lives in one enum ([`DbState`]) instead of an env check
//! scattered across every call site — see the Phase-2 plan's "Postgres
//! optional in dev" decision.
//!
//! [`authenticate`] is on the hot path of every request (host invoke *and*
//! control upload), so every bearer token hits a process-local, bounded TTL
//! cache ([`AUTH_CACHE`]) before it ever touches Postgres — both a valid
//! key's outcome and an unknown key's absence are cached, so a client
//! hammering a bad token can't turn into a pool-exhausting query storm
//! either. **Consequence: a config change or key revocation can take up to
//! `WARPLINE_AUTH_CACHE_TTL_SECS` (default 30) to take effect** — the admin
//! route itself never reads this cache, so it always sees/writes current
//! state, but a *different*, already-cached request against the same
//! tenant/key can still observe the old config for up to that long.
//! `WARPLINE_AUTH_CACHE_TTL_SECS=0` disables the cache outright (every call
//! hits Postgres), which is what tests that change a config and immediately
//! expect it to be visible through [`authenticate`] should set.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use lru::LruCache;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::cache::digest;

/// Entry count cap for [`AUTH_CACHE`] — bounds worst-case memory from a
/// flood of distinct (mostly bogus) bearer tokens.
const AUTH_CACHE_CAP: usize = 10_000;

/// What [`authenticate`] caches per key hash: either a resolved tenant (a
/// "positive" hit) or the fact that no key matched at all (a "negative"
/// hit, so a client hammering a bad token doesn't hit Postgres every time
/// either).
#[derive(Clone)]
enum CachedAuth {
    Found {
        id: Option<uuid::Uuid>,
        tenant_name: String,
        config: TenantConfig,
    },
    NotFound,
}

/// Process-wide `key_hash -> (CachedAuth, expiry)`. A `Mutex<LruCache>`
/// rather than a sharded/lock-free cache — ponytail: one global lock around
/// a cache lookup is far cheaper than the Postgres round-trip it replaces;
/// revisit only if profiling ever shows contention here.
static AUTH_CACHE: OnceLock<Mutex<LruCache<String, (Instant, CachedAuth)>>> = OnceLock::new();

fn auth_cache() -> &'static Mutex<LruCache<String, (Instant, CachedAuth)>> {
    AUTH_CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            std::num::NonZeroUsize::new(AUTH_CACHE_CAP).unwrap(),
        ))
    })
}

/// `WARPLINE_AUTH_CACHE_TTL_SECS`, default 30. `0` disables the cache (every
/// [`authenticate`] call goes straight to Postgres) — see module docs.
fn auth_cache_ttl() -> Duration {
    parse_ttl_secs(std::env::var("WARPLINE_AUTH_CACHE_TTL_SECS").ok())
}

/// Pure parse of the `WARPLINE_AUTH_CACHE_TTL_SECS` value, split out of
/// [`auth_cache_ttl`] so it's testable without touching process env state.
/// Anything unset or unparseable falls back to the 30s default rather than
/// disabling the cache — only an explicit `"0"` does that.
fn parse_ttl_secs(raw: Option<String>) -> Duration {
    let secs = raw.and_then(|s| s.parse::<u64>().ok()).unwrap_or(30);
    Duration::from_secs(secs)
}

/// Migrations embedded at compile time. The path is relative to *this*
/// crate's manifest dir (`crates/core`) — one level under the repo root,
/// same as `crates/host` and `crates/control` — so it resolves to
/// `<repo>/migrations` regardless of which binary links this in.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Per-tenant resource caps + outbound-HTTP allowlist. Comes from the
/// `tenants` table when [`DbState::Postgres`], or a fixed default under
/// [`DbState::InsecureDev`].
#[derive(Debug, Clone, PartialEq)]
pub struct TenantConfig {
    pub allowed_hosts: Vec<String>,
    pub cpu_budget_ms: u64,
    pub mem_cap_bytes: usize,
}

impl TenantConfig {
    /// Empty allowlist, 100 ms CPU, 64 MiB memory — applied to every tenant
    /// when running without Postgres.
    pub fn dev_default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            cpu_budget_ms: 100,
            mem_cap_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Optional-Postgres process state.
#[derive(Clone)]
pub enum DbState {
    /// `DATABASE_URL` was set: auth is enforced, metering rows are
    /// written, migrations already ran on boot.
    Postgres(PgPool),
    /// No `DATABASE_URL`, `WARPLINE_INSECURE_DEV=1` set instead: no auth,
    /// [`TenantConfig::dev_default`] for every tenant, metering just
    /// logged at debug level.
    InsecureDev,
}

impl DbState {
    /// `DATABASE_URL` set -> [`Self::connect_to`] it. Unset -> refuses
    /// unless `WARPLINE_INSECURE_DEV=1`, in which case [`Self::InsecureDev`].
    pub async fn connect() -> anyhow::Result<Self> {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            anyhow::ensure!(
                std::env::var("WARPLINE_INSECURE_DEV").as_deref() == Ok("1"),
                "DATABASE_URL is not set. Refusing to start without a database \
                 (no auth would be enforced and no invocations would be metered). \
                 Set WARPLINE_INSECURE_DEV=1 to run without one in local dev."
            );
            tracing::warn!(
                "WARPLINE_INSECURE_DEV=1: starting without Postgres — no auth is \
                 enforced, every tenant gets the default resource caps, and \
                 invocations are logged instead of metered. Do not run this in \
                 production."
            );
            return Ok(Self::InsecureDev);
        };
        Self::connect_to(&url).await
    }

    /// Connect directly to `url` and run migrations, bypassing the
    /// `DATABASE_URL` env var. Used by [`Self::connect`] and by DB-backed
    /// tests, which each want their own pool against a fixed test Postgres
    /// instance without mutating process-wide env state.
    pub async fn connect_to(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            // Default is 30s: with every bearer request hitting this pool
            // (see module docs on `AUTH_CACHE`), a saturated pool would
            // otherwise let a burst of requests pile up for 30s each
            // instead of failing fast (finding: auth pool-exhaustion DoS).
            .acquire_timeout(Duration::from_secs(2))
            .connect(url)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self::Postgres(pool))
    }

    pub fn pool(&self) -> Option<&PgPool> {
        match self {
            Self::Postgres(pool) => Some(pool),
            Self::InsecureDev => None,
        }
    }
}

/// A successfully authenticated caller: its resolved [`TenantConfig`], plus
/// its `tenants.id` when [`DbState::Postgres`] (`None` under
/// [`DbState::InsecureDev`], where there's no row to point at — used by
/// `warpline-control` to upsert the `functions` row on upload).
#[derive(Debug, Clone)]
pub struct AuthedTenant {
    pub id: Option<uuid::Uuid>,
    pub config: TenantConfig,
}

/// Result of [`authenticate`].
pub enum AuthOutcome {
    Ok(AuthedTenant),
    /// No bearer token, or one that doesn't match any stored key hash —
    /// callers map this to `401`.
    Unauthorized,
    /// A valid key whose tenant doesn't match the path's tenant — callers
    /// map this to `403`.
    Forbidden,
}

/// Authenticate `bearer_token` as a caller acting on `tenant` (the path
/// segment). Under [`DbState::InsecureDev`] every call succeeds with
/// [`TenantConfig::dev_default`], regardless of the token.
pub async fn authenticate(
    db: &DbState,
    tenant: &str,
    bearer_token: Option<&str>,
) -> anyhow::Result<AuthOutcome> {
    let pool = match db {
        DbState::InsecureDev => {
            return Ok(AuthOutcome::Ok(AuthedTenant {
                id: None,
                config: TenantConfig::dev_default(),
            }))
        }
        DbState::Postgres(pool) => pool,
    };
    let Some(token) = bearer_token else {
        return Ok(AuthOutcome::Unauthorized);
    };
    let key_hash = digest(token.as_bytes());
    let ttl = auth_cache_ttl();

    if ttl > Duration::ZERO {
        if let Some((cached_at, cached)) = auth_cache().lock().unwrap().get(&key_hash).cloned() {
            if cached_at.elapsed() < ttl {
                return Ok(outcome_from_cached(cached, tenant));
            }
        }
    }

    let row: Option<(uuid::Uuid, String, Vec<String>, i32, i64)> = sqlx::query_as(
        "SELECT t.id, t.name, t.allowed_hosts, t.cpu_budget_ms, t.mem_cap_bytes \
         FROM api_keys k JOIN tenants t ON t.id = k.tenant_id \
         WHERE k.key_hash = $1",
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await?;

    let cached = match row {
        None => CachedAuth::NotFound,
        Some((id, tenant_name, allowed_hosts, cpu_budget_ms, mem_cap_bytes)) => CachedAuth::Found {
            id: Some(id),
            tenant_name,
            config: TenantConfig {
                allowed_hosts,
                cpu_budget_ms: cpu_budget_ms as u64,
                mem_cap_bytes: mem_cap_bytes as usize,
            },
        },
    };
    if ttl > Duration::ZERO {
        auth_cache()
            .lock()
            .unwrap()
            .put(key_hash, (Instant::now(), cached.clone()));
    }
    Ok(outcome_from_cached(cached, tenant))
}

/// Turn a cached (or freshly-fetched) lookup into the [`AuthOutcome`] for
/// `requested_tenant` — split out of [`authenticate`] so both the
/// cache-hit and cache-miss paths apply the same tenant-match check.
fn outcome_from_cached(cached: CachedAuth, requested_tenant: &str) -> AuthOutcome {
    match cached {
        CachedAuth::NotFound => AuthOutcome::Unauthorized,
        CachedAuth::Found {
            id,
            tenant_name,
            config,
        } => {
            if tenant_name != requested_tenant {
                return AuthOutcome::Forbidden;
            }
            AuthOutcome::Ok(AuthedTenant { id, config })
        }
    }
}

/// Strip a `"Bearer "` scheme from an `Authorization` header value. The
/// scheme name is matched case-insensitively (RFC 7235 auth-schemes are
/// case-insensitive) and the token is trimmed of surrounding whitespace;
/// an empty token after trimming is treated as absent.
pub fn parse_bearer(header_value: &str) -> Option<&str> {
    let (scheme, rest) = header_value.trim().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::{outcome_from_cached, parse_bearer, parse_ttl_secs, AuthOutcome, CachedAuth};

    #[test]
    fn ttl_secs_defaults_and_zero_disables() {
        assert_eq!(parse_ttl_secs(None), std::time::Duration::from_secs(30));
        assert_eq!(
            parse_ttl_secs(Some("garbage".to_string())),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            parse_ttl_secs(Some("0".to_string())),
            std::time::Duration::ZERO
        );
        assert_eq!(
            parse_ttl_secs(Some("5".to_string())),
            std::time::Duration::from_secs(5)
        );
    }

    #[test]
    fn cached_not_found_is_unauthorized() {
        assert!(matches!(
            outcome_from_cached(CachedAuth::NotFound, "any-tenant"),
            AuthOutcome::Unauthorized
        ));
    }

    #[test]
    fn cached_found_checks_tenant_match() {
        let cached = CachedAuth::Found {
            id: None,
            tenant_name: "tenant-a".to_string(),
            config: super::TenantConfig::dev_default(),
        };
        assert!(matches!(
            outcome_from_cached(cached.clone(), "tenant-a"),
            AuthOutcome::Ok(_)
        ));
        assert!(matches!(
            outcome_from_cached(cached, "tenant-b"),
            AuthOutcome::Forbidden
        ));
    }

    #[test]
    fn parse_bearer_strips_prefix() {
        assert_eq!(parse_bearer("Bearer wl_abc"), Some("wl_abc"));
        assert_eq!(parse_bearer("wl_abc"), None);
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn parse_bearer_is_case_insensitive_and_trims_token() {
        assert_eq!(parse_bearer("bearer wl_abc"), Some("wl_abc"));
        assert_eq!(parse_bearer("BEARER wl_abc"), Some("wl_abc"));
        assert_eq!(parse_bearer("  Bearer   wl_abc  "), Some("wl_abc"));
        assert_eq!(parse_bearer("Bearer    "), None);
        assert_eq!(parse_bearer("Bearer"), None);
    }
}
