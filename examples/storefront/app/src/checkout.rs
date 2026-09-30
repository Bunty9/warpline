//! The checkout domain: what the app sends a merchant's hook, how it treats
//! what comes back, and what it does when the hook cannot answer.
//!
//! Pure functions, no I/O, so the whole policy is unit-tested below.

use serde::{Deserialize, Serialize};
use warpline_core::InvokeError;

/// The function name every merchant publishes their hook under.
pub const HOOK_FN: &str = "checkout";
/// Hook output is untrusted input to this app: cap it before parsing.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_MESSAGE_CHARS: usize = 200;

const MAX_ITEMS: usize = 100;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Item {
    pub sku: String,
    pub qty: u32,
    pub unit_cents: u64,
}

/// The public request body of `POST /shops/{merchant}/checkout`.
#[derive(Debug, Deserialize)]
pub struct Order {
    pub customer: String,
    pub items: Vec<Item>,
}

impl Order {
    /// Check the request and return the subtotal in cents. Request data is
    /// untrusted too: bounded sizes, checked arithmetic.
    pub fn subtotal(&self) -> Result<u64, &'static str> {
        if self.customer.is_empty() || self.customer.chars().count() > 100 {
            return Err("customer must be 1-100 characters");
        }
        if self.items.is_empty() || self.items.len() > MAX_ITEMS {
            return Err("items must have 1-100 entries");
        }
        let mut total: u64 = 0;
        for item in &self.items {
            if item.sku.is_empty() || item.sku.chars().count() > 64 {
                return Err("sku must be 1-64 characters");
            }
            if item.qty == 0 {
                return Err("qty must be at least 1");
            }
            let line = item
                .unit_cents
                .checked_mul(u64::from(item.qty))
                .ok_or("order total overflows")?;
            total = total.checked_add(line).ok_or("order total overflows")?;
        }
        Ok(total)
    }
}

/// What the hook is given, as JSON.
#[derive(Serialize)]
pub struct HookInput<'a> {
    pub customer: &'a str,
    pub items: &'a [Item],
    pub subtotal_cents: u64,
    /// Where the hook finds the fraud service. Comes from this app's
    /// config, so a merchant's code carries no hardcoded hosts.
    pub fraud_api: &'a str,
}

/// What the hook must answer, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub approved: bool,
    pub discount_cents: u64,
    pub message: String,
}

/// Parse and vet hook output. Anything that fails is treated like a hook
/// failure by [`resolve`].
pub fn parse_decision(output: &[u8], subtotal_cents: u64) -> Result<Decision, String> {
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(format!(
            "output is {} bytes, max {MAX_OUTPUT_BYTES}",
            output.len()
        ));
    }
    let d: Decision = serde_json::from_slice(output).map_err(|e| format!("bad shape: {e}"))?;
    if d.discount_cents > subtotal_cents {
        return Err(format!(
            "discount {} exceeds subtotal {subtotal_cents}",
            d.discount_cents
        ));
    }
    if d.message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(format!(
            "message longer than {MAX_MESSAGE_CHARS} characters"
        ));
    }
    Ok(d)
}

/// What the app does with an invocation result. This enum is the policy.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The hook answered and the answer is valid: apply it.
    Hook(Decision),
    /// The merchant has no hook: approve, no discount.
    NoHook,
    /// The hook failed or misbehaved: approve, no discount. The string is
    /// for the operator's log, never shown to the shopper.
    FailOpen(String),
    /// We are at capacity: ask the shopper's client to retry (503).
    Retry,
}

/// The per-error-class policy (README documents it as a table):
///
/// | Result                                                  | Verdict          |
/// |---------------------------------------------------------|------------------|
/// | valid output                                            | `Hook`           |
/// | `NotFound` (nothing uploaded yet)                       | `NoHook`         |
/// | `Overloaded`, `TenantBusy`                              | `Retry`          |
/// | CPU/memory/wall-clock limit, trap, oversize/invalid output, load failure | `FailOpen` |
///
/// Fail open because a broken merchant hook should cost the merchant a
/// discount rule, not a sale. A shop that cannot tolerate that (say, a
/// mandatory fraud gate) would map these to a rejection instead: change
/// `FailOpen` to reject in the handler and nothing else.
pub fn resolve(result: Result<Vec<u8>, InvokeError>, subtotal_cents: u64) -> Verdict {
    match result {
        Ok(output) => match parse_decision(&output, subtotal_cents) {
            Ok(d) => Verdict::Hook(d),
            Err(why) => Verdict::FailOpen(format!("invalid hook output: {why}")),
        },
        Err(InvokeError::NotFound) => Verdict::NoHook,
        Err(InvokeError::Overloaded | InvokeError::TenantBusy) => Verdict::Retry,
        // Every other class, including variants added in later warpline
        // releases (`InvokeError` is non-exhaustive).
        Err(e) => Verdict::FailOpen(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warpline_core::{wasmtime, Error, Usage};

    fn item(qty: u32, unit_cents: u64) -> Item {
        Item {
            sku: "sku".into(),
            qty,
            unit_cents,
        }
    }
    fn order(items: Vec<Item>) -> Order {
        Order {
            customer: "alice".into(),
            items,
        }
    }
    fn ok(body: &str) -> Result<Vec<u8>, InvokeError> {
        Ok(body.as_bytes().to_vec())
    }

    #[test]
    fn subtotal_sums_and_validates() {
        assert_eq!(order(vec![item(2, 500), item(1, 250)]).subtotal(), Ok(1250));
        assert!(order(vec![]).subtotal().is_err());
        assert!(order(vec![item(0, 1)]).subtotal().is_err());
        assert!(order(vec![item(2, u64::MAX)]).subtotal().is_err());
        let mut o = order(vec![item(1, 1)]);
        o.customer.clear();
        assert!(o.subtotal().is_err());
    }

    #[test]
    fn decision_is_vetted() {
        let good = br#"{"approved":true,"discount_cents":100,"message":"hi"}"#;
        assert!(parse_decision(good, 100).is_ok());
        assert!(parse_decision(good, 99).is_err(), "discount > subtotal");
        assert!(parse_decision(b"not json", 100).is_err());
        assert!(parse_decision(br#"{"approved":true}"#, 100).is_err());
        assert!(parse_decision(
            br#"{"approved":true,"discount_cents":0,"message":"","x":1}"#,
            100
        )
        .is_err());
        let long = format!(
            r#"{{"approved":true,"discount_cents":0,"message":"{}"}}"#,
            "x".repeat(MAX_MESSAGE_CHARS + 1)
        );
        assert!(parse_decision(long.as_bytes(), 100).is_err());
        assert!(parse_decision(&vec![b' '; MAX_OUTPUT_BYTES + 1], 100).is_err());
    }

    #[test]
    fn policy_table() {
        let u = Usage::new(0, 0, 0);
        let trap = || wasmtime::Error::msg("boom");
        assert!(matches!(
            resolve(
                ok(r#"{"approved":false,"discount_cents":0,"message":"no"}"#),
                10
            ),
            Verdict::Hook(_)
        ));
        assert!(matches!(resolve(ok("garbage"), 10), Verdict::FailOpen(_)));
        assert_eq!(resolve(Err(InvokeError::NotFound), 10), Verdict::NoHook);
        assert_eq!(resolve(Err(InvokeError::Overloaded), 10), Verdict::Retry);
        assert_eq!(resolve(Err(InvokeError::TenantBusy), 10), Verdict::Retry);
        let fail_open = [
            InvokeError::CpuBudgetExceeded {
                usage: u,
                budget_ms: 1,
            },
            InvokeError::MemoryCapExceeded {
                usage: u,
                cap_bytes: 1,
            },
            InvokeError::WallClockTimeout { usage: u },
            InvokeError::OutputTooLarge { usage: u, limit: 1 },
            InvokeError::GuestTrap {
                usage: u,
                source: trap(),
            },
            InvokeError::Load(Error::Corrupt("x".into())),
        ];
        for e in fail_open {
            let name = e.to_string();
            assert!(
                matches!(resolve(Err(e), 10), Verdict::FailOpen(_)),
                "{name}"
            );
        }
    }
}
