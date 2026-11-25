//! 账本投影的耗时（Codex 复审 7c5a03fe，Medium）。
//!
//! 准入和结算在**写事务**里读账户的投影：有过授信的账户要把它**全期**的花费回放一遍（`project.rs` 的 `separate_books`：读全部花费、
//! 全部额度，一笔一笔找覆盖它的额度），账户的历史越长，写锁占得越久。这个文件造一个长历史（十万笔已结算的花费、120 笔月度额度），
//! 量一次投影要多久，并核对读出来的账和手算的一致（回放变快不能变错）：
//!
//! * `a_long_history_projects_correctly_and_quickly`：2 万笔 × 120 笔额度，每次 `cargo test` 都跑，核对账 + 宽松的耗时上限（防止回到
//!   「每笔花费扫一遍全部额度」的量级以上，不是基准）；
//! * `admission_latency_with_compacted_history`：每账户 1 千、10 万、20 万条分录与 120 笔额度，`#[ignore]`，手动跑并把耗时打出来：
//!   `cargo test -p idoris-tenancy --release --test ledger_projection_perf -- --ignored --nocapture`。
//!
//! * 补贴池账期（Codex 四复审 Low）：`a_pool_period_reads_only_the_period_and_matches_the_hand_computed_consumption`（每次都跑，核对数）和
//!   `reading_a_pool_period_does_not_grow_with_a_million_historical_entries`（`#[ignore]`，手动跑：一百万条历史分录、这个账期 100 条）。
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
        START + j * ((buckets * WINDOW_MS - 2 * DAY) / spends)
    }
}

/// 一个有 `buckets` 笔月度额度、`spends` 笔已结算花费的 `emp`。返回（账户库，读的时刻，手算的 `ceiling_remaining`）。
///
/// 手算：读的时刻取最后一个窗口里（最后一个窗口结束前 1 天），只有最后一个窗口的额度此刻可花；
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

    // 批量写入已经核对过形状的历史。省掉造数时逐行执行的分录触发器；
    // 在提交之前恢复它们，计时的准入与结算仍经过完整写入路径。
    let step = (buckets * WINDOW_MS - 2 * DAY) / spends;
    let at_sql = format!("CASE WHEN i % 10 = 0 THEN {START} - 1 - i ELSE {START} + i * {step} END");
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.busy_timeout(Duration::from_secs(30)).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    let triggers = remove_entry_triggers(&conn);
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
    for sql in triggers {
        conn.execute_batch(&sql).unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);

    let now = START + buckets * WINDOW_MS - DAY;
    let last_window_from = START + (buckets - 1) * WINDOW_MS;
    let in_last_window = (0..spends)
        .filter(|j| j % 10 != 0 && spend_at(*j, spends, buckets) >= last_window_from)
        .count() as i64;
    let uncovered = (0..spends).filter(|j| j % 10 == 0).count() as i64;
    let expected = (CAP - in_last_window * AMOUNT) - uncovered * AMOUNT;
    (store, now, expected)
}

fn remove_entry_triggers(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'ledger_entries' ORDER BY rowid")
        .unwrap();
    let triggers = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    triggers
        .into_iter()
        .map(|(name, sql)| {
            conn.execute_batch(&format!("DROP TRIGGER {name}")).unwrap();
            sql
        })
        .collect()
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
#[ignore = "perf: 100k entries/account, run by hand with --ignored --nocapture (see the module docs)"]
fn admission_latency_with_compacted_history() {
    let mut medians = Vec::new();
    for spends in [500, 50_000, 100_000] {
        let db = TempDb::new(&format!("perf-{spends}"));
        let (store, now, expected) = build(&db, spends, 120);
        project_once(&store, now, expected, 1);

        // Same fixture as the timed requests. This first write includes building
        // the checkpoint over the existing history.
        let warmup = Req::new(&store, "warmup", 0, "cred-e", 45).at(now);
        let started = Instant::now();
        assert!(store.admit_attempt(&warmup.input()).unwrap().is_admitted());
        let cold = started.elapsed();
        store
            .settle_attempt(&warmup.attempt, &price(0), ts(now))
            .unwrap();
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 3);
        assert_eq!(
            scalar(
                &db,
                "SELECT COUNT(*) FROM ledger_entries e JOIN ledger_checkpoints c
                                USING (account_id) WHERE e.account_id = 'emp'
                                AND e.entry_id <= c.through_entry"
            ),
            2 * spends
        );

        let mut admits = Vec::new();
        let mut settlements = Vec::new();
        for run in 0..7 {
            let req = Req::new(&store, &format!("perf-{run}"), 0, "cred-e", 45).at(now);
            let started = Instant::now();
            assert!(store.admit_attempt(&req.input()).unwrap().is_admitted());
            admits.push(started.elapsed());

            // Overage forces settlement to project the chain inside its write transaction.
            let started = Instant::now();
            let outcome = store
                .settle_attempt(&req.attempt, &price(101), ts(now + 500))
                .unwrap();
            settlements.push(started.elapsed());
            assert_eq!(outcome.sponsor_covered, thb(1));
            assert_eq!(outcome.platform_loss, thb(0));
        }
        project_once(&store, now, expected - 7 * 101, 1);
        admits.sort_unstable();
        settlements.sort_unstable();
        eprintln!(
            "PERF {} entries/account: cold admission {cold:?}, warm admission {:?} [{:?}–{:?}], overage settlement {:?} [{:?}–{:?}] (median of 7)",
            2 * spends,
            admits[3],
            admits[0],
            admits[6],
            settlements[3],
            settlements[0],
            settlements[6]
        );
        medians.push(admits[3]);
    }
    assert!(
        medians[1] < medians[0] * 5 + Duration::from_millis(2),
        "{medians:?}"
    );
}

// ------------------------------------------------------------------------------------------
// 补贴池账期的消耗：只读这个账期的分录（Codex 四复审 Low）
// ------------------------------------------------------------------------------------------

/// 2026-12-01T00:00:00Z：补贴池的账期边界（账本时区默认 UTC）。
const DEC1: i64 = 1_796_083_200_000;
const MINUTE_MS: i64 = 60_000;
const POOL_CAP: i64 = 100_000;

/// 补贴池 `pool` 上的合成历史：`history` 笔花费（每笔 `reserve` + `settle` 各一条分录，共 `2 × history` 条）一分钟一笔，往 12 月 1 日之前排
/// （第 `i` 笔（从 0 数）的预留时刻是 `DEC1 - 60 秒 × (i + 1)`，结算在半秒后），再加这个账期（12 月）的 `this_month` 笔（`DEC1 + 60 秒 × (j + 1)`），
/// 外加一笔**跨账期的旧格式**结算：预留 100 在 `DEC1 - 1`（11 月），按 160 结算在 `DEC1`（12 月），超出的 60 还整条记在 `settle` 里
/// （v0.5.0 / main 的写法，归属结算时刻）：11 月拿预留之内的 100，12 月拿超出的 60。每笔 100。
///
/// 手算：12 月的消耗 = `this_month × 100 + 60`；11 月的消耗 = `min(history, 43 200) × 100 + 100`（11 月 30 天 = 43 200 分钟，
/// 第 43 200 笔正好落在 11 月 1 日 0 点，账期左闭）。批量写，先拆掉 `ledger_entries` 上的触发器（合成的历史不需要它们，一百万行跑触发器太慢，
/// 这个库只给这个测试用）。
fn build_pool(db: &TempDb, history: i64, this_month: i64) -> AccountStore {
    let store = world(db);
    store
        .set_pool_cap(&aid("pool"), month(2026, 12), &thb(POOL_CAP), t0())
        .unwrap();
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(1_000_000_000), t0())
        .unwrap();
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.busy_timeout(Duration::from_secs(30)).unwrap();
    remove_entry_triggers(&conn);
    conn.execute_batch("BEGIN").unwrap();
    for (prefix, count, at_sql) in [
        ("h", history, format!("{DEC1} - {MINUTE_MS} * (i + 1)")),
        ("m", this_month, format!("{DEC1} + {MINUTE_MS} * (i + 1)")),
    ] {
        for (kind, held, spent, delay) in [
            ("reserve", AMOUNT, 0, 0_i64),
            ("settle", -AMOUNT, AMOUNT, 500),
        ] {
            conn.execute(
                &format!(
                    "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < {count})
                     INSERT INTO ledger_entries
                        (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
                         window_at_ms, at_ms, idempotency_key)
                     SELECT 'pool', '{prefix}' || i || '#0', '{kind}', 'THB', {held}, {spent}, 0, 0,
                            {at_sql}, {at_sql} + {delay}, '{prefix}' || i || '#0|{kind}|pool'
                       FROM n"
                ),
                [],
            )
            .unwrap();
        }
    }
    // 跨账期的旧格式结算：预留 100（`DEC1 - 1`），按 160 结算（`DEC1`），超出的 60 整条记在 `settle` 里
    conn.execute(
        &format!(
            "INSERT INTO ledger_holds
                (hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id, unit,
                 reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros, state)
             VALUES ('strad#0', 'strad#0', 'model', 'strad', 0, 'kid-a', 'cred-a', 'THB',
                     {AMOUNT}, '2026-11', {DEC1} - 1, {DEC1} - 1 + 1800000, NULL, 'open')"
        ),
        [],
    )
    .unwrap();
    conn.execute(
        &format!(
            "UPDATE ledger_holds SET state = 'settled', closed_at_ms = {DEC1}, charged_micros = {} WHERE hold_id = 'strad#0'",
            AMOUNT + 60
        ),
        [],
    )
    .unwrap();
    for (kind, held, spent, at) in [
        ("reserve", AMOUNT, 0, DEC1 - 1),
        ("settle", -AMOUNT, AMOUNT + 60, DEC1),
    ] {
        conn.execute(
            &format!(
                "INSERT INTO ledger_entries
                    (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
                     window_at_ms, at_ms, idempotency_key)
                 VALUES ('pool', 'strad#0', '{kind}', 'THB', {held}, {spent}, 0, 0, {DEC1} - 1, {at}, 'strad#0|{kind}|pool')"
            ),
            [],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    store
}

/// 核对池两个账期的消耗、闸门（视图里的剩余）和手算一致，返回读 12 月消耗（`pool_report`）最快的一次。
fn check_pool_period(
    store: &AccountStore,
    history: i64,
    this_month: i64,
    rounds: usize,
) -> Duration {
    let december = this_month * AMOUNT + 60;
    let november = history.min(43_200) * AMOUNT + AMOUNT;
    let mut best = Duration::MAX;
    for _ in 0..rounds {
        let started = Instant::now();
        let report = store.pool_report(&aid("pool"), month(2026, 12)).unwrap();
        best = best.min(started.elapsed());
        assert_eq!(
            report.consumed,
            thb(december),
            "December: this month's spends plus the 60 of overage settled on December 1st"
        );
        assert_eq!(report.cap, thb(POOL_CAP));
    }
    let report = store.pool_report(&aid("pool"), month(2026, 11)).unwrap();
    assert_eq!(
        report.consumed,
        thb(november),
        "November: its spends plus the 100 inside the reservation of the one settled on December 1st"
    );
    let view = store
        .account_view(&aid("pool"), ts(DEC1 + 12 * HOUR))
        .unwrap();
    assert_eq!(
        view.subsidy_pool_remaining,
        Some(thb(POOL_CAP - december)),
        "the gate and the report say the same number"
    );
    best
}

#[test]
fn a_pool_period_reads_only_the_period_and_matches_the_hand_computed_consumption() {
    // 3 000 笔历史（比 11 月的 43 200 笔少，全在 11 月）+ 12 月 50 笔 + 一笔跨账期的旧格式结算
    let db = TempDb::new("perf-pool-small");
    let store = build_pool(&db, 3_000, 50);
    let best = check_pool_period(&store, 3_000, 50, 3);
    eprintln!("pool period, 6k historical entries + 100 this month: {best:?}");
    // 历史再长，11 月最多 43 200 笔（86 400 条分录）：同一个断言换一份更长的历史（11 月之外还有十个月）
    let db = TempDb::new("perf-pool-long-history");
    let store = build_pool(&db, 60_000, 50);
    check_pool_period(&store, 60_000, 50, 1);
}

#[test]
#[ignore = "perf: 1M historical pool entries, run by hand with --ignored --nocapture (see the module docs)"]
fn reading_a_pool_period_does_not_grow_with_a_million_historical_entries() {
    // 历史分录 1 万 / 10 万 / 100 万条（5 千 / 5 万 / 50 万笔），这个账期 100 条（50 笔）：读 12 月的消耗应该不随历史变长。
    // 对照：原来的短语句（把池账户的全部历史读一遍）的耗时，在同一个库上量。
    let mut reads = Vec::new();
    for history in [5_000_i64, 50_000, 500_000] {
        let db = TempDb::new(&format!("perf-pool-{history}"));
        let started = Instant::now();
        let store = build_pool(&db, history, 50);
        let build_time = started.elapsed();
        let best = check_pool_period(&store, history, 50, 5);
        // 对照：原来的写法（全部历史）
        let conn = rusqlite::Connection::open(db.path()).unwrap();
        let mut before = Duration::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            let raw: (i64, i64) = conn
                .query_row(
                    "SELECT COALESCE(SUM(CASE WHEN e.window_at_ms >= ?2 AND e.window_at_ms < ?3 THEN e.held_micros + e.spent_micros ELSE 0 END), 0),
                            COALESCE(SUM(CASE WHEN e.kind = 'settle'
                                               AND ((e.window_at_ms >= ?2 AND e.window_at_ms < ?3) <> (e.at_ms >= ?2 AND e.at_ms < ?3))
                                              THEN 1 ELSE 0 END), 0)
                       FROM ledger_entries e WHERE e.account_id = ?1",
                    rusqlite::params!["pool", DEC1, DEC1 + 31 * DAY],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            before = before.min(started.elapsed());
            assert_eq!(
                raw.1, 1,
                "the one settlement that crosses the period boundary"
            );
        }
        // 池账户整个投影（视图）：`entry_sums` 照旧读这个账户的全部历史（和链上每个账户一样），不在这条修复里
        let view_started = Instant::now();
        store
            .account_view(&aid("pool"), ts(DEC1 + 12 * HOUR))
            .unwrap();
        let view_time = view_started.elapsed();
        eprintln!(
            "PERF pool {} historical entries (+ 100 this month): build {build_time:?}, \
             read December's consumption (best of 5) {best:?}, the old short statement over the whole history {before:?}, \
             one whole pool projection {view_time:?}",
            2 * history
        );
        reads.push(best);
    }
    // 宽松的断言：100 万条历史下读一个账期不会比 1 万条历史下慢十倍以上（原来是线性的：慢约 100 倍）
    let (small, large) = (reads[0], reads[2]);
    assert!(
        large < small * 10 + Duration::from_millis(5),
        "reading a pool period grew with the history: {small:?} -> {large:?}"
    );
}
