# warpline

> Multi-tenant WASM function runtime in Rust. Wasmtime host + control plane,
> epoch-based CPU caps, `.cwasm` content-hashed warm cache, deny-by-default
> outbound HTTP. Built as the cloud-runtime portfolio project for the Rust
> Level-4 roadmap — the Cloudflare Workers / Fastly Compute / Fermyon Spin
> pattern, scoped to one Hetzner box and ten clients.

[![ci](https://img.shields.io/badge/ci-pending-lightgrey.svg)](./.github/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## The problem

Customer-customization is a recurring need in any SaaS or agency product —
clients want hooks into automation flows. Today this usually means "open a
PR and redeploy." A WASM function host lets clients upload sandboxed code
in any language that compiles to WASI, with real resource caps and a
metered capability surface. **warpline** is that host, sized to embed into
a real customer pilot on one Fly.io / Hetzner box, with the engineering
defenses (cwasm cache, epoch CPU caps, http-out allowlist) the interview
panels want to talk about.

## Architecture

```
+---------+     POST /tenants/{t}/functions/{f}     +-----------------------+
| client  | ---------------------------------------> | warpline-control      |
+----+----+                                          |  - .wasm validate     |
     |                                               |  - compile to .cwasm  |
     | POST /tenants/{t}/functions/{f}/invoke        |  - store on disk/S3   |
     |                                               +-----+-----------------+
     v                                                     |
+----+----------+      pull .cwasm by content hash         |
| warpline-host |<------------------------------------------+
|  (axum API)   |
+----+----------+
     |
     | spawn instance (or reuse warm) with capability set
     v
+----+----------+
| wasmtime     |  Store::set_epoch_deadline + Engine::increment_epoch
| Store        |  ResourceLimiter (memory cap, table cap)
|   - host_kv  |  Host imports: kv_get/kv_put (backed by P5 driftdb!)
|   - host_log |  tracing-spans propagated as host fn
|   - host_http|  outbound HTTP via reqwest, allowlist per tenant
+--------------+
     |
     v
   billing/metering ledger (Postgres):
     (tenant, fn, instance_id, cpu_us, mem_peak, calls)
```

## Stack

| Layer              | Crate(s)                                                        |
| ------------------ | --------------------------------------------------------------- |
| Runtime            | `wasmtime` 27 (async, cranelift, parallel-compilation, components) |
| HTTP server        | `axum` 0.8 + `tokio` 1.47 + `tower-http`                        |
| Database           | `sqlx` 0.8 + Postgres 16                                        |
| WASI               | `wasmtime-wasi` 27                                              |
| KV trait + in-mem  | `async-trait` + `tokio::sync::RwLock`                           |
| Module cache       | `sha2` + `hex` (content-hash → `.cwasm`)                        |
| Outbound HTTP      | `reqwest` (Phase 2 — stub today)                                |
| Observability      | `tracing` + `metrics-exporter-prometheus`                       |
| Errors             | `anyhow` + `thiserror` 2                                        |

The full pinned stack lives in
[`../backend-cloud-roadmap.md`](../backend-cloud-roadmap.md) § 3.

## How to upload + invoke

Start the stack:

```bash
docker compose up -d postgres
# in one terminal
cargo run -p warpline-control
# in another terminal
cargo run -p warpline-host
```

Build the demo guest:

```bash
rustup target add wasm32-wasip1
cd examples/hello-wasm && cargo build --target wasm32-wasip1 --release
```

Upload it:

```bash
curl -X POST \
  -F "wasm=@examples/hello-wasm/target/wasm32-wasip1/release/hello_wasm.wasm" \
  http://localhost:8081/tenants/demo/functions/hello
```

Invoke it:

```bash
curl -X POST \
  --data-binary '{"hello":"world"}' \
  http://localhost:8080/tenants/demo/functions/hello/invoke
```

Expected response body in Phase 2 (once the wit-bindgen-generated `handle`
glue lands): `hello from wasm`.

## Bench targets

| Metric                                        | Target                | Notes                                          |
| --------------------------------------------- | --------------------- | ---------------------------------------------- |
| Cold-start latency from `.wasm`               | full cranelift run    | baseline, not the goal — we cache against it   |
| Cold-start latency from `.cwasm`              | < 1 ms                | deserialise-only path; the load-bearing metric |
| Warm invocation p99 (10 ms budget fn)         | < 5 ms                | wasmtime store reuse, no compile               |
| Instances/sec/core (trivial handler)          | > 1,000               | the multi-tenant density number                |
| CPU cap accuracy (infinite-loop trap)         | budget ± 5 ms         | epoch-interrupt resolution                     |
| Memory cap accuracy (runaway allocator)       | hard ceiling          | `ResourceLimiter::memory_growing` returns false |
| Cost per million invocations                  | vs Cloudflare Workers | the pitch deck slide                           |

Real numbers land at the end of Phase 2 — these are the targets driving
the design.

## Design tradeoffs

The defenses behind every load-bearing runtime choice. These are the
answers you give in the cloud-runtime interview.

### epoch_interruption over fuel metering

Wasmtime gives you two CPU-budget enforcement primitives:

- **Fuel metering**: every WASM instruction consumes fuel; trap when the
  store runs out. Cost: one decrement per instruction, in the hot path.
- **Epoch interruption**: every function entry checks the engine's epoch
  counter against the store's deadline; trap on mismatch. Cost: one
  load + compare per fn call.

For a function host running short-lived handlers, the per-instruction
fuel cost dominates total CPU time. Epoch interruption pays one check at
the call boundary and lets the optimiser go to town in between. Wasmtime
docs explicitly recommend epochs for time-based budgets. Trap-latency
resolution is bounded by how often the guest hits a function call — fine
for tens-of-milliseconds budgets, the wrong tool for sub-µs deadlines.

### `.cwasm` cache by content hash

The control plane SHA-256s every uploaded `.wasm` and names the
compiled `.cwasm` after the digest. Three benefits:

1. **Idempotent uploads.** Re-uploading the same module is a no-op —
   no recompile, no garbage churn.
2. **Tamper-evident storage.** If the on-disk byte-for-byte hash
   doesn't match the file name, the cache entry is bogus and gets
   discarded. (Phase 2 — the Phase-1 scaffold trusts the cache dir.)
3. **Multi-tenant deduplication.** Two tenants uploading the same
   module share the cache entry. Disk pressure on the host scales with
   the number of *unique* modules, not invocations.

The cost is the `unsafe` `Module::deserialize` call — wasmtime can't
prove the `.cwasm` was produced by the same engine version + config.
Mitigation: the cache dir is host-trusted local storage, written only
by the control plane, and we rebuild it on every host upgrade.

### Capability allowlist for `http-out` (not open by default)

Guest modules can request `warpline:host/http-out::fetch` to make
outbound HTTP. Phase 1 stubs the call — Phase 2 wires reqwest behind a
per-tenant allowlist that:

- Resolves `req.url` to a host, compares against `ctx.allowed_hosts`,
  rejects with `Err("host {h} not allowed")` on mismatch.
- Enforces a per-request timeout (default 5 s) so a slow upstream
  can't pin a tenant's CPU budget at zero progress.
- Bounds response body size before allocating into guest memory.

The default is **empty allowlist** — a tenant has to explicitly opt in
to every outbound host. The alternative (full open egress) means one
hostile module can use your fleet to DDoS a third party; the
multi-tenant runtime takes the blame either way. This is the
Cloudflare Workers `fetch` policy.

## Repository layout

```
warpline/
  Cargo.toml                          # workspace root (excludes examples/hello-wasm)
  wit/warpline.wit                    # capability surface (Component Model)
  crates/
    core/                             # Engine + host imports + cache + meter + KV
      src/{lib,runtime,kv,cache,meter,types}.rs
    host/                             # axum invoke API, port :8080
      src/main.rs
    control/                          # axum upload API, port :8081
      src/main.rs
  examples/hello-wasm/                # standalone guest crate, target=wasm32-wasip1
  migrations/0001_init.sql            # tenants, functions, meter tables
  Dockerfile                          # cargo-chef multi-stage, distroless cc
  docker-compose.yml                  # postgres + minio + host + control
  fly.toml                            # single-region sin
  .github/workflows/ci.yml            # fmt + clippy + nextest + deny + wasm build
  deny.toml, rust-toolchain.toml, .gitignore
  docs/
    specs/2026-05-28-warpline-design.md
    plans/2026-05-28-warpline-phase-1-scaffold.md
  PROGRESS.md                         # per-sprint tracker, P6 bench targets
```

## Roadmap

Phase 1 (scaffold + `cargo check --workspace` green) is the current sprint
— see
[`docs/plans/2026-05-28-warpline-phase-1-scaffold.md`](./docs/plans/2026-05-28-warpline-phase-1-scaffold.md).
Subsequent phases (wit-bindgen `handle` export, real kv/log/http-out host
fns, metering wiring, S3-backed cache, Cloudflare-Workers comparison
bench) are tracked in [`PROGRESS.md`](./PROGRESS.md).

## License <a id="license"></a>

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](./LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.
