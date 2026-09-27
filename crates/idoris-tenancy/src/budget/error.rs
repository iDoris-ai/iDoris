//! Structured budget-ledger errors. Every non-success outcome is a distinct
//! variant carrying the fields a caller needs to build a response, per
//! contract-tenancy §4's "结构化" requirement — no free-text parsing.

use serde::Serialize;

use super::scope::BudgetScope;

/// `reason_code` the router's 402 body carries (contract-tenancy §4).
pub const BUDGET_EXCEEDED_REASON_CODE: &str = "budget_exceeded";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    /// Reserving `estimated_cost_minor` would exceed the scope's remaining
    /// balance for the current billing period. **Terminal rejection, not a
    /// downgrade** (总体规划 §4.6 / 不变式 #3) — the caller must surface a
    /// 402, not silently retry with a cheaper candidate itself.
    #[error(
        "budget exceeded: balance_minor={balance_minor}, estimated_cost_minor={estimated_cost_minor}"
    )]
    Exceeded {
        balance_minor: i64,
        estimated_cost_minor: i64,
        topup_hint: String,
    },

    /// `reserve` was called with `Price::Unknown`. Unknown price must never
    /// be treated as free (不变式 #3: "价格未知 ≠ 免费").
    #[error("price unknown: refusing to reserve budget for an unpriced candidate")]
    PriceUnknown,

    /// No `limit_minor`/`billing_timezone` configured for this scope —
    /// distinct from `Exceeded` because it's a setup gap, not a spend
    /// decision.
    #[error("no budget configured for scope {scope:?}")]
    NotConfigured { scope: BudgetScope },

    #[error("reservation not found: {reservation_id}")]
    ReservationNotFound { reservation_id: String },

    /// The reservation exists but isn't `active` (already settled, released,
    /// or TTL-expired) — settling/releasing it again is a caller bug, not
    /// something to silently ignore.
    #[error("reservation {reservation_id} is not active (status={status})")]
    ReservationNotActive {
        reservation_id: String,
        status: String,
    },

    /// A [`BudgetScope`] field was blank — rejected at `configure` time
    /// (contract-tenancy §3's "`tenant_id` must not be blank" rule,
    /// extended here to the whole scope tuple): silently accepting it would
    /// let a blank field become part of the SQL composite key, merging
    /// otherwise-unrelated calls onto the same budget.
    #[error("budget scope field {field:?} must not be blank")]
    InvalidScope { field: &'static str },

    #[error("invalid limit_minor: {limit_minor} (must be >= 0)")]
    InvalidLimit { limit_minor: i64 },

    #[error("invalid estimated_cost_minor: {estimated_cost_minor} (must be >= 0)")]
    InvalidEstimate { estimated_cost_minor: i64 },

    #[error("invalid actual_cost_minor: {actual_cost_minor} (must be >= 0)")]
    InvalidActualCost { actual_cost_minor: i64 },

    #[error("billing_timezone {billing_timezone:?} is not a recognized IANA time zone")]
    InvalidTimeZone { billing_timezone: String },

    #[error("timestamp out of range: {now_ms}")]
    InvalidTimestamp { now_ms: i64 },

    /// Reservation TTL must be positive: a zero/negative TTL would create
    /// reservations that are already expired, never counting against the
    /// balance (fail-open budget bypass).
    #[error("invalid reservation ttl_ms: {ttl_ms} (must be > 0)")]
    InvalidTtl { ttl_ms: i64 },

    #[error("sqlite storage error: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for BudgetError {
    fn from(err: rusqlite::Error) -> Self {
        BudgetError::Storage(err.to_string())
    }
}

impl BudgetError {
    /// Build `Exceeded` with a computed, human-readable `topup_hint` — the
    /// one constructor call sites should use instead of hand-rolling the
    /// hint text differently in different places.
    pub fn exceeded(balance_minor: i64, estimated_cost_minor: i64) -> Self {
        // Widen to i128 before subtracting: `estimated_cost_minor -
        // balance_minor` as plain i64 arithmetic can overflow (e.g. a large
        // negative `balance_minor` combined with a large positive
        // `estimated_cost_minor`), which would panic in debug builds and
        // silently produce a wrong (possibly negative-looking-positive)
        // shortfall in release builds (Codex review) — exactly the kind of
        // silent wrongness this crate's error messages are supposed to
        // avoid. i128 comfortably holds the difference of any two i64s.
        let shortfall = (estimated_cost_minor as i128 - balance_minor as i128).max(0);
        BudgetError::Exceeded {
            balance_minor,
            estimated_cost_minor,
            topup_hint: format!(
                "余额不足：还差 {shortfall} minor units 才能覆盖本次预估成本，请充值或提升预算上限后重试"
            ),
        }
    }

    /// Structured `402 Payment Required` body (contract-tenancy §4). `None`
    /// for every other variant — those aren't 402s at the HTTP layer, which
    /// this crate doesn't know about.
    pub fn to_402_body(&self) -> Option<Budget402Body> {
        match self {
            BudgetError::Exceeded {
                balance_minor,
                estimated_cost_minor,
                topup_hint,
            } => Some(Budget402Body {
                balance_minor: *balance_minor,
                estimated_cost_minor: *estimated_cost_minor,
                topup_hint: topup_hint.clone(),
                reason_code: BUDGET_EXCEEDED_REASON_CODE,
            }),
            _ => None,
        }
    }
}

/// The four fields a `budget_exceeded` rejection carries
/// (`balance_minor`/`estimated_cost_minor`/`topup_hint`/`reason_code`, per
/// this task's spec). This crate has no notion of HTTP, so it is
/// deliberately *not* contract-tenancy §4's full wire envelope
/// (`{"error": {"type": "budget_exceeded", "message": ..., "tenant_id":
/// ..., "limit_minor": ..., "spent_minor": ...}}`) — the router is the
/// layer that knows about HTTP status codes and the tenant/limit/spent
/// fields (which this ledger-level type doesn't have: it only ever sees a
/// `BudgetScope`, not the wider `TenantContext`). Serializing this struct
/// directly as an HTTP response body would therefore not match that
/// envelope shape; a router integration is expected to fold these fields
/// into its own `error` object rather than emit this JSON verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Budget402Body {
    pub balance_minor: i64,
    pub estimated_cost_minor: i64,
    pub topup_hint: String,
    pub reason_code: &'static str,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn exceeded_402_body_is_structured_and_carries_reason_code() {
        let err = BudgetError::exceeded(100, 250);
        let body = err.to_402_body().expect("Exceeded must yield a 402 body");
        assert_eq!(body.balance_minor, 100);
        assert_eq!(body.estimated_cost_minor, 250);
        assert_eq!(body.reason_code, "budget_exceeded");
        assert!(body.topup_hint.contains("150"));
    }

    /// Negative control: a non-`Exceeded` variant has no 402 body — callers
    /// must not accidentally treat e.g. `PriceUnknown` as a budget-exceeded
    /// response.
    #[test]
    fn non_exceeded_variants_have_no_402_body() {
        assert!(BudgetError::PriceUnknown.to_402_body().is_none());
        assert!(
            BudgetError::ReservationNotFound {
                reservation_id: "r1".to_string(),
            }
            .to_402_body()
            .is_none()
        );
    }
}
