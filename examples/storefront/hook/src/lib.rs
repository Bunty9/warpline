//! A merchant's `checkout` hook: the code a customer uploads to warpline.
//!
//! Input (JSON, from the storefront app):
//!   `{customer, items, subtotal_cents, fraud_api}`
//! Output (JSON): `{approved, discount_cents, message}`
//!
//! It uses all three host capabilities of the `handler` world:
//! - `kv`       - a per-customer order counter (loyalty discount, 3rd order on)
//! - `http-out` - asks the fraud service for a risk score (host must be in
//!   the merchant's allowlist, or the fetch returns an error)
//! - `log`      - one line per decision
//!
//! Build with `../build.sh`.

// A copier of this example vendors `warpline.wit` next to this crate (for
// instance into `hook/wit/`) and points `path` at that directory. Here it is
// read straight from the warpline repo.
wit_bindgen::generate!({
    world: "handler",
    path: "../../../crates/core/wit",
});

use serde::{Deserialize, Serialize};
use warpline::host::http_out::{fetch, Request};
use warpline::host::{kv, log};

/// Scores above this are rejected.
const MAX_RISK_SCORE: u64 = 80;
/// From this order on (counting the current one) the customer gets a discount.
const LOYALTY_FROM_ORDER: u64 = 3;
const LOYALTY_PERCENT: u64 = 10;

#[derive(Deserialize)]
struct Input {
    customer: String,
    subtotal_cents: u64,
    fraud_api: String,
}

#[derive(Serialize)]
struct Decision {
    approved: bool,
    discount_cents: u64,
    message: String,
}

#[derive(Deserialize)]
struct Score {
    score: u64,
}

struct Hook;

impl Guest for Hook {
    fn handle(input: Vec<u8>) -> Vec<u8> {
        let decision = match serde_json::from_slice::<Input>(&input) {
            Ok(input) => decide(&input),
            Err(e) => Decision {
                approved: false,
                discount_cents: 0,
                message: format!("bad hook input: {e}"),
            },
        };
        log::emit(
            "info",
            &format!(
                "approved={} discount_cents={} message={}",
                decision.approved, decision.discount_cents, decision.message
            ),
        );
        // Serializing this plain struct cannot fail.
        serde_json::to_vec(&decision).unwrap_or_default()
    }
}

fn decide(input: &Input) -> Decision {
    // 1. Fraud check. An error (host not allowlisted, service down, garbage
    //    reply) approves the order but says so, rather than blocking sales.
    let mut note = None;
    match risk_score(input) {
        Ok(score) if score > MAX_RISK_SCORE => {
            return Decision {
                approved: false,
                discount_cents: 0,
                message: format!("rejected: risk score {score}"),
            };
        }
        Ok(_) => {}
        Err(_) => note = Some("fraud check unavailable"),
    }

    // 2. Loyalty counter. `kv` is scoped to this merchant by the host, so
    //    the key only needs the customer. Only approved orders count.
    let key = format!("orders/{}", input.customer);
    let previous = kv::get(&key)
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let this_order = previous + 1;
    kv::put(&key, this_order.to_string().as_bytes());

    let discount_cents = if this_order >= LOYALTY_FROM_ORDER {
        input.subtotal_cents * LOYALTY_PERCENT / 100
    } else {
        0
    };
    let message = match (note, discount_cents) {
        (Some(n), _) => n.to_string(),
        (None, 0) => "approved".to_string(),
        (None, _) => format!("approved: {LOYALTY_PERCENT}% loyalty discount (order {this_order})"),
    };
    Decision {
        approved: true,
        discount_cents,
        message,
    }
}

fn risk_score(input: &Input) -> Result<u64, String> {
    let url = format!(
        "{}/score?customer={}",
        input.fraud_api.trim_end_matches('/'),
        percent_encode(&input.customer)
    );
    let resp = fetch(&Request {
        url,
        method: "GET".to_string(),
        body: Vec::new(),
    })?;
    if resp.status != 200 {
        return Err(format!("fraud api returned {}", resp.status));
    }
    let score: Score = serde_json::from_slice(&resp.body).map_err(|e| e.to_string())?;
    Ok(score.score)
}

/// Minimal query-string escaping (RFC 3986 unreserved characters pass through).
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

export!(Hook);
