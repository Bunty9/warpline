---
title: warpline Phase 2 — Live runtime (Component Model, host fns, registry, metering)
status: in-progress
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

1. Runtime core — deps upgrade, bindgen host, live kv/log/http-out,
   epoch ticker, limiter-in-ctx, component cache, test guest + tests.
2. Registry + control/host wiring — pointer files, LRU, auth, metering,
   migrations on boot, upload size cap, name validation.
3. Bench harness (criterion) + numbers in PROGRESS.md.
4. Docs, CI, Docker/compose refresh; `cargo deny` green.

## Deferred

- S3/MinIO `.cwasm` backend (local shared volume covers one-box deploys).
- Instance pooling / warm-store reuse (pooling allocator).
- Hyperlight.
