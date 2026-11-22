//! 授信的旧债与新额度**分开记**（owner 2026-11-15 拍板，方案 C；PR #600 第三轮复审 M1 / L1）。
//!
//! 第三轮修复让「没充值、只有授信」的账户按累计债务约束授信，但债务的起点取「最后一次有别的钱覆盖它」：
//!
//! * **M1**：账户一有生效的花费类额度，就改走「额度窗口」，窗口起点之前的债务不再计入。授信 200 用光之后发一笔 10 的额度，
//!   可用额从 -200 变成 +10，又放行两笔 100，授信累计被占了 390；每月自动发免费额度就会触发。
//! * **L1**：被撤销的花费类额度名下已经花掉的钱永远算授信债务，预付额度 1000 下花了 900、撤销后再开 100 授信，
//!   可用额是 -900，新的 100 预留被拒。
//!
//! owner 拍板的口径：**新发的花费类额度照常可花，但不抵旧的授信债务；旧债务继续占着授信，直到上级补钱（充值）。**
//! 平台补贴不替上级还债，孩子的免费额度也不被旧债吃掉。落到账本上：
//!
//! ```text
//! 授信可用 = 授信额度 - 累计授信债务         债务 = 已花（含在途）里由授信承担的部分，不因新额度窗口而重置
//! 能花的   = 此刻有效的额度剩余 + 授信可用    同一笔花费先用额度（先到期的先用），用完才动授信
//! ```
//!
//! 这个文件有两个**独立于被测代码**的模型（`support/ledger_books.rs`）：[`model`] 只读原始表（`allocations`、`allocation_revocations`、
//! `ledger_allocation_marks`、`ledger_entries`），把「额度创建」「额度撤销」「一笔花费」当成同一条时间线上的事件，按提交先后
//! 逐件回放「先额度后授信」，不经过被测的投影函数，也不共用被测实现里「这笔额度能不能覆盖这笔花费」的判断（被测实现拿花费的第一条
//! 分录号和额度的序号直接比，模型靠事件顺序维护「此刻已经存在、没被撤销的额度」）；[`Sim`] 不读库，只吃测试自己做过的事
//! （发额度、撤销、预留、释放、结算），随机历史的期望从它来。两个模型也不照抄被测实现的时间假设（读钟更早时不把已提交的花费压到
//! 读钟上，预留和它超出预留的部分按种类分、不按毫秒合并），所以两边对不上就是实现有 bug。
//! 所有场景都核对金额守恒：额度覆盖的 + 授信承担的 = 分录里的已花，授信承担的永远不超过授信额度。
//! 「Codex 复审（2026-11-22）」那一节的测试不用任何模型，期望数字全部手算。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢，
//! 所以 `max_tokens = 45` 的预留正好是 100，`max_tokens = 10` 是最低有用的预留 30。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_books.rs"]
mod books;
#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use books::*;
use fixture::*;
use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::admission::{AdmissionDecision, AncestorView};
use idoris_compute_contracts::control::WriteResult;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::ledger::{
    CreditLineSetting, FreeTierIssue, FreeTierOutcome, LedgerError, SettleOutcome,
};
use idoris_tenancy::accounts::{AccountStore, GrantAllocation, Meta, Who};

/// `max_tokens = 45`：预留 100。
const M100: u64 = 45;
/// `max_tokens = 10`：最低有用的预留 30。
const M30: u64 = 10;
const THREADS: usize = 24;

/// 2026-12-01T00:00:00Z、2027-01-01T00:00:00Z、2027-02-01T00:00:00Z（账本时区默认 UTC，免费额度窗口是整月）。
const DEC1: i64 = 1_796_083_200_000;
const JAN1: i64 = 1_798_761_600_000;
const FEB1: i64 = 1_801_440_000_000;

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

/// 一个请求：先不允许宽限，被拒（钱不够）再允许宽限试一遍，和云执行器的两遍一样。
fn admit_with_ladder(
    store: &AccountStore,
    credential: &str,
    request: &str,
    max_tokens: u64,
    now: i64,
) -> AdmissionDecision {
    let first = Req::new(store, request, 0, credential, max_tokens).at(now);
    let decision = admit(store, &first);
    if decision.is_admitted() || denied(&decision) != Some(ReasonCode::BudgetExhausted) {
        return decision;
    }
    admit(store, &first.clone().grace())
}

/// 准入一个请求，放行了就在半秒后按预留额结算（预留多少就收多少）。返回准入的决定。
fn pay(
    store: &AccountStore,
    credential: &str,
    request: &str,
    at: i64,
    max_tokens: u64,
) -> AdmissionDecision {
    let req = Req::new(store, request, 0, credential, max_tokens).at(at);
    let decision = admit(store, &req);
    if let Some(admitted) = decision.admitted() {
        let charged = admitted.reservation.price.micros();
        store
            .settle_attempt(&req.attempt, &price(charged), ts(at + 500))
            .unwrap();
    }
    decision
}

/// `emp` 付款的 [`pay`]。
fn spend(store: &AccountStore, request: &str, at: i64, max_tokens: u64) -> AdmissionDecision {
    pay(store, "cred-e", request, at, max_tokens)
}

fn view_of(store: &AccountStore, account: &str, now: i64) -> AncestorView {
    store.account_view(&aid(account), ts(now)).unwrap()
}

fn view(store: &AccountStore, now: i64) -> AncestorView {
    view_of(store, "emp", now)
}

/// 没有充值的账户还能再花多少：额度剩余（净头寸）加授信额度。
fn headroom(view: &AncestorView) -> i64 {
    assert!(
        view.balance.is_none(),
        "headroom() is for unfunded accounts"
    );
    view.ceiling_remaining.as_ref().map_or(0, |m| m.micros) + view.credit_limit.micros
}

/// `emp`（成员）没有充值、没有任何花费类额度；组织 `paid` 资金充足，并且从平台拿到了足够下发额度的额度
/// （S0 要求授权方自己有授权，窗口 `[T0 - 1 天, T0 + 60 天)`）。授信、额度由调用者自己开。
fn credit_only_world(db: &TempDb) -> AccountStore {
    let store = world(db);
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    allocate(&store, "e-0", "platform", "paid", 9_000_000);
    store
}

/// 给 `emp` 开 / 重设授信（存储层，和控制面写的是同一份 `allocations` 记录）。
fn credit_line(store: &AccountStore, account: &str, micros: i64, at: i64) {
    store
        .set_credit_line(
            &aid(account),
            &CreditLineSetting {
                limit: thb(micros),
                valid_until: None,
            },
            ts(at),
        )
        .unwrap();
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

/// 一笔预付额度：`paid` 发给 `emp`，窗口 `[from, until)`，`created` 时刻创建。
fn prepaid(store: &AccountStore, id: &str, micros: i64, from: i64, until: i64, created: i64) {
    store
        .create_allocation(
            &Allocation {
                id: AllocationId::new(id).unwrap(),
                parent: aid("paid"),
                child: aid("emp"),
                ceiling: grant(micros, Purpose::Prepaid, from, until),
                idempotency_key: None,
            },
            ts(created),
        )
        .unwrap();
}

// ------------------------------------------------------------------------------------------
// 独立于被测代码的模型（`support/ledger_books.rs`）：读原始表回放的 `model`，和不读库、只吃测试自己做过的事的 `Sim`
// ------------------------------------------------------------------------------------------

/// 视图和模型对得上，并且金额守恒。返回模型的账。
///
/// 视图把「额度剩余」和「授信已占用」记成净头寸：`ceiling_remaining = 额度剩余 - min(债务, 授信额度)`，授信额度原样放在
/// `credit_limit` 里（债务超过授信额度的部分不再往额度里扣：它再也花不出去，新额度不该被它吃掉）。
fn check_books(store: &AccountStore, db: &TempDb, account: &str, now: i64) -> Books {
    let books = model(db.path(), account, now);
    let v = view_of(store, account, now);
    assert!(v.balance.is_none(), "{account} must be an unfunded account");
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(
            books.allowance_left - books.credit_debt.min(v.credit_limit.micros)
        )),
        "the view disagrees with the model: {books:?} credit line {:?}",
        v.credit_limit
    );
    // 金额守恒：额度覆盖的 + 授信承担的 = 分录里的已花，并且和原生 SQL 对得上
    assert_eq!(books.covered + books.credit_debt, books.spent, "{books:?}");
    assert_eq!(
        books.spent,
        scalar(
            db,
            &format!(
                "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = '{account}'"
            )
        ),
        "{books:?}"
    );
    assert!(books.covered >= 0 && books.credit_debt >= 0, "{books:?}");
    books
}

// ------------------------------------------------------------------------------------------
// M1：新额度照常可花，但不抵旧的授信债务
// ------------------------------------------------------------------------------------------

#[test]
fn a_new_allowance_is_spendable_but_does_not_pay_the_old_credit_debt() {
    // 复审探测的原场景：授信 200 用满（第三笔 BudgetExhausted），随后发一笔额度、窗口自此起。
    // 旧代码：可用额从 -200 变成 +额度，授信又被放行 2 笔，累计占用 390。
    let db = TempDb::new("books-m1");
    let store = credit_only_world(&db);
    open_credit_through_the_control_plane(&store, "cl-1", 200, T0, T0 + 40 * DAY);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let third = admit_with_ladder(&store, "cred-e", "r-3", M100, T0 + 3_000);
    assert_eq!(
        denied(&third),
        Some(ReasonCode::BudgetExhausted),
        "{third:?}"
    );
    let books = check_books(&store, &db, "emp", T0 + 3_000);
    assert_eq!((books.covered, books.credit_debt), (0, 200));

    // 发一笔 100 的预付额度，窗口自此起
    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
    let v = view(&store, later + 1);
    assert_eq!(v.credit_limit, thb(200));
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(-100)),
        "额度 100，旧债 200 仍占满授信：净头寸 -100"
    );
    assert_eq!(headroom(&v), 100, "能花的只有新额度；授信可用仍是 0");
    check_books(&store, &db, "emp", later + 1);

    // 想要 102，只给得出 100（新额度）：压到 100，不是 300
    let req = Req::new(&store, "r-4", 0, "cred-e", M100 + 1).at(later + 1_000);
    let decision = admit(&store, &req);
    let AdmissionDecision::Degrade { admitted, .. } = &decision else {
        panic!("{decision:?}");
    };
    assert_eq!(admitted.reservation.price.micros(), 100, "{decision:?}");
    store
        .settle_attempt(&req.attempt, &price(100), ts(later + 1_500))
        .unwrap();

    // 新额度花完了：授信还是占满的（旧债没有被抵掉），再小的预留也放不下
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (100, 200, 300)
    );
    assert_eq!(
        view(&store, later + 2_000).ceiling_remaining,
        Some(thb(-200))
    );
    let fifth = admit_with_ladder(&store, "cred-e", "r-5", M30, later + 3_000);
    assert_eq!(
        denied(&fifth),
        Some(ReasonCode::BudgetExhausted),
        "{fifth:?}"
    );
    assert!(store.hold(&attempt_of("r-5"), "model").unwrap().is_none());
    assert_clean(&store);
}

#[test]
fn an_allowance_that_expires_unspent_does_not_give_the_credit_back() {
    // 月初发了免费额度、整个月没花：到期之后它不能把旧债勾销（旧代码把「最后一次有额度到期」当成覆盖了之前的一切）
    let db = TempDb::new("books-expire-unspent");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 10 * DAY, later);
    assert_eq!(headroom(&view(&store, later + 1)), 100);

    let after = later + 11 * DAY;
    let v = view(&store, after);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(-200)),
        "没花的额度过期了，旧债一分没少"
    );
    assert_eq!(headroom(&v), 0, "授信仍是 0");
    let books = check_books(&store, &db, "emp", after);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (0, 200, 200)
    );
    let request = admit_with_ladder(&store, "cred-e", "r-3", M30, after + 1_000);
    assert_eq!(
        denied(&request),
        Some(ReasonCode::BudgetExhausted),
        "{request:?}"
    );
    assert_clean(&store);
}

#[test]
fn a_late_issued_allowance_does_not_pay_debt_from_before_it_existed() {
    // 额度的窗口是整个月，但它是月中才发的（或者起点倒填）：它只覆盖发出之后的花费，
    // 发出之前已经记在授信上的 200 不会被它抵掉
    let db = TempDb::new("books-late-issue");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-late", 100, T0 - DAY, T0 + 30 * DAY, later);
    let v = view(&store, later + 1);
    assert_eq!(v.ceiling_remaining, Some(thb(-100)));
    assert_eq!(headroom(&v), 100, "新额度 100 可花");
    let books = check_books(&store, &db, "emp", later + 1);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (100, 0, 200),
        "窗口里更早的 200 不是它覆盖的：额度整整 100 还在"
    );
    // 一分没花，窗口结束：债务还是 200，授信仍占满（如果它追溯抵了窗口里更早的花费，这里会是 100）
    let after = T0 + 31 * DAY;
    assert_eq!(view(&store, after).ceiling_remaining, Some(thb(-200)));
    let books = check_books(&store, &db, "emp", after);
    assert_eq!((books.covered, books.credit_debt), (0, 200));
    let request = admit_with_ladder(&store, "cred-e", "r-3", M30, after);
    assert_eq!(
        denied(&request),
        Some(ReasonCode::BudgetExhausted),
        "{request:?}"
    );
    assert_clean(&store);
}

#[test]
fn a_late_issued_allowance_still_covers_what_is_spent_after_it_was_issued() {
    // 对照：同一笔迟发的额度，发出之后花的 100 由它覆盖，授信债务仍是发出之前的 200
    let db = TempDb::new("books-late-issue-spent");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-late", 100, T0 - DAY, T0 + 30 * DAY, later);
    assert!(spend(&store, "r-3", later + 1_000, M100).is_admitted());
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!(
        (
            books.allowance_left,
            books.covered,
            books.credit_debt,
            books.spent
        ),
        (0, 100, 200, 300)
    );
    assert_eq!(
        view(&store, later + 2_000).ceiling_remaining,
        Some(thb(-200))
    );
    assert_clean(&store);
}

#[test]
fn a_closed_credit_line_does_not_let_the_old_debt_eat_a_new_allowance() {
    // 授信被关掉（或过期）之后，账户没有授信可用，但欠下的债还在；这时新发的额度照常可花，不被旧债吃掉：
    // 额度窗口里更早的 200 是授信债务，不是这笔额度的消耗。
    // （回退到「额度窗口」算法的话，窗口自 T0 - 1 天起，200 会算成额度的消耗，额度变成 -100，一分也花不出去。）
    let db = TempDb::new("books-closed-credit");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    store
        .disable_credit_line(&aid("emp"), ts(T0 + 3_000))
        .unwrap();
    let later = T0 + 10_000;
    prepaid(&store, "emp-late", 100, T0 - DAY, T0 + 30 * DAY, later);
    let v = view(&store, later + 1);
    assert_eq!(v.credit_limit, thb(0), "授信已经关掉");
    assert_eq!(v.ceiling_remaining, Some(thb(100)), "新额度整整 100 可花");
    let books = check_books(&store, &db, "emp", later + 1);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (100, 0, 200)
    );
    assert!(spend(&store, "r-3", later + 1_000, M100).is_admitted());
    let blocked = admit_with_ladder(&store, "cred-e", "r-4", M30, later + 2_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );

    // 上级重新开 200 授信：旧债 200 仍在，授信可用是 0（重设不勾销，也不因为关过一阵就忘掉）
    credit_line(&store, "emp", 200, later + 3_000);
    let v = view(&store, later + 3_500);
    assert_eq!(v.ceiling_remaining, Some(thb(-200)));
    assert_eq!(headroom(&v), 0);
    let books = check_books(&store, &db, "emp", later + 3_500);
    assert_eq!((books.covered, books.credit_debt), (100, 200));
    assert_clean(&store);
}

#[test]
fn grace_is_for_accounts_that_owe_nothing_even_when_the_credit_line_is_closed() {
    // 宽限是「额度用完之后的最后一单」，不是给欠着钱的账户续命。授信开过又关掉：
    // 欠着授信的钱 -> 额度花光也没有宽限；没欠钱 -> 额度花光照常有一次宽限（对照，防止把宽限一刀切掉）。
    for (owes, tag) in [(true, "owes"), (false, "owes-nothing")] {
        let db = TempDb::new(&format!("books-grace-{tag}"));
        let store = credit_only_world(&db);
        credit_line(&store, "emp", 200, T0);
        if owes {
            for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
                assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
            }
        }
        store
            .disable_credit_line(&aid("emp"), ts(T0 + 3_000))
            .unwrap();
        let later = T0 + 10_000;
        prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
        assert!(spend(&store, "r-3", later + 1_000, M100).is_admitted());
        let decision = admit_with_ladder(&store, "cred-e", "r-4", M100, later + 2_000);
        if owes {
            assert_eq!(
                denied(&decision),
                Some(ReasonCode::BudgetExhausted),
                "{tag}: {decision:?}"
            );
            assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
        } else {
            assert!(
                matches!(decision, AdmissionDecision::Grace { .. }),
                "{tag}: {decision:?}"
            );
        }
        assert_clean(&store);
    }
}

#[test]
fn debt_beyond_the_credit_line_does_not_eat_a_new_allowance_either() {
    // 授信被调小（200 -> 100）而债务已经是 200：超出授信的那部分再也花不出去，但新额度也不该被它吃掉
    let db = TempDb::new("books-over-limit");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    credit_line(&store, "emp", 100, T0 + 3_000);
    let v = view(&store, T0 + 4_000);
    assert_eq!(v.credit_limit, thb(100));
    assert_eq!(headroom(&v), 0, "债务 200 已经超过授信 100：什么都不允许");

    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
    let v = view(&store, later + 1);
    assert_eq!(
        headroom(&v),
        100,
        "新额度 100 照常可花，不被超出授信的旧债吃掉"
    );
    check_books(&store, &db, "emp", later + 1);
    assert!(spend(&store, "r-3", later + 1_000, M100).is_admitted());
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!((books.covered, books.credit_debt), (100, 200));
    assert_eq!(headroom(&view(&store, later + 2_000)), 0);
    assert_clean(&store);
}

#[test]
fn setting_the_credit_line_again_with_allowances_around_still_does_not_forgive_the_debt() {
    // 债务只会被还掉（充值），不会被忘掉：重新设置授信（替换）既不勾销债务，也不让新额度去抵它
    let db = TempDb::new("books-reset-credit");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    credit_line(&store, "emp", 200, T0 + 3_000);
    assert_eq!(view(&store, T0 + 3_500).ceiling_remaining, Some(thb(-200)));
    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
    assert_eq!(
        headroom(&view(&store, later + 1)),
        100,
        "重设授信 + 新额度 100：能花的是 100，不是 300"
    );
    // 把授信调大是上级明确多给的授信：旧债仍在，多出来的部分才可用
    credit_line(&store, "emp", 350, later + 2_000);
    assert_eq!(headroom(&view(&store, later + 2_500)), 100 + 150);
    check_books(&store, &db, "emp", later + 2_500);
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 先额度后授信：同一笔花费先用额度（先到期的先用），用完才动授信
// ------------------------------------------------------------------------------------------

#[test]
fn one_request_uses_the_allowance_first_and_only_the_rest_comes_from_credit() {
    let db = TempDb::new("books-allowance-first");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    prepaid(&store, "emp-prepaid-1", 100, T0, T0 + 30 * DAY, T0);
    let v = view(&store, T0 + 500);
    assert_eq!(v.ceiling_remaining, Some(thb(100)));
    assert_eq!(headroom(&v), 300);
    // 一笔 210（max_tokens = 100）横跨额度和授信：额度 100 先用，授信只承担 110
    let req = Req::new(&store, "r-1", 0, "cred-e", 100).at(T0 + 1_000);
    let decision = admit(&store, &req);
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    store
        .settle_attempt(&req.attempt, &price(210), ts(T0 + 1_500))
        .unwrap();
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 100, 110),
        "先额度后授信：授信债务是 110，不是 210"
    );
    let v = view(&store, T0 + 2_000);
    assert_eq!(v.ceiling_remaining, Some(thb(-110)));
    assert_eq!(headroom(&v), 90, "授信还剩 200 - 110");
    assert_clean(&store);
}

#[test]
fn allowances_that_expire_first_are_spent_first() {
    // 两笔额度同时有效：短的（10 天）先用，长的（60 天）留着。短的到期之后长的还是满的
    let db = TempDb::new("books-earliest-expiry-first");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    allocate(&store, "emp-long", "paid", "emp", 1_000);
    prepaid(&store, "emp-short", 100, T0, T0 + 10 * DAY, T0);
    assert!(spend(&store, "r-1", T0 + 1_000, M100).is_admitted());
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        books.allowance_left, 1_000,
        "100 先从先到期的那笔扣，长的一分没动"
    );
    let after = T0 + 11 * DAY;
    let v = view(&store, after);
    assert_eq!(v.ceiling_remaining, Some(thb(1_000)));
    let books = check_books(&store, &db, "emp", after);
    assert_eq!((books.allowance_left, books.credit_debt), (1_000, 0));
    // 短的到期之后再花 100，就只能从长的扣了
    assert!(spend(&store, "r-2", after + 1_000, M100).is_admitted());
    let books = check_books(&store, &db, "emp", after + 2_000);
    assert_eq!(books.allowance_left, 900);
    assert_clean(&store);
}

#[test]
fn in_flight_reservations_use_the_allowance_first_and_a_release_gives_it_back() {
    let db = TempDb::new("books-in-flight");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
    // 一个还没结算的预留占的是新额度（授信已经满了）
    let open = Req::new(&store, "r-open", 0, "cred-e", M100).at(later + 1_000);
    assert!(admit(&store, &open).is_admitted());
    let books = check_books(&store, &db, "emp", later + 1_500);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 100, 200)
    );
    let blocked = admit_with_ladder(&store, "cred-e", "r-blocked", M30, later + 2_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );
    // 放掉它：额度回来了，旧债不变
    store
        .release_attempt(&open.attempt, ts(later + 3_000))
        .unwrap();
    let books = check_books(&store, &db, "emp", later + 3_500);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (100, 0, 200)
    );
    assert!(spend(&store, "r-again", later + 4_000, M100).is_admitted());
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 每月自动发免费额度：连续三个月，授信占用从不超过授信额度
// ------------------------------------------------------------------------------------------

/// 补贴池下的一枝：`kid-a`（成员）经 `teacher`、`school`、`pool` 到平台。链上每个账户的额度铺到 `T0 + 200 天`，
/// 补贴池 11 月到次年 2 月的账期上限都足够；老师给 `kid-a` 授信 200。
fn kid_world(db: &TempDb) -> AccountStore {
    let store = world(db);
    for (id, parent, child) in [
        ("w-0", "platform", "pool"),
        ("w-1", "pool", "school"),
        ("w-2", "school", "teacher"),
    ] {
        store
            .create_allocation(
                &Allocation {
                    id: AllocationId::new(id).unwrap(),
                    parent: aid(parent),
                    child: aid(child),
                    ceiling: grant(1_000_000_000, Purpose::Prepaid, T0 - DAY, T0 + 200 * DAY),
                    idempotency_key: None,
                },
                t0(),
            )
            .unwrap();
    }
    for (y, m) in [(2026, 11), (2026, 12), (2027, 1), (2027, 2)] {
        store
            .set_pool_cap(&aid("pool"), month(y, m), &thb(1_000_000), t0())
            .unwrap();
    }
    credit_line(&store, "kid-a", 200, T0);
    store
}

fn issue_free_tier(store: &AccountStore, y: u16, m: u8, micros: i64, now: i64) -> FreeTierOutcome {
    store
        .issue_free_tier(&FreeTierIssue {
            principal: &pid("p-a"),
            member_account: &aid("kid-a"),
            month: month(y, m),
            amount: thb(micros),
            now: ts(now),
        })
        .unwrap()
}

#[test]
fn a_monthly_free_tier_for_three_months_never_pushes_credit_use_past_the_credit_line() {
    let db = TempDb::new("books-monthly");
    let store = kid_world(&db);
    let kid =
        |request: &str, at: i64, max_tokens: u64| pay(&store, "cred-a", request, at, max_tokens);
    let kid_books = |now: i64| check_books(&store, &db, "kid-a", now);

    // ---- 11 月：免费额度 100 + 授信 200 = 300，三笔 100，第四笔拒绝 ----
    let nov = issue_free_tier(&store, 2026, 11, 100, T0);
    assert!(matches!(nov, FreeTierOutcome::Granted(_)));
    for (n, request) in ["n-1", "n-2", "n-3"].into_iter().enumerate() {
        let at = T0 + 1_000 * (i64::try_from(n).unwrap() + 1);
        assert!(kid(request, at, M100).is_admitted(), "{request}");
    }
    let fourth = admit_with_ladder(&store, "cred-a", "n-4", M30, T0 + 5_000);
    assert_eq!(
        denied(&fourth),
        Some(ReasonCode::BudgetExhausted),
        "{fourth:?}"
    );
    let books = kid_books(T0 + 6_000);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (100, 200, 300),
        "11 月：免费额度先花，授信承担 200"
    );

    // ---- 12 月：新发 100。照常可花，授信仍是满的：只能花这 100 ----
    let dec = DEC1 + 5 * HOUR;
    let granted = issue_free_tier(&store, 2026, 12, 100, dec);
    let FreeTierOutcome::Granted(allocation) = &granted else {
        panic!("{granted:?}");
    };
    assert_eq!(allocation.ceiling.valid_from().millis(), DEC1);
    let v = view_of(&store, "kid-a", dec + 1);
    assert_eq!(v.ceiling_remaining, Some(thb(-100)), "100 - 旧债 200");
    assert_eq!(headroom(&v), 100);
    assert!(kid("d-1", dec + 1_000, M100).is_admitted());
    let second = admit_with_ladder(&store, "cred-a", "d-2", M30, dec + 2_000);
    assert_eq!(
        denied(&second),
        Some(ReasonCode::BudgetExhausted),
        "12 月只能花新发的 100，授信没有因为新额度恢复：{second:?}"
    );
    let books = kid_books(dec + 3_000);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (200, 200, 400)
    );

    // ---- 1 月：新发 100，只花 30，剩下的 70 到月底作废 ----
    let jan = JAN1 + 5 * HOUR;
    issue_free_tier(&store, 2027, 1, 100, jan);
    assert!(kid("j-1", jan + 1_000, M30).is_admitted());
    let books = kid_books(jan + 2_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (70, 230, 200)
    );
    assert_eq!(headroom(&view_of(&store, "kid-a", jan + 2_000)), 70);

    // ---- 2 月：免费额度这个月还没发。1 月没花完的不会变成授信 ----
    let feb = FEB1 + 5 * HOUR;
    let v = view_of(&store, "kid-a", feb);
    assert_eq!(v.ceiling_remaining, Some(thb(-200)), "债务仍是 200");
    assert_eq!(headroom(&v), 0);
    let blocked = admit_with_ladder(&store, "cred-a", "f-1", M30, feb + 1_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );
    let books = kid_books(feb + 2_000);
    assert!(
        books.credit_debt <= 200,
        "授信占用从不超过授信额度: {books:?}"
    );
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (230, 200, 430),
        "三个月一共花了 430：三个月免费额度里花掉的 100 + 100 + 30，加授信 200"
    );

    // 守恒：沿链每一层账都对得上。链上每个账户对同一笔花费各记一条，所以补贴池每个账期的消耗就是成员那个月花掉的钱
    // （含授信承担的那部分：池的上限卡的是整条链的总消耗），老师账上的总额是三个月的总和；池发出去的免费额度每月 100，
    // 2 月没发。
    for (y, m, consumed, granted) in [
        (2026, 11, 300, 100),
        (2026, 12, 100, 100),
        (2027, 1, 30, 100),
        (2027, 2, 0, 0),
    ] {
        let report = store.pool_report(&aid("pool"), month(y, m)).unwrap();
        assert_eq!(
            (report.consumed, report.granted),
            (thb(consumed), thb(granted)),
            "{y}-{m}"
        );
    }
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = 'teacher'"
        ),
        430
    );
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 充值：债务只因上级补钱而减少
// ------------------------------------------------------------------------------------------

#[test]
fn a_top_up_restores_exactly_that_much_credit() {
    // 充值之后账户有了自有资金，余额 = 充值 - 全期已花（带着欠下的钱）：补 X，授信恢复 X，不多不少
    for (top_up, tag) in [(30_i64, "30"), (50, "50"), (150, "150")] {
        let db = TempDb::new(&format!("books-top-up-{tag}"));
        let store = credit_only_world(&db);
        credit_line(&store, "emp", 200, T0);
        for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
            assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
        }
        let blocked = admit_with_ladder(&store, "cred-e", "r-3", M30, T0 + 3_000);
        assert_eq!(
            denied(&blocked),
            Some(ReasonCode::BudgetExhausted),
            "{top_up}: {blocked:?}"
        );

        store
            .fund_account(&aid("emp"), &thb(top_up), &key("f-emp"), ts(T0 + 4_000))
            .unwrap();
        let v = view(&store, T0 + 4_500);
        assert_eq!(v.balance, Some(thb(top_up - 200)), "余额带着欠下的 200");
        assert_eq!(
            v.balance.as_ref().unwrap().micros + v.credit_limit.micros,
            top_up,
            "授信恢复了 {top_up}"
        );
        // 恰好能花 top_up：一笔刚好 top_up 的预留放行，再来最小的一笔就拒绝
        let max_tokens = u64::try_from((top_up - 10) / 2).unwrap();
        let exact = spend(&store, "r-4", T0 + 5_000, max_tokens);
        assert!(exact.is_admitted(), "{top_up}: {exact:?}");
        assert_eq!(
            exact.admitted().unwrap().reservation.price.micros(),
            top_up,
            "{exact:?}"
        );
        let more = admit_with_ladder(&store, "cred-e", "r-5", M30, T0 + 6_000);
        assert_eq!(
            denied(&more),
            Some(ReasonCode::BudgetExhausted),
            "{top_up}: {more:?}"
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
            ),
            200 + top_up,
            "守恒：花掉的 = 授信 200 + 充值 {top_up}"
        );
        assert_clean(&store);
    }
}

// ------------------------------------------------------------------------------------------
// L1：被撤销的额度，撤销之前在额度内的花费不是授信债务
// ------------------------------------------------------------------------------------------

#[test]
fn spending_inside_an_allowance_before_it_was_revoked_is_not_credit_debt() {
    // 复审探测：预付额度 1000 下已花 900，额度被撤销后再开 100 授信。旧代码 ceiling_remaining = -900，新的 100 预留被拒。
    let db = TempDb::new("books-revoked-covered");
    let store = credit_only_world(&db);
    allocate(&store, "emp-prepaid", "paid", "emp", 1_000);
    // 一笔 900（max_tokens = 445）
    let big = spend(&store, "r-1", T0 + 1_000, 445);
    assert!(big.is_admitted(), "{big:?}");
    assert_eq!(big.admitted().unwrap().reservation.price.micros(), 900);
    revoke_allocation_at(&db, "emp-prepaid", T0 + 5_000);
    credit_line(&store, "emp", 100, T0 + 6_000);

    let v = view(&store, T0 + 7_000);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(0)),
        "撤销前在额度内花的 900 不是授信债务（旧代码是 -900）"
    );
    assert_eq!(v.credit_limit, thb(100));
    let books = check_books(&store, &db, "emp", T0 + 7_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 900, 0)
    );
    // 100 授信可用：一笔 100 放行，之后最小的预留也放不下
    assert!(spend(&store, "r-2", T0 + 8_000, M100).is_admitted());
    let blocked = admit_with_ladder(&store, "cred-e", "r-3", M30, T0 + 9_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );
    let books = check_books(&store, &db, "emp", T0 + 9_500);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (900, 100, 1_000)
    );
    assert_clean(&store);
}

#[test]
fn spending_from_the_revocation_instant_on_is_not_covered_by_the_revoked_allowance() {
    // 撤销（提交）之后的花费不被它覆盖：只有授信能承担。
    // 第四轮复审之前这里把撤销先提交、毫秒写成「将来」的 T0 + 5_000，再去花 T0 + 4_999 的一笔，靠毫秒比出「撤销之前」。
    // 撤销行的时刻是撤销请求在进事务之前取的，从来不会比它之后提交的花费更「将来」；而拿毫秒比先后，排队乱序时会让已撤销的额度
    // 回头覆盖撤销之后才提交的花费（见 `a_revoked_allowance_does_not_cover_a_spend_that_committed_after_its_revocation`）。
    // 所以这里按真实的顺序：先花，再撤销（取到的时刻是 T0 + 5_000），再花；断言的数值一个没变。
    let db = TempDb::new("books-revoked-boundary");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    allocate(&store, "emp-prepaid", "paid", "emp", 1_000);

    assert!(spend(&store, "r-1", T0 + 4_999, M100).is_admitted());
    let books = check_books(&store, &db, "emp", T0 + 4_999 + 600);
    assert_eq!(
        (books.covered, books.credit_debt),
        (100, 0),
        "撤销前一毫秒花的 100 在额度内"
    );
    revoke_allocation_at(&db, "emp-prepaid", T0 + 5_000);
    // 撤销那一刻：额度不再授权，这一笔只能走授信
    let at_revocation = spend(&store, "r-2", T0 + 5_000, M100);
    assert!(at_revocation.is_admitted(), "{at_revocation:?}");
    let books = check_books(&store, &db, "emp", T0 + 5_700);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 100, 100)
    );
    assert_eq!(
        headroom(&view(&store, T0 + 5_700)),
        100,
        "授信 200 里还剩 100"
    );
    assert_clean(&store);
}

#[test]
fn an_allowance_window_is_half_open_at_both_ends_when_deciding_what_it_covers() {
    // 窗口 [T0 + 10 秒, T0 + 20 秒)：起点前一毫秒不覆盖，起点那一刻覆盖，终点前一毫秒覆盖，终点那一刻不覆盖
    let db = TempDb::new("books-half-open");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    let (from, until) = (T0 + 10_000, T0 + 20_000);
    prepaid(&store, "emp-window", 100, from, until, from);
    for (request, at, covered_so_far, debt_so_far) in [
        ("r-before", from - 1, 0, 30),
        ("r-start", from, 30, 30),
        ("r-last", until - 1, 60, 30),
        ("r-end", until, 60, 60),
    ] {
        let decision = spend(&store, request, at, M30);
        assert!(decision.is_admitted(), "{request}: {decision:?}");
        let books = check_books(&store, &db, "emp", at + 700);
        assert_eq!(
            (books.covered, books.credit_debt),
            (covered_so_far, debt_so_far),
            "{request}"
        );
    }
    assert_clean(&store);
}

#[test]
fn a_revoked_allowance_is_never_spendable_even_when_the_books_are_read_before_its_revocation() {
    // 撤销不看读的时刻：被撤销的额度不再给任何人授权（`not_revoked_clause` 对所有读额度的地方都是这条规矩）。
    // 读一个早于撤销时刻的时刻，它覆盖过的花费照样算它覆盖的（债务是 0），但它剩下的额度不可花（额度剩余是 0，不是 900）。
    let db = TempDb::new("books-revoked-read-before");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    allocate(&store, "emp-prepaid", "paid", "emp", 1_000);
    assert!(spend(&store, "r-1", T0 + 1_000, M100).is_admitted());
    revoke_allocation_at(&db, "emp-prepaid", T0 + 5_000);

    let v = view(&store, T0 + 3_000);
    assert_eq!(v.ceiling_remaining, Some(thb(0)));
    assert_eq!(headroom(&v), 200, "额度不可花，授信 200 一分没动");
    let books = check_books(&store, &db, "emp", T0 + 3_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 100, 0)
    );
    assert_clean(&store);
}

#[test]
fn a_late_settlement_lands_in_the_window_of_its_settlement_time() {
    // 预留过了 TTL 才结算：过期那一刻额度放回去（`expire` 记在预留创建的时刻上，净额回到 0），迟到的结算记在结算的时刻上
    // （`late_settle`），所以它落在结算那一刻的额度窗口里：额度还在，就由额度覆盖；额度已经到期，就是授信债务（overage）。
    for (window_minutes, covered_by_allowance) in [(60_i64, true), (10, false)] {
        let db = TempDb::new(&format!("books-late-settle-{window_minutes}"));
        let store = credit_only_world(&db);
        credit_line(&store, "emp", 200, T0);
        prepaid(
            &store,
            "emp-prepaid-1",
            100,
            T0,
            T0 + window_minutes * MINUTE,
            T0,
        );
        let req = Req::new(&store, "r-late", 0, "cred-e", M100).at(T0 + 1_000);
        assert!(admit(&store, &req).is_admitted());
        // 在途：占着额度
        let books = check_books(&store, &db, "emp", T0 + 2_000);
        assert_eq!(
            (books.allowance_left, books.covered, books.credit_debt),
            (0, 100, 0)
        );

        // 过了 TTL，扫掉过期的预留：占着的额度放回账上（10 分钟的那笔额度此时已经到期了，放回去也没有额度可用）
        let ttl = store.ledger_config().unwrap().reservation_ttl_ms;
        let settle_at = T0 + 1_000 + ttl + 5_000;
        assert_eq!(store.sweep_expired_holds(ts(settle_at - 1_000)).unwrap(), 1);
        let books = check_books(&store, &db, "emp", settle_at - 500);
        assert_eq!((books.covered, books.credit_debt, books.spent), (0, 0, 0));
        assert_eq!(
            books.allowance_left,
            if covered_by_allowance { 100 } else { 0 },
            "{window_minutes} 分钟的窗口"
        );

        // 迟到的结算：整笔是预留之外的（overage），担保人在授信范围内吸收；账上记在结算那一刻
        let outcome = store
            .settle_attempt(&req.attempt, &price(100), ts(settle_at))
            .unwrap();
        assert!(outcome.late, "{outcome:?}");
        let books = check_books(&store, &db, "emp", settle_at + 500);
        let expected = if covered_by_allowance {
            (0, 100, 0)
        } else {
            (0, 0, 100)
        };
        assert_eq!(
            (books.allowance_left, books.covered, books.credit_debt),
            expected,
            "{window_minutes} 分钟的窗口：{books:?}"
        );
        assert_clean(&store);
    }
}

// ------------------------------------------------------------------------------------------
// 先后看提交顺序，不看入事务之前取的毫秒（PR #600 第四轮复审 Medium 3）
// ------------------------------------------------------------------------------------------
//
// 额度的创建时间、撤销时间、花费的归属时间，都是调用方在**进事务之前**取的毫秒（HTTP 处理函数取时之后还要等工作线程和写锁）。
// 排队时间能让「先提交」和「时间更早」对不上，所以拿毫秒比先后，会让后来才存在的额度追溯偿还它存在之前的授信债务
// （方案 C 的底线：新额度不抵旧债），或者让已经撤销的额度继续覆盖撤销之后的花费。账本在额度创建 / 撤销的事务里记下
// 「此刻账本里最后一条分录是第几号」（`ledger_allocation_marks`，迁移 0004），一笔花费的先后看它第一条分录的号。
//
// 这几个测试里「额度窗口结束之后授信仍占满」是区分点：旧口径下新额度追溯覆盖了旧花费，窗口结束之后债务凭空少了一块。
// 表里的时间故意写成和提交顺序相反：`prepaid(.., created)` 的 `created` 是发额度的请求取到的毫秒，调用顺序才是提交顺序。

/// 授信 200 已经被 r-1（T0 + 1_100）、r-2（T0 + 1_200）用光。返回时 emp 的授信债务是 200。
fn credit_used_up_by_two_spends(db: &TempDb) -> AccountStore {
    let store = credit_only_world(db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_100), ("r-2", T0 + 1_200)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    store
}

/// 额度窗口结束之后：没有人用过这笔额度，授信仍占满（债务 200），再小的预留也放不下。
fn assert_old_debt_survives_the_allowance(store: &AccountStore, db: &TempDb, after: i64) {
    let books = check_books(store, db, "emp", after);
    assert_eq!(
        (
            books.allowance_left,
            books.covered,
            books.credit_debt,
            books.spent
        ),
        (0, 0, 200, 200),
        "额度过期了，没有覆盖过任何一笔花费"
    );
    let v = view(store, after);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(-200)),
        "新额度没有被后来的请求用过，窗口结束了，旧的授信债务一分没少"
    );
    assert_eq!(headroom(&v), 0, "授信仍是 0");
    let blocked = admit_with_ladder(store, "cred-e", "r-after", M30, after + 1_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );
    assert_clean(store);
}

#[test]
fn an_allowance_that_commits_after_the_spends_it_was_timed_before_does_not_pay_their_debt() {
    // 复审场景（排队乱序）：发额度的请求先取到时间 T0 + 1_000，随后等工作线程 / 锁；另一个请求在 T0 + 1_100 起花光了
    // 200 授信并结算。发额度的事务随后才提交，它保存的创建时间（T0 + 1_000）比那两笔花费的归属时间早，
    // 可它是在两笔花费**之后**才存在的。旧口径把 T0 + 1_100 的旧花费算成这笔后来才存在的额度覆盖的，债务从 200 降到 100。
    let db = TempDb::new("books-queued-issue");
    let store = credit_used_up_by_two_spends(&db);
    assert_eq!(view(&store, T0 + 2_000).ceiling_remaining, Some(thb(-200)));

    prepaid(
        &store,
        "emp-queued",
        100,
        T0 - DAY,
        T0 + 30 * DAY,
        T0 + 1_000,
    );
    let v = view(&store, T0 + 2_000);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(-100)),
        "额度整整 100 还在，旧债 200"
    );
    assert_eq!(headroom(&v), 100, "能花的只有新额度；授信仍是 0");
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (100, 0, 200),
        "旧花费不是这笔后来才存在的额度覆盖的"
    );

    assert_old_debt_survives_the_allowance(&store, &db, T0 + 31 * DAY);
}

#[test]
fn an_allowance_stamped_in_the_same_millisecond_as_an_earlier_spend_does_not_pay_for_it() {
    // 同毫秒：花费先提交（归属时间 T0 + 2_000），额度后提交、创建时间也是 T0 + 2_000。两个时间一样，毫秒分不出先后；
    // 旧口径把「创建时刻 <= 花费时刻」当成额度先存在。
    let db = TempDb::new("books-same-ms");
    let store = credit_used_up_by_two_spends(&db);
    prepaid(
        &store,
        "emp-same-ms",
        100,
        T0 - DAY,
        T0 + 30 * DAY,
        T0 + 1_200,
    );
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (100, 0, 200)
    );
    assert_old_debt_survives_the_allowance(&store, &db, T0 + 31 * DAY);
}

#[test]
fn an_allowance_committed_first_covers_a_spend_in_the_same_millisecond() {
    // 对照：额度先提交，花费后来、同一毫秒——那一笔是它存在之后的花费，由它覆盖，授信一分没动
    let db = TempDb::new("books-same-ms-first");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    prepaid(
        &store,
        "emp-first",
        100,
        T0 - DAY,
        T0 + 30 * DAY,
        T0 + 1_000,
    );
    assert!(spend(&store, "r-1", T0 + 1_000, M100).is_admitted());
    let after = T0 + 31 * DAY;
    let v = view(&store, after);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(0)),
        "额度覆盖了它，授信没有债务"
    );
    assert_eq!(headroom(&v), 200);
    let books = check_books(&store, &db, "emp", after);
    assert_eq!((books.covered, books.credit_debt), (100, 0));
    assert_clean(&store);
}

#[test]
fn a_spend_committed_after_the_allowance_is_covered_even_when_its_timestamp_is_older() {
    // 反方向：额度先提交（创建时间 T0 + 5_000），花费排在它后面提交，但它的归属时间（T0 + 1_000）比额度的创建时间早。
    // 准入那一刻额度已经在库里，窗口也包含 T0 + 1_000，所以这笔花费用的是额度；之后任何时候再读，账都必须一样：
    // 旧口径按毫秒，把它算成额度创建之前的花费，授信平白多出 100 的债务。
    let db = TempDb::new("books-stale-spend");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    prepaid(
        &store,
        "emp-first",
        100,
        T0 - DAY,
        T0 + 30 * DAY,
        T0 + 5_000,
    );
    let decision = spend(&store, "r-1", T0 + 1_000, M100);
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    let v = view(&store, T0 + 6_000);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(0)),
        "额度 100 被它用掉了，授信没有债务"
    );
    assert_eq!(headroom(&v), 200);
    let books = check_books(&store, &db, "emp", T0 + 6_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 100, 0)
    );
    let after = T0 + 31 * DAY;
    let v = view(&store, after);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(0)),
        "窗口结束之后也一样：这一笔是额度覆盖的，不是授信债务"
    );
    assert_eq!(headroom(&v), 200, "授信一分没动");
    let books = check_books(&store, &db, "emp", after);
    assert_eq!((books.covered, books.credit_debt), (100, 0));
    assert_clean(&store);
}

#[test]
fn a_revoked_allowance_does_not_cover_a_spend_that_committed_after_its_revocation() {
    // 撤销同理：撤销请求取到的毫秒（T0 + 5_000）比后来提交的花费的归属时间（T0 + 4_000）晚。撤销提交之后，额度不再授权：
    // 准入那一刻这笔花费只能走授信，之后再读，账也必须是「授信承担」，不能因为撤销的毫秒更晚，就让已撤销的额度回头覆盖它，
    // 把授信空出来。
    let db = TempDb::new("books-queued-revoke");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    allocate(&store, "emp-prepaid", "paid", "emp", 1_000);
    revoke_allocation_at(&db, "emp-prepaid", T0 + 5_000);
    assert!(spend(&store, "r-1", T0 + 4_000, M100).is_admitted());
    let v = view(&store, T0 + 6_000);
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(-100)),
        "撤销之后才提交的花费是授信债务"
    );
    assert_eq!(headroom(&v), 100, "授信 200 里还剩 100");
    let books = check_books(&store, &db, "emp", T0 + 6_000);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (0, 0, 100)
    );
    assert_clean(&store);
}

#[test]
fn an_allocation_that_lost_its_order_mark_fails_admission_closed() {
    // 每笔额度都由触发器记了创建序号、每笔撤销都记了撤销序号。缺了就是库被改坏了（先拆掉只追加的触发器才删得掉）：
    // 失败即关闭，不能按「没有序号」当成「很早就存在」，让它覆盖不该覆盖的花费、把授信空出来。
    for (what, drop_mark) in [("creation", "'created'"), ("revocation", "'revoked'")] {
        let db = TempDb::new(&format!("books-lost-mark-{what}"));
        let store = credit_only_world(&db);
        credit_line(&store, "emp", 200, T0);
        prepaid(&store, "emp-prepaid", 100, T0, T0 + 30 * DAY, T0);
        revoke_allocation_at(&db, "emp-prepaid", T0 + 5_000);
        let conn = rusqlite::Connection::open(db.path()).unwrap();
        conn.execute_batch(&format!(
            "DROP TRIGGER ledger_allocation_marks_append_only_delete;
             DELETE FROM ledger_allocation_marks WHERE allocation_id = 'emp-prepaid' AND event = {drop_mark}"
        ))
        .unwrap();
        let req = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 6_000);
        let error = store
            .admit_attempt(&req.input())
            .expect_err("admission must fail closed");
        assert!(
            matches!(error, LedgerError::Corrupt(_)),
            "{what}: {error:?}"
        );
        assert!(
            store.hold(&req.attempt, "model").unwrap().is_none(),
            "{what}: nothing was reserved"
        );
        assert!(
            store.account_view(&aid("emp"), ts(T0 + 6_000)).is_err(),
            "{what}: the view fails closed too"
        );
    }
}

// ------------------------------------------------------------------------------------------
// Codex 复审（2026-11-22）：超出预留的部分、读钟乱序——没充值的账户在 main 上就有同样的两条造钱的路径
// ------------------------------------------------------------------------------------------
//
// 期望数字全部从输入事件手算（推导写在注释里），不经过被测代码，也不经过 `model` 和 `Sim`。区分点是「同一笔钱不能花两次」：
// 任何时刻已花（含在途）不超过 额度 + 授信（被撤销额度没花掉的部分不算）。

/// 结算一个预留。
fn settle_at(store: &AccountStore, req: &Req, charged: i64, at: i64) -> SettleOutcome {
    store
        .settle_attempt(&req.attempt, &price(charged), ts(at))
        .unwrap()
}

#[test]
fn the_overage_of_an_unexpired_settlement_spends_the_allowance_of_its_own_moment_once() {
    // 授信 100；额度 100。
    //
    //   T0 + 9    预留 100：额度的窗口 [T0 + 10, ..) 还没开始（或者额度这时还不存在），授信 100 全部占用，债务 100
    //   T0 + 10   按 200 结算，超出预留 100
    //
    // 结算那一刻额度生效、一分没动：能吸收超出部分的是额度 100 + 授信可用 (100 - 100) = 100，担保人承担 100，没有平台损失。
    // 这 100 就是额度花掉的：结算之后额度剩余 0、债务 100、能花的 0 + (100 - 100) = 0。
    // 旧代码把整笔 200 记在预留的时刻 T0 + 9 上，额度够不着它：债务 200、额度仍剩 100，净头寸 100 - min(200, 100) = 0，
    // 能花的是 0 + 授信额度 100，一笔 100 的请求照样放行：花了 200 + 100，只有额度 100 + 授信 100。
    for (label, allowance_exists_before_the_reservation) in [
        ("the window starts after the reservation", true),
        ("the allowance is created after the reservation", false),
    ] {
        let db = TempDb::new(&format!(
            "books-overage-attribution-{allowance_exists_before_the_reservation}"
        ));
        let store = credit_only_world(&db);
        credit_line(&store, "emp", 100, T0);
        if allowance_exists_before_the_reservation {
            prepaid(&store, "emp-new", 100, T0 + 10, T0 + 10 + 30 * DAY, T0 + 5);
        }
        let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 9);
        assert_eq!(
            admit(&store, &first)
                .admitted()
                .map(|a| a.reservation.price.micros()),
            Some(100),
            "{label}"
        );
        if !allowance_exists_before_the_reservation {
            // 窗口早就开始了，但它是预留提交之后才存在的：覆盖不到那一笔预留
            prepaid(&store, "emp-new", 100, T0 - DAY, T0 + 30 * DAY, T0 + 9);
        }

        let outcome = settle_at(&store, &first, 200, T0 + 10);
        assert!(!outcome.late, "{label}");
        assert_eq!(outcome.overage, thb(100), "{label}");
        assert_eq!(outcome.user_borne, thb(100), "{label}");
        assert_eq!(
            (
                outcome.sponsor_covered.clone(),
                outcome.platform_loss.clone()
            ),
            (thb(100), thb(0)),
            "{label}: {outcome:?}"
        );
        assert!(!outcome.credential_frozen, "{label}");

        let after = view(&store, T0 + 11);
        assert_eq!(
            after.ceiling_remaining,
            Some(thb(-100)),
            "{label}: allowance 0 - debt 100"
        );
        assert_eq!(headroom(&after), 0, "{label}: 200 spent of 200");
        let next = Req::new(&store, "r-2", 0, "cred-e", M100).at(T0 + 12);
        let rejected = admit(&store, &next);
        assert_eq!(
            denied(&rejected),
            Some(ReasonCode::BudgetExhausted),
            "{label}: {rejected:?}"
        );
        let laddered = admit_with_ladder(&store, "cred-e", "r-3", M30, T0 + 13);
        assert_eq!(
            denied(&laddered),
            Some(ReasonCode::BudgetExhausted),
            "{label}: {laddered:?}"
        );
        assert_eq!(
            scalar(
                &db,
                "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
            ),
            200,
            "{label}"
        );
        assert_eq!(store.loss_total("platform_loss").unwrap(), 0, "{label}");
        assert_clean(&store);
    }
}

#[test]
fn an_old_allowance_revoked_after_the_reservation_and_a_new_one_in_the_same_millisecond_do_not_merge_the_overage_back()
 {
    // 预留、撤销旧额度、发新额度、结算全在同一毫秒 X = T0 + 10，只有调用顺序（提交顺序）分得出先后。授信 100，旧额度 100，新额度 100，一共 300。
    //
    //   预留 150：旧额度 100（还没撤销）+ 授信 50
    //   撤销 `old`（预留之后），发 `new`（窗口从 X 起，撤销之后）
    //   按 250 结算，超出预留 100
    //
    // 结算前：旧额度只覆盖预留，新额度一分没动；能花的 = 额度 100 + (授信 100 - 债务 50) = 150，担保人吸收超出的 100，没有平台损失。
    // 超出的 100 是撤销和发新额度之后才提交的：旧额度不再覆盖，新额度覆盖，结算之后额度 0、债务 50、能花的 0 + (100 - 50) = 50
    // （旧代码在同一毫秒把超出部分并回预留那一笔：新额度够不着它，债务 150，新额度还剩 100，能花的是 100，一笔 100 的请求照样放行）。
    // 下一笔 100 的请求只能降到 50；总共花了 250 + 50 = 300，正好是全部的钱。
    let db = TempDb::new("books-overage-same-ms");
    let store = credit_only_world(&db);
    let x = T0 + 10;
    credit_line(&store, "emp", 100, T0);
    prepaid(&store, "old", 100, T0 - DAY, T0 + 30 * DAY, T0);

    let first = Req::new(&store, "r-1", 0, "cred-e", 70).at(x);
    assert_eq!(
        admit(&store, &first)
            .admitted()
            .map(|a| a.reservation.price.micros()),
        Some(150)
    );
    revoke_allocation_at(&db, "old", x);
    prepaid(&store, "new", 100, x, x + 30 * DAY, x);

    let outcome = settle_at(&store, &first, 250, x);
    assert_eq!(outcome.overage, thb(100));
    assert_eq!(outcome.user_borne, thb(150));
    assert_eq!(
        (
            outcome.sponsor_covered.clone(),
            outcome.platform_loss.clone()
        ),
        (thb(100), thb(0)),
        "room before the settlement: 100 + (100 - 50) = 150: {outcome:?}"
    );
    assert!(!outcome.credential_frozen);

    let after = view(&store, x + 1);
    assert_eq!(
        after.ceiling_remaining,
        Some(thb(-50)),
        "allowance 0 - debt 50"
    );
    assert_eq!(headroom(&after), 50);
    let next = admit(
        &store,
        &Req::new(&store, "r-2", 0, "cred-e", M100).at(x + 2),
    );
    assert_eq!(
        next.admitted().map(|a| a.reservation.price.micros()),
        Some(50),
        "only 50 of the 300 is left: {next:?}"
    );
    assert!(
        matches!(next, AdmissionDecision::Degrade { .. }),
        "{next:?}"
    );
    let none = admit_with_ladder(&store, "cred-e", "r-3", M30, x + 3);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_eq!(store.loss_total("platform_loss").unwrap(), 0);
    assert_clean(&store);
}

#[test]
fn a_request_that_read_the_clock_earlier_cannot_move_a_committed_spend_into_a_revoked_allowance() {
    // 授信 100；旧额度 100，窗口是毫秒级的 [T0, T0 + 100)。调用顺序（提交顺序）：
    //
    //   A  T0 + 101 准入 100：额度窗口已经过了，用的是授信，债务 100
    //   撤销旧额度（A 提交之后）
    //   B  读钟读到的是 T0 + 99（早于 A，排队才拿到写锁），准入 100
    //
    // A 的花费归属时刻是 T0 + 101，不在旧额度的窗口里，所以是授信承担的：债务 100；旧额度已撤销，不可花：
    // B 能花的 0 + (100 - 100) = 0，拒绝。旧代码把 A 的归属时刻按 B 的读钟压到 T0 + 99，落进旧额度的窗口、算成它覆盖的
    // （撤销在 A 之后提交，照样覆盖）：债务 0，授信又是满的，B 放行——同一笔 100 的授信花了两次。
    let db = TempDb::new("books-stale-read-revoked");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 100, T0);
    prepaid(&store, "old", 100, T0, T0 + 100, T0);
    let a = Req::new(&store, "a-1", 0, "cred-e", M100).at(T0 + 101);
    let decision = admit(&store, &a);
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    revoke_allocation_at(&db, "old", T0 + 102);

    let b = Req::new(&store, "b-1", 0, "cred-e", M100).at(T0 + 99);
    let rejected = admit(&store, &b);
    assert_eq!(
        denied(&rejected),
        Some(ReasonCode::BudgetExhausted),
        "A spent the credit at T0 + 101, outside the window: {rejected:?}"
    );
    assert!(store.hold(&b.attempt, "model").unwrap().is_none());
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
        ),
        100,
        "only A is on the books"
    );
    assert_clean(&store);
}

#[test]
fn a_spend_that_belongs_to_an_allowance_starting_after_the_read_time_still_uses_that_allowance() {
    // 授信 100；额度 `b2` 100，窗口 [T0 + 100, T0 + 200)。
    //
    //   S  T0 + 150 准入 100：b2 在窗口里，先用 b2 的 100，授信一分没动
    //   B  读钟读到的是 T0 + 99，准入 100：b2 在 B 的时刻还没生效，能用的只有授信 100——而 S 用的是 b2，授信是满的，放行
    //
    // 只回放读钟那一刻已经开始的额度，S 找不到 b2，被算成授信债务，B 就被无谓地拒绝了。
    // 放行之后：B 用授信 100，S 用 b2 100，一共 200 = 全部的钱，再来一笔拒绝。
    let db = TempDb::new("books-future-allowance");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 100, T0);
    prepaid(&store, "b2", 100, T0 + 100, T0 + 200, T0);
    let s = Req::new(&store, "s-1", 0, "cred-e", M100).at(T0 + 150);
    assert_eq!(
        admit(&store, &s)
            .admitted()
            .map(|a| a.reservation.price.micros()),
        Some(100)
    );
    let b = Req::new(&store, "b-1", 0, "cred-e", M100).at(T0 + 99);
    let decision = admit(&store, &b);
    assert_eq!(
        decision.admitted().map(|a| a.reservation.price.micros()),
        Some(100),
        "S used b2, so the credit is intact: {decision:?}"
    );
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    let c = Req::new(&store, "c-1", 0, "cred-e", M30).at(T0 + 151);
    let none = admit(&store, &c);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
        ),
        200
    );
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 并发：24 个线程同时来，额度和授信各自恰好用掉该用的
// ------------------------------------------------------------------------------------------

/// 让每个请求各自在一个线程里（各开一条连接）同时放行，返回各自的准入决定。竞争期间采样者线程反复读 `emp` 的投影和
/// 独立模型：任何时刻授信债务都不超过授信额度，已花（含在途）不超过额度加授信。
fn race(db: &TempDb, requests: Vec<Req>, now: i64, spend_cap: i64) -> Vec<AdmissionDecision> {
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
                let view = store.account_view(&aid("emp"), ts(now)).unwrap();
                let books = model(&path, "emp", now);
                assert!(
                    books.credit_debt <= view.credit_limit.micros,
                    "the credit debt {} went beyond the credit line {}: {books:?}",
                    books.credit_debt,
                    view.credit_limit.micros
                );
                assert!(
                    books.spent <= spend_cap,
                    "spent {} (settled + in flight) went beyond the {spend_cap} that allowances and credit allow",
                    books.spent
                );
                assert_eq!(books.covered + books.credit_debt, books.spent);
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
    decisions
}

#[test]
fn concurrent_admissions_split_exactly_between_a_new_allowance_and_the_credit_line() {
    // 场景 A：授信 200 已经用满，新发 100：24 个并发请求恰好放行 1 个（新额度），授信一分没多占
    let db = TempDb::new("books-race-a");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("race-a-{i}"), 0, "cred-e", M100).at(later + 1_000))
        .collect();
    let decisions = race(&db, requests, later + 1_000, 300);
    let admitted = decisions.iter().filter(|d| d.is_admitted()).count();
    assert_eq!(admitted, 1, "{decisions:?}");
    assert!(
        decisions
            .iter()
            .filter(|d| !d.is_admitted())
            .all(|d| denied(d) == Some(ReasonCode::BudgetExhausted)),
        "{decisions:?}"
    );
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (100, 200, 300)
    );
    assert_eq!(
        view(&store, later + 2_000).ceiling_remaining,
        Some(thb(-200))
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_allowance_plus_credit_line() {
    // 场景 B：没有旧债，额度 100 + 授信 200 = 300：24 个并发请求恰好放行 3 个，授信占用恰好 200
    let db = TempDb::new("books-race-b");
    let store = credit_only_world(&db);
    credit_line(&store, "emp", 200, T0);
    prepaid(&store, "emp-prepaid-1", 100, T0, T0 + 30 * DAY, T0);
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("race-b-{i}"), 0, "cred-e", M100).at(T0 + 1_000))
        .collect();
    let decisions = race(&db, requests, T0 + 1_000, 300);
    let admitted = decisions.iter().filter(|d| d.is_admitted()).count();
    assert_eq!(admitted, 3, "{decisions:?}");
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (100, 200, 300)
    );
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 有充值的账户（issue #601）：第五轮留着不动的对照，现在也按方案 C 记账
// ------------------------------------------------------------------------------------------

#[test]
fn an_account_with_funds_keeps_its_old_debt_apart_from_a_new_allowance() {
    // 第五轮这里是一条「有充值的账户行为不变」的对照：余额 = 充值 - 全期已花，带着欠下的钱，新额度不是花费的来源（可用额 = 余额与
    // 额度剩余里较小的），余额 -100 时新额度 100 一分也花不出去。issue #601 把方案 C 延伸到有充值的账户（S3-22）：同样的历史，
    // 新额度照常可花，不抵旧债，授信仍是占满的。（范围、顺序、告警、并发的场景在 `ledger_credit_books_funded.rs`。）
    let db = TempDb::new("books-funded-control");
    let store = credit_only_world(&db);
    store
        .fund_account(&aid("emp"), &thb(100), &key("f-emp"), t0())
        .unwrap();
    credit_line(&store, "emp", 100, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    assert_eq!(
        view(&store, T0 + 3_000).balance,
        Some(thb(-100)),
        "还没有任何额度：S0 自己的余额，没变"
    );

    let later = T0 + 10_000;
    prepaid(&store, "emp-prepaid-1", 100, later, later + 30 * DAY, later);
    let v = view(&store, later + 1);
    assert_eq!(v.balance, None, "自有资金并进了净头寸");
    assert_eq!(
        v.ceiling_remaining,
        Some(thb(0)),
        "额度 100，旧债 100 仍占满授信 100：净头寸 0，不是旧代码的 -100"
    );
    assert_eq!(v.credit_limit, thb(100));
    // 新额度 100 照常可花（30 + 70），授信一分没动
    assert!(spend(&store, "r-3", later + 1_000, M30).is_admitted());
    assert!(spend(&store, "r-4", later + 2_000, 30).is_admitted());
    let blocked = admit_with_ladder(&store, "cred-e", "r-5", M30, later + 3_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "额度 100 花完了，授信仍被旧债占满：{blocked:?}"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
        ),
        300
    );
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 随机历史：视图、两个独立模型、准入决定、结算分摊对得上
// ------------------------------------------------------------------------------------------
//
// 期望从 [`Sim`] 来：它只吃这个测试自己做过的事（发额度、撤销额度、预留、释放、结算）和业务规则，不读库、不读被测实现。
// 每一步之后，视图、读原始表回放的 `model`（账本的分录有没有记对）、`Sim` 三方必须逐项一致。历史里有超出预留的结算
// （超出的部分记在结算那一刻）、读钟乱序（`jitter`）和迟发、起点倒填的额度。

/// xorshift64*：不引入依赖，结果确定。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> i64 {
        i64::try_from(self.next() % n).unwrap()
    }
}

/// 按模型预测一个预留 `reserve` 微泰铢的请求在账上的结果：能花的 = 额度剩余 + 授信可用；
/// 放得下就全额放行，放不下但够最低有用的预留（30）就压到「能花的」里最大的偶数，再小就拒绝。
fn expected_price(books: &Books, credit: i64, reserve: i64) -> Option<i64> {
    let room = books.room(credit);
    if reserve <= room {
        Some(reserve)
    } else if room >= 30 {
        Some(room - (room % 2))
    } else {
        None
    }
}

/// 视图、读原始表回放的 `model`、不读库的 `Sim` 三方一致。返回账。
fn check_against_sim(store: &AccountStore, db: &TempDb, sim: &Sim, now: i64) -> Books {
    let books = check_books(store, db, "emp", now);
    assert_eq!(
        books,
        sim.books(now),
        "the entries in the ledger replay to something other than what the events say, read at {now}"
    );
    books
}

/// 结算 `req`（预留 `reserved`，实际 `charged`），同步到 `Sim`，并核对这次结算的分摊：调用方保证超出预留的部分不超过此刻整条链
/// 能吸收的，所以全由担保人承担，没有平台损失，不冻结凭证。
#[allow(clippy::too_many_arguments)]
fn settle_both(
    store: &AccountStore,
    sim: &mut Sim,
    req: &Req,
    spend: usize,
    reserved: i64,
    charged: i64,
    at: i64,
    reached: &mut Reached,
) {
    let extra = (charged - reserved).max(0);
    if extra > 0 {
        reached.overage_settled += 1;
        if sim.books(at).allowance_left > 0 {
            reached.overage_with_allowance += 1;
        }
    }
    let outcome = settle_at(store, req, charged, at);
    assert!(!outcome.late, "{outcome:?}");
    assert_eq!(outcome.overage, thb(extra), "{outcome:?}");
    assert_eq!(outcome.sponsor_covered, thb(extra), "{outcome:?}");
    assert_eq!(outcome.platform_loss, thb(0), "{outcome:?}");
    assert!(!outcome.credential_frozen, "{outcome:?}");
    sim.settle(spend, charged, at);
}

/// 随机历史走过了哪些有意思的状态（防止随机测试悄悄退化成只走平凡路径）。
#[derive(Debug, Default, Clone, Copy)]
struct Reached {
    allowed: u32,
    degraded: u32,
    rejected: u32,
    /// 额度剩余和授信债务同时大于 0：「新额度照常可花、旧债不被抵」的状态。
    allowance_and_debt: u32,
    /// 授信被占满（债务 = 授信额度）。
    credit_full: u32,
    /// 撤销了一笔已经覆盖过花费的额度（L1）。
    revoked_after_cover: u32,
    released: u32,
    half_settled: u32,
    in_flight_at_check: u32,
    /// 已经发过额度，但此刻额度剩余是 0（花光、到期或被撤销之后），债务仍留着。
    debt_without_allowance: u32,
    /// 超出预留的结算。
    overage_settled: u32,
    /// 超出预留的结算，结算那一刻还有额度可花：超出的部分用了结算那一刻的额度。
    overage_with_allowance: u32,
    /// 写进表里的毫秒取的是排队之前的旧时间的步骤数（只有抖动的随机历史才有）。
    stale_stamped: u32,
    /// 读钟比已经提交的某笔花费的归属时刻更早的请求数：回放不能把那笔花费压到读钟上。
    read_behind_a_spend: u32,
}

impl Reached {
    fn add(&mut self, other: Reached) {
        self.allowed += other.allowed;
        self.degraded += other.degraded;
        self.rejected += other.rejected;
        self.allowance_and_debt += other.allowance_and_debt;
        self.credit_full += other.credit_full;
        self.revoked_after_cover += other.revoked_after_cover;
        self.released += other.released;
        self.half_settled += other.half_settled;
        self.in_flight_at_check += other.in_flight_at_check;
        self.debt_without_allowance += other.debt_without_allowance;
        self.overage_settled += other.overage_settled;
        self.overage_with_allowance += other.overage_with_allowance;
        self.stale_stamped += other.stale_stamped;
        self.read_behind_a_spend += other.read_behind_a_spend;
    }
}

/// `jitter`：一半的步骤写进表里的毫秒（额度的创建时间、撤销时间、请求的读钟）比提交顺序更早（排队乱序，最多早 6 秒），也会和
/// 前一步相同（同毫秒）；先后仍是调用顺序。
fn run_random_history(seed: u64, steps: usize, jitter: bool) -> Reached {
    const CREDIT: i64 = 200;
    let db = TempDb::new(&format!("books-random-{seed}-{jitter}"));
    let store = credit_only_world(&db);
    credit_line(&store, "emp", CREDIT, T0);
    let mut sim = Sim::new(CREDIT);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut now = T0;
    // （额度 id，它在 Sim 里的编号）
    let mut allowances: Vec<(String, usize)> = Vec::new();
    let mut revoked: Vec<String> = Vec::new();
    // 在途的请求，和它在 Sim 里的花费编号
    let mut open: Vec<(Req, usize)> = Vec::new();
    let mut next_request = 0_u32;
    let mut reached = Reached::default();
    for step in 0..steps {
        now += 1_000 * (1 + rng.below(4));
        // 这一步写进表里的毫秒（额度的创建时间、撤销时间、请求的读钟）。不抖动时就是 `now`；抖动时一半的步骤取到的是排队之前的
        // 旧时间（最多早 6 秒），所以会比前面已经提交的花费更早，也会和前一步的毫秒相同。
        let stamp = if jitter && rng.below(2) == 0 {
            reached.stale_stamped += 1;
            // 授信是 T0 起生效的：读钟不早于 T0（更早时授信还没生效，模型里的授信额度是个常数）
            (now - 1_000 * rng.below(7)).max(T0)
        } else {
            now
        };
        match rng.below(10) {
            0 | 1 => {
                // 新额度：起点在 [stamp - 2 秒, stamp + 3 秒]，长度 5 ~ 44 秒，额度 50 / 100 / 150 / 250。
                // 抖动的历史里一半的额度是「迟发、起点倒填」的：起点比创建时刻早 3 ~ 10 秒，窗口里已经有更早的花费，
                // 它们不是这笔额度覆盖的（只有创建之后提交的花费才可能是）。
                let backdated = jitter && rng.below(2) == 0;
                let (from, until) = if backdated {
                    (
                        stamp - 1_000 * (3 + rng.below(8)),
                        stamp + 1_000 * (5 + rng.below(40)),
                    )
                } else {
                    let from = stamp + 1_000 * (rng.below(6) - 2);
                    (from, from + 1_000 * (5 + rng.below(40)))
                };
                let cap = [50_i64, 100, 150, 250][usize::try_from(rng.below(4)).unwrap()];
                let id = format!("rnd-{seed}-{step}");
                prepaid(&store, &id, cap, from, until, stamp);
                let bucket = sim.allowance(from, until, cap);
                allowances.push((id, bucket));
            }
            2 => {
                if let Some((id, bucket)) = allowances
                    .iter()
                    .find(|(id, _)| !revoked.contains(id))
                    .cloned()
                {
                    if sim.books(now).covered > 0 {
                        reached.revoked_after_cover += 1;
                    }
                    revoke_allocation_at(&db, &id, stamp);
                    sim.revoke(bucket);
                    revoked.push(id);
                }
            }
            3 => {
                // 收尾一个在途的预留：释放，按一半结算，或者超出预留结算（额外的部分不超过结算那一刻整条链能吸收的）
                if let Some((req, spend)) = open.pop() {
                    let reserved = store
                        .hold(&req.attempt, "model")
                        .unwrap()
                        .unwrap()
                        .reserved
                        .micros;
                    match rng.below(3) {
                        0 => {
                            store.release_attempt(&req.attempt, ts(now)).unwrap();
                            sim.release(spend);
                            reached.released += 1;
                        }
                        1 => {
                            settle_both(
                                &store,
                                &mut sim,
                                &req,
                                spend,
                                reserved,
                                reserved / 2,
                                now,
                                &mut reached,
                            );
                            reached.half_settled += 1;
                        }
                        _ => {
                            let extra = [1_i64, 20, 60, 150]
                                [usize::try_from(rng.below(4)).unwrap()]
                            .min(sim.room(now));
                            settle_both(
                                &store,
                                &mut sim,
                                &req,
                                spend,
                                reserved,
                                reserved + extra,
                                now,
                                &mut reached,
                            );
                        }
                    }
                }
            }
            _ => {
                next_request += 1;
                let max_tokens = [10_u64, 20, 45, 100][usize::try_from(rng.below(4)).unwrap()];
                let reserve = i64::try_from(10 + 2 * max_tokens).unwrap();
                let before = sim.books(stamp);
                let want = expected_price(&before, CREDIT, reserve);
                if stamp < sim.latest_at() {
                    reached.read_behind_a_spend += 1;
                }
                let req = Req::new(
                    &store,
                    &format!("q-{seed}-{next_request}"),
                    0,
                    "cred-e",
                    max_tokens,
                )
                .at(stamp);
                let decision = admit(&store, &req);
                let got = decision
                    .admitted()
                    .map(|admitted| admitted.reservation.price.micros());
                assert_eq!(
                    got, want,
                    "seed {seed} step {step}: reserve {reserve}, books {before:?}: {decision:?}"
                );
                match &decision {
                    AdmissionDecision::Allow { .. } => reached.allowed += 1,
                    AdmissionDecision::Degrade { .. } => reached.degraded += 1,
                    _ => reached.rejected += 1,
                }
                if let Some(charged) = got {
                    let spend = sim.reserve(stamp, charged);
                    if rng.below(4) == 0 {
                        open.push((req, spend)); // 留着在途，之后的某一步再收尾
                    } else {
                        let at = now + 500;
                        let extra = if rng.below(4) == 0 {
                            [1_i64, 20, 60, 150][usize::try_from(rng.below(4)).unwrap()]
                                .min(sim.room(at))
                        } else {
                            0
                        };
                        settle_both(
                            &store,
                            &mut sim,
                            &req,
                            spend,
                            charged,
                            charged + extra,
                            at,
                            &mut reached,
                        );
                    }
                } else {
                    assert_eq!(denied(&decision), Some(ReasonCode::BudgetExhausted));
                }
            }
        }
        // 每一步之后：视图、模型、事件三方对得上，授信占用不超过授信额度
        let check_at = now + 700;
        let books = check_against_sim(&store, &db, &sim, check_at);
        assert!(
            books.credit_debt <= CREDIT,
            "seed {seed} step {step}: credit use beyond the line: {books:?}"
        );
        if books.allowance_left > 0 && books.credit_debt > 0 {
            reached.allowance_and_debt += 1;
        }
        if books.credit_debt == CREDIT {
            reached.credit_full += 1;
        }
        if books.allowance_left == 0 && books.credit_debt > 0 && !allowances.is_empty() {
            reached.debt_without_allowance += 1;
        }
        reached.in_flight_at_check += u32::from(!open.is_empty());
    }
    assert_clean(&store);
    reached
}

#[test]
fn random_histories_agree_with_the_independent_models_and_the_owner_rule() {
    let mut total = Reached::default();
    for seed in [1_u64, 2, 3, 4, 5, 6] {
        total.add(run_random_history(seed, 50, false));
    }
    eprintln!("random histories reached: {total:?}");
    // 没有走到这些状态，上面「几方对得上」就没有说服力
    assert!(total.allowed >= 30, "{total:?}");
    assert!(total.degraded >= 3, "{total:?}");
    assert!(total.rejected >= 10, "{total:?}");
    assert!(total.allowance_and_debt >= 10, "{total:?}");
    assert!(total.credit_full >= 10, "{total:?}");
    assert!(total.revoked_after_cover >= 1, "{total:?}");
    assert!(total.released >= 3, "{total:?}");
    assert!(total.half_settled >= 3, "{total:?}");
    assert!(total.in_flight_at_check >= 10, "{total:?}");
    assert!(total.debt_without_allowance >= 10, "{total:?}");
    assert!(total.overage_settled >= 5, "{total:?}");
    assert!(total.overage_with_allowance >= 3, "{total:?}");
}

#[test]
fn random_histories_with_out_of_order_clocks_still_agree_with_the_independent_models() {
    // 同样的随机历史（有超出预留的结算），但额度的创建时间、撤销时间、请求的读钟有一半取的是排队之前的旧时间：比前面已经提交的
    // 花费更早，也会和前一步同一毫秒；一半的新额度起点倒填。先后始终是调用顺序（提交顺序），视图、两个模型、准入决定、
    // 结算分摊必须仍然对得上，授信占用仍不超过授信额度。
    let mut total = Reached::default();
    for seed in [11_u64, 12, 13, 14, 15, 16, 17, 18] {
        total.add(run_random_history(seed, 60, true));
    }
    eprintln!("jittered random histories reached: {total:?}");
    assert!(total.stale_stamped >= 100, "{total:?}");
    assert!(total.read_behind_a_spend >= 15, "{total:?}");
    assert!(total.allowed >= 40, "{total:?}");
    assert!(total.rejected >= 10, "{total:?}");
    assert!(total.allowance_and_debt >= 10, "{total:?}");
    assert!(total.credit_full >= 10, "{total:?}");
    assert!(total.debt_without_allowance >= 10, "{total:?}");
    assert!(total.overage_settled >= 10, "{total:?}");
    assert!(total.overage_with_allowance >= 3, "{total:?}");
}

fn attempt_of(request: &str) -> idoris_compute_contracts::ids::AttemptId {
    idoris_compute_contracts::ids::AttemptId::new(
        idoris_compute_contracts::ids::RequestId::new(request).unwrap(),
        idoris_compute_contracts::ids::AttemptIndex(0),
    )
}
