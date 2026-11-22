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
//! 这个文件有一个**独立于被测代码**的模型（[`model`]）：只读原始表（`allocations`、`allocation_revocations`、`ledger_allocation_marks`、
//! `ledger_entries`），把「额度创建」「额度撤销」「一笔花费」按提交先后逐件回放，再把充值加进来，不经过被测的投影函数。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢，所以 `max_tokens = 45`
//! 的预留正好是 100，`max_tokens = 10` 是最低有用的预留 30。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use fixture::*;
use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::admission::{AdmissionDecision, AncestorView};
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AlertKind, CreditLineSetting, SettleResult};

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
// 独立于被测代码的模型
// ------------------------------------------------------------------------------------------

/// 一个账户上额度、自有资金、授信债务三本账。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Books {
    /// 此刻还能花的额度：没被撤销、此刻生效的额度，扣掉它们已经覆盖的花费之后的剩余。
    allowance_left: i64,
    /// 已花（含在途预留）里被额度覆盖的部分。
    covered: i64,
    /// 已花（含在途预留）里没被额度覆盖的部分：由自有资金和授信承担。
    uncovered: i64,
    /// 全部充值。
    funds: i64,
    /// 自有资金剩余：`max(充值 - 没被额度覆盖的花费, 0)`。
    funds_left: i64,
    /// 授信债务：`max(没被额度覆盖的花费 - 充值, 0)`。
    credit_debt: i64,
    /// 分录里 `Σ (held + spent)`：已花加在途。
    spent: i64,
}

impl Books {
    /// 不动授信还能花的钱。
    fn without_credit(&self) -> i64 {
        self.allowance_left + self.funds_left
    }

    /// 授信额度为 `credit` 时一共还能花的钱：额度剩余 + 自有资金剩余 + 授信可用。
    fn room(&self, credit: i64) -> i64 {
        self.without_credit() + credit - self.credit_debt.min(credit)
    }

    /// 视图里 `ceiling_remaining` 应该是的净头寸：`额度剩余 + 自有资金剩余 - min(债务, 授信额度)`。
    fn net(&self, credit: i64) -> i64 {
        self.without_credit() - self.credit_debt.min(credit)
    }
}

/// 读原始表，按事件逐笔回放（不经过被测代码）：
///
/// * 事件按**提交先后**排：分录号是提交的先后；额度创建 / 撤销的序号 `after_entry` 是「它提交那一刻账本里最后一条分录的号」，
///   所以它排在这个号的分录之后、下一个号的分录之前；毫秒只用来判断额度自己的窗口 `[valid_from, valid_until)`；
/// * 一笔花费 = 一个预留在这个账户上、同一个归属时刻上的 `held + spent` 净额，先后看它第一条分录的号；
/// * 回放时维护「此刻已经创建、没被撤销的额度」：一笔花费先用其中窗口包含它的归属时刻的额度里**先到期**的（同时到期的先创建的先用），
///   用完的部分记成「没被额度覆盖」；
/// * 额度之外的花费先用自有资金（全部充值，不分先后：充值不过期），用完才是授信债务。
fn model(db_path: &Path, account: &str, now: i64) -> Books {
    struct Bucket {
        from: i64,
        until: i64,
        left: i64,
        revoked: bool,
    }
    // 事件在时间线上的位置：`(分录号, 种类, 下标)`。种类 0 = 一笔花费，1 = 额度创建，2 = 额度撤销。
    let mut buckets: Vec<Bucket> = Vec::new();
    let mut events: Vec<(i64, u8, usize)> = Vec::new();
    let mut spends: Vec<(i64, i64)> = Vec::new(); // (归属时刻, 净额)
    let funds: i64;
    {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.busy_timeout(Duration::from_secs(10)).unwrap();
        // 一个读事务里读完所有表：竞争测试里采样者线程读的时候别的线程还在写，分开读会读到不一致的快照
        conn.execute_batch("BEGIN").unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT a.valid_from_ms, a.valid_until_ms, a.ceiling_micros,
                        (SELECT m.after_entry FROM ledger_allocation_marks m
                          WHERE m.allocation_id = a.allocation_id AND m.event = 'created'),
                        (SELECT m.after_entry FROM ledger_allocation_marks m
                          WHERE m.allocation_id = a.allocation_id AND m.event = 'revoked')
                   FROM allocations a
                  WHERE a.child_id = ?1 AND a.purpose IN ('free_tier', 'prepaid', 'org_quota')
                    AND a.valid_from_ms <= ?2 AND a.ceiling_micros > 0
                  ORDER BY a.rowid",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![account, now], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (from, until, cap, created_after, revoked_after) = row.unwrap();
            let index = buckets.len();
            buckets.push(Bucket {
                from,
                until,
                left: cap,
                revoked: false,
            });
            events.push((
                created_after.expect("every allocation has a creation mark"),
                1,
                index,
            ));
            if let Some(after) = revoked_after {
                events.push((after, 2, index));
            }
        }
        drop(stmt);
        let mut stmt = conn
            .prepare(
                "SELECT MIN(entry_id), window_at_ms, SUM(held_micros + spent_micros) FROM ledger_entries
                  WHERE account_id = ?1 AND hold_id IS NOT NULL
                  GROUP BY hold_id, window_at_ms
                 HAVING SUM(held_micros + spent_micros) <> 0",
            )
            .unwrap();
        let rows = stmt
            .query_map([account], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (first_entry, at, amount) = row.unwrap();
            assert!(amount > 0, "a hold never nets to a negative amount");
            events.push((first_entry, 0, spends.len()));
            spends.push((at.min(now), amount));
        }
        drop(stmt);
        funds = conn
            .query_row(
                "SELECT COALESCE(SUM(funds_micros), 0) FROM ledger_entries WHERE account_id = ?1 AND kind = 'fund'",
                [account],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute_batch("COMMIT").unwrap();
    }
    events.sort_unstable();

    let mut known: Vec<usize> = Vec::new();
    let (mut covered, mut uncovered, mut spent) = (0_i64, 0_i64, 0_i64);
    for (_, kind, index) in events {
        match kind {
            1 => known.push(index),
            2 => buckets[index].revoked = true,
            _ => {
                let (at, amount) = spends[index];
                spent += amount;
                let mut rest = amount;
                let mut usable: Vec<usize> = known
                    .iter()
                    .copied()
                    .filter(|i| {
                        !buckets[*i].revoked && buckets[*i].from <= at && at < buckets[*i].until
                    })
                    .collect();
                usable.sort_by_key(|i| (buckets[*i].until, *i));
                for i in usable {
                    let take = rest.min(buckets[i].left);
                    buckets[i].left -= take;
                    rest -= take;
                    covered += take;
                }
                uncovered += rest;
            }
        }
    }
    let allowance_left = buckets
        .iter()
        .filter(|bucket| !bucket.revoked && bucket.from <= now && now < bucket.until)
        .map(|bucket| bucket.left)
        .sum();
    Books {
        allowance_left,
        covered,
        uncovered,
        funds,
        funds_left: (funds - uncovered).max(0),
        credit_debt: (uncovered - funds).max(0),
        spent,
    }
}

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
    if i % 5 == 0 {
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
            let charge = if i % 2 == 0 { 100 } else { 40 };
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
// 随机历史：视图、独立模型、准入决定、告警四方对得上
// ------------------------------------------------------------------------------------------

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
    }
}

fn run_random_history(seed: u64, steps: usize, initial_funds: i64) -> Reached {
    const CREDIT: i64 = 200;
    let db = TempDb::new(&format!("funded-random-{seed}-{initial_funds}"));
    let store = credit_world(&db);
    credit_line(&store, CREDIT, T0);
    if initial_funds > 0 {
        top_up(&store, initial_funds, "init", T0);
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut now = T0;
    let mut allowances: Vec<String> = Vec::new();
    let mut revoked: Vec<String> = Vec::new();
    let mut open: Vec<Req> = Vec::new();
    let mut next_request = 0_u32;
    let mut next_top_up = 0_u32;
    let mut reached = Reached::default();
    for step in 0..steps {
        now += 1_000 * (1 + rng.below(4));
        match rng.below(12) {
            0 | 1 => {
                // 新额度：起点在 [now - 2 秒, now + 3 秒]，长度 5 ~ 44 秒，额度 50 / 100 / 150 / 250
                let from = now + 1_000 * (rng.below(6) - 2);
                let until = from + 1_000 * (5 + rng.below(40));
                let cap = [50_i64, 100, 150, 250][usize::try_from(rng.below(4)).unwrap()];
                let id = format!("rnd-{seed}-{step}");
                prepaid(&store, &id, cap, from, until, now);
                allowances.push(id);
            }
            2 => {
                if let Some(id) = allowances.iter().find(|id| !revoked.contains(id)).cloned() {
                    if model(db.path(), "emp", now).covered > 0 {
                        reached.revoked_after_cover += 1;
                    }
                    revoke_allocation_at(&db, &id, now);
                    revoked.push(id);
                }
            }
            3 => {
                // 收尾一个在途的预留：释放，或者按一半结算
                if let Some(req) = open.pop() {
                    let reserved = store
                        .hold(&req.attempt, "model")
                        .unwrap()
                        .unwrap()
                        .reserved
                        .micros;
                    if rng.below(2) == 0 {
                        store.release_attempt(&req.attempt, ts(now)).unwrap();
                        reached.released += 1;
                    } else {
                        store
                            .settle_attempt(&req.attempt, &price(reserved / 2), ts(now))
                            .unwrap();
                        reached.half_settled += 1;
                    }
                }
            }
            4 => {
                let amount = [30_i64, 50, 100, 250][usize::try_from(rng.below(4)).unwrap()];
                if model(db.path(), "emp", now).credit_debt > 0 {
                    reached.top_up_with_debt += 1;
                }
                next_top_up += 1;
                top_up(&store, amount, &format!("rnd-{seed}-{next_top_up}"), now);
            }
            _ => {
                next_request += 1;
                let max_tokens = [10_u64, 20, 45, 100][usize::try_from(rng.below(4)).unwrap()];
                let reserve = i64::try_from(10 + 2 * max_tokens).unwrap();
                let before = model(db.path(), "emp", now);
                let want = expected_price(&before, CREDIT, reserve);
                let alerts_before = credit_used_amounts(&store).len();
                let req = Req::new(
                    &store,
                    &format!("q-{seed}-{next_request}"),
                    0,
                    "cred-e",
                    max_tokens,
                )
                .at(now);
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
                    if rng.below(4) == 0 {
                        open.push(req); // 留着在途，之后的某一步再收尾
                    } else {
                        store
                            .settle_attempt(&req.attempt, &price(charged), ts(now + 500))
                            .unwrap();
                    }
                } else {
                    assert_eq!(denied(&decision), Some(ReasonCode::BudgetExhausted));
                }
            }
        }
        // 每一步之后：视图对得上模型，授信占用不超过授信额度
        let books = check_books(&store, &db, "emp", now + 700);
        assert!(
            books.credit_debt <= CREDIT,
            "seed {seed} step {step}: credit use beyond the line: {books:?}"
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
fn random_histories_with_top_ups_agree_with_the_independent_model_decisions_and_alerts() {
    let mut total = Reached::default();
    for seed in [1_u64, 2, 3, 4, 5, 6] {
        total.add(run_random_history(seed, 50, 0));
    }
    for seed in [21_u64, 22, 23, 24, 25, 26] {
        total.add(run_random_history(seed, 50, 100));
    }
    eprintln!("funded random histories reached: {total:?}");
    // 没有走到这些状态，上面「四方对得上」就没有说服力
    assert!(total.allowed >= 60, "{total:?}");
    assert!(total.degraded >= 5, "{total:?}");
    assert!(total.rejected >= 15, "{total:?}");
    assert!(total.allowance_and_debt >= 15, "{total:?}");
    assert!(total.allowance_and_funds >= 15, "{total:?}");
    assert!(total.top_up_with_debt >= 3, "{total:?}");
    assert!(total.credit_full >= 15, "{total:?}");
    assert!(total.covered_while_owing >= 5, "{total:?}");
    assert!(total.alerted >= 15, "{total:?}");
    assert!(total.revoked_after_cover >= 1, "{total:?}");
    assert!(total.released >= 3, "{total:?}");
    assert!(total.half_settled >= 3, "{total:?}");
}
