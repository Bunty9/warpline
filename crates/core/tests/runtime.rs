//! Integration tests for [`warpline_core::Runtime`] against the
//! `examples/test-guest` fixture (`tests/fixtures/test_guest.wasm`, built
//! by `scripts/build-guests.sh`).
//!
//! Each test builds its own `Runtime` (own engine, own epoch ticker, own
//! tempdir registry) so tests run concurrently without interfering.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use warpline_core::{
    Bytes, InvokeError, KvError, KvStore, Limits, MeterSink, PublishError, Runtime, RuntimeConfig,
    Usage,
};

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

const DEFAULT_MEM_CAP: usize = 64 * 1024 * 1024;
const FN: &str = "test-fn";

fn limits(cpu_budget_ms: u64) -> Limits {
    Limits::new(cpu_budget_ms, DEFAULT_MEM_CAP).unwrap()
}

fn config(dir: &Path) -> RuntimeConfig {
    let mut cfg = RuntimeConfig::new(dir);
    cfg.allow_private_egress = true;
    cfg
}

/// A runtime over a fresh tempdir with the fixture published as `FN` for
/// each of `tenants`.
async fn setup_with(
    tenants: &[&str],
    tweak: impl FnOnce(&mut RuntimeConfig),
) -> (Runtime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    tweak(&mut cfg);
    let rt = Runtime::new(cfg).expect("build runtime");
    for t in tenants {
        rt.publish(t, FN, Bytes::from_static(TEST_GUEST_WASM))
            .await
            .expect("publish fixture");
    }
    (rt, dir)
}

async fn setup() -> (Runtime, tempfile::TempDir) {
    setup_with(&["tenant-a", "tenant-b"], |_| {}).await
}

/// Records every `record` call.
#[derive(Default)]
struct RecordingMeter(Mutex<Vec<(String, String, Usage, bool)>>);

impl MeterSink for RecordingMeter {
    fn record(&self, tenant: &str, func: &str, usage: Usage, ok: bool) {
        self.0
            .lock()
            .unwrap()
            .push((tenant.into(), func.into(), usage, ok));
    }
}

/// Spawn a bare-bones HTTP/1.1 responder on `127.0.0.1:0` that replies with
/// a fixed `response` to every connection. Good enough for exercising
/// `http-out::fetch`'s status/redirect/body-cap handling without pulling
/// axum into `warpline-core`'s dependency graph.
async fn spawn_raw_http_server(response: Vec<u8>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let response = response.clone();
            tokio::spawn(async move {
                // Best-effort drain of the request; every test here sends
                // a bare GET with no body, which fits in one read.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

fn http_response(status_line: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
    let mut resp = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

#[tokio::test]
async fn echo_roundtrip() {
    let (rt, _dir) = setup().await;
    let out = rt
        .invoke("tenant-a", FN, b"hello there".to_vec(), &limits(5_000))
        .await
        .expect("invoke ok");
    assert_eq!(out.output, b"hello there");
}

#[tokio::test]
async fn kv_is_scoped_per_tenant() {
    let (rt, _dir) = setup().await;
    let l = limits(5_000);

    let out = rt
        .invoke("tenant-a", FN, b"kv:put:foo:bar".to_vec(), &l)
        .await
        .expect("put ok");
    assert_eq!(out.output, b"ok");

    let out = rt
        .invoke("tenant-a", FN, b"kv:get:foo".to_vec(), &l)
        .await
        .expect("get ok");
    assert_eq!(out.output, b"bar");

    let out = rt
        .invoke("tenant-b", FN, b"kv:get:foo".to_vec(), &l)
        .await
        .expect("get ok");
    assert_eq!(out.output, b"none");
}

#[tokio::test]
async fn log_emit_returns_ok() {
    let (rt, _dir) = setup().await;
    let out = rt
        .invoke(
            "tenant-a",
            FN,
            b"log:hello from a test".to_vec(),
            &limits(5_000),
        )
        .await
        .expect("invoke ok");
    assert_eq!(out.output, b"ok");
}

#[tokio::test]
async fn cpu_budget_traps_infinite_loop() {
    let (rt, _dir) = setup().await;
    let started = Instant::now();
    let err = rt
        .invoke("tenant-a", FN, b"loop".to_vec(), &limits(50))
        .await
        .expect_err("infinite loop should trap on cpu budget");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, InvokeError::CpuBudgetExceeded { budget_ms: 50, .. }),
        "expected CpuBudgetExceeded, got {err:?}"
    );
    assert_eq!(err.http_status(), 408);
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

/// The deadline callback yields to the executor on every tick instead of
/// pinning the worker thread: on a `current_thread` runtime, two concurrent
/// `loop` invocations run alongside a task that only needs a 10 ms sleep.
#[tokio::test(flavor = "current_thread")]
async fn cooperative_yield_lets_other_tasks_run_during_loop() {
    let (rt, _dir) = setup().await;
    let loop_budget_ms = 300;
    let start = Instant::now();
    let sleep_elapsed: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let sleep_elapsed2 = sleep_elapsed.clone();

    let l = limits(loop_budget_ms);
    let loop1 = rt.invoke("tenant-a", FN, b"loop".to_vec(), &l);
    let loop2 = rt.invoke("tenant-b", FN, b"loop".to_vec(), &l);
    let sleeper = async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        *sleep_elapsed2.lock().unwrap() = Some(start.elapsed());
    };

    let (r1, r2, _) = tokio::join!(loop1, loop2, sleeper);
    assert!(
        matches!(r1, Err(InvokeError::CpuBudgetExceeded { .. })),
        "got {r1:?}"
    );
    assert!(
        matches!(r2, Err(InvokeError::CpuBudgetExceeded { .. })),
        "got {r2:?}"
    );

    let sleep_at = sleep_elapsed.lock().unwrap().expect("sleeper never ran");
    assert!(
        sleep_at < Duration::from_millis(loop_budget_ms / 2),
        "sleeper only completed after {sleep_at:?} — loops appear to have \
         starved the executor instead of yielding"
    );
}

/// A guest that writes KV entries in a tight loop traps on the
/// per-invocation cap long before it could grow the store without bound.
#[tokio::test]
async fn kv_fill_traps_on_invocation_quota() {
    let (rt, _dir) = setup().await;
    let err = rt
        .invoke("tenant-a", FN, b"kv:fill".to_vec(), &limits(5_000))
        .await
        .expect_err("unbounded kv writer should trap");
    assert!(matches!(err, InvokeError::GuestTrap { .. }), "got {err:?}");
    assert_eq!(err.http_status(), 500);
}

struct FailingKv;

#[async_trait::async_trait]
impl KvStore for FailingKv {
    async fn get(&self, _tenant: &str, _key: &str) -> Result<Option<Vec<u8>>, KvError> {
        Err(KvError::Backend("connection refused".into()))
    }
    async fn put(&self, _tenant: &str, _key: &str, _value: Vec<u8>) -> Result<(), KvError> {
        Ok(())
    }
}

#[tokio::test]
async fn kv_backend_error_traps_the_guest() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Runtime::builder(config(dir.path()))
        .kv(Arc::new(FailingKv))
        .build()
        .unwrap();
    rt.publish("tenant-a", FN, Bytes::from_static(TEST_GUEST_WASM))
        .await
        .unwrap();
    let err = rt
        .invoke("tenant-a", FN, b"kv:get:x".to_vec(), &limits(5_000))
        .await
        .expect_err("backend failure must surface");
    assert!(matches!(err, InvokeError::GuestTrap { .. }), "got {err:?}");
}

#[tokio::test]
async fn http_out_blocks_private_address_by_default() {
    // `allow_private_egress = false`: 127.0.0.1 must be refused even though
    // it is in the tenant's allowlist, both by the client's resolver and by
    // the IP-literal check.
    let (rt, _dir) = setup_with(&["tenant-a"], |c| c.allow_private_egress = false).await;
    let l = limits(5_000)
        .with_allowed_hosts(&["127.0.0.1".to_string()])
        .unwrap();

    let addr = spawn_raw_http_server(http_response("200 OK", b"hi", "")).await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = rt
        .invoke("tenant-a", FN, input, &l)
        .await
        .expect("invoke ok — blocked address is a guest-visible error, not a trap");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("err:"), "got {text}");
    assert!(
        text.contains("blocked") || text.contains("private"),
        "got {text}"
    );
}

#[tokio::test]
async fn memory_cap_traps_runaway_allocator() {
    let (rt, _dir) = setup().await;
    let cap = 16 * 1024 * 1024;
    let l = Limits::new(5_000, cap).unwrap();

    let err = rt
        .invoke("tenant-a", FN, b"alloc".to_vec(), &l)
        .await
        .expect_err("runaway allocator should trap on memory cap");

    match &err {
        InvokeError::MemoryCapExceeded { usage, cap_bytes } => {
            assert_eq!(*cap_bytes, cap);
            assert!(
                usage.mem_peak_bytes <= cap,
                "peak {} > cap {cap}",
                usage.mem_peak_bytes
            );
        }
        other => panic!("expected MemoryCapExceeded, got {other:?}"),
    }
    assert_eq!(err.http_status(), 507);
}

#[tokio::test]
async fn http_out_disallowed_host_is_rejected() {
    let (rt, _dir) = setup().await;
    let out = rt
        .invoke(
            "tenant-a",
            FN,
            b"fetch:http://example.invalid/".to_vec(),
            &limits(5_000),
        )
        .await
        .expect("invoke ok — disallowed host is a guest-visible error, not a trap");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("err:"), "got {text}");
    assert!(text.contains("not allowed"), "got {text}");
}

fn loopback_limits() -> Limits {
    limits(5_000)
        .with_allowed_hosts(&["127.0.0.1".to_string()])
        .unwrap()
}

#[tokio::test]
async fn http_out_allowed_host_returns_status_and_body() {
    let (rt, _dir) = setup().await;
    let addr = spawn_raw_http_server(http_response("200 OK", b"hi", "")).await;
    let out = rt
        .invoke(
            "tenant-a",
            FN,
            format!("fetch:http://{addr}/").into_bytes(),
            &loopback_limits(),
        )
        .await
        .expect("invoke ok");
    assert_eq!(out.output, b"status:200:hi");
}

#[tokio::test]
async fn http_out_does_not_follow_redirects() {
    let (rt, _dir) = setup().await;
    let addr = spawn_raw_http_server(http_response(
        "302 Found",
        b"",
        "Location: http://127.0.0.1:1/elsewhere\r\n",
    ))
    .await;
    let out = rt
        .invoke(
            "tenant-a",
            FN,
            format!("fetch:http://{addr}/").into_bytes(),
            &loopback_limits(),
        )
        .await
        .expect("invoke ok");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("status:302:"), "got {text}");
}

#[tokio::test]
async fn http_out_body_over_cap_is_rejected() {
    let (rt, _dir) = setup().await;
    let big_body = vec![b'x'; 1024 * 1024 + 4096];
    let addr = spawn_raw_http_server(http_response("200 OK", &big_body, "")).await;
    let out = rt
        .invoke(
            "tenant-a",
            FN,
            format!("fetch:http://{addr}/").into_bytes(),
            &loopback_limits(),
        )
        .await
        .expect("invoke ok — oversized body is a guest-visible error, not a trap");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("err:"), "got {text}");
}

#[tokio::test]
async fn guest_panic_traps() {
    let (rt, _dir) = setup().await;
    let err = rt
        .invoke("tenant-a", FN, b"panic".to_vec(), &limits(5_000))
        .await
        .expect_err("guest panic should trap");
    assert!(matches!(err, InvokeError::GuestTrap { .. }), "got {err:?}");
}

// ---- admission control, output cap, budget floor, ticker lifetime --------

/// A burst of concurrent invokes against a memory budget that admits only
/// two of them: the rest fail fast with `Overloaded`, never queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_of_concurrent_invokes_returns_overloaded() {
    let tenants: Vec<String> = (0..6).map(|i| format!("burst-{i}")).collect();
    let names: Vec<&str> = tenants.iter().map(String::as_str).collect();
    // Budget fits two 64 MiB invocations.
    let (rt, _dir) = setup_with(&names, |c| c.memory_budget_bytes = 128 * 1024 * 1024).await;

    let l = limits(150);
    let tasks: Vec<_> = tenants
        .iter()
        .map(|t| {
            let (rt, t, l) = (rt.clone(), t.clone(), l.clone());
            tokio::spawn(async move { rt.invoke(&t, FN, b"loop".to_vec(), &l).await })
        })
        .collect();
    let mut overloaded = 0;
    let mut ran = 0;
    for task in tasks {
        match task.await.unwrap() {
            Err(InvokeError::Overloaded) => overloaded += 1,
            Err(InvokeError::CpuBudgetExceeded { .. }) => ran += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(ran + overloaded, 6);
    assert!(overloaded >= 1, "no invocation was shed");
    assert!(ran >= 2, "budget should have admitted two, admitted {ran}");

    // Permits came back: the same runtime admits again.
    rt.invoke("burst-0", FN, b"x".to_vec(), &l)
        .await
        .expect("admitted again");
}

#[tokio::test]
async fn request_heavier_than_whole_budget_is_overloaded_immediately() {
    let (rt, _dir) = setup_with(&["tenant-a"], |c| c.memory_budget_bytes = 32 * 1024 * 1024).await;
    let started = Instant::now();
    let err = rt
        .invoke("tenant-a", FN, b"x".to_vec(), &limits(5_000)) // 64 MiB > 32 MiB
        .await
        .expect_err("cannot ever fit");
    assert!(matches!(err, InvokeError::Overloaded), "got {err:?}");
    assert_eq!(err.http_status(), 503);
    assert!(started.elapsed() < Duration::from_millis(500));
}

/// The per-tenant in-flight cap: with two loops running for a tenant, a
/// third is refused; once they finish (error paths included) the slots are
/// free again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_tenant_in_flight_cap_returns_tenant_busy() {
    let (rt, _dir) = setup_with(&["tenant-a", "tenant-b"], |c| {
        c.max_in_flight_per_tenant = 2
    })
    .await;
    let l = limits(300);
    let running: Vec<_> = (0..2)
        .map(|_| {
            let (rt, l) = (rt.clone(), l.clone());
            tokio::spawn(async move { rt.invoke("tenant-a", FN, b"loop".to_vec(), &l).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let err = rt
        .invoke("tenant-a", FN, b"x".to_vec(), &l)
        .await
        .expect_err("third in flight");
    assert!(matches!(err, InvokeError::TenantBusy), "got {err:?}");
    assert_eq!(err.http_status(), 429);
    // Another tenant is unaffected.
    rt.invoke("tenant-b", FN, b"x".to_vec(), &l)
        .await
        .expect("other tenant ok");

    for r in running {
        assert!(matches!(
            r.await.unwrap(),
            Err(InvokeError::CpuBudgetExceeded { .. })
        ));
    }
    rt.invoke("tenant-a", FN, b"x".to_vec(), &l)
        .await
        .expect("slots released");
}

#[tokio::test]
async fn oversized_output_is_rejected() {
    let (rt, _dir) = setup_with(&["tenant-a"], |c| c.max_output_bytes = 16).await;
    let ok = rt
        .invoke("tenant-a", FN, vec![b'x'; 16], &limits(5_000))
        .await
        .expect("exactly at the limit");
    assert_eq!(ok.output.len(), 16);

    let err = rt
        .invoke("tenant-a", FN, vec![b'x'; 17], &limits(5_000))
        .await
        .expect_err("one byte over");
    assert!(
        matches!(err, InvokeError::OutputTooLarge { limit: 16, .. }),
        "got {err:?}"
    );
    assert_eq!(err.http_status(), 502);
    assert!(err.usage().is_some());
}

/// The ticker belongs to the runtime's shared state, not to any one handle:
/// dropping the caller's handle while an invoke is in flight must not stop
/// the epoch, or the loop would never trap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticker_outlives_dropped_handle_while_invoke_is_in_flight() {
    let (rt, _dir) = setup().await;
    let task = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.invoke("tenant-a", FN, b"loop".to_vec(), &limits(100))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(rt);
    let res = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("loop must still be interrupted")
        .unwrap();
    assert!(
        matches!(res, Err(InvokeError::CpuBudgetExceeded { .. })),
        "got {res:?}"
    );
}

/// `cpu_us` counts ticks the guest actually ran, so a neighbour hogging the
/// executor does not inflate it: on a single thread the victim shares the
/// CPU with another tenant's loop and still reports about its own budget.
#[tokio::test(flavor = "current_thread")]
async fn cpu_us_stays_near_budget_under_neighbour_load() {
    let (rt, _dir) = setup().await;
    let budget_ms = 50;
    let (lv, ln) = (limits(budget_ms), limits(400));
    let victim = rt.invoke("tenant-a", FN, b"loop".to_vec(), &lv);
    let neighbour = rt.invoke("tenant-b", FN, b"loop".to_vec(), &ln);
    let (v, n) = tokio::join!(victim, neighbour);

    let usage = v
        .expect_err("victim loop must hit its budget")
        .usage()
        .unwrap();
    assert!(
        usage.cpu_us < 2 * budget_ms * 1000,
        "cpu_us {} should stay under 2x the {budget_ms} ms budget",
        usage.cpu_us
    );
    assert!(usage.cpu_us >= budget_ms * 1000, "cpu_us {}", usage.cpu_us);
    assert!(n.is_err());
}

// ---- metering -------------------------------------------------------------

#[tokio::test]
async fn meter_is_called_once_per_invoke_that_reached_the_guest() {
    let dir = tempfile::tempdir().unwrap();
    let meter = Arc::new(RecordingMeter::default());
    let rt = Runtime::builder(config(dir.path()))
        .meter(meter.clone())
        .build()
        .unwrap();
    rt.publish("tenant-a", FN, Bytes::from_static(TEST_GUEST_WASM))
        .await
        .unwrap();
    let l = limits(5_000);

    rt.invoke("tenant-a", FN, b"ok".to_vec(), &l).await.unwrap();
    rt.invoke("tenant-a", FN, b"panic".to_vec(), &l)
        .await
        .unwrap_err();
    // Rejected before the guest ran: not metered.
    assert!(matches!(
        rt.invoke("Bad", FN, vec![], &l).await,
        Err(InvokeError::InvalidName)
    ));
    assert!(matches!(
        rt.invoke("tenant-a", "nope", vec![], &l).await,
        Err(InvokeError::NotFound)
    ));

    let rows = meter.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3),
        ("tenant-a", FN, true)
    );
    assert!(!rows[1].3);
}

#[tokio::test]
async fn meter_skips_overloaded_and_tenant_busy() {
    let dir = tempfile::tempdir().unwrap();
    let meter = Arc::new(RecordingMeter::default());
    let mut cfg = config(dir.path());
    cfg.memory_budget_bytes = 32 * 1024 * 1024;
    let rt = Runtime::builder(cfg).meter(meter.clone()).build().unwrap();
    assert!(matches!(
        rt.invoke("tenant-a", FN, vec![], &limits(100)).await, // 64 MiB > 32 MiB
        Err(InvokeError::Overloaded)
    ));
    assert!(meter.0.lock().unwrap().is_empty());
}

/// Dropping an in-flight invoke must still meter it (as a failure) and free
/// its slot, so cancelling cannot make CPU use free or leak capacity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_invoke_is_metered_and_releases_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let meter = Arc::new(RecordingMeter::default());
    let mut cfg = config(dir.path());
    cfg.max_in_flight_per_tenant = 1;
    cfg.memory_budget_bytes = 64 * 1024 * 1024;
    let rt = Runtime::builder(cfg).meter(meter.clone()).build().unwrap();
    rt.publish("tenant-a", FN, Bytes::from_static(TEST_GUEST_WASM))
        .await
        .unwrap();

    let l = limits(5_000);
    let res = tokio::time::timeout(
        Duration::from_millis(60),
        rt.invoke("tenant-a", FN, b"loop".to_vec(), &l),
    )
    .await;
    assert!(res.is_err(), "loop should still be running");

    let rows = meter.0.lock().unwrap().clone();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(!rows[0].3);
    assert!(rows[0].2.cpu_us > 0, "ticks so far should be recorded");

    // Tenant slot and memory permits are free again.
    rt.invoke("tenant-a", FN, b"x".to_vec(), &l).await.unwrap();
}

// ---- publishing, registry -------------------------------------------------

#[tokio::test]
async fn stage_does_not_expose_until_activate() {
    let (rt, _dir) = setup_with(&[], |_| {}).await;
    let staged = rt.stage(Bytes::from_static(TEST_GUEST_WASM)).await.unwrap();
    assert_eq!(staged.digest, warpline_core::digest(TEST_GUEST_WASM));
    assert!(matches!(
        rt.invoke("tenant-a", FN, vec![], &limits(100)).await,
        Err(InvokeError::NotFound)
    ));
    rt.activate("tenant-a", FN, &staged).await.unwrap();
    rt.invoke("tenant-a", FN, b"x".to_vec(), &limits(100))
        .await
        .unwrap();
}

#[tokio::test]
async fn publish_rejects_bad_input() {
    let (rt, _dir) = setup_with(&[], |_| {}).await;
    let wasm = Bytes::from_static(TEST_GUEST_WASM);
    assert!(matches!(
        rt.publish("Bad Name", FN, wasm.clone()).await,
        Err(PublishError::InvalidName)
    ));
    assert!(matches!(
        rt.publish("tenant-a", FN, Bytes::from_static(b"not a wasm component"))
            .await,
        Err(PublishError::Compile(_))
    ));
    // A valid but empty component: compiles, lacks the `handle` export.
    let empty_component = Bytes::from_static(b"\0asm\x0d\0\x01\0");
    assert!(matches!(
        rt.publish("tenant-a", FN, empty_component).await,
        Err(PublishError::ImportMismatch(_))
    ));
}

#[tokio::test]
async fn gc_errors_on_unreadable_pointer_and_deletes_nothing() {
    let (rt, dir) = setup_with(&["tenant-a"], |_| {}).await;
    // An orphan blob GC would normally collect...
    let orphan = dir
        .path()
        .join("wasm")
        .join(format!("{}.wasm", "b".repeat(64)));
    std::fs::write(&orphan, b"y").unwrap();
    // ...plus a pointer slot that cannot be read as a file.
    std::fs::create_dir_all(dir.path().join("tenants/tenant-a/broken")).unwrap();

    let err = rt.gc(Duration::ZERO).await;
    assert!(err.is_err(), "expected an error, got {err:?}");
    assert!(orphan.exists(), "nothing may be deleted");

    std::fs::remove_dir(dir.path().join("tenants/tenant-a/broken")).unwrap();
    assert_eq!(rt.gc(Duration::ZERO).await.unwrap(), 1);
    assert!(!orphan.exists());
}

/// Failing to re-publish the recompiled `.cwasm` (read-only cache dir) is
/// logged, and the invoke still succeeds.
#[cfg(unix)]
#[tokio::test]
async fn recompile_works_with_read_only_cwasm_dir() {
    use std::os::unix::fs::PermissionsExt;

    let (rt, dir) = setup_with(&["tenant-a"], |_| {}).await;
    drop(rt);
    let cwasm_dir = dir.path().join("cwasm");
    for entry in std::fs::read_dir(&cwasm_dir).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    std::fs::set_permissions(&cwasm_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Fresh runtime: empty in-memory cache, so the invoke must recompile.
    let rt = Runtime::new(config(dir.path())).unwrap();
    let res = rt
        .invoke("tenant-a", FN, b"still works".to_vec(), &limits(5_000))
        .await;

    std::fs::set_permissions(&cwasm_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        res.expect("invoke must survive a read-only cwasm dir")
            .output,
        b"still works"
    );
}
