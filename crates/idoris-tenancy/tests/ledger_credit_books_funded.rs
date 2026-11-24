//! 充过值的账户也按「授信旧债与新额度分开记」（方案 C）记账（issue #601；owner 2026-11-15 拍板的方案 C，第五轮只做了没充值的账户）。
//!
//! 第五轮之后，**没充值、只靠授信**的账户新额度照常可花、不抵旧债；**充过值**的账户仍按 S0 的 `可用 = min(余额, 额度剩余)` 记账：
//!
//! * 余额 = 充值 - 全期已花（连额度覆盖的花费一起扣），所以余额被花成负数（比如透支授信）之后，新发的免费额度花不出去；
//! * 授信用光、手上还有一笔没花的额度 100 时，补 50 之后能花的反而从 100 变成 50（余额 -150，授信恢复 50，额度只是上限）；
//! * 准入告警 `credit_used` 的金额是净头寸：旧债 200、新额度 100、花 50，这一笔完全由新额度覆盖，却记一条金额 150 的告警，对担保人是噪声。
//!
//! 现在口径（有过授信的账户，不管有没有充值）：**额度、自有资金、授信债务三本分开记**。
//!
//! ```text
//! 一笔花费先用额度（先到期的先用），额度之外的部分 U 先用自有资金，用完才记成授信债务
//! 授信债务 = max(U - 全部充值, 0)        自有资金剩余 = max(全部充值 - U, 0)
//! 能花的   = 额度剩余 + 自有资金剩余 + (授信额度 - min(授信债务, 授信额度))
//! ```
//!
//! 没充值时 U 就是第五轮的授信债务。没有任何额度的充值账户，数字和 S0 的 `余额 + 授信` 一样。没有授信的充值账户（从没有过授信额度）
//! 不进这套账，仍是 S0 的 `min(余额, 额度剩余)`（额度是上限）。
//!
//! 这个文件有两个**独立于被测代码**的模型（`support/ledger_books.rs`）：[`model`] 只读原始表（`allocations`、`allocation_revocations`、
//! `ledger_allocation_marks`、`ledger_entries`），把「额度创建」「额度撤销」「一笔花费」按提交先后逐件回放，再把充值加进来；
//! [`Sim`] 不读库，只吃测试自己做过的事（充值、发额度、撤销、预留、释放、结算），随机历史的期望从它来。两个模型都不经过被测的投影函数，
//! 也不照抄被测实现的时间假设（读钟更早时不把已提交的花费压到读钟上，预留和它超出预留的部分按种类分、不按毫秒合并）。
//! 「Codex 复审（2026-11-22）」那一节的测试不用任何模型，期望数字全部手算。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢，所以 `max_tokens = 45`
//! 的预留正好是 100，`max_tokens = 10` 是最低有用的预留 30。
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
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{
    AlertKind, ComponentDecision, CreditLineSetting, LedgerError, SettleOutcome, SettleResult,
};

/// `max_tokens = 45`：预留 100。
const M100: u64 = 45;
/// `max_tokens = 10`：最低有用的预留 30。
const M30: u64 = 10;
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

/// 预留额（微泰铢）；拒绝是 `None`。
fn price_of(decision: &AdmissionDecision) -> Option<i64> {
    decision
        .admitted()
        .map(|admitted| admitted.reservation.price.micros())
}

/// 一个请求：先不允许宽限，被拒（钱不够）再允许宽限试一遍，和云执行器的两遍一样。
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

/// `emp` 付一笔：准入，放行了就在半秒后按预留额结算（预留多少就收多少）。
fn spend(store: &AccountStore, request: &str, at: i64, max_tokens: u64) -> AdmissionDecision {
    let req = Req::new(store, request, 0, "cred-e", max_tokens).at(at);
    let decision = admit(store, &req);
    if let Some(charged) = price_of(&decision) {
        store
            .settle_attempt(&req.attempt, &price(charged), ts(at + 500))
            .unwrap();
    }
    decision
}

fn view(store: &AccountStore, now: i64) -> AncestorView {
    store.account_view(&aid("emp"), ts(now)).unwrap()
}

/// 视图里能花的钱：`可用额度 + 授信额度`（S0 的算法；`balance` 和 `ceiling_remaining` 都有时取小）。
fn headroom(view: &AncestorView) -> i64 {
    let available = [&view.balance, &view.ceiling_remaining]
        .into_iter()
        .flatten()
        .map(|m| m.micros)
        .min()
        .unwrap_or(0);
    available + view.credit_limit.micros
}

/// `emp`（成员）什么都没有；组织 `paid` 资金充足，并且从平台拿到了足够下发额度的额度。授信、额度、充值由调用者自己开。
fn credit_world(db: &TempDb) -> AccountStore {
    let store = world(db);
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    allocate(&store, "e-0", "platform", "paid", 9_000_000);
    store
}

/// 给 `emp` 开 / 重设授信。
fn credit_line(store: &AccountStore, micros: i64, at: i64) {
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(micros),
                valid_until: None,
            },
            ts(at),
        )
        .unwrap();
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

/// 给 `emp` 充值。
fn top_up(store: &AccountStore, micros: i64, tag: &str, at: i64) {
    store
        .fund_account(&aid("emp"), &thb(micros), &key(&format!("f-{tag}")), ts(at))
        .unwrap();
}

/// `emp` 名下 `credit_used` 告警的金额，按写入顺序。
fn credit_used_amounts(store: &AccountStore) -> Vec<i64> {
    store
        .alerts(Some(AlertKind::CreditUsed))
        .unwrap()
        .into_iter()
        .filter(|alert| alert.account == "emp")
        .map(|alert| alert.amount.micros)
        .collect()
}

// ------------------------------------------------------------------------------------------
// 独立于被测代码的模型（`support/ledger_books.rs`）：读原始表回放的 `model`，和不读库、只吃测试自己做过的事的 `Sim`
// ------------------------------------------------------------------------------------------

/// 视图和模型对得上，并且金额守恒。返回模型的账。
///
/// 有额度（哪怕已经撤销、到期）又有授信的账户走「分开记」：自有资金并进净头寸，`balance` 是 `None`，`ceiling_remaining` 是
/// `额度剩余 + 自有资金剩余 - min(债务, 授信额度)`。从来没有过额度的充值账户仍是 S0 的 `余额 = 充值 - 已花`。
fn check_books(store: &AccountStore, db: &TempDb, account: &str, now: i64) -> Books {
    let books = model(db.path(), account, now);
    let v = store.account_view(&aid(account), ts(now)).unwrap();
    let ever_had_allowance = scalar(
        db,
        &format!(
            "SELECT EXISTS (SELECT 1 FROM allocations WHERE child_id = '{account}'
                             AND purpose IN ('free_tier', 'prepaid', 'org_quota'))"
        ),
    ) == 1;
    if books.funds > 0 && !ever_had_allowance {
        assert_eq!(
            v.balance,
            Some(thb(books.funds - books.spent)),
            "{account}: a funded account that never had an allowance keeps S0's balance: {books:?}"
        );
        assert_eq!(v.ceiling_remaining, None, "{account}: {books:?}");
    } else {
        assert!(
            v.balance.is_none(),
            "{account}: separate books fold the funds into the net position: {books:?}"
        );
        assert_eq!(
            v.ceiling_remaining,
            Some(thb(books.net(v.credit_limit.micros))),
            "the view disagrees with the model: {books:?} credit line {:?}",
            v.credit_limit
        );
    }
    // 金额守恒：额度覆盖的 + 没被覆盖的 = 分录里的已花，并且和原生 SQL 对得上
    assert_eq!(books.covered + books.uncovered, books.spent, "{books:?}");
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
    assert!(
        books.covered >= 0
            && books.uncovered >= 0
            && books.funds_left >= 0
            && books.credit_debt >= 0,
        "{books:?}"
    );
    books
}

// ------------------------------------------------------------------------------------------
// issue #601：复现
// ------------------------------------------------------------------------------------------

#[test]
fn a_funded_account_that_spent_past_its_balance_can_still_spend_a_new_allowance() {
    // issue 场景 1：充值 100、授信 100，花到余额 -100。新发的免费额度 100 应该照常可花，授信仍是占满的（旧债没有被抵掉）。
    // 旧代码：可用额 = min(余额 -100, 额度 100) = -100，加授信 100 = 0，新额度一分也花不出去。
    let db = TempDb::new("funded-negative-balance");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let blocked = admit_with_ladder(&store, "r-3", M30, T0 + 3_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );

    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    let fresh = spend(&store, "r-4", later + 1_000, M100);
    assert_eq!(
        price_of(&fresh),
        Some(100),
        "the new allowance must be spendable: {fresh:?}"
    );
    assert!(
        matches!(fresh, AdmissionDecision::Allow { .. }),
        "{fresh:?}"
    );

    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!(
        (
            books.allowance_left,
            books.covered,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (0, 100, 0, 100, 300),
        "the allowance covered the new spend; the old 100 of debt is untouched"
    );
    assert_eq!(headroom(&view(&store, later + 2_000)), 0);
    let after = admit_with_ladder(&store, "r-5", M30, later + 3_000);
    assert_eq!(
        denied(&after),
        Some(ReasonCode::BudgetExhausted),
        "{after:?}"
    );
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
    assert_clean(&store);
}

#[test]
fn topping_up_never_reduces_what_the_account_can_spend() {
    // issue 场景 2：授信 200 用光，手上有 100 没花的额度。补 50 之后能花的应该是 100 + 50（授信恢复 50），不是 50。
    let db = TempDb::new("funded-top-up-monotone");
    let store = credit_world(&db);
    credit_line(&store, 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    let before = headroom(&view(&store, later + 1));
    assert_eq!(before, 100, "only the new allowance is spendable");

    top_up(&store, 50, "emp", later + 500);
    let after = headroom(&view(&store, later + 600));
    assert!(
        after >= before,
        "a top-up must not lower what can be spent: {before} -> {after}"
    );
    assert_eq!(
        after, 150,
        "the allowance 100 plus the 50 of credit the top-up gave back"
    );

    // 恰好 150：放行；再来最小的一笔就拒绝
    let exact = spend(&store, "r-3", later + 1_000, 70);
    assert_eq!(price_of(&exact), Some(150), "{exact:?}");
    assert!(
        matches!(exact, AdmissionDecision::Allow { .. }),
        "{exact:?}"
    );
    let more = admit_with_ladder(&store, "r-4", M30, later + 2_000);
    assert_eq!(denied(&more), Some(ReasonCode::BudgetExhausted), "{more:?}");
    let books = check_books(&store, &db, "emp", later + 3_000);
    assert_eq!(
        (books.allowance_left, books.funds_left, books.credit_debt),
        (0, 0, 200),
        "the top-up paid down the old debt; the 150 then took the allowance and gave the credit back out"
    );
    assert_clean(&store);
}

#[test]
fn the_credit_used_alert_is_quiet_when_the_new_allowance_covers_the_spend() {
    // issue：旧债 200、新额度 100、花 50，这一笔完全由新额度覆盖，旧代码仍记一条金额 150 的告警（净头寸口径），对担保人是噪声。
    let db = TempDb::new("alert-noise-credit-only");
    let store = credit_world(&db);
    credit_line(&store, 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    assert_eq!(
        credit_used_amounts(&store),
        [100, 200],
        "the two spends that went onto the credit line are the ones the guarantor is told about"
    );

    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    for (request, at) in [("r-3", later + 1_000), ("r-4", later + 2_000)] {
        let decision = spend(&store, request, at, 20);
        assert_eq!(price_of(&decision), Some(50), "{request}: {decision:?}");
        assert_eq!(
            credit_used_amounts(&store),
            [100, 200],
            "{request}: covered by the new allowance, no credit was used, so no alert"
        );
    }
    // 额度花完了，授信仍是满的：再来一笔放不下，也没有告警
    let blocked = admit_with_ladder(&store, "r-5", M30, later + 3_000);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );
    assert_eq!(credit_used_amounts(&store), [100, 200]);
    assert_clean(&store);
}

#[test]
fn a_degraded_request_that_fits_the_new_allowance_raises_no_alert() {
    // 想要 102（max_tokens = 46），额度只有 100：压到 100 放行，整笔落在新额度里，没动授信，不记告警。
    // 旧代码按净头寸（-100）算：压低后的 100 要「透支 200」，记一条金额 200 的告警。
    let db = TempDb::new("alert-degrade-covered");
    let store = credit_world(&db);
    credit_line(&store, 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    let degraded = spend(&store, "r-3", later + 1_000, 46);
    assert!(
        matches!(degraded, AdmissionDecision::Degrade { .. }),
        "{degraded:?}"
    );
    assert_eq!(price_of(&degraded), Some(100), "{degraded:?}");
    assert_eq!(credit_used_amounts(&store), [100, 200]);
    assert_clean(&store);
}

#[test]
fn the_credit_used_alert_is_quiet_for_a_funded_account_too() {
    // 同样的噪声，有充值的账户：充值 100、授信 100，花到余额 -100（第二笔动了授信，告警一条），新额度 100 覆盖的花费不再告警。
    let db = TempDb::new("alert-noise-funded");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    assert_eq!(
        credit_used_amounts(&store),
        [100],
        "only the second spend went past the funds and onto the credit line"
    );
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    let covered = spend(&store, "r-3", later + 1_000, 20);
    assert_eq!(price_of(&covered), Some(50), "{covered:?}");
    assert_eq!(credit_used_amounts(&store), [100], "{covered:?}");
    assert_clean(&store);
}

#[test]
fn the_credit_used_alert_amount_is_still_the_credit_in_use_when_a_spend_does_draw_credit() {
    // 正向对照：这一笔只有一部分由额度覆盖、其余动了授信时，告警照记，金额口径没变（准入之后授信已占用的总额，含旧债）：
    // 授信 200，先花 100（债务 100，告警 100）；新额度 30；再花 50：额度覆盖 30，授信承担 20，债务 120，告警 120。
    let db = TempDb::new("alert-amount-partial");
    let store = credit_world(&db);
    credit_line(&store, 200, T0);
    assert!(spend(&store, "r-1", T0 + 1_000, M100).is_admitted());
    assert_eq!(credit_used_amounts(&store), [100]);
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 30, later, later + 30 * DAY, later);
    let partial = spend(&store, "r-2", later + 1_000, 20);
    assert_eq!(price_of(&partial), Some(50), "{partial:?}");
    assert_eq!(
        credit_used_amounts(&store),
        [100, 120],
        "30 of the 50 came from the allowance, 20 from the credit line: 100 + 20 in use"
    );
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!((books.covered, books.credit_debt), (30, 120));
    // 没有旧债的账户：这一笔的授信部分就是金额
    let db2 = TempDb::new("alert-amount-fresh");
    let fresh = credit_world(&db2);
    credit_line(&fresh, 200, T0);
    prepaid(&fresh, "emp-allow", 30, T0, T0 + 30 * DAY, T0);
    assert!(spend(&fresh, "r-1", T0 + 1_000, 20).is_admitted());
    assert_eq!(credit_used_amounts(&fresh), [20]);
}

// ------------------------------------------------------------------------------------------
// 三本账的顺序与口径
// ------------------------------------------------------------------------------------------

#[test]
fn one_spend_uses_the_allowance_then_the_funds_then_the_credit() {
    // 充值 100、授信 100、额度 100，一共 300。一笔 250：额度 100 先用，自有资金 100 其次，授信只承担 50。
    let db = TempDb::new("funded-order");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
    assert_eq!(headroom(&view(&store, T0 + 500)), 300);

    let big = spend(&store, "r-1", T0 + 1_000, 120);
    assert_eq!(price_of(&big), Some(250), "{big:?}");
    assert!(matches!(big, AdmissionDecision::Allow { .. }), "{big:?}");
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (
            books.allowance_left,
            books.covered,
            books.funds_left,
            books.credit_debt
        ),
        (0, 100, 0, 50),
        "allowance first, then the funds; the credit line carries only the last 50"
    );
    assert_eq!(credit_used_amounts(&store), [50]);
    assert_eq!(view(&store, T0 + 2_000).ceiling_remaining, Some(thb(-50)));
    assert_eq!(
        headroom(&view(&store, T0 + 2_000)),
        50,
        "100 of credit minus the 50 in use"
    );
    // 再花 50 就是授信可用的全部：告警金额是准入之后授信占用的总额 100
    let rest = spend(&store, "r-2", T0 + 3_000, 20);
    assert_eq!(price_of(&rest), Some(50), "{rest:?}");
    assert_eq!(credit_used_amounts(&store), [50, 100]);
    let none = admit_with_ladder(&store, "r-3", M30, T0 + 4_000);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_clean(&store);
}

#[test]
fn a_top_up_pays_the_debt_first_and_only_the_excess_is_spendable_funds() {
    // 授信 200 用光，额度 100。补 250：先还清债务 200，多出来的 50 是自有资金；额度一分没动。
    let db = TempDb::new("funded-excess-top-up");
    let store = credit_world(&db);
    credit_line(&store, 200, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    top_up(&store, 250, "emp", later + 500);
    let books = check_books(&store, &db, "emp", later + 600);
    assert_eq!(
        (books.allowance_left, books.funds_left, books.credit_debt),
        (100, 50, 0),
        "the top-up paid the 200 of debt, the excess 50 is funds, the allowance is untouched"
    );
    assert_eq!(headroom(&view(&store, later + 600)), 350);

    // 恰好 350：放行；之后什么都花不出去
    let all = spend(&store, "r-3", later + 1_000, 170);
    assert_eq!(price_of(&all), Some(350), "{all:?}");
    assert!(matches!(all, AdmissionDecision::Allow { .. }), "{all:?}");
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!(
        (
            books.allowance_left,
            books.funds_left,
            books.credit_debt,
            books.covered
        ),
        (0, 0, 200, 100)
    );
    let none = admit_with_ladder(&store, "r-4", M30, later + 3_000);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_clean(&store);
}

#[test]
fn adding_an_allowance_never_lowers_what_a_funded_account_with_a_credit_line_can_spend() {
    // 连续性：充值 100、授信 100，花了 150（余额 -50）。能花的是授信剩下的 50；新发 100 额度之后应该是 150，不是还是 50。
    let db = TempDb::new("funded-allowance-monotone");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    let spent = spend(&store, "r-1", T0 + 1_000, 70);
    assert_eq!(price_of(&spent), Some(150), "{spent:?}");
    let before = view(&store, T0 + 2_000);
    assert_eq!(
        before.balance,
        Some(thb(-50)),
        "no allowance yet: S0's own balance"
    );
    assert_eq!(headroom(&before), 50);

    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    assert_eq!(headroom(&view(&store, later + 1)), 150);
    let exact = spend(&store, "r-2", later + 1_000, 70);
    assert_eq!(price_of(&exact), Some(150), "{exact:?}");
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!((books.allowance_left, books.credit_debt), (0, 100));
    assert_clean(&store);
}

#[test]
fn an_allowance_revoked_after_it_covered_a_spend_leaves_no_debt_on_a_funded_account() {
    // L1（撤销之前在额度内的花费不算债务）对有充值的账户同样成立：充值 100、授信 100、预付额度 1000 下花了 900，撤销额度。
    // 旧代码：余额 = 100 - 900 = -800，额度撤销之后可用 -800，新的预留被拒。
    let db = TempDb::new("funded-revoked-covered");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 1_000, T0 - DAY, T0 + 30 * DAY, T0);
    let big = spend(&store, "r-1", T0 + 1_000, 445);
    assert_eq!(price_of(&big), Some(900), "{big:?}");
    revoke_allocation_at(&db, "emp-allow", T0 + 5_000);

    let books = check_books(&store, &db, "emp", T0 + 6_000);
    assert_eq!(
        (books.covered, books.funds_left, books.credit_debt),
        (900, 100, 0),
        "the 900 was inside the allowance when it was spent: neither funds nor credit paid for it"
    );
    assert_eq!(
        headroom(&view(&store, T0 + 6_000)),
        200,
        "100 of funds and 100 of credit"
    );
    let rest = spend(&store, "r-2", T0 + 8_000, 95);
    assert_eq!(price_of(&rest), Some(200), "{rest:?}");
    let none = admit_with_ladder(&store, "r-3", M30, T0 + 9_000);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_clean(&store);
}

#[test]
fn a_late_issued_allowance_does_not_pay_debt_from_before_it_existed_on_a_funded_account() {
    // 额度窗口是整个月，但它是月中才发的（起点倒填）：只覆盖发出之后的花费。充值 100、授信 100，发出之前花了 200（债务 100）；
    // 额度整月没花、到期之后，债务还是 100，授信仍占满。
    let db = TempDb::new("funded-late-issue");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-late", 100, T0 - DAY, T0 + 30 * DAY, later);
    let books = check_books(&store, &db, "emp", later + 1);
    assert_eq!(
        (books.allowance_left, books.covered, books.credit_debt),
        (100, 0, 100),
        "the earlier 200 is not covered by an allowance that did not exist yet"
    );
    assert_eq!(headroom(&view(&store, later + 1)), 100);

    let after = T0 + 31 * DAY;
    let books = check_books(&store, &db, "emp", after);
    assert_eq!((books.allowance_left, books.credit_debt), (0, 100));
    assert_eq!(
        headroom(&view(&store, after)),
        0,
        "the credit line stays full"
    );
    let blocked = admit_with_ladder(&store, "r-3", M30, after);
    assert_eq!(
        denied(&blocked),
        Some(ReasonCode::BudgetExhausted),
        "{blocked:?}"
    );
    assert_clean(&store);
}

#[test]
fn debt_beyond_a_lowered_credit_line_does_not_eat_a_new_allowance_on_a_funded_account() {
    // 充值 100、授信 200，花了 300（债务 200）；授信调小到 100：债务超出授信的 100 再也花不出去，但新额度不该被它吃掉。
    let db = TempDb::new("funded-over-limit");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 200, T0);
    let spent = spend(&store, "r-1", T0 + 1_000, 145);
    assert_eq!(price_of(&spent), Some(300), "{spent:?}");
    credit_line(&store, 100, T0 + 3_000);
    assert_eq!(
        headroom(&view(&store, T0 + 4_000)),
        -100,
        "debt 200 is beyond the line 100 (no allowance yet: S0's own balance -200, unchanged)"
    );

    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    let books = check_books(&store, &db, "emp", later + 1);
    assert_eq!((books.allowance_left, books.credit_debt), (100, 200));
    assert_eq!(
        headroom(&view(&store, later + 1)),
        100,
        "the new allowance is spendable"
    );
    let fresh = spend(&store, "r-2", later + 1_000, M100);
    assert_eq!(price_of(&fresh), Some(100), "{fresh:?}");
    let none = admit_with_ladder(&store, "r-3", M30, later + 2_000);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 宽限：欠着授信的钱不给宽限（S3-03），没欠的照旧
// ------------------------------------------------------------------------------------------

#[test]
fn grace_is_refused_while_a_funded_account_owes_credit_even_when_its_net_position_is_not_negative()
{
    // 授信 20、充值 100，花了 120（债务 20）。之后新发额度 25：净头寸 25 - 20 = 5 不是负数，能花的只有 25（< 最低有用的预留 30）。
    // S0 只看净头寸，5 >= 0，宽限的资格检查会放行；账户明明欠着授信的钱，不给宽限。
    let db = TempDb::new("funded-grace-owes");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 20, T0);
    let spent = spend(&store, "r-1", T0 + 1_000, 55);
    assert_eq!(price_of(&spent), Some(120), "{spent:?}");
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 25, later, later + 30 * DAY, later);
    let books = model(db.path(), "emp", later + 1);
    assert_eq!((books.allowance_left, books.credit_debt), (25, 20));
    assert_eq!(
        books.net(20),
        5,
        "the net position is not negative: only the debt tells S0's grace check apart"
    );
    assert_eq!(
        books.room(20),
        25,
        "less than the smallest useful reservation, 30"
    );

    let decision = admit_with_ladder(&store, "r-2", 100, later + 1_000);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "an account that owes credit gets no grace: {decision:?}"
    );
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
    assert_clean(&store);
}

#[test]
fn grace_is_still_given_to_a_funded_account_in_the_books_that_owes_nothing() {
    // 正向对照：同样是额度和资金都不够一笔最低有用的预留，没欠授信的钱就还有一次宽限（授信开过又关掉，所以走分开记）。
    let db = TempDb::new("funded-grace-owes-nothing");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 20, T0);
    store
        .disable_credit_line(&aid("emp"), ts(T0 + 500))
        .unwrap();
    let spent = spend(&store, "r-1", T0 + 1_000, 40);
    assert_eq!(price_of(&spent), Some(90), "{spent:?}");
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 15, later, later + 30 * DAY, later);
    let books = model(db.path(), "emp", later + 1);
    assert_eq!(
        (books.allowance_left, books.funds_left, books.credit_debt),
        (15, 10, 0)
    );
    assert_eq!(
        books.room(0),
        25,
        "less than the smallest useful reservation, 30"
    );
    let decision = admit_with_ladder(&store, "r-2", 100, later + 1_000);
    assert!(
        matches!(decision, AdmissionDecision::Grace { .. }),
        "{decision:?}"
    );
    assert_eq!(store.grace_uses(&aid("emp")).unwrap().len(), 1);
    assert_clean(&store);
}

#[test]
fn an_expired_reservation_gives_the_allowance_and_the_funds_back_and_its_late_settlement_is_a_new_spend()
 {
    // 充值 100、授信 100、额度 100。一笔 250 的预留占着三本账（额度 100、自有资金 100、授信 50），迟迟没结算，过了 TTL 被过期：
    // 额度、自有资金、授信都回来。之后它迟到的结算是一笔新的花费（记在结算的那一刻），照样先用额度、再用自有资金、最后才动授信。
    let db = TempDb::new("funded-expiry");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
    let first = Req::new(&store, "r-1", 0, "cred-e", 120).at(T0 + 1_000);
    assert_eq!(price_of(&admit(&store, &first)), Some(250));
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (
            books.allowance_left,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (0, 0, 50, 250),
        "in flight, the reservation already uses all three books"
    );

    // 默认的预留 TTL 是 30 分钟
    let expired = T0 + 31 * MINUTE;
    assert_eq!(store.sweep_expired_holds(ts(expired)).unwrap(), 1);
    let books = check_books(&store, &db, "emp", expired + 1);
    assert_eq!(
        (
            books.allowance_left,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (100, 100, 0, 0),
        "expiry gives all three back"
    );
    assert_eq!(headroom(&view(&store, expired + 1)), 300);

    let outcome = store
        .settle_attempt(&first.attempt, &price(250), ts(expired + 1_000))
        .unwrap();
    assert!(outcome.late, "{outcome:?}");
    assert_eq!(
        outcome.platform_loss,
        thb(0),
        "the chain has room for all of it: {outcome:?}"
    );
    let books = check_books(&store, &db, "emp", expired + 2_000);
    assert_eq!(
        (
            books.allowance_left,
            books.covered,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (0, 100, 0, 50, 250),
        "the late settlement is a new spend: allowance first, then funds, then credit"
    );
    assert_clean(&store);
}

#[test]
fn a_late_settlement_is_judged_when_it_settles_so_an_allowance_that_ended_since_does_not_cover_it()
{
    // 迟到的结算是结算那一刻才发生的新花费：额度的窗口在这之前就结束了，就覆盖不了它。充值 100、授信 100，额度 100 的窗口在 T0 + 10 分钟结束。
    //
    //   T0 + 1 秒        预留 100（额度覆盖）
    //   T0 + 31 分钟     预留过了 TTL（30 分钟）被过期，占着的额度放回去；额度的窗口早就结束了，放回去也没有用
    //   T0 + 31 分钟 + 1 秒  迟到的结算按 100：额度够不着，花的是自有资金 100，授信一分没动
    //
    // 结算之后：额度 0、自有资金 0、债务 0，能花的 0 + 0 + 100 = 100。记在预留创建的时刻（T0 + 1 秒）的话，已经到期的额度会覆盖它：
    // 自有资金 100 还在，能花的是 200。
    let db = TempDb::new("funded-late-after-window");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 10 * MINUTE, T0);
    let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 1_000);
    assert_eq!(price_of(&admit(&store, &first)), Some(100));
    let expired = T0 + 31 * MINUTE;
    assert_eq!(store.sweep_expired_holds(ts(expired)).unwrap(), 1);

    let outcome = settle_at(&store, &first, 100, expired + 1_000);
    assert!(outcome.late, "{outcome:?}");
    assert_eq!(
        (
            outcome.sponsor_covered.clone(),
            outcome.platform_loss.clone()
        ),
        (thb(100), thb(0)),
        "the chain still has room (funds 100 + credit 100): {outcome:?}"
    );
    let after = view(&store, expired + 2_000);
    assert_eq!(
        after.ceiling_remaining,
        Some(thb(0)),
        "allowance gone, funds spent, no debt"
    );
    assert_eq!(headroom(&after), 100);
    assert_clean(&store);
}

#[test]
fn extra_costs_are_checked_against_the_same_three_books() {
    // 附加成本（比如安全检查）的预留只放行或拒绝，走的是同一份视图：充值 100、授信 100、额度 100，一共 300。
    let db = TempDb::new("funded-extra-cost");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
    let reserve = |request: &str, micros: i64| {
        store
            .reserve_request_component(
                &idoris_compute_contracts::ids::RequestId::new(request).unwrap(),
                "safety_input",
                &payment(&store, "cred-e"),
                &price(micros),
                ts(T0 + 1_000),
            )
            .unwrap()
    };
    assert!(
        matches!(reserve("c-1", 300), ComponentDecision::Reserved { .. }),
        "allowance 100 + funds 100 + credit 100"
    );
    assert_eq!(
        credit_used_amounts(&store),
        [100],
        "the last 100 of the 300 is credit: one alert, like a model reservation (the control that stays inside allowance + funds is below)"
    );
    assert!(
        matches!(
            reserve("c-2", 30),
            ComponentDecision::Rejected(ReasonCode::BudgetExhausted)
        ),
        "nothing is left"
    );
    assert_eq!(
        credit_used_amounts(&store),
        [100],
        "a rejection writes no alert"
    );
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (
            books.covered,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (100, 0, 100, 300)
    );
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 范围：哪些账户走这套账，哪些行为不变
// ------------------------------------------------------------------------------------------

#[test]
fn a_funded_account_that_never_had_a_credit_line_keeps_the_allowance_as_a_cap() {
    // 没有授信额度的充值账户没有「授信旧债」，不进这套账：仍是 S0 的 `可用 = min(余额, 额度剩余)`，额度是上限、不是另一份钱。
    // 充值 100 + 额度 100：最多花 100，不是 200。这一条在改动前后必须一样。
    let db = TempDb::new("funded-no-credit-cap");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
    let v = view(&store, T0 + 500);
    assert_eq!(v.balance, Some(thb(100)));
    assert_eq!(v.ceiling_remaining, Some(thb(100)));
    assert_eq!(v.credit_limit, thb(0));

    let want = spend(&store, "r-1", T0 + 1_000, 70);
    let AdmissionDecision::Degrade { admitted, .. } = &want else {
        panic!("{want:?}");
    };
    assert_eq!(
        admitted.reservation.price.micros(),
        100,
        "min(funds 100, allowance 100), not 200"
    );
    let v = view(&store, T0 + 2_000);
    assert_eq!(
        (v.balance, v.ceiling_remaining),
        (Some(thb(0)), Some(thb(0)))
    );
    let none = admit(
        &store,
        &Req::new(&store, "r-2", 0, "cred-e", M30).at(T0 + 3_000),
    );
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_clean(&store);
}

#[test]
fn a_funded_account_with_a_credit_line_and_no_allowance_is_exactly_what_it_was() {
    // 没有任何额度的充值账户：S0 自己的 `余额 + 授信`，视图里仍是余额（`balance`），没有净头寸。
    let db = TempDb::new("funded-no-allowance");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    let spent = spend(&store, "r-1", T0 + 1_000, 70);
    assert_eq!(price_of(&spent), Some(150), "{spent:?}");
    let v = view(&store, T0 + 2_000);
    assert_eq!(v.balance, Some(thb(-50)));
    assert_eq!(v.ceiling_remaining, None);
    assert_eq!(v.credit_limit, thb(100));
    assert_eq!(
        credit_used_amounts(&store),
        [50],
        "S0's overdraft: balance -50"
    );
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!((books.funds_left, books.credit_debt), (0, 50));
    assert_clean(&store);
}

#[test]
fn a_negative_amount_cannot_offset_the_spending_of_a_reservation_on_an_account_whose_books_are_not_replayed()
 {
    // Codex 四复审 Medium：不走三本账的花钱账户——充了值却从没有过花费类额度、或者从没有过授信——视图直接用整个账户的 `Σ (held + spent)`
    // （`balance = 充值 - 已花`，`ceiling_remaining = 额度 - 窗口里的消耗`），不按预留查净额：一个被改坏了的预留的负净额抵掉别的预留真实的占用，
    // 余额被抬高。充值 100 + 授信 200，没有额度：r-1 预留 90、r-2 预留 30，余额 100 - 120 = -20；把 r-2 的净额改成 -40（在 30 的预留之外又放掉 70）：
    // 90 和 -40 并存，整个账户的求和是 50，旧代码读出余额 50，多出 70。现在失败即关闭：每个预留的净额必须不为负，读到负的返回 `Corrupt`，
    // 视图和准入都不例外。四种不走三本账的形状 × 负的预留净额 / 负的结算时刻花费（负的 `late_settle` 要关掉 CHECK 才插得进去）。
    //
    // 旧代码读出的数（手算，改坏之后，r-1 的 90 + r-2 的 30 + 改坏的 -70）：负的预留净额：余额 / 额度剩余各比健康时多 70；
    // 负的 `late_settle`（-40）：多 40。
    for (label, funds, credit, allowance) in [
        ("funded with a credit line, no allowance", 100, 200, 0),
        ("funded, no credit line, no allowance", 200, 0, 0),
        ("funded with an allowance, no credit line", 200, 0, 500),
        ("an allowance only", 0, 0, 200),
    ] {
        for negative_settlement_spend in [false, true] {
            let what = format!(
                "{label}, negative {}",
                if negative_settlement_spend {
                    "settlement-time spend"
                } else {
                    "reservation"
                }
            );
            let db = TempDb::new(&format!(
                "raw-negative-net-{funds}-{credit}-{allowance}-{negative_settlement_spend}"
            ));
            let store = credit_world(&db);
            if funds > 0 {
                top_up(&store, funds, "emp", T0);
            }
            if credit > 0 {
                credit_line(&store, credit, T0);
            }
            if allowance > 0 {
                prepaid(&store, "emp-allow", allowance, T0 - DAY, T0 + 30 * DAY, T0);
            }
            assert_eq!(
                price_of(&spend(&store, "r-1", T0 + 1_000, 40)),
                Some(90),
                "{what}"
            );
            assert_eq!(
                price_of(&spend(&store, "r-2", T0 + 2_000, M30)),
                Some(30),
                "{what}"
            );
            // 改坏之前账是对的：这个账户不走三本账（`balance` 或 `ceiling_remaining` 是整个账户求和出来的数）
            let sound = view(&store, T0 + 3_000);
            assert_eq!(
                sound.balance,
                (funds > 0).then(|| thb(funds - 120)),
                "{what}: balance before the tampering (r-1 90 + r-2 30)"
            );
            assert_eq!(
                sound.ceiling_remaining,
                (allowance > 0).then(|| thb(allowance - 120)),
                "{what}: allowance left before the tampering"
            );
            drop(store);
            if negative_settlement_spend {
                make_the_settlement_time_spend_negative(&db, "emp", "r-2#0", -40);
            } else {
                make_the_net_negative(&db, "emp", "r-2#0", 70);
            }

            let store = AccountStore::open(db.path()).unwrap();
            let error = store
                .account_view(&aid("emp"), ts(T0 + 4_000))
                .expect_err("the view must fail closed");
            assert!(
                matches!(error, LedgerError::Corrupt(_)),
                "{what}: {error:?}"
            );
            let next = Req::new(&store, "r-3", 0, "cred-e", M30).at(T0 + 4_000);
            let error = store
                .admit_attempt(&next.input())
                .expect_err("admission must fail closed");
            assert!(
                matches!(error, LedgerError::Corrupt(_)),
                "{what}: {error:?}"
            );
            assert!(
                store.hold(&next.attempt, "model").unwrap().is_none(),
                "{what}: nothing was reserved"
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// 超出预留（overage）：授信债务超出授信额度的部分，每一分都有平台损失记着
// ------------------------------------------------------------------------------------------

#[test]
fn overage_on_a_funded_account_is_absorbed_within_the_headroom_and_never_goes_unrecorded_beyond_it()
{
    // 充值 100、授信 100、额度 100，一共 300。
    // 第一笔预留 100、实际 250（超出 150）：预留占着额度 100，还剩自有资金 100 + 授信 100，能吸收 200，150 由担保人承担。
    // 第二笔预留 50、实际 400（超出 350）：预留 50 已经把授信占满，能吸收 0，整笔 350 都是平台损失、冻结凭证；
    // 债务超出授信的部分恰好是平台损失（350），不会比它多。
    let db = TempDb::new("funded-overage");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);

    let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 1_000);
    assert_eq!(price_of(&admit(&store, &first)), Some(100));
    let outcome = store
        .settle_attempt(&first.attempt, &price(250), ts(T0 + 1_500))
        .unwrap();
    assert_eq!(outcome.result, SettleResult::Settled);
    assert_eq!(outcome.overage, thb(150));
    assert_eq!(
        (
            outcome.sponsor_covered.clone(),
            outcome.platform_loss.clone()
        ),
        (thb(150), thb(0)),
        "the chain could still absorb 200: {outcome:?}"
    );
    assert_eq!(outcome.user_borne, thb(100));
    assert!(!outcome.credential_frozen);
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (
            books.covered,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (100, 0, 50, 250)
    );

    let second = Req::new(&store, "r-2", 0, "cred-e", 20).at(T0 + 3_000);
    assert_eq!(price_of(&admit(&store, &second)), Some(50));
    let outcome = store
        .settle_attempt(&second.attempt, &price(400), ts(T0 + 3_500))
        .unwrap();
    assert_eq!(outcome.overage, thb(350));
    assert_eq!(
        outcome.sponsor_covered,
        thb(0),
        "the reservation of 50 already took the last of the credit line"
    );
    assert_eq!(
        outcome.platform_loss,
        thb(350),
        "all of the overage is the platform's loss"
    );
    assert!(outcome.credential_frozen);
    let books = check_books(&store, &db, "emp", T0 + 4_000);
    let loss = store.loss_total("platform_loss").unwrap();
    assert_eq!(loss, 350);
    assert_eq!(
        books.credit_debt - 100,
        loss,
        "the debt beyond the credit line is exactly what the platform absorbed: {books:?}"
    );
    assert!(
        Req::try_new(&store, "r-3", 0, "cred-e", M30, T0 + 5_000).is_none(),
        "the credential was frozen: nothing more can be admitted on it"
    );
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// Codex 复审（2026-11-22）：两条造钱的路径
// ------------------------------------------------------------------------------------------
//
// 这一节每个测试的期望数字都是从输入事件手算出来的（推导写在注释里），不经过被测代码，也不经过 `model` 和 `Sim`。
// 共同的区分点是「同一笔钱不能花两次」：任何时刻，所有预留和结算（含超出预留的部分）占用的钱，不超过
// 额度 + 充值 + 授信 的总和（被撤销额度没花掉的部分不算）；超出的每一分都要有平台损失记着。

/// 结算一个预留。
fn settle_at(store: &AccountStore, req: &Req, charged: i64, at: i64) -> SettleOutcome {
    store
        .settle_attempt(&req.attempt, &price(charged), ts(at))
        .unwrap()
}

/// `emp` 名下一个预留的分录：（种类，在途，已花，归属时刻），按写入顺序。
fn entries_on_emp(store: &AccountStore, req: &Req) -> Vec<(String, i64, i64, i64)> {
    store
        .entries_of_hold(&req.attempt, "model")
        .unwrap()
        .into_iter()
        .filter(|entry| entry.account == "emp")
        .map(|entry| {
            (
                entry.kind,
                entry.held,
                entry.spent,
                entry.window_at.millis(),
            )
        })
        .collect()
}

#[test]
fn the_overage_of_an_unexpired_settlement_spends_the_allowance_of_its_own_moment_once() {
    // High 1。充值 100、授信 100、额度 100，一共 300。
    //
    //   T0 + 9    预留 200：自有资金 100 + 授信 100（额度的窗口 [T0 + 10, ..) 还没开始，或者额度这时还不存在）
    //   T0 + 10   按 300 结算，超出预留 100
    //
    // 结算那一刻额度生效、一分没动，授信已满、自有资金已空：能吸收超出部分的是额度 100，担保人承担 100，没有平台损失。
    // 这 100 就是额度花掉的：结算之后额度剩余 0，授信债务 100（预留 200 = 自有资金 100 + 授信 100），能花的 0 + 0 + (100 - 100) = 0。
    // 旧代码把整笔 300 记在预留的时刻 T0 + 9 上，额度够不着它：债务 200、额度仍剩 100，净头寸 100 - min(200, 100) = 0，
    // 能花的是 0 + 授信额度 100，一笔 100 的请求照样放行——一共花了 300 + 100 = 400，只有 300 的钱，额度被用了两次。
    for (label, allowance_exists_before_the_reservation) in [
        ("the window starts after the reservation", true),
        ("the allowance is created after the reservation", false),
    ] {
        let db = TempDb::new(&format!(
            "funded-overage-attribution-{allowance_exists_before_the_reservation}"
        ));
        let store = credit_world(&db);
        top_up(&store, 100, "emp", T0);
        credit_line(&store, 100, T0);
        if allowance_exists_before_the_reservation {
            prepaid(&store, "emp-new", 100, T0 + 10, T0 + 10 + 30 * DAY, T0 + 5);
        }
        let first = Req::new(&store, "r-1", 0, "cred-e", 95).at(T0 + 9);
        assert_eq!(price_of(&admit(&store, &first)), Some(200), "{label}");
        if !allowance_exists_before_the_reservation {
            // 窗口早就开始了，但它是预留提交之后才存在的：覆盖不到那一笔预留
            prepaid(&store, "emp-new", 100, T0 - DAY, T0 + 30 * DAY, T0 + 9);
        }
        assert_eq!(
            credit_used_amounts(&store),
            [100],
            "{label}: the reservation of 200 draws 100 of credit"
        );

        let outcome = settle_at(&store, &first, 300, T0 + 10);
        assert_eq!(outcome.result, SettleResult::Settled, "{label}");
        assert!(!outcome.late, "{label}");
        assert_eq!(outcome.overage, thb(100), "{label}");
        assert_eq!(outcome.user_borne, thb(200), "{label}");
        assert_eq!(
            (
                outcome.sponsor_covered.clone(),
                outcome.platform_loss.clone()
            ),
            (thb(100), thb(0)),
            "{label}: the allowance is untouched at that moment, so the guarantor can cover all of it: {outcome:?}"
        );
        assert!(!outcome.credential_frozen, "{label}");
        assert_eq!(
            entries_on_emp(&store, &first),
            [
                ("reserve".to_string(), 200, 0, T0 + 9),
                ("settle".to_string(), -200, 200, T0 + 9),
                ("late_settle".to_string(), 0, 100, T0 + 10),
            ],
            "{label}: the 200 inside the reservation stays at the reservation's moment; the 100 beyond it is booked when it settles"
        );

        let after = view(&store, T0 + 11);
        assert_eq!(
            after.ceiling_remaining,
            Some(thb(-100)),
            "{label}: allowance 0 + funds 0 - debt 100"
        );
        assert_eq!(after.credit_limit, thb(100), "{label}");
        assert_eq!(
            headroom(&after),
            0,
            "{label}: all 300 of the money is spent"
        );
        let next = Req::new(&store, "r-2", 0, "cred-e", M100).at(T0 + 12);
        let rejected = admit(&store, &next);
        assert_eq!(
            denied(&rejected),
            Some(ReasonCode::BudgetExhausted),
            "{label}: {rejected:?}"
        );
        let laddered = admit_with_ladder(&store, "r-3", M30, T0 + 13);
        assert_eq!(
            denied(&laddered),
            Some(ReasonCode::BudgetExhausted),
            "{label}: no grace for an account that owes credit: {laddered:?}"
        );
        assert!(store.grace_uses(&aid("emp")).unwrap().is_empty(), "{label}");
        assert_eq!(
            scalar(
                &db,
                "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
            ),
            300,
            "{label}"
        );
        assert_eq!(store.loss_total("platform_loss").unwrap(), 0, "{label}");
        assert_clean(&store);
    }
}

#[test]
fn an_old_allowance_revoked_after_the_reservation_and_a_new_one_in_the_same_millisecond_do_not_merge_the_overage_back()
 {
    // High 1 / Medium 2：预留、撤销旧额度、发新额度、结算，全在同一毫秒 X = T0 + 10，只有调用顺序（提交顺序）分得出先后。
    // 充值 100、授信 100，旧额度 `old` 100 和新额度 `new` 100，一共 400。
    //
    //   预留 250：旧额度 100（还没撤销）+ 自有资金 100 + 授信 50
    //   撤销 `old`（预留之后），发 `new`（窗口从 X 起，撤销之后）
    //   按 350 结算，超出预留 100
    //
    // 结算前：旧额度只覆盖预留（撤销在预留之后），新额度一分没动；能花的 = 额度 100 + 自有资金 0 + (授信 100 - 债务 50) = 150，
    // 担保人吸收超出的 100，没有平台损失。
    // 超出的 100 是撤销和发新额度之后才提交的：旧额度不再覆盖，新额度覆盖，所以结算之后额度 0、自有资金 0、债务 50，
    // 能花的 0 + 0 + (100 - 50) = 50（旧代码在同一毫秒把超出部分并回预留那一笔：新额度够不着它，债务 150，
    // 新额度还剩 100，能花的是 100，一笔 100 的请求照样放行）。
    // 下一笔 100 的请求只能降到 50；总共花了 350 + 50 = 400，正好是全部的钱。
    let db = TempDb::new("funded-overage-same-ms");
    let store = credit_world(&db);
    let x = T0 + 10;
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "old", 100, T0 - DAY, T0 + 30 * DAY, T0);

    let first = Req::new(&store, "r-1", 0, "cred-e", 120).at(x);
    assert_eq!(price_of(&admit(&store, &first)), Some(250));
    assert_eq!(
        credit_used_amounts(&store),
        [50],
        "250 is 50 beyond allowance 100 + funds 100"
    );
    revoke_allocation_at(&db, "old", x);
    prepaid(&store, "new", 100, x, x + 30 * DAY, x);

    let outcome = settle_at(&store, &first, 350, x);
    assert_eq!(outcome.overage, thb(100));
    assert_eq!(outcome.user_borne, thb(250));
    assert_eq!(
        (
            outcome.sponsor_covered.clone(),
            outcome.platform_loss.clone()
        ),
        (thb(100), thb(0)),
        "room before the settlement: 100 + 0 + (100 - 50) = 150: {outcome:?}"
    );
    assert!(!outcome.credential_frozen);
    assert_eq!(
        entries_on_emp(&store, &first),
        [
            ("reserve".to_string(), 250, 0, x),
            ("settle".to_string(), -250, 250, x),
            ("late_settle".to_string(), 0, 100, x),
        ],
        "the overage is a separate entry even in the reservation's own millisecond"
    );

    let after = view(&store, x + 1);
    assert_eq!(
        after.ceiling_remaining,
        Some(thb(-50)),
        "allowance 0 + funds 0 - debt 50"
    );
    assert_eq!(headroom(&after), 50);
    let next = admit(
        &store,
        &Req::new(&store, "r-2", 0, "cred-e", M100).at(x + 2),
    );
    assert_eq!(
        price_of(&next),
        Some(50),
        "only 50 of the 400 is left: {next:?}"
    );
    assert!(
        matches!(next, AdmissionDecision::Degrade { .. }),
        "{next:?}"
    );
    let none = admit_with_ladder(&store, "r-3", M30, x + 3);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_eq!(store.loss_total("platform_loss").unwrap(), 0);
    assert_clean(&store);
}

/// 授信开过又关了（现在是 0，`ever_had_credit_line` 为真），所以走三本账；充值 100；旧额度 `old` 100，窗口是毫秒级的
/// `[T0, T0 + 100)`，T0 创建。
fn stale_read_world(db: &TempDb) -> AccountStore {
    let store = credit_world(db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    store.disable_credit_line(&aid("emp"), ts(T0 + 1)).unwrap();
    prepaid(&store, "old", 100, T0, T0 + 100, T0);
    store
}

#[test]
fn a_request_that_read_the_clock_earlier_cannot_move_a_committed_spend_into_a_revoked_allowance() {
    // High 2。充值 100，授信 0（开过又关了），旧额度 100 的窗口是 [T0, T0 + 100)。调用顺序（提交顺序）：
    //
    //   A  T0 + 101 准入 100：额度窗口已经过了，用的是自有资金 100（没有授信、没有额度可用）
    //   撤销旧额度（A 提交之后）
    //   B  读钟读到的是 T0 + 99（早于 A，排队才拿到写锁），准入 100
    //
    // A 的花费归属时刻是 T0 + 101，不在旧额度的窗口里，所以是自有资金花掉的：自有资金剩余 0；旧额度已撤销，不可花；授信 0。
    // B 没有任何钱可用，拒绝。旧代码把 A 的归属时刻按 B 的读钟压到 T0 + 99，落进旧额度的窗口、算成它覆盖的（撤销在 A 之后提交，
    // 照样覆盖）：自有资金又显示 100，B 放行——同一笔 100 的自有资金花了两次。
    let db = TempDb::new("funded-stale-read-revoked");
    let store = stale_read_world(&db);
    let a = Req::new(&store, "a-1", 0, "cred-e", M100).at(T0 + 101);
    let decision = admit(&store, &a);
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    assert_eq!(price_of(&decision), Some(100));
    assert!(
        credit_used_amounts(&store).is_empty(),
        "own funds, no credit"
    );
    revoke_allocation_at(&db, "old", T0 + 102);

    let b = Req::new(&store, "b-1", 0, "cred-e", M100).at(T0 + 99);
    let rejected = admit(&store, &b);
    assert_eq!(
        denied(&rejected),
        Some(ReasonCode::BudgetExhausted),
        "A spent the 100 of funds at T0 + 101, outside the window: {rejected:?}"
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
fn a_request_with_an_earlier_read_time_may_still_use_an_allowance_that_was_valid_at_that_time() {
    // High 2 的对照（不撤销）：同样的事件，旧额度没被撤销。B 的读钟是 T0 + 99，那一刻旧额度的窗口包含它、一分没动
    // （A 的花费归属 T0 + 101，不在窗口里）：B 可以用旧额度的 100，放行；两笔各 100，一共 200 = 自有资金 100 + 旧额度 100，
    // 正好花完，再来一笔就没有钱了。
    let db = TempDb::new("funded-stale-read-control");
    let store = stale_read_world(&db);
    let a = Req::new(&store, "a-1", 0, "cred-e", M100).at(T0 + 101);
    assert_eq!(price_of(&admit(&store, &a)), Some(100));
    let b = Req::new(&store, "b-1", 0, "cred-e", M100).at(T0 + 99);
    let decision = admit(&store, &b);
    assert_eq!(
        price_of(&decision),
        Some(100),
        "the allowance was valid at T0 + 99: {decision:?}"
    );
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    let c = Req::new(&store, "c-1", 0, "cred-e", M30).at(T0 + 103);
    let none = admit(&store, &c);
    assert_eq!(denied(&none), Some(ReasonCode::BudgetExhausted), "{none:?}");
    assert_eq!(
        view(&store, T0 + 103).ceiling_remaining,
        Some(thb(0)),
        "all 200 is spent"
    );
    assert_clean(&store);
}

#[test]
fn a_spend_that_belongs_to_an_allowance_starting_after_the_read_time_still_uses_that_allowance() {
    // High 2 的第二半：回放要能看到读钟之后才开始的额度。充值 100，授信 0（开过又关了），额度 `b2` 100 窗口 [T0 + 100, T0 + 200)。
    //
    //   S  T0 + 150 准入 100：b2 在窗口里，先用 b2 的 100，自有资金一分没动
    //   B  读钟读到的是 T0 + 99，准入 100：b2 在 B 的时刻还没生效，能用的只有自有资金 100——而 S 用的是 b2，自有资金是满的，放行
    //
    // 只回放读钟那一刻已经开始的额度，S 找不到 b2，被算成自有资金花掉的，B 就被无谓地拒绝了。
    // 放行之后：B 用自有资金 100，S 用 b2 100，一共 200 = 全部的钱，再来一笔拒绝。
    let db = TempDb::new("funded-future-allowance");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    store.disable_credit_line(&aid("emp"), ts(T0 + 1)).unwrap();
    prepaid(&store, "b2", 100, T0 + 100, T0 + 200, T0);

    let s = Req::new(&store, "s-1", 0, "cred-e", M100).at(T0 + 150);
    assert_eq!(price_of(&admit(&store, &s)), Some(100));
    let b = Req::new(&store, "b-1", 0, "cred-e", M100).at(T0 + 99);
    let decision = admit(&store, &b);
    assert_eq!(
        price_of(&decision),
        Some(100),
        "S used b2, so the funds are intact: {decision:?}"
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
// Medium 1：附加成本动了授信，也要通知担保人
// ------------------------------------------------------------------------------------------

/// 附加成本的预留（挂在请求的第 0 号尝试下，主要预留还不存在）。
fn reserve_extra(store: &AccountStore, request: &str, micros: i64, at: i64) -> ComponentDecision {
    store
        .reserve_request_component(
            &idoris_compute_contracts::ids::RequestId::new(request).unwrap(),
            "safety_input",
            &payment(store, "cred-e"),
            &price(micros),
            ts(at),
        )
        .unwrap()
}

/// 一条 `credit_used` 告警的完整内容：（金额，担保人，凭证，预留，说明）。
type AlertContent = (i64, Option<String>, Option<String>, Option<String>, String);

/// `emp` 名下 `credit_used` 告警的完整内容，按写入顺序。
fn credit_used_alerts(store: &AccountStore) -> Vec<AlertContent> {
    store
        .alerts(Some(AlertKind::CreditUsed))
        .unwrap()
        .into_iter()
        .filter(|alert| alert.account == "emp")
        .map(|alert| {
            (
                alert.amount.micros,
                alert.guarantor,
                alert.credential,
                alert.hold,
                alert.detail,
            )
        })
        .collect()
}

#[test]
fn an_extra_cost_that_draws_credit_raises_one_credit_used_alert_and_one_inside_the_allowance_raises_none()
 {
    // 附加成本（比如安全检查）的预留和主要预留走同一份视图，动了授信同样要通知担保人。充值 100、授信 100、额度 100，一共 300。
    //
    //   控制：预留 200 = 额度 100 + 自有资金 100，没有动授信 -> 没有告警
    //   预留 300：额度 100 + 自有资金 100 + 授信 100。S0 的透支额：可用 200，预留 300，透支 100 -> 一条金额 100 的告警
    for (micros, expected_alerts) in [(200_i64, 0_usize), (300, 1)] {
        let db = TempDb::new(&format!("funded-extra-alert-{micros}"));
        let store = credit_world(&db);
        top_up(&store, 100, "emp", T0);
        credit_line(&store, 100, T0);
        prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
        let decision = reserve_extra(&store, "c-1", micros, T0 + 1_000);
        assert!(
            matches!(decision, ComponentDecision::Reserved { .. }),
            "{micros}: {decision:?}"
        );
        let alerts = credit_used_alerts(&store);
        assert_eq!(alerts.len(), expected_alerts, "{micros}: {alerts:?}");
        if expected_alerts == 1 {
            assert_eq!(
                alerts[0],
                (
                    100,
                    Some("paid".to_string()),
                    Some("cred-e".to_string()),
                    Some("c-1#0~safety_input".to_string()),
                    "overdraft_within_credit".to_string()
                ),
                "the guarantor of emp is paid"
            );
        }
        assert_clean(&store);
    }
}

#[test]
fn an_extra_cost_reserved_inside_an_attempt_raises_the_credit_used_alert_too() {
    // 同一份账，走挂在尝试上的 `reserve_component`：主要预留 100（额度）之后，额度 0、自有资金 100、授信 100。
    // 附加成本 150：自有资金 100 + 授信 50，S0 的透支额 = 150 - 100 = 50，一条金额 50 的告警；主要预留本身没有告警。
    let db = TempDb::new("funded-extra-alert-attempt");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
    let main = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 1_000);
    assert_eq!(price_of(&admit(&store, &main)), Some(100));
    assert!(credit_used_alerts(&store).is_empty());
    let decision = store
        .reserve_component(&main.attempt, "safety_output", &price(150), ts(T0 + 2_000))
        .unwrap();
    assert!(
        matches!(decision, ComponentDecision::Reserved { .. }),
        "{decision:?}"
    );
    assert_eq!(
        credit_used_alerts(&store),
        [(
            50,
            Some("paid".to_string()),
            Some("cred-e".to_string()),
            Some("r-1#0~safety_output".to_string()),
            "overdraft_within_credit".to_string()
        )]
    );
    // 金额 0 的附加成本（挂在尝试上的那个入口）也不再记告警
    let decision = store
        .reserve_component(&main.attempt, "safety_zero", &price(0), ts(T0 + 3_000))
        .unwrap();
    assert!(
        matches!(decision, ComponentDecision::Reserved { .. }),
        "{decision:?}"
    );
    assert_eq!(credit_used_alerts(&store).len(), 1);
    assert_clean(&store);
}

#[test]
fn an_extra_cost_raises_the_credit_used_alert_on_the_other_account_shapes_too() {
    // 只有授信：附加成本 60 全部是授信（可用 0，透支 60）-> 告警 60；再来 40，授信占用累计 100 -> 告警 100（含已经欠下的 60）。
    let db = TempDb::new("funded-extra-alert-credit-only");
    let store = credit_world(&db);
    credit_line(&store, 100, T0);
    assert!(matches!(
        reserve_extra(&store, "c-1", 60, T0 + 1_000),
        ComponentDecision::Reserved { .. }
    ));
    assert!(matches!(
        reserve_extra(&store, "c-2", 40, T0 + 2_000),
        ComponentDecision::Reserved { .. }
    ));
    assert_eq!(
        credit_used_alerts(&store)
            .iter()
            .map(|alert| alert.0)
            .collect::<Vec<_>>(),
        [60, 100]
    );
    // 金额 0 的附加成本不占任何钱，不是「动了授信」：账户欠着授信的钱（S0 对它也是 `PassUsingCredit`），但不再记一条重复的告警
    assert!(matches!(
        reserve_extra(&store, "c-3", 0, T0 + 3_000),
        ComponentDecision::Reserved { .. }
    ));
    assert_eq!(
        credit_used_alerts(&store)
            .iter()
            .map(|alert| alert.0)
            .collect::<Vec<_>>(),
        [60, 100],
        "a zero-amount extra cost draws no credit"
    );
    assert_clean(&store);

    // 充了值、没有额度（S0 的 `余额 + 授信`，视图仍是余额）：充值 100、授信 100，附加成本 100 全在自有资金里 -> 没有告警；
    // 再来 50，余额 0，授信承担 50 -> 告警 50。
    let db = TempDb::new("funded-extra-alert-balance");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    assert!(matches!(
        reserve_extra(&store, "c-1", 100, T0 + 1_000),
        ComponentDecision::Reserved { .. }
    ));
    assert!(credit_used_alerts(&store).is_empty());
    assert!(matches!(
        reserve_extra(&store, "c-2", 50, T0 + 2_000),
        ComponentDecision::Reserved { .. }
    ));
    assert_eq!(
        credit_used_alerts(&store)
            .iter()
            .map(|alert| alert.0)
            .collect::<Vec<_>>(),
        [50]
    );
    // 金额 0 的附加成本：余额 -50 时 S0 对它也是 `PassUsingCredit`（透支 50），但它不占任何钱，不再记一条重复的金额 50 的告警
    assert!(matches!(
        reserve_extra(&store, "c-3", 0, T0 + 3_000),
        ComponentDecision::Reserved { .. }
    ));
    assert_eq!(
        credit_used_alerts(&store)
            .iter()
            .map(|alert| alert.0)
            .collect::<Vec<_>>(),
        [50],
        "a zero-amount extra cost draws no credit: the alert sequence is still [50]"
    );
    assert_clean(&store);
}

#[test]
fn an_extra_cost_covered_by_a_new_allowance_while_the_account_owes_credit_raises_no_alert() {
    // 和主要预留同一条规则：S0 只看净头寸，旧债未清时一笔完全落在新额度里的附加成本，`CreditFloor` 照样是 `PassUsingCredit`，
    // 但它没有动授信。充值 100、授信 100，先花 200（自有资金 100 + 授信 100，一条金额 100 的告警），之后发新额度 100。
    //
    //   附加成本 50：额度剩余 100 + 自有资金剩余 0 = 100 >= 50，落在新额度里 -> 没有新的告警（旧代码丢掉判断，会记一条金额 50 的告警）
    //   之后：额度剩余 100 - 50 = 50，自有资金 0，债务 100，净头寸 50 - 100 = -50，能花的 -50 + 100 = 50
    let db = TempDb::new("funded-extra-alert-owing");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    let big = spend(&store, "r-1", T0 + 1_000, 95);
    assert_eq!(price_of(&big), Some(200), "{big:?}");
    assert_eq!(credit_used_amounts(&store), [100]);
    let later = T0 + 10_000;
    prepaid(&store, "emp-new", 100, later, later + 30 * DAY, later);

    let decision = reserve_extra(&store, "c-1", 50, later + 1_000);
    assert!(
        matches!(decision, ComponentDecision::Reserved { .. }),
        "{decision:?}"
    );
    assert_eq!(
        credit_used_amounts(&store),
        [100],
        "the 50 sits inside the new allowance: the credit line is not touched"
    );
    let after = view(&store, later + 2_000);
    assert_eq!(after.ceiling_remaining, Some(thb(-50)));
    assert_eq!(headroom(&after), 50);
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 超出预留的归属时刻：跨过额度到期的请求（owner 需要确认的口径）
// ------------------------------------------------------------------------------------------

#[test]
fn the_overage_of_a_request_that_straddles_the_expiry_of_its_allowance_is_judged_when_it_settles() {
    // 超出预留的部分是结算那一刻才发生的新花费，用的是结算那一刻的额度（和担保人能吸收多少读的是同一份额度）。
    // 充值 100、授信 100，额度 `a` 300 的窗口在 T0 + 1_000 结束。
    //
    //   T0 + 500    预留 100：在 `a` 的窗口里，`a` 覆盖它（剩 200）
    //   T0 + 2_000  按 250 结算（预留还没过 TTL），超出 150；`a` 已经到期
    //
    // 结算时：额度 0、自有资金 100（预留被 `a` 覆盖，没动自有资金）、授信可用 100，能花的 200 >= 150，担保人吸收 150。
    // 超出的 150 在 T0 + 2_000 记账，`a` 够不着：自有资金 100 + 授信 50。结算之后额度 0、自有资金 0、债务 50，能花的 50。
    // （旧写法把整笔 250 记在 T0 + 500，`a` 全部覆盖：自有资金 100、授信 100 一分没动，能花的 200。新口径对用户更严：
    // 请求跨过额度到期时，超出的部分花的是自有资金和授信，不是已经到期的额度。方案里没有说过这一点，列在待 owner 确认里。）
    let db = TempDb::new("funded-overage-straddles-expiry");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "a", 300, T0 - DAY, T0 + 1_000, T0);
    let first = Req::new(&store, "r-1", 0, "cred-e", M100).at(T0 + 500);
    assert_eq!(price_of(&admit(&store, &first)), Some(100));
    assert!(credit_used_amounts(&store).is_empty());

    let outcome = settle_at(&store, &first, 250, T0 + 2_000);
    assert_eq!(outcome.overage, thb(150));
    assert_eq!(outcome.user_borne, thb(100));
    assert_eq!(
        (
            outcome.sponsor_covered.clone(),
            outcome.platform_loss.clone()
        ),
        (thb(150), thb(0)),
        "{outcome:?}"
    );
    assert!(!outcome.credential_frozen);
    let after = view(&store, T0 + 2_001);
    assert_eq!(after.ceiling_remaining, Some(thb(-50)));
    assert_eq!(headroom(&after), 50);
    let next = admit(
        &store,
        &Req::new(&store, "r-2", 0, "cred-e", M100).at(T0 + 2_002),
    );
    assert_eq!(price_of(&next), Some(50), "{next:?}");
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 并发
// ------------------------------------------------------------------------------------------

/// 让每个请求各自在一个线程里（各开一条连接）同时放行，返回各自的准入决定。竞争期间采样者线程反复读 `emp` 的独立模型：
/// 任何时刻授信债务都不超过授信额度，已花（含在途）不超过额度加自有资金加授信，三本账的金额守恒。
fn race(
    db: &TempDb,
    requests: Vec<Req>,
    now: i64,
    credit: i64,
    spend_cap: i64,
) -> Vec<AdmissionDecision> {
    let barrier = Arc::new(Barrier::new(requests.len() + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(AtomicUsize::new(0));
    let sampler = {
        let path = db.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        let samples = Arc::clone(&samples);
        thread::spawn(move || {
            barrier.wait();
            loop {
                let books = model(&path, "emp", now);
                assert!(
                    books.credit_debt <= credit,
                    "the credit debt {} went beyond the credit line {credit}: {books:?}",
                    books.credit_debt
                );
                assert!(
                    books.spent <= spend_cap,
                    "spent {} (settled + in flight) went beyond the {spend_cap} that allowance, funds and credit allow",
                    books.spent
                );
                assert_eq!(books.covered + books.uncovered, books.spent, "{books:?}");
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
fn concurrent_admissions_split_exactly_between_allowance_funds_and_credit() {
    // 充值 100、额度 100、授信 100：一共 300，24 个并发的 100 恰好放行 3 个。旧代码额度只是上限，能花的是 min(100, 100) + 100 = 200，只放行 2 个。
    let db = TempDb::new("funded-race-split");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    prepaid(&store, "emp-allow", 100, T0 - DAY, T0 + 30 * DAY, T0);
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("race-{i}"), 0, "cred-e", M100).at(T0 + 1_000))
        .collect();
    let decisions = race(&db, requests, T0 + 1_000, 100, 300);
    let admitted = decisions.iter().filter(|d| d.is_admitted()).count();
    assert_eq!(admitted, 3, "{decisions:?}");
    assert!(
        decisions
            .iter()
            .filter(|d| !d.is_admitted())
            .all(|d| denied(d) == Some(ReasonCode::BudgetExhausted)),
        "{decisions:?}"
    );
    let books = check_books(&store, &db, "emp", T0 + 2_000);
    assert_eq!(
        (
            books.covered,
            books.funds_left,
            books.credit_debt,
            books.spent
        ),
        (100, 0, 100, 300)
    );
    assert_eq!(
        credit_used_amounts(&store),
        [100],
        "only the last of the three used credit"
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_spend_a_new_allowance_twice_on_an_account_that_owes_credit() {
    // 充值 100、授信 100，已经花到余额 -100；新发额度 100：24 个并发的 100 恰好放行 1 个（新额度），授信一分没多占。
    let db = TempDb::new("funded-race-owes");
    let store = credit_world(&db);
    top_up(&store, 100, "emp", T0);
    credit_line(&store, 100, T0);
    for (request, at) in [("r-1", T0 + 1_000), ("r-2", T0 + 2_000)] {
        assert!(spend(&store, request, at, M100).is_admitted(), "{request}");
    }
    let later = T0 + 10_000;
    prepaid(&store, "emp-allow", 100, later, later + 30 * DAY, later);
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("race-{i}"), 0, "cred-e", M100).at(later + 1_000))
        .collect();
    let decisions = race(&db, requests, later + 1_000, 100, 300);
    assert_eq!(
        decisions.iter().filter(|d| d.is_admitted()).count(),
        1,
        "{decisions:?}"
    );
    let books = check_books(&store, &db, "emp", later + 2_000);
    assert_eq!(
        (books.covered, books.credit_debt, books.spent),
        (100, 100, 300)
    );
    assert_clean(&store);
}

/// 一个有充值、授信、额度各 2000 的 `emp`：一共 6000，能同时放下 24 个预留 120。返回 24 个都在途的请求。
fn settlement_world(db: &TempDb) -> (AccountStore, Vec<Req>) {
    let store = credit_world(db);
    top_up(&store, 2_000, "emp", T0);
    credit_line(&store, 2_000, T0);
    prepaid(&store, "emp-allow", 2_000, T0 - DAY, T0 + 30 * DAY, T0);
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| {
            Req::new(&store, &format!("s-{i}"), 0, "cred-e", 55)
                .at(T0 + 1_000 + i64::try_from(i).unwrap())
        })
        .collect();
    for request in &requests {
        assert_eq!(price_of(&admit(&store, request)), Some(120));
    }
    (store, requests)
}

/// 第 `i` 个预留的收尾：每五个里一个释放（0），其余按 `55 + 6i` 结算（比预留 120 少的、多的、整好相等的都有）。
fn charge_of(i: usize) -> i64 {
    if i.is_multiple_of(5) {
        0
    } else {
        55 + 6 * i64::try_from(i).unwrap()
    }
}

fn close(store: &AccountStore, request: &Req, i: usize, at: i64) {
    match charge_of(i) {
        0 => {
            store.release_attempt(&request.attempt, ts(at)).unwrap();
        }
        charge => {
            let outcome = store
                .settle_attempt(&request.attempt, &price(charge), ts(at))
                .unwrap();
            assert_eq!(outcome.result, SettleResult::Settled);
            // 账本里每一次结算都守恒：实际消费 = 用户承担 + 担保人承担 + 平台承担
            assert_eq!(
                outcome.charged.micros,
                outcome.user_borne.micros
                    + outcome.sponsor_covered.micros
                    + outcome.platform_loss.micros
                    + outcome.grace_absorbed.micros,
                "{outcome:?}"
            );
            assert_eq!(
                outcome.platform_loss,
                thb(0),
                "the chain has room for every overage"
            );
        }
    }
}

#[test]
fn concurrent_settlements_of_one_funded_account_leave_exactly_the_books_a_serial_run_leaves() {
    // 24 个预留同时在途，24 个线程同时收尾（结算的实际金额有少于、等于、多于预留的，每五个里一个释放）。采样者线程在竞争期间反复读独立模型。
    // 收尾完之后：在途归零、已花等于所有结算之和、三本账和一条一条按顺序收尾得到的完全一样（一笔花费先后看它第一条分录，和谁先结算无关）。
    let db = TempDb::new("funded-race-settle");
    let (store, requests) = settlement_world(&db);

    let barrier = Arc::new(Barrier::new(THREADS + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let path = db.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            barrier.wait();
            let mut samples = 0_u32;
            loop {
                let books = model(&path, "emp", T0 + 5_000);
                assert_eq!(books.covered + books.uncovered, books.spent, "{books:?}");
                assert!(books.credit_debt <= 2_000, "{books:?}");
                assert!(books.spent <= 6_000, "{books:?}");
                samples += 1;
                if stop.load(Ordering::Relaxed) {
                    break samples;
                }
            }
        })
    };
    let handles: Vec<_> = requests
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, request)| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                close(&store, &request, i, T0 + 3_000);
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    assert!(sampler.join().unwrap() > 0);

    let total: i64 = (0..THREADS).map(charge_of).sum();
    let concurrent = check_books(&store, &db, "emp", T0 + 5_000);
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
        ),
        0
    );
    assert_eq!(concurrent.spent, total);
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
        ),
        total,
        "every settlement was charged exactly once"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(liability_micros), 0) FROM ledger_entries WHERE kind = 'sponsor_covered'"
        ),
        (0..THREADS)
            .map(|i| (charge_of(i) - 120).max(0))
            .sum::<i64>(),
        "the guarantor covers exactly the overage, nobody else"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_entries WHERE kind = 'platform_loss'"
        ),
        0
    );
    assert_clean(&store);

    // 串行对照：同样的预留、同样的收尾，一条一条按顺序做
    let serial_db = TempDb::new("funded-race-settle-serial");
    let (serial_store, serial_requests) = settlement_world(&serial_db);
    for (i, request) in serial_requests.iter().enumerate() {
        close(&serial_store, request, i, T0 + 3_000);
    }
    assert_eq!(
        check_books(&serial_store, &serial_db, "emp", T0 + 5_000),
        concurrent,
        "the settlement order must not change the books"
    );
    assert_eq!(
        view(&serial_store, T0 + 5_000).ceiling_remaining,
        view(&store, T0 + 5_000).ceiling_remaining
    );
}

#[test]
fn settling_while_others_are_being_admitted_never_over_commits_the_account() {
    // 12 个预留在途（占 12 × 100 = 1200），一边线程们同时收尾（都不超过预留）、一边另一批线程同时准入 12 个新的：
    // 容量 = 充值 600 + 额度 600 + 授信 600 = 1800；任何时刻（采样者）已花加在途都不超过 1800，授信债务不超过 600。
    // 新放行的个数取决于收尾和准入谁先到：至少 6 个（容量里本来就空着 600）；收尾少收的 6 × 60 = 360 让出来之后，后到的请求
    // 还能拿到被压低的预留，所以最多时 12 个都放行。不管几个，已花加在途从不超过容量。
    let db = TempDb::new("funded-race-mixed");
    let store = credit_world(&db);
    top_up(&store, 600, "emp", T0);
    credit_line(&store, 600, T0);
    prepaid(&store, "emp-allow", 600, T0 - DAY, T0 + 30 * DAY, T0);
    let open: Vec<Req> = (0..12)
        .map(|i| Req::new(&store, &format!("o-{i}"), 0, "cred-e", M100).at(T0 + 1_000 + i))
        .collect();
    for request in &open {
        assert_eq!(price_of(&admit(&store, request)), Some(100));
    }
    let fresh: Vec<Req> = (0..12)
        .map(|i| Req::new(&store, &format!("n-{i}"), 0, "cred-e", M100).at(T0 + 3_000 + i))
        .collect();

    let barrier = Arc::new(Barrier::new(open.len() + fresh.len() + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let path = db.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            barrier.wait();
            loop {
                let books = model(&path, "emp", T0 + 5_000);
                assert!(books.spent <= 1_800, "over-committed: {books:?}");
                assert!(books.credit_debt <= 600, "{books:?}");
                assert_eq!(books.covered + books.uncovered, books.spent, "{books:?}");
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
    };
    let mut handles = Vec::new();
    for (i, request) in open.iter().cloned().enumerate() {
        let path = db.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let store = AccountStore::open(&path).unwrap();
            barrier.wait();
            // 偶数号按预留收，奇数号只收 40（少收，把额度还回去）
            let charge = if i.is_multiple_of(2) { 100 } else { 40 };
            store
                .settle_attempt(&request.attempt, &price(charge), ts(T0 + 4_000))
                .unwrap();
            true
        }));
    }
    for request in fresh {
        let path = db.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let store = AccountStore::open(&path).unwrap();
            barrier.wait();
            store
                .admit_attempt(&request.input())
                .expect("admission must not error under contention")
                .is_admitted()
        }));
    }
    let admitted_new = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .skip(open.len())
        .filter(|admitted| *admitted)
        .count();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();

    assert!(
        admitted_new >= 6,
        "capacity 1800 with 1200 in flight leaves room for six whatever the interleaving: {admitted_new}"
    );
    let books = check_books(&store, &db, "emp", T0 + 6_000);
    assert!(books.spent <= 1_800, "{books:?}");
    assert!(books.credit_debt <= 600, "{books:?}");
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 随机历史：视图、两个独立模型、准入决定、告警、结算分摊对得上
// ------------------------------------------------------------------------------------------
//
// 期望从 [`Sim`] 来：它只吃这个测试自己做过的事（充值、发额度、撤销额度、预留、释放、结算）和业务规则，不读库、不读被测实现。
// 每一步之后，视图、读原始表回放的 `model`（账本的分录有没有记对）、`Sim` 三方必须逐项一致。历史里有三件旧测试没覆盖的事：
//
// * 超出预留的结算（担保人全部吸收）：超出的部分记在结算那一刻，用的是结算那一刻的额度；
// * 读钟乱序（`jitter`）：三分之一的步骤用的是排队之前取的旧时间，比前面已经提交的花费更早，也和前一步同一毫秒；
// * 迟发、起点倒填的额度。

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

/// 按模型预测一个预留 `reserve` 微泰铢的请求在账上的结果：能花的 = 额度剩余 + 自有资金剩余 + 授信可用；
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

/// 结算 `req`（预留 `reserved`，实际 `charged`），同步到 `Sim`，并按**手算规则**核对这次结算的分摊（`Sim::settle`）：先记预留之内的部分、再读
/// 整条链的余量，担保人能吸收 `min(超出, max(余量, 0))`，吸收不了的是平台损失，凭证因此被冻结（然后解冻，历史接着走）。超出预留的部分
/// **不再**被夹到余量以内：余量不够时要有平台损失和冻结。`legacy`：结算之后把这个预留的分录改回 v0.5.0 的写法（超出的部分并回 `settle`、
/// 没有 `late_settle`），账不能变——读旧格式的分录要和新格式是同一回事。
#[allow(clippy::too_many_arguments)]
fn settle_both(
    store: &AccountStore,
    db: &TempDb,
    sim: &mut Sim,
    req: &Req,
    spend: usize,
    reserved: i64,
    charged: i64,
    at: i64,
    legacy: bool,
    reached: &mut Reached,
) {
    let hold = store.hold(&req.attempt, "model").unwrap().unwrap().hold_id;
    let extra = (charged - reserved).max(0);
    if extra > 0 {
        reached.overage_settled += 1;
        if sim.books(at).allowance_left > 0 {
            reached.overage_with_allowance += 1;
        }
    }
    let outcome = settle_at(store, req, charged, at);
    let expected = sim.settle(spend, charged, at);
    assert_eq!(outcome.result, SettleResult::Settled);
    assert!(!outcome.late, "{outcome:?}");
    assert_eq!(outcome.overage, thb(expected.overage), "{outcome:?}");
    assert_eq!(
        outcome.sponsor_covered,
        thb(expected.sponsor),
        "{outcome:?}"
    );
    assert_eq!(outcome.platform_loss, thb(expected.loss), "{outcome:?}");
    assert_eq!(
        outcome.credential_frozen,
        expected.loss > 0,
        "a platform loss freezes the credential, and only a platform loss does: {outcome:?}"
    );
    reached.loss_total += expected.loss;
    assert_eq!(
        store.loss_total("platform_loss").unwrap(),
        reached.loss_total,
        "the platform's losses are exactly the overage the chain could not absorb"
    );
    if expected.loss > 0 {
        reached.loss_settled += 1;
        unfreeze_credential(db, "cred-e");
    }
    if legacy && extra > 0 {
        rewrite_overage_as_v0_5_0(db, &hold);
        reached.legacy_rewritten += 1;
    }
}

/// 随机历史走过了哪些有意思的状态。
#[derive(Debug, Default, Clone, Copy)]
struct Reached {
    allowed: u32,
    degraded: u32,
    rejected: u32,
    /// 额度剩余和授信债务同时大于 0：「新额度照常可花、旧债不被抵」。
    allowance_and_debt: u32,
    /// 额度剩余和自有资金剩余同时大于 0。
    allowance_and_funds: u32,
    /// 有授信债务时又充了值（还债）。
    top_up_with_debt: u32,
    /// 授信被占满。
    credit_full: u32,
    /// 这一笔完全落在额度和自有资金里，而账户欠着授信的钱：不该有告警。
    covered_while_owing: u32,
    /// 这一笔动了授信：该有一条告警。
    alerted: u32,
    revoked_after_cover: u32,
    released: u32,
    half_settled: u32,
    /// 超出预留的结算。
    overage_settled: u32,
    /// 超出预留的结算，结算那一刻还有额度可花：超出的部分用了结算那一刻的额度。
    overage_with_allowance: u32,
    /// 超出预留的部分超过了整条链能吸收的：有平台损失，凭证被冻结（然后解冻，历史接着走）。
    loss_settled: u32,
    /// 平台损失的总额。
    loss_total: i64,
    /// 结算之后把分录改回 v0.5.0 的写法（超出的部分并回 `settle`）的次数。
    legacy_rewritten: u32,
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
        self.allowance_and_funds += other.allowance_and_funds;
        self.top_up_with_debt += other.top_up_with_debt;
        self.credit_full += other.credit_full;
        self.covered_while_owing += other.covered_while_owing;
        self.alerted += other.alerted;
        self.revoked_after_cover += other.revoked_after_cover;
        self.released += other.released;
        self.half_settled += other.half_settled;
        self.overage_settled += other.overage_settled;
        self.overage_with_allowance += other.overage_with_allowance;
        self.loss_settled += other.loss_settled;
        self.loss_total += other.loss_total;
        self.legacy_rewritten += other.legacy_rewritten;
        self.stale_stamped += other.stale_stamped;
        self.read_behind_a_spend += other.read_behind_a_spend;
    }
}

/// `jitter`：一半的步骤写进表里的毫秒（额度的创建时间、撤销时间、请求的读钟）比提交顺序更早（排队乱序，最多早 6 秒），也会和前一步相同（同毫秒）；
/// 先后仍是调用顺序。
fn run_random_history(seed: u64, steps: usize, initial_funds: i64, jitter: bool) -> Reached {
    const CREDIT: i64 = 200;
    let db = TempDb::new(&format!("funded-random-{seed}-{initial_funds}-{jitter}"));
    let store = credit_world(&db);
    credit_line(&store, CREDIT, T0);
    let mut sim = Sim::new(CREDIT);
    if initial_funds > 0 {
        top_up(&store, initial_funds, "init", T0);
        sim.top_up(initial_funds);
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut now = T0;
    // （额度 id，它在 Sim 里的编号）
    let mut allowances: Vec<(String, usize)> = Vec::new();
    let mut revoked: Vec<String> = Vec::new();
    // 在途的请求，和它在 Sim 里的花费编号
    let mut open: Vec<(Req, usize)> = Vec::new();
    let mut next_request = 0_u32;
    let mut next_top_up = 0_u32;
    let mut reached = Reached::default();
    for step in 0..steps {
        now += 1_000 * (1 + rng.below(4));
        let stamp = if jitter && rng.below(2) == 0 {
            reached.stale_stamped += 1;
            // 授信是 T0 起生效的：读钟不早于 T0（更早时授信还没生效，模型里的授信额度是个常数）
            (now - 1_000 * rng.below(7)).max(T0)
        } else {
            now
        };
        match rng.below(12) {
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
                                &db,
                                &mut sim,
                                &req,
                                spend,
                                reserved,
                                reserved / 2,
                                now,
                                false,
                                &mut reached,
                            );
                            reached.half_settled += 1;
                        }
                        _ => {
                            // 超出预留的部分不再夹到余量以内：余量不够就是平台损失，凭证被冻结
                            let extra =
                                [1_i64, 20, 60, 150, 400][usize::try_from(rng.below(5)).unwrap()];
                            let legacy = rng.below(3) == 0;
                            settle_both(
                                &store,
                                &db,
                                &mut sim,
                                &req,
                                spend,
                                reserved,
                                reserved + extra,
                                now,
                                legacy,
                                &mut reached,
                            );
                        }
                    }
                }
            }
            4 => {
                let amount = [30_i64, 50, 100, 250][usize::try_from(rng.below(4)).unwrap()];
                if sim.books(now).credit_debt > 0 {
                    reached.top_up_with_debt += 1;
                }
                next_top_up += 1;
                top_up(&store, amount, &format!("rnd-{seed}-{next_top_up}"), now);
                sim.top_up(amount);
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
                let alerts_before = credit_used_amounts(&store).len();
                let req = Req::new(
                    &store,
                    &format!("q-{seed}-{next_request}"),
                    0,
                    "cred-e",
                    max_tokens,
                )
                .at(stamp);
                let decision = admit(&store, &req);
                let got = price_of(&decision);
                assert_eq!(
                    got, want,
                    "seed {seed} step {step}: reserve {reserve}, books {before:?}: {decision:?}"
                );
                match &decision {
                    AdmissionDecision::Allow { .. } => reached.allowed += 1,
                    AdmissionDecision::Degrade { .. } => reached.degraded += 1,
                    _ => reached.rejected += 1,
                }
                // 告警：这一笔动了授信（超出额度和自有资金）才有一条，金额是准入之后授信占用的总额
                let alerts = credit_used_amounts(&store);
                match got {
                    Some(charged) if charged > before.without_credit() => {
                        reached.alerted += 1;
                        assert_eq!(
                            alerts.len(),
                            alerts_before + 1,
                            "seed {seed} step {step}: this spend drew credit and must raise one alert: {before:?}"
                        );
                        assert_eq!(
                            alerts.last().copied(),
                            Some(
                                charged - before.without_credit() + before.credit_debt.min(CREDIT)
                            ),
                            "seed {seed} step {step}: {before:?}"
                        );
                    }
                    _ => {
                        if got.is_some() && before.credit_debt > 0 {
                            reached.covered_while_owing += 1;
                        }
                        assert_eq!(
                            alerts.len(),
                            alerts_before,
                            "seed {seed} step {step}: no credit was drawn, no alert: {before:?} {decision:?}"
                        );
                    }
                }
                if let Some(charged) = got {
                    let spend = sim.reserve(stamp, charged);
                    if rng.below(4) == 0 {
                        open.push((req, spend)); // 留着在途，之后的某一步再收尾
                    } else {
                        let at = now + 500;
                        let extra = if rng.below(4) == 0 {
                            [1_i64, 20, 60, 150, 400][usize::try_from(rng.below(5)).unwrap()]
                        } else {
                            0
                        };
                        let legacy = rng.below(3) == 0;
                        settle_both(
                            &store,
                            &db,
                            &mut sim,
                            &req,
                            spend,
                            charged,
                            charged + extra,
                            at,
                            legacy,
                            &mut reached,
                        );
                    }
                } else {
                    assert_eq!(denied(&decision), Some(ReasonCode::BudgetExhausted));
                }
            }
        }
        // 每一步之后：视图、模型、事件三方对得上，授信占用不超过授信额度
        let books = check_against_sim(&store, &db, &sim, now + 700);
        assert!(
            books.credit_debt <= CREDIT + reached.loss_total,
            "seed {seed} step {step}: credit use beyond the line by more than the platform lost: {books:?}"
        );
        if books.allowance_left > 0 && books.credit_debt > 0 {
            reached.allowance_and_debt += 1;
        }
        if books.allowance_left > 0 && books.funds_left > 0 {
            reached.allowance_and_funds += 1;
        }
        if books.credit_debt == CREDIT {
            reached.credit_full += 1;
        }
    }
    assert_clean(&store);
    reached
}

#[test]
fn random_histories_with_top_ups_agree_with_the_independent_models_decisions_and_alerts() {
    let mut total = Reached::default();
    for seed in [1_u64, 2, 3, 4, 5, 6] {
        total.add(run_random_history(seed, 50, 0, false));
    }
    for seed in [21_u64, 22, 23, 24, 25, 26] {
        total.add(run_random_history(seed, 50, 100, false));
    }
    eprintln!("funded random histories reached: {total:?}");
    // 没有走到这些状态，上面「几方对得上」就没有说服力
    assert!(total.allowed >= 100, "{total:?}");
    assert!(total.degraded >= 15, "{total:?}");
    assert!(total.rejected >= 40, "{total:?}");
    assert!(total.allowance_and_debt >= 60, "{total:?}");
    assert!(total.allowance_and_funds >= 25, "{total:?}");
    assert!(total.top_up_with_debt >= 15, "{total:?}");
    assert!(total.credit_full >= 60, "{total:?}");
    assert!(total.covered_while_owing >= 30, "{total:?}");
    assert!(total.alerted >= 40, "{total:?}");
    assert!(total.revoked_after_cover >= 15, "{total:?}");
    assert!(total.released >= 5, "{total:?}");
    assert!(total.half_settled >= 3, "{total:?}");
    assert!(total.overage_settled >= 15, "{total:?}");
    assert!(total.overage_with_allowance >= 5, "{total:?}");
    assert!(total.loss_settled >= 8, "{total:?}");
    assert!(total.legacy_rewritten >= 6, "{total:?}");
}

#[test]
fn random_histories_with_out_of_order_clocks_still_agree_with_the_independent_models() {
    // 同样的随机历史（有超出预留的结算），但额度的创建时间、撤销时间、请求的读钟有一半取的是排队之前的旧时间：比前面已经提交的
    // 花费更早，也会和前一步同一毫秒；一半的新额度起点倒填。先后始终是调用顺序（提交顺序），视图、两个模型、准入决定、告警、
    // 结算分摊必须仍然对得上。
    let mut total = Reached::default();
    for seed in [31_u64, 32, 33, 34, 35, 36] {
        total.add(run_random_history(seed, 60, 0, true));
    }
    for seed in [41_u64, 42, 43, 44, 45, 46] {
        total.add(run_random_history(seed, 60, 100, true));
    }
    eprintln!("funded jittered random histories reached: {total:?}");
    assert!(total.stale_stamped >= 80, "{total:?}");
    assert!(total.read_behind_a_spend >= 20, "{total:?}");
    assert!(total.allowed >= 100, "{total:?}");
    assert!(total.rejected >= 30, "{total:?}");
    assert!(total.allowance_and_debt >= 40, "{total:?}");
    assert!(total.credit_full >= 40, "{total:?}");
    assert!(total.alerted >= 30, "{total:?}");
    assert!(total.overage_settled >= 20, "{total:?}");
    assert!(total.overage_with_allowance >= 5, "{total:?}");
    assert!(total.loss_settled >= 8, "{total:?}");
    assert!(total.legacy_rewritten >= 10, "{total:?}");
}
