#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use std::time::Duration;

use idoris_tenancy::budget::{
    BudgetError, BudgetLedger, BudgetScope, Clock, Price, ReservationId, SpendGate,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TempDb(std::path::PathBuf);
impl Drop for TempDb {
    fn drop(&mut self) {
        let sidecar = self.0.with_added_extension("settlements.sqlite3");
        for file in [&self.0, &sidecar] {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = file.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn setup() -> (
    TempDb,
    BudgetLedger,
    Arc<TestClock>,
    BudgetScope,
    ReservationId,
) {
    let db = TempDb(std::env::temp_dir().join(format!(
        "idoris-sidecar-recovery-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )));
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    let ledger =
        BudgetLedger::open_with_busy_timeout(&db.0, clock.clone(), 100, Duration::ZERO).unwrap();
    ledger
        .configure_tenant("tenant", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger.configure(&scope, 100, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant", &id).unwrap();
    (db, ledger, clock, scope, id)
}

fn assert_pending(sidecar: &Connection, id: &str, expected: Option<(&str, i64)>) {
    let row: Option<(String, i64)> = sidecar
        .query_row(
            "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap();
    assert_eq!(
        row,
        expected.map(|(tenant, cost)| (tenant.to_owned(), cost))
    );
}

fn assert_busy<T>(result: Result<T, BudgetError>) {
    assert!(matches!(result, Err(BudgetError::Busy)));
}

fn recover_after_sidecar_unlock(primary_unreadable: bool, block_commit: bool) {
    let (db, ledger, clock, scope, id) = setup();
    let mut primary = Connection::open(&db.0).unwrap();
    let mut sidecar = Connection::open(db.0.with_added_extension("settlements.sqlite3")).unwrap();
    if primary_unreadable {
        primary
            .execute_batch("ALTER TABLE reservations RENAME TO reservations_hidden;")
            .unwrap();
        // begin_settlement captured this ledger's verified dispatch owner.
        // The unreadable primary blocks replay, but the actual cost must still
        // reach the sidecar before retry attempts that primary read.
        assert_eq!(ledger.settle_durable("tenant", &id, 31).unwrap(), None);
        primary
            .execute_batch("ALTER TABLE reservations_hidden RENAME TO reservations;")
            .unwrap();
    }
    let primary_lock = primary
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    if !primary_unreadable {
        let writer = sidecar
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_busy(ledger.settle_durable("tenant", &id, 31));
        writer.rollback().unwrap();
    }
    if block_commit {
        // A rollback-journal reader permits BEGIN IMMEDIATE and INSERT but
        // prevents COMMIT. Failed promotion must retain the in-memory cost.
        let mode: String = sidecar
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        let reader = sidecar.transaction().unwrap();
        let expected = primary_unreadable.then_some(("tenant", 31));
        assert_pending(&reader, &id.0, expected);
        assert_busy(ledger.retry_settlements());
        assert_pending(&reader, &id.0, expected);
        reader.rollback().unwrap();
    }
    // Only the sidecar has recovered; the primary writer remains held.
    assert_busy(ledger.retry_settlements());
    assert_pending(&sidecar, &id.0, Some(("tenant", 31)));
    clock.0.store(10_000, Ordering::SeqCst);
    drop(ledger);
    // Startup migrations need the primary writer; all volatile costs are
    // already gone before it becomes available again.
    assert_busy(BudgetLedger::open_with_busy_timeout(
        &db.0,
        clock.clone(),
        100,
        Duration::ZERO,
    ));
    primary_lock.rollback().unwrap();

    let recovered = BudgetLedger::open_with(&db.0, clock.clone(), 100).unwrap();
    for _ in 0..2 {
        recovered.retry_settlements().unwrap();
        assert_eq!(recovered.tenant_balance("tenant").unwrap(), 69);
        assert_eq!(recovered.balance(&scope).unwrap(), 69);
    }
    let recorded: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(recorded, ("settled".into(), 31));
    assert_pending(&sidecar, &id.0, None);
    drop(recovered);
    let reopened = BudgetLedger::open_with(&db.0, clock, 100).unwrap();
    assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
    assert!(reopened.reserve(&scope, Price::Known(1)).is_ok());
}

#[test]
fn verified_cost_survives_sidecar_recovery_before_primary_unlock() {
    recover_after_sidecar_unlock(false, false);
}

#[test]
fn primary_unreadable_verified_cost_survives_sidecar_recovery_before_primary_unlock() {
    recover_after_sidecar_unlock(true, false);
}

#[test]
fn verified_cost_survives_retry_sidecar_commit_busy() {
    recover_after_sidecar_unlock(false, true);
}

#[test]
fn primary_unreadable_verified_cost_survives_retry_sidecar_commit_busy() {
    recover_after_sidecar_unlock(true, true);
}

#[derive(Clone, Copy)]
enum SidecarFailure {
    BeginBusy,
    InsertRollback,
    CommitBusy,
}

fn recover_after_primary_fallback(sidecar_failure: SidecarFailure) {
    let (db, ledger, clock, scope, first) = setup();
    let second = ledger.reserve(&scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant", &second).unwrap();
    let mut primary = Connection::open(&db.0).unwrap();
    let sidecar_path = db.0.with_added_extension("settlements.sqlite3");
    let sidecar = Connection::open(&sidecar_path).unwrap();
    let primary_lock = primary
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    sidecar.execute_batch("BEGIN IMMEDIATE;").unwrap();

    assert_busy(ledger.settle_durable("tenant", &first, 31));
    assert_busy(ledger.settle_durable("tenant", &second, 22));
    // Let promotion write to the primary while the sidecar remains blocked.
    primary_lock.rollback().unwrap();
    if !matches!(sidecar_failure, SidecarFailure::BeginBusy) {
        sidecar.execute_batch("ROLLBACK;").unwrap();
    }
    if matches!(sidecar_failure, SidecarFailure::InsertRollback) {
        sidecar
            .execute_batch(
                "CREATE TRIGGER rollback_pending BEFORE INSERT ON pending_settlements
                 BEGIN SELECT RAISE(ROLLBACK, 'injected rollback'); END;",
            )
            .unwrap();
    } else if matches!(sidecar_failure, SidecarFailure::CommitBusy) {
        let mode: String = sidecar
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }
    if matches!(sidecar_failure, SidecarFailure::CommitBusy) {
        sidecar.execute_batch("BEGIN DEFERRED;").unwrap();
        assert_pending(&sidecar, &first.0, None);
    }
    let retry_result = ledger.retry_settlements();
    match sidecar_failure {
        SidecarFailure::BeginBusy | SidecarFailure::CommitBusy => assert_busy(retry_result),
        SidecarFailure::InsertRollback => {
            assert!(matches!(retry_result, Err(BudgetError::Storage(_))));
        }
    }
    for (id, expected) in [(&first, 31), (&second, 22)] {
        let recorded: Option<i64> = primary
            .query_row(
                "SELECT actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |r| r.get::<_, Option<i64>>(0),
            )
            .unwrap();
        assert_eq!(recorded, Some(expected));
        assert_pending(&sidecar, &id.0, None);
    }

    drop(ledger);
    if matches!(
        sidecar_failure,
        SidecarFailure::BeginBusy | SidecarFailure::CommitBusy
    ) {
        sidecar.execute_batch("ROLLBACK;").unwrap();
    }
    if matches!(sidecar_failure, SidecarFailure::InsertRollback) {
        sidecar
            .execute_batch("DROP TRIGGER rollback_pending;")
            .unwrap();
    }
    clock.0.store(10_000, Ordering::SeqCst);
    let recovered = BudgetLedger::open_with(&db.0, clock.clone(), 100).unwrap();
    for _ in 0..3 {
        recovered.retry_settlements().unwrap();
        assert_eq!(recovered.tenant_balance("tenant").unwrap(), 47);
        assert_eq!(recovered.balance(&scope).unwrap(), 47);
    }
    for (id, expected) in [(&first, 31), (&second, 22)] {
        let recorded: (String, i64) = primary
            .query_row(
                "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(recorded, ("settled".into(), expected));
        assert_pending(&sidecar, &id.0, None);
    }
    drop(recovered);
    let reopened = BudgetLedger::open_with(&db.0, clock, 100).unwrap();
    assert_eq!(reopened.tenant_balance("tenant").unwrap(), 47);
    assert_eq!(reopened.balance(&scope).unwrap(), 47);
    assert!(reopened.reserve(&scope, Price::Known(1)).is_ok());
}

#[test]
fn verified_cost_survives_retry_sidecar_begin_busy_after_primary_unlock() {
    recover_after_primary_fallback(SidecarFailure::BeginBusy);
}

#[test]
fn verified_cost_survives_retry_sidecar_insert_rollback() {
    recover_after_primary_fallback(SidecarFailure::InsertRollback);
}

#[test]
fn verified_cost_survives_retry_sidecar_commit_busy_after_primary_unlock() {
    recover_after_primary_fallback(SidecarFailure::CommitBusy);
}

fn retry_after_primary_fallback_failure(conflicting_cost: bool) {
    let (db, ledger, clock, scope, id) = setup();
    let mut primary = Connection::open(&db.0).unwrap();
    let sidecar = Connection::open(db.0.with_added_extension("settlements.sqlite3")).unwrap();
    let primary_lock = primary
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    sidecar.execute_batch("BEGIN IMMEDIATE;").unwrap();
    assert_busy(ledger.settle_durable("tenant", &id, 31));
    primary_lock.rollback().unwrap();
    if conflicting_cost {
        primary
            .execute(
                "UPDATE reservations SET actual_cost_minor=32 WHERE id=?1",
                [&id.0],
            )
            .unwrap();
    } else {
        primary.execute_batch(
            "CREATE TRIGGER fail_primary_fallback BEFORE UPDATE OF actual_cost_minor ON reservations
             BEGIN SELECT RAISE(ABORT, 'injected primary failure'); END;",
        )
        .unwrap();
    }

    assert_busy(ledger.retry_settlements());
    let absent: Option<i64> = primary
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(absent, conflicting_cost.then_some(32));
    if conflicting_cost {
        // Remove the injected conflicting fixture; the verified amount must
        // still be in memory and become durable on the next retry.
        primary
            .execute(
                "UPDATE reservations SET actual_cost_minor=NULL WHERE id=?1",
                [&id.0],
            )
            .unwrap();
    } else {
        primary
            .execute_batch("DROP TRIGGER fail_primary_fallback;")
            .unwrap();
    }
    assert_busy(ledger.retry_settlements());
    let recovered: Option<i64> = primary
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recovered, Some(31));

    drop(ledger);
    sidecar.execute_batch("ROLLBACK;").unwrap();
    clock.0.store(10_000, Ordering::SeqCst);
    let reopened = BudgetLedger::open_with(&db.0, clock, 100).unwrap();
    reopened.retry_settlements().unwrap();
    assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
    assert_eq!(reopened.balance(&scope).unwrap(), 69);
    assert!(reopened.reserve(&scope, Price::Known(1)).is_ok());
}

#[test]
fn verified_cost_remains_retryable_when_primary_fallback_fails() {
    retry_after_primary_fallback_failure(false);
}

#[test]
fn verified_cost_preserves_conflicting_primary_fallback() {
    retry_after_primary_fallback_failure(true);
}
