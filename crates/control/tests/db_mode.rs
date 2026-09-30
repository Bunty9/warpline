//! DB-backed tests for the admin route and upload auth. Only run when
//! `WARPLINE_TEST_DATABASE_URL` is set — see `README`/plan for how to stand
//! up a local test Postgres. Each test picks a fresh tenant name (PID +
//! nanosecond timestamp + a per-process counter) so concurrent test
//! functions against the same database never collide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use warpline_control::{router, AppState};
use warpline_core::pg::{self, Authenticator};
use warpline_core::{Runtime, RuntimeConfig};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, `valid_name`-shaped tenant name, unique within this process and
/// (with overwhelming probability) across concurrent `cargo test` runs.
fn unique_tenant(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("t-{label}-{nanos}-{n}", nanos = nanos, n = n)
}

/// Skips the calling test (with a message) if `WARPLINE_TEST_DATABASE_URL`
/// isn't set, otherwise connects (and migrates) against it.
macro_rules! require_test_db {
    () => {{
        let url = std::env::var("WARPLINE_TEST_DATABASE_URL").unwrap_or_default();
        if url.is_empty() {
            eprintln!("skipping: WARPLINE_TEST_DATABASE_URL not set");
            return;
        }
        let pool = sqlx::PgPool::connect(&url)
            .await
            .expect("connect to test db");
        pg::migrate(&pool).await.expect("migrate");
        Authenticator::new(pool, Duration::ZERO)
    }};
}

async fn state(db: Authenticator, admin_token: Option<&str>) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = Runtime::new(RuntimeConfig::new(dir.path())).expect("build runtime");
    let mut state = AppState::new(runtime, Some(db));
    state.admin_token = admin_token.map(str::to_string);
    (state, dir)
}

async fn post(
    router: &axum::Router,
    uri: &str,
    bearer: Option<&str>,
    json_body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match json_body {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    let req = builder.body(body).unwrap();
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

#[tokio::test]
async fn admin_route_disabled_without_token() {
    let db = require_test_db!();
    let (state, _dir) = state(db, None).await;
    let app = router(state);

    let (status, _) = post(&app, "/admin/tenants/whatever", Some("anything"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_route_rejects_missing_or_wrong_token() {
    let db = require_test_db!();
    let (state, _dir) = state(db, Some("correct-token")).await;
    let app = router(state);

    let (status, _) = post(&app, "/admin/tenants/whatever", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = post(&app, "/admin/tenants/whatever", Some("wrong-token"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_validates_config_ranges() {
    let db = require_test_db!();
    let tenant = unique_tenant("cfg");
    let (state, _dir) = state(db, Some("tok")).await;
    let app = router(state);

    let (status, _) = post(
        &app,
        &format!("/admin/tenants/{tenant}"),
        Some("tok"),
        Some(serde_json::json!({ "cpu_budget_ms": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post(
        &app,
        &format!("/admin/tenants/{tenant}"),
        Some("tok"),
        Some(serde_json::json!({ "mem_cap_bytes": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let too_many_hosts: Vec<String> = (0..65).map(|i| format!("h{i}.example.com")).collect();
    let (status, _) = post(
        &app,
        &format!("/admin/tenants/{tenant}"),
        Some("tok"),
        Some(serde_json::json!({ "allowed_hosts": too_many_hosts })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_create_tenant_is_idempotent_and_issues_a_key_each_time() {
    let db = require_test_db!();
    let tenant = unique_tenant("idem");
    let (state, _dir) = state(db, Some("tok")).await;
    let app = router(state);

    let (status1, json1) = post(&app, &format!("/admin/tenants/{tenant}"), Some("tok"), None).await;
    assert_eq!(status1, StatusCode::CREATED);
    let key1 = json1["api_key"].as_str().unwrap().to_string();
    assert!(key1.starts_with("wl_"));

    let (status2, json2) = post(&app, &format!("/admin/tenants/{tenant}"), Some("tok"), None).await;
    assert_eq!(status2, StatusCode::CREATED);
    let key2 = json2["api_key"].as_str().unwrap().to_string();
    assert_ne!(key1, key2, "each admin call issues a fresh key");
}

#[tokio::test]
async fn upload_requires_a_valid_key_for_the_matching_tenant() {
    let db = require_test_db!();
    let tenant_a = unique_tenant("a");
    let tenant_b = unique_tenant("b");
    let wasm: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core/tests/fixtures/test_guest.wasm"
    ));

    let (state, _dir) = state(db, Some("tok")).await;
    let app = router(state);

    let (_, json) = post(
        &app,
        &format!("/admin/tenants/{tenant_a}"),
        Some("tok"),
        None,
    )
    .await;
    let key_a = json["api_key"].as_str().unwrap();
    let (_, _) = post(
        &app,
        &format!("/admin/tenants/{tenant_b}"),
        Some("tok"),
        None,
    )
    .await;

    // No key -> 401.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant_a}/functions/echo"))
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from(multipart_body(wasm)))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Tenant A's key against tenant B's path -> 403.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant_b}/functions/echo"))
        .header("authorization", format!("Bearer {key_a}"))
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from(multipart_body(wasm)))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Tenant A's key against tenant A's path -> 201.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant_a}/functions/echo"))
        .header("authorization", format!("Bearer {key_a}"))
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from(multipart_body(wasm)))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
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

async fn upload_status(
    app: &axum::Router,
    tenant: &str,
    func: &str,
    key: &str,
    wasm: &[u8],
) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/tenants/{tenant}/functions/{func}"))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "multipart/form-data; boundary=x")
        .body(Body::from(multipart_body(wasm)))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// Finding 3: a tenant is capped at 100 distinct function names. Re-upload
/// of an existing name must still work at the cap (only *new* names count
/// against it), and going over it is a 403, not a 500 or a silently
/// accepted upload.
#[tokio::test]
async fn upload_enforces_per_tenant_function_quota() {
    let db = require_test_db!();
    let tenant = unique_tenant("quota");
    let wasm: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core/tests/fixtures/test_guest.wasm"
    ));
    let (state, _dir) = state(db, Some("tok")).await;
    let app = router(state);

    let (_, json) = post(&app, &format!("/admin/tenants/{tenant}"), Some("tok"), None).await;
    let key = json["api_key"].as_str().unwrap().to_string();

    for i in 0..100 {
        let status = upload_status(&app, &tenant, &format!("f{i}"), &key, wasm).await;
        assert_eq!(status, StatusCode::CREATED, "upload f{i} should succeed");
    }

    let status = upload_status(&app, &tenant, "f100", &key, wasm).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "101st distinct name over quota"
    );

    // Re-uploading an existing name still works even sitting right at the cap.
    let status = upload_status(&app, &tenant, "f0", &key, wasm).await;
    assert_eq!(status, StatusCode::CREATED, "re-upload of existing name");
}

/// Finding 5: the per-tenant quota check + insert must be serialized across
/// *all* of a tenant's uploads, not just uploads of the same func name — two
/// concurrent uploads of two different *new* names, both racing the last
/// slot under the cap, must not both pass the count check. Uses
/// `AppState::max_functions_per_tenant` (rather than uploading 100 real
/// functions) to get to "one slot left" cheaply.
#[tokio::test]
async fn upload_quota_race_allows_exactly_one_of_two_new_names() {
    let db = require_test_db!();
    let tenant = unique_tenant("qrace");
    let wasm: &'static [u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core/tests/fixtures/test_guest.wasm"
    ));
    let (mut state, _dir) = state(db, Some("tok")).await;
    state.max_functions_per_tenant = 2;
    let app = router(state);

    let (_, json) = post(&app, &format!("/admin/tenants/{tenant}"), Some("tok"), None).await;
    let key = json["api_key"].as_str().unwrap().to_string();

    // One slot used, one left under the cap of 2.
    let status = upload_status(&app, &tenant, "f0", &key, wasm).await;
    assert_eq!(status, StatusCode::CREATED);

    // Two distinct new names race for that last slot.
    let (r1, r2) = tokio::join!(
        tokio::spawn({
            let app = app.clone();
            let tenant = tenant.clone();
            let key = key.clone();
            async move { upload_status(&app, &tenant, "f1", &key, wasm).await }
        }),
        tokio::spawn({
            let app = app.clone();
            let tenant = tenant.clone();
            let key = key.clone();
            async move { upload_status(&app, &tenant, "f2", &key, wasm).await }
        }),
    );
    let (s1, s2) = (r1.unwrap(), r2.unwrap());
    let successes = [s1, s2]
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent new-name upload should pass the quota, got {s1:?} and {s2:?}"
    );
}

/// A tenant deleted while its key is still cached must get 401, not 500.
#[tokio::test]
async fn upload_for_deleted_tenant_with_cached_key_is_unauthorized() {
    let db = require_test_db!();
    let tenant = unique_tenant("gone");
    let wasm: &'static [u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core/tests/fixtures/test_guest.wasm"
    ));
    let pool = db.pool().clone();
    let cached = Authenticator::new(pool.clone(), Duration::from_secs(60));
    let (state, _dir) = state(cached, Some("tok")).await;
    let app = router(state);

    let (_, json) = post(&app, &format!("/admin/tenants/{tenant}"), Some("tok"), None).await;
    let key = json["api_key"].as_str().unwrap().to_string();
    assert_eq!(
        upload_status(&app, &tenant, "f", &key, wasm).await,
        StatusCode::CREATED
    );

    sqlx::query("DELETE FROM warpline.tenants WHERE name = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        upload_status(&app, &tenant, "f", &key, wasm).await,
        StatusCode::UNAUTHORIZED
    );
}
