//! M4/B-8 (Opus Tier-2 re-review): a deterministic — no manual source
//! edits, no sleep-to-widen-the-race-window — proof that
//! `tests/budget_concurrency.rs`'s exact-count assertions would actually
//! catch a "split the atomic check-and-deduct transaction" regression, the
//! LiteLLM #32614 bug class this crate exists to prevent.
//!
//! Isolated in its own test binary (see `Cargo.toml`'s `[[test]]` entry and
//! its doc comment for why): the hook is process-global state that must not
//! leak into the other concurrency tests running in the same process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Price, test_hooks};

const THREADS: i64 = 16;
const PER_RESERVE_MINOR: i64 = 100;
const LIMIT_MINOR: i64 = 1_000;
const EXPECTED_SUCCESSES: i64 = LIMIT_MINOR / PER_RESERVE_MINOR; // 10, if atomic

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
        "idoris-tenancy-budget-mutation-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

/// This test asserts the *opposite* of every correctness test elsewhere in
/// this crate: that overspend actually happens once `reserve`'s atomicity
/// is deliberately broken. That's the point — it's a permanent regression
/// test for the *test suite's* discriminating power, not for the ledger.
/// `test_hooks::arm` forces every one of the `THREADS` participating
/// threads to pass its own balance check (all reading zero prior
/// reservations, since none of them has committed an insert yet) before any
/// of them is allowed to proceed to its insert — deterministically, since
/// the barrier can't release until all `THREADS` have arrived, no sleep or
/// lucky scheduling required.
#[test]
fn split_transaction_mutation_is_caught_without_manual_edits_or_sleeps() {
    test_hooks::arm(THREADS as usize);

    let path = temp_db_path("split-transaction");
    let scope = BudgetScope::new("acme-co", "key-mutation", "openai", "gpt-5");
    {
        let ledger = BudgetLedger::open(&path).expect("open");
        ledger
            .configure(&scope, LIMIT_MINOR, "UTC")
            .expect("configure");
    }

    let barrier = Arc::new(Barrier::new(THREADS as usize));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = path.0.clone();
            let scope = scope.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let ledger = BudgetLedger::open(&path).expect("open in thread");
                barrier.wait();
                ledger.reserve(&scope, Price::Known(PER_RESERVE_MINOR))
            })
        })
        .collect();

    let succeeded = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread panicked"))
        .filter(|r| r.is_ok())
        .count() as i64;

    assert!(
        succeeded > EXPECTED_SUCCESSES,
        "the split-transaction hook should have caused overspend (deterministically, since \
         every thread's check-transaction commits before any insert can happen), but got only \
         {succeeded} successes (a correct, atomic reserve would cap this at {EXPECTED_SUCCESSES})"
    );
}
