//! Criterion micro-benchmarks for `warpline_core` against the
//! `test_guest.wasm` fixture (see `tests/runtime.rs` for what commands its
//! `handle` export understands).
//!
//! - `cold_compile` — `Component::new` from raw `.wasm` bytes, full
//!   cranelift pass. Baseline the `.cwasm` cache exists to avoid.
//! - `cold_deserialize` — `Component::deserialize` of the pre-serialized
//!   bytes, the cache's warm path. Target: < 1 ms.
//! - `warm_invoke_echo` — a full `Runtime::invoke` of the echo path on an
//!   already-loaded component, 10 ms budget. Criterion reports mean/stddev
//!   here; p50/p99 over a larger sequential run are reported separately by
//!   `benches/report.rs` (criterion doesn't expose percentiles).
//!
//! Run with `cargo bench -p warpline-core --bench runtime` (release only —
//! criterion always builds benches in release regardless of profile flags).

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use warpline_core::wasmtime::component::Component;
use warpline_core::{Bytes, Limits, Runtime, RuntimeConfig};

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

/// The "default 10 ms budget" `warm_invoke_echo` is asked to measure
/// against.
const WARM_BUDGET_MS: u64 = 10;

fn runtime() -> (Runtime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = Runtime::new(RuntimeConfig::new(dir.path())).expect("build runtime");
    (rt, dir)
}

fn cold_compile(c: &mut Criterion) {
    let (rt, _dir) = runtime();
    c.bench_function("cold_compile", |b| {
        b.iter(|| {
            let component =
                Component::new(rt.engine(), black_box(TEST_GUEST_WASM)).expect("compile");
            black_box(component);
        });
    });
}

fn cold_deserialize(c: &mut Criterion) {
    let (rt, _dir) = runtime();
    let component = Component::new(rt.engine(), TEST_GUEST_WASM).expect("compile");
    let serialized = component.serialize().expect("serialize");

    c.bench_function("cold_deserialize", |b| {
        b.iter(|| {
            // SAFETY: `serialized` was produced by `Component::serialize` on
            // this exact engine, immediately above — same process, same
            // build, same config.
            let component = unsafe { Component::deserialize(rt.engine(), black_box(&serialized)) }
                .expect("deserialize");
            black_box(component);
        });
    });
}

fn warm_invoke_echo(c: &mut Criterion) {
    let (rt, _dir) = runtime();
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    tokio_rt
        .block_on(rt.publish(
            "bench-tenant",
            "bench-fn",
            Bytes::from_static(TEST_GUEST_WASM),
        ))
        .expect("publish fixture");
    let limits = Limits::new(WARM_BUDGET_MS, 64 * 1024 * 1024).expect("limits");

    c.bench_function("warm_invoke_echo", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                let out = rt
                    .invoke(
                        "bench-tenant",
                        "bench-fn",
                        black_box(b"warm-invoke-echo-payload".to_vec()),
                        &limits,
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
