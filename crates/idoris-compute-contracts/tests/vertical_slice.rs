//! 一条纵向路径：从凭证到结算，只用公开 API，看 S0 的各块拼得起来（设计稿 §13 的纵向验收，
//! 只取其中纯类型层面能表达的部分；存储、并发、路由接线是 S1–S7 的事）。
//!
//! 场景：泰国班级里的学生 kid1 通过 eduai 后端提问（eduai 的服务凭证提交假名成员 id），走免费额度，
//! 租户是 `child_edu`；第一个候选超时、未确认，首字节之前切换到第二个候选，完成；最后撤销凭证，再调用被拒。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use idoris_compute_contracts::account::{
    Allocation, AllocationError, subsidy_idempotency_key, validate_allocation,
};
use idoris_compute_contracts::admission::{
    AdmissionDecision, Admitted, AncestorView, DegradeReason, Reservation, evaluate_admission,
};
use idoris_compute_contracts::attempt::AttemptEvent;
use idoris_compute_contracts::credential::{
    Credential, CredentialBinding, CredentialStatus, ResolveError, resolve_payment_context,
};
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AllocationId, AttemptIndex, PriceVersion, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::permit::{
    Billing, CapabilityNeeds, DataClass, PermitTable, Retention, RouteRequirements,
    TenantDataPolicy, select_compliant,
};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard, Usage};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::request::{
    AdmittedCandidate, AttemptRole, ChargeBasis, CostBasis, Request, split_overage,
};
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest, fit_max_tokens, reserve};
use support::*;

const PERMIT_YAML: &str = include_str!("../testdata/endpoint-permit-table.example.yaml");
const MONTH_START: i64 = 1_793_836_800_000; // 2026-11-05T00:00:00Z
const MONTH_END: i64 = 1_796_428_800_000; // 2026-12-05T00:00:00Z（示例窗口，不必是自然月）
const NOW: TimestampMs = TimestampMs(MONTH_START + 86_400_000);

fn thb(micros: i64) -> Price {
    Price::from_micros(Unit::Thb, micros).unwrap()
}

fn usd(micros: i64) -> Cost {
    Cost::from_micros(Unit::Usd, micros).unwrap()
}

fn grant(micros: i64) -> Grant {
    Grant::new(
        Money::new(Unit::Thb, micros),
        Purpose::FreeTier,
        TimestampMs(MONTH_START),
        TimestampMs(MONTH_END),
    )
    .unwrap()
}

fn allocation(id: &str, parent: &str, child: &str, micros: i64) -> Allocation {
    Allocation {
        id: AllocationId::new(id).unwrap(),
        parent: aid(parent),
        child: aid(child),
        ceiling: grant(micros),
        idempotency_key: None,
    }
}

#[test]
fn a_students_free_tier_request_from_credential_to_settlement() {
    let dir = directory();
    let tree = dir.tree();

    // ---- 1. 额度委派链：平台 → 补贴池 → eduai → 老师 → 学生（授权上限，不是转移余额）----
    let mut allocations: Vec<Allocation> = Vec::new();
    let chain = [
        allocation("a-pool", "platform", "pool", 1_000_000_000),
        allocation("a-edu", "pool", "eduai", 100_000_000),
        allocation("a-teacher", "eduai", "teacher", 20_000_000),
        {
            let mut a = allocation("a-s1", "teacher", "s1", 5_000_000);
            a.idempotency_key =
                Some(subsidy_idempotency_key(&pid("kid1"), "2026-11".parse().unwrap()).unwrap());
            a
        },
    ];
    for a in chain {
        validate_allocation(tree, &allocations, &a).unwrap();
        allocations.push(a);
    }
    // 下发不能超过授权者本人：老师给学生再发 21 泰铢（老师自己只有 20）被拒绝
    assert!(matches!(
        validate_allocation(
            tree,
            &allocations,
            &allocation("a-too-much", "teacher", "s1", 21_000_000)
        )
        .unwrap_err(),
        AllocationError::ExceedsGrantor { .. }
    ));
    assert!(matches!(
        validate_allocation(
            tree,
            &allocations,
            &allocation("a-too-much", "eduai", "teacher", 100_000_001)
        )
        .unwrap_err(),
        AllocationError::ExceedsGrantor { .. }
    ));
    // 同一个主体同一个月的免费额度发放是幂等的：第二次用同一个键被拒绝
    let mut again = allocation("a-s1-again", "teacher", "s1", 1);
    again.idempotency_key =
        Some(subsidy_idempotency_key(&pid("kid1"), "2026-11".parse().unwrap()).unwrap());
    assert!(matches!(
        validate_allocation(tree, &allocations, &again).unwrap_err(),
        AllocationError::DuplicateIdempotencyKey(_)
    ));

    // ---- 2. 付款上下文：由凭证决定，请求只能提交经校验的成员 id ----
    let service = credential(
        "c-edu",
        CredentialBinding::Service {
            account: aid("eduai"),
        },
    );
    let kid1 = pid("kid1");
    let payment = resolve_payment_context(&dir, &service, Some(&kid1), NOW).unwrap();
    assert_eq!(payment.payer(), &aid("s1"));
    assert_eq!(payment.unit(), &Unit::Thb);
    // 同一个服务凭证提交别的学生，付款账户就是别的学生的；提交不在范围内的主体被拒绝
    let kid2 = pid("kid2");
    assert_eq!(
        resolve_payment_context(&dir, &service, Some(&kid2), NOW)
            .unwrap()
            .payer(),
        &aid("s2")
    );
    let outsider = pid("adult");
    assert_eq!(
        resolve_payment_context(&dir, &service, Some(&outsider), NOW).unwrap_err(),
        ResolveError::MemberNotInScope
    );

    // ---- 3. 路由硬约束：child_edu 租户，云形态，免费额度（补贴），非流式 ----
    let table = PermitTable::from_yaml_str(PERMIT_YAML).unwrap();
    let tenant = TenantDataPolicy::new(
        [DataClass::ChildEdu].into_iter().collect(),
        None,
        Retention::Retained, // 租户自己写得再松，含 child_edu 仍固定 zdr + 已签 DPA
        false,
    )
    .unwrap();
    let requirements = RouteRequirements {
        form: DeploymentForm::Cloud,
        tenant,
        stream: false,
        billing: Billing::Subsidized,
        needs: CapabilityNeeds::default(),
    };
    // 别名背后的候选（偏好顺序）：官方 API 不合规，订阅路径云端排除，过期的被停用
    let candidates = [
        endpoint("example-official", "example-org/chat-70b", "official-api"),
        endpoint("example-cli", "example-org/chat-70b", "subscription-cli"),
        endpoint(
            "example-stale-host",
            "example-org/chat-70b",
            "openai-compat",
        ),
        endpoint("example-router", "example-org/chat-70b", "zdr-route"),
        endpoint("example-zdr-host", "example-org/chat-70b", "openai-compat"),
    ];
    let compliant = select_compliant(&table, &candidates, &requirements, NOW).unwrap();
    let keys: Vec<_> = compliant.iter().map(|e| e.key()).collect();
    assert_eq!(keys, vec![candidates[3].clone(), candidates[4].clone()]);
    for e in compliant.iter() {
        assert_eq!(e.retention, Retention::Zdr);
    }
    // 没有合规候选就拒绝，而不是空列表
    let none = select_compliant(&table, &candidates[..3], &requirements, NOW).unwrap_err();
    assert_eq!(none.reason_code(), ReasonCode::NoCompliantEndpoint);

    // ---- 4. 预留与准入：沿付款账户的所有祖先同时检查 ----
    let price_rates = RateCard::new(thb(280_000), thb(420_000), None).unwrap();
    let cost_rates = RateCard::new(usd(280_000), usd(420_000), None).unwrap();
    let policy = ReservePolicy::new(4_096, 16).unwrap();
    let request = ReserveRequest {
        requested_max_tokens: Some(4_000),
        input_tokens_estimate: 1_500,
    };
    let wanted = reserve(&policy, &request, &price_rates).unwrap();
    assert_eq!(wanted.amount.micros(), 2_100);

    // 学生 s1 这个月只剩 1_200 微泰铢（授权上限用掉了大半）；其余祖先充裕
    let views: Vec<AncestorView> = tree
        .ancestors(payment.payer())
        .unwrap()
        .iter()
        .map(|account| {
            let mut v = AncestorView::unbounded(account.id.clone(), Unit::Thb);
            match account.id.as_str() {
                "s1" => v.ceiling_remaining = Some(Money::new(Unit::Thb, 1_200)),
                "teacher" => v.balance = Some(Money::new(Unit::Thb, 9_000_000)),
                "pool" => v.subsidy_pool_remaining = Some(Money::new(Unit::Thb, 50_000_000)),
                _ => {}
            }
            v
        })
        .collect();
    let report = evaluate_admission(&wanted.amount, &views).unwrap();
    assert!(
        !report.passed(),
        "按 max_tokens=4000 预留 2100 超出学生剩余的 1200"
    );
    assert!(
        report.only_budget_blocked(),
        "只有钱不够：可以先压低 max_tokens，而不是拒绝"
    );
    assert_eq!(report.blocking_codes(), vec![ReasonCode::BudgetExhausted]);
    // 先自动压低 max_tokens：取所有祖先里最紧的上限
    let headroom = report.headroom().unwrap().unwrap();
    assert_eq!(headroom.micros(), 1_200);
    let fitted = fit_max_tokens(&policy, &request, &price_rates, &headroom)
        .unwrap()
        .unwrap();
    assert!(fitted.effective_max_tokens < 4_000 && fitted.effective_max_tokens >= 16);
    assert!(fitted.amount.micros() <= 1_200);
    assert!(evaluate_admission(&fitted.amount, &views).unwrap().passed());
    // 同样的预留放到成本侧：另一个类型，挂账用
    let cost_reserved = reserve(
        &policy,
        &ReserveRequest {
            requested_max_tokens: Some(fitted.effective_max_tokens),
            ..request
        },
        &cost_rates,
    )
    .unwrap();

    // ---- 5. 请求：第一个候选发出去后超时（maybe_executed），首字节之前切换，重新准入 ----
    let candidate = |idx: usize, reason: Option<DegradeReason>| {
        let admitted = Admitted {
            endpoint: keys[idx].clone(),
            payer: payment.payer().clone(),
            effective_max_tokens: fitted.effective_max_tokens,
            reservation: Reservation {
                price: fitted.amount.clone(),
                cost: cost_reserved.amount.clone(),
            },
        };
        AdmittedCandidate {
            decision: match reason {
                Some(reason) => AdmissionDecision::Degrade { admitted, reason },
                None => AdmissionDecision::Allow { admitted },
            },
            price_version: PriceVersion::new("pv-1").unwrap(),
            cost_rates: cost_rates.clone(),
            price_rates: price_rates.clone(),
        }
    };
    // 额度紧所以第一个候选就是降级过的（压低了 max_tokens）
    let mut req = Request::admit(
        RequestId::new("req-1").unwrap(),
        payment.clone(),
        candidate(0, Some(DegradeReason::Budget)),
    )
    .unwrap();
    req.apply(AttemptIndex(0), AttemptEvent::Dispatch).unwrap();
    req.apply(AttemptIndex(0), AttemptEvent::WireWriteStarted)
        .unwrap(); // 字节已出去，之后超时
    // 切换：必须带对新候选的准入（拒绝的决定进不来）
    let rejected = AdmittedCandidate {
        decision: AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap(),
        ..candidate(1, None)
    };
    assert!(req.begin_next_attempt(rejected).is_err());
    req.begin_next_attempt(candidate(1, Some(DegradeReason::UpstreamFailover)))
        .unwrap();
    let usage = Usage::new(600, 80, 0).unwrap();
    for event in [
        AttemptEvent::Dispatch,
        AttemptEvent::WireWriteStarted,
        AttemptEvent::UsageReported { usage },
    ] {
        req.apply(AttemptIndex(1), event).unwrap();
    }
    req.publish_first_byte(AttemptIndex(1)).unwrap();
    // 发布边界：现在起不能再切换
    assert!(req.begin_next_attempt(candidate(0, None)).is_err());
    req.apply(AttemptIndex(1), AttemptEvent::Settle).unwrap();

    // ---- 6. 结算：用户只付一次；第一个候选的成本按预留额挂账、待核销，记给平台 ----
    let plan = req.plan_settlement().unwrap();
    let charge = plan.user_charge.as_ref().unwrap();
    assert_eq!(charge.attempt.index(), AttemptIndex(1));
    assert_eq!(charge.basis, ChargeBasis::ActualUsage);
    // 600 入 × 0.28 + 80 出 × 0.42 = 168 + 33.6 = 201.6 -> 202 微泰铢
    assert_eq!(charge.charged.micros(), 202);
    assert_eq!(charge.overage.micros(), 0);
    let first = &plan.lines[0];
    assert_eq!(first.role, AttemptRole::Extra);
    assert_eq!(first.cost_basis, CostBasis::ReservedPendingReconciliation);
    assert_eq!(first.cost, cost_reserved.amount);
    assert_eq!(plan.pending_reconciliation().len(), 1);
    assert_eq!(
        plan.platform_absorbed_by_unit().unwrap()[&Unit::Usd],
        cost_reserved.amount
    );
    // 第一次尝试占用学生额度的价预留整笔释放（用户不为它付钱）
    assert_eq!(first.price_reservation_released, fitted.amount);

    // overage 的拆分示例：超出预留 500，担保人授信内还能吸收 300 -> 担保人 300、平台损失 200，并冻结凭证
    let split = split_overage(&thb(500), &thb(300)).unwrap();
    assert_eq!(
        (
            split.sponsor.micros(),
            split.platform_loss.micros(),
            split.freeze_credential
        ),
        (300, 200, true)
    );

    // ---- 7. 撤销：凭证被撤销后，调用被拒，且原因码稳定 ----
    let revoked = Credential::new(
        service.id().clone(),
        service.binding().clone(),
        CredentialStatus::Revoked,
        None,
        vec![],
    )
    .unwrap();
    let err = resolve_payment_context(&dir, &revoked, Some(&kid1), NOW).unwrap_err();
    assert_eq!(err.reason_code(), ReasonCode::CredentialRevoked);
    assert_eq!(err.reason_code().as_str(), "credential_revoked");
}

fn endpoint(provider: &str, model: &str, name: &str) -> idoris_compute_contracts::ids::EndpointKey {
    idoris_compute_contracts::ids::EndpointKey::new(provider, model, name).unwrap()
}
