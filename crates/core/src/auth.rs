//! Bearer-token auth + tenant config resolution, and the optional-Postgres
//! bootstrap that backs both. Shared by `warpline-host` and
//! `warpline-control` so "is there a database, and if not are we allowed to
//! run anyway" lives in one enum ([`DbState`]) instead of an env check
//! scattered across every call site — see the Phase-2 plan's "Postgres
//! optional in dev" decision.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::cache::digest;

/// Migrations embedded at compile time. The path is relative to *this*
/// crate's manifest dir (`crates/core`) — one level under the repo root,
/// same as `crates/host` and `crates/control` — so it resolves to
/// `<repo>/migrations` regardless of which binary links this in.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

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
        let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;
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
    let row: Option<(uuid::Uuid, String, Vec<String>, i32, i64)> = sqlx::query_as(
        "SELECT t.id, t.name, t.allowed_hosts, t.cpu_budget_ms, t.mem_cap_bytes \
         FROM api_keys k JOIN tenants t ON t.id = k.tenant_id \
         WHERE k.key_hash = $1",
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await?;
    let Some((id, tenant_name, allowed_hosts, cpu_budget_ms, mem_cap_bytes)) = row else {
        return Ok(AuthOutcome::Unauthorized);
    };
    if tenant_name != tenant {
        return Ok(AuthOutcome::Forbidden);
    }
    Ok(AuthOutcome::Ok(AuthedTenant {
        id: Some(id),
        config: TenantConfig {
            allowed_hosts,
            cpu_budget_ms: cpu_budget_ms as u64,
            mem_cap_bytes: mem_cap_bytes as usize,
        },
    }))
}

/// Strip a leading `"Bearer "` from an `Authorization` header value.
pub fn parse_bearer(header_value: &str) -> Option<&str> {
    header_value.strip_prefix("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::parse_bearer;

    #[test]
    fn parse_bearer_strips_prefix() {
        assert_eq!(parse_bearer("Bearer wl_abc"), Some("wl_abc"));
        assert_eq!(parse_bearer("wl_abc"), None);
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer(""), None);
    }
}
