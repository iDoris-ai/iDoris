//! Recovery regressions for durable actual-cost settlements.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{path::Path, sync::Arc, time::Duration};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

struct TempDb(std::path::PathBuf);

impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".settlements.sqlite3",
            ".settlements.sqlite3-wal",
            ".settlements.sqlite3-shm",
        ] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(self.0.with_added_extension("dispatch-locks"));
    }
}

fn db() -> TempDb {
    TempDb(std::env::temp_dir().join(format!(
        "idoris-settlement-recovery-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_000
    }
}

fn open(path: &TempDb) -> BudgetLedger {
    BudgetLedger::open_with_busy_timeout(path, Arc::new(FixedClock), 60_000, Duration::ZERO)
        .expect("open ledger")
}

fn sidecar(path: &TempDb) -> std::path::PathBuf {
    path.as_ref().with_added_extension("settlements.sqlite3")
}

fn pending_actual(path: &TempDb, id: &str) -> Option<i64> {
    Connection::open(sidecar(path))
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

#[test]
fn sidecar_retry_persists_actual_before_primary_recovers_and_survives_restart() {
    let path = db();
    let ledger = open(&path);
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    ledger.configure(&scope, 100, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    ledger.begin_settlement("tenant", &id).unwrap();

    let mut primary = Connection::open(path.as_ref()).unwrap();
    primary.busy_timeout(Duration::ZERO).unwrap();
    let primary_lock = primary
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let mut journal = Connection::open(sidecar(&path)).unwrap();
    journal.busy_timeout(Duration::ZERO).unwrap();
    let journal_lock = journal
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();

    assert!(matches!(
        ledger.settle_durable("tenant", &id, 25),
        Err(BudgetError::Busy)
    ));

    // The original sidecar contention has cleared, while the primary writer
    // remains blocked. The retry must commit the actual amount independently.
    drop(journal_lock);
    assert!(matches!(ledger.retry_settlements(), Err(BudgetError::Busy)));
    assert_eq!(pending_actual(&path, &id.0), Some(25));

    // Simulate process loss while the primary remains locked. Verify the
    // actual amount is durable before releasing the lock; reopening performs
    // initialization writes, so it must wait until that lock is released.
    drop(ledger);
    assert_eq!(pending_actual(&path, &id.0), Some(25));
    drop(primary_lock);
    let recovered = open(&path);
    recovered.retry_settlements().unwrap();
    recovered.retry_settlements().unwrap();

    let (status, actual): (String, Option<i64>) = Connection::open(path.as_ref())
        .unwrap()
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), actual), ("settled", Some(25)));
    assert_eq!(pending_actual(&path, &id.0), None);
    let journal = Connection::open(sidecar(&path)).unwrap();
    let intents: i64 = journal
        .query_row(
            "SELECT count(*) FROM settlement_intents WHERE reservation_id=?1",
            [&id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(intents, 0);
    let owners: i64 = journal
        .query_row(
            "SELECT count(*) FROM dispatch_owners WHERE reservation_id=?1",
            [&id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owners, 0);
    assert_eq!(recovered.balance(&scope).unwrap(), 75);

    let next = recovered.reserve(&scope, Price::Known(20)).unwrap();
    assert_eq!(recovered.balance(&scope).unwrap(), 55);
    recovered.settle_durable("tenant", &next, 20).unwrap();
    assert_eq!(recovered.balance(&scope).unwrap(), 55);
}
