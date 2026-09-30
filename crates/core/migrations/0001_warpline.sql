-- warpline schema. Everything lives in the `warpline` schema so embedding
-- apps can share a database without name clashes; `pg::migrate` creates the
-- schema and points `search_path` at it (so `_sqlx_migrations` lands there
-- too). Object names are qualified anyway so this file never touches
-- another schema.
--
--   tenants   — accounts plus their resource limits (CHECKs mirror the
--               validators in `warpline_core::types`).
--   api_keys  — SHA-256 hashes of bearer keys; the raw key is shown once.
--   functions — (tenant, name) -> content hash of the uploaded wasm.
--   meter     — append-only per-invocation ledger.
--
-- Postgres 13+: `gen_random_uuid()` is built in.

CREATE TABLE warpline.tenants (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name          TEXT NOT NULL UNIQUE,
    allowed_hosts TEXT[] NOT NULL DEFAULT '{}',
    cpu_budget_ms INT NOT NULL DEFAULT 100
        CHECK (cpu_budget_ms BETWEEN 1 AND 10000),
    mem_cap_bytes BIGINT NOT NULL DEFAULT 67108864
        CHECK (mem_cap_bytes BETWEEN 1048576 AND 536870912),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE warpline.api_keys (
    key_hash   TEXT PRIMARY KEY,
    tenant_id  UUID NOT NULL REFERENCES warpline.tenants(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_api_keys_tenant ON warpline.api_keys (tenant_id);

CREATE TABLE warpline.functions (
    tenant_id  UUID NOT NULL REFERENCES warpline.tenants(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    wasm_hash  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, name)
);

CREATE TABLE warpline.meter (
    id             BIGSERIAL PRIMARY KEY,
    tenant         TEXT        NOT NULL,
    func           TEXT        NOT NULL,
    cpu_us         BIGINT      NOT NULL,
    wall_us        BIGINT      NOT NULL,
    mem_peak_bytes BIGINT      NOT NULL,
    ok             BOOLEAN     NOT NULL,
    ts             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_meter_tenant_ts ON warpline.meter (tenant, ts DESC);
