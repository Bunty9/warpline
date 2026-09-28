//! Integration tests for `warpline_core::runtime` against the
//! `examples/test-guest` fixture (`tests/fixtures/test_guest.wasm`, built
//! by `scripts/build-guests.sh`).
//!
//! Each test builds its own [`wasmtime::Engine`] + [`EpochTicker`] so tests
//! run concurrently without one test's epoch bumps interrupting another's
//! store.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wasmtime::component::Component;
use wasmtime::Engine;

use warpline_core::cache;
use warpline_core::kv::{KvStore, MemKv};
use warpline_core::runtime::{
    build_engine, build_http_client, build_linker, instantiate_pre, invoke, EpochTicker,
    HandlerPre, InvokeError,
};
use warpline_core::types::HostCtx;

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test_guest.wasm"
));

const DEFAULT_MEM_CAP: usize = 64 * 1024 * 1024;

/// `pre` (a [`HandlerPre`], not a bare `Component` + `Linker`) is what
/// [`invoke`] now takes — see finding 11: it's built once here, the same
/// way `ComponentCache::get_or_load` builds it once per digest instead of
/// on every invoke.
async fn setup() -> (Engine, HandlerPre<HostCtx>, EpochTicker) {
    let engine = build_engine().expect("build engine");
    let linker = build_linker(&engine).expect("build linker");
    let ticker = EpochTicker::spawn(engine.clone());
    let component = Component::new(&engine, TEST_GUEST_WASM).expect("compile test-guest fixture");
    let pre = instantiate_pre(&linker, &component).expect("instantiate_pre");
    (engine, pre, ticker)
}

fn make_ctx(
    tenant: &str,
    kv: Arc<dyn KvStore>,
    allowed_hosts: Vec<String>,
    mem_cap_bytes: usize,
    allow_private: bool,
) -> HostCtx {
    HostCtx::new(
        tenant.to_string(),
        "test-fn".to_string(),
        kv,
        allowed_hosts,
        allow_private,
        build_http_client(allow_private).expect("build http client"),
        mem_cap_bytes,
    )
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
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let outcome = invoke(&engine, &pre, ctx, b"hello there".to_vec(), 5_000)
        .await
        .expect("invoke ok");
    assert_eq!(outcome.output, b"hello there");
}

#[tokio::test]
async fn kv_is_scoped_per_tenant() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());

    let put_ctx = make_ctx("tenant-a", kv.clone(), Vec::new(), DEFAULT_MEM_CAP, true);
    let out = invoke(&engine, &pre, put_ctx, b"kv:put:foo:bar".to_vec(), 5_000)
        .await
        .expect("put ok");
    assert_eq!(out.output, b"ok");

    let get_same_ctx = make_ctx("tenant-a", kv.clone(), Vec::new(), DEFAULT_MEM_CAP, true);
    let out = invoke(&engine, &pre, get_same_ctx, b"kv:get:foo".to_vec(), 5_000)
        .await
        .expect("get ok");
    assert_eq!(out.output, b"bar");

    let get_other_ctx = make_ctx("tenant-b", kv, Vec::new(), DEFAULT_MEM_CAP, true);
    let out = invoke(&engine, &pre, get_other_ctx, b"kv:get:foo".to_vec(), 5_000)
        .await
        .expect("get ok");
    assert_eq!(out.output, b"none");
}

#[tokio::test]
async fn log_emit_returns_ok() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let out = invoke(&engine, &pre, ctx, b"log:hello from a test".to_vec(), 5_000)
        .await
        .expect("invoke ok");
    assert_eq!(out.output, b"ok");
}

#[tokio::test]
async fn cpu_budget_traps_infinite_loop() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let started = Instant::now();
    let err = invoke(&engine, &pre, ctx, b"loop".to_vec(), 50)
        .await
        .expect_err("infinite loop should trap on cpu budget");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, InvokeError::CpuBudgetExceeded { .. }),
        "expected CpuBudgetExceeded, got {err:?}"
    );
    // Cooperative yielding (each tick hands control back to the executor
    // before re-polling) adds scheduling overhead relative to a hard
    // interrupt, so this bound is looser than a tight budget check would
    // otherwise need.
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

/// Proves the epoch-deadline callback actually yields to the async executor
/// on every tick instead of blocking the worker thread until the CPU budget
/// is exhausted: on a `current_thread` runtime, two concurrent `loop`
/// invocations run alongside a third task that only needs a 10 ms sleep to
/// complete. If `invoke` didn't yield, the sleeper would be starved until
/// both loops trapped (hundreds of ms away); instead it completes almost
/// immediately.
#[tokio::test(flavor = "current_thread")]
async fn cooperative_yield_lets_other_tasks_run_during_loop() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx1 = make_ctx("tenant-a", kv.clone(), Vec::new(), DEFAULT_MEM_CAP, true);
    let ctx2 = make_ctx("tenant-b", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let loop_budget_ms = 300;
    let start = Instant::now();
    let sleep_elapsed: Arc<std::sync::Mutex<Option<Duration>>> =
        Arc::new(std::sync::Mutex::new(None));
    let sleep_elapsed2 = sleep_elapsed.clone();

    let loop1 = invoke(&engine, &pre, ctx1, b"loop".to_vec(), loop_budget_ms);
    let loop2 = invoke(&engine, &pre, ctx2, b"loop".to_vec(), loop_budget_ms);
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

/// A guest that writes KV entries in a tight, unbounded loop traps on the
/// per-invocation cap (`MAX_KV_PUTS_PER_INVOCATION` / `MAX_KV_PUT_BYTES_PER_INVOCATION`)
/// long before it could grow the in-memory KV store without bound.
#[tokio::test]
async fn kv_fill_traps_on_invocation_quota() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let err = invoke(&engine, &pre, ctx, b"kv:fill".to_vec(), 5_000)
        .await
        .expect_err("unbounded kv writer should trap");
    assert!(matches!(err, InvokeError::GuestTrap(_)), "got {err:?}");
}

#[tokio::test]
async fn http_out_blocks_private_address_by_default() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    // `allow_private = false` this time — 127.0.0.1 must be refused even
    // though it's in the host allowlist, both because the client's own
    // resolver would filter it and because the URL is an IP literal that
    // bypasses the resolver entirely (see `runtime::http_fetch`).
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
        false,
    );

    let addr = spawn_raw_http_server(http_response("200 OK", b"hi", "")).await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &pre, ctx, input, 5_000)
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
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let cap = 16 * 1024 * 1024;
    let ctx = make_ctx("tenant-a", kv, Vec::new(), cap, true);

    let err = invoke(&engine, &pre, ctx, b"alloc".to_vec(), 5_000)
        .await
        .expect_err("runaway allocator should trap on memory cap");

    match err {
        InvokeError::MemoryCapExceeded {
            peak_bytes,
            cap_bytes,
        } => {
            assert_eq!(cap_bytes, cap);
            assert!(
                peak_bytes <= cap_bytes,
                "peak {peak_bytes} > cap {cap_bytes}"
            );
        }
        other => panic!("expected MemoryCapExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn http_out_disallowed_host_is_rejected() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let out = invoke(
        &engine,
        &pre,
        ctx,
        b"fetch:http://example.invalid/".to_vec(),
        5_000,
    )
    .await
    .expect("invoke ok — disallowed host is a guest-visible error, not a trap");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("err:"), "got {text}");
    assert!(text.contains("not allowed"), "got {text}");
}

#[tokio::test]
async fn http_out_allowed_host_returns_status_and_body() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
        true,
    );

    let addr = spawn_raw_http_server(http_response("200 OK", b"hi", "")).await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &pre, ctx, input, 5_000)
        .await
        .expect("invoke ok");
    assert_eq!(out.output, b"status:200:hi");
}

#[tokio::test]
async fn http_out_does_not_follow_redirects() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
        true,
    );

    let addr = spawn_raw_http_server(http_response(
        "302 Found",
        b"",
        "Location: http://127.0.0.1:1/elsewhere\r\n",
    ))
    .await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &pre, ctx, input, 5_000)
        .await
        .expect("invoke ok");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("status:302:"), "got {text}");
}

#[tokio::test]
async fn http_out_body_over_cap_is_rejected() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
        true,
    );

    let big_body = vec![b'x'; 1024 * 1024 + 4096];
    let addr = spawn_raw_http_server(http_response("200 OK", &big_body, "")).await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &pre, ctx, input, 5_000)
        .await
        .expect("invoke ok — oversized body is a guest-visible error, not a trap");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("err:"), "got {text}");
}

#[tokio::test]
async fn guest_panic_traps() {
    let (engine, pre, _ticker) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP, true);

    let err = invoke(&engine, &pre, ctx, b"panic".to_vec(), 5_000)
        .await
        .expect_err("guest panic should trap");
    assert!(matches!(err, InvokeError::GuestTrap(_)), "got {err:?}");
}

#[test]
fn cache_second_load_hits_the_cwasm_warm_path() {
    let engine = build_engine().expect("build engine");
    let dir = tempfile::tempdir().expect("tempdir");

    let _first =
        cache::load_or_compile(&engine, TEST_GUEST_WASM, dir.path()).expect("cold compile");
    let digest = cache::digest(TEST_GUEST_WASM);
    let cwasm_path = dir.path().join(cache::cache_file_name(&engine, &digest));
    assert!(cwasm_path.exists());
    let modified_before = std::fs::metadata(&cwasm_path).unwrap().modified().unwrap();

    let _second = cache::load_or_compile(&engine, TEST_GUEST_WASM, dir.path()).expect("warm load");
    let modified_after = std::fs::metadata(&cwasm_path).unwrap().modified().unwrap();
    assert_eq!(
        modified_before, modified_after,
        "second load_or_compile call should not have rewritten the .cwasm file"
    );

    // And `load_cwasm` on its own, by digest, works too.
    let _third = cache::load_cwasm(&engine, dir.path(), &digest).expect("load_cwasm");
}

#[test]
fn load_or_compile_recovers_from_corrupt_cwasm() {
    let engine = build_engine().expect("build engine");
    let dir = tempfile::tempdir().expect("tempdir");

    let digest = cache::digest(TEST_GUEST_WASM);
    let cwasm_path = dir.path().join(cache::cache_file_name(&engine, &digest));
    std::fs::write(&cwasm_path, b"not a real cwasm file").expect("write garbage");

    // Must not propagate the deserialize failure — it should log a
    // warning, recompile from source, and overwrite the bad entry.
    let _component = cache::load_or_compile(&engine, TEST_GUEST_WASM, dir.path())
        .expect("load_or_compile should recover from a corrupt cache entry");

    // The overwritten file must now be a valid, loadable .cwasm.
    let _reloaded = cache::load_cwasm(&engine, dir.path(), &digest)
        .expect("recompiled cache entry should be loadable");
}

#[test]
fn load_cwasm_rejects_malformed_digest() {
    let engine = build_engine().expect("build engine");
    let dir = tempfile::tempdir().expect("tempdir");

    for bad in [
        "",
        "short",
        "../../etc/passwd",
        "UPPERCASE0000000000000000000000000000000000000000000000000000",
        &"a".repeat(63),
        &"a".repeat(65),
    ] {
        assert!(
            cache::load_cwasm(&engine, dir.path(), bad).is_err(),
            "expected digest {bad:?} to be rejected"
        );
    }
}
