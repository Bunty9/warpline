# warpline

> Multi-tenant WASM function runtime in Rust. Wasmtime 49 Component Model
> host + control plane, epoch-based CPU caps, content-addressed `.cwasm`
> warm cache, deny-by-default outbound HTTP. Built as the cloud-runtime
> portfolio project for the Rust Level-4 roadmap — the Cloudflare Workers /
> Fastly Compute / Fermyon Spin pattern, scoped to one box and ten clients.

[![ci](https://github.com/Bunty9/warpline/actions/workflows/ci.yml/badge.svg)](https://github.com/Bunty9/warpline/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## The problem

Customer-customization is a recurring need in any SaaS or agency product —
clients want hooks into automation flows. Today this usually means "open a
PR and redeploy." A WASM function host lets clients upload sandboxed code
in any language that compiles to WASI, with real resource caps and a
metered capability surface. **warpline** is that host, sized to embed into
a real customer pilot on one box, with the engineering defenses (cwasm
cache, epoch CPU caps, http-out allowlist) the interview panels want to
talk about.

## Architecture

Two axum services share one on-disk module registry and one Postgres
database; the host's Prometheus scrape endpoint runs on a second,
loopback-only listener so it never shares a socket with whatever's publicly
reachable. On Fly (one machine, one volume) the host runs both APIs in one
process via `WARPLINE_EMBED_CONTROL=1`.

```
                         +----------------------+
                         |       Postgres        |
                         | tenants, api_keys,     |
                         | functions, meter       |
                         +----^--------------^----+
                              |              |
                     auth + tenant config    upsert function row,
                     (both services)         per-tenant quota, metering
                              |              |
   client --upload-->  +-----+------+   +----+-------------+  --invoke--> client
                        | warpline-  |   |  warpline-host    |
                        | control    |   |  (axum :8080)      |
                        | (axum      |   |                     |
                        |  :8081)    |   |  /metrics on its own|
                        +-----+------+   |  listener, :9090    |
                              |          |  loopback-only by   |
                              |          |  default             |
                              |          +----------+-----------+
                              |                     |
                              v                     v
                        +-----+---------------------+-----+
                        |   shared modules volume          |
                        |   wasm/{digest}.wasm              |
                        |   cwasm/{digest}-{compat}.cwasm   |
                        |   tenants/{tenant}/{func} pointer |
                        +-----------------------------------+
                                        |
                              wasmtime Engine, shared Linker,
                              one epoch-ticker thread per engine
```

`warpline-control` compiles an uploaded `.wasm` to a `Component`,
type-checks its imports/exports against the `warpline:host/handler` world,
and only then persists the source bytes + compiled `.cwasm` and publishes a
`(tenant, func) -> digest` pointer file. `warpline-host` reads the pointer,
resolves the digest through an in-memory LRU backed by the shared `cwasm/`
directory, and calls the component's `handle` export inside a fresh
`wasmtime::Store` with per-tenant caps installed. There is no S3/MinIO in
this deployment shape — the module registry is a single shared volume,
which is enough for a one-box deploy (see "Roadmap / deferred").

## Stack

| Layer              | Crate(s)                                                            |
| ------------------ | -------------------------------------------------------------------- |
| Runtime            | `wasmtime` 49 + `wasmtime-wasi` 49 p2 (async, cranelift, parallel-compilation, component-model) |
| HTTP server        | `axum` 0.8 + `tokio` 1.47 + `tower-http`                              |
| Database           | `sqlx` 0.8 + Postgres 16                                              |
| KV trait + in-mem  | `async-trait` + `tokio::sync::RwLock` (`MemKv`)                       |
| Module cache       | `sha2` + `hex` (content-hash → `.cwasm`) + `lru` (in-memory `Component` cache) |
| Outbound HTTP      | `reqwest` (rustls-tls), allowlist + no redirects + no proxy + private-address blocking |
| Observability      | `tracing` (+ `tracing-subscriber` json) + `metrics` + `metrics-exporter-prometheus` |
| Errors             | `anyhow` + `thiserror` 2                                              |

## Security model / isolation

Every cap below is enforced per invocation or per tenant. Values marked
"configurable" come from the `tenants` table (`admin_create_tenant`,
validated against the ranges in `warpline_core::types`) and default to
[`TenantConfig::dev_default`](crates/core/src/auth.rs) under
`WARPLINE_INSECURE_DEV=1`.

| Cap                                             | Value                                          | Enforced by |
| ------------------------------------------------ | ----------------------------------------------- | ----------- |
| CPU budget (configurable)                       | 1 – 10,000 ms, default 100 ms                   | epoch-deadline callback, `runtime::invoke` |
| Memory cap — linear memory (configurable)       | 1 MiB – 512 MiB, default 64 MiB                 | `TenantLimiter::memory_growing` |
| Core instances per `Store`                      | 32                                               | `TenantLimiter::instances` |
| Core tables per `Store`                         | 8                                                 | `TenantLimiter::tables` |
| Core linear memories per `Store`                | 4                                                 | `TenantLimiter::memories` |
| Table elements per `Store`                      | 10,000                                            | `TenantLimiter::table_growing` |
| KV key size                                     | 512 B                                             | `warpline::host::kv::Host::get`/`put` |
| KV value size (per `put`)                       | 1 MiB                                             | `kv::Host::put` |
| KV `put` calls per invocation                   | 1,000                                             | `HostCtx::kv_put_count` |
| KV `put` bytes per invocation                   | 8 MiB                                             | `HostCtx::kv_put_bytes` |
| KV storage per tenant (`MemKv`)                 | 16 MiB                                            | `kv::MemKv::DEFAULT_TENANT_CAP_BYTES` |
| Log line length                                 | 4 KiB (truncated, not trapped)                    | `log::Host::emit` |
| Log lines per invocation                        | 100 (dropped after, one "suppressed" notice)      | `log::Host::emit` |
| Log bytes per invocation                        | 64 KiB                                            | `log::Host::emit` |
| `http-out` response body                        | 1 MiB (fetch errs past this)                      | `runtime::http_fetch` |
| `http-out` request timeout                      | 5 s                                               | shared `reqwest::Client` |
| `http-out` allowed hosts per tenant             | 64                                                | `validate_allowed_hosts` |
| `http-out` redirects                            | none followed                                     | `reqwest::redirect::Policy::none()` |
| `http-out` proxy                                | disabled (`no_proxy()`), so a proxy can't dodge the allowlist | `build_http_client` |
| `http-out` private/loopback/link-local targets  | blocked unless `WARPLINE_ALLOW_PRIVATE_EGRESS=1`  | `is_blocked_ip`, `GuardedResolver` |
| Upload body (`.wasm`)                           | 16 MiB                                            | `UPLOAD_BODY_LIMIT_BYTES` |
| Invoke request body                             | 1 MiB                                             | `INVOKE_BODY_LIMIT_BYTES` |
| Functions per tenant                            | 100                                               | `MAX_FUNCTIONS_PER_TENANT`, enforced in the publish transaction |
| Compile concurrency (process-wide)              | `max(available_parallelism / 2, 1)` permits       | `AppState::new`'s `compile_semaphore` |
| In-memory component cache                       | 256 entries, 512 MiB total serialized bytes       | `COMPONENT_CACHE_CAP`, `COMPONENT_CACHE_BYTE_BUDGET` |
| Epoch tick                                      | 1 ms                                              | `EpochTicker`, `EPOCH_TICK_MS` |
| Wall-clock backstop per invoke                  | `cpu_budget_ms` + http timeout (5 s) + 1 s slack  | `tokio::time::timeout` around the whole call |

Guests get a deny-by-default WASI p2 context: no preopens, no env, no args,
no sockets — only what a `wasm32-wasip2` Rust std needs to link (clocks,
random, a stdio sink). Outbound network access exists only through the
`warpline:host/http-out` import, which is allowlisted per tenant.

### CPU budget: one epoch ticker, cooperative yield

CPU budget is enforced by wasmtime's epoch interruption, driven by a single
background thread per `Engine` ([`EpochTicker`](crates/core/src/runtime.rs))
that increments the engine-wide epoch counter every `EPOCH_TICK_MS` (1 ms),
sleeping to an **absolute** schedule rather than a relative `sleep(tick)` in
a loop — a relative sleep overshoots a little every iteration, and since
budgets are counted in ticks that drift compounds (it measured ~+12% at a
100 ms budget before this fix). Each `Store` sets its own deadline in
ticks; when the deadline callback fires, it returns
`UpdateDeadline::Yield(1)` and extends the deadline by one more tick, so a
long-running guest **yields back to the tokio executor** on every tick
instead of blocking the worker thread — other tenants' invocations and the
ticker itself keep making progress. Once the callback has fired
`budget_ticks` times, it returns a distinguishable error instead of
extending the deadline again, which the host classifies as a CPU-budget
trap (408). This replaced an earlier per-call `tokio::spawn` ticker that
bumped the epoch for *every* concurrent store, not just the one whose
budget had actually elapsed — one engine-wide ticker with per-store tick
counts fixes that. A `tokio::time::timeout` wraps the whole call as a
wall-clock backstop, so a guest stuck inside a slow host call can't hang
the request indefinitely even if epoch ticks can't reach it.

### `.cwasm` cache: content digest + engine compatibility hash

The control plane SHA-256s every uploaded `.wasm` and names the compiled
`.cwasm` `{digest}-{compat_hash}.cwasm`, where `compat_hash` is derived
from `Engine::precompile_compatibility_hash()`. Folding the compat hash
into the file name means a wasmtime upgrade or config change can never load
a `.cwasm` compiled by an incompatible engine — it just misses the cache
and recompiles, rather than deserializing something it can't safely trust.

The **source `.wasm` is kept on disk** (`modules/wasm/{digest}.wasm`)
specifically so that miss is cheap to recover from: if the `.cwasm` goes
missing (deleted, an engine/config upgrade invalidated the compat hash, or
the file is simply corrupt) `ComponentCache::get_or_load` recompiles from
the persisted source and republishes a fresh `.cwasm`, instead of every
pointer into that stale cache 500ing forever. Two tenants uploading the
same module share both the `wasm/` and `cwasm/` entries, so disk pressure
scales with unique modules, not invocations or tenants.

`Component::deserialize` is `unsafe` — wasmtime can't fully verify a
`.cwasm` blob was produced by the same engine version + config it's being
loaded into. The compat hash narrows that gap; `load_cwasm` additionally
rejects any digest that isn't exactly 64 lowercase hex characters before it
ever builds a path, so a digest can't be used to escape the cache
directory. The cache directory is otherwise treated as host-trusted local
storage, written by the control plane on upload and by the host when it
recompiles a missing or engine-incompatible `.cwasm` from the stored source
`.wasm` — so both need write access to the modules volume.

### `http-out`: allowlist, not open egress

Guest modules request outbound HTTP through `warpline:host/http-out::fetch`.
The default per-tenant `allowed_hosts` is **empty** — a tenant must
explicitly opt a host in via the admin API. On top of the allowlist:

- **No redirects** (`reqwest::redirect::Policy::none()`) — a redirect could
  walk a request from an allowlisted host to a non-allowlisted one, and the
  allowlist check only ever sees the first hop.
- **No proxy** (`.no_proxy()`) — a configured proxy resolves the target
  itself, which would bypass the DNS-level private-address guard entirely.
- **Private/loopback/link-local addresses blocked** by default
  (`is_blocked_ip`: loopback, RFC 1918, link-local, CGNAT, unique-local,
  multicast, IPv4-mapped/embedded forms, and a handful of other reserved
  ranges), both via a custom DNS resolver (`GuardedResolver`, which filters
  blocked answers before reqwest ever connects) and via a direct check on
  URL IP literals (reqwest never consults the configured resolver when the
  host is already an IP literal). `WARPLINE_ALLOW_PRIVATE_EGRESS=1` turns
  this off host-wide, for local dev against a sidecar.
- **5 s request timeout** and a **1 MiB response-body cap**, read
  incrementally so an oversized response never gets fully buffered into
  guest-reachable memory.

This is the Cloudflare Workers `fetch` policy: full open egress means one
hostile module can use the fleet to attack a third party, and the
multi-tenant runtime takes the blame either way.

## Quickstart

```bash
docker compose up -d
```

This starts Postgres, `warpline-host` (`:8080`, invoke) and
`warpline-control` (`:8081`, upload + admin), migrations included (both
binaries run `sqlx::migrate!` on boot against `DATABASE_URL`).
`WARPLINE_ADMIN_TOKEN` is set to the placeholder `change-me` in
`docker-compose.yml` — replace it before any non-local deployment.

Create a tenant and get an API key:

```bash
curl -X POST -H "Authorization: Bearer change-me" \
  http://localhost:8081/admin/tenants/demo
# => {"tenant":"demo","api_key":"wl_<64 hex chars>"}
```

Every call to `POST /admin/tenants/{tenant}` is idempotent on the tenant
row but issues a **fresh** API key each time; older keys for the same
tenant keep working (each key hash is its own row in `api_keys`).

Build the demo guest (installs `wasm32-wasip2` if missing, builds
`examples/hello-wasm` and `examples/test-guest`, refreshes the committed
test fixture):

```bash
./scripts/build-guests.sh
```

Upload it (multipart field name is `wasm`; `module` and `file` are also
accepted):

```bash
curl -X POST \
  -H "Authorization: Bearer wl_<key from above>" \
  -F "wasm=@examples/hello-wasm/target/wasm32-wasip2/release/hello_wasm.wasm" \
  http://localhost:8081/tenants/demo/functions/hello
# => 201 {"tenant":"demo","func":"hello","digest":"<sha256 of the .wasm>"}
```

Invoke it:

```bash
curl -X POST \
  -H "Authorization: Bearer wl_<key from above>" \
  --data-binary '' \
  http://localhost:8080/tenants/demo/functions/hello/invoke
# => 200 "hello from wasm"
```

The request body becomes `handle`'s `input: list<u8>` argument verbatim;
`hello-wasm` ignores it.

## Configuration

Every environment variable either binary reads, with its default:

| Variable                        | Read by         | Default                                                | Purpose |
| -------------------------------- | ---------------- | ------------------------------------------------------- | ------- |
| `DATABASE_URL`                  | host, control    | unset (refuses to start unless `WARPLINE_INSECURE_DEV=1`) | Postgres connection string. Enables auth, tenant config, and metering; runs migrations on boot. |
| `WARPLINE_INSECURE_DEV`         | host, control    | unset                                                    | Set to `1` to run without Postgres: no auth enforced, every tenant gets `TenantConfig::dev_default` (empty allowlist, 100 ms CPU, 64 MiB memory), invocations are logged at debug level instead of metered. Not for production. |
| `WARPLINE_ADMIN_TOKEN`          | control (host if embedded) | unset (`/admin/tenants/{tenant}` 404s)                   | Bearer token guarding the admin route. |
| `WARPLINE_MODULES_DIR`          | host, control    | `./modules`                                              | Root of the shared module registry (`wasm/`, `cwasm/`, `tenants/`). |
| `WARPLINE_HOST_BIND`            | host             | `127.0.0.1:8080` under `WARPLINE_INSECURE_DEV`, else `0.0.0.0:8080` | `warpline-host`'s invoke-API listen address. |
| `WARPLINE_CONTROL_BIND`         | control (host if embedded) | `127.0.0.1:8081` under `WARPLINE_INSECURE_DEV`, else `0.0.0.0:8081` | `warpline-control`'s listen address. |
| `WARPLINE_METRICS_BIND`         | host             | `127.0.0.1:9090`                                         | Separate `/metrics` listener — never on the main router (would leak per-tenant series to whatever's publicly reachable). |
| `WARPLINE_AUTH_CACHE_TTL_SECS`  | host, control    | `30` (`0` disables)                                      | TTL of the in-process API-key lookup cache (positive and negative results). Key revocation and tenant config changes take up to this long to apply. |
| `WARPLINE_EMBED_CONTROL`        | host             | unset                                                    | Set to `1` to also serve the control-plane API from the host process (same engine, same volume), and run the startup blob GC there. Used by `fly.toml`, since a Fly volume attaches to one machine. |
| `WARPLINE_ALLOW_PRIVATE_EGRESS` | host             | unset (`false`)                                          | Set to `1` to let `http-out` reach loopback/private/link-local addresses (local dev only). |
| `WARPLINE_TEST_DATABASE_URL`    | tests only       | unset (DB-backed tests skip themselves)                  | Postgres URL for `crates/{host,control}/tests/db_mode.rs`. |

## HTTP API reference

### `warpline-control` (default `:8081`)

- **`POST /tenants/{tenant}/functions/{func}`** — multipart upload, field
  `wasm` (`module`/`file` also accepted). `Authorization: Bearer <tenant
  api key>` required unless `WARPLINE_INSECURE_DEV=1`. Body capped at 16
  MiB.
  - `201` `{"tenant","func","digest"}` — compiled, typechecked, and
    published.
  - `400` invalid tenant/func name, missing `wasm` field, malformed
    multipart.
  - `401` no/invalid bearer token. `403` token belongs to a different
    tenant, or the tenant's function quota (100) is exceeded.
  - `413` body over 16 MiB.
  - `422` not a valid component, or it fails typecheck against the
    `warpline:host/handler` world (missing export or unsatisfiable
    import).
  - `500` internal error (compile task panic, persist failure).
- **`POST /admin/tenants/{tenant}`** — `Authorization: Bearer
  <WARPLINE_ADMIN_TOKEN>`. JSON body, every field optional:
  `{"allowed_hosts": [string], "cpu_budget_ms": int, "mem_cap_bytes": int}`.
  - `201` `{"tenant","api_key"}` — creates the tenant if new (idempotent),
    applies any config fields given, always issues a fresh `wl_`-prefixed
    key.
  - `400` invalid tenant name, invalid JSON, or a config value outside its
    range (`cpu_budget_ms` 1–10,000; `mem_cap_bytes` 1 MiB–512 MiB;
    `allowed_hosts` ≤ 64 entries).
  - `401` missing/wrong admin token. `404` `WARPLINE_ADMIN_TOKEN` unset.
  - `503` no `DATABASE_URL` (the admin API requires Postgres).
- **`GET /healthz`** — `200 ok`.

### `warpline-host` (default `:8080`; `/metrics` on its own listener)

- **`POST /tenants/{tenant}/functions/{func}/invoke`** —
  `Authorization: Bearer <tenant api key>` required unless
  `WARPLINE_INSECURE_DEV=1`. Raw request body is passed verbatim as
  `handle`'s `input`. Body capped at 1 MiB.
  - `200` raw bytes — the guest's `handle` return value.
  - `400` invalid tenant/func name. `401`/`403` as above.
  - `404` no module published for `(tenant, func)`.
  - `408` CPU budget exceeded, or the wall-clock backstop fired.
  - `500` guest trap, failed to instantiate, or an internal error.
  - `507` memory cap exceeded.
- **`GET /healthz`** — `200 ok`.
- **`GET /metrics`** — on `WARPLINE_METRICS_BIND` (default
  `127.0.0.1:9090`), a separate `axum::serve` listener, not on the router
  above. Prometheus text exposition; includes `warpline_invoke_duration_us`
  (labeled by tenant), `warpline_invoke_total` (labeled by
  tenant/func/outcome) and
  `warpline_meter_dropped_total`.

## Writing a guest

A guest is a `wasm32-wasip2` component built against `wit/warpline.wit`'s
`handler` world:

```wit
world handler {
    import kv;        // get(key) -> option<list<u8>>; put(key, value)
    import log;       // emit(level, msg)
    import http-out;   // fetch(request) -> result<response, string>
    export handle: func(input: list<u8>) -> list<u8>;
}
```

Only `handle` is required — a guest need not call any of the imports.
Using `wit-bindgen` (Rust):

```rust
wit_bindgen::generate!({ world: "handler", path: "../../wit" });

struct MyGuest;
impl Guest for MyGuest {
    fn handle(input: Vec<u8>) -> Vec<u8> { input } // echo
}
export!(MyGuest);
```

`wasm32-wasip2` emits Component-Model binaries directly — no adapter step,
unlike `wasm32-wasip1`. See [`examples/hello-wasm`](./examples/hello-wasm)
for a complete, minimal example (build/upload/invoke instructions in its
own README), and `examples/test-guest` (used only to produce the test
fixture at `crates/core/tests/fixtures/test_guest.wasm`) for one that
exercises the loop/alloc/kv/log/http-out paths the bench harness and tests
drive.

## Bench results

Measured via `cargo bench -p warpline-core`
(`crates/core/benches/{runtime,report}.rs`) on an 8-core Intel i5-9300H
laptop against the `test_guest.wasm` fixture (trivial echo/loop/alloc
handler, not representative of real tenant code). Full numbers,
methodology, and caveats are in
[`PROGRESS.md`](./PROGRESS.md#bench-numbers-targets-per-projects-l3-l4md--p6-updated-weekly).

| Metric                                   | Target        | Result                              |
| ------------------------------------------ | --------------- | -------------------------------------- |
| Cold-start from `.cwasm`                  | < 1 ms        | 0.26 ms mean — **pass**              |
| Warm invocation p99 (10 ms budget)        | < 5 ms        | 0.121 ms — **pass**                  |
| Instances/sec/core                        | > 1,000       | 16,349 inv/s — **pass**              |
| CPU cap accuracy                          | budget ± 5 ms | −0.66 ms @10ms, +0.05 ms @50ms, −0.09 ms @100ms — **pass** |
| Memory cap accuracy                       | hard ceiling  | 16 MiB cap holds, peak 15.79 MiB — **pass** |
| Cost per million vs CF Workers            | within 2×     | back-of-envelope estimate only, see PROGRESS.md — not a real quote |

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo nextest run --workspace     # or `cargo test --workspace`
```

Most of the test suite runs with no external dependencies (`InsecureDev`
mode, in-memory `MemKv`, tempdir module registries). A second tier of
DB-backed tests (`crates/{host,control}/tests/db_mode.rs`) only runs when
`WARPLINE_TEST_DATABASE_URL` is set — they cover the admin route, API-key
auth, and the per-tenant function quota against a real Postgres:

```bash
docker compose up -d postgres
export WARPLINE_TEST_DATABASE_URL=postgres://warpline:warpline@localhost:5432/warpline
cargo nextest run --workspace
```

`crates/core`'s own tests read the committed guest fixture
(`crates/core/tests/fixtures/test_guest.wasm`) via `include_bytes!`, so
`cargo test -p warpline-core` never needs the `wasm32-wasip2` target
installed. Only rebuild the fixture — via `./scripts/build-guests.sh` —
when `examples/test-guest` changes; commit the result.

## Repository layout

```
warpline/
  Cargo.toml                          # workspace root (excludes examples/*)
  wit/warpline.wit                    # capability surface (Component Model)
  crates/
    core/                             # Engine + host imports + cache + registry + meter + auth
      src/{lib,runtime,types,kv,cache,registry,meter,auth}.rs
    host/                             # axum invoke API, :8080 (+ /metrics on :9090)
    control/                          # axum upload + admin API, :8081
  examples/
    hello-wasm/                       # minimal guest crate + its own README
    test-guest/                       # bench/test fixture source, not a public example
  migrations/0001_init.sql            # tenants, functions, meter tables
  migrations/0002_auth_config.sql     # api_keys, tenant resource caps, meter.ok
  scripts/build-guests.sh             # builds the guest examples, refreshes the test fixture
  Dockerfile                          # cargo-chef multi-stage, distroless cc
  docker-compose.yml                  # postgres + warpline-host + warpline-control
  fly.toml                            # single-region deployment manifest
  .github/workflows/ci.yml            # fmt + clippy + nextest (+ Postgres) + deny + MSRV + bench + guest-fixture build
  deny.toml, rust-toolchain.toml, .gitignore
  docs/
    specs/2026-05-28-warpline-design.md
    plans/2026-05-28-warpline-phase-1-scaffold.md
    plans/2026-09-26-warpline-phase-2-runtime.md
  PROGRESS.md                         # per-sprint tracker, bench numbers
```

## Roadmap / deferred

Out of scope for the current runtime, tracked for a later phase:

- **S3/MinIO-backed `.cwasm` registry** — the shared local volume covers a
  one-box deploy; a multi-host deploy needs a real object store behind the
  same `registry` trait boundary.
- **Instance pooling / warm-store reuse** (wasmtime's pooling allocator) —
  every invoke currently builds a fresh `Store`.
- **Single-flight dedupe on a cold digest** — a burst of concurrent
  first-invokes for one freshly-uploaded function each pay for their own
  `.cwasm` recompile today (`ponytail:` note in
  `crates/core/src/registry.rs`); a per-digest in-flight map would fix that
  if it shows up as real load.
- **Hyperlight** — sub-millisecond ephemeral instances (Microsoft, Mar
  2025) as a possible alternative isolation boundary; write-up candidate if
  it lands.

## License <a id="license"></a>

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](./LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.
