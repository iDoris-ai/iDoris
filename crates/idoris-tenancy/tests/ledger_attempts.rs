//! S3 准入账本与 S4 尝试记录的对接：两张账的对应关系（`accounts/ledger/hook.rs` 的表）和「收尾同一个事务」。
//!
//! 每个场景都用真实的 SQLite 文件、同一个库里的 `AccountStore`（账本）和 `AttemptStore`（尝试记录与结算分录），
//! 走完整的生命周期：准入（账本）→ 创建尝试（钩子核对预留）→ 事件 → 发布 → 收尾（钩子在同一个事务里结算账本），
//! 最后跑对账：账本自守恒，并且和尝试记录的预留、`user_price`、`release` 分录逐项对得上。
//!
//! 数字约定：价目每个输入 token 1 微泰铢、每个输出 token 2 微泰铢（`support/ledger_fixture.rs`）；
//! `max_tokens = 100` 的预留是 210；用量 10 入 30 出的实际价是 `10 + 60 = 70`。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::collections::BTreeSet;

use fixture::*;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::attempt::AttemptEvent;
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::{AttemptIndex, IdempotencyKey, PriceVersion};
use idoris_compute_contracts::permit::{Billing, DataClass};
use idoris_compute_contracts::pricing::Usage;
use idoris_compute_contracts::request::{AdmittedCandidate, Request};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AlertKind, ComponentDecision, HoldState};
use idoris_tenancy::attempts::{
    AttemptStore, AttemptStoreError, NewRequest, RequestClose, RequestOutcome,
};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

struct Env {
    db: TempDb,
    accounts: AccountStore,
    attempts: AttemptStore,
}

/// 同一个库文件：账本（`AccountStore`）和尝试记录（`AttemptStore`，装了账本钩子）。
fn env(tag: &str, emp_funds: i64) -> Env {
    let db = TempDb::new(tag);
    let accounts = world(&db);
    accounts
        .fund_account(&aid("emp"), &thb(emp_funds), &key("f-emp"), t0())
        .unwrap();
    accounts
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let attempts = AttemptStore::open(db.path()).unwrap();
    attempts
        .install_ledger_hook(accounts.attempt_ledger_hook())
        .unwrap();
    Env {
        db,
        accounts,
        attempts,
    }
}

fn candidate(decision: AdmissionDecision, req: &Req) -> AdmittedCandidate {
    AdmittedCandidate {
        decision,
        price_version: PriceVersion::new("pv-1").unwrap(),
        cost_rates: req.cost.clone(),
        price_rates: req.price.clone(),
    }
}

fn classes() -> BTreeSet<DataClass> {
    BTreeSet::from([DataClass::Adult])
}

fn new_request<'a>(
    request: &'a Request,
    classes: &'a BTreeSet<DataClass>,
    at: i64,
) -> NewRequest<'a> {
    NewRequest {
        id: request.id(),
        payment: request.payment(),
        form: DeploymentForm::Cloud,
        billing: Billing::Paid,
        stream: false,
        data_classes: classes,
        created_at: ts(at),
    }
}

/// 准入（账本）+ 创建请求和第一次尝试（尝试记录，钩子核对预留）。
fn start(env: &Env, request_id: &str, credential: &str, max_tokens: u64) -> (Req, Request) {
    let req = Req::new(&env.accounts, request_id, 0, credential, max_tokens);
    let decision = env.accounts.admit_attempt(&req.input()).unwrap();
    assert!(decision.is_admitted(), "{decision:?}");
    let request = Request::admit(
        req.attempt.request().clone(),
        req.payment.clone(),
        candidate(decision, &req),
    )
    .unwrap();
    env.attempts
        .create_request(
            &new_request(&request, &classes(), T0),
            &request.attempts()[0],
        )
        .unwrap();
    (req, request)
}

/// 切换到下一个候选：账本准入（序号 1，释放上一次）+ 追加尝试。
fn switch(env: &Env, request: &mut Request, req: &Req, at: i64) -> Req {
    let next = Req {
        attempt: idoris_compute_contracts::ids::AttemptId::new(
            req.attempt.request().clone(),
            AttemptIndex(request.latest().0 + 1),
        ),
        ..req.clone()
    }
    .at(at);
    let decision = env.accounts.admit_attempt(&next.input()).unwrap();
    assert!(decision.is_admitted(), "{decision:?}");
    let idx = request
        .begin_next_attempt(candidate(decision, &next))
        .unwrap();
    env.attempts
        .append_attempt(request.id(), request.attempt(idx).unwrap(), ts(at))
        .unwrap();
    next
}

fn apply(env: &Env, request: &mut Request, index: u8, events: &[AttemptEvent], at: i64) {
    let id = request.attempt(AttemptIndex(index)).unwrap().id().clone();
    env.attempts.apply_events(&id, events, ts(at)).unwrap();
    for event in events {
        request.apply(AttemptIndex(index), event.clone()).unwrap();
    }
}

fn usage_70() -> Usage {
    Usage::new(10, 30, 0).unwrap()
}

fn publish(env: &Env, request: &mut Request, index: u8, at: i64) {
    let id = request.attempt(AttemptIndex(index)).unwrap().id().clone();
    env.attempts.record_publish(&id, ts(at)).unwrap();
    request.publish_first_byte(AttemptIndex(index)).unwrap();
}

/// 与执行器 `RunState::finalize` 同样的收尾：在途的尝试 `NeverLeftHost`，算计划，同一个事务里落账，内存跟上。
fn finalize(env: &Env, request: &mut Request, at: i64) -> Result<(), AttemptStoreError> {
    let in_flight: Vec<u8> = request
        .attempts()
        .iter()
        .filter(|a| a.state().is_in_flight())
        .map(|a| a.index().0)
        .collect();
    for index in in_flight {
        apply(env, request, index, &[AttemptEvent::NeverLeftHost], at);
    }
    let plan = request.plan_settlement().unwrap();
    let to_settle: Vec<_> = request
        .attempts()
        .iter()
        .filter(|a| {
            matches!(
                a.state().kind(),
                idoris_compute_contracts::attempt::AttemptStateKind::ExecutedUsageKnown
                    | idoris_compute_contracts::attempt::AttemptStateKind::ExecutedUsageUnknown
            )
        })
        .map(|a| a.id().clone())
        .collect();
    let outcome = if request.published().is_some() {
        RequestOutcome::Answered
    } else {
        RequestOutcome::Unanswered
    };
    env.attempts.finalize_request(
        &plan,
        &to_settle,
        RequestClose {
            outcome,
            reason: None,
            at: ts(at),
        },
    )?;
    for id in &to_settle {
        request.apply(id.index(), AttemptEvent::Settle).unwrap();
    }
    Ok(())
}

fn postings(env: &Env, request: &Request) -> Vec<(u8, String, i64)> {
    env.attempts
        .postings(request.id())
        .unwrap()
        .into_iter()
        .map(|p| (p.attempt.index().0, p.kind, p.money.micros))
        .collect()
}

fn balance(env: &Env, account: &str, now: i64) -> i64 {
    env.accounts
        .account_view(&aid(account), ts(now))
        .unwrap()
        .balance
        .unwrap()
        .micros
}

fn p(attempt: u8, kind: &str, micros: i64) -> (u8, String, i64) {
    (attempt, kind.to_string(), micros)
}

// ------------------------------------------------------------------------------------------
// 完整生命周期：账本和尝试记录的对应关系
// ------------------------------------------------------------------------------------------

#[test]
fn a_full_request_settles_the_ledger_in_the_same_transaction_as_the_postings() {
    let env = env("attempts-lifecycle", 1_000);
    let (req, mut request) = start(&env, "life-1", "cred-e", 100);
    assert_eq!(balance(&env, "emp", T0), 1_000 - 210, "准入占住预留");
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env, &mut request, 0, T0 + 300);
    finalize(&env, &mut request, T0 + 400).unwrap();

    // 对应关系：user_price == 账本的 charged；release == 预留 - charged
    assert_eq!(
        postings(&env, &request),
        vec![
            p(0, "user_price", 70),
            p(0, "platform_cost", 18),
            p(0, "release", 140)
        ]
    );
    let hold = env.accounts.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(
        (hold.state, hold.charged),
        (HoldState::Settled, Some(thb(70)))
    );
    assert_eq!(balance(&env, "emp", T0 + 400), 1_000 - 70);
    assert_eq!(balance(&env, "paid", T0 + 400), 1_000_000 - 70);
    assert_clean(&env.accounts);
    let report = env.accounts.reconcile_with_attempts().unwrap();
    assert_eq!(report.holds_checked, 1);
}

#[test]
fn a_creating_an_attempt_requires_a_matching_open_reservation() {
    let env = env("attempts-bind", 1_000);
    let req = Req::new(&env.accounts, "bind-1", 0, "cred-e", 100);
    // 没有准入就想创建尝试：钩子拒绝，什么都没写
    let decision = {
        // 借别的请求的准入决定造一个候选，但账本里没有这个尝试的预留
        let other = Req::new(&env.accounts, "bind-other", 0, "cred-e", 100);
        env.accounts.admit_attempt(&other.input()).unwrap()
    };
    let request = Request::admit(
        req.attempt.request().clone(),
        req.payment.clone(),
        candidate(decision, &req),
    )
    .unwrap();
    let err = env
        .attempts
        .create_request(
            &new_request(&request, &classes(), T0),
            &request.attempts()[0],
        )
        .unwrap_err();
    assert!(matches!(err, AttemptStoreError::LedgerHook(_)), "{err}");
    assert!(err.to_string().contains("not found"), "{err}");
    assert!(
        env.attempts.request_row(request.id()).unwrap().is_none(),
        "同一个事务回滚，请求行也没有"
    );
    assert_eq!(scalar(&env.db, "SELECT COUNT(*) FROM compute_attempts"), 0);

    // 金额对不上：账本里预留的是 210，尝试声称预留了别的数
    let req2 = Req::new(&env.accounts, "bind-2", 0, "cred-e", 100);
    let decision = env.accounts.admit_attempt(&req2.input()).unwrap();
    let mut tampered = decision.clone();
    if let AdmissionDecision::Allow { admitted } = &mut tampered {
        admitted.reservation.price = price(999);
    }
    let request2 = Request::admit(
        req2.attempt.request().clone(),
        req2.payment.clone(),
        candidate(tampered, &req2),
    )
    .unwrap();
    let err = env
        .attempts
        .create_request(
            &new_request(&request2, &classes(), T0),
            &request2.attempts()[0],
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");

    // 预留在创建尝试之前就过期了：不能再创建（没有预留的请求不能跑）
    let ttl = env.accounts.ledger_config().unwrap().reservation_ttl_ms;
    let req3 = Req::new(&env.accounts, "bind-3", 0, "cred-e", 100);
    let decision = env.accounts.admit_attempt(&req3.input()).unwrap();
    let request3 = Request::admit(
        req3.attempt.request().clone(),
        req3.payment.clone(),
        candidate(decision, &req3),
    )
    .unwrap();
    let err = env
        .attempts
        .create_request(
            &new_request(&request3, &classes(), T0 + ttl),
            &request3.attempts()[0],
        )
        .unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");
}

#[test]
fn failover_releases_the_failed_attempt_and_charges_only_the_answering_one() {
    let env = env("attempts-failover", 1_000);
    let (req, mut request) = start(&env, "fo-1", "cred-e", 100);
    // 第一个候选：字节可能已经出去，然后超时（maybe_executed，成本挂账，用户不为它付钱）
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    // 切换：账本准入第二个候选，同一个事务里释放第一个的预留
    let second = switch(&env, &mut request, &req, T0 + 200);
    assert_eq!(
        env.accounts
            .hold(&req.attempt, "model")
            .unwrap()
            .unwrap()
            .state,
        HoldState::Released,
        "切换时就释放，不等收尾"
    );
    assert_eq!(
        balance(&env, "emp", T0 + 200),
        1_000 - 210,
        "只占着第二次的预留"
    );
    apply(
        &env,
        &mut request,
        1,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 300,
    );
    apply(
        &env,
        &mut request,
        1,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 400,
    );
    publish(&env, &mut request, 1, T0 + 500);
    finalize(&env, &mut request, T0 + 600).unwrap();

    // 用户只付一次：第二个尝试 70；第一个尝试 0（预留整笔释放，成本 210 的预留成本挂账）
    let by_attempt = postings(&env, &request);
    assert!(
        by_attempt.contains(&p(1, "user_price", 70)),
        "{by_attempt:?}"
    );
    assert!(by_attempt.contains(&p(0, "release", 210)), "{by_attempt:?}");
    assert!(
        by_attempt
            .iter()
            .all(|(i, k, _)| !(*i == 0 && k == "user_price")),
        "{by_attempt:?}"
    );
    let first = env.accounts.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(
        (first.state, first.charged),
        (HoldState::Released, Some(thb(0)))
    );
    let answering = env
        .accounts
        .hold(&second.attempt, "model")
        .unwrap()
        .unwrap();
    assert_eq!(
        (answering.state, answering.charged),
        (HoldState::Settled, Some(thb(70)))
    );
    assert_eq!(balance(&env, "emp", T0 + 600), 1_000 - 70);
    assert_clean(&env.accounts);
}

#[test]
fn a_request_that_never_published_charges_nothing_and_releases_everything() {
    let env = env("attempts-unanswered", 1_000);
    let (req, mut request) = start(&env, "un-1", "cred-e", 100);
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    // 完整输出没通过检查：没有发布，用户付 0
    finalize(&env, &mut request, T0 + 300).unwrap();
    let hold = env.accounts.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(
        (hold.state, hold.charged),
        (HoldState::Released, Some(thb(0)))
    );
    assert_eq!(balance(&env, "emp", T0 + 300), 1_000);
    assert!(
        postings(&env, &request)
            .iter()
            .all(|(_, k, _)| k != "user_price")
    );
    assert_clean(&env.accounts);
}

#[test]
fn a_published_attempt_with_unknown_usage_is_charged_the_reserved_amount() {
    let env = env("attempts-unknown-usage", 1_000);
    let (req, mut request) = start(&env, "uk-1", "cred-e", 100);
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    // 流式：已经发布，usage 一直没有到（maybe_executed）：按预留价付费，成本挂账
    publish(&env, &mut request, 0, T0 + 200);
    finalize(&env, &mut request, T0 + 300).unwrap();
    let hold = env.accounts.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(hold.charged, Some(thb(210)));
    assert_eq!(balance(&env, "emp", T0 + 300), 1_000 - 210);
    assert_clean(&env.accounts);
}

#[test]
fn finalizing_twice_never_double_charges() {
    let env = env("attempts-replay", 1_000);
    let (req, mut request) = start(&env, "rp-1", "cred-e", 100);
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env, &mut request, 0, T0 + 300);
    finalize(&env, &mut request, T0 + 400).unwrap();
    let entries = env
        .accounts
        .entries_of_hold(&req.attempt, "model")
        .unwrap()
        .len();
    let postings_before = postings(&env, &request);
    // 同一份计划再收尾一次（恢复后重复结算）：账本和分录都不变
    let plan = request.plan_settlement().unwrap();
    let report = env
        .attempts
        .finalize_request(
            &plan,
            &[],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: ts(T0 + 500),
            },
        )
        .unwrap();
    assert!(report.replayed);
    assert_eq!(
        env.accounts
            .entries_of_hold(&req.attempt, "model")
            .unwrap()
            .len(),
        entries
    );
    assert_eq!(postings(&env, &request), postings_before);
    assert_eq!(balance(&env, "emp", T0 + 500), 1_000 - 70);
    assert_clean(&env.accounts);
}

#[test]
fn if_the_ledger_refuses_the_settlement_the_whole_finalization_rolls_back() {
    // 同一个事务：账本这一侧出错，尝试记录的分录、结算、请求关闭都不会写
    let env = env("attempts-atomic", 1_000);
    let (req, mut request) = start(&env, "at-1", "cred-e", 100);
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env, &mut request, 0, T0 + 300);
    // 有人已经在账本里把这个预留按另一个金额结算了（冲突）
    env.accounts
        .settle_attempt(&req.attempt, &price(5), ts(T0 + 350))
        .unwrap();
    let err = finalize(&env, &mut request, T0 + 400).unwrap_err();
    assert!(matches!(err, AttemptStoreError::LedgerHook(_)), "{err}");
    let row = env.attempts.request_row(request.id()).unwrap().unwrap();
    assert!(row.outcome.is_none(), "请求没有关闭");
    assert!(
        env.attempts.postings(request.id()).unwrap().is_empty(),
        "一条分录都没有写"
    );
    assert_eq!(
        env.attempts.attempt_rows(request.id()).unwrap()[0].state_kind(),
        idoris_compute_contracts::attempt::AttemptStateKind::ExecutedUsageKnown,
        "尝试也没有被推进到 settled"
    );
}

#[test]
fn recovering_a_request_after_the_reservation_expired_applies_the_late_settlement_rule() {
    // 进程在上游返回之后、收尾之前崩溃：预留过期。用 S4 的恢复接口重新收尾，账本按迟到结算处理
    let env = env("attempts-recovery", 1_000);
    let ttl = env.accounts.ledger_config().unwrap().reservation_ttl_ms;
    let (req, mut request) = start(&env, "rc-1", "cred-e", 100);
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env, &mut request, 0, T0 + 300);
    // ……崩溃。过期的预留被别的准入扫掉了：
    env.accounts.sweep_expired_holds(ts(T0 + ttl + 1)).unwrap();
    assert_eq!(
        env.accounts
            .hold(&req.attempt, "model")
            .unwrap()
            .unwrap()
            .state,
        HoldState::Expired
    );
    // 重启后：找出还没关闭的请求，重建、算计划、收尾（幂等）
    let open = env.attempts.open_requests(ts(T0 + ttl + 10), 10).unwrap();
    assert_eq!(open, vec![request.id().clone()]);
    let mut recovered = env.attempts.load_request(&open[0]).unwrap().unwrap();
    finalize(&env, &mut recovered, T0 + ttl + 20).unwrap();
    let hold = env.accounts.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(
        (hold.state, hold.late, hold.charged),
        (HoldState::Settled, true, Some(thb(70)))
    );
    assert_eq!(
        env.accounts
            .alerts(Some(AlertKind::LateSettlement))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(balance(&env, "emp", T0 + ttl + 20), 1_000 - 70);
    assert_clean(&env.accounts);
}

#[test]
fn extra_costs_left_unsettled_at_close_are_released_with_a_visible_alert() {
    let env = env("attempts-extra", 1_000);
    let (req, mut request) = start(&env, "ex-1", "cred-e", 100);
    let reserved = env
        .accounts
        .reserve_component(&req.attempt, "safety_output", &price(40), ts(T0 + 50))
        .unwrap();
    assert!(matches!(reserved, ComponentDecision::Reserved { .. }));
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env, &mut request, 0, T0 + 300);
    finalize(&env, &mut request, T0 + 400).unwrap();
    let extra = env
        .accounts
        .hold(&req.attempt, "safety_output")
        .unwrap()
        .unwrap();
    assert_eq!(
        (extra.state, extra.charged),
        (HoldState::Released, Some(thb(0)))
    );
    assert_eq!(
        env.accounts
            .alerts(Some(AlertKind::ExtraUnsettled))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        balance(&env, "emp", T0 + 400),
        1_000 - 70,
        "附加成本的预留已经放回去"
    );
    assert_clean(&env.accounts);
}

#[test]
fn reconciliation_really_detects_the_failure_modes_the_hook_exists_to_prevent() {
    // 对账不能只会说「干净」：每一种「两张账不一致」都要真的被发现。
    let env = env("attempts-reconcile-detect", 1_000);
    let (_, mut request) = start(&env, "rd-1", "cred-e", 100);
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env,
        &mut request,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env, &mut request, 0, T0 + 300);
    finalize(&env, &mut request, T0 + 400).unwrap();
    assert_clean(&env.accounts);

    // 1. 没有装钩子的尝试记录存储（等于「有人绕过了接线」）创建了一个账本里没有预留的尝试
    let bare = AttemptStore::open(env.db.path()).unwrap();
    let other = Req::new(&env.accounts, "rd-other", 0, "cred-e", 100);
    let decision = env.accounts.admit_attempt(&other.input()).unwrap();
    let ghost = Req::new(&env.accounts, "rd-ghost", 0, "cred-e", 100);
    let ghost_request = Request::admit(
        ghost.attempt.request().clone(),
        ghost.payment.clone(),
        candidate(decision, &ghost),
    )
    .unwrap();
    bare.create_request(
        &new_request(&ghost_request, &classes(), T0 + 500),
        &ghost_request.attempts()[0],
    )
    .unwrap();
    let report = env.accounts.reconcile_with_attempts().unwrap();
    assert!(
        report
            .mismatches
            .iter()
            .any(|m| m.subject == "rd-ghost#0" && m.detail.contains("no reservation")),
        "{:#?}",
        report.mismatches
    );

    // 2. 尝试记录这一侧结算了（user_price 写了、请求关了），账本那一侧的预留却还开着：
    //    钩子存在就是为了让这种「只成功一半」不可能发生；绕过它就会被对账看见
    let env2 = self::env("attempts-reconcile-half", 1_000);
    let (req2, mut request2) = start(&env2, "rd-2", "cred-e", 100);
    apply(
        &env2,
        &mut request2,
        0,
        &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        T0 + 100,
    );
    apply(
        &env2,
        &mut request2,
        0,
        &[AttemptEvent::UsageReported { usage: usage_70() }],
        T0 + 200,
    );
    publish(&env2, &mut request2, 0, T0 + 300);
    let bare2 = AttemptStore::open(env2.db.path()).unwrap(); // 没有钩子
    let plan = request2.plan_settlement().unwrap();
    bare2
        .finalize_request(
            &plan,
            std::slice::from_ref(&req2.attempt),
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: ts(T0 + 400),
            },
        )
        .unwrap();
    let report = env2.accounts.reconcile_with_attempts().unwrap();
    assert!(
        report
            .mismatches
            .iter()
            .any(|m| m.subject == "rd-2#0" && m.detail.contains("still open")),
        "{:#?}",
        report.mismatches
    );
    // 对照：补上账本那一侧（同一份计划重放，钩子在）之后一致。这正是恢复后「再收尾一次」做的事
    env2.attempts
        .finalize_request(
            &plan,
            &[],
            RequestClose {
                outcome: RequestOutcome::Answered,
                reason: None,
                at: ts(T0 + 500),
            },
        )
        .unwrap();
    assert_clean(&env2.accounts);

    // 3. 账本自己的守恒：往账本里塞一条让在途对不上的分录（过得了触发器，但对账看得见）
    let conn = rusqlite::Connection::open(env.db.path()).unwrap();
    conn.execute(
        "INSERT INTO ledger_entries (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros,
                                     liability_micros, window_at_ms, at_ms, idempotency_key)
         VALUES ('emp', 'rd-1#0', 'reserve', 'THB', 210, 0, 0, 0, 0, 0, 'forged-reserve')",
        [],
    )
    .unwrap();
    let report = env.accounts.reconcile_with_attempts().unwrap();
    assert!(
        report
            .mismatches
            .iter()
            .any(|m| m.subject == "rd-1#0" && m.detail.contains("in-flight entries sum to 210")),
        "{:#?}",
        report.mismatches
    );
}
