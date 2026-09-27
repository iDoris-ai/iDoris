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

/// H1 (Opus Tier-2 acceptance): settling a reservation whose TTL has
/// already lapsed must still charge — real money may already have been
/// spent by a slow upstream call, and the TTL exists to free budget held by
/// *abandoned* calls, not to make a late-but-real charge disappear.
/// `SettleReceipt::late` reports it happened past the deadline.
#[test]
fn settling_an_expired_reservation_still_charges_and_is_marked_late() {
    let path = temp_db_path("expiry-settle");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    clock.advance(TTL_MS + 1);
    let receipt = ledger
        .settle(&scope.tenant_id, &id, 50)
        .expect("settle after expiry must still charge");
    assert!(receipt.late);
    assert_eq!(ledger.balance(&scope).expect("balance"), 50);
}

/// Negative control: settling an expired reservation *twice* still only
/// charges once — the first settle finalizes it (`status='settled'`), so a
/// second attempt is rejected exactly like the non-expired double-settle
/// case.
#[test]
fn settling_an_expired_reservation_twice_only_charges_once() {
    let path = temp_db_path("expiry-settle-twice");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    clock.advance(TTL_MS + 1);
    ledger
        .settle(&scope.tenant_id, &id, 50)
        .expect("first settle");
    assert!(matches!(
        ledger.settle(&scope.tenant_id, &id, 50),
        Err(BudgetError::ReservationNotActive { .. })
    ));
    assert_eq!(ledger.balance(&scope).expect("balance"), 50);
}

/// L3 (Opus Tier-2 acceptance): releasing an already-expired reservation is
/// `Ok(())`, not an error — no charge was ever recorded for it, so
/// "release" (no charge is due) is already true.
#[test]
fn releasing_an_expired_reservation_succeeds() {
    let path = temp_db_path("expiry-release");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    clock.advance(TTL_MS + 1);
    assert!(ledger.release(&scope.tenant_id, &id).is_ok());
    // No charge was recorded.
    assert_eq!(ledger.balance(&scope).expect("balance"), 100);
}

/// Negative control: once released (even post-expiry), settling it must
/// still be rejected — release is a terminal outcome too.
#[test]
fn settling_after_releasing_an_expired_reservation_is_rejected() {
    let path = temp_db_path("expiry-release-then-settle");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    clock.advance(TTL_MS + 1);
    ledger.release(&scope.tenant_id, &id).expect("release");
    assert!(matches!(
        ledger.settle(&scope.tenant_id, &id, 50),
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

/// `extend` renews a still-active reservation's deadline so a slower-than-
/// anticipated upstream call doesn't lose its held budget to the TTL.
#[test]
fn extend_renews_an_active_reservations_deadline() {
    let path = temp_db_path("extend-renews");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    // Extend well before the original TTL would lapse.
    ledger
        .extend(&scope.tenant_id, &id, TTL_MS * 10)
        .expect("extend");

    // Advance past the *original* deadline — the extend should have pushed
    // it out, so settling now must not be `late`.
    clock.advance(TTL_MS + 1);
    let receipt = ledger
        .settle(&scope.tenant_id, &id, 50)
        .expect("settle before the extended deadline");
    assert!(!receipt.late, "extend should have prevented a late settle");
}

/// Negative control: `extend` on an already-settled reservation is
/// rejected — extending a finalized reservation makes no sense.
#[test]
fn extend_on_settled_reservation_errors() {
    let path = temp_db_path("extend-settled");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    ledger.settle(&scope.tenant_id, &id, 50).expect("settle");
    assert!(matches!(
        ledger.extend(&scope.tenant_id, &id, 1_000),
        Err(BudgetError::ReservationNotActive { .. })
    ));
}

/// Negative control: a non-positive extension is rejected, same
/// fail-closed reasoning as the original TTL validation.
#[test]
fn extend_rejects_non_positive_ttl() {
    let path = temp_db_path("extend-bad-ttl");
    let clock = FakeClock::new(0);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), TTL_MS).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 100, "UTC").expect("configure");

    let id = ledger.reserve(&scope, Price::Known(50)).expect("reserve");
    assert!(matches!(
        ledger.extend(&scope.tenant_id, &id, 0),
        Err(BudgetError::InvalidTtl { .. })
    ));
}
