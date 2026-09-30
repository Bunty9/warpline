# Changelog

All notable changes to warpline are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/) (0.x: minor releases may
break).

## [0.2.0] - 2026-09-30

warpline-core is now embeddable: hold a `Runtime`, publish components and
invoke them from your own app. See `examples/storefront` for a complete
reference integration.

### Added

- `warpline_core::Runtime` (with `RuntimeConfig` and `RuntimeBuilder`):
  `stage`, `activate`, `activate_digest`, `deactivate`, `active`, `publish`, `invoke` and `gc`
  behind one cheap-to-clone handle that owns the component cache and epoch
  ticker. Typed `InvokeError`, `PublishError` and `Error`; `InvokeError`
  has `http_status()`.
- Admission control: a memory-weighted admission budget (503 when it cannot
  admit an invocation), a per-tenant in-flight limit (429 when the tenant is
  busy), an output size cap (502) and cancellation-safe metering.
  `Runtime::invoke` re-validates the limits it is given
  (`InvokeError::InvalidLimits`, 400).
- `warpline_core::pg` (cargo feature `postgres`, on by default): `migrate`,
  `connect`, `Authenticator` (per-instance auth cache), tenant admin
  (`create_tenant`, `patch_tenant`, `tenant_limits`, `LimitsPatch`; the control plane's
  PATCH behaviour is unchanged, now a library function), a usage
  summary and a batching, retrying `PgMeter`. Auth lookups time out after
  2 s; if Postgres is down, cached entries are served stale for up to
  `max(10 x TTL, 5 min)`, otherwise the lookup fails (host and control
  answer 503). `Usage`, `MeterSink` and
  `Limits` live at the crate root.
- Metric `warpline_meter_dropped_total`; `PgMeter` warns when metering
  events are dropped.
- `warpline-host` cargo feature `embed-control` (default on) gating the
  `warpline-control` dependency and `WARPLINE_EMBED_CONTROL`.
- `examples/storefront`: an axum app embedding warpline-core, a Rust
  `wasm32-wasip2` checkout hook, a fraud-service mock, docker compose, a
  `demo.sh` walkthrough and an end-to-end test.
- CI job for the example (including a live demo run), release workflow
  publishing via crates.io trusted publishing, Dependabot.

### Changed (breaking)

- **Postgres schema.** Everything now lives in a dedicated `warpline`
  schema, created by a single squashed migration. Data from 0.1 in `public`
  is not migrated: start fresh, or copy the tenants and API keys across
  manually.
- **Migrations are explicit.** Nothing migrates implicitly any more; call
  `warpline_core::pg::migrate(&pool)` at startup. The host and control
  binaries do it for you.
- **Embedding API.** `Runtime` replaces the free functions. The component
  cache, registry, sandbox and `HostCtx` are private; only `warpline_core::digest`
  stays public. Public structs and enums that may grow are `#[non_exhaustive]`.
- **`KvStore::get` is fallible** (returns a `Result`), so a store outage is
  no longer indistinguishable from a missing key.
- **New HTTP status codes** from the invoke path: 503 (overloaded), 429
  (tenant busy) and 502 (guest output too large), in addition to the
  existing mapping. Clients that only handled the old set should treat them
  as retryable (503, 429) or as a guest error (502).
- **`cpu_us` counts epoch ticks** (ticks x 1 ms) rather than wall-derived
  microseconds. A 1 ms budget gets an extra tick plus a bounded
  instantiation grace.
- **`warpline-host` depends on `warpline-control` only behind the
  `embed-control` feature.** Library users who disable default features lose
  `WARPLINE_EMBED_CONTROL`.
- **Environment variables.** Empty values now count as unset, and an
  unparsable `WARPLINE_AUTH_CACHE_TTL_SECS` makes the binary refuse to start
  instead of silently using the default.
- Auth caches are per `Authenticator`, not process-global.

### Fixed

- Registry: GC no longer misbehaves on unreadable pointers, a read-only
  cwasm directory is tolerated, and re-uploading a module bumps its mtime so
  GC keeps it. `PublishError::Registry` reports storage failures accurately.
- Cold cache misses load the on-disk cwasm before queueing for a compile
  permit, take the permit asynchronously (no deadlock on a small blocking
  pool), and recompiles are gated on the compile semaphore.
- CPU budget: exact tick accounting (no off-by-one), and at most the
  instantiation grace is forgiven when starting the call budget; guest start
  functions that loop are now covered by tests.
- Control: the pointer is activated inside the quota transaction and
  restored (or removed) if the commit fails, and reconciled under the lock;
  lock waits for publish and reconcile are bounded; uploads survive client
  disconnects; `activate_digest` validates its input; a cached tenant that
  was deleted now yields 401; graceful shutdown.
- Host: the embedded control task is awaited with a timeout on shutdown, the
  metrics recorder is installed first, and `metrics_handle()` no longer
  panics if a recorder already exists.
- `IssuedKey` redacts the API key in its `Debug` output; `Debug` impls added
  for public types.
- Documentation errors in the README, PROGRESS and doc comments (caps, env
  and status tables).

### Removed

- The free-function invoke API and public access to the cache, registry and
  sandbox internals (use `Runtime`).
- The core `auth.rs` module and the Postgres metering in `meter.rs` (now
  `pg::Authenticator` and `pg::PgMeter`) and the
  process-global auth cache.
- Unused dependencies in `warpline-host` and `warpline-control`.
- The separate second migration (squashed into `0001_warpline.sql`).

## [0.1.0] - 2026-09-28

First release.

### Added

- `warpline-core`: Wasmtime 49 Component Model runtime with epoch-based CPU
  caps, memory caps, content-addressed `.cwasm` warm cache, deny-by-default
  outbound HTTP with per-tenant allowlists and SSRF protection, per-tenant
  key-value store, module registry with atomic activation and GC, API-key
  auth and usage metering (Postgres).
- `warpline-host`: invoke server with Prometheus metrics; optional embedded
  control plane (`WARPLINE_EMBED_CONTROL`).
- `warpline-control`: upload and admin API (tenants, keys, limits, usage).
- WIT world (`warpline.wit`), example guests, criterion benches, Dockerfile
  and fly.io config.
- CI: tests with Postgres, MSRV check, cargo-deny, `cargo package`, guest
  fixture reproducibility.
- crates.io metadata and dual MIT/Apache-2.0 licensing.

[0.2.0]: https://github.com/Bunty9/warpline/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Bunty9/warpline/releases/tag/v0.1.0
