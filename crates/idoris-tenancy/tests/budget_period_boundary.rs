//! Billing-period boundary at the ledger level: settled spend in one period
//! must not carry over into the next, and two timestamps in the same period
//! must share the same spend bucket. Complements the pure
//! `billing_period_key` unit tests in `budget/period.rs` by exercising the
//! full `configure`/`reserve`/`settle`/`balance` path through
//! [`BudgetLedger`], with a fake clock so the boundary can be crossed
//! without sleeping real time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{TimeZone, Utc};
use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Clock, Price};

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

    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
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
        "idoris-tenancy-budget-period-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

fn ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("valid fixture datetime")
        .timestamp_millis()
}

#[test]
fn spend_does_not_carry_across_a_period_boundary() {
    let path = temp_db_path("no-carryover");
    // 2026-08-31T10:00:00Z is 2026-08-31 17:00 in Asia/Bangkok (UTC+7, no
    // DST) — still August there too, so this is safely mid-period.
    let mid_august = ms(2026, 8, 31, 10, 0);
    // 2026-08-31T20:00:00Z is 2026-09-01 03:00 in Asia/Bangkok — the same
    // "换台机器账单就变" trap `budget/period.rs`'s tests exercise at the
    // pure-function level, here at the ledger level.
    let just_after_bangkok_midnight = ms(2026, 8, 31, 20, 0);

    let clock = FakeClock::new(mid_august);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), 60_000).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger
        .configure(&scope, 1_000, "Asia/Bangkok")
        .expect("configure");

    // Spend in the August (Bangkok) period.
    let id = ledger.reserve(&scope, Price::Known(900)).expect("reserve");
    ledger.settle(&id, 900).expect("settle");
    assert_eq!(ledger.balance(&scope).expect("balance"), 100);

    // Cross into the September (Bangkok) period.
    clock.set(just_after_bangkok_midnight);
    assert_eq!(
        ledger.balance(&scope).expect("balance"),
        1_000,
        "a new billing period must start with the full limit, regardless of \
         what was spent in the previous one"
    );
}

/// Negative control: two timestamps that stay within the same month (a day
/// apart) must keep sharing the same spend bucket — the ledger must not
/// treat every distinct timestamp as its own period.
#[test]
fn spend_within_the_same_period_accumulates() {
    let path = temp_db_path("same-period-accumulates");
    let day_one = ms(2026, 9, 10, 8, 0);
    let day_two = ms(2026, 9, 11, 8, 0);

    let clock = FakeClock::new(day_one);
    let ledger = BudgetLedger::open_with(&path, clock.clone(), 60_000).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger
        .configure(&scope, 1_000, "Asia/Bangkok")
        .expect("configure");

    let id1 = ledger
        .reserve(&scope, Price::Known(400))
        .expect("reserve 1");
    ledger.settle(&id1, 400).expect("settle 1");

    clock.set(day_two);
    let id2 = ledger
        .reserve(&scope, Price::Known(400))
        .expect("reserve 2");
    ledger.settle(&id2, 400).expect("settle 2");

    assert_eq!(
        ledger.balance(&scope).expect("balance"),
        200,
        "both settles should have landed in the same period bucket"
    );
}
