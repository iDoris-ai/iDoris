#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price, ReservationId};
use rusqlite::Connection;

struct TempDb(PathBuf);

impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

fn temp_db() -> TempDb {
    TempDb(std::env::temp_dir().join(format!(
        "idoris-settlement-replay-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_800_000_000_000
    }
}

fn open(path: &TempDb) -> BudgetLedger {
    BudgetLedger::open_with(path, Arc::new(FixedClock), 60_000).expect("open ledger")
}

fn open_zero_timeout(path: &TempDb) -> BudgetLedger {
    BudgetLedger::open_with_busy_timeout(path, Arc::new(FixedClock), 60_000, Duration::ZERO)
        .expect("open ledger with zero busy timeout")
}

fn setup(path: &TempDb, reserved: i64) -> (BudgetScope, ReservationId) {
    let scope = BudgetScope::new("tenant-a", "key-a", "openai", "gpt-5");
    let ledger = open(path);
    ledger.configure(&scope, 10_000, "UTC").expect("configure");
    let id = ledger
        .reserve(&scope, Price::Known(reserved))
        .expect("reserve");
    (scope, id)
}

fn totals(path: &TempDb, scope: &BudgetScope) -> (i64, i64, i64) {
    let conn = Connection::open(path).expect("open sqlite");
    let scope_spent = conn.query_row(
        "SELECT COALESCE(SUM(spent_minor), 0) FROM budget_periods WHERE tenant_id=?1 AND key_id=?2",
        (&scope.tenant_id, &scope.key_id), |row| row.get(0),
    ).expect("scope spend");
    let tenant_spent = conn
        .query_row(
            "SELECT COALESCE(SUM(spent_minor), 0) FROM tenant_periods WHERE tenant_id=?1",
            [&scope.tenant_id],
            |row| row.get(0),
        )
        .expect("tenant spend");
    let overages = conn
        .query_row(
            "SELECT COUNT(*) FROM budget_overage_events WHERE tenant_id=?1",
            [&scope.tenant_id],
            |row| row.get(0),
        )
        .expect("overage count");
    (scope_spent, tenant_spent, overages)
}

#[test]
fn busy_and_storage_failures_leave_replayable_reservation_unchanged() {
    let path = temp_db();
    let (scope, id) = setup(&path, 100);
    let ledger = open_zero_timeout(&path);
    let locker = Connection::open(&path).expect("locker");
    locker.busy_timeout(Duration::ZERO).expect("zero timeout");
    locker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("lock writer");
    assert!(matches!(
        ledger.replay_settlement(&scope.tenant_id, &id, 80),
        Err(BudgetError::Busy)
    ));
    assert_eq!(totals(&path, &scope), (0, 0, 0));
    locker.execute_batch("ROLLBACK").expect("unlock writer");
    ledger
        .replay_settlement(&scope.tenant_id, &id, 80)
        .expect("replay after busy");
    assert_eq!(totals(&path, &scope), (80, 80, 0));

    let (scope, id) = setup(&path, 100);
    let before = totals(&path, &scope);
    let ledger = open(&path);
    let conn = Connection::open(&path).expect("trigger connection");
    conn.execute_batch("CREATE TRIGGER reject_tenant_period BEFORE INSERT ON tenant_periods BEGIN SELECT RAISE(ABORT, 'injected'); END;").expect("install trigger");
    assert!(matches!(
        ledger.replay_settlement(&scope.tenant_id, &id, 80),
        Err(BudgetError::Storage(_))
    ));
    assert_eq!(totals(&path, &scope), before);
    conn.execute_batch("DROP TRIGGER reject_tenant_period")
        .expect("drop trigger");
    ledger
        .replay_settlement(&scope.tenant_id, &id, 80)
        .expect("replay after rollback");
    assert_eq!(totals(&path, &scope), (160, 160, 0));
}

#[test]
fn reopening_replay_does_not_double_charge_and_overage_is_single() {
    let path = temp_db();
    let (scope, id) = setup(&path, 100);
    {
        let ledger = open(&path);
        ledger
            .replay_settlement(&scope.tenant_id, &id, 80)
            .expect("first settlement");
    }
    open(&path)
        .replay_settlement(&scope.tenant_id, &id, 80)
        .expect("replay after reopen");
    assert_eq!(totals(&path, &scope), (80, 80, 0));

    let (scope, id) = setup(&path, 10);
    let ledger = open(&path);
    ledger
        .replay_settlement(&scope.tenant_id, &id, 50)
        .expect("first replay commits large overage");
    ledger
        .replay_settlement(&scope.tenant_id, &id, 50)
        .expect("replay committed overage");
    assert_eq!(totals(&path, &scope), (130, 130, 1));
}

#[test]
fn mismatched_or_non_actionable_settlements_are_rejected() {
    let path = temp_db();
    let (scope, id) = setup(&path, 100);
    let ledger = open(&path);
    ledger
        .replay_settlement(&scope.tenant_id, &id, 40)
        .expect("settle once");
    assert!(ledger.replay_settlement(&scope.tenant_id, &id, 41).is_err());
    assert!(ledger.replay_settlement("tenant-b", &id, 40).is_err());
    assert!(ledger.replay_settlement(&scope.tenant_id, &id, -1).is_err());

    let (_, released) = setup(&path, 100);
    ledger
        .release(&scope.tenant_id, &released)
        .expect("release");
    assert!(
        ledger
            .replay_settlement(&scope.tenant_id, &released, 20)
            .is_err()
    );
    assert!(
        ledger
            .replay_settlement(&scope.tenant_id, &ReservationId("missing".into()), 20)
            .is_err()
    );
    assert_eq!(totals(&path, &scope), (40, 40, 0));
}
