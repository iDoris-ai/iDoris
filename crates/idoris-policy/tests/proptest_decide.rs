//! 性质测试（proptest）：决策管道的两条不变式。
//!
//! - `local_only` 的决策结果里永远不会出现非 loopback 的候选。
//! - 同一输入两次运行 `decide` 结果一致（纯函数、无 IO、无随机数）。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use idoris_contracts::{ComponentCard, TaskProfile};
use idoris_policy::{
    AdmissionStatus, Card, PolicyCtx, RequestProfile, Role, decide, effective_served_locality,
};
use proptest::prelude::*;

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
        prop::option::of(0i64..=1000),
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
}
