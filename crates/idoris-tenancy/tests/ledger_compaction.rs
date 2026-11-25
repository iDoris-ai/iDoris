#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{
    AccountLimits, AlertKind, CreditLineSetting, SettleOutcome,
};
use rusqlite::{Connection, params};

const NOW: i64 = T0 + 2 * HOUR;

fn setup(db: &TempDb, allowance: i64) -> AccountStore {
    let store = world(db);
    store
        .fund_account(
            &aid("paid"),
            &thb(1_000_000),
            &IdempotencyKey::new("paid-funds").unwrap(),
            t0(),
        )
        .unwrap();
    allocate(&store, "paid-prepaid", "platform", "paid", 1_000_000);
    allocate(&store, "emp-prepaid", "paid", "emp", allowance);
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(100_000),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                inflight_cap: Some(thb(100_000)),
                minute_cap: Some(thb(100_000)),
                hour_cap: Some(thb(100_000)),
            },
            t0(),
        )
        .unwrap();
    store
}

fn seed_funds(db: &TempDb, label: &str, at: i64) {
    Connection::open(db.path())
        .unwrap()
        .execute(
            "WITH RECURSIVE n(i) AS (VALUES (1) UNION ALL SELECT i + 1 FROM n WHERE i < 256)
         INSERT INTO ledger_entries
            (account_id, kind, unit, funds_micros, window_at_ms, at_ms, idempotency_key)
         SELECT 'emp', 'fund', 'THB', 1, ?2, ?2, ?1 || i FROM n",
            params![label, at],
        )
        .unwrap();
}

fn admit(store: &AccountStore, name: &str, at: i64, amount: u64) -> Req {
    let req = Req::new(store, name, 0, "cred-e", (amount - 10) / 2).at(at);
    let decision = store.admit_attempt(&req.input()).unwrap();
    assert_eq!(
        decision.admitted().unwrap().reservation.price,
        price(amount.try_into().unwrap())
    );
    req
}

fn settle(store: &AccountStore, req: &Req, amount: i64, at: i64) -> SettleOutcome {
    store
        .settle_attempt(&req.attempt, &price(amount), ts(at))
        .unwrap()
}

fn compact(store: &AccountStore, name: &str, now: i64) {
    let req = admit(store, name, now, 30);
    settle(store, &req, 0, now);
}

fn view(store: &AccountStore, now: i64) -> idoris_compute_contracts::admission::AncestorView {
    store.account_view(&aid("emp"), ts(now)).unwrap()
}

fn net(store: &AccountStore, now: i64) -> i64 {
    view(store, now).ceiling_remaining.unwrap().micros
}

fn through(db: &TempDb) -> i64 {
    scalar(
        db,
        "SELECT through_entry FROM ledger_checkpoints WHERE account_id = 'emp'",
    )
}

fn full_replay(db: &TempDb, store: &AccountStore, times: &[i64]) {
    let copy = TempDb::new("full-replay");
    Connection::open(db.path())
        .unwrap()
        .execute("VACUUM INTO ?1", [copy.path().to_str().unwrap()])
        .unwrap();
    Connection::open(copy.path())
        .unwrap()
        .execute("DELETE FROM ledger_checkpoints", [])
        .unwrap();
    let full = AccountStore::open(copy.path()).unwrap();
    for &now in times {
        assert_eq!(
            store.chain_views(&aid("emp"), ts(now)).unwrap(),
            full.chain_views(&aid("emp"), ts(now)).unwrap(),
            "at {now}"
        );
    }
}

#[test]
fn releasing_an_open_reservation_restores_its_allowance_across_the_checkpoint() {
    let db = TempDb::new("checkpoint-open");
    let store = setup(&db, 160);
    seed_funds(&db, "seed", T0 - DAY);
    let mut config = store.ledger_config().unwrap();
    config.reservation_ttl_ms = 4 * HOUR;
    store.set_ledger_config(&config).unwrap();
    let held = admit(&store, "open", T0, 100);
    compact(&store, "advance", NOW);
    assert_eq!(through(&db), 257);
    assert_eq!(net(&store, NOW), 316);
    assert_eq!(view(&store, NOW).in_flight.unwrap().current, thb(100));

    settle(&store, &held, 0, NOW);
    assert_eq!(net(&store, NOW), 416);
    assert_eq!(view(&store, NOW).in_flight.unwrap().current, thb(0));
    full_replay(&db, &store, &[T0, NOW]);
}

#[test]
fn under_settlement_after_a_newer_hold_started_folds_the_actual_charge_once() {
    let db = TempDb::new("checkpoint-spill");
    let store = setup(&db, 160);
    seed_funds(&db, "seed", T0);
    let first = admit(&store, "first", T0, 100);
    let mut config = store.ledger_config().unwrap();
    config.reservation_ttl_ms = 4 * HOUR;
    store.set_ledger_config(&config).unwrap();
    let blocker = admit(&store, "blocker", T0 + 1, 30);
    config.reservation_ttl_ms = HOUR / 2;
    store.set_ledger_config(&config).unwrap();
    settle(&store, &first, 40, T0 + 2);
    compact(&store, "advance", NOW);

    let watermark = through(&db);
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_checkpoint_spill WHERE account_id = 'emp'"
        ),
        1
    );
    assert_eq!(net(&store, NOW), 346);
    full_replay(&db, &store, &[T0, NOW]);
    settle(&store, &blocker, 0, NOW);
    seed_funds(&db, "second", T0 - DAY);
    compact(&store, "advance-again", NOW + DAY);
    assert!(through(&db) > watermark);
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_checkpoint_spill WHERE account_id = 'emp'"
        ),
        0
    );
    assert_eq!(net(&store, NOW + DAY), 632);
    full_replay(&db, &store, &[T0, NOW + DAY]);
}

#[test]
fn expiry_reverses_the_reserve_and_late_settlement_uses_the_new_window() {
    let db = TempDb::new("checkpoint-expiry");
    let store = setup(&db, 160);
    seed_funds(&db, "seed", T0 - DAY);
    let req = admit(&store, "expiring", T0, 100);
    compact(&store, "expire", NOW);
    assert_eq!(through(&db), 257);
    assert_eq!(net(&store, NOW), 416);
    assert_eq!(view(&store, NOW).in_flight.unwrap().current, thb(0));

    grant_from(&store, "settlement-window", 80, NOW, NOW + DAY);
    let outcome = settle(&store, &req, 70, NOW + 1);
    assert!(outcome.late);
    assert_eq!(outcome.user_borne, thb(0));
    assert_eq!(net(&store, NOW + 1), 426);
    assert_eq!(view(&store, NOW + 1).rate_windows[0].spent, thb(70));
    full_replay(&db, &store, &[T0, NOW + 1]);
}

fn grant_from(store: &AccountStore, name: &str, amount: i64, from: i64, until: i64) {
    store
        .create_allocation(
            &Allocation {
                id: AllocationId::new(name).unwrap(),
                parent: aid("paid"),
                child: aid("emp"),
                ceiling: grant(amount, Purpose::Prepaid, from, until),
                idempotency_key: None,
            },
            ts(from),
        )
        .unwrap();
}

#[test]
fn old_format_overage_keeps_its_settlement_attribution_in_a_folded_hold() {
    let db = TempDb::new("checkpoint-legacy");
    let store = setup(&db, 160);
    let req = admit(&store, "legacy", T0, 100);
    let at = T0 + MINUTE + 1;
    grant_from(&store, "settlement-window", 80, at, NOW + DAY);
    assert_eq!(settle(&store, &req, 170, at).sponsor_covered, thb(70));
    rewrite_overage_as_v0_5_0(&db, "legacy#0");
    seed_funds(&db, "seed", T0 - DAY);
    compact(&store, "advance", NOW);
    assert!(through(&db) > 257);
    assert_eq!(net(&store, NOW), 326);
    assert_eq!(view(&store, at).rate_windows[0].spent, thb(70));
    assert_eq!(view(&store, at).rate_windows[1].spent, thb(170));
    full_replay(&db, &store, &[T0 - 1, T0, at, NOW]);
}

#[test]
fn folded_allowance_coverage_keeps_funds_available_and_alerts_only_for_credit() {
    let db = TempDb::new("checkpoint-alert");
    let store = setup(&db, 160);
    let req = admit(&store, "history", T0, 100);
    settle(&store, &req, 100, T0 + 1);
    seed_funds(&db, "seed", T0 - DAY);
    let allowance = admit(&store, "allowance", NOW, 50);
    assert!(through(&db) > 257);
    settle(&store, &allowance, 50, NOW + 1);
    let own = admit(&store, "own-funds", NOW + 2, 250);
    settle(&store, &own, 250, NOW + 3);
    assert_eq!(net(&store, NOW + 3), 16);
    assert!(
        store
            .alerts(Some(AlertKind::CreditUsed))
            .unwrap()
            .is_empty()
    );

    admit(&store, "uses-credit", NOW + 4, 30);
    let alerts = store.alerts(Some(AlertKind::CreditUsed)).unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].amount, thb(14));
    assert_eq!(net(&store, NOW + 4), -14);
    full_replay(&db, &store, &[NOW + 4]);
}

#[test]
fn randomized_histories_equal_full_replay_before_and_after_advancing() {
    for seed in 1_u64..=8 {
        let db = TempDb::new("checkpoint-random");
        let store = setup(&db, 900);
        seed_funds(&db, "seed", T0 - DAY);
        let mut rng = seed;
        let mut next = |max| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            i64::try_from(rng % max).unwrap()
        };
        let mut at = T0;
        for i in 0..48 {
            at += next(500) + 1;
            if i % 8 == 0 {
                grant_from(&store, &format!("grant-{i}"), 85, at - MINUTE, at + MINUTE);
            }
            let req = admit(&store, &format!("r{i}"), at, 100);
            match i % 8 {
                0 => {
                    settle(&store, &req, 0, at + 1);
                }
                1 => {
                    settle(&store, &req, 1 + next(99), at + 1);
                }
                2..=4 => {
                    settle(&store, &req, 160, at + 1);
                    let hold = req.attempt.to_string();
                    if i % 8 == 2 {
                        rewrite_overage_as_v0_5_0(&db, &hold);
                    } else if i % 8 == 3 {
                        rewrite_overage_partially(&db, &hold, 30);
                    }
                }
                5 => {
                    at += HOUR;
                    assert!(settle(&store, &req, 1 + next(130), at).late);
                }
                6 => {
                    /* This stays open until the next sweep, then is closed below. */
                    at += HOUR;
                    store.sweep_expired_holds(ts(at)).unwrap();
                    settle(&store, &req, 0, at);
                }
                _ => {
                    settle(&store, &req, 100, at + 1);
                }
            }
        }
        let open = admit(&store, "random-open", at + 1000, 100);
        let watermark = through(&db);
        full_replay(&db, &store, &[T0, T0 + HOUR, at + 1234]);
        settle(&store, &open, 1 + next(99), at + 2000);
        seed_funds(&db, "more", at);
        compact(&store, "advance-random", at + DAY);
        assert!(through(&db) > watermark, "seed {seed}");
        full_replay(&db, &store, &[T0, T0 + HOUR, at + 1234, at + DAY]);
    }
}

#[test]
fn upgrading_a_0005_database_keeps_history_and_builds_a_checkpoint_lazily() {
    let db = TempDb::new("checkpoint-upgrade");
    let store = setup(&db, 160);
    seed_funds(&db, "seed", T0);
    let req = admit(&store, "existing-history", T0, 100);
    settle(&store, &req, 40, T0 + 1);
    let expected = store.chain_views(&aid("emp"), ts(NOW)).unwrap();
    let before = scalar(&db, "SELECT COUNT(*) FROM ledger_entries");
    drop(store);
    // Reconstruct the published 0005 schema with populated records. Strip
    // every object introduced by 0006, including its migration receipt.
    Connection::open(db.path())
        .unwrap()
        .execute_batch(
            "DROP TRIGGER ledger_checkpoints_invalidate;
         DROP TABLE ledger_checkpoints;
         DROP TABLE ledger_checkpoint_spill;
         DROP INDEX ledger_entries_by_account_entry;
         DELETE FROM ledger_schema_migrations WHERE version = 6;",
        )
        .unwrap();
    assert_eq!(
        scalar(&db, "SELECT MAX(version) FROM ledger_schema_migrations"),
        5
    );
    let store = AccountStore::open(db.path()).unwrap();
    assert_eq!(
        scalar(&db, "SELECT MAX(version) FROM ledger_schema_migrations"),
        6
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_entries"), before);
    assert_eq!(store.chain_views(&aid("emp"), ts(NOW)).unwrap(), expected);
    compact(&store, "first-write", NOW);
    assert!(through(&db) > 257);
    assert_eq!(store.chain_views(&aid("emp"), ts(NOW)).unwrap(), expected);
}

#[test]
fn checkpoint_and_spill_roll_back_if_the_admission_write_fails() {
    let db = TempDb::new("checkpoint-rollback");
    let store = setup(&db, 160);
    seed_funds(&db, "seed", T0);
    let first = admit(&store, "first", T0, 100);
    let mut config = store.ledger_config().unwrap();
    config.reservation_ttl_ms = 4 * HOUR;
    store.set_ledger_config(&config).unwrap();
    admit(&store, "blocker", T0 + 1, 30);
    config.reservation_ttl_ms = HOUR / 2;
    store.set_ledger_config(&config).unwrap();
    settle(&store, &first, 40, T0 + 2);

    let conn = Connection::open(db.path()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_admission BEFORE INSERT ON ledger_holds WHEN NEW.hold_id = 'fail#0'
         BEGIN SELECT RAISE(ABORT, 'simulate a failed admission'); END;",
    )
    .unwrap();
    let req = Req::new(&store, "fail", 0, "cred-e", 10).at(NOW);
    assert!(
        store
            .admit_attempt(&req.input())
            .unwrap_err()
            .is_constraint()
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoint_spill"),
        0
    );

    conn.execute_batch("DROP TRIGGER fail_admission").unwrap();
    assert!(store.admit_attempt(&req.input()).unwrap().is_admitted());
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoint_spill"),
        1
    );
    full_replay(&db, &store, &[T0, NOW]);
}

#[test]
fn a_settlement_time_spend_cannot_cross_an_earlier_mutable_spend() {
    for legacy in [false, true] {
        let db = TempDb::new("checkpoint-split-order");
        let store = setup(&db, 160);
        seed_funds(&db, "seed", T0);
        let first = admit(&store, "first", T0, 100);
        let mut config = store.ledger_config().unwrap();
        config.reservation_ttl_ms = 4 * HOUR;
        store.set_ledger_config(&config).unwrap();
        let blocker = admit(&store, "blocker", T0 + 1, 30);
        config.reservation_ttl_ms = HOUR / 2;
        store.set_ledger_config(&config).unwrap();
        grant_from(&store, "settlement-window", 80, T0 + 2, NOW + DAY);
        settle(&store, &first, 170, T0 + 2);
        if legacy {
            rewrite_overage_as_v0_5_0(&db, "first#0");
        }
        compact(&store, "advance", NOW);
        assert_eq!(through(&db), 257, "legacy={legacy}");
        assert_eq!(net(&store, NOW), 296);
        full_replay(&db, &store, &[T0, NOW]);
        settle(&store, &blocker, 0, NOW);
        assert_eq!(net(&store, NOW), 326);
    }
}

// ------------------------------------------------------------------------------------------
// Codex 预审（2 High + 1 Medium）的回归测试，和预审列出的变异式测试
// ------------------------------------------------------------------------------------------

fn emp_range_of(db: &TempDb, hold: &str) -> (i64, i64) {
    let conn = Connection::open(db.path()).unwrap();
    conn.query_row(
        "SELECT MIN(entry_id), MAX(entry_id) FROM ledger_entries WHERE hold_id = ?1 AND account_id = 'emp'",
        [hold],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn folded_entries(db: &TempDb) -> i64 {
    scalar(
        db,
        "SELECT COUNT(*) FROM ledger_entries e JOIN ledger_checkpoints c USING (account_id)
          WHERE e.account_id = 'emp' AND e.entry_id <= c.through_entry",
    )
}

fn set_ttl(store: &AccountStore, ttl: i64) {
    let mut config = store.ledger_config().unwrap();
    config.reservation_ttl_ms = ttl;
    store.set_ledger_config(&config).unwrap();
}

/// 把 0006 引入的对象和迁移回执全部摘掉，还原出已发布的 0005 库。
fn downgrade_to_0005(db: &TempDb) {
    Connection::open(db.path())
        .unwrap()
        .execute_batch(
            "DROP TRIGGER ledger_checkpoints_invalidate;
             DROP TRIGGER ledger_checkpoints_clear_spill;
             DROP TABLE ledger_checkpoints;
             DROP TABLE ledger_checkpoint_spill;
             DROP INDEX ledger_entries_by_account_entry;
             DELETE FROM ledger_schema_migrations WHERE version = 6;",
        )
        .unwrap();
}

/// 已发布的架构允许显式的 0 和负数分录序号。把整库的序号平移，让 `hold` 在 emp 上的 `reserve` 正好是 0。
fn shift_ids_so_that_emp_reserve_is_zero(db: &TempDb, hold: &str) {
    let (first, _) = emp_range_of(db, hold);
    let conn = Connection::open(db.path()).unwrap();
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS ledger_entries_append_only_update;
         DROP TRIGGER IF EXISTS ledger_entries_append_only_delete;",
    )
    .unwrap();
    conn.execute(
        "UPDATE ledger_entries SET entry_id = entry_id - ?1",
        [first],
    )
    .unwrap();
    // 一条分录的序号恰好是 -1 时，BEFORE INSERT 触发器里 NEW.entry_id 的占位值 -1 会和它相撞，库再也写不进去
    // （已发布架构本身的限制，和检查点无关）。挪开，其余的负序号保留。
    conn.execute(
        "UPDATE ledger_entries SET entry_id = -1000 WHERE entry_id = -1",
        [],
    )
    .unwrap();
}

#[test]
fn entries_with_ids_at_or_below_zero_are_replayed_and_folded_after_the_upgrade() {
    let db = TempDb::new("checkpoint-nonpositive-ids");
    let store = setup(&db, 160);
    let req = admit(&store, "low", T0, 100);
    settle(&store, &req, 100, T0 + 1);
    seed_funds(&db, "seed", T0);
    drop(store);
    shift_ids_so_that_emp_reserve_is_zero(&db, "low#0");
    assert_eq!(emp_range_of(&db, "low#0").0, 0);
    assert!(scalar(&db, "SELECT MIN(entry_id) FROM ledger_entries") < 0);
    downgrade_to_0005(&db);

    // 手算：额度 160 花掉 100，剩 60；自有资金 256 没动 = 316。花过的额度不能因为序号 <= 0 被漏掉而回来。
    let store = AccountStore::open(db.path()).unwrap();
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
    assert_eq!(net(&store, NOW), 316);
    full_replay(&db, &store, &[T0, NOW]);

    // 第一次折叠要把序号 <= 0 的分录一起折进去，之后读到的数不变。
    compact(&store, "first-fold", NOW);
    assert!(folded_entries(&db) >= 258);
    assert_eq!(net(&store, NOW), 316);
    full_replay(&db, &store, &[T0, NOW]);
}

fn bulk_released_holds(db: &TempDb, holds: i64) {
    let conn = Connection::open(db.path()).unwrap();
    let triggers: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'ledger_entries'")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    for (name, _) in &triggers {
        conn.execute_batch(&format!("DROP TRIGGER {name}")).unwrap();
    }
    conn.execute(
        &format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < {holds})
             INSERT INTO ledger_holds
                (hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id, unit,
                 reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros, state)
             SELECT 'bulk' || i || '#0', 'bulk' || i || '#0', 'model', 'bulk' || i, 0, 'emp', 'cred-e', 'THB',
                    100, '2026-11', {T0} + i * 50, {T0} + i * 50 + 1800000, NULL, 'open'
               FROM n"
        ),
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE ledger_holds SET state = 'released', closed_at_ms = created_at_ms + 10, charged_micros = 0
          WHERE hold_id LIKE 'bulk%'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ledger_entries
            (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
             window_at_ms, at_ms, idempotency_key)
         SELECT 'emp', hold_id, k.kind, 'THB', k.held, 0, 0, 0, created_at_ms, created_at_ms + k.delay,
                hold_id || '|' || k.kind
           FROM ledger_holds h,
                (SELECT 0 AS o, 'reserve' AS kind, 100 AS held, 0 AS delay
                 UNION ALL SELECT 1, 'release', -100, 10) k
          WHERE h.hold_id LIKE 'bulk%'
          ORDER BY h.created_at_ms, k.o",
        [],
    )
    .unwrap();
    for (_, sql) in triggers {
        conn.execute_batch(&sql).unwrap();
    }
}

#[test]
fn a_cold_account_is_folded_in_bounded_batches_across_requests() {
    let db = TempDb::new("checkpoint-bounded");
    let store = setup(&db, 160);
    seed_funds(&db, "seed", T0 - DAY);
    bulk_released_holds(&db, 1_500);
    let total = scalar(
        &db,
        "SELECT COUNT(*) FROM ledger_entries WHERE account_id = 'emp'",
    );
    assert_eq!(total, 256 + 3_000);
    let at = T0 + DAY;
    let mut previous = 0;
    let mut rounds = 0;
    loop {
        compact(&store, &format!("cold{rounds}"), at);
        rounds += 1;
        let folded = if scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints") == 0 {
            0
        } else {
            folded_entries(&db)
        };
        // 一次请求最多折 1024 条（跨过切点的预留整笔读入，这里每个预留的两条分录相邻，没有越界）。
        assert!(
            folded - previous <= 1024,
            "round {rounds} folded {}",
            folded - previous
        );
        if rounds == 1 {
            assert!(folded > 0, "the first request must make progress");
            full_replay(&db, &store, &[T0, at]);
        }
        if folded == previous || rounds > 20 {
            break;
        }
        previous = folded;
    }
    assert!(rounds >= 3, "{rounds} rounds for {total} entries");
    // 未折叠的尾部不足 256 条时不再推进（摊销写入）。
    assert!(folded_entries(&db) >= total - 255);
    full_replay(&db, &store, &[T0, at]);
    assert_eq!(net(&store, at), 416);
}

fn tail_of(db: &TempDb) -> (i64, i64) {
    (
        scalar(
            db,
            "SELECT through_entry FROM ledger_checkpoints WHERE account_id = 'emp'",
        ),
        scalar(db, "SELECT COUNT(*) FROM ledger_checkpoint_spill"),
    )
}

#[test]
fn an_orphan_expired_hold_beyond_the_horizon_is_folded_and_a_late_charge_replays_it() {
    let db = TempDb::new("checkpoint-orphan");
    let store = setup(&db, 160);
    set_ttl(&store, HOUR / 2);
    seed_funds(&db, "first-", T0 - DAY);
    let orphan = admit(&store, "orphan", T0, 100);
    let expires = T0 + HOUR / 2;
    // 过期、扫掉，但从来没有最终的用量回报。
    assert_eq!(store.sweep_expired_holds(ts(expires)).unwrap(), 1);
    seed_funds(&db, "second-", T0 - DAY);
    let (first, last) = emp_range_of(&db, "orphan#0");

    // 恰好在地平线上（地平线 = now - 1 小时 = 预留的过期时刻）：留在活动端。
    compact(&store, "edge", expires + HOUR);
    assert!(
        through_value(&db) < first,
        "retained exactly at the horizon"
    );
    seed_funds(&db, "third-", T0 - DAY);

    // 越过地平线 1 毫秒：净额核实为零的孤儿折进去，水位越过它。
    compact(&store, "beyond", expires + HOUR + 1);
    assert!(
        through_value(&db) > last,
        "an orphan beyond the horizon is folded"
    );
    let folded = tail_of(&db);
    assert_eq!(net(&store, expires + HOUR + 1), 928);
    full_replay(&db, &store, &[T0, expires, expires + HOUR + 1]);

    // 迟到的收费追加到已折叠的预留上：检查点作废，从这条预留自己的位置重放。
    let outcome = settle(&store, &orphan, 70, expires + 2 * HOUR);
    assert!(outcome.late);
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
    assert!(folded.0 > 0);
    assert_eq!(net(&store, expires + 2 * HOUR), 858);
    full_replay(&db, &store, &[T0, expires, expires + 2 * HOUR]);
}

fn through_value(db: &TempDb) -> i64 {
    through(db)
}

fn setup_allowance_only(db: &TempDb) -> AccountStore {
    let store = world(db);
    store
        .fund_account(
            &aid("paid"),
            &thb(1_000_000),
            &IdempotencyKey::new("paid-funds").unwrap(),
            t0(),
        )
        .unwrap();
    allocate(&store, "paid-prepaid", "platform", "paid", 1_000_000);
    store
}

fn views_at(
    store: &AccountStore,
    times: &[i64],
) -> Vec<Vec<idoris_compute_contracts::admission::AncestorView>> {
    times
        .iter()
        .map(|now| store.chain_views(&aid("emp"), ts(*now)).unwrap())
        .collect()
}

#[test]
fn a_settlement_time_spend_older_than_the_checkpoint_range_keeps_its_own_attribution() {
    let db = TempDb::new("checkpoint-old-late");
    let store = setup(&db, 160);
    seed_funds(&db, "seed-", T0 - DAY);
    let recent = admit(&store, "recent", T0 + 3 * HOUR, 100);
    settle(&store, &recent, 100, T0 + 3 * HOUR + 1);
    // 迟到的时钟：序号在后，归属时刻在 T0。TTL 很长，水位停在它前面。
    set_ttl(&store, 3 * DAY);
    let old = admit(&store, "old", T0, 50);
    set_ttl(&store, HOUR / 2);
    seed_funds(&db, "later-", T0 - DAY);
    compact(&store, "advance", T0 + DAY);
    let (old_first, _) = emp_range_of(&db, "old#0");
    assert!(through(&db) > emp_range_of(&db, "recent#0").1);
    assert!(through(&db) < old_first);

    // 超出预留的 40 归属结算时刻 T0 + 1 小时，早于检查点覆盖的全部时间（T0 + 3 小时起）。
    settle(&store, &old, 90, T0 + HOUR);
    let times = [
        T0,
        T0 + HOUR - 1,
        T0 + HOUR,
        T0 + HOUR + 1,
        T0 + 3 * HOUR,
        T0 + DAY,
    ];
    assert_eq!(view(&store, T0 + HOUR).rate_windows[0].spent, thb(140));
    assert_eq!(view(&store, T0 + HOUR).rate_windows[1].spent, thb(140));
    full_replay(&db, &store, &times);
    let before = views_at(&store, &times);
    compact(&store, "again", T0 + 2 * DAY);
    assert_eq!(views_at(&store, &times), before);
    full_replay(&db, &store, &times);
}

#[test]
fn overlapping_split_settlements_equal_full_replay() {
    for keep in [0_i64, 30] {
        let db = TempDb::new("checkpoint-overlap");
        let store = setup(&db, 160);
        seed_funds(&db, "seed-", T0);
        let a = admit(&store, "a", T0, 100);
        set_ttl(&store, 4 * HOUR);
        let blocker = admit(&store, "blocker", T0 + 1, 30);
        set_ttl(&store, HOUR / 2);
        let b = admit(&store, "b", T0 + 2, 100);
        grant_from(&store, "w-a", 80, T0 + 3, NOW + DAY);
        grant_from(&store, "w-b", 80, T0 + 4, NOW + DAY);
        settle(&store, &a, 170, T0 + 3);
        settle(&store, &b, 160, T0 + 4);
        rewrite_overage_as_v0_5_0(&db, "a#0");
        rewrite_overage_partially(&db, "b#0", keep);
        let times = [T0, T0 + 3, T0 + 4, T0 + 5, NOW];
        let before = views_at(&store, &times);
        compact(&store, "advance", NOW);
        assert_eq!(views_at(&store, &times), before, "keep {keep}");
        full_replay(&db, &store, &times);
        settle(&store, &blocker, 0, NOW);
        compact(&store, "advance-2", NOW + 1);
        full_replay(&db, &store, &times);
    }
}

#[test]
fn an_allowance_whose_start_lies_inside_the_folded_prefix_counts_only_later_spends() {
    let db = TempDb::new("checkpoint-allowance-only");
    let store = setup_allowance_only(&db);
    grant_from(&store, "first-grant", 160, T0, T0 + 20 * MINUTE);
    let early = admit(&store, "early", T0 + MINUTE, 100);
    settle(&store, &early, 100, T0 + MINUTE);
    grant_from(&store, "second-grant", 160, T0 + 10 * MINUTE, T0 + DAY);
    let inside = admit(&store, "inside", T0 + 15 * MINUTE, 100);
    settle(&store, &inside, 100, T0 + 15 * MINUTE);
    for i in 0..130 {
        compact(&store, &format!("filler{i}"), T0 + 16 * MINUTE);
    }
    compact(&store, "advance", NOW);
    assert!(folded_entries(&db) > 256);
    // 读的时刻只有第二笔额度生效，窗口从 T0 + 10 分钟起：只算 15 分钟那笔 100，提前花掉的 100 不算。
    assert_eq!(net(&store, T0 + 30 * MINUTE), 60);
    assert_eq!(net(&store, T0 + 15 * MINUTE), 160 + 160 - 200);
    full_replay(&db, &store, &[T0, T0 + 15 * MINUTE, T0 + 30 * MINUTE, NOW]);
}

#[test]
fn minute_and_hour_windows_cut_folded_consumption_at_the_exact_millisecond() {
    let db = TempDb::new("checkpoint-exact-cutoffs");
    let store = setup(&db, 1000);
    seed_funds(&db, "seed-", T0 - DAY);
    let first = admit(&store, "first", T0, 100);
    settle(&store, &first, 100, T0);
    let second = admit(&store, "second", T0 + 1, 100);
    settle(&store, &second, 100, T0 + 1);
    seed_funds(&db, "later-", T0 - DAY);
    compact(&store, "advance", NOW);
    assert!(folded_entries(&db) >= 256 + 4);
    // 窗口是 (now - 窗口长度, now]：归属时刻 = now - 窗口长度 的已经出去。
    for (now, minute, hour) in [
        (T0 + MINUTE - 1, 200, 200),
        (T0 + MINUTE, 100, 200),
        (T0 + MINUTE + 1, 0, 200),
        (T0 + HOUR - 1, 0, 200),
        (T0 + HOUR, 0, 100),
        (T0 + HOUR + 1, 0, 0),
    ] {
        let v = view(&store, now);
        assert_eq!(v.rate_windows[0].spent, thb(minute), "minute at {now}");
        assert_eq!(v.rate_windows[1].spent, thb(hour), "hour at {now}");
    }
    full_replay(
        &db,
        &store,
        &[
            T0 + MINUTE - 1,
            T0 + MINUTE,
            T0 + MINUTE + 1,
            T0 + HOUR - 1,
            T0 + HOUR,
            T0 + HOUR + 1,
        ],
    );
}

#[test]
fn a_v0_5_0_partial_split_hold_crossing_the_checkpoint_boundary_reads_as_hand_computed() {
    let db = TempDb::new("checkpoint-partial-split");
    let store = setup(&db, 160);
    seed_funds(&db, "seed-", T0);
    let first = admit(&store, "first", T0, 100);
    set_ttl(&store, 4 * HOUR);
    let blocker = admit(&store, "blocker", T0 + 1, 30);
    set_ttl(&store, HOUR / 2);
    grant_from(&store, "settlement-window", 80, T0 + 2, NOW + DAY);
    settle(&store, &first, 170, T0 + 2);
    // 一半的超出部分（40）还留在 settle 里，另一半（30）已另记 late_settle。
    rewrite_overage_partially(&db, "first#0", 30);
    compact(&store, "advance", NOW);
    assert_eq!(through(&db), 257);
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_checkpoint_spill WHERE account_id = 'emp'"
        ),
        0
    );
    // 手算：预留 100 + 超出 70 + 阻塞 30。额度 160 先盖 100 和阻塞的 30，剩 30；
    // 超出的 70 归属结算时刻，由 [T0+2, ..) 的额度 80 盖住，剩 10；自有资金 256。30 + 10 + 256 = 296。
    assert_eq!(net(&store, NOW), 296);
    full_replay(&db, &store, &[T0, T0 + 2, NOW]);
    settle(&store, &blocker, 0, NOW);
    assert_eq!(net(&store, NOW), 326);
}

fn late_settle_orphan_with_the_0005_binary(db: &TempDb, hold: &str, at: i64) {
    // The 0005 binary knows nothing about checkpoints: it appends the settlement entries
    // and flips the hold, nothing else.
    let conn = Connection::open(db.path()).unwrap();
    conn.execute(
        "UPDATE ledger_holds SET state = 'settled', closed_at_ms = ?2, charged_micros = 70, late = 1
          WHERE hold_id = ?1",
        params![hold, at],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ledger_entries
            (account_id, hold_id, kind, unit, spent_micros, window_at_ms, at_ms, idempotency_key)
         VALUES ('emp', ?1, 'late_settle', 'THB', 70, ?2, ?2, ?1 || '|late|emp')",
        params![hold, at],
    )
    .unwrap();
}

#[test]
fn writes_by_the_0005_binary_between_upgrades_survive_a_downgrade_round_trip() {
    for stripped in [false, true] {
        let db = TempDb::new("checkpoint-round-trip");
        let store = setup(&db, 160);
        set_ttl(&store, HOUR / 2);
        seed_funds(&db, "first-", T0 - DAY);
        admit(&store, "orphan", T0, 100);
        store.sweep_expired_holds(ts(T0 + HOUR / 2)).unwrap();
        seed_funds(&db, "second-", T0 - DAY);
        compact(&store, "fold", T0 + 2 * HOUR);
        assert!(
            through(&db) > emp_range_of(&db, "orphan#0").1,
            "the orphan is folded"
        );
        assert_eq!(net(&store, T0 + 2 * HOUR), 160 + 512);
        drop(store);

        if stripped {
            downgrade_to_0005(&db);
        }
        late_settle_orphan_with_the_0005_binary(&db, "orphan#0", T0 + 3 * HOUR);
        if !stripped {
            // 触发器在库里，老二进制的写入也会让它作废。
            assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
        }

        let store = AccountStore::open(db.path()).unwrap();
        assert_eq!(
            net(&store, T0 + 3 * HOUR),
            160 + 512 - 70,
            "stripped={stripped}"
        );
        full_replay(&db, &store, &[T0, T0 + 2 * HOUR, T0 + 3 * HOUR]);
        compact(&store, "refold", T0 + 2 * DAY);
        assert!(through(&db) >= emp_range_of(&db, "orphan#0").1);
        assert_eq!(net(&store, T0 + 3 * HOUR), 160 + 512 - 70);
        full_replay(&db, &store, &[T0, T0 + 2 * HOUR, T0 + 3 * HOUR]);
    }
}

#[test]
fn a_failed_settlement_rolls_back_the_checkpoint_advance_made_inside_it() {
    let db = TempDb::new("checkpoint-settle-rollback");
    let store = setup(&db, 160);
    // The history is still inside the horizon when the hold is admitted, so
    // admission leaves the checkpoint alone; the settlement three hours later
    // is what folds it.
    let recent = T0 + DAY;
    seed_funds(&db, "seed-", recent - 10_000);
    set_ttl(&store, 4 * HOUR);
    let held = admit(&store, "held", recent, 100);
    set_ttl(&store, HOUR / 2);
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
    let settle_at = recent + 3 * HOUR;

    let conn = Connection::open(db.path()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_settlement BEFORE INSERT ON ledger_entries
         WHEN NEW.kind = 'late_settle' AND NEW.hold_id = 'held#0'
         BEGIN SELECT RAISE(ABORT, 'simulate a failed settlement'); END;",
    )
    .unwrap();
    let outcome = store.settle_attempt(&held.attempt, &price(170), ts(settle_at));
    assert!(outcome.unwrap_err().is_constraint());
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoints"), 0);
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM ledger_checkpoint_spill"),
        0
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_entries WHERE kind = 'settle'"
        ),
        0
    );

    conn.execute_batch("DROP TRIGGER fail_settlement").unwrap();
    settle(&store, &held, 170, settle_at);
    assert!(
        through(&db) >= 256,
        "the same settlement folds the history when it commits"
    );
    full_replay(&db, &store, &[T0, recent, settle_at]);
}

#[test]
fn concurrent_admissions_on_one_account_build_and_advance_the_checkpoint_correctly() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let db = TempDb::new("checkpoint-concurrent");
    let store = setup(&db, 100_000);
    let mut previous: Vec<(Req, i64)> = Vec::new();
    for wave in 1..=2_i64 {
        // The previous wave's holds are closed and past the horizon by now.
        let at = NOW + wave * 3 * HOUR;
        for (req, when) in previous.drain(..) {
            store
                .settle_attempt(&req.attempt, &price(0), ts(when))
                .unwrap();
        }
        seed_funds(&db, &format!("w{wave}-"), T0 - DAY);
        let requests: Vec<Req> = (0..8)
            .map(|i| Req::new(&store, &format!("race{wave}-{i}"), 0, "cred-e", 10).at(at))
            .collect();
        previous = requests.iter().map(|req| (req.clone(), at)).collect();
        let barrier = Arc::new(Barrier::new(requests.len()));
        let handles: Vec<_> = requests
            .into_iter()
            .map(|req| {
                let path = db.path().to_path_buf();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let store = AccountStore::open(&path).unwrap();
                    barrier.wait();
                    store.admit_attempt(&req.input()).unwrap().is_admitted()
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap());
        }
        assert!(folded_entries(&db) >= 256 * wave, "wave {wave}");
        full_replay(&db, &store, &[T0, at, at + DAY]);
    }
    assert_eq!(net(&store, NOW + 7 * HOUR), 100_000 + 512 - 8 * 30);
    assert_clean(&store);
}

#[test]
fn an_entry_at_i64_min_is_counted_before_and_after_the_first_fold() {
    let db = TempDb::new("checkpoint-i64-min");
    let store = setup(&db, 160);
    // 已发布的架构接受任何整数序号，包括 i64::MIN：它不能被「没有检查点」的占位值吞掉。
    Connection::open(db.path())
        .unwrap()
        .execute(
            "INSERT INTO ledger_entries
                (entry_id, account_id, kind, unit, funds_micros, window_at_ms, at_ms, idempotency_key)
             VALUES (?1, 'emp', 'fund', 'THB', 100, ?2, ?2, 'min-id-fund')",
            params![i64::MIN, T0 - DAY],
        )
        .unwrap();
    seed_funds(&db, "seed-", T0 - DAY);
    // 手算：额度 160 + 序号 i64::MIN 的 100 + 256 条各 1 = 516。
    assert_eq!(net(&store, NOW), 516);
    compact(&store, "first-fold", NOW);
    assert!(folded_entries(&db) >= 257);
    assert_eq!(net(&store, NOW), 516);
    full_replay(&db, &store, &[T0, NOW]);
}

#[test]
fn a_retained_hold_starting_at_i64_min_blocks_the_fold_instead_of_being_folded() {
    let db = TempDb::new("checkpoint-hold-at-min");
    let store = setup(&db, 160);
    set_ttl(&store, 4 * HOUR);
    admit(&store, "pinned", T0, 100);
    set_ttl(&store, HOUR / 2);
    // 这个仍开着的预留在 emp 上的 reserve 分录序号是 i64::MIN：「在它之前停下」没有可表示的值。
    let conn = Connection::open(db.path()).unwrap();
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS ledger_entries_append_only_update;
         DROP TRIGGER IF EXISTS ledger_entries_append_only_delete;",
    )
    .unwrap();
    conn.execute(
        "UPDATE ledger_entries SET entry_id = ?1
          WHERE hold_id = 'pinned#0' AND account_id = 'emp' AND kind = 'reserve'",
        [i64::MIN],
    )
    .unwrap();
    seed_funds(&db, "seed-", T0 - DAY);
    let before = net(&store, NOW);
    compact(&store, "advance", NOW);
    // 不能前进：预留还开着，它的第一条分录就是 i64::MIN。
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM ledger_checkpoints WHERE account_id = 'emp'"
        ),
        0
    );
    assert_eq!(net(&store, NOW), before);
    full_replay(&db, &store, &[T0, NOW]);
}
