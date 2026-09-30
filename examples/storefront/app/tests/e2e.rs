//! The whole storefront flow through the real router, a real Postgres and
//! the real prebuilt hook (`tests/fixtures/checkout_hook.wasm`, refreshed by
//! `../build.sh`). Skipped unless `WARPLINE_TEST_DATABASE_URL` is set.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use storefront::{build_runtime, fraud, router, AppState};
use tower::ServiceExt;
use warpline_core::pg;

const HOOK: &[u8] = include_bytes!("fixtures/checkout_hook.wasm");
const ADMIN: &str = "test-admin-token";

struct Client(axum::Router);

impl Client {
    async fn call(
        &self,
        method: Method,
        uri: &str,
        bearer: Option<&str>,
        body: Body,
        json_body: bool,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if json_body {
            req = req.header("content-type", "application/json");
        }
        let resp = self
            .0
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn json(
        &self,
        method: Method,
        uri: &str,
        bearer: Option<&str>,
        v: Value,
    ) -> (StatusCode, Value) {
        self.call(method, uri, bearer, Body::from(v.to_string()), true)
            .await
    }

    async fn checkout(&self, merchant: &str, customer: &str) -> (StatusCode, Value) {
        let order =
            json!({"customer": customer, "items": [{"sku": "mug", "qty": 2, "unit_cents": 1000}]});
        self.json(
            Method::POST,
            &format!("/shops/{merchant}/checkout"),
            None,
            order,
        )
        .await
    }

    async fn new_merchant(&self, name: &str, hosts: Value) -> String {
        let (st, v) = self
            .json(
                Method::POST,
                "/admin/merchants",
                Some(ADMIN),
                json!({"name": name, "allowed_hosts": hosts}),
            )
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["api_key"].as_str().unwrap().to_string()
    }

    async fn upload(&self, merchant: &str, key: &str, wasm: &[u8]) -> (StatusCode, Value) {
        self.call(
            Method::PUT,
            &format!("/merchant/{merchant}/hook"),
            Some(key),
            Body::from(wasm.to_vec()),
            false,
        )
        .await
    }
}

#[tokio::test]
async fn storefront_flow() {
    let url = match std::env::var("WARPLINE_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skipping: WARPLINE_TEST_DATABASE_URL not set");
            return;
        }
    };

    // Fraud mock in-process on a free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fraud_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, fraud::router()).await });

    let pool = pg::connect(&url).await.unwrap();
    pg::migrate(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, meter) = build_runtime(dir.path(), &pool).unwrap();
    let state = AppState::new(
        runtime,
        pool.clone(),
        ADMIN,
        &format!("http://{fraud_addr}"),
    );
    let app = Client(router(state));

    // Unique per run so reruns against the same database don't collide.
    let suffix =
        &warpline_core::digest(format!("{:?}", std::time::SystemTime::now()).as_bytes())[..10];
    let (m1, m2) = (format!("shop-{suffix}"), format!("noegress-{suffix}"));

    // Admin route is guarded.
    let (st, _) = app
        .json(
            Method::POST,
            "/admin/merchants",
            Some("wrong"),
            json!({"name": &m1}),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Merchant 1 may call the fraud mock's host.
    let key1 = app.new_merchant(&m1, json!(["127.0.0.1"])).await;

    // No hook yet: approve without discount.
    let (st, v) = app.checkout(&m1, "alice").await;
    assert_eq!(
        (st, v["hook"].as_str(), v["discount_cents"].as_u64()),
        (StatusCode::OK, Some("none"), Some(0))
    );

    // Auth on the merchant routes.
    let (st, _) = app.upload(&m1, "wl_nope", HOOK).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let key_other = app
        .new_merchant(&format!("other-{suffix}"), json!([]))
        .await;
    let (st, _) = app.upload(&m1, &key_other, HOOK).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Invalid wasm is a 4xx and does not disturb anything.
    let (st, _) = app.upload(&m1, &key1, b"not wasm").await;
    assert!(st.is_client_error(), "{st}");

    let (st, v) = app.upload(&m1, &key1, HOOK).await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // Loyalty: 2000 subtotal; the 3rd order gets 10% = 200.
    for (n, expect) in [0u64, 0, 200].into_iter().enumerate() {
        let (st, v) = app.checkout(&m1, "alice").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["approved"], true, "order {}: {v}", n + 1);
        assert_eq!(v["discount_cents"], expect, "order {}: {v}", n + 1);
        assert_eq!(v["total_cents"], 2000 - expect);
        assert_eq!(v["hook"], "ok");
    }

    // Fraud: risky customers are rejected.
    let (st, v) = app.checkout(&m1, "risky-bob").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["approved"], false, "{v}");
    // A clean new customer is approved (also brings m1 to 5 invocations).
    let (_, v) = app.checkout(&m1, "carol").await;
    assert_eq!(
        (v["approved"].clone(), v["discount_cents"].clone()),
        (json!(true), json!(0))
    );

    // Merchant 2 has no allowed hosts: fetch fails, hook approves and says so.
    let key2 = app.new_merchant(&m2, json!([])).await;
    let (st, _) = app.upload(&m2, &key2, HOOK).await;
    assert_eq!(st, StatusCode::OK);
    let (_, v) = app.checkout(&m2, "risky-bob").await;
    assert_eq!(v["approved"], true, "{v}");
    assert_eq!(v["message"], "fraud check unavailable");

    // Bad requests and unknown merchants.
    let (st, _) = app
        .json(
            Method::POST,
            &format!("/shops/{m1}/checkout"),
            None,
            json!({"customer": "x", "items": []}),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = app.checkout("no-such-shop", "alice").await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Usage: metering is batched, so poll briefly.
    let mut seen = 0;
    for _ in 0..50 {
        let (st, v) = app
            .call(
                Method::GET,
                &format!("/merchant/{m1}/usage"),
                Some(&key1),
                Body::empty(),
                false,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        seen = v["invocations"].as_i64().unwrap();
        if seen >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(seen, 5, "usage should count alice x3, risky-bob, carol");

    // Shutdown drains the meter.
    meter.shutdown(std::time::Duration::from_secs(5)).await;
}
