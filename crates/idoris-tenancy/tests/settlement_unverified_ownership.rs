//! Security regressions for separating verified dispatch outcomes from
//! unverified caller supplied recovery amounts.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{path::Path, sync::Arc, time::Duration};

use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Clock, Price};
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
        "idoris-unverified-settlement-{}-{}.sqlite3",
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

fn pending(path: &TempDb, id: &str) -> Option<(String, i64)> {
    Connection::open(sidecar(path))
        .unwrap()
        .query_row(
            "SELECT tenant_id, actual_cost_minor FROM pending_settlements WHERE reservation_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .unwrap()
}

fn actual(path: &TempDb, id: &str) -> Option<i64> {
    Connection::open(path.as_ref())
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
        .flatten()
}

#[test]
fn verified_outcome_commits_before_unverified_primary_check_and_recovers_once() {
    let path = db();
    let owner = open(&path);
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    owner.configure(&scope, 100, "UTC").unwrap();
    let verified_id = owner.reserve(&scope, Price::Known(30)).unwrap();
    let unclaimed_id = owner.reserve(&scope, Price::Known(20)).unwrap();
    owner.begin_settlement("tenant", &verified_id).unwrap();

    let primary = Connection::open(path.as_ref()).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations RENAME TO reservations_hidden;")
        .unwrap();
    drop(primary);

    let mut journal = Connection::open(sidecar(&path)).unwrap();
    journal.busy_timeout(Duration::ZERO).unwrap();
    let journal_lock = journal
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();

    assert!(owner.settle_durable("tenant", &verified_id, 31).is_err());
    assert!(owner.settle_durable("tenant", &unclaimed_id, 7).is_err());

    // Once sidecar locking clears, retry can verify neither primary row. It
    // must still commit A's already verified result before B's unverified
    // lookup fails.
    drop(journal_lock);
    assert!(owner.retry_settlements().is_err());
    assert_eq!(pending(&path, &verified_id.0), Some(("tenant".into(), 31)));

    drop(journal);
    drop(owner);
    let primary = Connection::open(path.as_ref()).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations_hidden RENAME TO reservations;")
        .unwrap();
    drop(primary);

    let recovered = open(&path);
    recovered.retry_settlements().unwrap();
    recovered.retry_settlements().unwrap();
    assert_eq!(actual(&path, &verified_id.0), Some(31));
    assert_eq!(actual(&path, &unclaimed_id.0), None);
    assert_eq!(pending(&path, &verified_id.0), None);
    // A charged 31 and B's 20-unit hold remains reserved.
    assert_eq!(recovered.balance(&scope).unwrap(), 49);
    recovered.retry_settlements().unwrap();
    assert_eq!(recovered.balance(&scope).unwrap(), 49);
}

#[test]
fn unverified_foreign_instance_cannot_settle_another_instances_claim() {
    let path = db();
    let owner = open(&path);
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    owner.configure(&scope, 100, "UTC").unwrap();
    let id = owner.reserve(&scope, Price::Known(30)).unwrap();
    owner.begin_settlement("tenant", &id).unwrap();
    let foreign = open(&path);

    let primary = Connection::open(path.as_ref()).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations RENAME TO reservations_hidden;")
        .unwrap();
    drop(primary);

    assert!(foreign.settle_durable("tenant", &id, 1).is_err());

    let primary = Connection::open(path.as_ref()).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations_hidden RENAME TO reservations;")
        .unwrap();
    drop(primary);

    // Recovery can verify the tenant, but the durable claim belongs to the
    // other ledger instance. Its unverified amount must be discarded without
    // entering pending_settlements or removing that owner claim.
    foreign.retry_settlements().unwrap();
    assert_eq!(pending(&path, &id.0), None);
    assert_eq!(actual(&path, &id.0), None);
    let owner_claims: i64 = Connection::open(sidecar(&path))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM dispatch_owners WHERE reservation_id=?1 AND tenant_id='tenant'",
            [&id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(owner_claims, 1);
    assert_eq!(foreign.balance(&scope).unwrap(), 70);

    assert_eq!(owner.settle_durable("tenant", &id, 31).unwrap(), Some(31));
    assert_eq!(actual(&path, &id.0), Some(31));
    assert_eq!(pending(&path, &id.0), None);
    foreign.retry_settlements().unwrap();
    assert_eq!(owner.balance(&scope).unwrap(), 69);
    assert_eq!(foreign.balance(&scope).unwrap(), 69);
    drop(owner);
    drop(foreign);
}

#[test]
fn legacy_emergency_cost_blocks_new_claim_and_conflicting_submission() {
    let path = db();
    let ledger = open(&path);
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    ledger.configure(&scope, 100, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    Connection::open(path.as_ref())
        .unwrap()
        .execute(
            "INSERT INTO budget_emergency_settlements VALUES (?1, 'tenant', 31)",
            [&id.0],
        )
        .unwrap();
    assert!(ledger.begin_settlement("tenant", &id).is_err());
    assert!(ledger.settle_durable("tenant", &id, 1).is_err());
    assert!(ledger.settle("tenant", &id, 1).is_err());
    assert!(ledger.release("tenant", &id).is_err());
    drop(ledger);
    let recovered = open(&path);
    recovered.retry_settlements().unwrap();
    recovered.retry_settlements().unwrap();
    assert_eq!(actual(&path, &id.0), Some(31));
    assert_eq!(recovered.balance(&scope).unwrap(), 69);
    let count: i64 = Connection::open(path.as_ref())
        .unwrap()
        .query_row(
            "SELECT count(*) FROM budget_emergency_settlements",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn unclaimed_unverified_cost_is_recovered_after_owner_lookup_recovers() {
    let path = db();
    let ledger = open(&path);
    let scope = BudgetScope::new("tenant", "key", "provider", "model");
    ledger.configure(&scope, 100, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();
    let primary = Connection::open(path.as_ref()).unwrap();
    primary
        .execute_batch("ALTER TABLE reservations RENAME TO reservations_hidden")
        .unwrap();
    assert!(ledger.settle_durable("tenant", &id, 31).is_err());
    primary
        .execute_batch("ALTER TABLE reservations_hidden RENAME TO reservations")
        .unwrap();
    assert!(ledger.release("tenant", &id).is_err());
    assert!(ledger.settle("tenant", &id, 1).is_err());
    assert!(ledger.begin_settlement("tenant", &id).is_err());
    ledger.retry_settlements().unwrap();
    ledger.retry_settlements().unwrap();
    assert_eq!(actual(&path, &id.0), Some(31));
    assert_eq!(ledger.balance(&scope).unwrap(), 69);
    drop(ledger);
    let reopened = open(&path);
    assert_eq!(reopened.balance(&scope).unwrap(), 69);
}
