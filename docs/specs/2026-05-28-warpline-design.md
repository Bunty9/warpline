---
title: warpline — Multi-tenant WASM Function Runtime (P6)
status: draft
date: 2026-05-28
related:
    - ../../../backend-cloud-roadmap.md
    - ../../../projects-l3-l4.md
---

# warpline — Design Spec

> Companion spec lifted from `projects-l3-l4.md` § "P6 — Multi-tenant WASM
> Function Runtime (warpline)". Code blocks are the authoritative
> implementation reference for the scaffold; downstream phases extend,
> they do not contradict. Default stack pins live in
> `backend-cloud-roadmap.md` § 3.

## Problem

Customer-customization is a recurring need in any SaaS or agency product —
clients want hooks into automation flows. Today this usually means "open a
PR and redeploy." A WASM function host lets clients upload sandboxed code
in any language that compiles to WASI. **Interview pitch:** this is the
Cloudflare Workers / Fastly Compute / Fermyon Spin pattern — multi-tenant
compute is the *2026* growth area, especially in AI infra (Modal,
Together, Replicate, Anthropic).

## Architecture

```
+---------+      POST /tenants/{t}/functions/{f}   +-----------------------+
| client  | --------------------------------------> | warpline-control      |
+----+----+                                         |  - .wasm validate     |
     |                                              |  - compile to .cwasm  |
     | POST /tenants/{t}/functions/{f}/invoke       |  - store in S3/local  |
     |                                              +----+------------------+
     v                                                   |
+----+----------+      pull .cwasm by content-hash       |
| warpline-host |<----------------------------------------+
|  (axum API)   |
+----+----------+
     |
     | spawn instance (or reuse warm) with capability set
     v
+----+----------+
| wasmtime     |  Store::add_fuel + epoch_interruption
| Store        |  ResourceLimiter (memory cap, table cap)
|   - host_kv  |  Host imports: kv_get/kv_put (backed by P5!)
|   - host_log |  tracing-spans propagated as host fn
|   - host_http|  outbound HTTP via reqwest, allowlist per tenant
+--------------+
     |
     v
   billing/metering ledger (Postgres):
     (tenant, fn, instance_id, cpu_ms, mem_peak, calls)
```

## Stack

- `wasmtime` 27 (with `cranelift`, `parallel-compilation`, `component-model`).
- `wasi-preview1-component-adapter` for legacy `.wasm` modules.
- `wit-bindgen` for .wit IDL → Rust.
- `axum` + `tokio` for HTTP API.
- `aws-sdk-s3` (or local fs for demo) for `.cwasm` cache.
- `sqlx` for tenant + metering tables.

## Key Rust code

### `warpline.wit` (capability surface)

```wit
package warpline:host@0.1.0;

interface kv {
    get: func(key: string) -> option<list<u8>>;
    put: func(key: string, value: list<u8>);
}

interface log { emit: func(level: string, msg: string); }

interface http-out {
    record request { url: string, method: string, body: list<u8> }
    record response { status: u16, body: list<u8> }
    fetch: func(req: request) -> result<response, string>;
}

world handler {
    import kv;
    import log;
    import http-out;
    export handle: func(input: list<u8>) -> list<u8>;
}
```

### Host bootstrap with epoch-based CPU caps (`crates/host/src/runtime.rs`)

```rust
use wasmtime::{Engine, Config, Store, Module, Linker, ResourceLimiter};
use wasmtime_wasi::preview1::WasiP1Ctx;

pub struct HostCtx {
    pub tenant_id: String,
    pub fn_name: String,
    pub kv: Arc<dyn KvStore>,
    pub allowed_hosts: Vec<String>,
    pub mem_cap_bytes: usize,
    pub wasi: WasiP1Ctx,
}

pub struct TenantLimiter { pub mem_cap_bytes: usize }
impl ResourceLimiter for TenantLimiter {
    fn memory_growing(&mut self, current: usize, desired: usize, _max: Option<usize>) -> anyhow::Result<bool> {
        Ok(desired <= self.mem_cap_bytes && desired >= current)
    }
    fn table_growing(&mut self, _: u32, desired: u32, _: Option<u32>) -> anyhow::Result<bool> {
        Ok(desired <= 10_000)
    }
}

pub fn build_engine() -> anyhow::Result<Engine> {
    let mut cfg = Config::new();
    cfg.async_support(true)
       .epoch_interruption(true)
       .consume_fuel(false)               // epoch is cheaper than fuel
       .cranelift_opt_level(wasmtime::OptLevel::Speed)
       .parallel_compilation(true);
    Engine::new(&cfg).map_err(Into::into)
}

pub async fn invoke(
    engine: &Engine,
    module: &Module,
    ctx: HostCtx,
    input: Vec<u8>,
    cpu_budget_ms: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut store = Store::new(engine, ctx);
    store.limiter(|c| &mut TenantLimiter { mem_cap_bytes: c.mem_cap_bytes });
    store.set_epoch_deadline(1);

    // Background ticker: bump epoch every `cpu_budget_ms`. When invocation
    // hits its deadline, wasmtime traps the running fn.
    let engine2 = engine.clone();
    let _t = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(cpu_budget_ms)).await;
        engine2.increment_epoch();
    });

    let mut linker: Linker<HostCtx> = Linker::new(engine);
    register_host_fns(&mut linker)?;

    let instance = linker.instantiate_async(&mut store, module).await?;
    let handle = instance.get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "handle")?;
    let (out,) = handle.call_async(&mut store, (input,)).await?;
    Ok(out)
}
```

### Host imports (kv + log + http-out)

```rust
fn register_host_fns(linker: &mut Linker<HostCtx>) -> anyhow::Result<()> {
    linker.func_wrap_async("warpline:host/kv", "get",
        |mut caller: wasmtime::Caller<'_, HostCtx>, (key,): (String,)| Box::new(async move {
            let ctx = caller.data();
            let scoped = format!("t/{}/{}", ctx.tenant_id, key);
            Ok((ctx.kv.get(&scoped).await,))
        })
    )?;
    linker.func_wrap_async("warpline:host/kv", "put",
        |mut caller: wasmtime::Caller<'_, HostCtx>, (key, val): (String, Vec<u8>)| Box::new(async move {
            let ctx = caller.data();
            ctx.kv.put(&format!("t/{}/{}", ctx.tenant_id, key), val).await;
            Ok(())
        })
    )?;
    linker.func_wrap("warpline:host/log", "emit",
        |caller: wasmtime::Caller<'_, HostCtx>, (level, msg): (String, String)| {
            tracing::info!(tenant = caller.data().tenant_id, level, "%s", msg);
            Ok(())
        }
    )?;
    linker.func_wrap_async("warpline:host/http-out", "fetch",
        |mut caller: wasmtime::Caller<'_, HostCtx>, (req,): (HttpReq,)| Box::new(async move {
            let ctx = caller.data();
            let host = url::Url::parse(&req.url)?.host_str().unwrap_or("").to_string();
            if !ctx.allowed_hosts.iter().any(|h| h == &host) {
                return Ok((Err::<HttpResp, _>(format!("host {} not allowed", host)),));
            }
            let resp = reqwest::Client::new()
                .request(req.method.parse()?, &req.url)
                .body(req.body)
                .send().await?;
            Ok((Ok(HttpResp { status: resp.status().as_u16(), body: resp.bytes().await?.to_vec() }),))
        })
    )?;
    Ok(())
}
```

### Module cache (warm starts via `.cwasm`)

```rust
use sha2::{Sha256, Digest};

pub async fn load_or_compile(engine: &Engine, wasm_bytes: &[u8], cache_dir: &Path) -> anyhow::Result<Module> {
    let mut hasher = Sha256::new();
    hasher.update(wasm_bytes);
    let digest = hex::encode(hasher.finalize());
    let cached = cache_dir.join(format!("{}.cwasm", digest));
    if cached.exists() {
        // unsafe but standard — wasmtime documents the rules for cwasm trust
        let bytes = std::fs::read(&cached)?;
        unsafe { Module::deserialize(engine, &bytes) }
    } else {
        let module = Module::new(engine, wasm_bytes)?;
        let bytes = module.serialize()?;
        std::fs::write(&cached, &bytes)?;
        Ok(module)
    }
}
```

### Metering (write after every invocation)

```rust
pub async fn record(pool: &PgPool, tenant: &str, func: &str, cpu_us: u64, mem_peak: usize) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO meter (tenant, func, cpu_us, mem_peak_bytes, ts) VALUES ($1,$2,$3,$4, now())",
        tenant, func, cpu_us as i64, mem_peak as i64
    ).execute(pool).await?;
    Ok(())
}
```

## Deployment

- Fly.io single region (multi-tenant single host is the whole point).
- S3 (or MinIO) for `.cwasm` cache.
- Postgres (Neon) for tenant + metering.
- Internal demo: wire as a customization hook into a real customer's
  automation flow — production user beats synthetic load any day.

## Eval / benchmarks

- Cold-start latency: full compile from `.wasm` vs `.cwasm` deserialize.
  Target `.cwasm` < 1ms.
- Warm invocation p99 < 5ms for a 10ms-budget function.
- Instances/sec/core under a benchmark of trivial `handle()` impls.
- CPU cap accuracy: upload an infinite-loop module, verify it traps
  within `cpu_budget_ms ± 5ms`.
- Memory cap accuracy: upload a runaway-allocator module, verify cap
  holds.
- Cost per million invocations vs equivalent Cloudflare Workers
  pricing.

## Stretch / writeup angle

- Component Model: switch from `.wasm` preview1 to full Component-Model
  with `wit-bindgen` — major 2026 wow factor.
- Hyperlight integration (Microsoft, Mar 2025) — sub-millisecond
  instances for ephemeral workloads.
- Blog post: "I built a function runtime for my agency clients and it
  does X invocations/sec on a Hetzner box."

## Source references

- Wasmtime docs + Bytecode Alliance posts.
- WASI Preview 2 status (eunomia.dev 2025-02).
- wasmCloud / Fermyon Spin source for prior art.
- `gemini-research.md` (no direct section — extends Domain 2 patterns).
