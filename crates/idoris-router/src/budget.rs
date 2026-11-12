//! Atomic reserve/settle/release around every selected candidate. A wired
//! ledger enforces SpendGate even for zero-cost calls; an unwired ledger
//! leaves free deployments usable and rejects paid calls.
//!
//! No `packages/tenancy/src/budget.ts` 1:1 port here: this crate's
//! `idoris-tenancy::BudgetLedger` (R2-C) is a Rust-native atomic ledger the
//! TS side doesn't have; this module is just the request-shaped glue
//! around it (estimate → reserve → settle/release).

use idoris_contracts::provider::Cost;
use idoris_tenancy::budget::{
    BudgetError, BudgetLedger, BudgetScope, Price, ReservationId, estimate_tokens,
};
use serde_json::Value;

/// Legacy reservation floor when no larger explicit output cap is supplied.
/// A smaller caller cap never reduces the existing conservative reservation.
pub(crate) const RESERVED_COMPLETION_TOKENS: u64 = 1024;

/// Validate both forwarded output caps before any quotation. Reject explicit
/// non-integers rather than letting an upstream coerce a larger cap than quoted.
/// Null is absent for quotation. The inclusive 2^53 ceiling preserves the
/// integer cap conversion; floating-point rate arithmetic may still round.
/// When both fields are present, quote their maximum regardless of upstream
/// precedence; retain the original fields and the legacy 1024-token floor.
pub(crate) fn completion_reservation_tokens(body: &Value) -> Result<u64, &'static str> {
    // Quotation accounts for one completion; reject multiplicity before forwarding.
    if body
        .get("n")
        .is_some_and(|count| !count.is_null() && count.as_u64() != Some(1))
    {
        return Err("n");
    }
    const MAX_EXACT_COMPLETION_TOKENS: u64 = 1_u64 << 53;
    let mut completion = RESERVED_COMPLETION_TOKENS;
    for field in ["max_tokens", "max_completion_tokens"] {
        if let Some(value) = body.get(field).filter(|value| !value.is_null()) {
            let cap = value
                .as_u64()
                .filter(|cap| *cap <= MAX_EXACT_COMPLETION_TOKENS)
                .ok_or(field)?;
            completion = completion.max(cap);
        }
    }
    Ok(completion)
}

/// The `key_id` the ledger records for a request that carries **no virtual
/// key**: the personal-deployment `IDORIS_DEV_NO_KEY=1` path, and library
/// callers that wire no authenticator.
///
/// This is deliberately the very value every ledger row used before the
/// virtual-key system was wired into the budget (the old private
/// `DEFAULT_KEY_ID`). Historical rows, and any sub-scope limit configured
/// against them, are keyed on it, so keeping the value keeps those rows
/// readable and keeps them counting toward the same tenant total. It is a
/// named, tested constant rather than an empty string or a new sentinel;
/// a real virtual key id is always `vk_` plus 32 hex digits, so the two
/// can never collide.
pub const NO_VIRTUAL_KEY_ID: &str = "default";

/// Ledger bookkeeping key for a personal deployment (no `X-iDoris-Tenant`
/// header, no tenant mode) — purely internal, distinct from the wire-level
/// "tenant mode requires X-iDoris-Tenant" gate in `profile.rs`.
pub const PERSONAL_TENANT_ID: &str = "personal";

/// Which virtual key and which model a reservation is recorded under.
///
/// Before S1 every reservation was written as `(default, provider, provider)`:
/// the key was a placeholder and the model column held the provider id.
/// Now both are the real values known when the reservation is made — the
/// authenticated virtual key's id, and the model that will actually serve the
/// request. A settlement needs neither again: it inherits both from the
/// reservation row it settles.
///
/// The tenant total is unaffected by either field; the ledger sums spend and
/// in-flight reservations per tenant, so changing which key or model a request
/// is recorded under never creates a fresh allowance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetIdentity {
    key_id: String,
    model_id: String,
}

impl BudgetIdentity {
    /// - `key_id`: the id of the virtual key that authenticated the request,
    ///   or `None` when no virtual key is involved (recorded as
    ///   [`NO_VIRTUAL_KEY_ID`]).
    /// - `model`: the model name that will serve the request (the backend
    ///   model for a local call, the model forwarded upstream for a direct
    ///   call). Recorded as given when it is a valid S0 `ModelId` (at most 128
    ///   bytes of letters, digits and `. _ - : ~ /`, which covers
    ///   `gpt-4o`, `qwen3:4b`, `org/model-name`).
    /// - `provider_id`: the component card's id, used as the model when none
    ///   was named or the name is not a valid `ModelId`. An unbounded,
    ///   caller-chosen string must not be written into every ledger row, so
    ///   an odd name degrades to the card (which is what was recorded before
    ///   S1) instead of being stored or rejected.
    pub fn resolve(key_id: Option<&str>, model: Option<&str>, provider_id: &str) -> Self {
        Self {
            key_id: key_id.unwrap_or(NO_VIRTUAL_KEY_ID).to_string(),
            model_id: ledger_model_id(model, provider_id),
        }
    }

    /// As [`resolve`](Self::resolve), taking the model from a request body's
    /// top-level `"model"` string (the value forwarded upstream). A missing
    /// or non-string `model` is "none named".
    pub fn for_request(key_id: Option<&str>, body: &serde_json::Value, provider_id: &str) -> Self {
        Self::resolve(
            key_id,
            body.get("model").and_then(serde_json::Value::as_str),
            provider_id,
        )
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

fn ledger_model_id(model: Option<&str>, provider_id: &str) -> String {
    match model {
        Some(name) if idoris_compute_contracts::ids::ModelId::new(name).is_ok() => name.to_string(),
        _ => provider_id.to_string(),
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageEvidence {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageEvidenceError {
    InvalidJson,
    MissingUsage,
    InvalidPromptTokens,
    InvalidCompletionTokens,
    InvalidPriceOrAmount,
}

impl std::fmt::Display for UsageEvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidJson => "upstream usage response is not valid JSON",
            Self::MissingUsage => "upstream response is missing usage evidence",
            Self::InvalidPromptTokens => "upstream prompt_tokens must be a non-negative integer",
            Self::InvalidCompletionTokens => {
                "upstream completion_tokens must be a non-negative integer"
            }
            Self::InvalidPriceOrAmount => "upstream usage cannot be converted to a valid charge",
        })
    }
}

impl std::error::Error for UsageEvidenceError {}

/// Parses the OpenAI chat-completion usage object used as billing evidence
/// for a paid direct proxy call. Missing or malformed usage is never
/// interpreted as zero.
pub fn parse_openai_usage(body: &[u8]) -> Result<UsageEvidence, UsageEvidenceError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| UsageEvidenceError::InvalidJson)?;
    let usage = value
        .as_object()
        .and_then(|object| object.get("usage"))
        .and_then(Value::as_object)
        .ok_or(UsageEvidenceError::MissingUsage)?;
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .ok_or(UsageEvidenceError::InvalidPromptTokens)?;
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .ok_or(UsageEvidenceError::InvalidCompletionTokens)?;
    Ok(UsageEvidence {
        input_tokens,
        output_tokens,
    })
}

/// Computes a charge from explicit upstream usage evidence. This has no
/// fallback to the reserved estimate: an estimate is not actual usage.
pub fn actual_cost_from_usage(
    cost: &Cost,
    usage: UsageEvidence,
) -> Result<i64, UsageEvidenceError> {
    priced_minor(cost, usage.input_tokens, usage.output_tokens)
        .ok_or(UsageEvidenceError::InvalidPriceOrAmount)
}

/// Per-candidate quote using the legacy output reservation floor.
pub fn estimate_cost_minor(cost: &Cost, prompt: &str) -> Option<i64> {
    estimate_cost_minor_with_completion(cost, prompt, RESERVED_COMPLETION_TOKENS)
}

pub(crate) fn estimate_cost_minor_with_completion(
    cost: &Cost,
    prompt: &str,
    completion: u64,
) -> Option<i64> {
    priced_minor(
        cost,
        estimate_tokens(prompt, "unknown"),
        completion.max(RESERVED_COMPLETION_TOKENS),
    )
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
pub fn ledger_unavailable_error(
    tenant_id: Option<&str>,
    identity: &BudgetIdentity,
    provider_id: &str,
) -> BudgetError {
    BudgetError::NotConfigured {
        scope: scope(tenant_id, identity, provider_id),
    }
}

/// Whether a candidate's estimate represents a real charge (as opposed to
/// exactly free or price-unknown/excluded).
pub fn is_paid(estimated_cost_minor: Option<i64>) -> bool {
    estimated_cost_minor.is_some_and(|v| v > 0)
}

fn scope(tenant_id: Option<&str>, identity: &BudgetIdentity, provider_id: &str) -> BudgetScope {
    let tenant_id = tenant_id.unwrap_or(PERSONAL_TENANT_ID);
    BudgetScope::new(
        tenant_id,
        identity.key_id(),
        provider_id,
        identity.model_id(),
    )
}

/// Reserves `estimated_cost_minor` for `provider_id` against `tenant_id`
/// (`None` → [`PERSONAL_TENANT_ID`]), recorded under `identity`'s real key
/// and model.
pub fn reserve(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    identity: &BudgetIdentity,
    provider_id: &str,
    estimated_cost_minor: i64,
) -> Result<ReservationId, BudgetError> {
    ledger.reserve(
        &scope(tenant_id, identity, provider_id),
        Price::Known(estimated_cost_minor),
    )
}

/// Runs the same admission checks as [`reserve`] without inserting a row.
pub fn check_admission(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    identity: &BudgetIdentity,
    provider_id: &str,
    estimated_cost_minor: i64,
) -> Result<(), BudgetError> {
    ledger.check_admission(
        &scope(tenant_id, identity, provider_id),
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

/// Idempotent settlement for a completed call whose actual cost is known.
/// This treats an already-committed extreme overage as complete and accepts
/// replay only when the stored settled amount matches exactly.
pub fn settle_replayable(
    ledger: &BudgetLedger,
    tenant_id: Option<&str>,
    reservation_id: &ReservationId,
    actual_cost_minor: i64,
) -> Result<i64, BudgetError> {
    let tenant_id = tenant_id.unwrap_or(PERSONAL_TENANT_ID);
    ledger.replay_settlement(tenant_id, reservation_id, actual_cost_minor)?;
    Ok(actual_cost_minor)
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
    fn explicit_openai_usage_is_the_only_buffered_proxy_billing_evidence() {
        let usage = parse_openai_usage(
            br#"{"usage":{"prompt_tokens":7,"completion_tokens":11,"total_tokens":18}}"#,
        )
        .unwrap();
        assert_eq!(
            usage,
            UsageEvidence {
                input_tokens: 7,
                output_tokens: 11
            }
        );
        assert_eq!(actual_cost_from_usage(&paid_cost(), usage).unwrap(), 29);
    }

    #[test]
    fn missing_or_malformed_usage_never_becomes_zero_cost() {
        for body in [
            br#"{}"#.as_slice(),
            br#"{"usage":null}"#.as_slice(),
            br#"{"usage":{"completion_tokens":1}}"#.as_slice(),
            br#"{"usage":{"prompt_tokens":1}}"#.as_slice(),
            br#"{"usage":{"prompt_tokens":-1,"completion_tokens":1}}"#.as_slice(),
            br#"{"usage":{"prompt_tokens":1.5,"completion_tokens":1}}"#.as_slice(),
            br#"{"usage":{"prompt_tokens":"1","completion_tokens":1}}"#.as_slice(),
        ] {
            assert!(parse_openai_usage(body).is_err(), "{body:?}");
        }
    }

    #[test]
    fn malformed_json_and_unrepresentable_charge_fail_closed_without_body_leakage() {
        let error = parse_openai_usage(br#"{"usage":"SECRET_SENTINEL""#).unwrap_err();
        assert_eq!(error, UsageEvidenceError::InvalidJson);
        assert!(!error.to_string().contains("SECRET_SENTINEL"));

        let usage = UsageEvidence {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
        };
        assert_eq!(
            actual_cost_from_usage(&paid_cost(), usage),
            Err(UsageEvidenceError::InvalidPriceOrAmount)
        );
    }

    /// No virtual key, no model named: records exactly what every pre-S1 row carries.
    fn unkeyed() -> BudgetIdentity {
        BudgetIdentity::resolve(None, None, "omlx")
    }

    #[test]
    fn ledger_unavailable_error_matches_a_real_not_configured_rejection() {
        let (_dir, ledger) = ledger();
        let via_helper = ledger_unavailable_error(Some("acme"), &unkeyed(), "omlx");
        let via_real_call = reserve(&ledger, Some("acme"), &unkeyed(), "omlx", 500).unwrap_err();
        assert_eq!(via_helper, via_real_call);
        // The same holds for a real key and model: the "no ledger" error names the same scope.
        let real = BudgetIdentity::resolve(Some("vk_abc"), Some("qwen3:4b"), "omlx");
        assert_eq!(
            ledger_unavailable_error(Some("acme"), &real, "omlx"),
            reserve(&ledger, Some("acme"), &real, "omlx", 500).unwrap_err()
        );
    }

    fn ledger() -> (TempDir, BudgetLedger) {
        let dir = TempDir::new().unwrap();
        let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
        (dir, ledger)
    }

    #[test]
    fn the_no_key_value_is_the_one_every_historical_row_carries() {
        // Pinned on purpose: changing it would orphan every historical row and any sub-scope limit
        // configured against it.
        assert_eq!(NO_VIRTUAL_KEY_ID, "default");
        assert!(!NO_VIRTUAL_KEY_ID.is_empty());
        // A real key id (`vk_` + 32 hex) can never equal it.
        assert!(!NO_VIRTUAL_KEY_ID.starts_with("vk_"));
    }

    #[test]
    fn no_key_and_no_model_reproduces_the_pre_s1_scope_exactly() {
        // The legacy tuple: (tenant, "default", provider, provider).
        assert_eq!(
            scope(None, &unkeyed(), "omlx"),
            BudgetScope::new(PERSONAL_TENANT_ID, "default", "omlx", "omlx")
        );
        assert_eq!(
            scope(Some("acme"), &unkeyed(), "omlx"),
            BudgetScope::new("acme", "default", "omlx", "omlx")
        );
    }

    #[test]
    fn a_real_key_and_model_replace_both_placeholders() {
        let identity = BudgetIdentity::resolve(
            Some("vk_0123456789abcdef0123456789abcdef"),
            Some("mlx-community/Qwen3-4B-4bit"),
            "omlx",
        );
        assert_eq!(identity.key_id(), "vk_0123456789abcdef0123456789abcdef");
        assert_eq!(identity.model_id(), "mlx-community/Qwen3-4B-4bit");
        assert_eq!(
            scope(Some("acme"), &identity, "omlx"),
            BudgetScope::new(
                "acme",
                "vk_0123456789abcdef0123456789abcdef",
                "omlx",
                "mlx-community/Qwen3-4B-4bit"
            )
        );
        // The provider column is still the provider, never the model, and the model is no longer the provider.
        let scope = scope(None, &identity, "omlx");
        assert_eq!(scope.provider_id, "omlx");
        assert_ne!(scope.model_id, scope.provider_id);
    }

    #[test]
    fn an_unnamed_or_unusable_model_name_degrades_to_the_provider_id() {
        let long_ok = "m".repeat(128);
        let too_long = "m".repeat(129);
        for ok in [
            "gpt-4o",
            "qwen3:4b",
            "org/model-name",
            "mlx-community/Qwen3-4B-4bit",
            "a.b_c~d-9",
            long_ok.as_str(),
        ] {
            assert_eq!(
                BudgetIdentity::resolve(None, Some(ok), "omlx").model_id(),
                ok,
                "{ok}"
            );
        }
        for odd in [
            "",
            " ",
            "has space",
            "a@b",
            "line\nbreak",
            "学生",
            "model#1",
            too_long.as_str(),
        ] {
            assert_eq!(
                BudgetIdentity::resolve(None, Some(odd), "omlx").model_id(),
                "omlx",
                "{odd:?} must not be written into the ledger"
            );
        }
        assert_eq!(
            BudgetIdentity::resolve(None, None, "omlx").model_id(),
            "omlx"
        );
    }

    #[test]
    fn for_request_reads_the_forwarded_model_from_the_body() {
        let named = serde_json::json!({"model": "vendor/model-v2", "messages": []});
        assert_eq!(
            BudgetIdentity::for_request(Some("vk_1"), &named, "omlx"),
            BudgetIdentity::resolve(Some("vk_1"), Some("vendor/model-v2"), "omlx")
        );
        // Missing, null, or a non-string `model` count as "none named".
        for body in [
            serde_json::json!({"messages": []}),
            serde_json::json!({"model": null}),
            serde_json::json!({"model": 7}),
            serde_json::json!({"model": ["a"]}),
            serde_json::json!("not an object"),
        ] {
            let identity = BudgetIdentity::for_request(None, &body, "omlx");
            assert_eq!(identity.model_id(), "omlx", "{body}");
            assert_eq!(identity.key_id(), NO_VIRTUAL_KEY_ID);
        }
    }

    #[test]
    fn a_reservation_and_its_settlement_land_under_the_real_key_and_model() {
        let (dir, ledger) = ledger();
        ledger
            .configure_tenant("acme", 10_000, "UTC", SpendGate::PaidOnly)
            .unwrap();
        let identity = BudgetIdentity::resolve(Some("vk_real"), Some("qwen3:4b"), "omlx");
        let id = reserve(&ledger, Some("acme"), &identity, "omlx", 500).unwrap();
        settle(&ledger, Some("acme"), &id, 400).unwrap();

        let conn = rusqlite::Connection::open(dir.path().join("budget.sqlite3")).unwrap();
        let row: (String, String, String) = conn
            .query_row(
                "SELECT key_id, provider_id, model_id FROM reservations WHERE id = ?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("vk_real".into(), "omlx".into(), "qwen3:4b".into()));
        // Settlement inherits both from the row: spend is booked under the same real scope.
        let spent: i64 = conn
            .query_row(
                "SELECT spent_minor FROM budget_periods WHERE tenant_id='acme' AND key_id='vk_real' \
                 AND provider_id='omlx' AND model_id='qwen3:4b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(spent, 400);
        let placeholders: i64 = conn
            .query_row(
                "SELECT count(*) FROM reservations WHERE key_id = 'default' OR model_id = provider_id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            placeholders, 0,
            "no placeholder may be written once the identity is known"
        );
    }

    #[test]
    fn reserve_settle_release_round_trip() {
        let (_dir, ledger) = ledger();
        ledger
            .configure_tenant("acme", 10_000, "UTC", SpendGate::PaidOnly)
            .unwrap();

        let id = reserve(&ledger, Some("acme"), &unkeyed(), "omlx", 500).unwrap();
        let charged = settle(&ledger, Some("acme"), &id, 400).unwrap();
        assert_eq!(charged, 400);

        // A second reservation, released instead of settled, must not count
        // against the balance afterward.
        let id2 = reserve(&ledger, Some("acme"), &unkeyed(), "omlx", 500).unwrap();
        release(&ledger, Some("acme"), &id2).unwrap();
        let balance = ledger.tenant_balance("acme").unwrap();
        assert_eq!(balance, 10_000 - 400);
    }

    #[test]
    fn replayable_settlement_accepts_committed_extreme_overage_without_double_charge() {
        let (_dir, ledger) = ledger();
        ledger
            .configure_tenant("acme", 10_000, "UTC", SpendGate::PaidOnly)
            .unwrap();

        let id = reserve(&ledger, Some("acme"), &unkeyed(), "omlx", 1).unwrap();
        assert_eq!(settle_replayable(&ledger, Some("acme"), &id, 5).unwrap(), 5);
        let once = ledger.tenant_readview("acme").unwrap();
        assert_eq!((once.spent_minor, once.reserved_minor), (5, 0));

        assert_eq!(settle_replayable(&ledger, Some("acme"), &id, 5).unwrap(), 5);
        let replayed = ledger.tenant_readview("acme").unwrap();
        assert_eq!((replayed.spent_minor, replayed.reserved_minor), (5, 0));
    }

    #[test]
    fn reserve_without_configuration_fails_closed() {
        let (_dir, ledger) = ledger();
        let err = reserve(&ledger, Some("nobody"), &unkeyed(), "omlx", 500).unwrap_err();
        assert!(matches!(err, BudgetError::NotConfigured { .. }));
    }

    #[test]
    fn personal_deployment_uses_the_fixed_tenant_id() {
        let (_dir, ledger) = ledger();
        ledger
            .configure_tenant(PERSONAL_TENANT_ID, 1_000, "UTC", SpendGate::PaidOnly)
            .unwrap();
        let id = reserve(&ledger, None, &unkeyed(), "omlx", 100).unwrap();
        settle(&ledger, None, &id, 100).unwrap();
        assert_eq!(ledger.tenant_balance(PERSONAL_TENANT_ID).unwrap(), 900);
    }
}
