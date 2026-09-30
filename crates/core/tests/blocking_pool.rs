//! A tiny blocking pool must not deadlock when stages and cold-miss invokes
//! compete for the compile permit: permits are taken asynchronously, before
//! a blocking thread is used, on every path. Its own binary because it needs
//! a hand-built tokio runtime.

use std::time::Duration;

use warpline_core::{Bytes, Limits, Runtime, RuntimeConfig};

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

#[test]
fn cold_invokes_and_stages_complete_with_two_blocking_threads() {
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();

    tokio_rt.block_on(async {
        let mut cfg = RuntimeConfig::new(dir.path());
        cfg.compile_concurrency = 1;
        let rt = Runtime::new(cfg.clone()).unwrap();
        for t in ["ta", "tb", "tc", "td"] {
            rt.publish(t, "f", Bytes::from_static(TEST_GUEST_WASM))
                .await
                .unwrap();
        }
        drop(rt);
        // Lose every .cwasm so a fresh runtime must recompile on each cold miss.
        for e in std::fs::read_dir(dir.path().join("cwasm")).unwrap() {
            std::fs::remove_file(e.unwrap().path()).unwrap();
        }

        let rt = Runtime::new(cfg).unwrap();
        let limits = Limits::new(5_000, 16 << 20).unwrap();
        let mut tasks = Vec::new();
        for t in ["ta", "tb", "tc", "td"] {
            let (invoke_rt, limits) = (rt.clone(), limits.clone());
            tasks.push(tokio::spawn(async move {
                invoke_rt
                    .invoke(t, "f", b"x".to_vec(), &limits)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }));
            let stage_rt = rt.clone();
            tasks.push(tokio::spawn(async move {
                stage_rt
                    .stage(Bytes::from_static(TEST_GUEST_WASM))
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }));
        }
        let all = async {
            for t in tasks {
                t.await.unwrap().unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(120), all)
            .await
            .expect("deadlocked: blocking pool exhausted by permit waiters");
    });
}
