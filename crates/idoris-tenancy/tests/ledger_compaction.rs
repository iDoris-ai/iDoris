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
