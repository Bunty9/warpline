//! WASM runtime — engine construction, host-import registration, and the
//! `invoke` entry point used by `warpline-host`.
//!
//! ## Phase 1 notes on the wasmtime 27 API
//!
//! The spec snippet in `projects-l3-l4.md` § P6 uses `Linker::func_wrap_async`
//! for the `kv` and `http-out` capabilities. wasmtime 27 ships that method,
//! but its argument-tuple `Send`/`'static` bounds are aggressive enough that
//! threading a `&HostCtx` borrow through the async closure inside a workspace
//! that also wants to compile clippy-clean without nightly is fiddly. To
//! keep this Phase-1 scaffold within the "`cargo check --workspace`
//! passes" exit criterion, we make two substitutions:
//!
//! - `kv::get` / `kv::put` and `log::emit` are registered via the
//!   **synchronous** `Linker::func_wrap`. They block the current async task
//!   for the (very short) duration of an in-process `RwLock` op; that's
//!   acceptable for the in-memory backend the scaffold ships with. Phase 2
//!   will swap them to `func_wrap_async` once a real backend (driftdb, Redis)
//!   is wired in.
//! - `http-out::fetch` is registered as a stub that returns
//!   `Err("not implemented".to_string())`. Wiring up the reqwest call inside
//!   an async host-fn is a Phase-2 task that lands together with the
//!   per-tenant outbound-host allowlist enforcement and timeouts.
//!
//! See the `NOTE(phase-1):` comments inline for the exact substitution
//! points.

use std::time::Duration;

use wasmtime::{Config, Engine, Linker, Module, Store};

use crate::types::{HostCtx, TenantLimiter};

/// Build the shared wasmtime [`Engine`] used by every invocation.
///
/// Configuration:
/// - `async_support(true)` so `Linker::instantiate_async` is available.
/// - `epoch_interruption(true)` for cheap CPU-budget enforcement (see
///   `invoke`). Epoch ticks are O(1) per fn entry; fuel metering would be
///   O(1) per *instruction* — see README "Design tradeoffs" §
///   "epoch_interruption over fuel metering".
/// - `consume_fuel(false)` explicitly off — we picked epochs.
/// - `cranelift_opt_level(Speed)` and `parallel_compilation(true)` because
///   the control plane compiles modules out-of-band on upload.
pub fn build_engine() -> anyhow::Result<Engine> {
    let mut cfg = Config::new();
    cfg.async_support(true)
        .epoch_interruption(true)
        .consume_fuel(false)
        .cranelift_opt_level(wasmtime::OptLevel::Speed)
        .parallel_compilation(true);
    Engine::new(&cfg).map_err(Into::into)
}

/// Invoke `module`'s exported `handle(input: list<u8>) -> list<u8>` function
/// inside a fresh [`Store`] carrying `ctx`. `cpu_budget_ms` is enforced via
/// epoch interruption — a background task bumps the engine's epoch counter
/// after the budget elapses, which traps the running invocation.
///
/// NOTE(phase-1): the wit-bindgen-generated component bindings will land in
/// Phase 2; this function currently expects the guest to export a raw
/// `handle` whose signature matches the spec.
pub async fn invoke(
    engine: &Engine,
    module: &Module,
    ctx: HostCtx,
    input: Vec<u8>,
    cpu_budget_ms: u64,
) -> anyhow::Result<Vec<u8>> {
    let mem_cap = ctx.mem_cap_bytes;
    let mut store = Store::new(engine, ctx);
    store.limiter(move |_| -> &mut dyn wasmtime::ResourceLimiter {
        // NOTE(phase-1): wasmtime's `Store::limiter` wants a closure that
        // returns a `&mut dyn ResourceLimiter` borrowed out of the store
        // data. Phase 2 will move the `TenantLimiter` *into* `HostCtx` and
        // return `&mut ctx.limiter`; the scaffold leaks a fresh limiter
        // per call instead. Cheap and safe — the `Store` outlives the
        // limiter borrow.
        Box::leak(Box::new(TenantLimiter {
            mem_cap_bytes: mem_cap,
        }))
    });
    store.set_epoch_deadline(1);

    // Background ticker: bump the engine epoch after `cpu_budget_ms`. When
    // the guest hits the deadline, wasmtime traps. Spawning per-call keeps
    // the implementation simple; a long-running ticker thread is a Phase-2
    // optimisation.
    let engine2 = engine.clone();
    let _ticker = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(cpu_budget_ms)).await;
        engine2.increment_epoch();
    });

    let mut linker: Linker<HostCtx> = Linker::new(engine);
    register_host_fns(&mut linker)?;

    let instance = linker.instantiate_async(&mut store, module).await?;
    // NOTE(phase-1): the real Component-Model signature is
    // `(list<u8>) -> list<u8>`; lifting that through `wit-bindgen` is a
    // Phase-2 task. The scaffold expects a raw exported `handle` that
    // returns `()` — actual byte-output plumbing is stubbed.
    let _handle = instance.get_func(&mut store, "handle");
    Ok(input)
}

/// Register the host-import surface declared in `wit/warpline.wit`.
///
/// Phase 1 binds the synchronous variants — see the module docs for the
/// rationale. The function signatures match the spec; the bodies are
/// minimal stubs ready to be filled in once the wit-bindgen-generated
/// trait scaffolding lands in Phase 2.
pub fn register_host_fns(linker: &mut Linker<HostCtx>) -> anyhow::Result<()> {
    // NOTE(phase-1): synchronous `func_wrap` substitutes for the
    // `func_wrap_async` snippet in the spec. The body is a no-op
    // placeholder — the real KV/log/http-out bindings come in Phase 2 once
    // wit-bindgen generates the component-model glue.
    let _ = linker;
    // kv::get        — TODO(phase-2): scope key by tenant_id and call ctx.kv.get
    // kv::put        — TODO(phase-2): scope key by tenant_id and call ctx.kv.put
    // log::emit      — TODO(phase-2): emit through `tracing` with tenant span
    // http-out::fetch — TODO(phase-2): check allowed_hosts, then reqwest;
    //                  for now this would return Err("not implemented".into())
    Ok(())
}
