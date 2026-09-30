# warpline

> Multi-tenant WASM function runtime in Rust. Wasmtime 49 Component Model
> host + control plane, epoch-based CPU caps, content-addressed `.cwasm`
> warm cache, deny-by-default outbound HTTP. The Cloudflare Workers /
> Fastly Compute / Fermyon Spin pattern, sized for a single box.

[![ci](https://github.com/Bunty9/warpline/actions/workflows/ci.yml/badge.svg)](https://github.com/Bunty9/warpline/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/warpline-core.svg)](https://crates.io/crates/warpline-core)
[![docs.rs](https://docs.rs/warpline-core/badge.svg)](https://docs.rs/warpline-core)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

| Crate | What it is |
| --- | --- |
| [`warpline-core`](https://crates.io/crates/warpline-core) | Library: sandboxed runtime, host bindings, caps, registry, auth, metering |
| [`warpline-host`](https://crates.io/crates/warpline-host) | Binary: invoke server (`cargo install warpline-host`) |
| [`warpline-control`](https://crates.io/crates/warpline-control) | Binary: upload + admin API (`cargo install warpline-control`) |

## Embedding warpline in your app

`warpline-core` is a library: hold a `Runtime`, publish components under
`(tenant, function)` names and invoke them. The same example runs as a
doctest in [`crates/core/src/runtime.rs`](crates/core/src/runtime.rs). Its
`# ` scaffolding lines (a `main` and a tempdir) are shown here as real code:

```rust
use std::sync::Arc;
use warpline_core::{Bytes, Limits, MemKv, Runtime, RuntimeConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let runtime = Runtime::builder(RuntimeConfig::new(dir.path()))
        .kv(Arc::new(MemKv::new()))
        .build()?;

    // A component implementing the `handler` world (see `wit/warpline.wit`);
    // this one, from the test suite, echoes its input.
    let wasm = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test_guest.wasm"));
    runtime.publish("acme", "hello", Bytes::from_static(wasm)).await?;

    let limits = Limits::new(50, 32 << 20)?;
    let out = runtime.invoke("acme", "hello", b"hi".to_vec(), &limits).await?;
    assert_eq!(out.output, b"hi");
    println!("{} bytes back, {} us cpu", out.output.len(), out.usage.cpu_us);
    Ok(())
}
```

`invoke` never queues: a tenant over its in-flight cap gets `TenantBusy`
(429) and an invocation the memory budget cannot admit gets `Overloaded`
(503); `InvokeError::http_status` gives the mapping. Plug in your own
`KvStore` and `MeterSink` through the builder. With the default `postgres`
feature, `warpline_core::pg` adds schema-isolated migrations
(`pg::migrate`, everything in a `warpline` schema), API-key auth
(`Authenticator`), tenant admin (`create_tenant`, `patch_tenant`,
`set_limits`, `usage_summary`) and a batching `PgMeter`. The library never
reads environment variables or migrates implicitly.

| You want | Use |
| --- | --- |
| A ready-made HTTP invoke and upload server, configured by environment variables | The binaries: `warpline-host` (+ `warpline-control`, or `WARPLINE_EMBED_CONTROL=1` for one process) |
| Your own routes, auth, storage or metering around the runtime | Embed `warpline-core` |
| Untrusted customer hooks called from inside your own app's request path | Embed `warpline-core` |
| One box, customers upload and call functions directly | The binaries |

## The problem

Customer-customization is a recurring need in any SaaS or agency product —
clients want hooks into automation flows. Today this usually means "open a
PR and redeploy." A WASM function host lets clients upload sandboxed code
in any language that compiles to WASI, with real resource caps and a
metered capability surface. **warpline** is that host, sized to run a
real customer pilot on one box: precompiled-module cache, epoch-based CPU
caps, memory limits and an outbound-HTTP allowlist.

## Architecture

Two axum services share one on-disk module registry and one Postgres
database; the host's Prometheus scrape endpoint runs on a second,
loopback-only listener so it never shares a socket with whatever's publicly
reachable. On Fly (one machine, one volume) the host runs both APIs in one
process via `WARPLINE_EMBED_CONTROL=1`.

```
                         +----------------------+
                         |       Postgres        |
                         | schema `warpline`:     |
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

Both binaries are thin HTTP layers over `warpline_core::Runtime` and
`warpline_core::pg`. `warpline-control` calls `Runtime::stage`, which
compiles an uploaded `.wasm` to a `Component`, type-checks its
imports/exports against the `warpline:host/handler` world, and only then
persists the source bytes + compiled `.cwasm`. It then takes a per-tenant
advisory lock in a Postgres transaction, checks the function quota, upserts
the `functions` row, and calls `Runtime::activate` to write the
`(tenant, func) -> digest` pointer file *before* committing (restoring the
previous pointer if the commit fails). `warpline-host` calls
`Runtime::invoke`, which reads the pointer, resolves the digest through an
in-memory LRU backed by the shared `cwasm/` directory, and calls the
component's `handle` export inside a fresh `wasmtime::Store` with per-tenant
caps installed. There is no S3/MinIO in
this deployment shape — the module registry is a single shared volume,
which is enough for a one-box deploy (see "Roadmap / deferred").

## Stack

| Layer              | Crate(s)                                                            |
| ------------------ | -------------------------------------------------------------------- |
| Runtime            | `wasmtime` 49 + `wasmtime-wasi` 49 p2 (async, cranelift, parallel-compilation, component-model) |
| HTTP server        | `axum` 0.8 + `tokio` 1.47                                             |
| Database           | `sqlx` 0.8 + Postgres 16 (`warpline_core::pg`, feature `postgres`)    |
| KV trait + in-mem  | `async-trait` + `tokio::sync::RwLock` (`MemKv`)                       |
| Module cache       | `sha2` + `hex` (content-hash → `.cwasm`) + `lru` (in-memory `Component` cache) |
| Outbound HTTP      | `reqwest` (rustls-tls), allowlist + no redirects + no proxy + private-address blocking |
| Observability      | `tracing` (+ `tracing-subscriber` json) + `metrics` + `metrics-exporter-prometheus` |
| Errors             | `thiserror` 2 in the libraries, `anyhow` in the binaries              |

## Security model / isolation

Every cap below is enforced per invocation or per tenant. Values marked
"configurable" come from the `tenants` table (set through
`POST /admin/tenants/{tenant}` or `pg::create_tenant`/`pg::patch_tenant`,
validated against the ranges in `warpline_core::types`) and are
[`Limits::default`](crates/core/src/types.rs) under
`WARPLINE_INSECURE_DEV=1`. Process-wide values that name a `RuntimeConfig`
field are set by whoever builds the `Runtime`; the binaries use the
defaults shown.

| Cap                                             | Value                                          | Enforced by |
| ------------------------------------------------ | ----------------------------------------------- | ----------- |
| CPU budget (configurable)                       | 1 – 10,000 ms, default 100 ms                   | epoch-deadline callback, `sandbox::run` |
| Memory cap — linear memory (configurable)       | 1 MiB – 512 MiB, default 64 MiB                 | `TenantLimiter::memory_growing` |
| Core instances per `Store`                      | 32                                               | `TenantLimiter::instances` |
| Core tables per `Store`                         | 8                                                 | `TenantLimiter::tables` |
| Core linear memories per `Store`                | 4                                                 | `TenantLimiter::memories` |
| Table elements per `Store`                      | 10,000                                            | `TenantLimiter::table_growing` |
| KV key size                                     | 512 B                                             | `warpline::host::kv::Host::get`/`put` |
| KV value size (per `put`)                       | 1 MiB                                             | `kv::Host::put` |
| KV `put` calls per invocation                   | 1,000                                             | `HostCtx::kv_put_count` |
| KV `put` bytes per invocation                   | 8 MiB                                             | `HostCtx::kv_put_bytes` |
| KV storage per tenant (`MemKv`)                 | 16 MiB                                            | `kv::DEFAULT_TENANT_CAP_BYTES` |
| Log line length                                 | 4 KiB (truncated, not trapped)                    | `log::Host::emit` |
| Log lines per invocation                        | 100 (dropped after, one "suppressed" notice)      | `log::Host::emit` |
| Log bytes per invocation                        | 64 KiB                                            | `log::Host::emit` |
| `http-out` response body                        | 1 MiB (fetch errs past this)                      | `sandbox::http_fetch` |
| `http-out` request timeout                      | 5 s                                               | shared `reqwest::Client` |
| `http-out` allowed hosts per tenant             | 64                                                | `validate_allowed_hosts` |
| `http-out` redirects                            | none followed                                     | `reqwest::redirect::Policy::none()` |
| `http-out` proxy                                | disabled (`no_proxy()`), so a proxy can't dodge the allowlist | `build_http_client` |
| `http-out` private/loopback/link-local targets  | blocked unless `WARPLINE_ALLOW_PRIVATE_EGRESS=1`  | `is_blocked_ip`, `GuardedResolver` |
| Upload body (`.wasm`)                           | 16 MiB                                            | `UPLOAD_BODY_LIMIT_BYTES` |
| Invoke request body                             | 1 MiB                                             | `INVOKE_BODY_LIMIT_BYTES` |
| Functions per tenant                            | 100                                               | `DEFAULT_MAX_FUNCTIONS_PER_TENANT` (`warpline-control`), enforced in the publish transaction |
| Compile concurrency (process-wide)              | `max(available_parallelism / 2, 1)` permits       | `RuntimeConfig::compile_concurrency` |
| In-memory component cache                       | 256 entries, 512 MiB total serialized bytes       | `RuntimeConfig::component_cache_entries` / `component_cache_bytes` |
| Epoch tick                                      | 1 ms                                              | `sandbox::EpochTicker`, `EPOCH_TICK_MS` |
| Wall-clock backstop per invoke                  | `cpu_budget_ms` + http timeout (5 s) + 1 s slack  | `tokio::time::timeout` around the whole call |
| Admission budget (process-wide guest memory)    | 1 GiB (each invoke weighs its memory cap, whole MiB); over budget: fail fast, `503` | `RuntimeConfig::memory_budget_bytes` |
| In-flight invocations per tenant                | 32; over the cap: fail fast, `429`                 | `RuntimeConfig::max_in_flight_per_tenant` |
| Output size per invocation                      | 8 MiB; over the cap: `502`                         | `RuntimeConfig::max_output_bytes` |

Guests get a deny-by-default WASI p2 context: no preopens, no env, no args,
no sockets — only what a `wasm32-wasip2` Rust std needs to link (clocks,
random, a stdio sink). Outbound network access exists only through the
`warpline:host/http-out` import, which is allowlisted per tenant.

### CPU budget: one epoch ticker, cooperative yield

CPU budget is enforced by wasmtime's epoch interruption, driven by a single
background thread per `Engine` ([`EpochTicker`](crates/core/src/sandbox.rs))
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
binaries call `pg::migrate` on boot against `DATABASE_URL`).
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
tenant keep working (each key hash is its own row in `api_keys`). A body
with limits has PATCH semantics: fields you leave out keep the tenant's
current values (the defaults, for a new tenant), so
`{"cpu_budget_ms": 250}` does not reset the allowlist.

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

Every environment variable either binary reads, with its default (the
embedding library reads none):

| Variable                        | Read by         | Default                                                | Purpose |
| -------------------------------- | ---------------- | ------------------------------------------------------- | ------- |
| `DATABASE_URL`                  | host, control    | unset (refuses to start unless `WARPLINE_INSECURE_DEV=1`) | Postgres connection string. Enables auth, tenant limits, and metering; the binaries run `pg::migrate` on boot (everything lives in the `warpline` schema). Pool: 10 connections, 2 s acquire timeout. |
| `WARPLINE_INSECURE_DEV`         | host, control    | unset                                                    | Set to `1` to run without Postgres: no auth enforced, every tenant gets `Limits::default` (empty allowlist, 100 ms CPU, 64 MiB memory), invocations are logged at debug level instead of metered. Not for production. |
| `WARPLINE_ADMIN_TOKEN`          | control (host if embedded) | unset (`/admin/tenants/{tenant}` 404s)                   | Bearer token guarding the admin route. |
| `WARPLINE_MODULES_DIR`          | host, control    | `./modules`                                              | Root of the shared module registry (`wasm/`, `cwasm/`, `tenants/`). |
| `WARPLINE_HOST_BIND`            | host             | `127.0.0.1:8080` under `WARPLINE_INSECURE_DEV`, else `0.0.0.0:8080` | `warpline-host`'s invoke-API listen address. |
| `WARPLINE_CONTROL_BIND`         | control (host if embedded) | `127.0.0.1:8081` under `WARPLINE_INSECURE_DEV`, else `0.0.0.0:8081` | `warpline-control`'s listen address. |
| `WARPLINE_METRICS_BIND`         | host             | `127.0.0.1:9090`                                         | Separate `/metrics` listener — never on the main router (would leak per-tenant series to whatever's publicly reachable). |
| `WARPLINE_AUTH_CACHE_TTL_SECS`  | host, control    | `30` (`0` disables)                                      | TTL of the in-process API-key lookup cache (positive and negative results). Key revocation and tenant config changes take up to this long to apply. |
| `WARPLINE_EMBED_CONTROL`        | host             | unset                                                    | Set to `1` to also serve the control-plane API from the host process (same engine, same volume), and run the startup blob GC there. Needs the `embed-control` cargo feature (on by default; ignored with a warning without it). Used by `fly.toml`, since a Fly volume attaches to one machine. |
| `RUST_LOG`                      | host, control    | `info`                                                   | `tracing` filter directive; logs are JSON. |
| `WARPLINE_ALLOW_PRIVATE_EGRESS` | host             | unset (`false`)                                          | Set to `1` to let `http-out` reach loopback/private/link-local addresses (local dev only). |
| `WARPLINE_TEST_DATABASE_URL`    | tests only       | unset (DB-backed tests skip themselves)                  | Postgres URL for `crates/core/tests/pg.rs` and `crates/{host,control}/tests/db_mode.rs`. |

An environment variable that is set but empty counts as unset. The binaries
are the only place that reads the environment; `warpline-core` never does.

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
    applies the config fields given (PATCH semantics: omitted fields keep
    their current values, or the defaults for a new tenant), always issues
    a fresh `wl_`-prefixed key.
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
  - `429` the tenant already has its maximum invocations in flight.
  - `500` guest trap, failed to instantiate, or an internal error.
  - `502` the guest returned more than the output cap (8 MiB).
  - `503` the host's memory admission budget is full; retry shortly.
  - `507` memory cap exceeded.
- **`GET /healthz`** — `200 ok`.
- **`GET /metrics`** — on `WARPLINE_METRICS_BIND` (default
  `127.0.0.1:9090`), a separate `axum::serve` listener, not on the router
  above. Prometheus text exposition; includes `warpline_invoke_duration_us`
  (labeled by tenant), `warpline_invoke_total` (labeled by
  tenant/func/outcome) and
  `warpline_meter_dropped_total`.

## Writing a guest

A guest is a `wasm32-wasip2` component built against `crates/core/wit/warpline.wit`'s
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
wit_bindgen::generate!({ world: "handler", path: "../../crates/core/wit" });

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
methodology, and caveats are in [`PROGRESS.md`](./PROGRESS.md).

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

Most of the test suite runs with no external dependencies (dev mode without
a database, in-memory `MemKv`, tempdir module registries). A second tier of
DB-backed tests (`crates/core/tests/pg.rs`,
`crates/{host,control}/tests/db_mode.rs`) only runs when
`WARPLINE_TEST_DATABASE_URL` is set — they cover the admin route, API-key
auth, tenant limits, metering, and the per-tenant function quota against
a real Postgres:

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
  crates/
    core/                             # Runtime facade + sandbox + cache + registry (+ pg)
      src/{lib,runtime,sandbox,types,kv,cache,registry,meter,error}.rs
      src/pg/{mod,auth,admin,meter}.rs  # feature `postgres`: migrate, auth, admin, PgMeter
      wit/warpline.wit                # capability surface (Component Model)
      migrations/0001_warpline.sql    # `warpline` schema: tenants, api_keys, functions, meter
    host/                             # axum invoke API, :8080 (+ /metrics on :9090)
    control/                          # axum upload + admin API, :8081
  examples/
    hello-wasm/                       # minimal guest crate + its own README
    test-guest/                       # bench/test fixture source, not a public example
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
    plans/2026-09-28-publishing.md
    plans/2026-09-30-warpline-0.2-embedding.md
  PROGRESS.md                         # per-sprint tracker (Phase 1 is historical), 0.2 status, bench numbers
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
  `.cwasm` recompile today (see `ComponentCache` in
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
