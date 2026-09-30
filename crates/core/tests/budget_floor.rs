//! Timing-sensitive on purpose, so it lives in its own test binary: cargo runs
//! test binaries one after another, which keeps other tests' spinning guests
//! from starving the epoch ticker thread while this one measures.

use warpline_core::{Bytes, Limits, Runtime, RuntimeConfig};

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

/// 1 ms is the smallest budget a tenant can be given. The deadline allows one
/// extra tick and instantiation has its own grace, so an echo must fit it
/// every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_ms_budget_echo_succeeds_100_times() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::new(RuntimeConfig::new(dir.path())).unwrap();
    rt.publish("tenant-a", "echo", Bytes::from_static(TEST_GUEST_WASM))
        .await
        .unwrap();
    // First use pays one-off costs (loading the component, cold code and
    // page faults) that the budget is not meant to cover.
    let generous = Limits::new(5_000, 64 << 20).unwrap();
    rt.invoke("tenant-a", "echo", b"warm".to_vec(), &generous)
        .await
        .unwrap();

    let one_ms = Limits::new(1, 64 << 20).unwrap();
    for i in 0..100 {
        let out = rt
            .invoke("tenant-a", "echo", b"hi".to_vec(), &one_ms)
            .await
            .unwrap_or_else(|e| panic!("echo {i} failed under a 1 ms budget: {e:?}"));
        assert_eq!(out.output, b"hi");
    }
}
