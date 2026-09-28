-- warpline Phase 1 schema.
--
-- Three tables:
--   tenants   — registered customer accounts.
--   functions — uploaded module manifest: (tenant, name) -> content hash of
--               the source wasm. The .cwasm cache key derives from the hash.
--   meter     — append-only per-invocation ledger. Input table for the
--               billing rollup job (out of scope for Phase 1).
--
-- Run against Postgres 14+. `pgcrypto` is required for `gen_random_uuid()`.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS tenants (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS functions (
    tenant_id  UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    wasm_hash  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS meter (
    id              BIGSERIAL PRIMARY KEY,
    tenant          TEXT        NOT NULL,
    func            TEXT        NOT NULL,
    cpu_us          BIGINT      NOT NULL,
    mem_peak_bytes  BIGINT      NOT NULL,
    ts              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_meter_tenant_ts ON meter (tenant, ts DESC);
