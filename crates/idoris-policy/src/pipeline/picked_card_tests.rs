#![allow(clippy::unwrap_used)]

use super::*;
use crate::budget::BudgetView;
use crate::card::test_support::sample_card;
use idoris_contracts::TaskProfile;
use idoris_contracts::common::{FallbackPolicy, Tier};

fn request() -> RequestProfile {
    RequestProfile {
        task: TaskProfile {
            privacy: Some(PrivacyClass::Any),
            ..Default::default()
        },
        role: None,
        tenant_id: Some("tenant".into()),
        content_tightening: None,
    }
}

fn remote(id: &str) -> Card {
    let mut card = sample_card(id, &[]);
    card.component.provider.locality = Locality::Remote;
    card.component.provider.tier = Tier::Remote;
    card.component.privacy_class = PrivacyClass::Any;
    card
}

fn parity(
    req: &RequestProfile,
    cards: &[Card],
    ctx: &PolicyCtx<'_>,
    session: Option<&str>,
    ready: &BTreeSet<String>,
    expected_index: usize,
) -> Decision {
    let old = decide_with_affinity_and_ready(req, cards, ctx, session, ready).unwrap();
    let (new, selected) =
        decide_with_affinity_and_ready_selected(req, cards, ctx, session, ready).unwrap();
    // Decision equality includes the ordered reasons and degradations.
    assert_eq!(old, new);
    assert!(std::ptr::eq(selected, &cards[expected_index]));
    assert_eq!(new.chosen_id, cards[expected_index].id());
    new
}

fn refusal(req: &RequestProfile, cards: &[Card], ctx: &PolicyCtx<'_>, expected: Rejection) {
    let ready = BTreeSet::new();
    let old = decide_with_affinity_and_ready(req, cards, ctx, None, &ready).unwrap_err();
    let new = decide_with_affinity_and_ready_selected(req, cards, ctx, None, &ready).unwrap_err();
    assert_eq!(old, expected);
    assert_eq!(new, old);
}

#[test]
fn privacy_tightening_returns_actual_local_card_and_preserves_refusal() {
    let cards = [remote("a-remote"), sample_card("z-local", &[])];
    let ctx = PolicyCtx::default();
    let mut req = request();
    req.content_tightening = Some(PrivacyClass::LocalOnly);
    let d = parity(&req, &cards, &ctx, None, &BTreeSet::new(), 1);
    assert_eq!(
        d.reason_codes,
        vec![
            ReasonCode::PrivacyTightenedByContent,
            ReasonCode::PrivacyLoopbackOnly,
            ReasonCode::BudgetNoTenantContext,
            ReasonCode::AdmissionReady
        ]
    );
    req.task.privacy = Some(PrivacyClass::LocalOnly);
    parity(&req, &cards, &ctx, None, &BTreeSet::new(), 1);
    refusal(&req, &cards[..1], &ctx, Rejection::LocalOnlyUnavailable);
}

#[test]
fn role_capability_price_and_admission_filters_preserve_identity_and_stage() {
    let mut wrong_capability = sample_card("a-no-chat", &[Role::Fast]);
    wrong_capability.component.provider.capabilities.clear();
    let mut unknown = sample_card("b-unknown", &[Role::Fast]);
    unknown.estimated_cost_minor = None;
    let mut blocked = sample_card("c-blocked", &[Role::Fast]);
    blocked.admission_status = AdmissionStatus::Blocked;
    let cards = [
        wrong_capability,
        unknown,
        blocked,
        sample_card("z-chosen", &[Role::Fast]),
    ];
    let mut req = request();
    req.role = Some(Role::Fast);
    let ctx = PolicyCtx::default();
    let d = parity(&req, &cards, &ctx, None, &BTreeSet::new(), 3);
    assert_eq!(
        d.reason_codes,
        vec![
            ReasonCode::RoleMatched(Role::Fast),
            ReasonCode::PriceUnknownExcluded,
            ReasonCode::BudgetNoTenantContext,
            ReasonCode::AdmissionReady
        ]
    );
    refusal(
        &req,
        &cards[..1],
        &ctx,
        Rejection::NoEligibleCandidate {
            stage: Stage::RoleCapability,
        },
    );
    refusal(
        &req,
        &cards[1..2],
        &ctx,
        Rejection::NoEligibleCandidate {
            stage: Stage::Pricing,
        },
    );
    refusal(
        &req,
        &cards[2..3],
        &ctx,
        Rejection::NoEligibleCandidate {
            stage: Stage::Admission,
        },
    );
}

struct Exhausted;
impl BudgetView for Exhausted {
    fn snapshot(&self, _: &str) -> Option<BudgetSnapshot> {
        Some(BudgetSnapshot {
            limit_minor: 0,
            spent_minor: 0,
            scope: BudgetScope::PaidOnly,
        })
    }
}

#[test]
fn budget_refusal_and_free_fallback_preserve_both_degradations_in_order() {
    let mut paid = remote("a-paid");
    paid.estimated_cost_minor = Some(7);
    let mut free = sample_card("z-free", &[]);
    free.admission_status = AdmissionStatus::RequiresEviction;
    let cards = [paid, free];
    let mut req = request();
    let ctx = PolicyCtx {
        budget: Some(&Exhausted),
        ..Default::default()
    };
    let error =
        decide_with_affinity_and_ready(&req, &cards, &ctx, None, &BTreeSet::new()).unwrap_err();
    assert!(matches!(
        error,
        Rejection::BudgetExceeded {
            estimated_cost_minor: 7,
            ..
        }
    ));
    refusal(&req, &cards, &ctx, error);
    req.task.fallback = Some(FallbackPolicy::NextInChain);
    req.role = Some(Role::Deep);
    let d = parity(&req, &cards, &ctx, None, &BTreeSet::new(), 1);
    assert_eq!(
        d.degradations,
        vec![
            Degradation::RoleFallback {
                requested: Role::Deep
            },
            Degradation::BudgetFallback {
                estimated_cost_minor: 7
            }
        ]
    );
    assert_eq!(
        d.reason_codes,
        vec![
            ReasonCode::RoleFallbackCapabilityOnly,
            ReasonCode::BudgetFallbackToFreeCandidate,
            ReasonCode::AdmissionRequiresEviction
        ]
    );
}

#[test]
fn affinity_ready_ties_and_priority_guards_return_the_input_reference() {
    let mut cards = [sample_card("a", &[]), sample_card("b", &[])];
    let req = request();
    let ctx = PolicyCtx::default();
    let ready = ["b".to_string()].into();
    parity(&req, &cards, &ctx, None, &ready, 0);
    for n in 0..32 {
        let session = format!("session-{n}");
        let expected = if affinity_score(&session, "a") >= affinity_score(&session, "b") {
            0
        } else {
            1
        };
        parity(
            &req,
            &cards,
            &ctx,
            Some(&session),
            &BTreeSet::new(),
            expected,
        );
        parity(&req, &cards, &ctx, Some(&session), &ready, 1);
    }
    cards[1].estimated_cost_minor = Some(1);
    parity(&req, &cards, &ctx, Some("session"), &ready, 0);
    cards[1].estimated_cost_minor = Some(0);
    cards[1].admission_status = AdmissionStatus::RequiresEviction;
    parity(&req, &cards, &ctx, Some("session"), &ready, 0);
}

#[test]
fn selected_borrow_outlives_request_context_and_tie_break_inputs() {
    let cards = [sample_card("only", &[])];
    let selected = {
        let req = request();
        let budget = Exhausted;
        let ctx = PolicyCtx {
            budget: Some(&budget),
            ..Default::default()
        };
        let session = String::from("short-lived");
        let ready = BTreeSet::new();
        decide_with_affinity_and_ready_selected(&req, &cards, &ctx, Some(&session), &ready)
            .unwrap()
            .1
    };
    assert!(std::ptr::eq(selected, &cards[0]));
}
