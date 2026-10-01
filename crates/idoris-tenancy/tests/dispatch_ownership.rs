//! Cross-instance dispatch ownership and cancellation recovery regressions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::BufRead,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price};
use rusqlite::{Connection, TransactionBehavior};

struct TempDb(std::path::PathBuf);
impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.with_added_extension("dispatch-locks"));
        for suffix in [
            "",
            "-wal",
            "-shm",
            ".settlements.sqlite3",
            ".settlements.sqlite3-wal",
            ".settlements.sqlite3-shm",
        ] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}
fn db(tag: &str) -> TempDb {
    TempDb(std::env::temp_dir().join(format!(
        "idoris-dispatch-ownership-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}
fn scope(tenant: &str) -> BudgetScope {
    BudgetScope::new(tenant, "key", "provider", "model")
}
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
fn open(path: &TempDb, clock: Arc<TestClock>, ttl: i64) -> BudgetLedger {
    BudgetLedger::open_with_busy_timeout(path, clock, ttl, Duration::ZERO).expect("open ledger")
}
fn pending_releases(path: &TempDb) -> i64 {
    let sidecar = path.as_ref().with_added_extension("settlements.sqlite3");
    Connection::open(sidecar)
        .unwrap()
        .query_row("SELECT count(*) FROM pending_releases", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn independent_ledgers_admit_live_dispatch_and_retain_its_hold_past_ttl() {
    let path = db("parallel-live");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let a = open(&path, clock.clone(), 100);
    let b = open(&path, clock.clone(), 100);
    let s = scope("tenant");
    a.configure(&s, 100, "UTC").unwrap();
    let aid = a.reserve(&s, Price::Known(40)).unwrap();
    a.begin_settlement("tenant", &aid).unwrap();
    let bid = b.reserve(&s, Price::Known(40)).unwrap();
    b.begin_settlement("tenant", &bid).unwrap();
    clock.0.store(101, Ordering::SeqCst);
    assert_eq!(b.balance(&s).unwrap(), 20);
    assert_eq!(a.balance(&s).unwrap(), 20);
}

#[test]
fn dispatch_claim_is_exclusive_and_foreign_release_cannot_queue_cancellation() {
    let path = db("exclusive-owner");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let a = open(&path, clock.clone(), 60_000);
    let b = open(&path, clock, 60_000);
    let s = scope("tenant");
    a.configure(&s, 100, "UTC").unwrap();
    let id = a.reserve(&s, Price::Known(30)).unwrap();
    a.begin_settlement("tenant", &id).unwrap();
    assert!(matches!(
        a.begin_settlement("tenant", &id),
        Err(BudgetError::Storage(_))
    ));
    assert!(matches!(
        b.begin_settlement("tenant", &id),
        Err(BudgetError::Storage(_))
    ));
    assert!(b.release("tenant", &id).is_err());
    assert_eq!(
        pending_releases(&path),
        0,
        "foreign release must not leave executable work"
    );
    assert!(a.settle_durable("tenant", &id, 25).unwrap().is_some());
    assert_eq!(a.balance(&s).unwrap(), 75);
}

#[test]
fn child_process_holds_dispatch_owner_until_exit() {
    let Ok(path) = std::env::var("IDORIS_DISPATCH_OWNER_CHILD_DB") else {
        return;
    };
    let ledger = BudgetLedger::open(&path).expect("child opens ledger");
    let s = scope("tenant");
    ledger
        .configure(&s, 100, "UTC")
        .expect("child configures tenant");
    let id = ledger.reserve(&s, Price::Known(30)).expect("child reserve");
    ledger
        .begin_settlement("tenant", &id)
        .expect("child claims dispatch");
    println!("OWNERID:{}", id.0);
    let mut byte = [0];
    std::io::Read::read_exact(&mut std::io::stdin(), &mut byte).expect("parent releases child");
}

struct ChildGuard(Option<std::process::Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn another_process_cannot_claim_live_dispatch_and_keeps_unknown_outcome_after_owner_exit() {
    let path = db("process-owner");
    let mut child = ChildGuard(Some(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child_process_holds_dispatch_owner_until_exit",
                "--nocapture",
            ])
            .env("IDORIS_DISPATCH_OWNER_CHILD_DB", path.as_ref())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child test process"),
    ));
    let child_ref = child.0.as_mut().unwrap();
    let mut stdout = std::io::BufReader::new(child_ref.stdout.take().unwrap());
    let id = loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).expect("read child readiness"),
            0,
            "child exited before claiming dispatch"
        );
        if let Some(id) = line.trim().strip_prefix("OWNERID:") {
            break idoris_tenancy::budget::ReservationId(id.to_owned());
        }
    };
    let parent = BudgetLedger::open(&path).unwrap();
    assert!(
        parent.reserve(&scope("tenant"), Price::Known(1)).is_ok(),
        "live foreign dispatch must not block healthy same-tenant traffic"
    );
    assert!(matches!(
        parent.begin_settlement("tenant", &id),
        Err(BudgetError::Storage(_))
    ));
    child_ref.kill().unwrap();
    assert!(!child.0.take().unwrap().wait().unwrap().success());
    assert!(
        parent.begin_settlement("tenant", &id).is_err(),
        "dead owner leaves unknown outcome fail-closed"
    );
    assert!(parent.release("tenant", &id).is_err());
    assert!(matches!(
        parent.reserve(&scope("tenant"), Price::Known(1)),
        Err(BudgetError::Storage(_))
    ));
    let other = scope("other");
    parent.configure(&other, 100, "UTC").unwrap();
    assert!(parent.reserve(&other, Price::Known(1)).is_ok());
    drop(parent);
    let restarted = BudgetLedger::open(&path).unwrap();
    assert!(restarted.release("tenant", &id).is_err());
    assert!(matches!(
        restarted.reserve(&scope("tenant"), Price::Known(1)),
        Err(BudgetError::Storage(_))
    ));
}

#[test]
fn failed_durable_write_cannot_be_wiped_by_foreign_release_and_owner_retry_charges_once() {
    let path = db("actual-versus-release");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let a = open(&path, clock.clone(), 60_000);
    let b = open(&path, clock, 60_000);
    let s = scope("tenant");
    a.configure(&s, 100, "UTC").unwrap();
    let id = a.reserve(&s, Price::Known(30)).unwrap();
    a.begin_settlement("tenant", &id).unwrap();

    let mut primary = Connection::open(path.as_ref()).unwrap();
    let primary_tx = primary
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .unwrap();
    let sidecar_path = path.as_ref().with_added_extension("settlements.sqlite3");
    let mut sidecar = Connection::open(sidecar_path).unwrap();
    let sidecar_tx = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(a.settle_durable("tenant", &id, 27).is_err());
    drop(sidecar_tx);
    drop(primary_tx);

    assert!(
        b.release("tenant", &id).is_err(),
        "another ledger cannot confirm cancellation"
    );
    assert_eq!(pending_releases(&path), 0);
    assert!(
        matches!(b.reserve(&s, Price::Known(1)), Err(BudgetError::Storage(_))),
        "a completed but unpersisted result must not look live"
    );
    assert!(b.begin_settlement("tenant", &id).is_err());
    assert_eq!(a.settle_durable("tenant", &id, 27).unwrap(), Some(27));
    assert_eq!(a.balance(&s).unwrap(), 73);
    a.settle_durable("tenant", &id, 27).unwrap();
    a.retry_settlements().unwrap();
    assert_eq!(a.balance(&s).unwrap(), 73);
}

#[test]
fn foreign_instance_cannot_settle_claimed_dispatch_before_owner_records_actual() {
    let path = db("foreign-settlement-owner");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let a = open(&path, clock.clone(), 60_000);
    let b = open(&path, clock, 60_000);
    let s = scope("tenant");
    a.configure(&s, 100, "UTC").unwrap();
    let id = a.reserve(&s, Price::Known(30)).unwrap();
    a.begin_settlement("tenant", &id).unwrap();

    assert!(matches!(
        b.settle_durable("tenant", &id, 0),
        Err(BudgetError::Storage(_))
    ));
    let sidecar_path = path.as_ref().with_added_extension("settlements.sqlite3");
    let mut sidecar = Connection::open(&sidecar_path).unwrap();
    let sidecar_tx = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(b.settle_durable("tenant", &id, 0).is_err());
    drop(sidecar_tx);
    assert!(matches!(
        b.settle_durable("tenant", &id, 0),
        Err(BudgetError::Storage(_))
    ));
    let (status, actual): (String, Option<i64>) = Connection::open(path.as_ref())
        .unwrap()
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let intents: i64 = Connection::open(sidecar_path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM settlement_intents WHERE reservation_id=?1",
            [&id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "active");
    assert_eq!(actual, None);
    assert_eq!(
        intents, 1,
        "foreign result must preserve the dispatch intent"
    );
    assert_eq!(
        a.balance(&s).unwrap(),
        70,
        "the original hold remains reserved"
    );

    assert_eq!(a.settle_durable("tenant", &id, 27).unwrap(), Some(27));
    assert_eq!(a.settle_durable("tenant", &id, 27).unwrap(), Some(27));
    a.retry_settlements().unwrap();
    assert_eq!(a.balance(&s).unwrap(), 73);
    let (status, actual): (String, Option<i64>) = Connection::open(path.as_ref())
        .unwrap()
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), actual), ("settled", Some(27)));
}

#[test]
fn unclaimed_settlement_insert_failure_does_not_write_primary_fallback() {
    let path = db("unclaimed-insert-failure");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = open(&path, clock, 60_000);
    let s = scope("tenant");
    ledger.configure(&s, 100, "UTC").unwrap();
    let id = ledger.reserve(&s, Price::Known(30)).unwrap();

    let sidecar_path = path.as_ref().with_added_extension("settlements.sqlite3");
    let sidecar = Connection::open(&sidecar_path).unwrap();
    sidecar
        .execute_batch(
            "CREATE TRIGGER fail_pending_insert
             BEFORE INSERT ON pending_settlements
             BEGIN SELECT RAISE(ABORT, 'injected journal insert failure'); END;",
        )
        .unwrap();
    assert!(ledger.settle_durable("tenant", &id, 27).is_err());
    sidecar
        .execute_batch("DROP TRIGGER fail_pending_insert")
        .unwrap();

    ledger.retry_settlements().unwrap();
    let (status, actual): (String, Option<i64>) = Connection::open(path.as_ref())
        .unwrap()
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), actual), ("active", None));
    assert_eq!(ledger.balance(&s).unwrap(), 70);
}

#[test]
fn unresolved_intent_survives_owner_restart_and_blocks_only_its_tenant() {
    let path = db("restart-fail-closed");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let owner = open(&path, clock.clone(), 60_000);
    let s = scope("tenant");
    owner.configure(&s, 100, "UTC").unwrap();
    let id = owner.reserve(&s, Price::Known(30)).unwrap();
    owner.begin_settlement("tenant", &id).unwrap();

    let mut primary = Connection::open(path.as_ref()).unwrap();
    let primary_tx = primary
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .unwrap();
    let sidecar_path = path.as_ref().with_added_extension("settlements.sqlite3");
    let mut sidecar = Connection::open(sidecar_path).unwrap();
    let sidecar_tx = sidecar
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(owner.settle_durable("tenant", &id, 27).is_err());
    drop(sidecar_tx);
    drop(primary_tx);
    let foreign = open(&path, clock.clone(), 60_000);
    assert!(foreign.release("tenant", &id).is_err());
    assert_eq!(pending_releases(&path), 0);
    drop(foreign);
    drop(owner);

    let recovered = open(&path, clock, 60_000);
    assert!(recovered.release("tenant", &id).is_err());
    assert!(matches!(
        recovered.reserve(&s, Price::Known(1)),
        Err(BudgetError::Storage(_))
    ));
    let other = scope("other-tenant");
    recovered.configure(&other, 100, "UTC").unwrap();
    assert!(recovered.reserve(&other, Price::Known(1)).is_ok());
    assert_eq!(pending_releases(&path), 0);
}
