//! Tenant administration: create tenants, issue keys, change limits, read
//! usage. Moved out of the control plane so embedding apps get the same
//! transactional behaviour without running an HTTP server.

use sqlx::types::chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::cache::digest;
use crate::types::{valid_name, ConfigError};
use crate::Limits;

/// Admin operation failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdminError {
    #[error("invalid tenant name (lowercase letters, digits, '_' and '-', max 63)")]
    InvalidName,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("no such tenant: {0}")]
    NoSuchTenant(String),
    #[error("could not generate an API key: {0}")]
    Rng(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// A freshly issued API key. The raw key exists only here: only its SHA-256
/// hash is stored, so it cannot be shown again.
#[derive(Clone)]
#[non_exhaustive]
pub struct IssuedKey {
    pub tenant: String,
    pub api_key: String,
}

impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedKey")
            .field("tenant", &self.tenant)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Convert validated limits to their column types.
fn cols(l: &Limits) -> (Vec<String>, i32, i64) {
    (
        l.allowed_hosts.clone(),
        l.cpu_budget_ms as i32,
        l.mem_cap_bytes as i64,
    )
}

/// A partial change to a tenant's [`Limits`]: `None` fields keep the
/// tenant's current value (or the default, for a tenant being created).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct LimitsPatch {
    pub allowed_hosts: Option<Vec<String>>,
    pub cpu_budget_ms: Option<u64>,
    pub mem_cap_bytes: Option<usize>,
}

impl From<&Limits> for LimitsPatch {
    fn from(l: &Limits) -> Self {
        Self {
            allowed_hosts: Some(l.allowed_hosts.clone()),
            cpu_budget_ms: Some(l.cpu_budget_ms),
            mem_cap_bytes: Some(l.mem_cap_bytes),
        }
    }
}

/// Create the tenant if missing and issue it a new API key (each call
/// issues another; earlier keys keep working). With `Some(limits)` the
/// tenant's limits are set to them; with `None` a new tenant gets the
/// defaults and an existing one is left as it was. Tenant upsert, limits
/// and key insert happen in one transaction.
///
/// `limits` are re-validated here: its fields are public, so a caller can
/// mutate a valid value into an invalid one.
pub async fn create_tenant(
    pool: &PgPool,
    name: &str,
    limits: Option<&Limits>,
) -> Result<IssuedKey, AdminError> {
    patch_tenant(
        pool,
        name,
        &limits.map(LimitsPatch::from).unwrap_or_default(),
    )
    .await
}

/// Like [`create_tenant`], but only the fields set in `patch` change; the
/// merge with the tenant's current values happens inside the upsert, so
/// concurrent patches of different fields cannot overwrite each other.
pub async fn patch_tenant(
    pool: &PgPool,
    name: &str,
    patch: &LimitsPatch,
) -> Result<IssuedKey, AdminError> {
    if !valid_name(name) {
        return Err(AdminError::InvalidName);
    }
    let defaults = Limits::default();
    let cpu = patch.cpu_budget_ms.unwrap_or(defaults.cpu_budget_ms);
    let mem = patch.mem_cap_bytes.unwrap_or(defaults.mem_cap_bytes);
    let hosts = patch.allowed_hosts.as_deref().unwrap_or(&[]);
    // Validates every field that is present (and the defaults, harmlessly).
    let checked = Limits::new(cpu, mem)?.with_allowed_hosts(hosts)?;

    let mut key_bytes = [0u8; 32];
    getrandom::fill(&mut key_bytes).map_err(|e| AdminError::Rng(e.to_string()))?;
    let api_key = format!("wl_{}", hex::encode(key_bytes));
    let key_hash = digest(api_key.as_bytes());

    let (hosts, cpu, mem) = cols(&checked);
    let mut tx = pool.begin().await?;
    let tenant_id: sqlx::types::Uuid = sqlx::query_scalar(
        "INSERT INTO warpline.tenants (name, allowed_hosts, cpu_budget_ms, mem_cap_bytes) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (name) DO UPDATE SET \
            allowed_hosts = CASE WHEN $5 THEN EXCLUDED.allowed_hosts ELSE tenants.allowed_hosts END, \
            cpu_budget_ms = CASE WHEN $6 THEN EXCLUDED.cpu_budget_ms ELSE tenants.cpu_budget_ms END, \
            mem_cap_bytes = CASE WHEN $7 THEN EXCLUDED.mem_cap_bytes ELSE tenants.mem_cap_bytes END \
         RETURNING id",
    )
    .bind(name)
    .bind(hosts)
    .bind(cpu)
    .bind(mem)
    .bind(patch.allowed_hosts.is_some())
    .bind(patch.cpu_budget_ms.is_some())
    .bind(patch.mem_cap_bytes.is_some())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO warpline.api_keys (key_hash, tenant_id) VALUES ($1, $2)")
        .bind(&key_hash)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(IssuedKey {
        tenant: name.to_string(),
        api_key,
    })
}

/// A tenant's stored limits, or `None` if it does not exist.
pub async fn tenant_limits(pool: &PgPool, name: &str) -> Result<Option<Limits>, AdminError> {
    let row: Option<(Vec<String>, i32, i64)> = sqlx::query_as(
        "SELECT allowed_hosts, cpu_budget_ms, mem_cap_bytes FROM warpline.tenants WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(allowed_hosts, cpu, mem)| Limits {
        cpu_budget_ms: cpu as u64,
        mem_cap_bytes: mem as usize,
        allowed_hosts,
    }))
}

/// Replace an existing tenant's limits. Authenticators with a cache see the
/// change after their TTL.
pub async fn set_limits(pool: &PgPool, name: &str, limits: &Limits) -> Result<(), AdminError> {
    let l = Limits::new(limits.cpu_budget_ms, limits.mem_cap_bytes)?
        .with_allowed_hosts(&limits.allowed_hosts)?;
    let (hosts, cpu, mem) = cols(&l);
    let done = sqlx::query(
        "UPDATE warpline.tenants SET allowed_hosts = $2, cpu_budget_ms = $3, mem_cap_bytes = $4 \
         WHERE name = $1",
    )
    .bind(name)
    .bind(hosts)
    .bind(cpu)
    .bind(mem)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AdminError::NoSuchTenant(name.to_string()));
    }
    Ok(())
}

/// Totals over `warpline.meter` rows for one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UsageSummary {
    pub invocations: i64,
    pub errors: i64,
    pub cpu_us: i64,
    pub wall_us: i64,
    pub max_mem_peak_bytes: i64,
}

/// Sum a tenant's metered usage at or after `since` (UTC, `chrono`
/// re-exported by sqlx as `sqlx::types::chrono`). All zeros if none.
pub async fn usage_summary(
    pool: &PgPool,
    tenant: &str,
    since: DateTime<Utc>,
) -> Result<UsageSummary, AdminError> {
    let (invocations, errors, cpu_us, wall_us, max_mem_peak_bytes): (i64, i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE NOT ok), \
                    COALESCE(sum(cpu_us), 0)::bigint, COALESCE(sum(wall_us), 0)::bigint, \
                    COALESCE(max(mem_peak_bytes), 0)::bigint \
             FROM warpline.meter WHERE tenant = $1 AND ts >= $2",
        )
        .bind(tenant)
        .bind(since)
        .fetch_one(pool)
        .await?;
    Ok(UsageSummary {
        invocations,
        errors,
        cpu_us,
        wall_us,
        max_mem_peak_bytes,
    })
}
