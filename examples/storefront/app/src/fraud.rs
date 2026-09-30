//! The fraud-scoring service the merchant hook calls through `http-out`.
//! A stand-in for a real third-party API: any customer whose name starts
//! with `risky` scores 95, everyone else 10.

use axum::{extract::Query, routing::get, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
struct ScoreQuery {
    customer: String,
}

async fn score(Query(q): Query<ScoreQuery>) -> Json<Value> {
    let score = if q.customer.starts_with("risky") {
        95
    } else {
        10
    };
    Json(json!({ "score": score }))
}

pub fn router() -> Router {
    Router::new().route("/score", get(score))
}
