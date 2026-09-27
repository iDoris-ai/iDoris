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
    // L3：下限本来就是 local_only（不是内容检查收紧出来的），不该被打上
    // PrivacyTightenedByContent——这条请求压根没有声明 content_tightening。
    assert!(
        !decision
            .reason_codes
            .contains(&ReasonCode::PrivacyTightenedByContent)
    );
}

/// L3：下限本来就是 `local_only`，即使内容检查也返回 `local_only`，也不该
/// 算作"内容检查起了收紧作用"——它本来就没有放宽的空间。
#[test]
fn privacy_tightened_reason_code_only_fires_when_floor_actually_changes() {
    let mut req = local_only_profile(None);
    req.content_tightening = Some(PrivacyClass::LocalOnly);
    let cards = [sample_card("local-1", &[])];
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert!(
        !decision
            .reason_codes
            .contains(&ReasonCode::PrivacyTightenedByContent)
    );

    // 反例：下限是 any，内容检查真的收紧成 local_only——这次要打上标记。
    let mut req = any_privacy_profile(None);
    req.content_tightening = Some(PrivacyClass::LocalOnly);
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert!(
        decision
            .reason_codes
            .contains(&ReasonCode::PrivacyTightenedByContent)
    );
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
    // L5：`a-unknown-price` 的 id 字典序排在 `known-free` 之前——如果"价格
    // 未知按 None.unwrap_or(0) 当成免费"这个不变式 #3 检查被去掉，
    // `pick()` 的 tie-break 会因为 id 更小而错误选中它，这个测试才真正
    // 抓得住那个变异（旧的 id 顺序里"未知价"恰好字典序更大，去掉检查也测
    // 不出来）。
    let req = any_privacy_profile(Some(Role::Daily));
    let cheap_but_unknown = remote_card("a-unknown-price", &[Role::Daily], None);
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
    // （`bogus-negative` 的 id 字典序已经排在 `known-free` 之前，这个测试
    // 本身就能抓住"负成本当成免费"的变异，不需要像 L5 那样额外调整 id。）
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

/// H3 回归：无角色声明时，`experiment` 候选不能靠"没有具体角色可查"绕过——
/// 这三条路径（无角色、`idoris/auto`、角色降级）曾经完全不查 experiment/
/// min_ram_gb，只有走具体角色匹配（`is_eligible_for_role`）才会查。
#[test]
fn no_role_requested_still_excludes_experiment_candidates() {
    let mut experimental = sample_card("experimental", &[]);
    experimental.experiment = true;
    let req = any_privacy_profile(None);
    let ctx = PolicyCtx::default();
    assert_eq!(
        decide(&req, &[experimental], &ctx),
        Err(Rejection::NoEligibleCandidate {
            stage: Stage::RoleCapability
        })
    );
}

/// H3 回归：`idoris/auto` 同样要经过 min_ram_gb 硬门槛。
#[test]
fn auto_role_still_applies_min_ram_gb_threshold() {
    let mut too_big = sample_card("too-big", &[]);
    too_big.min_ram_gb = 64.0;
    let req = any_privacy_profile(Some(Role::Auto));
    let ctx = PolicyCtx {
        min_ram_gb: Some(16.0),
        budget: None,
    };
    assert_eq!(
        decide(&req, &[too_big], &ctx),
        Err(Rejection::NoEligibleCandidate {
            stage: Stage::RoleCapability
        })
    );
}

/// H3 回归：角色降级（有 Fallback）也不能放行 experiment 候选。
#[test]
fn role_fallback_still_excludes_experiment_candidates() {
    let mut experimental = sample_card("experimental", &[Role::Daily]);
    experimental.experiment = true;
    let req = with_fallback(any_privacy_profile(Some(Role::Deep)));
    let ctx = PolicyCtx::default();
    assert_eq!(
        decide(&req, &[experimental], &ctx),
        Err(Rejection::NoEligibleCandidate {
            stage: Stage::RoleCapability
        })
    );
}

#[test]
fn unknown_role_from_role_parse_error_maps_to_400() {
    let err = crate::role::parse_model_role("idoris/nope").unwrap_err();
    let rejection: Rejection = err.into();
    assert_eq!(rejection.http_status(), 400);
    assert_eq!(rejection.error_type(), "unknown_role");
}
