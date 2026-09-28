//! DB-backed tests: invoke auth (401/403/200), a tenant config
//! (`allowed_hosts`) actually reaching the invoked guest, and a meter row
//! with `ok = false` after a guest trap. Only run when
//! `WARPLINE_TEST_DATABASE_URL` is set. Uploads go through the real
//! `warpline-control` router (already a dev-dependency of this crate) so
//! these tests exercise the same pointer-file handoff `dev_mode.rs` does,
//! just with real auth and metering behind it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use warpline_core::auth::DbState;
use warpline_core::kv::MemKv;
use warpline_core::registry::ComponentCache;
use warpline_core::runtime::{build_engine, build_http_client, build_linker, EpochTicker};

const TEST_GUEST_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../core/tests/fixtures/test_guest.wasm"
));
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tenant(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("t-{label}-{nanos}-{n}")
}

macro_rules! require_test_db {
    () => {{
        let Ok(url) = std::env::var("WARPLINE_TEST_DATABASE_URL") else {
            eprintln!("skipping: WARPLINE_TEST_DATABASE_URL not set");
            return;
        };
        url
    }};
}

/// Bare-bones HTTP/1.1 responder, mirroring `crates/core/tests/runtime.rs`'s
/// helper of the same shape — used to prove a tenant's `allowed_hosts`
/// config actually reaches `http-out::fetch`.
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
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

fn multipart_body(wasm: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--x\r\nContent-Disposition: form-data; name=\"wasm\"; filename=\"m.wasm\"\r\n\r\n",
    );
    body.extend_from_slice(wasm);
    body.extend_from_slice(b"\r\n--x--\r\n");
    body
}

async fn admin_create(
    control: &axum::Router,
    tenant: &str,
    admin_token: &str,
    config: Option<serde_json::Value>,
) -> String {
    let body = match config {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    let req = Request::builder()
        .method("POST")
        .uri(format!("/admin/tenants/{tenant}"))
        .header("authorization", format!("Bearer {admin_token}"))
        .body(body)
        .unwrap();
    let resp = control.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "admin create failed: {}",
        String::from_utf8_lossy(&bytes)
    );
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["api_key"].as_str().unwrap().to_string()
}

async fn upload(control: &axum::Router, tenant: &str, func: &str, key: &str, wasm: &[u8]) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant}/functions/{func}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from(multipart_body(wasm)))
        .unwrap();
    let resp = control.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "upload failed");
}

async fn invoke(
    host: &axum::Router,
    tenant: &str,
    func: &str,
    key: Option<&str>,
    body: &[u8],
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant}/functions/{func}/invoke"));
    if let Some(k) = key {
        builder = builder.header("authorization", format!("Bearer {k}"));
    }
    let req = builder.body(Body::from(body.to_vec())).unwrap();
    let resp = host.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn auth_config_and_metering_against_real_postgres() {
    let url = require_test_db!();
    let db = DbState::connect_to(&url).await.expect("connect to test db");

    let engine = build_engine().expect("build engine");
    let linker = build_linker(&engine).expect("build linker");
    let dir = tempfile::tempdir().expect("tempdir");

    let mut control_state = warpline_control::AppState::new(
        engine.clone(),
        linker.clone(),
        dir.path().to_path_buf(),
        db.clone(),
    );
    control_state.admin_token = Some("test-admin-token".to_string());
    let control = warpline_control::router(control_state);

    let ticker = EpochTicker::spawn(engine.clone());
    let (meter_tx, _meter_handle) = warpline_core::meter::spawn_writer(
        db.clone(),
        warpline_core::meter::METER_CHANNEL_CAPACITY,
    );
    let host_state = warpline_host::AppState {
        engine,
        linker,
        modules_dir: dir.path().to_path_buf(),
        component_cache: Arc::new(ComponentCache::new()),
        kv: Arc::new(MemKv::new()),
        http_client: build_http_client(true).expect("build http client"),
        // The allowed_hosts test below deliberately targets a loopback
        // test server; the guarded resolver would otherwise refuse it
        // regardless of the tenant's own allowlist.
        allow_private_egress: true,
        db: db.clone(),
        metrics_handle: warpline_host::metrics_handle(),
        ticker: Arc::new(ticker),
        meter_tx,
    };
    let host = warpline_host::router(host_state);

    // --- tenant A: default config -------------------------------------
    let tenant_a = unique_tenant("a");
    let tenant_b = unique_tenant("b");
    let key_a = admin_create(&control, &tenant_a, "test-admin-token", None).await;
    upload(&control, &tenant_a, "echo", &key_a, TEST_GUEST_WASM).await;

    // No key -> 401.
    let (status, _) = invoke(&host, &tenant_a, "echo", None, b"hi").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Tenant A's key against tenant B's path -> 403 (tenant B has no
    // function uploaded at all — auth is checked before registry lookup).
    let (status, _) = invoke(&host, &tenant_b, "echo", Some(&key_a), b"hi").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Correct key + tenant -> 200, echoed body.
    let (status, body) = invoke(&host, &tenant_a, "echo", Some(&key_a), b"hi there").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hi there");

    // --- config applied: allowed_hosts ---------------------------------
    let addr = spawn_raw_http_server(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_vec(),
    )
    .await;

    // Tenant A has no allowed_hosts -> fetch is rejected (guest-visible
    // error, not a trap).
    let fetch_input = format!("fetch:http://{addr}/").into_bytes();
    let (status, body) = invoke(&host, &tenant_a, "echo", Some(&key_a), &fetch_input).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).starts_with("err:"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    // Tenant C, configured via admin with `addr` in allowed_hosts, can
    // fetch it — proves the admin-set config actually reaches the guest.
    let tenant_c = unique_tenant("c");
    let key_c = admin_create(
        &control,
        &tenant_c,
        "test-admin-token",
        // `allowed_hosts` compares against `url::Url::host_str()`, which
        // never includes the port — see `runtime::http_fetch`.
        Some(serde_json::json!({ "allowed_hosts": [addr.ip().to_string()] })),
    )
    .await;
    upload(&control, &tenant_c, "echo", &key_c, TEST_GUEST_WASM).await;
    let (status, body) = invoke(&host, &tenant_c, "echo", Some(&key_c), &fetch_input).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(String::from_utf8_lossy(&body), "status:200:hi");

    // --- meter row written with ok = false after a trap -----------------
    let (status, _) = invoke(&host, &tenant_a, "echo", Some(&key_a), b"panic").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // Metering is queued onto a bounded channel to a batching writer task
    // (finding 5), not written inline — poll rather than sleeping a fixed
    // amount then trusting insertion order (`ORDER BY id DESC`), which a
    // batched, concurrently-running writer doesn't guarantee lines up with
    // wall-clock call order.
    let pool = db.pool().expect("postgres pool");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut failed_count = 0i64;
    while Instant::now() < deadline {
        failed_count = sqlx::query_scalar(
            "SELECT count(*) FROM meter WHERE tenant = $1 AND func = $2 AND ok = false",
        )
        .bind(&tenant_a)
        .bind("echo")
        .fetch_one(pool)
        .await
        .expect("query meter table");
        if failed_count >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        failed_count >= 1,
        "expected at least one meter row with ok = false for the panicking invoke within 5s"
    );
}

/// Finding 14: a tenant configured with a near-zero `cpu_budget_ms` gets a
/// 408 on a looping guest, and the config the admin route accepted is the
/// config actually persisted (not just the config the response echoed).
#[tokio::test]
async fn admin_cpu_budget_config_is_enforced_and_persisted() {
    let url = require_test_db!();
    let db = DbState::connect_to(&url).await.expect("connect to test db");

    let engine = build_engine().expect("build engine");
    let linker = build_linker(&engine).expect("build linker");
    let dir = tempfile::tempdir().expect("tempdir");

    let mut control_state = warpline_control::AppState::new(
        engine.clone(),
        linker.clone(),
        dir.path().to_path_buf(),
        db.clone(),
    );
    control_state.admin_token = Some("test-admin-token".to_string());
    let control = warpline_control::router(control_state);

    let ticker = EpochTicker::spawn(engine.clone());
    let (meter_tx, _meter_handle) = warpline_core::meter::spawn_writer(
        db.clone(),
        warpline_core::meter::METER_CHANNEL_CAPACITY,
    );
    let host_state = warpline_host::AppState {
        engine,
        linker,
        modules_dir: dir.path().to_path_buf(),
        component_cache: Arc::new(ComponentCache::new()),
        kv: Arc::new(MemKv::new()),
        http_client: build_http_client(true).expect("build http client"),
        allow_private_egress: true,
        db: db.clone(),
        metrics_handle: warpline_host::metrics_handle(),
        ticker: Arc::new(ticker),
        meter_tx,
    };
    let host = warpline_host::router(host_state);

    let tenant = unique_tenant("cpubudget");
    let key = admin_create(
        &control,
        &tenant,
        "test-admin-token",
        Some(serde_json::json!({ "cpu_budget_ms": 1 })),
    )
    .await;
    upload(&control, &tenant, "echo", &key, TEST_GUEST_WASM).await;

    let (status, body) = invoke(&host, &tenant, "echo", Some(&key), b"loop").await;
    assert_eq!(
        status,
        StatusCode::REQUEST_TIMEOUT,
        "{}",
        String::from_utf8_lossy(&body)
    );

    let pool = db.pool().expect("postgres pool");
    let (persisted_cpu_budget_ms,): (i32,) =
        sqlx::query_as("SELECT cpu_budget_ms FROM tenants WHERE name = $1")
            .bind(&tenant)
            .fetch_one(pool)
            .await
            .expect("tenant row");
    assert_eq!(persisted_cpu_budget_ms, 1);
}
