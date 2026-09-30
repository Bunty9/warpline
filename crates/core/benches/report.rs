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

use std::time::Instant;

use warpline_core::{Bytes, InvokeError, Limits, Runtime, RuntimeConfig};

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

const TENANT: &str = "bench-tenant";
const FUNC: &str = "bench-fn";

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = Runtime::new(RuntimeConfig::new(dir.path())).expect("build runtime");
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    tokio_rt
        .block_on(rt.publish(TENANT, FUNC, Bytes::from_static(TEST_GUEST_WASM)))
        .expect("publish fixture");
    let warm = Limits::new(WARM_BUDGET_MS, DEFAULT_MEM_CAP).expect("limits");

    let echo = || rt.invoke(TENANT, FUNC, b"warm-invoke-echo-payload".to_vec(), &warm);

    println!("| Metric | Target | Result |");
    println!("|---|---|---|");

    // -- warm_invoke_echo p50/p99 over PERCENTILE_SAMPLES sequential calls --
    let mut samples = Vec::with_capacity(PERCENTILE_SAMPLES);
    tokio_rt.block_on(async {
        for _ in 0..PERCENTILE_SAMPLES {
            let start = Instant::now();
            echo().await.expect("invoke ok");
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
    tokio_rt.block_on(async {
        for _ in 0..THROUGHPUT_SAMPLES {
            echo().await.expect("invoke ok");
        }
    });
    let inv_per_sec = THROUGHPUT_SAMPLES as f64 / tp_start.elapsed().as_secs_f64();
    println!("| throughput (inv/s/core) | > 1000 | {inv_per_sec:.0} |");

    // -- cpu_cap_accuracy: `loop` guest, budgets 10/50/100 ms --
    for budget_ms in [10u64, 50, 100] {
        let limits = Limits::new(budget_ms, DEFAULT_MEM_CAP).expect("limits");
        let start = Instant::now();
        let err = tokio_rt
            .block_on(rt.invoke(TENANT, FUNC, b"loop".to_vec(), &limits))
            .expect_err("infinite loop should trap on cpu budget");
        let elapsed = start.elapsed();
        assert!(
            matches!(err, InvokeError::CpuBudgetExceeded { .. }),
            "expected CpuBudgetExceeded, got {err:?}"
        );
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let err_ms = elapsed_ms - budget_ms as f64;
        let cpu_ms = err.usage().map_or(0, |u| u.cpu_us) as f64 / 1000.0;
        println!(
            "| cpu_cap_accuracy ({budget_ms} ms budget) | budget (+1 ms tick) ± 5 ms | actual {elapsed_ms:.2} ms (error {err_ms:+.2} ms), metered cpu {cpu_ms:.0} ms |"
        );
    }

    // -- mem_cap: `alloc` guest, 16 MiB cap --
    let mem_cap = 16 * 1024 * 1024;
    let limits = Limits::new(5_000, mem_cap).expect("limits");
    let err = tokio_rt
        .block_on(rt.invoke(TENANT, FUNC, b"alloc".to_vec(), &limits))
        .expect_err("runaway allocator should trap on memory cap");
    match err {
        InvokeError::MemoryCapExceeded { usage, cap_bytes } => {
            assert_eq!(cap_bytes, mem_cap);
            let peak_bytes = usage.mem_peak_bytes;
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
