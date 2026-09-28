//! Non-criterion measurements from the Task 3 bench harness (PROGRESS.md
//! "Bench numbers" table): warm-invoke p50/p99 over a larger sequential
//! run than criterion samples, invocations/sec/core throughput, CPU-cap
//! trap-latency accuracy, and the memory-cap hard ceiling. These are
//! measurements of the running system, not micro-benchmarks to be
//! statistically sampled, so they're a plain `fn main` (`harness = false`)
//! printing a markdown table to stdout instead of a criterion group.
//!
//! Run with `cargo bench -p warpline-core --bench report` (release only).
//! Kept under ~60s total: 10k sequential warm invokes plus a handful of
//! single-shot trap/cap checks.

use std::sync::Arc;
use std::time::Instant;

use wasmtime::component::Component;

use warpline_core::kv::{KvStore, MemKv};
use warpline_core::runtime::{
    build_engine, build_http_client, build_linker, invoke, EpochTicker, InvokeError,
};
use warpline_core::types::HostCtx;

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

const DEFAULT_MEM_CAP: usize = 64 * 1024 * 1024;
const WARM_BUDGET_MS: u64 = 10;
/// Sample count for the p50/p99 sweep — "e.g. 10k sequential invokes" per
/// the task spec.
const PERCENTILE_SAMPLES: usize = 10_000;
/// Sample count for the throughput measurement — smaller than the
/// percentile sweep because only the aggregate rate matters here, keeping
/// total runtime down.
const THROUGHPUT_SAMPLES: usize = 5_000;

fn make_ctx(kv: Arc<dyn KvStore>, http_client: reqwest::Client, mem_cap_bytes: usize) -> HostCtx {
    HostCtx::new(
        "bench-tenant".to_string(),
        "bench-fn".to_string(),
        kv,
        Vec::new(),
        true,
        http_client,
        mem_cap_bytes,
    )
}

fn main() {
    let engine = build_engine().expect("build engine");
    let linker = build_linker(&engine).expect("build linker");
    let _ticker = EpochTicker::spawn(engine.clone());
    let component = Component::new(&engine, TEST_GUEST_WASM).expect("compile fixture");
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let http_client = build_http_client(true).expect("build http client");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    println!("| Metric | Target | Result |");
    println!("|---|---|---|");

    // -- warm_invoke_echo p50/p99 over PERCENTILE_SAMPLES sequential calls --
    let mut samples = Vec::with_capacity(PERCENTILE_SAMPLES);
    rt.block_on(async {
        for _ in 0..PERCENTILE_SAMPLES {
            let ctx = make_ctx(kv.clone(), http_client.clone(), DEFAULT_MEM_CAP);
            let start = Instant::now();
            invoke(
                &engine,
                &linker,
                &component,
                ctx,
                b"warm-invoke-echo-payload".to_vec(),
                WARM_BUDGET_MS,
            )
            .await
            .expect("invoke ok");
            samples.push(start.elapsed());
        }
    });
    samples.sort_unstable();
    let p50 = samples[PERCENTILE_SAMPLES * 50 / 100];
    let p99 = samples[PERCENTILE_SAMPLES * 99 / 100];
    println!(
        "| warm_invoke_echo p50 ({PERCENTILE_SAMPLES} samples) | < 5 ms | {:.3} ms |",
        p50.as_secs_f64() * 1000.0
    );
    println!(
        "| warm_invoke_echo p99 ({PERCENTILE_SAMPLES} samples) | < 5 ms | {:.3} ms |",
        p99.as_secs_f64() * 1000.0
    );

    // -- throughput: invocations/sec/core on a current_thread runtime --
    let tp_start = Instant::now();
    rt.block_on(async {
        for _ in 0..THROUGHPUT_SAMPLES {
            let ctx = make_ctx(kv.clone(), http_client.clone(), DEFAULT_MEM_CAP);
            invoke(
                &engine,
                &linker,
                &component,
                ctx,
                b"warm-invoke-echo-payload".to_vec(),
                WARM_BUDGET_MS,
            )
            .await
            .expect("invoke ok");
        }
    });
    let inv_per_sec = THROUGHPUT_SAMPLES as f64 / tp_start.elapsed().as_secs_f64();
    println!("| throughput (inv/s/core) | > 1000 | {inv_per_sec:.0} |");

    // -- cpu_cap_accuracy: `loop` guest, budgets 10/50/100 ms --
    for budget_ms in [10u64, 50, 100] {
        let ctx = make_ctx(kv.clone(), http_client.clone(), DEFAULT_MEM_CAP);
        let start = Instant::now();
        let err = rt
            .block_on(invoke(
                &engine,
                &linker,
                &component,
                ctx,
                b"loop".to_vec(),
                budget_ms,
            ))
            .expect_err("infinite loop should trap on cpu budget");
        let elapsed = start.elapsed();
        assert!(
            matches!(err, InvokeError::CpuBudgetExceeded { .. }),
            "expected CpuBudgetExceeded, got {err:?}"
        );
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let err_ms = elapsed_ms - budget_ms as f64;
        println!(
            "| cpu_cap_accuracy ({budget_ms} ms budget) | budget ± 5 ms | actual {elapsed_ms:.2} ms (error {err_ms:+.2} ms) |"
        );
    }

    // -- mem_cap: `alloc` guest, 16 MiB cap --
    let mem_cap = 16 * 1024 * 1024;
    let ctx = make_ctx(kv.clone(), http_client.clone(), mem_cap);
    let err = rt
        .block_on(invoke(
            &engine,
            &linker,
            &component,
            ctx,
            b"alloc".to_vec(),
            5_000,
        ))
        .expect_err("runaway allocator should trap on memory cap");
    match err {
        InvokeError::MemoryCapExceeded {
            peak_bytes,
            cap_bytes,
        } => {
            assert_eq!(cap_bytes, mem_cap);
            assert!(
                peak_bytes <= cap_bytes,
                "peak {peak_bytes} > cap {cap_bytes}"
            );
            println!(
                "| mem_cap (16 MiB cap) | trap, peak <= cap | trapped, peak {peak_bytes} bytes <= cap {cap_bytes} bytes |"
            );
        }
        other => {
            println!(
                "| mem_cap (16 MiB cap) | trap, peak <= cap | UNEXPECTED OUTCOME: {other:?} |"
            );
        }
    }
}
