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

/// H2 场景 C：唯一候选就是付费的，它自然就是 `pick()` 会选中的路径——预算
/// 超限且没有 Fallback，终态拒绝。
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
    // H2：`deep-local` 必须是唯一"本来就会输给付费候选"的那种（admission
    // 更差：RequiresEviction vs 付费候选的 Ready），不然 pick() 会因为它更
    // 便宜而本来就选中它——那样这次"回落"就是假的（H2 修复前的旧版本正是
    // 靠这个巧合让这个测试通过：不管付费候选会不会真的被选中，只要预算
    // 超限就无条件打上 BudgetFallback，见下面 `degrades_to_local` 反例）。
    let req = with_fallback(any_privacy_profile(Some(Role::Deep)));
    let mut local_alt = sample_card("deep-local", &[Role::Deep]);
    local_alt.admission_status = AdmissionStatus::RequiresEviction;
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

/// M4 变异防护：跟上面那个测试用同样的候选集合（付费候选才是 `pick()` 会
/// 自然选中的那条路径），唯一区别是**没有**声明 Fallback——一定不能静默
/// 回落到免费候选（那样"不需要请求头就能回落"或者"没声明也悄悄回落"这两个
/// 变异都测不出来），必须终态 402。
#[test]
fn budget_exceeded_natural_pick_is_paid_without_declared_fallback_is_terminal_402() {
    let req = any_privacy_profile(Some(Role::Deep)); // 没有 with_fallback
    let mut local_alt = sample_card("deep-local", &[Role::Deep]);
    local_alt.admission_status = AdmissionStatus::RequiresEviction;
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
    match decide(&req, &cards, &ctx) {
        Err(Rejection::BudgetExceeded {
            estimated_cost_minor,
            ..
        }) => assert_eq!(estimated_cost_minor, 500),
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
}

/// H2 场景 A：免费 Ready + 付费远程，预算超限，**没有**声明 Fallback——
/// `pick()` 本来就会选中免费的那个（admission 排位相同、成本更低），预算
/// 超限跟它无关，不该报 402，也不该产生降级标记。
#[test]
fn budget_exceeded_without_fallback_still_succeeds_when_the_natural_pick_is_free() {
    let req = any_privacy_profile(Some(Role::Deep)); // 没有 with_fallback
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
    assert!(!decision.is_degraded());
}

/// H2 场景 B：付费候选是 Blocked，免费候选 Ready——旧实现在 admission 之前
/// 就跑预算闸，会因为"候选集合里存在付费项"而误判；现在 admission 先排除
/// Blocked，预算阶段根本看不到这个付费候选，自然不会报 402。
#[test]
fn blocked_paid_candidate_never_reaches_the_budget_gate() {
    let req = any_privacy_profile(Some(Role::Deep));
    let local_alt = sample_card("deep-local", &[Role::Deep]);
    let mut remote = remote_card("deep-remote", &[Role::Deep], Some(500));
    remote.admission_status = AdmissionStatus::Blocked;
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
    assert!(!decision.is_degraded());
}

/// M2：即使租户还没有整体 `is_over()`，只要这一笔的成本超过剩余余额，也要
/// 按同样的规则处理（无 Fallback → 402）。
#[test]
fn balance_less_than_this_picks_cost_rejects_even_when_not_globally_over() {
    let req = any_privacy_profile(Some(Role::Deep));
    let remote = remote_card("deep-remote", &[Role::Deep], Some(500));
    let ctx_budget = FixedBudget(BudgetSnapshot {
        limit_minor: 1_000_000,
        spent_minor: 999_600, // balance_minor() == 400 < 500，但没有 is_over()
        scope: BudgetScope::PaidOnly,
    });
    let ctx = PolicyCtx {
        min_ram_gb: None,
        budget: Some(&ctx_budget),
    };
    match decide(&req, &[remote], &ctx) {
        Err(Rejection::BudgetExceeded {
            estimated_cost_minor,
            ..
        }) => assert_eq!(estimated_cost_minor, 500),
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
}

/// M1：角色/能力匹配后剩下的候选全部因为价格未知/非法被剔除——阶段码要
/// 落在 `Stage::Pricing`，不能被误判成 `Stage::Admission`（旧版本会因为
/// `priced_known` 提前变空、后面 `admission_eligible` 也跟着空而报错，
/// 但那样调用方会去查 admission 配置而不是定价数据源）。
#[test]
fn all_candidates_excluded_for_unknown_price_reports_the_pricing_stage() {
    let req = any_privacy_profile(Some(Role::Daily));
    let unknown = remote_card("unknown-price", &[Role::Daily], None);
    let cards = [unknown];
    let ctx = PolicyCtx::default();
    assert_eq!(
        decide(&req, &cards, &ctx),
        Err(Rejection::NoEligibleCandidate {
            stage: Stage::Pricing
        })
    );
}

/// M1：只有部分候选因为价格未知被剔除时，要在成功结果的 reason_codes 里
/// 留痕，而不是悄悄丢掉。
#[test]
fn partial_price_unknown_exclusion_is_recorded_as_a_reason_code() {
    let req = any_privacy_profile(Some(Role::Daily));
    let unknown = remote_card("a-unknown-price", &[Role::Daily], None);
    let known_free = sample_card("known-free", &[Role::Daily]);
    let cards = [unknown, known_free];
    let ctx = PolicyCtx::default();
    let decision = decide(&req, &cards, &ctx).unwrap();
    assert_eq!(decision.chosen_id, "known-free");
    assert!(
        decision
            .reason_codes
            .contains(&ReasonCode::PriceUnknownExcluded)
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

/// B1 task08 / D-B1-1: Rust deliberately keeps deterministic candidate
/// selection instead of the TS reference implementation's registration-order
/// pick. Every assertion puts the eventual winner second in the input slice,
/// so a weak "first eligible candidate" implementation fails this test.
#[test]
fn selection_contract_is_admission_then_cost_then_id_not_registration_order() {
    let req = any_privacy_profile(None);
    let ctx = PolicyCtx::default();

    let mut cheaper_but_eviction = sample_card("alpha", &[]);
    cheaper_but_eviction.admission_status = AdmissionStatus::RequiresEviction;
    cheaper_but_eviction.estimated_cost_minor = Some(0);
    let mut ready_but_paid = sample_card("zulu", &[]);
    ready_but_paid.estimated_cost_minor = Some(100);
    let decision = decide(&req, &[cheaper_but_eviction, ready_but_paid], &ctx).unwrap();
    assert_eq!(
        decision.chosen_id, "zulu",
        "admission rank must win before cost"
    );

    let mut expensive_first = sample_card("alpha", &[]);
    expensive_first.estimated_cost_minor = Some(100);
    let mut cheap_second = sample_card("zulu", &[]);
    cheap_second.estimated_cost_minor = Some(0);
    let decision = decide(&req, &[expensive_first, cheap_second], &ctx).unwrap();
    assert_eq!(decision.chosen_id, "zulu", "cost must win before id");

    let first = sample_card("zulu", &[]);
    let second = sample_card("alpha", &[]);
    let decision = decide(&req, &[first, second], &ctx).unwrap();
    assert_eq!(
        decision.chosen_id, "alpha",
        "id must deterministically break ties"
    );
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
