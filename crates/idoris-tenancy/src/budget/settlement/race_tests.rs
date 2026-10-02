#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::budget::{BudgetScope, Clock, Price, SpendGate, SystemClock};
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[test]
fn release_cannot_cross_active_read_and_pending_insert_window() {
    let path = std::env::temp_dir().join(format!(
        "idoris-settlement-controlled-race-{}.sqlite3",
        uuid::Uuid::new_v4()
    ));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let first =
        BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 60_000, Duration::ZERO).unwrap();
    first
        .configure_tenant("t", 100, "UTC", SpendGate::All)
        .unwrap();
    let id = first
        .reserve(&BudgetScope::new("t", "k", "p", "m"), Price::Known(10))
        .unwrap();

    // A real primary-ledger failure leaves the durable journal row pending.
    first
        .conn
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_controlled_settlement
             BEFORE UPDATE OF status ON reservations WHEN NEW.status='settled'
             BEGIN SELECT RAISE(ABORT, 'injected controlled settlement failure'); END;",
        )
        .unwrap();

    let second =
        BudgetLedger::open_with_busy_timeout(&path, clock, 60_000, Duration::ZERO).unwrap();
    let (at_window_tx, at_window_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let settle_id = id.clone();
    let settler = std::thread::spawn(move || {
        first.settle_durable_with_hook("t", &settle_id, 8, || {
            at_window_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
        })
    });

    // This notification only fires after the active state read has completed,
    // while the settling connection still owns its sidecar write transaction.
    at_window_rx.recv().unwrap();
    let release = second.release("t", &id);
    assert!(matches!(release, Err(BudgetError::Busy)));

    continue_tx.send(()).unwrap();
    assert_eq!(settler.join().unwrap().unwrap(), None);

    let pending = second
        .settlements
        .lock()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM pending_settlements WHERE reservation_id=?1",
            [&id.0],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(pending, 1);
    assert!(matches!(
        second.release("t", &id),
        Err(BudgetError::SettlementConflict { .. }) | Err(BudgetError::Storage(_))
    ));

    drop(second);
    let sidecar = path.with_added_extension("settlements.sqlite3");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar);
}

#[test]
fn memory_settlement_conflict_is_serialized_with_busy_recovery() {
    let path = std::env::temp_dir().join(format!(
        "idoris-settlement-memory-race-{}.sqlite3",
        uuid::Uuid::new_v4()
    ));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ledger = Arc::new(
        BudgetLedger::open_with_busy_timeout(&path, clock.clone(), 60_000, Duration::ZERO).unwrap(),
    );
    ledger
        .configure_tenant("t", 100, "UTC", SpendGate::All)
        .unwrap();
    let scope = BudgetScope::new("t", "k", "p", "m");
    let id = ledger.reserve(&scope, Price::Known(10)).unwrap();
    ledger.begin_settlement("t", &id).unwrap();

    let sidecar_path = path.with_added_extension("settlements.sqlite3");
    let mut external = rusqlite::Connection::open(&sidecar_path).unwrap();
    let sidecar_lock = external
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let mut external_primary = rusqlite::Connection::open(&path).unwrap();
    let primary_lock = external_primary
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();

    let (at_check_tx, at_check_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let thread_ledger = Arc::clone(&ledger);
    let thread_id = id.clone();
    let contender = std::thread::spawn(move || {
        thread_ledger.settle_durable_before_lock_hook("t", &thread_id, 1, || {
            at_check_tx.send(()).unwrap();
            resume_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("resume contender after busy result");
        })
    });

    at_check_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("contender reaches pre-coordination hook");
    assert!(matches!(
        ledger.settle_durable("t", &id, 31),
        Err(BudgetError::Busy)
    ));
    assert_eq!(
        ledger.settlement_outcomes.lock().unwrap().get(&id.0),
        Some(&("t".to_owned(), 31))
    );
    assert!(!ledger.live_intents.lock().unwrap().contains_key(&id.0));

    sidecar_lock.rollback().unwrap();
    primary_lock.rollback().unwrap();
    resume_tx.send(()).unwrap();
    let competing_result = contender.join().unwrap();
    assert!(
        matches!(
            competing_result,
            Err(BudgetError::SettlementConflict { .. }) | Err(BudgetError::Storage(_))
        ),
        "conflicting settlement returned {competing_result:?}"
    );
    let actual: Option<i64> = ledger
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1 AND status='settled'",
            [&id.0],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(actual, None);

    ledger.retry_settlements().unwrap();
    let recovered_actual: i64 = ledger
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1 AND status='settled'",
            [&id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recovered_actual, 31);
    assert_eq!(ledger.tenant_balance("t").unwrap(), 69);
    let next = ledger.reserve(&scope, Price::Known(1)).unwrap();
    ledger.release("t", &next).unwrap();
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.tenant_balance("t").unwrap(), 69);
    let quarantined: i64 = ledger
        .settlements
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM quarantined_settlements", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(quarantined, 0);

    drop(ledger);
    let restarted =
        BudgetLedger::open_with_busy_timeout(&path, clock, 60_000, Duration::ZERO).unwrap();
    restarted.retry_settlements().unwrap();
    assert_eq!(restarted.tenant_balance("t").unwrap(), 69);
    drop(restarted);
    drop(external);
    drop(external_primary);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar_path);
}
