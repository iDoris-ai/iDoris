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

/// Shared math for [`estimate_cost_minor`]/[`estimate_actual_cost_minor`]:
/// `Some(0)` for exactly-free, `None` for malformed (negative/non-finite)
/// cost data (invariant #3: "price unknown ≠ free" — a malformed cost must
/// exclude the candidate, never quietly become "free"), else the priced
/// total for the given token counts.
fn priced_minor(cost: &Cost, input_tokens: u64, output_tokens: u64) -> Option<i64> {
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
    #[allow(clippy::cast_precision_loss)] // token counts are nowhere near f64's precision ceiling
    let input_minor = input_tokens as f64 / 1_000_000.0 * cost.input_per_m;
    #[allow(clippy::cast_precision_loss)]
    let output_minor = output_tokens as f64 / 1_000_000.0 * cost.output_per_m;
    let total = (input_minor + output_minor).ceil();
    if total.is_finite() && (0.0..=(i64::MAX as f64)).contains(&total) {
        #[allow(clippy::cast_possible_truncation)] // range-checked just above
        Some(total as i64)
    } else {
        None
    }
}

/// A per-request, per-candidate cost estimate in minor currency units, used
/// to size a [`reserve`] call *before* the real usage is known — see
/// `RESERVED_COMPLETION_TOKENS`'s doc for why the completion side is a
/// fixed placeholder here.
pub fn estimate_cost_minor(cost: &Cost, prompt: &str) -> Option<i64> {
    let prompt_tokens = estimate_tokens(prompt, "unknown");
    priced_minor(cost, prompt_tokens, RESERVED_COMPLETION_TOKENS)
}

/// The *actual* cost once both the prompt and the real completion text are
/// known — used to size a [`settle`] call. Falls back to `reserved_minor`
/// (the amount actually reserved) if the real token counts somehow produce
/// `None` here (defensive only — the same cost data already passed this
/// same check once to be reserved in the first place).
pub fn estimate_actual_cost_minor(
    cost: &Cost,
    prompt: &str,
    completion: &str,
    reserved_minor: i64,
) -> i64 {
    let input_tokens = estimate_tokens(prompt, "unknown");
    let output_tokens = estimate_tokens(completion, "unknown");
    priced_minor(cost, input_tokens, output_tokens).unwrap_or(reserved_minor)
}

/// The [`BudgetError`] a paid candidate produces when no [`BudgetLedger`]
/// is wired at all — same shape `reserve` would fail with against an
/// unconfigured scope, so callers can treat "no ledger" and "ledger has no
/// config for this scope" identically.
pub fn ledger_unavailable_error(tenant_id: Option<&str>, provider_id: &str) -> BudgetError {
    BudgetError::NotConfigured {
        scope: scope(tenant_id, provider_id),
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
/// actually charged, or None when durably queued for retry.
pub fn settle(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    reservation_id: &ReservationId,
    actual_cost_minor: i64,
) -> Result<Option<i64>, BudgetError> {
    let tenant_id = tenant_id.unwrap_or(PERSONAL_TENANT_ID);
    ledger.settle_durable(tenant_id, reservation_id, actual_cost_minor)
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
        // Minor units (e.g. cents) per million tokens; large enough that
        // a 1024-token placeholder reservation and a 1-token real
        // completion round to visibly different totals after `ceil()`.
        Cost {
            input_per_m: 1_000_000.0,
            output_per_m: 2_000_000.0,
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

    #[test]
    fn actual_cost_reflects_the_real_completion_length_not_the_reservation_placeholder() {
        let cost = paid_cost();
        let reserved = estimate_cost_minor(&cost, "hi").unwrap();
        // A one-word completion should settle for less than the fixed
        // RESERVED_COMPLETION_TOKENS placeholder the reservation assumed.
        let actual = estimate_actual_cost_minor(&cost, "hi", "ok", reserved);
        assert!(actual < reserved);
    }

    #[test]
    fn ledger_unavailable_error_matches_a_real_not_configured_rejection() {
        let (_dir, ledger) = ledger();
        let via_helper = ledger_unavailable_error(Some("acme"), "omlx");
        let via_real_call = reserve(&ledger, Some("acme"), "omlx", 500).unwrap_err();
        assert_eq!(via_helper, via_real_call);
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
        assert_eq!(charged, Some(400));

        // A second reservation, released instead of settled, must not count
        // against the balance afterward.
        let id2 = reserve(&ledger, Some("acme"), "omlx", 500).unwrap();
        release(&ledger, Some("acme"), &id2).unwrap();
        let balance = ledger.tenant_balance("acme").unwrap();
        assert_eq!(balance, 10_000 - 400);
    }

    #[tokio::test]
    async fn app_retries_pending_usage_without_another_request_or_restart() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("budget.sqlite3");
        let ledger = std::sync::Arc::new(
            BudgetLedger::open_with_busy_timeout(
                &path,
                std::sync::Arc::new(idoris_tenancy::budget::SystemClock),
                60_000,
                std::time::Duration::ZERO,
            )
            .unwrap(),
        );
        ledger
            .configure_tenant("acme", 1000, "UTC", SpendGate::All)
            .unwrap();
        let id = reserve(&ledger, Some("acme"), "p", 100).unwrap();
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(settle(&ledger, Some("acme"), &id, 80).unwrap(), None);
        let _app = crate::build_app(crate::AppState {
            budget_ledger: Some(ledger.clone()),
            ..crate::AppState::default()
        });
        blocker.execute_batch("ROLLBACK").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while ledger.tenant_balance("acme").unwrap() != 920 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
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
