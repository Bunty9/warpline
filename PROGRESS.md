# PROGRESS — warpline

> Per-sprint tracker. Template adapted from `project-plan.md` § 7,
> customised for P6 (warpline) bench targets and the Phase C sequencing
> in `backend-cloud-roadmap.md` § 2 (weeks 39–44).

## Sprint — Phase 1 scaffold (historical)

> **Historical.** This is the original scaffold checklist, kept as a record.
> It describes the Phase-1 layout (`wasm32-wasip1`, MinIO in compose, a
> `meter.rs` that inserted directly) which no longer exists: see Phase 2 and
> "0.2 — embeddable runtime" below for the current shape. Unchecked items
> here were superseded, not left undone.

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
- [x] `docker-compose.yml` — postgres + host + control (Phase 1 planned a
      MinIO service too; it was never added, see README "Roadmap")
- [x] `fly.toml` — single region `sin`
- [x] `.github/workflows/ci.yml` — fmt + clippy + nextest + deny + bench
      + `build-wasm-example` (Phase 1 installed `wasm32-wasip1`; guests now
      build for `wasm32-wasip2`)
- [x] `deny.toml`, `rust-toolchain.toml`, `.gitignore`
- [x] `README.md`, design spec, phase plan
- [x] `cargo check --workspace` passes locally (verified at end of scaffold)
- [ ] `cargo run -p warpline-host` smoke-tested against `/healthz`
- [ ] `cargo build --target wasm32-wasip1 --release` inside
      `examples/hello-wasm/` produces a `.wasm` (deferred — requires
      `rustup target add wasm32-wasip1`)

## Sprint — Phase 2: wit-bindgen + live host fns + metering (done)

- [x] Generate Component-Model bindings via `wit-bindgen` for both host
      (Rust, `wasmtime::component::bindgen!` against `wit/warpline.wit`)
      and guest (`hello-wasm`/`test-guest` via `wit_bindgen::generate!`,
      targeting `wasm32-wasip2` — no adapter step).
- [x] Real `warpline:host/kv::{get,put}` — `HostCtx` holds a
      `Arc<dyn KvStore>` and scopes every call by `(tenant, key)` as a
      composite key (not string concatenation — see `kv.rs` module docs),
      with per-call/per-invocation size and count caps.
- [x] Real `warpline:host/log::emit` propagating to `tracing`, tagged with
      `tenant`/`func`, truncated/dropped past per-invocation caps rather
      than trapping the guest.
- [x] Real `warpline:host/http-out::fetch` with per-tenant `allowed_hosts`
      enforcement, a 5 s timeout, a 1 MiB response-body cap, no redirects,
      no proxy, and SSRF-hardened private/loopback/link-local blocking
      (`is_blocked_ip` + `GuardedResolver`) — this went further than the
      original plan, which only called for allowlist + timeout + body cap.
- [x] Metering wired on the invoke hot path — **deviated from the plan**:
      instead of a direct `meter::record` call (or a per-invoke
      `tokio::spawn`), completed invocations are queued onto a bounded
      `mpsc` channel to a single writer task that batches inserts
      (then `meter::spawn_writer`; since 0.2 `pg::PgMeter::spawn`, capacity
      10,000, batches of up to 200) — bounded backpressure under a burst instead of
      one Postgres write and one spawned task per request.
- [x] `TenantLimiter` moved into `HostCtx` (no `Box::leak`); doubles as the
      peak-memory recorder metering reads.
- [x] Module registry: `registry` module in `warpline-core` (private since 0.2) — pointer files
      (`modules/tenants/{tenant}/{func}`) resolved to a content digest,
      backed by an in-memory `ComponentCache` LRU (256 entries / 512 MiB
      byte budget). **Deviated from the plan**: the source `.wasm` is also
      kept on disk (`modules/wasm/{digest}.wasm`), not just the `.cwasm` —
      an engine/config upgrade invalidates the `.cwasm`'s compatibility
      hash, and without the source, every existing pointer would 500 until
      a re-upload; `get_or_load` now recompiles from source on a cache
      miss instead.
- [x] Tenant auth on `warpline-control`: bearer-token API keys
      (SHA-256-hashed, `api_keys` table) validated against `tenants`.
      **Partially deviated**: uploads are bounded by a process-wide compile
      concurrency semaphore and a 100-functions-per-tenant quota, not a
      time-window rate limit — see Phase 3.
- [ ] S3 / MinIO backend for the `.cwasm` cache behind a trait — deferred,
      see Phase 3 / README "Roadmap".
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
- [x] Fixed a CPU-cap drift bug found while gathering the bench numbers
      above: the epoch ticker now sleeps to an absolute schedule instead of
      a relative `sleep(tick)` per iteration (see "Notes on the misses"
      below).

## 0.2 — embeddable runtime

Source of requirements: the 2026-09-30 audit; plan in
`docs/plans/2026-09-30-warpline-0.2-embedding.md`. Tasks 1-3 of 5 done on
`release/0.2`:

- [x] **Task 1** — `warpline_core::pg` (feature `postgres`): schema-isolated
      `pg::migrate`, `Authenticator`, tenant admin (`create_tenant`,
      `patch_tenant`, `set_limits`, `tenant_limits`, `usage_summary`),
      batching `PgMeter` with a drop counter and bounded shutdown.
- [x] **Task 2** — `Runtime` facade with `RuntimeConfig`/`RuntimeBuilder`:
      `stage`/`activate`/`publish`/`invoke`/`gc`, memory admission budget
      (`503`), per-tenant in-flight cap (`429`), output cap (`502`),
      epoch-tick metering, compile permits, typed errors with
      `InvokeError::http_status`.
- [x] **Task 3** — `warpline-host` and `warpline-control` are thin HTTP
      layers over `Runtime` + `pg`: environment handling only in `main.rs`
      (empty means unset), control shuts down gracefully, upload activates
      the pointer inside the quota transaction and reconciles it with the
      `functions` row if the commit fails, admin bodies have PATCH semantics, host embeds
      control behind the `embed-control` feature and awaits it with a
      timeout on shutdown, README embedding section and doc fixes.
- [ ] **Task 4** — `examples/storefront` reference app.
- [ ] **Task 5** — version bump, changelog, trusted-publishing release.

## Next sprint — Phase 3

Real remaining work, from the `ponytail:` notes left in the code and the
items deferred out of Phase 2 (also tracked in README "Roadmap / deferred"):

- [ ] Per-tenant upload rate limiting (time-window, not just the compile
      concurrency semaphore + 100-function quota that landed in Phase 2).
- [ ] Single-flight dedupe on a cold `.cwasm` digest — a burst of
      concurrent first-invokes for one freshly-uploaded function each pay
      for their own recompile today (`crates/core/src/registry.rs`,
      `ComponentCache::get_or_load`).
- [ ] S3/MinIO-backed `.cwasm` registry, in place of the registry's
      local-filesystem functions, for a multi-host deploy (the shared local volume only
      covers one box).
- [ ] Instance pooling / warm-store reuse (wasmtime's pooling allocator) —
      every invoke currently builds a fresh `Store`.
- [ ] Hyperlight spike — sub-millisecond ephemeral instances (Microsoft,
      Mar 2025) as a possible alternative isolation boundary.
- [ ] End-to-end bench through `warpline-host`'s actual HTTP path with a
      realistic handler (the current bench harness calls
      `warpline-core::invoke` directly — see the cost-per-million caveat
      below).

## Done

- Phase 1 scaffold (see above) — workspace skeleton, stub host fns,
  Postgres schema v1, Docker/Fly deployment shape.
- Phase 2 — live wasmtime 49 Component Model runtime, real host imports,
  module registry + content-addressed cache, API-key auth, per-tenant
  config and quotas, batched metering, criterion + report bench harness
  with measured numbers (see below), the epoch-ticker drift fix.

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
| Warm invocation p99 (10 ms budget fn)           | < 5 ms              | **0.121 ms** p99 / 0.051 ms p50 (10k sequential invokes; criterion mean 0.086 ms) — PASS | 2026-09-28 |
| Instances/sec/core (trivial handler)            | > 1,000             | **16,349 inv/s** (single core, current-thread tokio, 5k sequential invokes) — PASS | 2026-09-28 |
| CPU cap accuracy                                | budget ± 5 ms       | 10 ms: **−0.66 ms**, 50 ms: **+0.05 ms**, 100 ms: **−0.09 ms** — PASS | 2026-09-28 |
| Memory cap accuracy                             | hard ceiling holds  | **PASS** — 16 MiB cap trapped at peak 15.79 MiB (15,794,176 bytes ≤ 16,777,216 bytes) | 2026-09-28 |
| Cost per million invocations vs CF Workers      | within 2×           | **~1800× cheaper on paper** (see estimate below) — number is not load-bearing, see caveat | 2026-09-28 |

### Notes on the misses / caveats

- **CPU cap accuracy (fixed).** The first run missed at 100 ms (+12.11 ms).
  Cause: the epoch ticker slept a relative 1 ms per iteration; every sleep
  overshoots slightly and budgets are counted in ticks, so the drift
  compounded (~0.12 ms/tick). The ticker now sleeps to an absolute
  schedule (`crates/core/src/sandbox.rs`, `EpochTicker::spawn`), which
  brought all three budgets within ±0.7 ms.
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
  2 × 16,349 ≈ **32,698 inv/s** aggregate (ignores contention, memory
  bandwidth, and the real host's per-request HTTP/axum overhead — ceiling,
  not a floor).
- 30-day month, 100% utilization (not realistic, but this is a ceiling
  estimate, not a capacity plan).
- CF Workers paid-plan reference rate: **$0.30 per million requests**
  above the included quota (from memory, not verified against current CF
  pricing pages — treat as approximate).

**Math:**
- Invocations/month ≈ 32,698 inv/s × 2,592,000 s/mo ≈ **84.8 billion**.
- warpline cost/million ≈ $14 / 84,800 million ≈ **$0.00017/million**.
- CF Workers cost/million ≈ **$0.30/million** (reference).
- Ratio ≈ **1,800× cheaper** on this ceiling number.

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
