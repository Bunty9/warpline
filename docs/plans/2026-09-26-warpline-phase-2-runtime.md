---
title: warpline Phase 2 — Live runtime (Component Model, host fns, registry, metering)
status: done
date: 2026-09-26
related:
    - ../specs/2026-05-28-warpline-design.md
    - ./2026-05-28-warpline-phase-1-scaffold.md
---

# warpline Phase 2 — Live runtime

> **Goal:** turn the Phase-1 scaffold into a working function host: upload a
> component, invoke it over HTTP, get its bytes back, with CPU/memory caps,
> tenant-scoped KV, logging, allowlisted outbound HTTP, API-key auth and a
> metering row per call.

## Decisions

- **wasmtime 27 → 49.** `cargo deny check` reports ~25 RUSTSEC advisories
  against wasmtime/wasmtime-wasi 27, several of them sandbox escapes. A
  multi-tenant sandbox cannot ship on that. MSRV rises accordingly.
- **Component Model, not core modules.** `wit/warpline.wit` already
  describes a component world. Host uses `wasmtime::component::bindgen!`
  against it; guests use `wit-bindgen` and target `wasm32-wasip2`, which
  emits components directly (no adapter step).
- **WASI p2 is deny-by-default.** Guests get a `WasiCtx` with no preopens,
  no env, no args, no sockets — only what std needs to link (clocks,
  random, stdio to a sink).
- **CPU budget = epoch deadline.** One engine-wide ticker thread bumps the
  epoch every `EPOCH_TICK_MS`; each store sets its deadline to
  `budget_ms / EPOCH_TICK_MS` ticks. Replaces the per-call `tokio::spawn`
  ticker, which bumped the epoch for *every* concurrent store.
- **Limiter lives in `HostCtx`.** Removes the `Box::leak` per call and lets
  the limiter record peak memory for metering.
- **Module registry on shared disk.** Control plane writes
  `modules/{sha256}.wasm` + `modules/{sha256}.cwasm` and a pointer file
  `modules/{tenant}/{func}` containing the hash. Host reads the pointer,
  deserialises the `.cwasm` and keeps an in-memory LRU of loaded
  components keyed by hash. S3 stays out of scope (see "Deferred").
- **Postgres is optional in dev.** With `DATABASE_URL` set: migrations run
  on boot, API keys are checked, metering rows are written. Without it, the
  binaries refuse to start unless `WARPLINE_INSECURE_DEV=1`.

## Tasks

1. [x] Runtime core — deps upgrade, bindgen host, live kv/log/http-out,
   epoch ticker, limiter-in-ctx, component cache, test guest + tests.
2. [x] Registry + control/host wiring — pointer files, LRU, auth, metering,
   migrations on boot, upload size cap, name validation.
3. [x] Bench harness (criterion) + numbers in PROGRESS.md.
4. [x] Docs, CI, Docker/compose refresh; `cargo deny` green.

## Deferred

- S3/MinIO `.cwasm` backend (local shared volume covers one-box deploys).
- Instance pooling / warm-store reuse (pooling allocator).
- Hyperlight.

## Outcome

Landed across `be938b4` (live Component Model runtime), `48f09c6` (runtime
caps/KV isolation/cache/egress hardening), `8f8d84e` (registry, API-key
auth, tenant config, metering), `1490999` (registry/upload/metering/metrics
hardening), `57901fb` (bench harness + measured numbers), and `046cc0e`
(epoch-ticker drift fix). All four tasks above are done; every decision in
"Decisions" above shipped as described, with these deviations from the
original task list:

- **Metering is batched, not per-call.** The plan said "wire
  `meter::record` on the invoke hot path." What shipped instead is a
  bounded `mpsc` channel (`meter::spawn_writer`, capacity 10,000, batches
  of up to 200) draining into a single writer task — a burst of invokes no
  longer spawns one Postgres write per request, and a full channel drops
  the row (counted via `warpline_meter_dropped_total`) instead of blocking
  the response.
- **`/metrics` is a separate listener.** Not called out in the original
  plan. `warpline-host` serves Prometheus metrics on its own
  `axum::serve` instance (`WARPLINE_METRICS_BIND`, default
  `127.0.0.1:9090`), not on the invoke router — a scrape endpoint exposing
  per-tenant series shouldn't share a socket with whatever's publicly
  reachable.
- **Source `.wasm` retention.** The plan described the registry as
  `.wasm` + `.cwasm` written by the control plane and read by the host; it
  didn't call out that the source `.wasm` needs to *stay* on disk. It does
  (`modules/wasm/{digest}.wasm`): the `.cwasm`'s cache key folds in
  `Engine::precompile_compatibility_hash()`, so an engine/config upgrade
  (or simple corruption) invalidates or removes the `.cwasm` without
  warning, and `ComponentCache::get_or_load` needs the source to recompile
  from rather than 500ing every existing pointer forever.
- **Epoch ticker drift fix.** Not part of the original plan — found while
  gathering the Phase 2 bench numbers (`PROGRESS.md`'s "Notes on the
  misses"). The ticker originally slept a relative `EPOCH_TICK_MS` per
  iteration; the overshoot compounded because budgets are counted in
  ticks (~+12% at a 100 ms budget). It now sleeps to an absolute schedule
  instead, landed in `046cc0e`.
- **Upload "rate-limit" became concurrency + count caps, not a rate
  limiter.** The plan's task 2 mentioned "rate-limit uploads"; what
  shipped is a process-wide compile-concurrency semaphore
  (`max(available_parallelism / 2, 1)` permits) plus a 100-functions-per-
  tenant quota, not a time-window rate limit. Tracked as real remaining
  work in `PROGRESS.md`'s Phase 3 list.
