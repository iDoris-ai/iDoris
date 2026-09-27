//! B-1 (Opus Tier-2 re-review): `SpendGate` was defined `pub` in
//! `ledger.rs` but never re-exported from `budget/mod.rs`, so an external
//! crate could not name it at all — `configure_tenant` was effectively
//! uncallable from outside this crate despite being `pub`. This is a
//! regression test living in `tests/` specifically because that's a
//! separate compilation unit that only sees this crate's public API,
//! exactly like a real downstream caller.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Price, SpendGate};

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
        "idoris-tenancy-budget-public-api-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

#[test]
fn configure_tenant_is_callable_from_outside_the_crate() {
    let path = temp_db_path("configure-tenant-public");
    let ledger = BudgetLedger::open(&path).expect("open");
    let scope = BudgetScope::new("acme-co", "key-1", "openai", "gpt-5");
    ledger.configure(&scope, 1_000, "UTC").expect("configure");
    ledger
        .configure_tenant("acme-co", 1_000, "UTC", SpendGate::All)
        .expect("configure_tenant");
    assert_eq!(
        ledger.tenant_balance("acme-co").expect("tenant_balance"),
        1_000
    );
    assert!(ledger.reserve(&scope, Price::Known(100)).is_ok());
}
