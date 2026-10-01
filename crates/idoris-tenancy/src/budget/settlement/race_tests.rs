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
        Err(BudgetError::SettlementConflict { .. })
    ));

    drop(second);
    let sidecar = path.with_added_extension("settlements.sqlite3");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar);
}
