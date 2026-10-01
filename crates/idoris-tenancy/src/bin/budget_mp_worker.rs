//! Worker for the multi-process budget-reserve concurrency test
//! (`tests/budget_concurrency.rs`): opens its own connection to a shared
//! SQLite file (given as argv[1]) and performs exactly one `reserve` for
//! the given scope/amount, then reports the outcome on stdout. A real OS
//! process boundary is the only way to exercise SQLite's cross-process
//! locking (`BEGIN IMMEDIATE` + `busy_timeout`) rather than Rust-level
//! in-process synchronization, which is why this is a separate binary
//! instead of a spawned thread.
//!
//! argv: `<db_path> <tenant> <key> <provider> <model> <amount_minor> <go_file>`.
//! Before reserving, the worker busy-waits for `go_file` to exist — the
//! parent test spawns every worker first (each blocked here) and only then
//! creates the file, so all workers race `reserve` starting as close to
//! simultaneously as the OS scheduler allows, instead of being staggered by
//! spawn order.

use std::time::Duration;

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Price};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [
        _,
        db_path,
        tenant_id,
        key_id,
        provider_id,
        model_id,
        amount_minor,
        go_file,
    ] = args.as_slice()
    else {
        eprintln!(
            "usage: budget_mp_worker <db> <tenant> <key> <provider> <model> <amount_minor> <go_file>"
        );
        std::process::exit(2);
    };
    let amount_minor: i64 = match amount_minor.parse() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("invalid amount_minor {amount_minor:?}: {err}");
            std::process::exit(2);
        }
    };

    let go_path = std::path::Path::new(go_file);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !go_path.exists() {
        if std::time::Instant::now() >= deadline {
            eprintln!("timed out waiting for go file {go_file}");
            std::process::exit(3);
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let scope = BudgetScope::new(tenant_id, key_id, provider_id, model_id);
    let ledger = match BudgetLedger::open(db_path) {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!("open failed: {err}");
            std::process::exit(1);
        }
    };
    match ledger.reserve(&scope, Price::Known(amount_minor)) {
        Ok(id) => println!("RESERVED {id}"),
        Err(BudgetError::Exceeded { .. }) => println!("EXCEEDED"),
        Err(err) => {
            // Anything other than a clean success or a structured budget
            // rejection is a bug, not an expected outcome — fail loudly
            // instead of letting the parent test's string matching quietly
            // misclassify it.
            eprintln!("unexpected reserve error: {err}");
            std::process::exit(1);
        }
    }
}
