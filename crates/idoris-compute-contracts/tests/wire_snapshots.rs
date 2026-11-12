//! 线上形状快照：用固定的 JSON / YAML 文件钉住所有对外类型的序列化形状和全部原因码字符串。
//!
//! 快照在 `testdata/snapshots/`。任何字段名、取值、结构的变化都会让这里失败；有意改动时用
//! `UPDATE_SNAPSHOTS=1 cargo test -p idoris-compute-contracts --test wire_snapshots` 重新生成，
//! 并逐行审阅 diff：线上字符串一经确定不可变，改它们等于破坏契约。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use idoris_compute_contracts::account::{
    Allocation, Membership, MembershipRole, MembershipStatus, Principal, PrincipalStatus,
    subsidy_idempotency_key,
};
use idoris_compute_contracts::admission::{
    AdmissionDecision, Admitted, AncestorView, DegradeReason, InFlightView, RateWindowKind,
    RateWindowView, Reservation, check_grace, evaluate_admission,
};
use idoris_compute_contracts::attempt::{
    AttemptEvent, AttemptEventKind, AttemptState, AttemptStateKind, SettleBasis, transition,
};
use idoris_compute_contracts::credential::{
    AdminScope, ControlOp, Credential, CredentialBinding, CredentialStatus, resolve_payment_context,
};
use idoris_compute_contracts::form::{DeployConfig, DeploymentForm};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AttemptIndex, EndpointKey, PriceVersion, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::{FxRate, Money, Rounding, Unit, convert};
use idoris_compute_contracts::permit::{PermitTable, RouteRequirements, TenantDataPolicy};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard, Usage, usage_amount};
use idoris_compute_contracts::reason::{ExperienceTier, ReasonCode};
use idoris_compute_contracts::request::{AdmittedCandidate, Request, settle_attempts};
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest, fit_max_tokens, reserve};
use serde_json::json;
use support::*;

const EXAMPLE_PERMIT_YAML: &str = include_str!("../testdata/endpoint-permit-table.example.yaml");

fn thb(micros: i64) -> Price {
    Price::from_micros(Unit::Thb, micros).unwrap()
}

fn usd(micros: i64) -> Cost {
    Cost::from_micros(Unit::Usd, micros).unwrap()
}

#[test]
fn units_money_pricing_and_fx() {
    let quota = quota_unit("mac-studio-1");
    let price_rates =
        RateCard::new(thb(10_000_000), thb(40_000_000), Some(thb(1_000_000))).unwrap();
    let usage = Usage::new(1_000, 200, 100).unwrap();
    let fx = FxRate::new(Unit::Usd, Unit::Thb, 35_500_000_000, TimestampMs(1_000)).unwrap();
    let converted = convert(
        &Money::new(Unit::Usd, 1_234_567),
        &fx,
        TimestampMs(1_000),
        0,
        Rounding::HalfAwayFromZero,
    )
    .unwrap();
    assert_snapshot(
        "money_units_pricing_fx",
        &json!({
            "units": [Unit::Thb, Unit::Usd, quota.clone()],
            "money": Money::new(quota.clone(), -1_500_000),
            "display": [
                Money::new(Unit::Thb, 1_235_000).display(),
                Money::new(Unit::Thb, -5_000).display(),
                Money::new(quota, 1_500_000).display(),
            ],
            "display_text": [
                Money::new(Unit::Thb, 1_235_000).display().to_string(),
                Money::new(Unit::Usd, 4_999).display().to_string(),
            ],
            "cost": usd(364),
            "price": thb(18_000),
            "rate_card": price_rates,
            "usage": usage,
            "usage_price": usage_amount(&price_rates, &usage, Rounding::Ceil).unwrap(),
            "fx_rate": fx,
            "converted": converted,
        }),
    );
}

#[test]
fn grants_and_allocations() {
    let grant = Grant::new(
        Money::new(Unit::Thb, 5_000_000),
        Purpose::FreeTier,
        TimestampMs(1_793_836_800_000),
        TimestampMs(1_796_428_800_000),
    )
    .unwrap();
    let key = subsidy_idempotency_key(&pid("kid1"), "2026-11".parse().unwrap()).unwrap();
    let allocation = Allocation {
        id: idoris_compute_contracts::ids::AllocationId::new("alloc-1").unwrap(),
        parent: aid("pool"),
        child: aid("eduai"),
        ceiling: grant.clone(),
        idempotency_key: Some(key),
    };
    assert_snapshot(
        "grant_allocation",
        &json!({
            "purposes": [Purpose::FreeTier, Purpose::Prepaid, Purpose::CreditLine, Purpose::OrgQuota],
            "grant": grant,
            "allocation": allocation,
        }),
    );
}

#[test]
fn accounts_memberships_and_directory_pieces() {
    assert_snapshot(
        "account_tree",
        &json!({
            "tree": tree(),
            "principal": Principal { id: pid("kid1"), status: PrincipalStatus::Active },
            "membership": Membership {
                id: mid("m-kid1"),
                principal: pid("kid1"),
                org: aid("eduai"),
                role: MembershipRole::Member,
                account: Some(aid("s1")),
                status: MembershipStatus::Active,
            },
            "roles": [MembershipRole::Member, MembershipRole::Sponsor, MembershipRole::Admin],
        }),
    );
}

#[test]
fn credentials_and_payment_contexts() {
    let dir = directory();
    let service = Credential::new(
        cid("c-edu"),
        CredentialBinding::Service {
            account: aid("eduai"),
        },
        CredentialStatus::Active,
        Some(TimestampMs(1_800_000_000_000)),
        vec![
            AdminScope {
                op: ControlOp::GrantAllocation,
                subtree: aid("eduai"),
                max_amount: Some(Money::new(Unit::Thb, 10_000_000)),
            },
            AdminScope {
                op: ControlOp::CreateAccount,
                subtree: aid("eduai"),
                max_amount: None,
            },
        ],
    )
    .unwrap();
    let node = credential(
        "c-node",
        CredentialBinding::Node {
            account: aid("eduai"),
        },
    );
    let member = credential(
        "c-kid1",
        CredentialBinding::Member {
            membership: mid("m-kid1"),
        },
    );
    let kid1 = pid("kid1");
    let via_service = resolve_payment_context(&dir, &service, Some(&kid1), TimestampMs(0)).unwrap();
    let via_node = resolve_payment_context(&dir, &node, None, TimestampMs(0)).unwrap();
    let via_member = resolve_payment_context(&dir, &member, None, TimestampMs(0)).unwrap();
    assert_snapshot(
        "credentials_payment_contexts",
        &json!({
            "service": service,
            "node": node,
            "member": member,
            "statuses": [CredentialStatus::Active, CredentialStatus::Frozen, CredentialStatus::Revoked],
            "control_ops": [
                ControlOp::CreateAccount, ControlOp::SetAccountStatus, ControlOp::ManageMembership,
                ControlOp::IssueCredential, ControlOp::RevokeCredential, ControlOp::FreezeCredential,
                ControlOp::GrantAllocation
            ],
            "payment_context_via_service": via_service,
            "payment_context_via_node": via_node,
            "payment_context_via_member": via_member,
        }),
    );
}

#[test]
fn every_reason_code_string_and_the_tier_ladder() {
    let codes: Vec<&str> = ReasonCode::ALL.iter().map(|c| c.as_str()).collect();
    let tiers: serde_json::Value = [
        ExperienceTier::Normal,
        ExperienceTier::Degraded,
        ExperienceTier::Grace,
        ExperienceTier::Exhausted,
    ]
    .iter()
    .map(|t| {
        (
            serde_json::to_value(t)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string(),
            json!(
                t.reason_codes()
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
            ),
        )
    })
    .collect::<serde_json::Map<_, _>>()
    .into();
    assert_snapshot("reason_codes", &json!({ "codes": codes, "tiers": tiers }));
}

#[test]
fn admission_report_decisions_and_reservation() {
    // 学生 s1：授权额度 120，授信 30；老师 teacher 余额 500；租户 eduai 有在途上限与分钟速率
    let mut s1 = AncestorView::unbounded(aid("s1"), Unit::Thb);
    s1.ceiling_remaining = Some(Money::new(Unit::Thb, 120));
    s1.credit_limit = Money::new(Unit::Thb, 30);
    let mut teacher = AncestorView::unbounded(aid("teacher"), Unit::Thb);
    teacher.balance = Some(Money::new(Unit::Thb, 500));
    let mut eduai = AncestorView::unbounded(aid("eduai"), Unit::Thb);
    eduai.in_flight = Some(InFlightView {
        current: Money::new(Unit::Thb, 10),
        cap: Money::new(Unit::Thb, 1_000),
    });
    eduai.rate_windows = vec![RateWindowView {
        window: RateWindowKind::Minute,
        spent: Money::new(Unit::Thb, 40),
        cap: Money::new(Unit::Thb, 300),
    }];
    let mut pool = AncestorView::unbounded(aid("pool"), Unit::Thb);
    pool.subsidy_pool_remaining = Some(Money::new(Unit::Thb, 9_000));
    let report = evaluate_admission(&thb(140), &[s1, teacher, eduai, pool]).unwrap();
    assert!(report.passed() && report.uses_credit());

    let admitted = Admitted {
        endpoint: EndpointKey::new("example-zdr-host", "example-org/chat-70b", "openai-compat")
            .unwrap(),
        payer: aid("s1"),
        effective_max_tokens: 400,
        reservation: Reservation {
            price: thb(588),
            cost: usd(300),
        },
    };
    let decisions = vec![
        AdmissionDecision::Allow {
            admitted: admitted.clone(),
        },
        AdmissionDecision::Degrade {
            admitted: admitted.clone(),
            reason: DegradeReason::Budget,
        },
        AdmissionDecision::Degrade {
            admitted: admitted.clone(),
            reason: DegradeReason::Capacity,
        },
        AdmissionDecision::Degrade {
            admitted: admitted.clone(),
            reason: DegradeReason::UpstreamFailover,
        },
        AdmissionDecision::Grace { admitted },
        AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap(),
        AdmissionDecision::reject(ReasonCode::NoCompliantEndpoint).unwrap(),
    ];

    let policy = ReservePolicy::new(4_096, 16).unwrap();
    let rates = RateCard::new(thb(280_000), thb(420_000), None).unwrap();
    let request = ReserveRequest {
        requested_max_tokens: Some(4_000),
        input_tokens_estimate: 1_500,
    };
    let plain = reserve(&policy, &request, &rates).unwrap();
    let fitted = fit_max_tokens(&policy, &request, &rates, &thb(587)).unwrap();
    let grace = check_grace(0, &Money::new(Unit::Thb, 30), &thb(80)).unwrap();

    assert_snapshot(
        "admission",
        &json!({
            "report": report,
            "decisions": decisions,
            "reserve": plain,
            "reserve_reduced_to_fit": fitted,
            "grace_grant": grace,
        }),
    );
}

#[test]
fn attempt_state_machine_transition_table() {
    let usage = Usage::new(100, 50, 10).unwrap();
    let state_of = |kind: AttemptStateKind| match kind {
        AttemptStateKind::Admitted => AttemptState::Admitted,
        AttemptStateKind::Sending => AttemptState::Sending,
        AttemptStateKind::NotSent => AttemptState::NotSent,
        AttemptStateKind::MaybeExecuted => AttemptState::MaybeExecuted,
        AttemptStateKind::ExecutedUsageKnown => AttemptState::ExecutedUsageKnown { usage },
        AttemptStateKind::ExecutedUsageUnknown => AttemptState::ExecutedUsageUnknown,
        AttemptStateKind::Settled => AttemptState::Settled {
            basis: SettleBasis::ReservedAmount,
        },
    };
    let event_of = |kind: AttemptEventKind| match kind {
        AttemptEventKind::Dispatch => AttemptEvent::Dispatch,
        AttemptEventKind::NeverLeftHost => AttemptEvent::NeverLeftHost,
        AttemptEventKind::WireWriteStarted => AttemptEvent::WireWriteStarted,
        AttemptEventKind::UpstreamRefused => AttemptEvent::UpstreamRefused { http_status: 429 },
        AttemptEventKind::UsageReported => AttemptEvent::UsageReported { usage },
        AttemptEventKind::ExecutionObservedUsageUnknown => {
            AttemptEvent::ExecutionObservedUsageUnknown
        }
        AttemptEventKind::Settle => AttemptEvent::Settle,
    };
    let mut rows = Vec::new();
    for from in AttemptStateKind::ALL {
        for event in AttemptEventKind::ALL {
            let to = transition(&state_of(from), &event_of(event))
                .ok()
                .map(|s| s.kind());
            rows.push(json!({ "from": from, "event": event, "to": to }));
        }
    }
    assert_eq!(rows.len(), 49);
    assert_snapshot("attempt_transition_table", &rows);
}

#[test]
fn request_lifecycle_with_failover_and_settlement() {
    let payment = payment_context();
    let candidate = |model: &str, reason: Option<DegradeReason>| {
        let admitted = Admitted {
            endpoint: EndpointKey::new("example-zdr-host", model, "openai-compat").unwrap(),
            payer: aid("s1"),
            effective_max_tokens: 400,
            reservation: Reservation {
                price: thb(20_000),
                cost: usd(400),
            },
        };
        AdmittedCandidate {
            decision: match reason {
                None => AdmissionDecision::Allow { admitted },
                Some(reason) => AdmissionDecision::Degrade { admitted, reason },
            },
            price_version: PriceVersion::new("pv-2026-11-01").unwrap(),
            cost_rates: RateCard::new(usd(280_000), usd(420_000), None).unwrap(),
            price_rates: RateCard::new(thb(10_000_000), thb(40_000_000), None).unwrap(),
        }
    };
    let mut req = Request::admit(
        RequestId::new("req-1").unwrap(),
        payment,
        candidate("example-org/chat-70b", None),
    )
    .unwrap();
    // 第 0 次：发出去之后超时，没有确认 -> 停在 maybe_executed
    req.apply(AttemptIndex(0), AttemptEvent::Dispatch).unwrap();
    req.apply(AttemptIndex(0), AttemptEvent::WireWriteStarted)
        .unwrap();
    // 首字节之前切换：重新准入（降级原因 upstream_failover）
    req.begin_next_attempt(candidate(
        "example-org/chat-70b-b",
        Some(DegradeReason::UpstreamFailover),
    ))
    .unwrap();
    for event in [
        AttemptEvent::Dispatch,
        AttemptEvent::WireWriteStarted,
        AttemptEvent::UsageReported {
            usage: Usage::new(1_000, 200, 0).unwrap(),
        },
    ] {
        req.apply(AttemptIndex(1), event).unwrap();
    }
    req.publish_first_byte(AttemptIndex(1)).unwrap();
    req.apply(AttemptIndex(1), AttemptEvent::Settle).unwrap();
    let plan = settle_attempts(req.id(), req.attempts(), req.published()).unwrap();
    assert_snapshot(
        "request_lifecycle",
        &json!({ "request": req, "settlement_plan": plan }),
    );
}

#[test]
fn endpoint_permit_table_in_json_and_canonical_yaml() {
    let table = PermitTable::from_yaml_str(EXAMPLE_PERMIT_YAML).unwrap();
    assert_snapshot("permit_table", &table);
    assert_text_snapshot(
        "permit_table.canonical.yaml",
        &serde_yaml::to_string(&table).unwrap(),
    );
    // 路由请求与租户策略的形状
    let tenant = TenantDataPolicy::new(
        [idoris_compute_contracts::permit::DataClass::ChildEdu]
            .into_iter()
            .collect(),
        Some(
            [idoris_compute_contracts::permit::Region::new("us").unwrap()]
                .into_iter()
                .collect(),
        ),
        idoris_compute_contracts::permit::Retention::Zdr,
        true,
    )
    .unwrap();
    let requirements = RouteRequirements {
        form: DeploymentForm::Cloud,
        tenant,
        stream: false,
        billing: idoris_compute_contracts::permit::Billing::Subsidized,
        needs: idoris_compute_contracts::permit::CapabilityNeeds {
            tool_calling: true,
            ..Default::default()
        },
    };
    assert_snapshot("route_requirements", &requirements);
}

#[test]
fn deployment_forms_profiles_and_deploy_config() {
    let profiles: serde_json::Map<String, serde_json::Value> = DeploymentForm::ALL
        .iter()
        .map(|f| {
            (
                f.as_str().to_string(),
                serde_json::to_value(f.profile()).unwrap(),
            )
        })
        .collect();
    assert_snapshot(
        "deployment_forms",
        &json!({
            "forms": DeploymentForm::ALL,
            "profiles": profiles,
            "deploy_config_existing_shape": serde_json::from_str::<DeployConfig>(r#"{"deploy_mode":"tenant"}"#).unwrap(),
            "deploy_config_with_form": serde_json::from_str::<DeployConfig>(r#"{"deploy_mode":"tenant","form":"cloud"}"#).unwrap(),
        }),
    );
}
