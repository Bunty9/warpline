# Storefront: embedding warpline in your app

A small but real example of what warpline is for: **untrusted customer code
running inside your request path**.

A storefront lets each merchant upload a `checkout` hook (a WebAssembly
component). At checkout the app prices the order, runs the merchant's hook
in a sandbox with that merchant's CPU, memory and network limits, validates
what comes back and applies it. This directory is meant to be copied.

```
examples/storefront/
  app/     axum app embedding warpline-core (binaries: storefront, fraud-mock)
  hook/    the merchant's hook: a Rust wasm32-wasip2 guest
  build.sh builds the hook into app/tests/fixtures/checkout_hook.wasm
  demo.sh  curl walkthrough with assertions
  docker-compose.yml, Dockerfile
```

## Run it

```sh
./demo.sh --up          # docker compose up -d --build, then the walkthrough
docker compose down -v  # when done
```

Storefront is on `localhost:3000`, the fraud mock on `localhost:3100`.
Without Docker, start Postgres, then:

```sh
export DATABASE_URL=postgres://... ADMIN_TOKEN=demo-admin-token FRAUD_API=http://127.0.0.1:3100
cargo run --bin fraud-mock &
cargo run --bin storefront            # in app/
FRAUD_HOST=127.0.0.1 ./demo.sh
```

Tests: `cd app && WARPLINE_TEST_DATABASE_URL=postgres://... cargo test`
(the DB test is skipped when the variable is unset). Rebuilding the hook
needs the `wasm32-wasip2` target; `./build.sh` uses the toolchain pinned in
`examples/rust-toolchain.toml` so the committed fixture stays reproducible.

## The API

| Route | Auth | Does |
| --- | --- | --- |
| `POST /admin/merchants` `{name, allowed_hosts?, cpu_budget_ms?, mem_cap_bytes?}` | `Bearer $ADMIN_TOKEN` | creates the merchant, returns its `api_key` once |
| `PUT /merchant/{name}/hook` (raw wasm body) | merchant key | publishes the hook |
| `GET /merchant/{name}/usage` | merchant key | invocation, error, CPU totals |
| `POST /shops/{merchant}/checkout` `{customer, items:[{sku,qty,unit_cents}]}` | public | prices the order, runs the hook |
| `GET /healthz` | - | liveness |

The hook receives `{customer, items, subtotal_cents, fraud_api}` and answers
`{approved, discount_cents, message}`. The demo hook gives a 10% discount from
a customer's 3rd order (kept in `kv`), asks the fraud service for a score
over `http-out` and rejects scores above 80, and logs each decision.

## Integration points

Each `[warpline N]` marker in the code matches a section here.

**0. Dependency.** `app/Cargo.toml` needs `warpline-core = "0.2"`. Its
`[patch.crates-io]` section only makes that resolve to this repository's
checkout: **delete that whole block** when you copy. The hook crate reads
`warpline.wit` from the repo by relative path; when you copy, vendor that one
file (say into `hook/wit/`) and point `wit_bindgen::generate!`'s `path` at it.

**1. The runtime** ([`app/src/lib.rs`](app/src/lib.rs), `build_runtime`). One
`Runtime` per process, cheap to clone. Config lives in `RuntimeConfig`; the
library reads no environment variables, so [`main.rs`](app/src/main.rs) is
where env becomes config. `allow_private_egress = true` is set here only
because the fraud mock is on localhost or a compose service name.
**Production keeps it `false`** (the default) and allowlists public hosts, so a
merchant hook cannot reach your internal network. Guest `kv` uses the default
in-memory store, so loyalty counters reset on restart; implement `KvStore` for
durable ones.

**2. Postgres** (`main.rs`). You create the pool and call `pg::migrate`
yourself. Everything warpline stores lives in a `warpline` schema, so it can
share your database.

**3. Tenants** (`create_merchant`). `pg::create_tenant` stores a merchant with
its `Limits` (CPU budget, memory cap, `allowed_hosts`) and issues an API key in
one transaction. Only the key's hash is kept. Here the app maps a merchant to
a warpline tenant one-to-one.

**4. Auth** (`authorize`). `Authenticator` checks `Authorization: Bearer` keys
against the tenant in the URL: 401 for an unknown key, 403 for another
merchant's. Lookups are cached for a few seconds, so revoking a key or
changing limits takes up to that TTL to be seen.

**5. Publish** (`upload_hook`). `runtime.publish(tenant, function, wasm)`
compiles the component, checks it matches the `handler` world and
atomically makes it live. Garbage or a wrong-world component is a
`PublishError` (422 here) and nothing changes.

**6. Invoke** (`checkout`). `runtime.invoke(tenant, function, input, &limits)`
runs the guest under the merchant's limits; `invoke` never queues. The
`http-out` allowlist is part of the limits: merchant `shop-*` in the demo
lists `fraud-mock`, the second merchant lists nothing and its fetch fails.

**7. Untrusted output and failure policy**
([`app/src/checkout.rs`](app/src/checkout.rs)). The hook's answer is checked:
at most 64 KiB, exactly the expected JSON shape, `discount_cents <= subtotal`,
message at most 200 characters. Invalid output counts as a hook failure. The
policy per result:

| Result | App does | Why |
| --- | --- | --- |
| valid decision | applies it | |
| `NotFound` (no hook uploaded) | approve, no discount | a new merchant can sell before writing code |
| `CpuBudgetExceeded`, `MemoryCapExceeded`, `WallClockTimeout`, `GuestTrap`, `OutputTooLarge`, `Load`, invalid output | fail open: approve, no discount, log a warning | a broken hook should not cost the merchant a sale |
| `Overloaded`, `TenantBusy` | 503 with `Retry-After: 1` | capacity, not the merchant's fault; retry helps |

Fail-closed is one line away in `checkout` if your domain needs it (a
mandatory fraud gate, say). New `InvokeError` variants land in the fail-open
arm because the enum is non-exhaustive.

**8. Metering and usage** (`build_runtime`, `usage`). `PgMeter` records every
invocation that reached a guest (successful or not) into `warpline.meter`
without blocking the request. `pg::usage_summary` sums it. Writes are batched,
so the newest invocations show up a moment later.

**9. Shutdown** (`main.rs`). After the server stops taking requests,
`meter.shutdown(timeout)` flushes queued rows. Skip it and the last
invocations are lost.

## The hook

[`hook/src/lib.rs`](hook/src/lib.rs) is an ordinary Rust crate: `serde`,
`serde_json` and `wit-bindgen`, built as a `cdylib` for `wasm32-wasip2`. It
never sees the fraud host name in its own source; the app passes `fraud_api`
in the input. Any language that can build a component for the same WIT world
works.
