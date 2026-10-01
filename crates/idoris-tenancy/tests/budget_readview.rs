//! Tenant budget read views expose one consistent, tenant-scoped snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price, SpendGate};

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TempDb(std::path::PathBuf);

impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

fn setup(tag: &str) -> (TempDb, BudgetLedger) {
    let path = TempDb(std::env::temp_dir().join(format!(
        "idoris-budget-readview-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )));
    // 2026-08-31 18:00 UTC is already 2026-09-01 in Asia/Bangkok.
    let clock = Arc::new(FakeClock(AtomicI64::new(1_788_199_200_000)));
    let ledger = BudgetLedger::open_with(&path, clock, 60_000).expect("open ledger");
    (path, ledger)
}

#[test]
fn reports_config_and_distinguishes_remaining_from_available() {
    let (_path, ledger) = setup("values");
    let scope = BudgetScope::new("acme", "key", "openai", "model");
    ledger
        .configure(&scope, 900, "Asia/Bangkok")
        .expect("scope config");
    ledger
        .configure_tenant("acme", 1_000, "Asia/Bangkok", SpendGate::All)
        .expect("tenant config");

    let initial = ledger.tenant_readview("acme").expect("initial view");
    assert_eq!(initial.tenant_id, "acme");
    assert_eq!(initial.limit_minor, 1_000);
    assert_eq!(initial.spent_minor, 0);
    assert_eq!(initial.reserved_minor, 0);
    assert_eq!(initial.remaining_minor, 1_000);
    assert_eq!(initial.available_minor, 1_000);
    assert_eq!(initial.billing_timezone, "Asia/Bangkok");
    assert_eq!(initial.scope, SpendGate::All);
    assert_eq!(initial.period, "2026-09");

    let id = ledger.reserve(&scope, Price::Known(300)).expect("reserve");
    let held = ledger.tenant_readview("acme").expect("reserved view");
    assert_eq!((held.spent_minor, held.reserved_minor), (0, 300));
    assert_eq!(held.remaining_minor, 1_000);
    assert_eq!(held.available_minor, 700);

    ledger.settle("acme", &id, 250).expect("settle");
    let settled = ledger.tenant_readview("acme").expect("settled view");
    assert_eq!((settled.spent_minor, settled.reserved_minor), (250, 0));
    assert_eq!(
        (settled.remaining_minor, settled.available_minor),
        (750, 750)
    );
    assert!(matches!(
        ledger.settle("acme", &id, 250),
        Err(BudgetError::ReservationNotActive { .. })
    ));
    assert_eq!(
        ledger.tenant_readview("acme").expect("unchanged view"),
        settled
    );
}

#[test]
fn same_scope_dimensions_remain_isolated_between_tenants() {
    let (_path, ledger) = setup("isolation");
    let a = BudgetScope::new("tenant-a", "same-key", "openai", "same-model");
    let b = BudgetScope::new("tenant-b", "same-key", "openai", "same-model");
    for (tenant, scope) in [("tenant-a", &a), ("tenant-b", &b)] {
        ledger.configure(scope, 1_000, "UTC").expect("scope config");
        ledger
            .configure_tenant(tenant, 1_000, "UTC", SpendGate::PaidOnly)
            .expect("tenant config");
    }
    let id = ledger.reserve(&a, Price::Known(400)).expect("reserve A");
    ledger.settle("tenant-a", &id, 350).expect("settle A");
    ledger
        .reserve(&a, Price::Known(200))
        .expect("leave reservation active for A");

    let view_a = ledger.tenant_readview("tenant-a").expect("view A");
    let view_b = ledger.tenant_readview("tenant-b").expect("view B");
    assert_eq!((view_a.spent_minor, view_a.reserved_minor), (350, 200));
    assert_eq!((view_a.remaining_minor, view_a.available_minor), (650, 450));
    assert_eq!((view_b.spent_minor, view_b.reserved_minor), (0, 0));
    assert_eq!(view_b.available_minor, 1_000);
    assert_eq!(view_b.scope, SpendGate::PaidOnly);
    assert_eq!(view_b.period, "2026-08");
}

#[test]
fn blank_and_unconfigured_tenants_return_errors() {
    let (_path, ledger) = setup("errors");
    assert!(matches!(
        ledger.tenant_readview("  "),
        Err(BudgetError::InvalidScope { field: "tenant_id" })
    ));
    assert!(matches!(
        ledger.tenant_readview("missing"),
        Err(BudgetError::TenantNotConfigured { .. })
    ));
}

#[test]
fn unavailable_storage_returns_an_error_instead_of_a_zero_view() {
    let (path, ledger) = setup("storage-error");
    ledger
        .configure_tenant("acme", 1_000, "UTC", SpendGate::PaidOnly)
        .expect("tenant config");
    let connection = rusqlite::Connection::open(&path).expect("second connection");
    connection
        .execute_batch("DROP TABLE tenant_config")
        .expect("remove config table");

    assert!(matches!(
        ledger.tenant_readview("acme"),
        Err(BudgetError::Storage(_))
    ));
}
