//! Dev-mode (no `DATABASE_URL`) end-to-end tests: upload through the real
//! `warpline-control` router into a tempdir modules dir, then invoke
//! through `warpline-host`'s router against that same dir — exercising the
//! registry pointer-file handoff between the two binaries without a real
//! Postgres or a real listening socket.

use std::path::Path;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use warpline_core::{Runtime, RuntimeConfig};

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../core/tests/fixtures/test_guest.wasm"
));

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

fn multipart_body(field_name: &str, content: &[u8]) -> (String, Vec<u8>) {
    let boundary = "warpline-test-boundary-7f3e9a";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"{field_name}\"; filename=\"m.wasm\"\r\n\
             Content-Type: application/wasm\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Control and host each get their own `Runtime` over the same modules dir,
/// like two processes sharing a volume: the pointer files are the handoff.
fn control_router(modules_dir: &Path) -> axum::Router {
    let runtime = Runtime::new(RuntimeConfig::new(modules_dir)).expect("build runtime");
    warpline_control::router(warpline_control::AppState::new(runtime, None))
}

fn host_router(modules_dir: &Path) -> axum::Router {
    host_router_with(modules_dir, |_| {})
}

fn host_router_with(modules_dir: &Path, tweak: impl FnOnce(&mut RuntimeConfig)) -> axum::Router {
    let mut cfg = RuntimeConfig::new(modules_dir);
    tweak(&mut cfg);
    let runtime = Runtime::builder(cfg)
        .meter(Arc::new(warpline_host::LogMeter))
        .build()
        .expect("build runtime");
    warpline_host::router(warpline_host::AppState {
        runtime,
        auth: None,
        metrics_handle: warpline_host::metrics_handle(),
    })
}

async fn upload(
    router: &axum::Router,
    tenant: &str,
    func: &str,
    wasm: &[u8],
) -> (StatusCode, serde_json::Value) {
    let (content_type, body) = multipart_body("wasm", wasm);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant}/functions/{func}"))
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

async fn invoke(
    router: &axum::Router,
    tenant: &str,
    func: &str,
    body: &[u8],
) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant}/functions/{func}/invoke"))
        .body(Body::from(body.to_vec()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

/// Everything in one test (rather than N tests each paying for their own
/// `Engine`/`Component` build) since every scenario shares the same
/// uploaded module and modules dir.
#[tokio::test]
async fn upload_then_invoke_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let control = control_router(dir.path());
    let host = host_router(dir.path());

    // Invalid names -> 400, on both routers. ("Bad-Name" is a valid URI
    // path segment but fails `valid_name`'s lowercase-only rule.)
    let (status, _) = upload(&control, "Bad-Name", "f", TEST_GUEST_WASM).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = invoke(&host, "Bad-Name", "f", b"x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Unknown function -> 404.
    let (status, _) = invoke(&host, "acme", "nope", b"x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Non-component bytes -> rejected with a 4xx (compile failure).
    let (status, _) = upload(&control, "acme", "garbage", b"not a wasm component").await;
    assert!(status.is_client_error(), "got {status}");

    // Successful upload -> 201, digest matches the source hash.
    let (status, json) = upload(&control, "acme", "echo", TEST_GUEST_WASM).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let expected_digest = warpline_core::digest(TEST_GUEST_WASM);
    assert_eq!(json["digest"], expected_digest);

    // Invoke the freshly uploaded function -> 200, echoed body.
    let (status, body) = invoke(&host, "acme", "echo", b"hello there").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(body, b"hello there");

    // Re-upload (same bytes) works and doesn't break the pointer.
    let (status, _) = upload(&control, "acme", "echo", TEST_GUEST_WASM).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = invoke(&host, "acme", "echo", b"still here").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"still here");

    // Oversized upload (> 16 MiB) -> 413.
    let big = vec![0u8; 16 * 1024 * 1024 + 4096];
    let (status, _) = upload(&control, "acme", "big", &big).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // CPU loop -> 408 (dev-mode default cpu_budget_ms = 100).
    let (status, body) = invoke(&host, "acme", "echo", b"loop").await;
    assert_eq!(
        status,
        StatusCode::REQUEST_TIMEOUT,
        "{}",
        String::from_utf8_lossy(&body)
    );
}

/// Finding 1: the `.cwasm` is a derived cache keyed by an engine
/// compatibility hash — it can go missing (deleted, an engine/config
/// upgrade invalidates it) without the source `.wasm` going with it.
/// Deleting it after a successful upload must not break the next invoke:
/// `ComponentCache::get_or_load` should recompile from the persisted
/// source and re-publish a fresh `.cwasm`.
#[tokio::test]
async fn invoke_recovers_after_cwasm_is_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let control = control_router(dir.path());
    let host = host_router(dir.path());

    let (status, json) = upload(&control, "acme", "resilient", TEST_GUEST_WASM).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let digest = json["digest"].as_str().unwrap();

    let cwasm_files = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir.path().join("cwasm"))
            .expect("cwasm dir")
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(digest))
            .collect()
    };
    let before = cwasm_files();
    assert_eq!(before.len(), 1, "expected a cwasm to have been persisted");
    std::fs::remove_file(&before[0]).expect("delete cwasm");
    assert!(cwasm_files().is_empty());

    let (status, body) = invoke(&host, "acme", "resilient", b"still works").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(body, b"still works");

    // Recompiling on the miss should have re-published the cwasm too.
    assert_eq!(
        cwasm_files(),
        before,
        "expected the cwasm to be re-published"
    );
}

/// Admission control surfaces as 503 (memory budget) and 429 (per-tenant
/// in-flight cap) over HTTP.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overload_and_tenant_busy_map_to_503_and_429() {
    let dir = tempfile::tempdir().expect("tempdir");
    let control = control_router(dir.path());
    let (status, _) = upload(&control, "acme", "echo", TEST_GUEST_WASM).await;
    assert_eq!(status, StatusCode::CREATED);

    // Default limits reserve 64 MiB; a 32 MiB budget can never admit that.
    let small = host_router_with(dir.path(), |c| c.memory_budget_bytes = 32 * 1024 * 1024);
    let (status, _) = invoke(&small, "acme", "echo", b"x").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // One slot per tenant: a spinning invoke holds it, so the next is 429.
    let busy = host_router_with(dir.path(), |c| c.max_in_flight_per_tenant = 1);
    let spinner = {
        let busy = busy.clone();
        tokio::spawn(async move {
            // A poll below may briefly hold the slot; retry until we get it.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let res = invoke(&busy, "acme", "echo", b"loop").await;
                if res.0 != StatusCode::TOO_MANY_REQUESTS || std::time::Instant::now() >= deadline {
                    return res;
                }
            }
        })
    };
    // Poll until the spinner holds the slot (no fixed sleep to race on slow CI).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // A missing function: 404 while the slot is free, 429 once it is held.
        let (status, _) = invoke(&busy, "acme", "nope", b"x").await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never saw 429, last status {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let (status, _) = spinner.await.unwrap();
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    // Slot released.
    let (status, _) = invoke(&busy, "acme", "echo", b"x").await;
    assert_eq!(status, StatusCode::OK);
}
