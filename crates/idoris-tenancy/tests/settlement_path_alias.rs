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
use rusqlite::Connection;

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
    std::fs::File::create(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    if a_uses_alias {
        (alias, real)
    } else {
        (real, alias)
    }
}

fn unknown_outcome(err: BudgetError) -> bool {
    // Both a live-but-unconfirmed completion and an orphaned dispatch owner
    // fail closed as storage errors; neither may admit another reservation.
    matches!(err, BudgetError::Storage(_))
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

        // The upstream outcome is unknown, so abandoning the live dispatch
        // must retain its durable intent and reservation hold.
        a.abandon_dispatch("tenant", &id).unwrap();
        let error = b.reserve(&scope, Price::Known(1)).unwrap_err();
        assert!(
            unknown_outcome(error.clone()),
            "unexpected reserve error: {error:?}"
        );
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

        assert!(b.release("tenant", &id).is_err());
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
        a.abandon_dispatch("tenant", &id).unwrap();
        clock.0.store(101, Ordering::SeqCst);
        let error = b.reserve(&scope, Price::Known(1)).unwrap_err();
        assert!(
            unknown_outcome(error.clone()),
            "unexpected reserve error: {error:?}"
        );
        drop(b);
        drop(a);

        let reopened = open(&b_path, clock, 100);
        assert!(unknown_outcome(
            reopened.reserve(&scope, Price::Known(1)).unwrap_err()
        ));
        // A reopened ledger has no proof that this process owned the
        // dispatch, so it cannot create a first actual-cost record.
        assert!(reopened.settle_durable("tenant", &id, 31).is_err());

        // Simulate a previously verified durable sidecar outcome. Opening via
        // the symlink must find and replay the canonical sidecar row.
        let sidecar =
            Connection::open(temp.real().with_added_extension("settlements.sqlite3")).unwrap();
        sidecar
            .execute(
                "INSERT INTO pending_settlements(reservation_id, tenant_id, actual_cost_minor) VALUES (?1, ?2, ?3)",
                (&id.0, "tenant", 31),
            )
            .unwrap();
        reopened.retry_settlements().unwrap();
        assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
        reopened.retry_settlements().unwrap();
        assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
        let next = reopened.reserve(&scope, Price::Known(1)).unwrap();
        reopened.release("tenant", &next).unwrap();
        assert_eq!(reopened.tenant_balance("tenant").unwrap(), 69);
    }
}
