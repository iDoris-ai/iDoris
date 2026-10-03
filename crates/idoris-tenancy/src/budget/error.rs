//! Structured budget-ledger errors. Every non-success outcome is a distinct
//! variant carrying the fields a caller needs to build a response, per
//! contract-tenancy §4's "结构化" requirement — no free-text parsing.

use serde::Serialize;

use super::scope::BudgetScope;

/// `reason_code` the router's 402 body carries (contract-tenancy §4).
pub const BUDGET_EXCEEDED_REASON_CODE: &str = "budget_exceeded";

/// Widen to i128 before subtracting, then saturate back into i64 range.
/// Plain i64 subtraction of two independently-sourced amounts (a persisted
/// limit vs. a persisted spend/estimate) can overflow — e.g. a corrupt or
/// adversarial row with a near-`i64::MAX` value — which would panic in
/// debug builds and wrap in release (Opus Tier-2 acceptance L1). i128
/// comfortably holds the difference of any two i64s, and saturating back
/// down is safe here: these values only ever feed a rejection message or a
/// >=/> comparison, never an on-disk column.
pub(crate) fn checked_sub_i64(a: i64, b: i64) -> i64 {
    let wide = a as i128 - b as i128;
    wide.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Same widen-then-saturate pattern as [`checked_sub_i64`], for sums of two
/// independently-sourced amounts (e.g. settled spend + active reservations)
/// that plain i64 addition could overflow (L1).
pub(crate) fn checked_add_i64(a: i64, b: i64) -> i64 {
    let wide = a as i128 + b as i128;
    wide.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    /// Reserving `estimated_cost_minor` would exceed the scope's remaining
    /// balance for the current billing period. **Terminal rejection, not a
    /// downgrade** (总体规划 §4.6 / 不变式 #3) — the caller must surface a
    /// 402, not silently retry with a cheaper candidate itself.
    ///
    /// `tenant_id`/`limit_minor`/`spent_minor` (Opus Tier-2 acceptance H2)
    /// describe whichever dimension actually failed the check — the
    /// tenant-level total (contract-tenancy §3) if that's what rejected, or
    /// the finer `(key, provider, model)` sub-scope if the tenant-level
    /// check passed but the sub-scope one didn't. `tenant_id` is always
    /// populated; `limit_minor`/`spent_minor` always describe the same
    /// dimension as `balance_minor`.
    #[error(
        "budget exceeded: tenant_id={tenant_id:?}, balance_minor={balance_minor}, estimated_cost_minor={estimated_cost_minor}"
    )]
    Exceeded {
        tenant_id: String,
        balance_minor: i64,
        estimated_cost_minor: i64,
        limit_minor: i64,
        spent_minor: i64,
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

    /// No tenant-level `limit_minor`/`billing_timezone` configured (Opus
    /// Tier-2 acceptance H2, landing in a follow-up PR) — distinct from
    /// [`NotConfigured`](Self::NotConfigured), which is about the finer
    /// `(key, provider, model)` sub-scope.
    #[error("no tenant-level budget configured for tenant {tenant_id:?}")]
    TenantNotConfigured { tenant_id: String },

    #[error("reservation not found: {reservation_id}")]
    ReservationNotFound { reservation_id: String },

    /// The reservation exists but isn't actionable (already settled or
    /// released, or in an earlier variant of this crate: also TTL-expired —
    /// see H1 in a follow-up PR for why "expired but not yet
    /// settled/released" stops being included here).
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

    /// The cost is durably retained for manual recovery; totals are unchanged.
    #[error(
        "amount overflow for reservation {reservation_id}: cost {actual_cost_minor} retained; tenant spending blocked"
    )]
    AmountOverflow {
        reservation_id: String,
        actual_cost_minor: i64,
    },

    /// `actual_cost_minor` charged in `settle` exceeded 4x the reserved
    /// amount (Opus Tier-2 acceptance M1, wired up in a follow-up PR). The
    /// charge is still recorded — this is a loud "something upstream is
    /// very wrong" signal, not a rejection of the money owed.
    #[error(
        "settled cost {actual_cost_minor} is more than 4x the reserved amount \
         {reserved_minor} for reservation {reservation_id} (charge was still recorded)"
    )]
    OverageTooLarge {
        reservation_id: String,
        reserved_minor: i64,
        actual_cost_minor: i64,
    },

    #[error("billing_timezone {billing_timezone:?} is not a recognized IANA time zone")]
    InvalidTimeZone { billing_timezone: String },

    /// `configure`/`configure_tenant` tried to change `billing_timezone`
    /// while reservations are still active for that scope/tenant (Opus
    /// Tier-2 acceptance L2, wired up in a follow-up PR) — the active
    /// reservation's period was computed under the *old* zone at reserve
    /// time, so changing the zone underneath it would leave `settle` unable
    /// to tell which period's bucket it belongs to.
    #[error("cannot change billing_timezone while reservations are still active")]
    TimeZoneChangeWithActiveReservations,

    /// `configure`/`configure_tenant` used a `billing_timezone` that
    /// disagrees with the one already in force elsewhere for the same
    /// tenant (Opus Tier-2 re-review B-2) — every `configure`/
    /// `configure_tenant` call for one tenant must agree on a single zone.
    /// Without this, a tenant-level sum computed under the tenant's own
    /// zone can silently disagree with a sub-scope's reservations bucketed
    /// under a different zone, especially near a period boundary.
    #[error(
        "tenant {tenant_id:?} already uses billing_timezone {existing:?}, cannot also use {requested:?}"
    )]
    InvalidConfig {
        tenant_id: String,
        existing: String,
        requested: String,
    },

    #[error("timestamp out of range: {now_ms}")]
    InvalidTimestamp { now_ms: i64 },

    /// Reservation TTL must be positive: a zero/negative TTL would create
    /// reservations that are already expired, never counting against the
    /// balance (fail-open budget bypass).
    #[error("invalid reservation ttl_ms: {ttl_ms} (must be > 0)")]
    InvalidTtl { ttl_ms: i64 },

    /// The reservation belongs to a different tenant than the one calling
    /// `settle`/`release`/`extend` (Opus Tier-2 acceptance M2, wired up in a
    /// follow-up PR). Hard tenant isolation: a caller must never learn
    /// *anything* about a reservation it doesn't own, so this is
    /// intentionally indistinguishable from `ReservationNotFound` from the
    /// outside.
    #[error("reservation not found: {reservation_id}")]
    TenantMismatch { reservation_id: String },

    /// SQLite reported `SQLITE_BUSY` even after waiting out the configured
    /// `busy_timeout` — a real contention/backpressure signal, distinct
    /// from every other `Storage(String)` failure (Opus Tier-2 acceptance
    /// L4): callers may reasonably want to retry a `Busy` but not e.g. a
    /// corrupt-database `Storage` error.
    #[error("sqlite busy: timed out waiting for a lock")]
    Busy,

    #[error("sqlite storage error: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for BudgetError {
    fn from(err: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(ffi_err, _) = &err
            && ffi_err.code == rusqlite::ErrorCode::DatabaseBusy
        {
            return BudgetError::Busy;
        }
        BudgetError::Storage(err.to_string())
    }
}

impl BudgetError {
    /// Build `Exceeded` with a computed, human-readable `topup_hint` — the
    /// one constructor call sites should use instead of hand-rolling the
    /// hint text differently in different places. `limit_minor`/
    /// `spent_minor` describe whichever dimension (tenant-level or sub-
    /// scope) actually rejected; `balance_minor` is derived from them so the
    /// two can never silently disagree.
    pub fn exceeded(
        tenant_id: impl Into<String>,
        limit_minor: i64,
        spent_minor: i64,
        estimated_cost_minor: i64,
    ) -> Self {
        let balance_minor = checked_sub_i64(limit_minor, spent_minor);
        let shortfall = checked_sub_i64(estimated_cost_minor, balance_minor).max(0);
        BudgetError::Exceeded {
            tenant_id: tenant_id.into(),
            balance_minor,
            estimated_cost_minor,
            limit_minor,
            spent_minor,
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
                tenant_id,
                balance_minor,
                estimated_cost_minor,
                limit_minor,
                spent_minor,
                topup_hint,
            } => Some(Budget402Body {
                tenant_id: tenant_id.clone(),
                balance_minor: *balance_minor,
                estimated_cost_minor: *estimated_cost_minor,
                limit_minor: *limit_minor,
                spent_minor: *spent_minor,
                topup_hint: topup_hint.clone(),
                reason_code: BUDGET_EXCEEDED_REASON_CODE,
            }),
            _ => None,
        }
    }
}

/// The fields a `budget_exceeded` rejection carries. Since Opus Tier-2
/// acceptance H2 this includes `tenant_id`/`limit_minor`/`spent_minor`,
/// matching contract-tenancy §4's 402 fields; this crate still has no
/// notion of HTTP, so it is deliberately *not* the full wire envelope
/// (`{"error": {"type": "budget_exceeded", "message": ..., ...}}`) — a
/// router integration is expected to fold these fields into its own
/// `error` object rather than emit this JSON verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Budget402Body {
    pub tenant_id: String,
    pub balance_minor: i64,
    pub estimated_cost_minor: i64,
    pub limit_minor: i64,
    pub spent_minor: i64,
    pub topup_hint: String,
    pub reason_code: &'static str,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn exceeded_402_body_is_structured_and_carries_reason_code() {
        let err = BudgetError::exceeded("acme-co", 100, 0, 250);
        let body = err.to_402_body().expect("Exceeded must yield a 402 body");
        assert_eq!(body.tenant_id, "acme-co");
        assert_eq!(body.balance_minor, 100);
        assert_eq!(body.estimated_cost_minor, 250);
        assert_eq!(body.limit_minor, 100);
        assert_eq!(body.spent_minor, 0);
        assert_eq!(body.reason_code, "budget_exceeded");
        assert!(body.topup_hint.contains("150"));
    }

    /// `limit_minor`/`spent_minor` can disagree with a naively-recomputed
    /// `balance_minor` only if the constructor's own arithmetic is wrong —
    /// pin the derived relationship down explicitly.
    #[test]
    fn exceeded_balance_is_always_limit_minus_spent() {
        let err = BudgetError::exceeded("t1", 1_000, 400, 700);
        match err {
            BudgetError::Exceeded {
                balance_minor,
                limit_minor,
                spent_minor,
                ..
            } => assert_eq!(balance_minor, limit_minor - spent_minor),
            other => panic!("expected Exceeded, got {other:?}"),
        }
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

    /// Negative control: overflowing i64 subtraction must saturate, not
    /// panic or silently wrap into a misleading sign (L1).
    #[test]
    fn checked_sub_i64_saturates_instead_of_overflowing() {
        assert_eq!(checked_sub_i64(i64::MIN, i64::MAX), i64::MIN);
        assert_eq!(checked_sub_i64(i64::MAX, i64::MIN), i64::MAX);
    }

    /// Negative control: overflowing i64 addition must saturate too (L1).
    #[test]
    fn checked_add_i64_saturates_instead_of_overflowing() {
        assert_eq!(checked_add_i64(i64::MAX, i64::MAX), i64::MAX);
        assert_eq!(checked_add_i64(i64::MIN, i64::MIN), i64::MIN);
    }

    /// L4: a raw `SQLITE_BUSY` failure maps to the dedicated `Busy` variant,
    /// not the generic `Storage` bucket — the `From` impl is the only place
    /// that distinction is made, so pin it down directly against a
    /// synthetic `rusqlite::Error` rather than relying on a real timeout.
    #[test]
    fn sqlite_busy_failure_maps_to_the_busy_variant() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: 5,
            },
            Some("database is locked".to_string()),
        );
        assert!(matches!(BudgetError::from(busy), BudgetError::Busy));
    }

    /// Negative control: a *different* sqlite failure code must still map
    /// to the generic `Storage` bucket, not `Busy`.
    #[test]
    fn non_busy_sqlite_failure_maps_to_storage() {
        let not_busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: 19,
            },
            Some("constraint failed".to_string()),
        );
        assert!(matches!(
            BudgetError::from(not_busy),
            BudgetError::Storage(_)
        ));
    }
}
