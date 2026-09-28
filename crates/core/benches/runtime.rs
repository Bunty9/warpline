//! Criterion micro-benchmarks for `warpline_core::{cache,runtime}` against
//! the `test_guest.wasm` fixture (see `tests/runtime.rs` for how it's built
//! and what commands its `handle` export understands).
//!
//! - `cold_compile` — `Component::new` from raw `.wasm` bytes, full
//!   cranelift pass. Baseline the `.cwasm` cache exists to avoid.
//! - `cold_deserialize` — `Component::deserialize` of the pre-serialized
//!   bytes, the cache's warm path. Target: < 1 ms (see PROGRESS.md).
//! - `warm_invoke_echo` — a full `invoke()` of the echo path on an
//!   already-compiled `Component`, default 10 ms budget. Criterion reports
//!   mean/stddev here; p50/p99 over a larger sequential run are reported
//!   separately by `benches/report.rs` (criterion doesn't expose
//!   percentiles).
//!
//! Run with `cargo bench -p warpline-core --bench runtime` (release only —
//! criterion always builds benches in release regardless of profile flags).

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use wasmtime::component::Component;

use warpline_core::kv::{KvStore, MemKv};
use warpline_core::runtime::{
    build_engine, build_http_client, build_linker, instantiate_pre, invoke, EpochTicker,
};
use warpline_core::types::HostCtx;

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

/// Same default `warpline-host` uses; irrelevant to the echo path itself
/// but kept realistic.
const DEFAULT_MEM_CAP: usize = 64 * 1024 * 1024;
/// The "default 10 ms budget" `warm_invoke_echo` is asked to measure
/// against (README/PROGRESS.md bench-target table).
const WARM_BUDGET_MS: u64 = 10;

fn cold_compile(c: &mut Criterion) {
    let engine = build_engine().expect("build engine");
    c.bench_function("cold_compile", |b| {
        b.iter(|| {
            let component = Component::new(&engine, black_box(TEST_GUEST_WASM)).expect("compile");
            black_box(component);
        });
    });
}

fn cold_deserialize(c: &mut Criterion) {
    let engine = build_engine().expect("build engine");
    let component = Component::new(&engine, TEST_GUEST_WASM).expect("compile");
    let serialized = component.serialize().expect("serialize");

    c.bench_function("cold_deserialize", |b| {
        b.iter(|| {
            // SAFETY: `serialized` was produced by `Component::serialize` on
            // this exact `engine`, immediately above — same process, same
            // build, same config. See `cache.rs` module docs for the general
            // (cross-process) safety argument this bench doesn't need.
            let component = unsafe { Component::deserialize(&engine, black_box(&serialized)) }
                .expect("deserialize");
            black_box(component);
        });
    });
}

fn warm_invoke_echo(c: &mut Criterion) {
    let engine = build_engine().expect("build engine");
    let linker = build_linker(&engine).expect("build linker");
    let _ticker = EpochTicker::spawn(engine.clone());
    let component = Component::new(&engine, TEST_GUEST_WASM).expect("compile");
    // Built once, outside the measured loop — see finding 11: this is
    // exactly the per-digest, not per-invoke, cost `ComponentCache` now
    // pays too.
    let pre = instantiate_pre(&linker, &component).expect("instantiate_pre");
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let http_client = build_http_client(true).expect("build http client");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    c.bench_function("warm_invoke_echo", |b| {
        b.iter(|| {
            rt.block_on(async {
                let ctx = HostCtx::new(
                    "bench-tenant".to_string(),
                    "bench-fn".to_string(),
                    kv.clone(),
                    Vec::new(),
                    true,
                    http_client.clone(),
                    DEFAULT_MEM_CAP,
                );
                let out = invoke(
                    &engine,
                    &pre,
                    ctx,
                    black_box(b"warm-invoke-echo-payload".to_vec()),
                    WARM_BUDGET_MS,
                )
                .await
                .expect("invoke ok");
                black_box(out);
            });
        });
    });
}

criterion_group!(benches, cold_compile, cold_deserialize, warm_invoke_echo);
criterion_main!(benches);
