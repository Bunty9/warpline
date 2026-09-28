-- warpline Phase 2 — auth + per-tenant config + invocation outcome.
--
-- api_keys      — bearer tokens for control/host auth. Only the SHA-256
--                 hash of the key is stored (see warpline_core::auth) —
--                 the raw key is returned once, at issue time, and never
--                 persisted.
-- tenants       — gains allowed_hosts / cpu_budget_ms / mem_cap_bytes, the
--                 per-tenant resource caps previously hardcoded in the host.
-- meter         — gains ok: whether the invocation this row records
--                 completed without a guest-visible error/trap.
--
-- 0001_init.sql's content is checksummed by `sqlx::migrate!` and must not
-- change; this migration only adds to what it created.

ALTER TABLE tenants
    ADD COLUMN IF NOT EXISTS allowed_hosts TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS cpu_budget_ms INT NOT NULL DEFAULT 100
        CHECK (cpu_budget_ms BETWEEN 1 AND 10000),
    ADD COLUMN IF NOT EXISTS mem_cap_bytes BIGINT NOT NULL DEFAULT 67108864
        CHECK (mem_cap_bytes BETWEEN 1048576 AND 536870912);

CREATE TABLE IF NOT EXISTS api_keys (
    key_hash   TEXT PRIMARY KEY,
    tenant_id  UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_api_keys_tenant ON api_keys (tenant_id);

ALTER TABLE meter
    ADD COLUMN IF NOT EXISTS ok BOOLEAN NOT NULL DEFAULT true;
