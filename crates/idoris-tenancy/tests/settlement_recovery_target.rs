#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price, SpendGate};
use rusqlite::{Connection, TransactionBehavior};
use std::time::Duration;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "idoris-settlement-recovery-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

fn open(path: &std::path::Path, clock: Arc<TestClock>) -> BudgetLedger {
    let ledger = BudgetLedger::open_with(path, clock, 100).unwrap();
    ledger
        .configure_tenant("tenant", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger
        .configure(
            &BudgetScope::new("tenant", "key", "provider", "model"),
            100,
            "UTC",
        )
        .unwrap();
    ledger
}

#[test]
fn unresolved_intent_is_scoped_to_its_tenant_across_expiry_and_restart() {
    let db = path();
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let a_scope = BudgetScope::new("tenant-a", "key", "provider", "model");
    let b_scope = BudgetScope::new("tenant-b", "key", "provider", "model");
    let ledger =
        BudgetLedger::open_with_busy_timeout(&db, clock.clone(), 100, Duration::ZERO).unwrap();
    for tenant in ["tenant-a", "tenant-b"] {
        ledger
            .configure_tenant(tenant, 100, "UTC", SpendGate::PaidOnly)
            .unwrap();
    }
    ledger.configure(&a_scope, 100, "UTC").unwrap();
    ledger.configure(&b_scope, 100, "UTC").unwrap();

    let a_id = ledger.reserve(&a_scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant-a", &a_id).unwrap();
    let mut busy_sidecar =
        Connection::open(db.with_added_extension("settlements.sqlite3")).unwrap();
    let lock = busy_sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    // Cancellation with unknown upstream outcome must retain the durable
    // intent and hold; ownership does not prove that no charge is due.
    ledger.abandon_dispatch("tenant-a", &a_id).unwrap();
    drop(lock);
    drop(busy_sidecar);
    let blocked = ledger.reserve(&a_scope, Price::Known(1)).unwrap_err();
    assert!(matches!(blocked, BudgetError::Storage(_)));
    assert!(!blocked.to_string().contains(&a_id.0));
    assert!(ledger.release("tenant-a", &a_id).is_err());

    let b_id = ledger.reserve(&b_scope, Price::Known(20)).unwrap();
    ledger.begin_settlement("tenant-b", &b_id).unwrap();
    assert_eq!(
        ledger.settle_durable("tenant-b", &b_id, 17).unwrap(),
        Some(17)
    );
    // Global recovery tolerates A's unresolved intent while still charging B.
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant-b").unwrap(), 83);
    clock.0.store(10_000, Ordering::SeqCst);
    assert!(ledger.reserve(&a_scope, Price::Known(1)).is_err());
    assert!(ledger.reserve(&b_scope, Price::Known(1)).is_ok());
    drop(ledger);

    let reopened = BudgetLedger::open_with(&db, clock, 100).unwrap();
    assert!(reopened.reserve(&a_scope, Price::Known(1)).is_err());
    assert_eq!(reopened.tenant_balance("tenant-b").unwrap(), 82);
    let after_restart = reopened.reserve(&b_scope, Price::Known(10)).unwrap();
    reopened
        .begin_settlement("tenant-b", &after_restart)
        .unwrap();
    assert_eq!(
        reopened
            .settle_durable("tenant-b", &after_restart, 9)
            .unwrap(),
        Some(9)
    );
    assert_eq!(reopened.tenant_balance("tenant-b").unwrap(), 73);
    let sidecar = Connection::open(db.with_added_extension("settlements.sqlite3")).unwrap();
    let retained: (String, String) = sidecar
        .query_row(
            "SELECT tenant_id, reservation_id FROM settlement_intents",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained, ("tenant-a".into(), a_id.0));
    drop(sidecar);
    drop(reopened);
    for file in [&db, &db.with_added_extension("settlements.sqlite3")] {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = file.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[test]
fn double_storage_fault_fails_closed_and_durably_recovers_after_expiry_and_restart() {
    let db = path();
    let sidecar_path = db.with_added_extension("settlements.sqlite3");
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = open(&db, clock.clone());
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant", &id).unwrap();

    // Fail outbox insertion and primary settlement simultaneously. The
    // recoverable actual amount must be saved in the primary emergency journal.
    let sidecar = Connection::open(&sidecar_path).unwrap();
    sidecar
        .execute_batch(
            "CREATE TRIGGER fail_pending_insert BEFORE INSERT ON pending_settlements
             BEGIN SELECT RAISE(FAIL, 'injected sidecar failure'); END;",
        )
        .unwrap();
    drop(sidecar);
    let primary = Connection::open(&db).unwrap();
    primary
        .execute_batch(
            "CREATE TRIGGER fail_settlement BEFORE UPDATE OF status ON reservations
             WHEN NEW.status='settled'
             BEGIN SELECT RAISE(FAIL, 'injected primary settlement failure'); END;",
        )
        .unwrap();
    drop(primary);
    assert_eq!(ledger.settle_durable("tenant", &id, 31).unwrap(), None);

    // The recorded emergency outcome owns this reservation. Conflicting
    // settlement/release requests must not replace or erase its actual cost.
    assert!(ledger.settle_durable("tenant", &id, 32).is_err());
    assert!(ledger.settle("tenant", &id, 32).is_err());
    assert!(ledger.release("tenant", &id).is_err());

    // While both faults remain, automatic recovery must fail closed. Try
    // before and after the original reservation TTL to guard against expiry
    // silently reopening admission.
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());
    clock.0.store(10_000, Ordering::SeqCst);
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());
    drop(ledger);

    // Restart while faulty: startup retries but retains the durable record;
    // admission remains blocked after restart as well.
    let restarted = open(&db, clock.clone());
    assert!(restarted.reserve(&scope, Price::Known(1)).is_err());

    // Restore both stores. Startup retained the emergency outcome while the
    // sidecar trigger failed; retry now imports it and replays the charge.
    let primary = Connection::open(&db).unwrap();
    primary
        .execute_batch("DROP TRIGGER fail_settlement;")
        .unwrap();
    drop(primary);
    let sidecar = Connection::open(&sidecar_path).unwrap();
    sidecar
        .execute_batch("DROP TRIGGER fail_pending_insert;")
        .unwrap();
    drop(sidecar);
    restarted.retry_settlements().unwrap();
    assert_eq!(restarted.tenant_balance("tenant").unwrap(), 69);
    drop(restarted);
    let recovered = open(&db, clock.clone());
    assert_eq!(recovered.tenant_balance("tenant").unwrap(), 69);
    assert_eq!(recovered.balance(&scope).unwrap(), 69);
    assert!(recovered.reserve(&scope, Price::Known(1)).is_ok());

    let primary = Connection::open(&db).unwrap();
    let row: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), 31));
    let emergency_count: i64 = primary
        .query_row(
            "SELECT COUNT(*) FROM budget_emergency_settlements",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(emergency_count, 0);
    let sidecar = Connection::open(&sidecar_path).unwrap();
    let pending: i64 = sidecar
        .query_row("SELECT COUNT(*) FROM pending_settlements", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(pending, 0);
    assert_eq!(recovered.tenant_balance("tenant").unwrap(), 68);
    drop(recovered);

    let reopened = open(&db, clock);
    assert_eq!(reopened.tenant_balance("tenant").unwrap(), 68);
    drop(reopened);
    for suffix in ["", "-wal", "-shm"] {
        let mut file = db.clone().into_os_string();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut file = sidecar_path.clone().into_os_string();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
}

#[test]
fn volatile_emergency_outcome_retries_after_primary_busy_without_cost_overwrite() {
    let db = path();
    let sidecar_path = db.with_added_extension("settlements.sqlite3");
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = BudgetLedger::open_with_busy_timeout(&db, clock, 100, Duration::ZERO).unwrap();
    ledger
        .configure_tenant("tenant", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger.configure(&scope, 100, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant", &id).unwrap();

    let sidecar = Connection::open(&sidecar_path).unwrap();
    sidecar
        .execute_batch(
            "CREATE TRIGGER fail_pending_insert BEFORE INSERT ON pending_settlements
             BEGIN SELECT RAISE(FAIL, 'injected sidecar failure'); END;",
        )
        .unwrap();
    let mut primary = Connection::open(&db).unwrap();
    let writer = primary
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();

    assert!(ledger.settle_durable("tenant", &id, 31).is_err());
    assert!(ledger.settle_durable("tenant", &id, 32).is_err());
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());

    writer.rollback().unwrap();
    sidecar
        .execute_batch("DROP TRIGGER fail_pending_insert;")
        .unwrap();
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 69);
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 69);
    let row: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), 31));
    drop(primary);
    drop(sidecar);
    drop(ledger);
    for suffix in ["", "-wal", "-shm"] {
        let mut file = db.clone().into_os_string();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut file = sidecar_path.clone().into_os_string();
        file.push(suffix);
        let _ = std::fs::remove_file(file);
    }
}

#[test]
fn sidecar_busy_foreign_tenant_cannot_poison_real_settlement() {
    let db = path();
    let sidecar_path = db.with_added_extension("settlements.sqlite3");
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger =
        BudgetLedger::open_with_busy_timeout(&db, clock.clone(), 100, Duration::ZERO).unwrap();
    ledger
        .configure_tenant("tenant", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger.configure(&scope, 100, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    let control_id = ledger.reserve(&scope, Price::Known(10)).unwrap();
    ledger.begin_settlement("tenant", &id).unwrap();

    let mut sidecar = Connection::open(&sidecar_path).unwrap();
    let writer = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();

    assert!(matches!(
        ledger.settle_durable("other", &id, 7),
        Err(BudgetError::TenantMismatch { .. })
    ));
    writer.rollback().unwrap();

    // A foreign tenant's rejected settlement must not erase A's live marker.
    // C can begin only while that real in-flight exemption is still present.
    ledger.begin_settlement("tenant", &control_id).unwrap();
    ledger
        .release_confirmed_unexecuted("tenant", &control_id)
        .unwrap();

    let writer = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert_eq!(ledger.settle_durable("tenant", &id, 31).unwrap(), None);

    // The persisted intent must continue to block admission after the TTL,
    // while the real outcome is waiting for the sidecar writer.
    clock.0.store(101, Ordering::SeqCst);
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());

    writer.rollback().unwrap();
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 69);
    assert_eq!(ledger.balance(&scope).unwrap(), 69);
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 69);
    assert!(ledger.reserve(&scope, Price::Known(1)).is_ok());

    let primary = Connection::open(&db).unwrap();
    let row: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), 31));
    let emergency_count: i64 = primary
        .query_row(
            "SELECT COUNT(*) FROM budget_emergency_settlements",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(emergency_count, 0);
    let intent_count: i64 = sidecar
        .query_row("SELECT COUNT(*) FROM settlement_intents", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(intent_count, 0);

    drop(primary);
    drop(sidecar);
    drop(ledger);
    for file in [&db, &sidecar_path] {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = file.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[test]
fn sidecar_busy_completed_settlement_fences_other_dispatch_until_recovered() {
    let db = path();
    let sidecar_path = db.with_added_extension("settlements.sqlite3");
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = BudgetLedger::open_with_busy_timeout(&db, clock, 100, Duration::ZERO).unwrap();
    ledger
        .configure_tenant("tenant", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger.configure(&scope, 100, "UTC").unwrap();
    let a_id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    let b_id = ledger.reserve(&scope, Price::Known(20)).unwrap();
    ledger.begin_settlement("tenant", &a_id).unwrap();

    let mut sidecar = Connection::open(&sidecar_path).unwrap();
    let writer = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert_eq!(ledger.settle_durable("tenant", &a_id, 31).unwrap(), None);
    drop(writer);

    // A completed request retains its durable intent but loses its volatile
    // live exemption after Busy. B must not dispatch on its earlier reserve.
    let intent_count: i64 = sidecar
        .query_row(
            "SELECT COUNT(*) FROM settlement_intents WHERE reservation_id=?1 AND tenant_id='tenant'",
            [&a_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(intent_count, 1);
    // Recovery promotes A's primary fallback once the sidecar writer clears.
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 49);
    ledger.begin_settlement("tenant", &b_id).unwrap();
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 49);

    drop(sidecar);
    drop(ledger);
    for file in [&db, &sidecar_path] {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = file.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[test]
fn failed_initial_owner_lookup_retains_cost_until_storage_recovers() {
    let db = path();
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = open(&db, clock.clone());
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant", &id).unwrap();

    // Removing the table makes the ownership SELECT fail deterministically;
    // restore it before retrying the in-memory completion record.
    let primary = Connection::open(&db).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations RENAME TO reservations_temporarily_hidden;")
        .unwrap();
    drop(primary);

    assert!(ledger.settle_durable("other", &id, 7).is_err());
    assert_eq!(ledger.settle_durable("tenant", &id, 31).unwrap(), None);
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());

    let primary = Connection::open(&db).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations_temporarily_hidden RENAME TO reservations;")
        .unwrap();
    drop(primary);
    for result in [
        ledger.settle_durable("tenant", &id, 32).map(|_| ()),
        ledger.settle("tenant", &id, 32).map(|_| ()),
        ledger.release_confirmed_unexecuted("tenant", &id),
    ] {
        assert!(result.is_err());
    }
    clock.0.store(10_000, Ordering::SeqCst);
    // Admission retries the saved charge first: the remaining 69 cannot
    // cover 70, even though the original reservation has expired.
    assert!(ledger.reserve(&scope, Price::Known(70)).is_err());

    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 69);
    assert_eq!(ledger.balance(&scope).unwrap(), 69);
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("tenant").unwrap(), 69);
    let primary = Connection::open(&db).unwrap();
    let row: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), 31));
    drop(primary);
    drop(ledger);
    for file in [&db, &db.with_added_extension("settlements.sqlite3")] {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = file.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[test]
fn another_instance_cannot_release_volatile_settlement_intent() {
    let db = path();
    let sidecar_path = db.with_added_extension("settlements.sqlite3");
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let a = BudgetLedger::open_with_busy_timeout(&db, clock.clone(), 100, Duration::ZERO).unwrap();
    a.configure_tenant("tenant", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    a.configure(&scope, 100, "UTC").unwrap();
    let b = BudgetLedger::open_with_busy_timeout(&db, clock.clone(), 100, Duration::ZERO).unwrap();
    let id = a.reserve(&scope, Price::Known(40)).unwrap();
    a.begin_settlement("tenant", &id).unwrap();

    let mut sidecar = Connection::open(&sidecar_path).unwrap();
    let writer = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert_eq!(a.settle_durable("tenant", &id, 31).unwrap(), None);
    writer.rollback().unwrap();

    assert!(b.release("tenant", &id).is_err());
    clock.0.store(10_000, Ordering::SeqCst);
    let after_recovery = b.reserve(&scope, Price::Known(1)).unwrap();
    assert_eq!(b.tenant_balance("tenant").unwrap(), 68);
    b.release("tenant", &after_recovery).unwrap();
    assert_eq!(b.tenant_balance("tenant").unwrap(), 69);
    a.retry_settlements().unwrap();
    assert_eq!(b.tenant_balance("tenant").unwrap(), 69);
    a.retry_settlements().unwrap();
    assert_eq!(b.tenant_balance("tenant").unwrap(), 69);

    let confirmed = a.reserve(&scope, Price::Known(10)).unwrap();
    a.begin_settlement("tenant", &confirmed).unwrap();
    assert!(matches!(
        a.release_confirmed_unexecuted("other", &confirmed),
        Err(BudgetError::TenantMismatch { .. })
    ));
    a.release_confirmed_unexecuted("tenant", &confirmed)
        .unwrap();
    let ordinary = a.reserve(&scope, Price::Known(10)).unwrap();
    a.release("tenant", &ordinary).unwrap();

    let primary = Connection::open(&db).unwrap();
    let row: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), 31));
    drop(primary);
    drop(sidecar);
    drop(a);
    drop(b);
    for file in [&db, &sidecar_path] {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = file.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}
