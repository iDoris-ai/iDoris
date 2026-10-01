#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price, SpendGate};

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "idoris-settlement-alias-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn real(&self) -> PathBuf {
        self.0.join("ledger.sqlite3")
    }

    fn alias(&self) -> PathBuf {
        self.0.join("ledger-alias.sqlite3")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(path: &Path, clock: Arc<TestClock>, ttl_ms: i64) -> BudgetLedger {
    let ledger = BudgetLedger::open_with(path, clock, ttl_ms).unwrap();
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

fn paths(temp: &TempDb, a_uses_alias: bool) -> (PathBuf, PathBuf) {
    let real = temp.real();
    let alias = temp.alias();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    if a_uses_alias {
        (alias, real)
    } else {
        (real, alias)
    }
}

fn unknown_outcome(err: BudgetError) -> bool {
    matches!(err, BudgetError::Storage(message) if message.contains("unconfirmed dispatch outcome"))
}

#[test]
fn cancellation_via_real_and_symlink_keeps_other_instance_fenced() {
    for a_uses_alias in [false, true] {
        let temp = TempDb::new();
        let (a_path, b_path) = paths(&temp, a_uses_alias);
        let clock = Arc::new(TestClock(AtomicI64::new(0)));
        let a = open(&a_path, clock.clone(), 100);
        let b = open(&b_path, clock, 100);
        let scope = BudgetScope::new("tenant", "key", "provider", "model");
        let id = a.reserve(&scope, Price::Known(40)).unwrap();
        a.begin_settlement("tenant", &id).unwrap();

        // release ends the request lifetime and clears the volatile exemption;
        // its conflict leaves the durable dispatch intent unresolved.
        assert!(matches!(
            a.release("tenant", &id),
            Err(BudgetError::SettlementConflict { .. })
        ));
        assert!(unknown_outcome(
            b.reserve(&scope, Price::Known(1)).unwrap_err()
        ));
    }
}

#[test]
fn other_instance_release_cannot_delete_unknown_intent_through_alias() {
    for a_uses_alias in [false, true] {
        let temp = TempDb::new();
        let (a_path, b_path) = paths(&temp, a_uses_alias);
        let clock = Arc::new(TestClock(AtomicI64::new(0)));
        let a = open(&a_path, clock.clone(), 100);
        let b = open(&b_path, clock, 100);
        let scope = BudgetScope::new("tenant", "key", "provider", "model");
        let id = a.reserve(&scope, Price::Known(40)).unwrap();
        a.begin_settlement("tenant", &id).unwrap();

        assert!(matches!(
            b.release("tenant", &id),
            Err(BudgetError::SettlementConflict { .. })
        ));
        let sidecar_path = temp
            .real()
            .canonicalize()
            .unwrap()
            .with_added_extension("settlements.sqlite3");
        let sidecar = rusqlite::Connection::open(sidecar_path).unwrap();
        let intents: i64 = sidecar
            .query_row(
                "SELECT COUNT(*) FROM settlement_intents WHERE reservation_id=?1",
                [&id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(intents, 1);
        assert!(
            !temp
                .alias()
                .with_added_extension("settlements.sqlite3")
                .exists()
        );
    }
}

#[test]
fn expired_unknown_outcome_stays_fenced_across_alias_restart_and_recovers_once() {
    for a_uses_alias in [false, true] {
        let temp = TempDb::new();
        let (a_path, b_path) = paths(&temp, a_uses_alias);
        let clock = Arc::new(TestClock(AtomicI64::new(0)));
        let a = open(&a_path, clock.clone(), 100);
        let b = open(&b_path, clock.clone(), 100);
        let scope = BudgetScope::new("tenant", "key", "provider", "model");
        let id = a.reserve(&scope, Price::Known(40)).unwrap();
        a.begin_settlement("tenant", &id).unwrap();
        assert!(matches!(
            a.release("tenant", &id),
            Err(BudgetError::SettlementConflict { .. })
        ));
        clock.0.store(101, Ordering::SeqCst);
        assert!(unknown_outcome(
            b.reserve(&scope, Price::Known(1)).unwrap_err()
        ));
        drop(b);
        drop(a);

        let reopened = open(&b_path, clock, 100);
        assert!(unknown_outcome(
            reopened.reserve(&scope, Price::Known(1)).unwrap_err()
        ));
        assert_eq!(
            reopened.settle_durable("tenant", &id, 31).unwrap(),
            Some(31)
        );
        reopened.retry_settlements().unwrap();
        assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
        assert_eq!(
            reopened.settle_durable("tenant", &id, 31).unwrap(),
            Some(31)
        );
        reopened.retry_settlements().unwrap();
        assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
        let next = reopened.reserve(&scope, Price::Known(1)).unwrap();
        reopened.release("tenant", &next).unwrap();
        assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
    }
}
