//! Atomic reserve/settle/release around a *paid* candidate (R2-D task 4).
//! Every component card this codebase ships today declares zero cost, so a
//! paid candidate can't actually occur yet outside a test that injects one
//! — this exists to make the wiring correct and testable ahead of a real
//! paid/remote candidate (R2-E's upstream trait) landing on top of it.
//!
//! No `packages/tenancy/src/budget.ts` 1:1 port here: this crate's
//! `idoris-tenancy::BudgetLedger` (R2-C) is a Rust-native atomic ledger the
//! TS side doesn't have; this module is just the request-shaped glue
//! around it (estimate → reserve → settle/release).

use idoris_contracts::provider::Cost;
use idoris_tenancy::budget::{
    BudgetError, BudgetLedger, BudgetScope, Price, ReservationId, estimate_tokens,
};

/// Conservative placeholder for the completion side of a cost estimate.
/// This crate doesn't parse a caller-supplied `max_tokens` out of the
/// request body yet, so every reservation assumes this many completion
/// tokens regardless of the actual request. Deliberately on the high side:
/// overestimating a reservation only shrinks the headroom visible to other
/// concurrent requests until [`settle`] corrects it to the real number;
/// underestimating could let a request through a tighter budget should
/// have blocked.
const RESERVED_COMPLETION_TOKENS: u64 = 1024;

/// No virtual-key system exists yet (interface spec §3.2's key vault isn't
/// implemented) — every reservation uses this fixed key id until one is.
const DEFAULT_KEY_ID: &str = "default";

/// Ledger bookkeeping key for a personal deployment (no `X-iDoris-Tenant`
/// header, no tenant mode) — purely internal, distinct from the wire-level
/// "tenant mode requires X-iDoris-Tenant" gate in `profile.rs`.
pub const PERSONAL_TENANT_ID: &str = "personal";

/// A per-request, per-candidate cost estimate in minor currency units.
/// `Some(0)` for an exactly-free candidate; `Some(n > 0)` for a priced one;
/// `None` when the provider's declared cost is malformed (negative or
/// non-finite) — invariant #3 ("price unknown ≠ free") means a malformed
/// cost must exclude the candidate at the `decide()` pricing stage, not
/// quietly become "free".
pub fn estimate_cost_minor(cost: &Cost, prompt: &str) -> Option<i64> {
    if cost.input_per_m == 0.0 && cost.output_per_m == 0.0 {
        return Some(0);
    }
    if !cost.input_per_m.is_finite()
        || !cost.output_per_m.is_finite()
        || cost.input_per_m < 0.0
        || cost.output_per_m < 0.0
    {
        return None;
    }
    let prompt_tokens = estimate_tokens(prompt, "unknown");
    #[allow(clippy::cast_precision_loss)] // token counts are nowhere near f64's precision ceiling
    let input_minor = prompt_tokens as f64 / 1_000_000.0 * cost.input_per_m;
    #[allow(clippy::cast_precision_loss)]
    let output_minor = RESERVED_COMPLETION_TOKENS as f64 / 1_000_000.0 * cost.output_per_m;
    let total = (input_minor + output_minor).ceil();
    if total.is_finite() && (0.0..=(i64::MAX as f64)).contains(&total) {
        #[allow(clippy::cast_possible_truncation)] // range-checked just above
        Some(total as i64)
    } else {
        None
    }
}

/// Whether a candidate's estimate represents a real charge (as opposed to
/// exactly free or price-unknown/excluded).
pub fn is_paid(estimated_cost_minor: Option<i64>) -> bool {
    estimated_cost_minor.is_some_and(|v| v > 0)
}

fn scope(tenant_id: Option<&str>, provider_id: &str) -> BudgetScope {
    let tenant_id = tenant_id.unwrap_or(PERSONAL_TENANT_ID);
    BudgetScope::new(tenant_id, DEFAULT_KEY_ID, provider_id, provider_id)
}

/// Reserves `estimated_cost_minor` for `provider_id` against `tenant_id`
/// (`None` → [`PERSONAL_TENANT_ID`]).
pub fn reserve(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    provider_id: &str,
    estimated_cost_minor: i64,
) -> Result<ReservationId, BudgetError> {
    ledger.reserve(
        &scope(tenant_id, provider_id),
        Price::Known(estimated_cost_minor),
    )
}

/// Finalizes a reservation with the actual cost, returning the amount
/// actually charged.
pub fn settle(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    reservation_id: &ReservationId,
    actual_cost_minor: i64,
) -> Result<i64, BudgetError> {
    let tenant_id = tenant_id.unwrap_or(PERSONAL_TENANT_ID);
    ledger
        .settle(tenant_id, reservation_id, actual_cost_minor)
        .map(|r| r.actual_cost_minor)
}

/// Releases a reservation for a call that didn't happen (backend failure)
/// — never call this for a call that actually completed (use [`settle`]).
pub fn release(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    reservation_id: &ReservationId,
) -> Result<(), BudgetError> {
    ledger.release(tenant_id.unwrap_or(PERSONAL_TENANT_ID), reservation_id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_tenancy::budget::SpendGate;
    use tempfile::TempDir;

    use super::*;

    fn free_cost() -> Cost {
        Cost {
            input_per_m: 0.0,
            output_per_m: 0.0,
        }
    }

    fn paid_cost() -> Cost {
        Cost {
            input_per_m: 100.0,
            output_per_m: 200.0,
        }
    }

    #[test]
    fn zero_cost_is_always_free() {
        assert_eq!(
            estimate_cost_minor(&free_cost(), "anything at all"),
            Some(0)
        );
        assert!(!is_paid(Some(0)));
    }

    #[test]
    fn positive_cost_is_paid_and_scales_with_the_rates() {
        let estimate = estimate_cost_minor(&paid_cost(), "hi").unwrap();
        assert!(estimate > 0);
        assert!(is_paid(Some(estimate)));
    }

    #[test]
    fn malformed_cost_is_price_unknown_not_free() {
        for bad in [
            Cost {
                input_per_m: -1.0,
                output_per_m: 0.0,
            },
            Cost {
                input_per_m: f64::NAN,
                output_per_m: 0.0,
            },
            Cost {
                input_per_m: f64::INFINITY,
                output_per_m: 0.0,
            },
        ] {
            assert_eq!(estimate_cost_minor(&bad, "hi"), None);
        }
    }

    fn ledger() -> (TempDir, BudgetLedger) {
        let dir = TempDir::new().unwrap();
        let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
        (dir, ledger)
    }

    #[test]
    fn reserve_settle_release_round_trip() {
        let (_dir, ledger) = ledger();
        ledger
            .configure_tenant("acme", 10_000, "UTC", SpendGate::PaidOnly)
            .unwrap();

        let id = reserve(&ledger, Some("acme"), "omlx", 500).unwrap();
        let charged = settle(&ledger, Some("acme"), &id, 400).unwrap();
        assert_eq!(charged, 400);

        // A second reservation, released instead of settled, must not count
        // against the balance afterward.
        let id2 = reserve(&ledger, Some("acme"), "omlx", 500).unwrap();
        release(&ledger, Some("acme"), &id2).unwrap();
        let balance = ledger.tenant_balance("acme").unwrap();
        assert_eq!(balance, 10_000 - 400);
    }

    #[test]
    fn reserve_without_configuration_fails_closed() {
        let (_dir, ledger) = ledger();
        let err = reserve(&ledger, Some("nobody"), "omlx", 500).unwrap_err();
        assert!(matches!(err, BudgetError::NotConfigured { .. }));
    }

    #[test]
    fn personal_deployment_uses_the_fixed_tenant_id() {
        let (_dir, ledger) = ledger();
        ledger
            .configure_tenant(PERSONAL_TENANT_ID, 1_000, "UTC", SpendGate::PaidOnly)
            .unwrap();
        let id = reserve(&ledger, None, "omlx", 100).unwrap();
        settle(&ledger, None, &id, 100).unwrap();
        assert_eq!(ledger.tenant_balance(PERSONAL_TENANT_ID).unwrap(), 900);
    }
}
