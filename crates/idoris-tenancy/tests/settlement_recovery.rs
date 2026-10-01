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
fn double_storage_fault_fails_closed_and_durably_recovers_after_expiry_and_restart() {
    let db = path();
    let sidecar_path = db.with_added_extension("settlements.sqlite3");
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = open(&db, clock.clone());
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();

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
    assert!(matches!(
        ledger.settle_durable("tenant", &id, 32),
        Err(BudgetError::SettlementConflict { .. })
    ));
    assert!(matches!(
        ledger.settle("tenant", &id, 32),
        Err(BudgetError::SettlementConflict { .. })
    ));
    assert!(matches!(
        ledger.release("tenant", &id),
        Err(BudgetError::SettlementConflict { .. })
    ));

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

    // Restore the primary while sidecar insertion is still failing. The
    // already-recorded emergency outcome should settle directly and replay
    // cleanup should then be idempotent across retry and another reopen.
    let primary = Connection::open(&db).unwrap();
    primary
        .execute_batch("DROP TRIGGER fail_settlement;")
        .unwrap();
    drop(primary);
    assert_eq!(
        restarted.settle_durable("tenant", &id, 31).unwrap(),
        Some(31)
    );
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

    let sidecar = Connection::open(&sidecar_path).unwrap();
    sidecar
        .execute_batch("DROP TRIGGER fail_pending_insert;")
        .unwrap();
    drop(sidecar);
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
    assert!(matches!(
        ledger.settle_durable("tenant", &id, 32),
        Err(BudgetError::SettlementConflict { .. })
    ));
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());

    writer.rollback().unwrap();
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
