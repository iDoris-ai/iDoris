//! S3 准入账本的并发：并发下多扣或少扣都是真金白银的错账，所以这是最重要的一组测试。
//!
//! 手法（沿用 `budget_concurrency.rs` / `account_store_concurrency.rs`，并按 S1 的经验收紧）：
//!
//! - **真实的 SQLite 文件、真实并发**：每个线程各开一条自己的连接到同一个库文件，靠的是 SQLite 的写锁
//!   （`BEGIN IMMEDIATE`），不是进程内的互斥；线程在屏障上同时放行；
//! - **让所有预留同时在途**：整个竞争阶段没有任何结算和释放，所以每一个被放行的预留都还占着额度——
//!   只有这样，漏算在途的 bug（检查时没把别人还没结算的预留算进去）才会被确定性地抓住；
//! - **「恰好 K 个成功」**，不是「大概不超过 K 个」：容量精确地定在 K 个预留，每一个都是整数倍，所以
//!   成功数、拒绝数、拒绝原因都是确定的，与线程调度无关；
//! - **采样者线程**：竞争期间另一条连接反复读整条链的投影（每次一个一致的只读快照），断言**任何时刻**
//!   每个祖先的已用 + 在途都不超过限额、补贴池不超过总上限、速率和在途上限生效。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`max_tokens = 100` 的预留是 210 微泰铢。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use fixture::*;
use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, Membership, MembershipRole, MembershipStatus,
};
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::credential::{Credential, CredentialBinding, CredentialStatus};
use idoris_compute_contracts::ids::IdempotencyKey;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{
    AccountLimits, FreeTierIssue, FreeTierOutcome, LedgerError, SettleResult,
};

const THREADS: usize = 24;
const R: i64 = 210; // max_tokens = 100 的预留

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

/// 让每个请求各自在一个线程里（各开一条连接）同时放行，返回各自的准入决定。
/// 竞争期间一个采样者线程反复读 `watch` 里每个账户链的投影，对每个快照调用 `check`。
fn race_admissions(
    db: &TempDb,
    requests: Vec<Req>,
    watch: &'static [&'static str],
    check: fn(&str, &idoris_compute_contracts::admission::AncestorView),
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
            let store = AccountStore::open(&path).unwrap();
            barrier.wait();
            loop {
                for leaf in watch {
                    let views = store.chain_views(&aid(leaf), t0()).unwrap();
                    for view in &views {
                        check(view.account.as_str(), view);
                    }
                    samples.fetch_add(1, Ordering::Relaxed);
                }
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
    assert!(
        samples.load(Ordering::Relaxed) > 0,
        "the sampler never observed anything"
    );
    decisions
}

fn admitted(decisions: &[AdmissionDecision]) -> usize {
    decisions.iter().filter(|d| d.is_admitted()).count()
}

fn reject_codes(decisions: &[AdmissionDecision]) -> Vec<ReasonCode> {
    decisions
        .iter()
        .filter_map(|d| d.reason_code().filter(|c| !c.is_served()))
        .collect()
}

/// 任何时刻、任何祖先：额度剩余不为负，在途和速率窗口不超过上限，补贴池不超支。
fn never_over_the_limit(account: &str, view: &idoris_compute_contracts::admission::AncestorView) {
    if let Some(remaining) = &view.ceiling_remaining {
        assert!(
            remaining.micros >= 0,
            "{account}: ceiling remaining went negative: {remaining:?}"
        );
    }
    if let Some(balance) = &view.balance {
        assert!(
            balance.micros >= 0,
            "{account}: balance went negative without a credit line: {balance:?}"
        );
    }
    if let Some(pool) = &view.subsidy_pool_remaining {
        assert!(
            pool.micros >= 0,
            "{account}: the subsidy pool is over its cap: {pool:?}"
        );
    }
    for window in &view.rate_windows {
        assert!(
            window.spent.micros <= window.cap.micros,
            "{account}: rate window over its cap: {window:?}"
        );
    }
    if let Some(flight) = &view.in_flight {
        assert!(
            flight.current.micros <= flight.cap.micros,
            "{account}: in-flight over its cap: {flight:?}"
        );
    }
}

const LEAVES: &[&str] = &["kid-a", "kid-b", "kid-c"];

/// 树：pool -> school -> {teacher -> {kid-a, kid-b}, kid-c}。每一级都铺 `big` 的额度（所以默认谁都不是瓶颈），
/// 补贴池 11 月上限 `pool_cap`。
fn tree(db: &TempDb, big: i64, pool_cap: i64) -> AccountStore {
    let store = world(db);
    allocate_chain(&store, "x", &["pool", "school", "teacher"], big);
    allocate(&store, "a-own", "teacher", "kid-a", big);
    allocate(&store, "b-own", "teacher", "kid-b", big);
    allocate(&store, "c-own", "school", "kid-c", big);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(pool_cap), t0())
        .unwrap();
    store
}

/// `THREADS` 个请求，轮流落在 `credentials` 上，每个预留 210。
fn requests(store: &AccountStore, credentials: &[&str]) -> Vec<Req> {
    (0..THREADS)
        .map(|i| {
            Req::new(
                store,
                &format!("race-{i}"),
                0,
                credentials[i % credentials.len()],
                100,
            )
        })
        .collect()
}

// ------------------------------------------------------------------------------------------
// 五项检查各自在并发下都不超发
// ------------------------------------------------------------------------------------------

#[test]
fn concurrent_admissions_never_exceed_an_ancestors_authorized_ceiling() {
    // 瓶颈在祖先：老师（teacher）的额度 = 5 个预留。16 个线程落在老师下的两个叶子上，叶子自己的额度充足。
    let db = TempDb::new("race-ancestor-ceiling");
    let store = world(&db);
    allocate_chain(&store, "x", &["pool", "school"], 1_000_000_000);
    allocate(&store, "school-teacher", "school", "teacher", 5 * R);
    allocate(&store, "a-own", "teacher", "kid-a", 5 * R);
    allocate(&store, "b-own", "teacher", "kid-b", 5 * R);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(1_000_000_000), t0())
        .unwrap();
    let reqs = (0..16)
        .map(|i| {
            Req::new(
                &store,
                &format!("race-{i}"),
                0,
                ["cred-a", "cred-b"][i % 2],
                100,
            )
        })
        .collect();
    let decisions = race_admissions(&db, reqs, &["kid-a", "kid-b"], never_over_the_limit);
    assert_eq!(admitted(&decisions), 5, "恰好 5 个");
    assert!(
        decisions
            .iter()
            .filter(|d| d.is_admitted())
            .all(|d| matches!(d, AdmissionDecision::Allow { .. }))
    );
    assert_eq!(
        reject_codes(&decisions),
        vec![ReasonCode::BudgetExhausted; 11]
    );
    // 所有预留都还在途：老师的额度被占满，恰好是 0
    assert_eq!(
        store
            .account_view(&aid("teacher"), t0())
            .unwrap()
            .ceiling_remaining,
        Some(thb(0))
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_holds WHERE state = 'open'"
        ),
        5
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_a_single_leafs_allowance() {
    // 瓶颈在叶子本身：kid-a 的额度 = 7 个预留，24 个线程都落在 kid-a 上
    let db = TempDb::new("race-leaf-ceiling");
    let store = world(&db);
    allocate_chain(&store, "x", &["pool", "school", "teacher"], 1_000_000_000);
    allocate(&store, "a-own", "teacher", "kid-a", 7 * R);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(1_000_000_000), t0())
        .unwrap();
    let decisions = race_admissions(
        &db,
        requests(&store, &["cred-a"]),
        &["kid-a"],
        never_over_the_limit,
    );
    assert_eq!(admitted(&decisions), 7);
    assert_eq!(
        reject_codes(&decisions),
        vec![ReasonCode::BudgetExhausted; THREADS - 7]
    );
    assert_eq!(
        store
            .account_view(&aid("kid-a"), t0())
            .unwrap()
            .ceiling_remaining,
        Some(thb(0))
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_an_organizations_funds() {
    // 余额这一条路：组织（paid）的资金 = 6 个预留，职员（emp）自己的资金充足
    let db = TempDb::new("race-funds");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(6 * R), &key("f-paid"), t0())
        .unwrap();
    let decisions = race_admissions(
        &db,
        requests(&store, &["cred-e"]),
        &["emp"],
        never_over_the_limit,
    );
    assert_eq!(admitted(&decisions), 6);
    assert_eq!(
        store.account_view(&aid("paid"), t0()).unwrap().balance,
        Some(thb(0))
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_the_credit_floor() {
    // 授信下限：emp 有 2 个预留的钱，授信再给 3 个预留的额度（可以透支到 -3R）：恰好 5 个放行
    let db = TempDb::new("race-credit");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(2 * R), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    store
        .set_credit_line(
            &aid("emp"),
            &idoris_tenancy::accounts::ledger::CreditLineSetting {
                limit: thb(3 * R),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    // 余额不会降到 -3R 以下：这里放宽 balance >= 0 的检查，改成 >= -3R
    fn floor_check(account: &str, view: &idoris_compute_contracts::admission::AncestorView) {
        if let Some(balance) = &view.balance {
            let floor = -view.credit_limit.micros;
            assert!(
                balance.micros >= floor,
                "{account}: balance {balance:?} below the credit floor {floor}"
            );
        }
    }
    let decisions = race_admissions(&db, requests(&store, &["cred-e"]), &["emp"], floor_check);
    assert_eq!(admitted(&decisions), 5);
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(-3 * R))
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_the_rate_gate() {
    // 速率闸门配在学校上：这一分钟的上限 = 6 个预留，三个叶子共用。同一个 now，所以都在同一个窗口里
    let db = TempDb::new("race-rate");
    let store = tree(&db, 1_000_000_000, 1_000_000_000);
    store
        .set_account_limits(
            &aid("school"),
            &AccountLimits {
                minute_cap: Some(thb(6 * R)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let decisions = race_admissions(
        &db,
        requests(&store, &["cred-a", "cred-b", "cred-c"]),
        LEAVES,
        never_over_the_limit,
    );
    assert_eq!(admitted(&decisions), 6);
    assert_eq!(
        reject_codes(&decisions),
        vec![ReasonCode::RateLimited; THREADS - 6]
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_the_in_flight_cap() {
    // 在途总额上限 = 4 个预留（学校），所有预留都还在途
    let db = TempDb::new("race-inflight");
    let store = tree(&db, 1_000_000_000, 1_000_000_000);
    store
        .set_account_limits(
            &aid("school"),
            &AccountLimits {
                inflight_cap: Some(thb(4 * R)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let decisions = race_admissions(
        &db,
        requests(&store, &["cred-a", "cred-b", "cred-c"]),
        LEAVES,
        never_over_the_limit,
    );
    assert_eq!(admitted(&decisions), 4);
    assert_eq!(
        reject_codes(&decisions),
        vec![ReasonCode::InFlightLimited; THREADS - 4]
    );
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_never_exceed_the_subsidy_pool_cap() {
    // 补贴池 11 月上限 = 9 个预留，三个叶子（挂在不同的中间账户下）共用；其余每一项都充足
    let db = TempDb::new("race-pool");
    let store = tree(&db, 1_000_000_000, 9 * R);
    let decisions = race_admissions(
        &db,
        requests(&store, &["cred-a", "cred-b", "cred-c"]),
        LEAVES,
        never_over_the_limit,
    );
    assert_eq!(admitted(&decisions), 9);
    assert_eq!(
        reject_codes(&decisions),
        vec![ReasonCode::SubsidyPoolExhausted; THREADS - 9]
    );
    let report = store.pool_report(&aid("pool"), month(2026, 11)).unwrap();
    assert_eq!(report.consumed, thb(9 * R));
    assert_clean(&store);
}

#[test]
fn concurrent_admissions_with_several_binding_limits_keep_every_one_of_them() {
    // 同时有多个瓶颈：老师额度 8R、学校速率 6R、池 5R：最紧的是池，恰好 5 个；任何时刻没有一个超
    let db = TempDb::new("race-mixed");
    let store = world(&db);
    allocate(&store, "plat-pool", "platform", "pool", 1_000_000_000);
    allocate(&store, "pool-school", "pool", "school", 1_000_000_000);
    allocate(&store, "school-teacher", "school", "teacher", 8 * R);
    allocate(&store, "a-own", "teacher", "kid-a", 8 * R);
    allocate(&store, "b-own", "teacher", "kid-b", 8 * R);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(5 * R), t0())
        .unwrap();
    store
        .set_account_limits(
            &aid("school"),
            &AccountLimits {
                minute_cap: Some(thb(6 * R)),
                inflight_cap: Some(thb(7 * R)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let reqs = (0..THREADS)
        .map(|i| {
            Req::new(
                &store,
                &format!("race-{i}"),
                0,
                ["cred-a", "cred-b"][i % 2],
                100,
            )
        })
        .collect();
    let decisions = race_admissions(&db, reqs, &["kid-a", "kid-b"], never_over_the_limit);
    assert_eq!(admitted(&decisions), 5);
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 结算与释放之后总账守恒
// ------------------------------------------------------------------------------------------

#[test]
fn after_concurrent_settlements_and_releases_the_books_balance_exactly() {
    let db = TempDb::new("race-conserve");
    let store = tree(&db, 1_000_000_000, 1_000_000_000);
    let reqs = requests(&store, &["cred-a", "cred-b", "cred-c"]);
    let decisions = race_admissions(&db, reqs.clone(), LEAVES, never_over_the_limit);
    assert_eq!(admitted(&decisions), THREADS);
    // 现在所有预留同时在途；并发地：偶数号结算（实际金额按序号变化，含小于和大于预留），奇数号释放
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = reqs
        .into_iter()
        .enumerate()
        .map(|(i, request)| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                if i % 2 == 0 {
                    let charge = 50 + (i as i64) * 7; // 50 .. 211
                    store
                        .settle_attempt(&request.attempt, &price(charge), ts(T0 + 1_000))
                        .map(|_| charge)
                } else {
                    store
                        .release_attempt(&request.attempt, ts(T0 + 1_000))
                        .map(|_| 0)
                }
            })
        })
        .collect();
    let charged: i64 = handles
        .into_iter()
        .map(|h| h.join().unwrap().unwrap())
        .sum();
    // 总账：在途全部归零；每个祖先的已花 = 所有结算金额之和；叶子各自只记自己的
    for account in ["platform", "pool", "school"] {
        assert_eq!(
            scalar(
                &db,
                &format!(
                    "SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries WHERE account_id = '{account}'"
                )
            ),
            0,
            "{account}"
        );
        assert_eq!(
            scalar(
                &db,
                &format!(
                    "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = '{account}'"
                )
            ),
            charged,
            "{account}"
        );
    }
    let leaf_spent: i64 =
        ["kid-a", "kid-b", "kid-c"].iter().map(|a| scalar(&db, &format!("SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = '{a}'"))).sum();
    assert_eq!(leaf_spent, charged);
    assert_eq!(
        store
            .pool_report(&aid("pool"), month(2026, 11))
            .unwrap()
            .consumed,
        thb(charged)
    );
    assert_clean(&store);
}

#[test]
fn racing_to_settle_the_same_reservation_charges_it_exactly_once() {
    let db = TempDb::new("race-settle-once");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let request = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(store.admit_attempt(&request.input()).unwrap().is_admitted());
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            let attempt = request.attempt.clone();
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store
                    .settle_attempt(&attempt, &price(150), ts(T0 + 1_000))
                    .unwrap()
                    .result
            })
        })
        .collect();
    let results: Vec<SettleResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == SettleResult::Settled)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == SettleResult::AlreadySettled)
            .count(),
        THREADS - 1
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'"
        ),
        150,
        "只收了一次"
    );
    assert_clean(&store);
}

#[test]
fn racing_settle_against_release_has_exactly_one_winner_and_a_consistent_book() {
    for round in 0..8 {
        let db = TempDb::new(&format!("race-settle-release-{round}"));
        let store = world(&db);
        store
            .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
            .unwrap();
        store
            .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
            .unwrap();
        let request = Req::new(&store, "r-1", 0, "cred-e", 100);
        store.admit_attempt(&request.input()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let settler = {
            let (path, barrier, attempt) = (
                db.path().to_path_buf(),
                Arc::clone(&barrier),
                request.attempt.clone(),
            );
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store.settle_attempt(&attempt, &price(100), ts(T0 + 1_000))
            })
        };
        let releaser = {
            let (path, barrier, attempt) = (
                db.path().to_path_buf(),
                Arc::clone(&barrier),
                request.attempt.clone(),
            );
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store.release_attempt(&attempt, ts(T0 + 1_000))
            })
        };
        let settled = settler.join().unwrap();
        let released = releaser.join().unwrap();
        // 不管谁先：最终状态要么是「收了 100」要么是「没收」，不会两个都成功地让账不平
        let spent = scalar(
            &db,
            "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = 'emp'",
        );
        let held = scalar(
            &db,
            "SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries WHERE account_id = 'emp'",
        );
        assert_eq!(held, 0, "round {round}");
        match (&settled, &released) {
            // 结算先到：释放被拒（预留已经按 100 结算，不能再说「没有钱要收」）
            (
                Ok(_),
                Err(LedgerError::SettleConflict {
                    existing: 100,
                    requested: 0,
                    ..
                }),
            ) => {
                assert_eq!(spent, 100, "round {round}: settle wins");
            }
            // 释放先到：结算被拒（已经宣告没有钱要收）
            (Err(LedgerError::ChargeAfterRelease { .. }), Ok(Some(_))) => {
                assert_eq!(spent, 0, "round {round}: release wins");
            }
            other => panic!("round {round}: exactly one must win, got {other:?}"),
        }
        assert_clean(&store);
    }
}

// ------------------------------------------------------------------------------------------
// 免费额度与宽限在并发下
// ------------------------------------------------------------------------------------------

/// 在 `school` 下再建 `count` 个成员（主体 `px-i`，成员账户 `mx-i`）。
fn extra_members(store: &AccountStore, count: usize) {
    for i in 0..count {
        let account_id = format!("mx-{i}");
        store
            .create_account(
                &Account {
                    id: aid(&account_id),
                    kind: AccountKind::Member,
                    parent: Some(aid("school")),
                    unit: idoris_compute_contracts::money::Unit::Thb,
                    status: AccountStatus::Active,
                },
                t0(),
            )
            .unwrap();
        store
            .ensure_principal(&pid(&format!("px-{i}")), t0())
            .unwrap();
        store
            .create_membership(
                &Membership {
                    id: mid(&format!("mem-x-{i}")),
                    principal: pid(&format!("px-{i}")),
                    org: aid("school"),
                    role: MembershipRole::Member,
                    account: Some(aid(&account_id)),
                    status: MembershipStatus::Active,
                },
                t0(),
            )
            .unwrap();
        store
            .create_credential(
                &Credential::new(
                    cid(&format!("cred-x-{i}")),
                    CredentialBinding::Member {
                        membership: mid(&format!("mem-x-{i}")),
                    },
                    CredentialStatus::Active,
                    None,
                    Vec::new(),
                )
                .unwrap(),
                t0(),
            )
            .unwrap();
    }
}

#[test]
fn racing_identical_free_tier_grants_create_one_allocation_and_replay_the_rest() {
    let db = TempDb::new("race-free-identical");
    let _store = tree(&db, 1_000_000_000, 1_000_000);
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store
                    .issue_free_tier(&FreeTierIssue {
                        principal: &pid("p-a"),
                        member_account: &aid("kid-a"),
                        month: month(2026, 11),
                        amount: thb(1_000),
                        now: t0(),
                    })
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<FreeTierOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted = outcomes
        .iter()
        .filter(|o| matches!(o, FreeTierOutcome::Granted(_)))
        .count();
    assert_eq!((granted, outcomes.len() - granted), (1, THREADS - 1));
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_subsidy_grants"), 1);
    let ids: std::collections::BTreeSet<String> = outcomes
        .iter()
        .map(|o| o.allocation().id.to_string())
        .collect();
    assert_eq!(ids.len(), 1, "每一次拿到的都是同一条额度");
}

#[test]
fn racing_free_tier_grants_never_exceed_the_pool_cap() {
    // 补贴池 11 月上限 = 5 份免费额度；12 个不同的成员同时领
    let db = TempDb::new("race-free-cap");
    let store = tree(&db, 1_000_000_000, 5 * 1_000);
    extra_members(&store, 12);
    let barrier = Arc::new(Barrier::new(12));
    let handles: Vec<_> = (0..12)
        .map(|i| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store.issue_free_tier(&FreeTierIssue {
                    principal: &pid(&format!("px-{i}")),
                    member_account: &aid(&format!("mx-{i}")),
                    month: month(2026, 11),
                    amount: thb(1_000),
                    now: t0(),
                })
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted = results
        .iter()
        .filter(|r| matches!(r, Ok(FreeTierOutcome::Granted(_))))
        .count();
    let capped = results
        .iter()
        .filter(|r| matches!(r, Err(LedgerError::PoolCapExceeded { .. })))
        .count();
    assert_eq!((granted, capped), (5, 7), "{results:?}");
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(amount_micros), 0) FROM ledger_subsidy_grants WHERE period = '2026-11'"
        ),
        5_000
    );
    assert_eq!(
        store
            .pool_report(&aid("pool"), month(2026, 11))
            .unwrap()
            .granted,
        thb(5_000)
    );
}

#[test]
fn racing_grace_requests_give_the_member_exactly_one_grace_per_period() {
    // emp 只剩 20：24 个线程各发一个不同的请求，都要用宽限。恰好一个成功，其余是 402
    let db = TempDb::new("race-grace");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(20), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let reqs: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("g-{i}"), 0, "cred-e", 100).grace())
        .collect();
    let decisions = race_admissions(&db, reqs, &["emp"], |_account, _view| {});
    let graced = decisions
        .iter()
        .filter(|d| matches!(d, AdmissionDecision::Grace { .. }))
        .count();
    assert_eq!(graced, 1);
    assert_eq!(
        reject_codes(&decisions),
        vec![ReasonCode::BudgetExhausted; THREADS - 1]
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_grace_uses WHERE account_id = 'emp' AND voided_at_ms IS NULL"
        ),
        1
    );
    assert_clean(&store);
}

#[test]
fn a_member_cannot_be_admitted_twice_for_the_same_attempt_even_under_contention() {
    let db = TempDb::new("race-same-attempt");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let request = Req::new(&store, "same", 0, "cred-e", 100);
    let reqs: Vec<Req> = (0..THREADS).map(|_| request.clone()).collect();
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = reqs
        .into_iter()
        .map(|r| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store.admit_attempt(&r.input())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let exists = results
        .iter()
        .filter(|r| matches!(r, Err(LedgerError::HoldExists(_))))
        .count();
    assert_eq!((ok, exists), (1, THREADS - 1));
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(1_000_000 - R)),
        "只占了一次"
    );
}
