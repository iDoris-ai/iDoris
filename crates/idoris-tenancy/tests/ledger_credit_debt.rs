//! 授信限制的是**累计债务**，不是单笔（PR #600 第二轮 Codex 评审 High）。
//!
//! 旧代码：没充值、也没有生效的花费类额度、只有一笔授信的账户（经控制面开通授信的成员），`ceiling_remaining` 被固定填 `Some(0)`，
//! 它自己账上的已花和在途预留一个也没进去，所以每次准入看到的都是「0 + 授信」的全额余量：授信 200，连续三笔 100 全部放行，
//! 并发的请求也一样。现在这种账户的 `ceiling_remaining` 是 `-债务`（已花 + 在途预留），授信限制的是累计。
//!
//! 第五轮（owner 2026-11-15，方案 C）把债务改成「没被额度覆盖的部分」，新额度不抵旧债、被撤销的额度撤销之前覆盖过的花费不算债务：
//! 那些场景在 `ledger_credit_books.rs`。这个文件里的场景（没有额度的账户、额度整段花费都在窗口内）在新口径下数字不变。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢，
//! 所以 `max_tokens = 45` 的预留正好是 100。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use fixture::*;
use idoris_compute_contracts::admission::{AdmissionDecision, AncestorView};
use idoris_compute_contracts::control::WriteResult;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::ledger::CreditLineSetting;
use idoris_tenancy::accounts::{AccountStore, GrantAllocation, Meta, Who};

/// `max_tokens = 45`：预留 100。
const M100: u64 = 45;
const THREADS: usize = 24;

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn admit(store: &AccountStore, req: &Req) -> AdmissionDecision {
    store.admit_attempt(&req.input()).unwrap()
}

fn denied(decision: &AdmissionDecision) -> Option<ReasonCode> {
    match decision {
        AdmissionDecision::Reject { reason } => Some(reason.code()),
        _ => None,
    }
}

/// 一个请求：先不允许宽限，被拒（钱不够）再允许宽限试一遍，和云执行器的两遍一样。返回最终的决定。
fn admit_with_ladder(
    store: &AccountStore,
    request: &str,
    max_tokens: u64,
    now: i64,
) -> AdmissionDecision {
    let first = Req::new(store, request, 0, "cred-e", max_tokens).at(now);
    let decision = admit(store, &first);
    if decision.is_admitted() || denied(&decision) != Some(ReasonCode::BudgetExhausted) {
        return decision;
    }
    admit(store, &first.clone().grace())
}

fn attempt_of(request: &str) -> idoris_compute_contracts::ids::AttemptId {
    idoris_compute_contracts::ids::AttemptId::new(
        idoris_compute_contracts::ids::RequestId::new(request).unwrap(),
        idoris_compute_contracts::ids::AttemptIndex(0),
    )
}

/// `emp`（成员）没有充值、没有任何花费类额度；组织 `paid` 资金充足，并且从平台拿到了足够下发授信的额度
/// （S0 要求授权方自己有授权）。授信由调用者自己开。
fn credit_only_world(db: &TempDb) -> AccountStore {
    let store = world(db);
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    allocate(&store, "e-0", "platform", "paid", 9_000_000);
    store
}

/// 经控制面（HTTP 开通走的同一个存储层函数）给 `emp` 开一笔授信。
fn open_credit_through_the_control_plane(
    store: &AccountStore,
    id: &str,
    micros: i64,
    from: i64,
    until: i64,
) {
    let written = store
        .control_grant_allocation(
            &Who::Operator("ledger-test"),
            &Meta {
                now: ts(from),
                request_id: "req-credit",
            },
            &GrantAllocation {
                id: AllocationId::new(id).unwrap(),
                parent: aid("paid"),
                child: aid("emp"),
                amount: thb(micros),
                purpose: Purpose::CreditLine,
                valid_from: ts(from),
                valid_until: ts(until),
                month: None,
            },
        )
        .unwrap();
    assert_eq!(written.result, WriteResult::Created);
}

fn view(store: &AccountStore, now: i64) -> AncestorView {
    store.account_view(&aid("emp"), ts(now)).unwrap()
}

#[test]
fn three_charges_of_100_on_a_200_credit_line_the_third_is_refused() {
    // Codex 的原场景：成员没有充值、没有花费类额度，仅经控制面开通 200 授信；父账户资金充足。
    // 旧代码：三笔 100 全部放行（分录累计 300，准入仍看到「0 + 200」）。
    let db = TempDb::new("credit-debt-sequential");
    let store = credit_only_world(&db);
    open_credit_through_the_control_plane(&store, "cl-1", 200, T0, T0 + DAY);
    let at = |n: i64| T0 + 1_000 * n;
    let before = view(&store, at(1));
    assert_eq!(
        (
            before.balance.clone(),
            before.ceiling_remaining.clone(),
            before.credit_limit.clone()
        ),
        (None, Some(thb(0)), thb(200)),
        "还没欠钱：可用 0，授信 200"
    );

    for (n, expected_debt) in [(1_i64, 100_i64), (2, 200)] {
        let req = Req::new(&store, &format!("r-{n}"), 0, "cred-e", M100).at(at(n));
        let decision = admit(&store, &req);
        assert!(decision.is_admitted(), "charge #{n}: {decision:?}");
        store
            .settle_attempt(&req.attempt, &price(100), ts(at(n) + 500))
            .unwrap();
        assert_eq!(
            view(&store, at(n) + 600).ceiling_remaining,
            Some(thb(-expected_debt)),
            "charge #{n}: 已花进了债务，可用额度 = -债务"
        );
    }

    // 第三笔：累计债务已经是 200，授信用完。不管有没有宽限，都被拒绝（没有宽限：这个账户从没配置过钱）
    let third = Req::new(&store, "r-3", 0, "cred-e", M100).at(at(3));
    assert_eq!(
        denied(&admit(&store, &third)),
        Some(ReasonCode::BudgetExhausted)
    );
    let decision = admit_with_ladder(&store, "r-3", M100, at(3));
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
    assert!(store.hold(&attempt_of("r-3"), "model").unwrap().is_none());
    assert_clean(&store);
}

#[test]
fn reservations_in_flight_count_against_the_credit_line_before_anything_is_settled() {
    let db = TempDb::new("credit-debt-in-flight");
    let store = credit_only_world(&db);
    open_credit_through_the_control_plane(&store, "cl-1", 200, T0, T0 + DAY);
    let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 1_000);
    let second = Req::new(&store, "r-2", 0, "cred-e", M100).at(T0 + 2_000);
    assert!(admit(&store, &first).is_admitted());
    assert!(admit(&store, &second).is_admitted());
    assert_eq!(
        view(&store, T0 + 2_500).ceiling_remaining,
        Some(thb(-200)),
        "两个还没结算的预留已经是债务"
    );
    let third = Req::new(&store, "r-3", 0, "cred-e", M100).at(T0 + 3_000);
    assert_eq!(
        denied(&admit(&store, &third)),
        Some(ReasonCode::BudgetExhausted)
    );
    // 放掉一个预留：债务少 100，第三个请求就放得下
    store
        .release_attempt(&first.attempt, ts(T0 + 4_000))
        .unwrap();
    let third = Req::new(&store, "r-3", 0, "cred-e", M100).at(T0 + 5_000);
    assert!(admit(&store, &third).is_admitted());
    assert_clean(&store);
}

#[test]
fn a_degraded_request_is_still_limited_by_what_the_credit_line_has_left() {
    // 授信 150：第一笔 100 全额；第二笔想要 100 但授信只剩 50，压低 max_tokens 放行（预留 <= 50）；之后连最低有用的预留都放不下
    let db = TempDb::new("credit-debt-degrade");
    let store = credit_only_world(&db);
    open_credit_through_the_control_plane(&store, "cl-1", 150, T0, T0 + DAY);
    let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 1_000);
    assert!(matches!(
        admit(&store, &first),
        AdmissionDecision::Allow { .. }
    ));
    store
        .settle_attempt(&first.attempt, &price(100), ts(T0 + 1_500))
        .unwrap();
    let second = Req::new(&store, "r-2", 0, "cred-e", M100).at(T0 + 2_000);
    let decision = admit(&store, &second);
    let AdmissionDecision::Degrade { admitted, .. } = &decision else {
        panic!("{decision:?}");
    };
    assert!(
        admitted.reservation.price.micros() <= 50,
        "债务 100 之后授信只剩 50，降级后的预留不能超过它: {decision:?}"
    );
    let reserved = admitted.reservation.price.micros();
    // 债务 = 100 + 降级预留，不会超过授信 150
    assert_eq!(
        view(&store, T0 + 2_500).ceiling_remaining,
        Some(thb(-(100 + reserved)))
    );
    assert!(100 + reserved <= 150);
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_a_credit_only_accounts_credit_line() {
    // 授信 = 3 个预留（每个 100）：24 个并发请求，恰好 3 个放行；采样者线程任何时刻都看不到债务超过授信
    let db = TempDb::new("credit-debt-race");
    let store = credit_only_world(&db);
    open_credit_through_the_control_plane(&store, "cl-1", 300, T0, T0 + DAY);
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("race-{i}"), 0, "cred-e", M100).at(T0 + 1_000))
        .collect();

    let barrier = Arc::new(Barrier::new(requests.len() + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let path = db.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        let samples = Arc::clone(&samples);
        thread::spawn(move || {
            let store = AccountStore::open(&path).unwrap();
            barrier.wait();
            loop {
                let view = store.account_view(&aid("emp"), ts(T0 + 1_000)).unwrap();
                let remaining = view.ceiling_remaining.unwrap().micros;
                assert!(
                    remaining >= -view.credit_limit.micros,
                    "the debt {} went beyond the credit line {}",
                    -remaining,
                    view.credit_limit.micros
                );
                samples.fetch_add(1, Ordering::Relaxed);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
    };
    let workers: Vec<_> = requests
        .into_iter()
        .map(|request| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store.admit_attempt(&request.input())
            })
        })
        .collect();
    let decisions: Vec<AdmissionDecision> = workers
        .into_iter()
        .map(|h| {
            h.join()
                .unwrap()
                .expect("admission must not error under contention")
        })
        .collect();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    assert!(samples.load(Ordering::Relaxed) > 0);

    let admitted = decisions.iter().filter(|d| d.is_admitted()).count();
    assert_eq!(admitted, 3, "{decisions:?}");
    assert!(
        decisions
            .iter()
            .filter(|d| !d.is_admitted())
            .all(|d| denied(d) == Some(ReasonCode::BudgetExhausted)),
        "{decisions:?}"
    );
    assert_eq!(view(&store, T0 + 2_000).ceiling_remaining, Some(thb(-300)));
    assert_clean(&store);
}

#[test]
fn setting_the_credit_line_again_does_not_forgive_the_debt() {
    // 债务只会被还掉，不会被忘掉：重新「设置」授信（替换）、关掉再开，都不能把已经欠下的清零，
    // 否则一个定时「确保授信是 200」的任务每跑一次就多送 200
    let db = TempDb::new("credit-debt-reset");
    let store = credit_only_world(&db);
    let setting = |limit: i64| CreditLineSetting {
        limit: thb(limit),
        valid_until: None,
    };
    store
        .set_credit_line(&aid("emp"), &setting(200), ts(T0))
        .unwrap();
    let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 1_000);
    assert!(admit(&store, &first).is_admitted());
    store
        .settle_attempt(&first.attempt, &price(100), ts(T0 + 1_500))
        .unwrap();

    // 再设置一遍同样的授信：债务还在
    store
        .set_credit_line(&aid("emp"), &setting(200), ts(T0 + 2_000))
        .unwrap();
    assert_eq!(view(&store, T0 + 2_100).ceiling_remaining, Some(thb(-100)));
    // 关掉：什么都不允许；再开一笔 200：债务 100 还在，只剩 100
    store
        .disable_credit_line(&aid("emp"), ts(T0 + 3_000))
        .unwrap();
    let blocked = Req::new(&store, "r-x", 0, "cred-e", M100).at(T0 + 3_500);
    assert_eq!(
        denied(&admit(&store, &blocked)),
        Some(ReasonCode::BudgetExhausted)
    );
    store
        .set_credit_line(&aid("emp"), &setting(200), ts(T0 + 4_000))
        .unwrap();
    let second = Req::new(&store, "r-2", 0, "cred-e", M100).at(T0 + 4_500);
    assert!(admit(&store, &second).is_admitted());
    store
        .settle_attempt(&second.attempt, &price(100), ts(T0 + 4_600))
        .unwrap();
    let third = Req::new(&store, "r-3", 0, "cred-e", M100).at(T0 + 5_000);
    assert_eq!(
        denied(&admit(&store, &third)),
        Some(ReasonCode::BudgetExhausted),
        "两笔 100 之后授信 200 用完，重设几遍都没用"
    );
    assert_clean(&store);
}

#[test]
fn spending_covered_by_an_allowance_that_has_ended_is_not_credit_debt() {
    // 额度（预付）窗口 [T0 - 1 天, T0 + 1 天)：窗口里花掉的 100 是额度覆盖的，不是授信债务。
    // 额度到期之后改靠授信：那 100 是额度覆盖的，授信债务从 0 算起，所以授信 200 还能放两笔 100，第三笔拒绝。
    let db = TempDb::new("credit-debt-after-allowance");
    let store = credit_only_world(&db);
    store
        .create_allocation(
            &idoris_compute_contracts::account::Allocation {
                id: AllocationId::new("emp-prepaid").unwrap(),
                parent: aid("paid"),
                child: aid("emp"),
                ceiling: grant(1_000, Purpose::Prepaid, T0 - DAY, T0 + DAY),
                idempotency_key: None,
            },
            t0(),
        )
        .unwrap();
    let covered = Req::new(&store, "r-0", 0, "cred-e", M100).at(T0 + 1_000);
    assert!(matches!(
        admit(&store, &covered),
        AdmissionDecision::Allow { .. }
    ));
    store
        .settle_attempt(&covered.attempt, &price(100), ts(T0 + 1_500))
        .unwrap();

    let later = T0 + 2 * DAY;
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(200),
                valid_until: None,
            },
            ts(later),
        )
        .unwrap();
    assert_eq!(
        view(&store, later + 1).ceiling_remaining,
        Some(thb(0)),
        "额度覆盖过的 100 不是授信债务"
    );
    for (n, request) in ["r-1", "r-2"].into_iter().enumerate() {
        let n = i64::try_from(n).unwrap();
        let req = Req::new(&store, request, 0, "cred-e", M100).at(later + 1_000 * (n + 1));
        assert!(admit(&store, &req).is_admitted(), "{request}");
        store
            .settle_attempt(&req.attempt, &price(100), ts(later + 1_000 * (n + 1) + 500))
            .unwrap();
    }
    let third = Req::new(&store, "r-3", 0, "cred-e", M100).at(later + 5_000);
    assert_eq!(
        denied(&admit(&store, &third)),
        Some(ReasonCode::BudgetExhausted)
    );
    assert_clean(&store);
}

#[test]
fn an_account_with_funds_is_unchanged_its_balance_already_carries_the_debt() {
    // 对照：有充值的账户，余额本来就记着已花和在途，授信的行为没有变
    let db = TempDb::new("credit-debt-funded-control");
    let store = credit_only_world(&db);
    store
        .fund_account(&aid("emp"), &thb(100), &key("f-emp"), t0())
        .unwrap();
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(100),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    for (n, request) in ["r-1", "r-2"].into_iter().enumerate() {
        let n = i64::try_from(n).unwrap();
        let req = Req::new(&store, request, 0, "cred-e", M100).at(T0 + 1_000 * (n + 1));
        assert!(admit(&store, &req).is_admitted(), "{request}");
        store
            .settle_attempt(&req.attempt, &price(100), ts(T0 + 1_000 * (n + 1) + 500))
            .unwrap();
    }
    assert_eq!(view(&store, T0 + 3_000).balance, Some(thb(-100)));
    let third = Req::new(&store, "r-3", 0, "cred-e", M100).at(T0 + 4_000);
    assert_eq!(
        denied(&admit(&store, &third)),
        Some(ReasonCode::BudgetExhausted)
    );
}
