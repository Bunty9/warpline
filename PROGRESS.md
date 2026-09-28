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
- [x] Bench harness (`crates/core/benches/{runtime,report}.rs`, criterion +
      a plain-`main` measurement report):
    - cold-start `.wasm` vs `.cwasm` (criterion `cold_compile` /
      `cold_deserialize`).
    - warm invocation p99 vs `cpu_budget_ms` (`report.rs`, 10k sequential
      invokes).
    - CPU cap accuracy on an infinite-loop module (`report.rs`, 10/50/100 ms
      budgets).
    - memory cap accuracy on a runaway allocator (`report.rs`, 16 MiB cap).
    - instances/sec/core sweep (`report.rs`, single-core sequential
      throughput).
    - cost-per-million vs Cloudflare Workers (back-of-envelope, see below).

## Done

(none yet — scaffold landing is the first commit)

## Blocked

- (none)

## Bench numbers (targets per `projects-l3-l4.md` § P6; updated weekly)

Machine: 8 logical cores, Intel(R) Core(TM) i5-9300H CPU @ 2.40GHz (laptop,
not the target Hetzner box — see caveat below). `cargo bench -p
warpline-core` (release profile: `lto = "fat"`, `codegen-units = 1`),
`wasmtime` 49, guest is `crates/core/tests/fixtures/test_guest.wasm`
(trivial echo/loop/alloc fixture, not a real handler).

| Metric                                          | Target              | Current                                         | As-of      |
|--------------------------------------------------|---------------------|--------------------------------------------------|------------|
| Cold-start latency from `.cwasm`                | < 1 ms              | **0.26 ms** mean (criterion `cold_deserialize`, [228 µs, 259 µs, 314 µs] CI) — PASS | 2026-09-28 |
| Warm invocation p99 (10 ms budget fn)           | < 5 ms              | **0.152 ms** p99 / 0.060 ms p50 (10k sequential invokes; criterion mean 0.086 ms) — PASS | 2026-09-28 |
| Instances/sec/core (trivial handler)            | > 1,000             | **13,564 inv/s** (single core, current-thread tokio, 5k sequential invokes) — PASS | 2026-09-28 |
| CPU cap accuracy                                | budget ± 5 ms       | 10 ms budget: **+0.45 ms** (PASS). 50 ms budget: **+3.18 ms** (PASS). 100 ms budget: **+12.11 ms** (MISS) | 2026-09-28 |
| Memory cap accuracy                             | hard ceiling holds  | **PASS** — 16 MiB cap trapped at peak 15.79 MiB (15,794,176 bytes ≤ 16,777,216 bytes) | 2026-09-28 |
| Cost per million invocations vs CF Workers      | within 2×           | **~1500× cheaper on paper** (see estimate below) — number is not load-bearing, see caveat | 2026-09-28 |

### Notes on the misses / caveats

- **CPU cap accuracy degrades with budget size.** The epoch ticker fires
  every `EPOCH_TICK_MS = 1` ms (`crates/core/src/runtime.rs`), and each tick
  the guest hits pays a cooperative-yield round trip through the tokio
  executor (`UpdateDeadline::Yield(1)`) before the deadline is re-armed. At
  a 10-tick budget that overhead is ~0.45 ms; at 100 ticks it's compounded
  to ~12 ms, blowing the ±5 ms target. The fix would be either a coarser
  per-invocation deadline check (set the deadline to the *whole* budget in
  ticks up front instead of one tick at a time, only re-entering the
  callback if the budget is actually exhausted) or accepting that the ±5 ms
  target only holds for short budgets on a loaded core. Not fixed here —
  benches are read-only with respect to `runtime.rs` per this task's scope.
- **Cost-per-million is a compute-bound ceiling, not a real quote.** It's
  derived from the single-core echo throughput above, which does zero real
  work and isn't measuring the axum HTTP layer, connection handling, or
  concurrent-tenant contention — see the estimate below for the assumptions
  and why the headline ratio shouldn't be taken as a serious pricing claim.
- Measured on a laptop CPU, not the target Hetzner box; numbers are
  directional, not a committed SLA.

### Cost per million invocations vs CF Workers (back-of-envelope estimate)

**Assumptions (not fetched — stated so the estimate can be redone with real
numbers):**
- Host: Hetzner CCX13 (2 dedicated vCPU, 8 GB RAM), assumed **€13/mo**
  (≈ $14/mo) list price.
- Naive linear scaling from the single-core measurement above to 2 cores:
  2 × 13,564 ≈ **27,128 inv/s** aggregate (ignores contention, memory
  bandwidth, and the real host's per-request HTTP/axum overhead — ceiling,
  not a floor).
- 30-day month, 100% utilization (not realistic, but this is a ceiling
  estimate, not a capacity plan).
- CF Workers paid-plan reference rate: **$0.30 per million requests**
  above the included quota (from memory, not verified against current CF
  pricing pages — treat as approximate).

**Math:**
- Invocations/month ≈ 27,128 inv/s × 2,592,000 s/mo ≈ **70.3 billion**.
- warpline cost/million ≈ $14 / 70,300 million ≈ **$0.0002/million**.
- CF Workers cost/million ≈ **$0.30/million** (reference).
- Ratio ≈ **1,500× cheaper** on this ceiling number.

This is not a credible apples-to-apples comparison — it's what a
compute-only, zero-work echo handler costs on paper, with no accounting
for the actual host's request-handling overhead, real handler CPU time, or
CF Workers' free tier / cold-start amortization. Read it as "the runtime
overhead itself is not the bottleneck relative to CF Workers' pricing";
the real per-tenant economics depend on what tenants' functions actually
do. A credible comparison needs an end-to-end bench through
`warpline-host`'s HTTP path with a realistic handler, which is out of
scope for this bench harness (Task 3 benches `warpline-core::invoke`
directly, not the axum server).

## Blog topics surfacing

- Phase-2 stretch: "I built a function runtime for my agency clients and
  it does X invocations/sec on a Hetzner box." (per `projects-l3-l4.md`
  § P6 stretch).
- Phase-3 candidate: Hyperlight integration for sub-millisecond
  ephemeral instances (Microsoft, Mar 2025) — write-up if it lands.
