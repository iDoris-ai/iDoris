//! 账本投影的耗时（Codex 复审 7c5a03fe，Medium）。
//!
//! 准入和结算在**写事务**里读账户的投影：有过授信的账户要把它**全期**的花费回放一遍（`project.rs` 的 `separate_books`：读全部花费、
//! 全部额度，一笔一笔找覆盖它的额度），账户的历史越长，写锁占得越久。这个文件造一个长历史（十万笔已结算的花费、120 笔月度额度），
//! 量一次投影要多久，并核对读出来的账和手算的一致（回放变快不能变错）：
//!
//! * `a_long_history_projects_correctly_and_quickly`：2 万笔 × 120 笔额度，每次 `cargo test` 都跑，核对账 + 宽松的耗时上限（防止回到
//!   「每笔花费扫一遍全部额度」的量级以上，不是基准）；
//! * `projecting_100k_spends_against_120_allowances`：10 万笔 × 120 笔额度，`#[ignore]`，手动跑并把耗时打出来：
//!   `cargo test -p idoris-tenancy --release --test ledger_projection_perf -- --ignored --nocapture`。
//!
//! 数据用 SQL 批量写（和账本自己写的分录同一个形状：每个预留在付款账户链上每个账户一条 `reserve`、一条 `settle`），
//! 数字都能手算：额度 `b-i` 的窗口是第 i 个 30 天，容量 1 000 万；第 j 笔花费 100，`j` 是 10 的倍数的在第一个窗口之前
//! （没有额度覆盖，授信债务），其余按 `j × 步长` 均匀落在 120 个窗口里（每个窗口 800 多笔，远没用满容量，全部被额度覆盖）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::time::{Duration, Instant};

use fixture::*;
use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::CreditLineSetting;

/// 第一个额度窗口的起点。
const START: i64 = T0;
const WINDOW_MS: i64 = 30 * DAY;
const AMOUNT: i64 = 100;
const CAP: i64 = 10_000_000;
const CREDIT: i64 = 5_000_000;

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

/// 第 `j` 笔花费的归属时刻：`j` 是 10 的倍数的在第一个窗口之前（没有额度覆盖），其余均匀落在 `buckets` 个窗口里。
fn spend_at(j: i64, spends: i64, buckets: i64) -> i64 {
    if j % 10 == 0 {
        START - 1 - j
    } else {
        START + j * (buckets * WINDOW_MS / spends)
    }
}

/// 一个有 `buckets` 笔月度额度、`spends` 笔已结算花费的 `emp`。返回（账户库，读的时刻，手算的 `ceiling_remaining`）。
///
/// 手算：读的时刻取最后一个窗口里（第 `buckets - 1` 个窗口的起点 + 1 天），只有最后一个窗口的额度此刻可花；
/// 它的剩余 = 容量 - 落在这个窗口里的花费 × 100；授信债务 = 没被额度覆盖的花费（`j` 是 10 的倍数的那些）× 100（没有充值），
/// 小于授信额度，所以净头寸 = 额度剩余 - 债务。
fn build(db: &TempDb, spends: i64, buckets: i64) -> (AccountStore, i64, i64) {
    let store = world(db);
    let end = START + buckets * WINDOW_MS + DAY;
    store
        .fund_account(&aid("paid"), &thb(1_000_000_000_000), &key("f-paid"), t0())
        .unwrap();
    store
        .create_allocation(
            &Allocation {
                id: AllocationId::new("e-0").unwrap(),
                parent: aid("platform"),
                child: aid("paid"),
                ceiling: grant(1_000_000_000_000, Purpose::Prepaid, START - DAY, end),
                idempotency_key: None,
            },
            ts(START),
        )
        .unwrap();
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(CREDIT),
                valid_until: None,
            },
            ts(START),
        )
        .unwrap();
    for i in 0..buckets {
        let from = START + i * WINDOW_MS;
        store
            .create_allocation(
                &Allocation {
                    id: AllocationId::new(format!("b-{i}")).unwrap(),
                    parent: aid("paid"),
                    child: aid("emp"),
                    ceiling: grant(CAP, Purpose::Prepaid, from, from + WINDOW_MS),
                    idempotency_key: None,
                },
                ts(START),
            )
            .unwrap();
    }

    // 批量写预留和分录（一个事务，触发器照常跑）
    let step = buckets * WINDOW_MS / spends;
    let at_sql = format!("CASE WHEN i % 10 = 0 THEN {START} - 1 - i ELSE {START} + i * {step} END");
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.busy_timeout(Duration::from_secs(30)).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    conn.execute(
        &format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < {spends})
             INSERT INTO ledger_holds
                (hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id, unit,
                 reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros, state)
             SELECT 'p' || i || '#0', 'p' || i || '#0', 'model', 'p' || i, 0, 'emp', 'cred-e', 'THB',
                    {AMOUNT}, '2026-11', {at_sql}, {at_sql} + 1800000, NULL, 'open'
               FROM n"
        ),
        [],
    )
    .unwrap();
    for (kind, held, spent, delay) in [
        ("reserve", AMOUNT, 0, 0_i64),
        ("settle", -AMOUNT, AMOUNT, 500),
    ] {
        conn.execute(
            &format!(
                "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < {spends}),
                      acct(a) AS (VALUES ('emp'), ('paid'), ('platform'))
                 INSERT INTO ledger_entries
                    (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
                     window_at_ms, at_ms, idempotency_key)
                 SELECT a, 'p' || i || '#0', '{kind}', 'THB', {held}, {spent}, 0, 0,
                        {at_sql}, {at_sql} + {delay}, 'p' || i || '#0|{kind}|' || a
                   FROM n, acct"
            ),
            [],
        )
        .unwrap();
    }
    conn.execute(
        "UPDATE ledger_holds SET state = 'settled', closed_at_ms = created_at_ms + 500, charged_micros = reserved_micros",
        [],
    )
    .unwrap();
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);

    let now = START + (buckets - 1) * WINDOW_MS + DAY;
    let last_window_from = START + (buckets - 1) * WINDOW_MS;
    let in_last_window = (0..spends)
        .filter(|j| j % 10 != 0 && spend_at(*j, spends, buckets) >= last_window_from)
        .count() as i64;
    let uncovered = (0..spends).filter(|j| j % 10 == 0).count() as i64;
    let expected = (CAP - in_last_window * AMOUNT) - uncovered * AMOUNT;
    (store, now, expected)
}

/// 量一次账户投影（取 `rounds` 次里最短的），顺带核对读出来的账。
fn project_once(store: &AccountStore, now: i64, expected: i64, rounds: usize) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..rounds {
        let started = Instant::now();
        let view = store.account_view(&aid("emp"), ts(now)).unwrap();
        best = best.min(started.elapsed());
        assert_eq!(
            view.ceiling_remaining,
            Some(thb(expected)),
            "the long history reads as the hand-computed books"
        );
        assert_eq!(view.credit_limit, thb(CREDIT));
        assert!(view.balance.is_none());
    }
    best
}

#[test]
fn a_long_history_projects_correctly_and_quickly() {
    let db = TempDb::new("perf-small");
    let started = Instant::now();
    let (store, now, expected) = build(&db, 20_000, 120);
    let build_time = started.elapsed();
    let best = project_once(&store, now, expected, 3);
    eprintln!("20k spends x 120 allowances: build {build_time:?}, one projection {best:?}");
    // 宽松的上限：只防止退回到「每笔花费扫一遍全部额度」再叠加别的 O(n^2)；真正的数字见忽略的那一条
    assert!(
        best < Duration::from_secs(5),
        "one projection of a 20k-spend history took {best:?}"
    );
    assert_clean(&store);
}

#[test]
#[ignore = "perf: 100k spends, run by hand with --ignored --nocapture (see the module docs)"]
fn projecting_100k_spends_against_120_allowances() {
    let db = TempDb::new("perf-100k");
    let started = Instant::now();
    let (store, now, expected) = build(&db, 100_000, 120);
    let build_time = started.elapsed();
    let best = project_once(&store, now, expected, 5);
    eprintln!(
        "PERF 100k spends x 120 allowances: build {build_time:?}, one account projection (best of 5) {best:?}"
    );

    // 整条准入：付款账户链上三个账户各投影一遍，再写预留（写事务里）
    let req = Req::new(&store, "perf-new", 0, "cred-e", 45).at(now);
    let admit_started = Instant::now();
    let decision = store.admit_attempt(&req.input()).unwrap();
    let admit_time = admit_started.elapsed();
    assert!(decision.is_admitted(), "{decision:?}");
    let settle_started = Instant::now();
    store
        .settle_attempt(&req.attempt, &price(100), ts(now + 500))
        .unwrap();
    let settle_time = settle_started.elapsed();
    eprintln!("PERF 100k: one admission {admit_time:?}, one settlement {settle_time:?}");
    assert!(
        best < Duration::from_secs(10),
        "one projection of a 100k-spend history took {best:?}"
    );
}
