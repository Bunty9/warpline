# PROGRESS — warpline

> Per-sprint tracker. Template adapted from `project-plan.md` § 7,
> customised for P6 (warpline) bench targets and the Phase C sequencing
> in `backend-cloud-roadmap.md` § 2 (weeks 39–44).

## Sprint — Phase 1 scaffold

- [x] `Cargo.toml` — workspace root, members `[core, host, control]`,
      `examples/hello-wasm` excluded (wasm32 target conflict)
- [x] `wit/warpline.wit` — capability surface
- [x] `crates/core/src/{lib,runtime,kv,cache,meter,types}.rs` — engine
      builder + KV trait + cache + metering writer + host-context types
- [x] `crates/host/src/main.rs` — axum invoke server, port `:8080`
- [x] `crates/control/src/main.rs` — axum upload server, port `:8081`
- [x] `examples/hello-wasm/{Cargo.toml,src/lib.rs,README.md}` —
      standalone guest crate (not in workspace)
- [x] `migrations/0001_init.sql` — tenants + functions + meter tables
- [x] `Dockerfile` — cargo-chef multi-stage, distroless `cc-debian12`
- [x] `docker-compose.yml` — postgres + minio + host + control
- [x] `fly.toml` — single region `sin`
- [x] `.github/workflows/ci.yml` — fmt + clippy + nextest + deny + bench
      + `build-wasm-example` (installs `wasm32-wasip1`)
- [x] `deny.toml`, `rust-toolchain.toml`, `.gitignore`
- [x] `README.md`, design spec, phase plan
- [x] `cargo check --workspace` passes locally (verified at end of scaffold)
- [ ] `cargo run -p warpline-host` smoke-tested against `/healthz`
- [ ] `cargo build --target wasm32-wasip1 --release` inside
      `examples/hello-wasm/` produces a `.wasm` (deferred — requires
      `rustup target add wasm32-wasip1`)

## Next sprint — Phase 2: wit-bindgen + live host fns + metering

- [ ] Generate Component-Model bindings via `wit-bindgen` for both host
      (Rust) and guest (`hello-wasm` switches off raw `extern "C"`).
- [ ] Real `warpline:host/kv::{get,put}` via `func_wrap_async`, scoped
      by `t/{tenant_id}/{key}`.
- [ ] Real `warpline:host/log::emit` propagating to `tracing` with a
      per-tenant span.
- [ ] Real `warpline:host/http-out::fetch` with per-tenant
      `allowed_hosts` enforcement, per-request timeout, response-body
      size cap.
- [ ] Wire `meter::record` on the invoke hot path; capture CPU µs from
      epoch deltas and memory peak from `ResourceLimiter` callbacks.
- [ ] Move the per-call `TenantLimiter` allocation off
      `Box::leak` — store inside `HostCtx`.
- [ ] Module registry: pull `.cwasm` from disk on first invoke of an
      unknown (tenant, fn) tuple, LRU-evict cold entries.
- [ ] Tenant auth on `warpline-control`: API-key header validated
      against `tenants` table; rate-limit uploads.
- [ ] S3 / MinIO backend for the `.cwasm` cache behind a trait.
- [ ] Bench harness:
    - cold-start `.wasm` vs `.cwasm` (criterion).
    - warm invocation p99 vs `cpu_budget_ms`.
    - CPU cap accuracy on an infinite-loop module.
    - memory cap accuracy on a runaway allocator.
    - instances/sec/core sweep.
    - cost-per-million vs Cloudflare Workers (back-of-envelope).

## Done

(none yet — scaffold landing is the first commit)

## Blocked

- (none)

## Bench numbers (targets per `projects-l3-l4.md` § P6; updated weekly)

| Metric                                          | Target              | Current | As-of |
|-------------------------------------------------|---------------------|---------|-------|
| Cold-start latency from `.cwasm`                | < 1 ms              |         |       |
| Warm invocation p99 (10 ms budget fn)           | < 5 ms              |         |       |
| Instances/sec/core (trivial handler)            | > 1,000             |         |       |
| CPU cap accuracy                                | budget ± 5 ms       |         |       |
| Memory cap accuracy                             | hard ceiling holds  |         |       |
| Cost per million invocations vs CF Workers      | within 2×           |         |       |

## Blog topics surfacing

- Phase-2 stretch: "I built a function runtime for my agency clients and
  it does X invocations/sec on a Hetzner box." (per `projects-l3-l4.md`
  § P6 stretch).
- Phase-3 candidate: Hyperlight integration for sub-millisecond
  ephemeral instances (Microsoft, Mar 2025) — write-up if it lands.
