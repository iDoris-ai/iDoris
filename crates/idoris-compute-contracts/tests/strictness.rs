//! 未知字段、未知枚举值的行为：**一律拒绝**，没有任何地方容忍。
//!
//! 这与仓库现有契约一致：`idoris-contracts` 里每个对象都是 `#[serde(deny_unknown_fields)]`，TS 侧是 zod
//! `.strict()`（见 `crates/idoris-contracts/src/lib.rs` 的模块文档）。本测试对本 crate 每一个能反序列化的
//! 类型做三件事：往返相等；顶层多一个未知字段会被拒绝；每个枚举遇到未知取值、错误大小写、错误类型会被拒绝。
//!
//! 值得单独说明的一点：serde 对「内部标签枚举的单元变体」不检查未知字段（`{"state":"sending","x":1}` 会被
//! 悄悄接受）。`AttemptState` / `AttemptEvent` 因此走镜像类型解析，下面的测试专门盯着它们。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fmt::Debug;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, Allocation, Membership, MembershipRole, MembershipStatus,
    Principal, PrincipalStatus,
};
use idoris_compute_contracts::admission::{
    AdmissionDecision, Admitted, AncestorView, CheckKind, DegradeReason, GraceGrant, GraceKey,
    InFlightView, RateWindowKind, RateWindowView, Reservation,
};
use idoris_compute_contracts::attempt::{
    AttemptEvent, AttemptEventKind, AttemptState, AttemptStateKind, Certainty, SettleBasis,
};
use idoris_compute_contracts::control::{AuditOutcome, ControlReason, WriteResult};
use idoris_compute_contracts::credential::{
    AdminScope, ControlOp, Credential, CredentialBinding, CredentialStatus,
};
use idoris_compute_contracts::form::{
    BillingModel, CloudIdentity, DeployConfig, DeploymentForm, LedgerAuthority, ListenScope,
    TenantSource,
};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, AttemptId, AttemptIndex, BillingMonth, EndpointKey, ModelId,
    PriceVersion, PrincipalId, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::{Converted, FxRate, Money, Unit};
use idoris_compute_contracts::permit::{
    AccessPath, Billing, CapabilityKind, CapabilityNeeds, DataClass, Dpa, PermitTable,
    PermitTableFile, Region, Retention, RouteRequirements, TenantDataPolicy,
};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard, Usage};
use idoris_compute_contracts::reason::{ExperienceTier, ReasonCode};
use idoris_compute_contracts::request::{
    AdmittedCandidate, AttemptRole, ChargeBasis, CostBasis, Request, UserCharge, split_overage,
};
use idoris_compute_contracts::reserve::{MaxTokensSource, ReservePolicy, ReserveRequest, Reserved};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use support::*;

fn thb(micros: i64) -> Price {
    Price::from_micros(Unit::Thb, micros).unwrap()
}

fn usd(micros: i64) -> Cost {
    Cost::from_micros(Unit::Usd, micros).unwrap()
}

/// 往返相等；若序列化结果是对象，顶层多一个未知字段必须被拒绝。
fn assert_strict_object<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let name = std::any::type_name::<T>();
    let json = serde_json::to_value(value).unwrap();
    let back: T = serde_json::from_value(json.clone())
        .unwrap_or_else(|e| panic!("{name} does not round-trip: {e}\n{json}"));
    assert_eq!(&back, value, "{name} round trip changed the value");
    let Value::Object(map) = json else {
        panic!("{name} is not serialized as an object: {json}");
    };
    let mut with_extra = map;
    with_extra.insert("__unknown_field__".to_string(), json!(1));
    assert!(
        serde_json::from_value::<T>(Value::Object(with_extra)).is_err(),
        "{name} silently accepted an unknown field"
    );
}

/// 枚举（序列化成字符串）：每个合法取值往返；未知取值、错误大小写、空串、数字、null 都拒绝。
fn assert_strict_enum<T: Serialize + DeserializeOwned + PartialEq + Debug>(variants: &[T]) {
    let name = std::any::type_name::<T>();
    assert!(!variants.is_empty());
    for v in variants {
        let json = serde_json::to_value(v).unwrap();
        let text = json
            .as_str()
            .unwrap_or_else(|| panic!("{name} is not a string enum: {json}"));
        assert_eq!(
            &serde_json::from_value::<T>(json.clone()).unwrap(),
            v,
            "{name}"
        );
        let shouted = text.to_uppercase();
        let capitalized = {
            let mut c = text.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        };
        for bad in [
            json!("__not_a_variant__"),
            json!(""),
            json!(shouted),
            json!(capitalized),
            json!(format!(" {text}")),
            json!(1),
            json!(null),
            json!(true),
        ] {
            assert!(
                serde_json::from_value::<T>(bad.clone()).is_err(),
                "{name} accepted {bad} (valid variant is {text:?})"
            );
        }
    }
}

#[test]
fn money_pricing_and_grant_types_reject_unknown_fields() {
    assert_strict_object(&Money::new(Unit::Thb, 1));
    assert_strict_object(&thb(1));
    assert_strict_object(&usd(1));
    assert_strict_object(&Usage::new(10, 5, 2).unwrap());
    assert_strict_object(&RateCard::new(thb(1), thb(2), Some(thb(1))).unwrap());
    assert_strict_object(&RateCard::new(usd(1), usd(2), None).unwrap());
    assert_strict_object(&FxRate::new(Unit::Usd, Unit::Thb, 1, TimestampMs(0)).unwrap());
    let grant = Grant::new(
        Money::new(Unit::Thb, 1),
        Purpose::FreeTier,
        TimestampMs(0),
        TimestampMs(1),
    )
    .unwrap();
    assert_strict_object(&grant);
    let converted = Converted {
        amount: Money::new(Unit::Thb, 1),
        rate: FxRate::new(Unit::Usd, Unit::Thb, 1, TimestampMs(0)).unwrap(),
    };
    assert_strict_object(&converted);
}

#[test]
fn core_object_types_reject_unknown_fields() {
    assert_strict_object(&Account {
        id: aid("a"),
        kind: AccountKind::Org,
        parent: Some(aid("p")),
        unit: Unit::Thb,
        status: AccountStatus::Active,
    });
    assert_strict_object(&Principal {
        id: pid("p"),
        status: PrincipalStatus::Active,
    });
    assert_strict_object(&Membership {
        id: mid("m"),
        principal: pid("p"),
        org: aid("o"),
        role: MembershipRole::Member,
        account: Some(aid("a")),
        status: MembershipStatus::Active,
    });
    let grant = Grant::new(
        Money::new(Unit::Thb, 1),
        Purpose::FreeTier,
        TimestampMs(0),
        TimestampMs(1),
    )
    .unwrap();
    assert_strict_object(&Allocation {
        id: AllocationId::new("al").unwrap(),
        parent: aid("p"),
        child: aid("c"),
        ceiling: grant,
        idempotency_key: None,
    });
    for binding in [
        CredentialBinding::Service { account: aid("a") },
        CredentialBinding::Node { account: aid("a") },
        CredentialBinding::Member {
            membership: mid("m"),
        },
    ] {
        assert_strict_object(
            &Credential::new(cid("c"), binding, CredentialStatus::Active, None, vec![]).unwrap(),
        );
    }
    assert_strict_object(&AdminScope {
        op: ControlOp::GrantAllocation,
        subtree: aid("a"),
        max_amount: Some(Money::new(Unit::Thb, 1)),
    });
    // 账户树：数组，不是对象；多塞一个账户字段同样被拒绝
    let tree_json = serde_json::to_value(tree()).unwrap();
    let mut accounts = tree_json.as_array().unwrap().clone();
    accounts[0]["__unknown_field__"] = json!(1);
    assert!(
        serde_json::from_value::<idoris_compute_contracts::account::AccountTree>(Value::Array(
            accounts
        ))
        .is_err()
    );
    // 付款上下文
    assert_strict_object(&payment_context());
}

#[test]
fn admission_and_reserve_types_reject_unknown_fields() {
    let admitted = Admitted {
        endpoint: EndpointKey::new("p", "m", "e").unwrap(),
        payer: aid("s1"),
        effective_max_tokens: 1,
        reservation: Reservation {
            price: thb(1),
            cost: usd(1),
        },
    };
    assert_strict_object(&admitted);
    assert_strict_object(&admitted.reservation);
    assert_strict_object(&admitted.endpoint);
    for decision in [
        AdmissionDecision::Allow {
            admitted: admitted.clone(),
        },
        AdmissionDecision::Degrade {
            admitted: admitted.clone(),
            reason: DegradeReason::Budget,
        },
        AdmissionDecision::Grace { admitted },
        AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap(),
    ] {
        assert_strict_object(&decision);
    }
    let mut view = AncestorView::unbounded(aid("a"), Unit::Thb);
    view.balance = Some(Money::new(Unit::Thb, 1));
    assert_strict_object(&view);
    assert_strict_object(&RateWindowView {
        window: RateWindowKind::Hour,
        spent: Money::new(Unit::Thb, 0),
        cap: Money::new(Unit::Thb, 1),
    });
    assert_strict_object(&InFlightView {
        current: Money::new(Unit::Thb, 0),
        cap: Money::new(Unit::Thb, 1),
    });
    assert_strict_object(&GraceKey {
        account: aid("a"),
        period: BillingMonth::new(2026, 11).unwrap(),
    });
    assert_strict_object(&GraceGrant { shortfall: thb(1) });
    assert_strict_object(&ReservePolicy::new(10, 1).unwrap());
    assert_strict_object(&ReserveRequest {
        requested_max_tokens: Some(1),
        input_tokens_estimate: 1,
    });
    assert_strict_object(&Reserved {
        effective_max_tokens: 1,
        source: MaxTokensSource::AsRequested,
        amount: thb(1),
    });
}

#[test]
fn attempt_state_machine_types_reject_unknown_fields_including_unit_variants() {
    let usage = Usage::new(1, 1, 0).unwrap();
    for state in [
        AttemptState::Admitted,
        AttemptState::Sending,
        AttemptState::NotSent,
        AttemptState::MaybeExecuted,
        AttemptState::ExecutedUsageKnown { usage },
        AttemptState::ExecutedUsageUnknown,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount,
        },
        AttemptState::Settled {
            basis: SettleBasis::ActualUsage(usage),
        },
    ] {
        assert_strict_object(&state);
    }
    for event in [
        AttemptEvent::Dispatch,
        AttemptEvent::NeverLeftHost,
        AttemptEvent::WireWriteStarted,
        AttemptEvent::UpstreamRefused { http_status: 429 },
        AttemptEvent::UsageReported { usage },
        AttemptEvent::ExecutionObservedUsageUnknown,
        AttemptEvent::Settle,
    ] {
        assert_strict_object(&event);
    }
    assert!(serde_json::from_value::<AttemptState>(json!({"state": "retrying"})).is_err());
    assert!(serde_json::from_value::<AttemptEvent>(json!({"event": "retry"})).is_err());
    assert!(serde_json::from_value::<AttemptState>(json!({"state": "sending", "x": 1})).is_err());
    assert!(serde_json::from_value::<AttemptEvent>(json!({"event": "settle", "x": 1})).is_err());
}

#[test]
fn request_and_settlement_types_reject_unknown_fields() {
    let mut req = support_request();
    assert_strict_object(&req);
    let plan = {
        use idoris_compute_contracts::attempt::AttemptEvent as E;
        req.apply(AttemptIndex(0), E::Dispatch).unwrap();
        req.apply(AttemptIndex(0), E::NeverLeftHost).unwrap();
        req.plan_settlement().unwrap()
    };
    assert_strict_object(&plan);
    assert_strict_object(&plan.lines[0]);
    assert_strict_object(&UserCharge {
        attempt: AttemptId::new(RequestId::new("r").unwrap(), AttemptIndex(0)),
        charged: thb(1),
        basis: ChargeBasis::ActualUsage,
        reserved: thb(1),
        overage: thb(0),
    });
    assert_strict_object(&split_overage(&thb(10), &thb(4)).unwrap());
    // 请求记录里直接塞一个付款账户字段（试图指定付款方）：拒绝
    let mut json = serde_json::to_value(support_request()).unwrap();
    json["payer"] = json!("someone-else");
    assert!(serde_json::from_value::<Request>(json).is_err());
}

fn support_request() -> Request {
    use idoris_compute_contracts::pricing::RateCard;
    let admitted = Admitted {
        endpoint: EndpointKey::new("p", "m", "e").unwrap(),
        payer: aid("s1"),
        effective_max_tokens: 100,
        reservation: Reservation {
            price: thb(1_000),
            cost: usd(10),
        },
    };
    Request::admit(
        RequestId::new("req-1").unwrap(),
        payment_context(),
        AdmittedCandidate {
            decision: AdmissionDecision::Allow { admitted },
            price_version: PriceVersion::new("pv-1").unwrap(),
            cost_rates: RateCard::new(usd(1), usd(1), None).unwrap(),
            price_rates: RateCard::new(thb(1), thb(1), None).unwrap(),
        },
    )
    .unwrap()
}

#[test]
fn permit_table_types_reject_unknown_fields() {
    let yaml = include_str!("../testdata/endpoint-permit-table.example.yaml");
    let table = PermitTable::from_yaml_str(yaml).unwrap();
    let file: PermitTableFile = table.clone().into();
    assert_strict_object(&file);
    assert_strict_object(&table.endpoints()[0]);
    assert_strict_object(&table.endpoints()[0].capabilities);
    assert_strict_object(&CapabilityNeeds {
        tool_calling: true,
        ..Default::default()
    });
    let tenant = TenantDataPolicy::new(
        [DataClass::Adult].into_iter().collect(),
        None,
        Retention::Zdr,
        false,
    )
    .unwrap();
    assert_strict_object(&tenant);
    assert_strict_object(&RouteRequirements {
        form: DeploymentForm::Cloud,
        tenant,
        stream: false,
        billing: Billing::Paid,
        needs: CapabilityNeeds::default(),
    });
    // YAML：多余的键同样被拒绝（顶层、条目、能力）
    for (name, broken) in [
        ("top", format!("{yaml}extra: 1\n")),
        (
            "entry",
            yaml.replacen(
                "    access: api\n",
                "    access: api\n    enabled: true\n",
                1,
            ),
        ),
        (
            "capabilities",
            yaml.replacen(
                "      cancellation: true\n",
                "      cancellation: true\n      vision: true\n",
                1,
            ),
        ),
    ] {
        assert!(PermitTable::from_yaml_str(&broken).is_err(), "{name}");
    }
}

#[test]
fn form_types_reject_unknown_fields() {
    assert_strict_object(&DeploymentForm::Cloud.profile());
    assert_strict_object(&DeployConfig {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        form: Some(DeploymentForm::Cloud),
    });
    assert_strict_object(&DeployConfig {
        deploy_mode: idoris_contracts::DeployMode::Personal,
        form: None,
    });
}

#[test]
fn every_enum_rejects_unknown_variants_wrong_case_and_wrong_types() {
    assert_strict_enum(&ReasonCode::ALL);
    assert_strict_enum(&[
        ExperienceTier::Normal,
        ExperienceTier::Degraded,
        ExperienceTier::Grace,
        ExperienceTier::Exhausted,
    ]);
    assert_strict_enum(&[
        Purpose::FreeTier,
        Purpose::Prepaid,
        Purpose::CreditLine,
        Purpose::OrgQuota,
    ]);
    assert_strict_enum(&[
        AccountKind::Platform,
        AccountKind::SubsidyPool,
        AccountKind::Org,
        AccountKind::Sponsor,
        AccountKind::Member,
    ]);
    assert_strict_enum(&[
        AccountStatus::Active,
        AccountStatus::Frozen,
        AccountStatus::Closed,
    ]);
    assert_strict_enum(&[PrincipalStatus::Active, PrincipalStatus::Revoked]);
    assert_strict_enum(&[
        MembershipRole::Member,
        MembershipRole::Sponsor,
        MembershipRole::Admin,
    ]);
    assert_strict_enum(&[MembershipStatus::Active, MembershipStatus::Revoked]);
    assert_strict_enum(&[
        CredentialStatus::Active,
        CredentialStatus::Frozen,
        CredentialStatus::Revoked,
    ]);
    assert_strict_enum(&[
        ControlOp::CreateAccount,
        ControlOp::SetAccountStatus,
        ControlOp::ManageMembership,
        ControlOp::IssueCredential,
        ControlOp::RevokeCredential,
        ControlOp::FreezeCredential,
        ControlOp::GrantAllocation,
        ControlOp::CreatePrincipal,
        ControlOp::RevokePrincipal,
        ControlOp::Read,
    ]);
    assert_strict_enum(&ControlReason::ALL);
    assert_strict_enum(&AuditOutcome::ALL);
    assert_strict_enum(&WriteResult::ALL);
    assert_strict_enum(&CheckKind::ALL);
    assert_strict_enum(&[RateWindowKind::Minute, RateWindowKind::Hour]);
    assert_strict_enum(&[
        DegradeReason::Budget,
        DegradeReason::Capacity,
        DegradeReason::UpstreamFailover,
    ]);
    assert_strict_enum(&[ChargeBasis::ActualUsage, ChargeBasis::ReservedAmount]);
    assert_strict_enum(&[
        CostBasis::NotIncurred,
        CostBasis::ActualUsage,
        CostBasis::ReservedPendingReconciliation,
    ]);
    assert_strict_enum(&[AttemptRole::Answering, AttemptRole::Extra]);
    assert_strict_enum(&[
        Certainty::NotExecuted,
        Certainty::Maybe,
        Certainty::Executed,
    ]);
    assert_strict_enum(&AttemptStateKind::ALL);
    assert_strict_enum(&AttemptEventKind::ALL);
    assert_strict_enum(&[
        MaxTokensSource::AsRequested,
        MaxTokensSource::ClampedToHardCap,
        MaxTokensSource::DefaultedToHardCap,
        MaxTokensSource::ReducedToFit,
    ]);
    assert_strict_enum(&DeploymentForm::ALL);
    assert_strict_enum(&[ListenScope::Loopback, ListenScope::Lan, ListenScope::Public]);
    assert_strict_enum(&[
        BillingModel::None,
        BillingModel::Quota,
        BillingModel::Currency,
    ]);
    assert_strict_enum(&[
        CloudIdentity::PersonalCloudAccount,
        CloudIdentity::OrgPaymentAccountOnly,
        CloudIdentity::NotApplicable,
    ]);
    assert_strict_enum(&[
        LedgerAuthority::Local,
        LedgerAuthority::OrgInternal,
        LedgerAuthority::CloudLedger,
    ]);
    assert_strict_enum(&[
        TenantSource::FixedPersonal,
        TenantSource::TenantHeader,
        TenantSource::FromCredential,
    ]);
    assert_strict_enum(&[DataClass::ChildEdu, DataClass::Adult, DataClass::Public]);
    assert_strict_enum(&[
        Retention::Zdr,
        Retention::WithExceptions,
        Retention::Retained,
    ]);
    assert_strict_enum(&[Dpa::Signed, Dpa::Unsigned]);
    assert_strict_enum(&[AccessPath::Api, AccessPath::Subscription]);
    assert_strict_enum(&[Billing::Paid, Billing::Subsidized, Billing::Unbilled]);
    assert_strict_enum(&[
        CapabilityKind::ToolCalling,
        CapabilityKind::StreamUsage,
        CapabilityKind::CachePricing,
        CapabilityKind::StructuredOutput,
        CapabilityKind::Cancellation,
    ]);
    // 带校验的字符串类型：region（Unit 的大小写规则另有专门测试，见 money.rs）
    assert_strict_enum(&[Region::new("sg-1").unwrap()]);
}

#[test]
fn validated_string_types_reject_garbage() {
    for bad in [
        json!(""),
        json!("a b"),
        json!("a@b"),
        json!(1),
        json!(null),
        json!(["a"]),
    ] {
        assert!(
            serde_json::from_value::<AccountId>(bad.clone()).is_err(),
            "{bad}"
        );
        assert!(
            serde_json::from_value::<PrincipalId>(bad.clone()).is_err(),
            "{bad}"
        );
        assert!(
            serde_json::from_value::<RequestId>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    assert!(serde_json::from_value::<ModelId>(json!("a b")).is_err());
    assert!(serde_json::from_value::<AttemptId>(json!("no-index")).is_err());
    assert!(serde_json::from_value::<BillingMonth>(json!("2026-13")).is_err());
    assert!(serde_json::from_value::<PriceVersion>(json!("")).is_err());
    assert!(serde_json::from_value::<Region>(json!("USA")).is_err());
}
