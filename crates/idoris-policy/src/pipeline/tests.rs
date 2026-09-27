#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::budget::BudgetView;
use crate::card::test_support::sample_card;
use idoris_contracts::TaskProfile;
use idoris_contracts::common::{FallbackPolicy, Tier};

struct FixedBudget(BudgetSnapshot);

impl BudgetView for FixedBudget {
    fn snapshot(&self, _tenant_id: &str) -> Option<BudgetSnapshot> {
        Some(self.0)
    }
}

fn any_privacy_profile(role: Option<Role>) -> RequestProfile {
    RequestProfile {
        task: TaskProfile {
            privacy: Some(PrivacyClass::Any),
            ..Default::default()
        },
        role,
        tenant_id: Some("tenant-1".to_string()),
        content_tightening: None,
    }
}

fn local_only_profile(role: Option<Role>) -> RequestProfile {
    RequestProfile {
        task: TaskProfile {
            privacy: Some(PrivacyClass::LocalOnly),
            ..Default::default()
        },
        role,
        tenant_id: None,
        content_tightening: None,
    }
}

fn with_fallback(mut profile: RequestProfile) -> RequestProfile {
    profile.task.fallback = Some(FallbackPolicy::NextInChain);
    profile
}

fn remote_card(id: &str, roles: &[Role], cost_minor: Option<i64>) -> Card {
    let mut card = sample_card(id, roles);
    card.component.provider.locality = Locality::Remote;
    card.component.provider.tier = Tier::Remote;
    card.component.privacy_class = PrivacyClass::Any;
    card.estimated_cost_minor = cost_minor;
    card
}

#[test]
fn local_only_unavailable_without_a_loopback_candidate() {
    let req = local_only_profile(None);
    let cards = [remote_card("remote-1", &[], Some(0))];
    let ctx = PolicyCtx::default();
    assert_eq!(
        decide(&req, &cards, &ctx),
        Err(Rejection::LocalOnlyUnavailable)
    );
}

#[test]
fn local_only_succeeds_with_a_loopback_candidate() {
    let req = local_only_profile(None);
    let cards = [sample_card("local-1", &[])];
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert_eq!(decision.chosen_id, "local-1");
    assert!(
        decision
            .reason_codes
            .contains(&ReasonCode::PrivacyLoopbackOnly)
    );
    assert!(!decision.is_degraded());
}

#[test]
fn role_requested_without_any_match_is_rejected_without_fallback() {
    let req = any_privacy_profile(Some(Role::Deep));
    let cards = [sample_card("daily-1", &[Role::Daily])];
    let ctx = PolicyCtx::default();
    assert_eq!(
        decide(&req, &cards, &ctx),
        Err(Rejection::NoEligibleCandidate {
            stage: Stage::RoleCapability
        })
    );
}

#[test]
fn role_requested_without_any_match_degrades_to_capability_only_with_fallback() {
    let req = with_fallback(any_privacy_profile(Some(Role::Deep)));
    let cards = [sample_card("daily-1", &[Role::Daily])];
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert_eq!(decision.chosen_id, "daily-1");
    assert_eq!(
        decision.degradations,
        vec![Degradation::RoleFallback {
            requested: Role::Deep
        }]
    );
}

#[test]
fn unknown_price_candidate_is_never_selected() {
    let req = any_privacy_profile(Some(Role::Daily));
    let cheap_but_unknown = remote_card("mystery", &[Role::Daily], None);
    let known_free = sample_card("known-free", &[Role::Daily]);
    let cards = [cheap_but_unknown, known_free];
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert_eq!(decision.chosen_id, "known-free");
}

#[test]
fn negative_price_candidate_is_treated_like_unknown_price_never_selected() {
    // 调用方不应该产出负成本，但 Card 的类型层面不禁止——按不变式 #3 一律
    // 当「价格未知」处理，绝不能当成「比免费还便宜」被优先选中。
    let req = any_privacy_profile(Some(Role::Daily));
    let bogus_negative = remote_card("bogus-negative", &[Role::Daily], Some(-1));
    let known_free = sample_card("known-free", &[Role::Daily]);
    let cards = [bogus_negative, known_free];
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert_eq!(decision.chosen_id, "known-free");
}

#[test]
fn intent_needs_remote_budget_exhausted_without_fallback_is_terminal_402() {
    let req = any_privacy_profile(Some(Role::Deep));
    let cards = [remote_card("deep-remote", &[Role::Deep], Some(500))];
    let over_budget = FixedBudget(BudgetSnapshot {
        limit_minor: 100,
        spent_minor: 100,
        scope: BudgetScope::PaidOnly,
    });
    let ctx = PolicyCtx {
        min_ram_gb: None,
        budget: Some(&over_budget),
    };
    match decide(&req, &cards, &ctx) {
        Err(Rejection::BudgetExceeded {
            estimated_cost_minor,
            ..
        }) => {
            assert_eq!(estimated_cost_minor, 500);
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
}

#[test]
fn intent_needs_remote_budget_exhausted_with_fallback_degrades_to_local() {
    let req = with_fallback(any_privacy_profile(Some(Role::Deep)));
    let local_alt = sample_card("deep-local", &[Role::Deep]);
    let remote = remote_card("deep-remote", &[Role::Deep], Some(500));
    let cards = [remote, local_alt];
    let over_budget = FixedBudget(BudgetSnapshot {
        limit_minor: 100,
        spent_minor: 100,
        scope: BudgetScope::PaidOnly,
    });
    let ctx = PolicyCtx {
        min_ram_gb: None,
        budget: Some(&over_budget),
    };
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert_eq!(decision.chosen_id, "deep-local");
    assert_eq!(
        decision.degradations,
        vec![Degradation::BudgetFallback {
            estimated_cost_minor: 500
        }]
    );
}

#[test]
fn budget_scope_all_rejects_even_free_candidates_regardless_of_fallback() {
    let req = with_fallback(any_privacy_profile(Some(Role::Deep)));
    let local_alt = sample_card("deep-local", &[Role::Deep]);
    let remote = remote_card("deep-remote", &[Role::Deep], Some(500));
    let cards = [remote, local_alt];
    let over_budget = FixedBudget(BudgetSnapshot {
        limit_minor: 100,
        spent_minor: 200,
        scope: BudgetScope::All,
    });
    let ctx = PolicyCtx {
        min_ram_gb: None,
        budget: Some(&over_budget),
    };
    assert!(matches!(
        decide(&req, &cards, &ctx),
        Err(Rejection::BudgetExceeded { .. })
    ));
}

#[test]
fn admission_blocked_candidates_are_excluded_entirely() {
    let mut card = sample_card("blocked-1", &[]);
    card.admission_status = AdmissionStatus::Blocked;
    let req = any_privacy_profile(None);
    let ctx = PolicyCtx::default();
    assert_eq!(
        decide(&req, &[card], &ctx),
        Err(Rejection::NoEligibleCandidate {
            stage: Stage::Admission
        })
    );
}

#[test]
fn admission_requires_eviction_is_selectable_but_flagged() {
    let mut card = sample_card("evict-1", &[]);
    card.admission_status = AdmissionStatus::RequiresEviction;
    let req = any_privacy_profile(None);
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &[card], &ctx).unwrap();
    assert_eq!(decision.chosen_id, "evict-1");
    assert!(
        decision
            .reason_codes
            .contains(&ReasonCode::AdmissionRequiresEviction)
    );
}

#[test]
fn tie_break_picks_the_lexicographically_smaller_id() {
    let a = sample_card("bravo", &[]);
    let b = sample_card("alpha", &[]);
    let req = any_privacy_profile(None);
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &[a, b], &ctx).unwrap();
    assert_eq!(decision.chosen_id, "alpha");
}

#[test]
fn unknown_role_from_role_parse_error_maps_to_400() {
    let err = crate::role::parse_model_role("idoris/nope").unwrap_err();
    let rejection: Rejection = err.into();
    assert_eq!(rejection.http_status(), 400);
    assert_eq!(rejection.error_type(), "unknown_role");
}
