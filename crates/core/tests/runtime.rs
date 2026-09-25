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
use wasmtime::component::{Component, Linker};
use wasmtime::Engine;

use warpline_core::cache;
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

async fn setup() -> (Engine, Linker<HostCtx>, EpochTicker, Component) {
    let engine = build_engine().expect("build engine");
    let linker = build_linker(&engine).expect("build linker");
    let ticker = EpochTicker::spawn(engine.clone());
    let component = Component::new(&engine, TEST_GUEST_WASM).expect("compile test-guest fixture");
    (engine, linker, ticker, component)
}

fn make_ctx(
    tenant: &str,
    kv: Arc<dyn KvStore>,
    allowed_hosts: Vec<String>,
    mem_cap_bytes: usize,
) -> HostCtx {
    HostCtx::new(
        tenant.to_string(),
        "test-fn".to_string(),
        kv,
        allowed_hosts,
        build_http_client().expect("build http client"),
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
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP);

    let outcome = invoke(
        &engine,
        &linker,
        &component,
        ctx,
        b"hello there".to_vec(),
        5_000,
    )
    .await
    .expect("invoke ok");
    assert_eq!(outcome.output, b"hello there");
}

#[tokio::test]
async fn kv_is_scoped_per_tenant() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());

    let put_ctx = make_ctx("tenant-a", kv.clone(), Vec::new(), DEFAULT_MEM_CAP);
    let out = invoke(
        &engine,
        &linker,
        &component,
        put_ctx,
        b"kv:put:foo:bar".to_vec(),
        5_000,
    )
    .await
    .expect("put ok");
    assert_eq!(out.output, b"ok");

    let get_same_ctx = make_ctx("tenant-a", kv.clone(), Vec::new(), DEFAULT_MEM_CAP);
    let out = invoke(
        &engine,
        &linker,
        &component,
        get_same_ctx,
        b"kv:get:foo".to_vec(),
        5_000,
    )
    .await
    .expect("get ok");
    assert_eq!(out.output, b"bar");

    let get_other_ctx = make_ctx("tenant-b", kv, Vec::new(), DEFAULT_MEM_CAP);
    let out = invoke(
        &engine,
        &linker,
        &component,
        get_other_ctx,
        b"kv:get:foo".to_vec(),
        5_000,
    )
    .await
    .expect("get ok");
    assert_eq!(out.output, b"none");
}

#[tokio::test]
async fn log_emit_returns_ok() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP);

    let out = invoke(
        &engine,
        &linker,
        &component,
        ctx,
        b"log:hello from a test".to_vec(),
        5_000,
    )
    .await
    .expect("invoke ok");
    assert_eq!(out.output, b"ok");
}

#[tokio::test]
async fn cpu_budget_traps_infinite_loop() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP);

    let started = Instant::now();
    let err = invoke(&engine, &linker, &component, ctx, b"loop".to_vec(), 50)
        .await
        .expect_err("infinite loop should trap on cpu budget");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, InvokeError::CpuBudgetExceeded { .. }),
        "expected CpuBudgetExceeded, got {err:?}"
    );
    assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
}

#[tokio::test]
async fn memory_cap_traps_runaway_allocator() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let cap = 16 * 1024 * 1024;
    let ctx = make_ctx("tenant-a", kv, Vec::new(), cap);

    let err = invoke(&engine, &linker, &component, ctx, b"alloc".to_vec(), 5_000)
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
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP);

    let out = invoke(
        &engine,
        &linker,
        &component,
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
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
    );

    let addr = spawn_raw_http_server(http_response("200 OK", b"hi", "")).await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &linker, &component, ctx, input, 5_000)
        .await
        .expect("invoke ok");
    assert_eq!(out.output, b"status:200:hi");
}

#[tokio::test]
async fn http_out_does_not_follow_redirects() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
    );

    let addr = spawn_raw_http_server(http_response(
        "302 Found",
        b"",
        "Location: http://127.0.0.1:1/elsewhere\r\n",
    ))
    .await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &linker, &component, ctx, input, 5_000)
        .await
        .expect("invoke ok");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("status:302:"), "got {text}");
}

#[tokio::test]
async fn http_out_body_over_cap_is_rejected() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx(
        "tenant-a",
        kv,
        vec!["127.0.0.1".to_string()],
        DEFAULT_MEM_CAP,
    );

    let big_body = vec![b'x'; 1024 * 1024 + 4096];
    let addr = spawn_raw_http_server(http_response("200 OK", &big_body, "")).await;
    let input = format!("fetch:http://{addr}/").into_bytes();

    let out = invoke(&engine, &linker, &component, ctx, input, 5_000)
        .await
        .expect("invoke ok — oversized body is a guest-visible error, not a trap");
    let text = String::from_utf8(out.output).unwrap();
    assert!(text.starts_with("err:"), "got {text}");
}

#[tokio::test]
async fn guest_panic_traps() {
    let (engine, linker, _ticker, component) = setup().await;
    let kv: Arc<dyn KvStore> = Arc::new(MemKv::new());
    let ctx = make_ctx("tenant-a", kv, Vec::new(), DEFAULT_MEM_CAP);

    let err = invoke(&engine, &linker, &component, ctx, b"panic".to_vec(), 5_000)
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
    let cwasm_path = dir.path().join(format!("{digest}.cwasm"));
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
