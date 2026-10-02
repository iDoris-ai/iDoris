#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Clock, Price, SpendGate};
use rusqlite::Connection;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TestDb(std::path::PathBuf);

impl TestDb {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-settlement-fallback-log-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4()
        )))
    }

    fn sidecar(&self) -> std::path::PathBuf {
        self.0.with_added_extension("settlements.sqlite3")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        for file in [&self.0, &self.sidecar()] {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = file.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[test]
fn sidecar_enqueue_failure_logs_committed_primary_fallback() {
    const CHILD_ENV: &str = "IDORIS_TEST_SETTLEMENT_FALLBACK_LOG_CASE";
    if let Some(case) = std::env::var_os(CHILD_ENV) {
        run_case(case.to_str().unwrap());
        return;
    }

    for (case, expected_fallback) in [("committed", "committed"), ("overage", "committed_overage")]
    {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sidecar_enqueue_failure_logs_committed_primary_fallback",
                "--nocapture",
            ])
            .env(CHILD_ENV, case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child case {case} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8(output.stdout).unwrap();
        let metadata = stdout
            .lines()
            .find(|line| line.starts_with("CASE "))
            .unwrap_or_else(|| panic!("child case {case} did not report metadata: {stdout}"));
        let tenant = field(metadata, "tenant");
        let reservation = field(metadata, "reservation");

        let stderr = String::from_utf8(output.stderr).unwrap();
        let log_line = stderr
            .lines()
            .find(|line| line.contains("budget settlement sidecar enqueue failed:"))
            .unwrap_or_else(|| panic!("child case {case} emitted no fallback log:\n{stderr}"));
        assert!(
            log_line.contains("injected sidecar enqueue failure"),
            "{log_line}"
        );
        assert!(log_line.contains(&format!("tenant={tenant}")), "{log_line}");
        assert!(
            log_line.contains(&format!("reservation={reservation}")),
            "{log_line}"
        );
        assert_eq!(field(log_line, "fallback"), expected_fallback, "{log_line}");
    }
}

fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap()
}

fn run_case(case: &str) {
    let (tenant, actual, expected_fallback) = match case {
        "committed" => ("fallback-tenant-normal", 31, "committed"),
        "overage" => ("fallback-tenant-overage", 201, "committed_overage"),
        other => panic!("unknown child case: {other}"),
    };
    let db = TestDb::new();
    let scope = BudgetScope::new(tenant, "key", "provider", "model");
    let clock = Arc::new(TestClock(AtomicI64::new(0)));
    let ledger = BudgetLedger::open_with(&db.0, clock.clone(), 100).unwrap();
    ledger
        .configure_tenant(tenant, 1_000, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger.configure(&scope, 1_000, "UTC").unwrap();
    let id = ledger.reserve(&scope, Price::Known(40)).unwrap();

    let sidecar = Connection::open(db.sidecar()).unwrap();
    sidecar
        .execute_batch(
            "CREATE TRIGGER fail_pending_insert BEFORE INSERT ON pending_settlements
             BEGIN SELECT RAISE(FAIL, 'injected sidecar enqueue failure'); END;",
        )
        .unwrap();
    drop(sidecar);

    if expected_fallback == "committed" {
        assert_eq!(
            ledger.settle_durable(tenant, &id, actual).unwrap(),
            Some(actual)
        );
    } else {
        assert!(matches!(
            ledger.settle_durable(tenant, &id, actual),
            Err(BudgetError::OverageTooLarge { .. })
        ));
    }
    assert_eq!(ledger.tenant_balance(tenant).unwrap(), 1_000 - actual);
    assert_eq!(ledger.balance(&scope).unwrap(), 1_000 - actual);
    drop(ledger);

    let reopened = BudgetLedger::open_with(&db.0, clock, 100).unwrap();
    assert_eq!(reopened.tenant_balance(tenant).unwrap(), 1_000 - actual);
    assert_eq!(reopened.balance(&scope).unwrap(), 1_000 - actual);
    reopened.retry_settlements().unwrap();
    assert_eq!(reopened.tenant_balance(tenant).unwrap(), 1_000 - actual);
    let primary = Connection::open(&db.0).unwrap();
    let row: (String, i64) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), actual));
    drop(primary);
    drop(reopened);
    println!(
        "CASE tenant={tenant} reservation={} expected={expected_fallback}",
        id.0
    );
}
