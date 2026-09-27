//! 性质测试（proptest）：决策管道的两条不变式。
//!
//! - `local_only` 的决策结果里永远不会出现非 loopback 的候选。
//! - 同一输入两次运行 `decide` 结果一致（纯函数、无 IO、无随机数）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use idoris_contracts::tenant::BudgetScope;
use idoris_contracts::{ComponentCard, TaskProfile};
use idoris_policy::{
    AdmissionStatus, BudgetSnapshot, BudgetView, Card, PolicyCtx, ReasonCode, Rejection,
    RequestProfile, Role, decide, effective_privacy, effective_served_locality,
};
use proptest::prelude::*;

struct FixedBudget(BudgetSnapshot);

impl BudgetView for FixedBudget {
    fn snapshot(&self, _tenant_id: &str) -> Option<BudgetSnapshot> {
        Some(self.0)
    }
}

fn budget_snapshot_strategy() -> impl Strategy<Value = BudgetSnapshot> {
    (
        // 跟 spent_minor 同一个量级，让 is_over() 大致五五开——避免全是超支，
        // 这样"成功且真正走过预算路径"的分支才有机会被采样到。
        0i64..=1000,
        0i64..=1000,
        prop_oneof![Just(BudgetScope::PaidOnly), Just(BudgetScope::All)],
    )
        .prop_map(|(limit_minor, spent_minor, scope)| BudgetSnapshot {
            limit_minor,
            spent_minor,
            scope,
        })
}

fn role_strategy() -> impl Strategy<Value = Role> {
    prop_oneof![
        Just(Role::Fast),
        Just(Role::Daily),
        Just(Role::Deep),
        Just(Role::Vision),
        Just(Role::Embed),
        Just(Role::Rerank),
        Just(Role::Decide),
    ]
}

fn locality_strategy() -> impl Strategy<Value = Locality> {
    prop_oneof![
        Just(Locality::Loopback),
        Just(Locality::Lan),
        Just(Locality::Remote)
    ]
}

fn form_strategy() -> impl Strategy<Value = Form> {
    prop_oneof![
        Just(Form::HttpService),
        Just(Form::SpawnCli),
        Just(Form::BundledBinary)
    ]
}

fn admission_strategy() -> impl Strategy<Value = AdmissionStatus> {
    prop_oneof![
        Just(AdmissionStatus::Ready),
        Just(AdmissionStatus::RequiresEviction),
        Just(AdmissionStatus::Blocked),
    ]
}

type CardFields = (
    Locality,
    Form,
    Vec<Role>,
    AdmissionStatus,
    Option<i64>,
    bool,
);

fn card_fields_strategy() -> impl Strategy<Value = CardFields> {
    (
        locality_strategy(),
        form_strategy(),
        prop::collection::vec(role_strategy(), 0..3),
        admission_strategy(),
        // 显式给 0（免费）一个不小的权重——连续区间 `1..=1000` 里精确采样到 0
        // 的概率可以忽略不计，如果只用 `0..=1000` 均匀采样，"存在免费候选"
        // 这个场景在性质测试里几乎不会被真正覆盖到。
        prop::option::of(prop_oneof![
            3 => Just(0i64),
            7 => 1i64..=1000,
        ]),
        any::<bool>(),
    )
}

/// 由随机字段构造一张候选卡；`id` 由调用方保证同一批候选里唯一、可读。
fn build_card(id: String, fields: CardFields) -> Card {
    let (locality, form, roles, admission, cost, experiment) = fields;
    let privacy_class = if locality == Locality::Loopback {
        PrivacyClass::LocalOnly
    } else {
        PrivacyClass::Any
    };
    Card {
        component: ComponentCard {
            provider: ProviderDescriptor {
                id,
                family: Family::Idoris,
                tier: if locality == Locality::Loopback {
                    Tier::Local
                } else {
                    Tier::Remote
                },
                capabilities: vec![Capability::Chat],
                privacy_class,
                cost: Cost {
                    input_per_m: 0.0,
                    output_per_m: 0.0,
                },
                locality,
                extensions: None,
            },
            form,
            endpoint: "http://127.0.0.1:8740".to_string(),
            version_pin: "0.0.0".to_string(),
            privacy_class,
            allowed_egress: vec![Egress::Loopback],
            fallback_policy: FallbackPolicy::FailClosed,
            fail_closed: true,
            load_policy: None,
            extensions: None,
        },
        roles,
        experiment,
        min_ram_gb: 0.0,
        estimated_cost_minor: cost,
        admission_status: admission,
    }
}

fn cards_strategy() -> impl Strategy<Value = Vec<Card>> {
    prop::collection::vec(card_fields_strategy(), 1..6).prop_map(|fields| {
        fields
            .into_iter()
            .enumerate()
            .map(|(i, f)| build_card(format!("card-{i}"), f))
            .collect()
    })
}

fn request_strategy(local_only: bool) -> impl Strategy<Value = RequestProfile> {
    (prop::option::of(role_strategy()), any::<bool>()).prop_map(move |(role, allow_fallback)| {
        RequestProfile {
            task: TaskProfile {
                privacy: Some(if local_only {
                    PrivacyClass::LocalOnly
                } else {
                    PrivacyClass::Any
                }),
                intent: None,
                complexity: None,
                capabilities: Some(vec![Capability::Chat]),
                fallback: if allow_fallback {
                    Some(FallbackPolicy::NextInChain)
                } else {
                    None
                },
            },
            role,
            tenant_id: None,
            content_tightening: None,
        }
    })
}

proptest! {
    /// `local_only` 请求的决策结果（若成功）里，被选中的候选一定是有效 loopback
    /// 的——绝不会出现 `effective_served_locality` 判定为非 loopback 的候选。
    #[test]
    fn local_only_decisions_never_pick_a_non_loopback_candidate(
        cards in cards_strategy(),
        req in request_strategy(true),
    ) {
        let ctx = PolicyCtx::default();
        if let Ok(decision) = decide(&req, &cards, &ctx) {
            let chosen = cards.iter().find(|c| c.id() == decision.chosen_id);
            prop_assert!(chosen.is_some());
            if let Some(chosen) = chosen {
                prop_assert_eq!(effective_served_locality(chosen), Locality::Loopback);
            }
        }
    }

    /// 纯函数性质：同样的输入调用两次，必须得到完全一样的输出（`Decision`/`Rejection`
    /// 都要求 `PartialEq`）。
    #[test]
    fn decide_is_deterministic_across_repeated_calls(
        cards in cards_strategy(),
        req in request_strategy(false),
    ) {
        let ctx = PolicyCtx::default();
        let first = decide(&req, &cards, &ctx);
        let second = decide(&req, &cards, &ctx);
        prop_assert_eq!(first, second);
    }

    /// M4：打乱候选顺序（这里用反转——把所有两两相对顺序都倒过来，比单次
    /// 交换更容易暴露"结果偷偷依赖了 `Vec` 迭代顺序"这类 bug）不改变结果：
    /// `pick()` 的确定性 tie-break、`reason_codes` 的推导都不该看 `cards`
    /// 数组本身的物理顺序。
    #[test]
    fn shuffling_card_order_does_not_change_the_decision(
        cards in cards_strategy(),
        req in request_strategy(false),
    ) {
        let mut reversed = cards.clone();
        reversed.reverse();
        let ctx = PolicyCtx::default();
        prop_assert_eq!(decide(&req, &cards, &ctx), decide(&req, &reversed, &ctx));
    }

    /// M4：被选中的候选，价格一定是已知且非负的（不变式 #3）——`decide()`
    /// 绝不会选出一个 `estimated_cost_minor` 是 `None` 或负数的候选。
    #[test]
    fn chosen_candidate_price_is_always_known_and_non_negative(
        cards in cards_strategy(),
        req in request_strategy(false),
    ) {
        let ctx = PolicyCtx::default();
        if let Ok(decision) = decide(&req, &cards, &ctx) {
            let chosen = cards.iter().find(|c| c.id() == decision.chosen_id);
            prop_assert!(chosen.is_some());
            if let Some(chosen) = chosen {
                prop_assert!(chosen.estimated_cost_minor.is_some_and(|v| v >= 0));
            }
        }
    }

    /// M4：只要成功结果打上了"降级已发生"的原因码（角色降级或预算回落），
    /// `degradations` 就必须非空——不能有名无实的降级标记，也不能有隐瞒的
    /// 降级（三个变异防护测试已经在 pipeline::tests 里手工验证过这条不变式
    /// 的具体场景，这里是跨随机输入的通用性质）。
    #[test]
    fn a_fallback_reason_code_always_comes_with_a_non_empty_degradations_list(
        cards in cards_strategy(),
        req in request_strategy(false),
    ) {
        let ctx = PolicyCtx::default();
        if let Ok(decision) = decide(&req, &cards, &ctx) {
            let claims_fallback = decision.reason_codes.iter().any(|r| {
                matches!(
                    r,
                    ReasonCode::RoleFallbackCapabilityOnly | ReasonCode::BudgetFallbackToFreeCandidate
                )
            });
            if claims_fallback {
                prop_assert!(!decision.degradations.is_empty());
            }
        }
    }

    /// M4：内容检查的收紧结果只能让隐私更严格——`floor` 已经是 `LocalOnly`
    /// 时，无论 `content_tightening` 传什么，`effective_privacy` 都不能变成
    /// `Any`（对 `decide()` 用到的同一个公开函数做性质测试，而不仅是
    /// `privacy` 模块内部的单元测试）。
    #[test]
    fn content_tightening_never_loosens_a_local_only_floor(
        tightening in prop::option::of(prop_oneof![Just(PrivacyClass::Any), Just(PrivacyClass::LocalOnly)]),
    ) {
        prop_assert_eq!(
            effective_privacy(PrivacyClass::LocalOnly, tightening),
            PrivacyClass::LocalOnly
        );
    }

    /// M4：`tenant_id` + `ctx.budget` 都给了的时候，预算阶段必须真正跑过——
    /// 不能因为某处疏漏（比如忘了传 tenant_id）而悄悄跳过预算判断。成功结果
    /// 一定带有预算相关的 reason_code（不会是"没有租户上下文"那个），
    /// 402 本身也是预算路径生效的证据。
    #[test]
    fn tenant_and_budget_context_genuinely_enters_the_budget_stage(
        cards in cards_strategy(),
        role in prop::option::of(role_strategy()),
        allow_fallback in any::<bool>(),
        snapshot in budget_snapshot_strategy(),
    ) {
        let req = RequestProfile {
            task: TaskProfile {
                privacy: Some(PrivacyClass::Any),
                intent: None,
                complexity: None,
                capabilities: Some(vec![Capability::Chat]),
                fallback: if allow_fallback { Some(FallbackPolicy::NextInChain) } else { None },
            },
            role,
            tenant_id: Some("tenant-1".to_string()),
            content_tightening: None,
        };
        let budget = FixedBudget(snapshot);
        let ctx = PolicyCtx { min_ram_gb: None, budget: Some(&budget) };
        match decide(&req, &cards, &ctx) {
            Ok(decision) => {
                prop_assert!(!decision.reason_codes.contains(&ReasonCode::BudgetNoTenantContext));
                let has_budget_reason = decision.reason_codes.iter().any(|r| {
                    matches!(r, ReasonCode::BudgetWithinLimit | ReasonCode::BudgetFallbackToFreeCandidate)
                });
                prop_assert!(has_budget_reason);
            }
            Err(Rejection::BudgetExceeded { .. }) => {
                // 402 本身就是预算路径真正生效（而不是被跳过）的证明。
            }
            Err(_) => {
                // 更早的阶段（隐私/角色/价格/admission）就被拒绝了，还没轮到
                // 预算——不是这条性质要断言的范围。
            }
        }
    }
}
