//! Reservation TTL: an abandoned reservation (caller crashed before
//! `settle`/`release`) must not lock up budget forever. Uses a fake
//! [`Clock`] so the test doesn't sleep real wall-clock time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price};

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl FakeClock {
    fn new(start_ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(start_ms)))
    }

    fn advance(&self, delta_ms: i64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

/// Temp SQLite path that deletes the database and its `-wal`/`-shm`
/// sidecars on drop — including when the test panics.
struct TempDb(std::path::PathBuf);

impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

fn temp_db_path(tag: &str) -> TempDb {
    TempDb(std::env::temp_dir().join(format!(
        "idoris-tenancy-budget-ttl-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

const TTL_MS: i64 = 1_000;

#[test]
fn expired_reservation_frees_its_budget_on_the_next_reserve() {
    let path = temp_db_path("expiry-frees");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    ledger.reserve(&scope, Price::Known(100)).expect("reserve");
    // Negative control: before the TTL elapses, the reservation still holds
    // the budget — a second reserve must fail.
    assert!(ledger.reserve(&scope, Price::Known(1)).is_err());

    clock.advance(TTL_MS + 1);
    // `reserve` sweeps its own scope/period before checking the balance, so
    // the expired reservation no longer counts against it.
    let after_expiry = ledger.reserve(&scope, Price::Known(100));
    assert!(
        after_expiry.is_ok(),
        "expired reservation should have freed the budget"
    );
}

#[test]
fn settling_an_expired_reservation_is_rejected() {
    let path = temp_db_path("expiry-settle");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    clock.advance(TTL_MS + 1);
    assert!(matches!(
        ledger.settle(&id, 50),
        Err(BudgetError::ReservationNotActive { .. })
    ));
}

#[test]
fn releasing_an_expired_reservation_is_rejected() {
    let path = temp_db_path("expiry-release");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    clock.advance(TTL_MS + 1);
    assert!(matches!(
        ledger.release(&id),
        Err(BudgetError::ReservationNotActive { .. })
    ));
}

#[test]
fn sweep_expired_reports_and_flips_expired_reservations() {
    let path = temp_db_path("sweep");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    // Negative control: nothing to sweep before the TTL elapses.
    assert_eq!(ledger.sweep_expired().expect("sweep"), 0);

    clock.advance(TTL_MS + 1);
    assert_eq!(ledger.sweep_expired().expect("sweep"), 1);
    // Idempotent: a second sweep finds nothing left to flip.
    assert_eq!(ledger.sweep_expired().expect("sweep"), 0);
}
