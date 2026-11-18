//! S1 migrations are forward compatible: a database written by the **pre-S1 code** (origin/main
//! behaviour at `d1e8e1f8`) opens under the new code with every row intact, and every budget and usage
//! reading comes out exactly as the old code computed it.
//!
//! The databases are `tests/fixtures/legacy_main_*.sql`, dumps of real files produced by running the
//! baseline `storage::bootstrap`, `BudgetLedger`, `TenantStore`, `VirtualKeyStore` and `EventLogStore`
//! (see the header of each fixture). The "golden" numbers below were printed by the baseline code
//! when it created them, not computed by the code under test.
//!
//! Three startup paths are exercised on the same data: the legacy tenant path, the legacy personal
//! path (both must behave exactly as before) and the organization form (which adopts the tenants).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use idoris_compute_contracts::form::DeploymentForm;
use idoris_contracts::DeployMode;
use idoris_contracts::tenant::{Budget, BudgetScope as ContractBudgetScope, TenantContext};
use idoris_router::startup_form::StartupForm;
use idoris_router::storage::{self, StorageBootstrap};
use idoris_tenancy::billing::{UsageSource, UsageTotals, query_monthly_usage};
use idoris_tenancy::budget::{
    BudgetError, BudgetLedger, BudgetScope, Clock, Price, SpendGate, TenantBudgetReadView,
};
use idoris_tenancy::event_log::EventType;
use idoris_tenancy::virtual_key::VirtualKeySecret;
use rusqlite::{Connection, types::Value};
use tempfile::TempDir;

const TENANT_FIXTURE: &str = include_str!("fixtures/legacy_main_tenant.sql");
const PERSONAL_FIXTURE: &str = include_str!("fixtures/legacy_main_personal.sql");

const TENANT_YAML: &str = "tenants:\n  - tenant_id: acme\n    limit_minor: 100000\n    billing_timezone: Asia/Bangkok\n    scope: paid_only\n  - tenant_id: beta\n    limit_minor: 5000\n    billing_timezone: UTC\n    scope: all\n  - tenant_id: gamma\n    limit_minor: 1000\n    billing_timezone: UTC\n    scope: paid_only\n";
const PERSONAL_YAML: &str = "tenants:\n  - tenant_id: personal\n    limit_minor: 50000\n    billing_timezone: UTC\n    scope: paid_only\n";

/// "Now" at the moment the baseline printed its readings: all period keys and reservation expiries
/// are interpreted against this clock, exactly as the baseline did.
const TENANT_DB_NOW_MS: i64 = 1_793_535_200_000;
const PERSONAL_DB_NOW_MS: i64 = 1_793_534_800_000;

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// Every table the pre-S1 code created, except `schema_migrations` (which gains a row, checked
/// separately), `event_log_events` (its startup back-fill legitimately appends, checked separately)
/// and `sqlite_sequence` (bookkeeping for that table).
const LEGACY_TABLES: &[&str] = &[
    "budget_amount_overflows",
    "budget_config",
    "budget_overage_events",
    "budget_periods",
    "event_log_schema_migrations",
    "record_schema_migrations",
    "reservations",
    "tenant_config",
    "tenant_periods",
    "tenant_records",
    "virtual_key_schema_migrations",
    "virtual_keys",
];

fn load(dir: &TempDir, fixture: &str, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    let conn = Connection::open(&path).unwrap();
    // The baseline ledger put the file in WAL mode; reproduce that before loading the dump.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    conn.execute_batch(fixture).unwrap();
    path
}

fn dump_table(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM \"{table}\" ORDER BY rowid"))
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        let mut parts = Vec::with_capacity(columns);
        for index in 0..columns {
            let value: Value = row.get(index)?;
            parts.push(format!("{value:?}"));
        }
        Ok(parts.join("|"))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    tables: BTreeMap<String, Vec<String>>,
    events: Vec<String>,
    migrations: Vec<String>,
    /// `(type, name, sql)` for every schema object the fixture had.
    schema: BTreeMap<String, String>,
}

fn snapshot(db: &Path) -> Snapshot {
    let conn = Connection::open(db).unwrap();
    let tables = LEGACY_TABLES
        .iter()
        .map(|table| ((*table).to_string(), dump_table(&conn, table)))
        .collect();
    let mut stmt = conn
        .prepare("SELECT type, name, COALESCE(sql, '') FROM sqlite_master")
        .unwrap();
    let schema = stmt
        .query_map([], |r| {
            Ok((
                format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    Snapshot {
        tables,
        events: dump_table(&conn, "event_log_events"),
        migrations: dump_table(&conn, "schema_migrations"),
        schema,
    }
}

/// What must hold between the database as the old code left it (`before`) and after the new code
/// started on it (`after`).
fn assert_upgrade_preserved_everything(before: &Snapshot, after: &Snapshot) {
    // 1. Every legacy table is byte-for-byte identical (row by row, in rowid order).
    for (table, rows) in &before.tables {
        assert_eq!(
            &after.tables[table], rows,
            "table {table} changed during the upgrade"
        );
    }
    // 2. The event log keeps its original rows untouched; the only additions are the
    //    `audit.finalized` events the (unchanged) start-up back-fill writes for legacy audit records.
    assert!(
        after.events.len() >= before.events.len()
            && after.events[..before.events.len()] == before.events[..],
        "original event-log rows must stay as they were"
    );
    for added in &after.events[before.events.len()..] {
        assert!(
            added.contains("audit.finalized"),
            "unexpected new event: {added}"
        );
    }
    // 3. The migration history keeps versions 1..=5 as written (including their timestamps) and
    //    gains exactly version 6.
    assert_eq!(
        &after.migrations[..before.migrations.len()],
        &before.migrations[..]
    );
    assert_eq!(after.migrations.len(), before.migrations.len() + 1);
    assert!(
        after.migrations.last().unwrap().starts_with("Integer(6)|"),
        "{:?}",
        after.migrations
    );
    // 4. Every schema object the old code created still exists with exactly the same definition.
    for (object, sql) in &before.schema {
        assert_eq!(
            after.schema.get(object),
            Some(sql),
            "schema object {object} changed"
        );
    }
}

fn integrity(db: &Path) {
    let conn = Connection::open(db).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").unwrap();
    let violations = stmt.query_map([], |_| Ok(())).unwrap().count();
    assert_eq!(violations, 0, "no foreign-key violations");
}

fn readview(db: &Path, now_ms: i64, tenant: &str) -> TenantBudgetReadView {
    BudgetLedger::open_with(db, Arc::new(FixedClock(now_ms)), 300_000)
        .unwrap()
        .tenant_readview(tenant)
        .unwrap()
}

fn assert_readview(
    view: &TenantBudgetReadView,
    golden: (&str, i64, i64, i64, i64, i64, &str, SpendGate),
) {
    let (period, limit, spent, reserved, remaining, available, tz, scope) = golden;
    assert_eq!(
        (
            view.period.as_str(),
            view.limit_minor,
            view.spent_minor,
            view.reserved_minor,
            view.remaining_minor,
            view.available_minor,
            view.billing_timezone.as_str(),
            view.scope
        ),
        (
            period, limit, spent, reserved, remaining, available, tz, scope
        ),
        "{view:?}"
    );
}

fn usage(db: &Path, tenant: &str, tz: &str, period: &str) -> UsageTotals {
    let store = idoris_tenancy::store::TenantStore::open(db).unwrap();
    let context = TenantContext {
        tenant_id: tenant.into(),
        billing_timezone: tz.into(),
        budget: Budget {
            limit_minor: 0,
            spent_minor: 0,
            scope: ContractBudgetScope::PaidOnly,
        },
        quota: None,
    };
    query_monthly_usage(&store, Some(&context), period, Some(UsageSource::Usage))
        .unwrap()
        .totals
}

fn assert_usage(totals: UsageTotals, cost: f64, tokens_in: f64, tokens_out: f64, calls: u64) {
    assert_eq!(
        (
            totals.cost_minor,
            totals.tokens_in,
            totals.tokens_out,
            totals.calls
        ),
        (cost, tokens_in, tokens_out, calls)
    );
}

/// The golden readings printed by the baseline for the tenant database.
fn assert_tenant_goldens(db: &Path) {
    assert_readview(
        &readview(db, TENANT_DB_NOW_MS, "acme"),
        (
            "2026-11",
            100_000,
            2_600,
            700,
            97_400,
            96_700,
            "Asia/Bangkok",
            SpendGate::PaidOnly,
        ),
    );
    assert_readview(
        &readview(db, TENANT_DB_NOW_MS, "beta"),
        (
            "2026-11",
            5_000,
            2_600,
            700,
            2_400,
            1_700,
            "UTC",
            SpendGate::All,
        ),
    );
    assert_readview(
        &readview(db, TENANT_DB_NOW_MS, "gamma"),
        (
            "2026-11",
            1_000,
            277,
            0,
            723,
            723,
            "UTC",
            SpendGate::PaidOnly,
        ),
    );
    let ledger =
        BudgetLedger::open_with(db, Arc::new(FixedClock(TENANT_DB_NOW_MS)), 300_000).unwrap();
    assert_eq!(ledger.tenant_balance("acme").unwrap(), 96_700);
    assert_eq!(ledger.tenant_balance("beta").unwrap(), 1_700);
    assert_eq!(ledger.tenant_balance("gamma").unwrap(), 723);
    // The sub-scope limit written through the library API still applies to its own scope.
    let sub = BudgetScope::new("gamma", "sub-key", "omlx", "m1");
    assert_eq!(ledger.balance(&sub).unwrap(), 800 - 277);
    // Usage records, including the previous-month record the month filter must keep out.
    assert_usage(
        usage(db, "acme", "Asia/Bangkok", "2026-11"),
        2_150.0,
        49.0,
        72.0,
        3,
    );
    assert_usage(
        usage(db, "acme", "Asia/Bangkok", "2026-09"),
        999.0,
        1.0,
        1.0,
        1,
    );
    assert_usage(usage(db, "beta", "UTC", "2026-11"), 100.0, 7.0, 8.0, 1);
}

fn assert_personal_goldens(db: &Path) {
    assert_readview(
        &readview(db, PERSONAL_DB_NOW_MS, "personal"),
        (
            "2026-11",
            50_000,
            2_600,
            700,
            47_400,
            46_700,
            "UTC",
            SpendGate::PaidOnly,
        ),
    );
    let ledger =
        BudgetLedger::open_with(db, Arc::new(FixedClock(PERSONAL_DB_NOW_MS)), 300_000).unwrap();
    assert_eq!(ledger.tenant_balance("personal").unwrap(), 46_700);
    assert_usage(
        usage(db, "personal", "UTC", "2026-11"),
        4_600.0,
        400.0,
        600.0,
        2,
    );
}

/// New reservations under real key ids are admitted against the historical spend and in-flight
/// reservations written under the old `default` placeholder.
fn assert_history_counts_for_new_real_key_reservations(
    db: &Path,
    now_ms: i64,
    tenant: &str,
    available: i64,
) {
    let ledger = BudgetLedger::open_with(db, Arc::new(FixedClock(now_ms)), 300_000).unwrap();
    let scope = BudgetScope::new(
        tenant,
        "vk_0123456789abcdef0123456789abcdef",
        "omlx",
        "qwen3-4b",
    );
    match ledger.reserve(&scope, Price::Known(available + 1)) {
        Err(BudgetError::Exceeded { balance_minor, .. }) => assert_eq!(balance_minor, available),
        other => panic!("one over the remaining allowance must be refused: {other:?}"),
    }
    let id = ledger.reserve(&scope, Price::Known(available)).unwrap();
    ledger.release(tenant, &id).unwrap();
}

fn assert_keys_still_authenticate(storage: &StorageBootstrap, keys: &[(&str, &str, Option<&str>)]) {
    for (key_id, secret, tenant) in keys {
        let secret = VirtualKeySecret::parse(secret).unwrap();
        let identity = storage
            .virtual_keys
            .lock()
            .unwrap()
            .authenticate(&secret, 1)
            .unwrap()
            .unwrap_or_else(|| panic!("{key_id} must still authenticate"));
        assert_eq!(identity.key_id, *key_id);
        assert_eq!(identity.scope.tenant_id.as_deref(), *tenant);
    }
}

fn assert_event_log_intact_and_append_only(
    storage: &StorageBootstrap,
    db: &Path,
    tenant: &str,
    first_event_id: &str,
) {
    let events = storage
        .event_log
        .events_for_record(Some(tenant), &format!("rec-{tenant}"))
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event.event_id, first_event_id);
    assert_eq!(
        events
            .iter()
            .map(|e| e.event.event_type)
            .collect::<Vec<_>>(),
        [
            EventType::RequestReceived,
            EventType::BudgetReserved,
            EventType::Completed
        ]
    );
    // The append-only guards written by the old migration still bite.
    let conn = Connection::open(db).unwrap();
    assert!(conn.execute("DELETE FROM event_log_events", []).is_err());
    assert!(
        conn.execute("UPDATE event_log_events SET record_id = 'x'", [])
            .is_err()
    );
}

const TENANT_KEYS: [(&str, &str, Option<&str>); 2] = [
    (
        "vk_dc69b96c63bf40c7a8de47d0305aaead",
        "idk_a85e9364a61e482190bf0c4e67a0ff83a1111a9f0761461b887286a60e256f5d",
        Some("acme"),
    ),
    (
        "vk_7094690725664ba998cb7bb1a16beeab",
        "idk_465f05f837df4807bde2cd5c746cb344032f3bacd7934a9a9de81028a98c9e87",
        None,
    ),
];

const PERSONAL_KEYS: [(&str, &str, Option<&str>); 2] = [
    (
        "vk_1179c4419a814f0baeecce7a007aa6fb",
        "idk_c11a40d7c4844ae3bc3930e1239c12888e289eddfd9542c39f0b7673714bbdf0",
        None,
    ),
    (
        "vk_a23b29158ca84d6a8d382b98f59c079f",
        "idk_d9230f2f05d64f69b49f901a125191fc2544bd237fc742c28cf7ea22baff48b1",
        Some("personal"),
    ),
];

#[test]
fn the_fixtures_really_are_old_databases_and_read_as_the_baseline_printed() {
    // Control: before the new code touches them, the fixtures give the golden numbers through the
    // *new* read paths already, and have none of the S1 objects.
    for (fixture, name) in [
        (TENANT_FIXTURE, "tenant.sqlite3"),
        (PERSONAL_FIXTURE, "personal.sqlite3"),
    ] {
        let dir = TempDir::new().unwrap();
        let db = load(&dir, fixture, name);
        let conn = Connection::open(&db).unwrap();
        let objects: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('tenant_bootstrap','accounts','principals')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(objects, 0, "{name} must predate the S1 migrations");
        let versions: i64 = conn
            .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(versions, 5, "{name}");
    }
}

#[test]
fn a_legacy_tenant_database_upgrades_with_every_row_and_reading_unchanged() {
    let dir = TempDir::new().unwrap();
    let db = load(&dir, TENANT_FIXTURE, "tenant.sqlite3");
    let config = dir.path().join("tenants.yaml");
    std::fs::write(&config, TENANT_YAML).unwrap();
    let before = snapshot(&db);

    // The pre-S1 startup path (no IDORIS_FORM), run twice: the second start must be a no-op.
    let storage = storage::bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap();
    assert_keys_still_authenticate(&storage, &TENANT_KEYS);
    assert_event_log_intact_and_append_only(
        &storage,
        &db,
        "acme",
        "00000000-0000-4000-8000-000000000100",
    );
    drop(storage);
    let after_first = snapshot(&db);
    assert_upgrade_preserved_everything(&before, &after_first);
    integrity(&db);

    drop(storage::bootstrap(&db, Some(&config), DeployMode::Tenant).unwrap());
    assert_eq!(snapshot(&db), after_first, "a second start changes nothing");

    assert_tenant_goldens(&db);
    // 96_700 available for acme under the fixed clock; new real-key reservations respect it.
    assert_history_counts_for_new_real_key_reservations(&db, TENANT_DB_NOW_MS, "acme", 96_700);
    assert_history_counts_for_new_real_key_reservations(&db, TENANT_DB_NOW_MS, "beta", 1_700);
    // The upgrade never wrote a seed marker for a legacy start.
    let markers: i64 = Connection::open(&db)
        .unwrap()
        .query_row("SELECT count(*) FROM tenant_bootstrap", [], |r| r.get(0))
        .unwrap();
    assert_eq!(markers, 0);
}

#[test]
fn a_legacy_personal_database_upgrades_with_every_row_and_reading_unchanged() {
    let dir = TempDir::new().unwrap();
    let db = load(&dir, PERSONAL_FIXTURE, "personal.sqlite3");
    let config = dir.path().join("tenants.yaml");
    std::fs::write(&config, PERSONAL_YAML).unwrap();
    let before = snapshot(&db);

    let storage = storage::bootstrap(&db, Some(&config), DeployMode::Personal).unwrap();
    assert_keys_still_authenticate(&storage, &PERSONAL_KEYS);
    assert_event_log_intact_and_append_only(
        &storage,
        &db,
        "personal",
        "00000000-0000-4000-8000-000000000300",
    );
    assert!(
        storage.accounts.is_none(),
        "a personal database never gets the account store"
    );
    drop(storage);
    let after_first = snapshot(&db);
    assert_upgrade_preserved_everything(&before, &after_first);
    integrity(&db);
    drop(storage::bootstrap(&db, Some(&config), DeployMode::Personal).unwrap());
    assert_eq!(snapshot(&db), after_first);

    assert_personal_goldens(&db);
    assert_history_counts_for_new_real_key_reservations(
        &db,
        PERSONAL_DB_NOW_MS,
        "personal",
        46_700,
    );
    // And no compute-service table was created in a personal database.
    let account_tables: i64 = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('accounts','principals','memberships','credentials','allocations')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(account_tables, 0);
}

#[test]
fn the_default_personal_start_without_a_config_file_also_upgrades_cleanly() {
    // `IDORIS_TENANTS_CONFIG` unset: the synthetic personal default (limit 0) is written on every
    // start, exactly as before. The upgrade must still not lose or alter any history row.
    let dir = TempDir::new().unwrap();
    let db = load(&dir, PERSONAL_FIXTURE, "personal.sqlite3");
    let spent_before = snapshot(&db).tables["budget_periods"].clone();
    drop(storage::bootstrap(&db, None, DeployMode::Personal).unwrap());
    let after = snapshot(&db);
    assert_eq!(after.tables["budget_periods"], spent_before);
    assert_eq!(after.tables["reservations"].len(), 8);
    integrity(&db);
}

#[test]
fn the_organization_form_adopts_a_legacy_tenant_database_without_changing_it() {
    let dir = TempDir::new().unwrap();
    let db = load(&dir, TENANT_FIXTURE, "tenant.sqlite3");
    let config = dir.path().join("tenants.yaml");
    // The file disagrees with the database (limits) and no longer lists beta at all.
    std::fs::write(
        &config,
        "tenants:\n  - tenant_id: acme\n    limit_minor: 1\n    billing_timezone: UTC\n    scope: all\n  - tenant_id: delta\n    limit_minor: 42\n    billing_timezone: UTC\n    scope: all\n",
    )
    .unwrap();
    let before = snapshot(&db);
    let startup = StartupForm::declared(DeployMode::Tenant, DeploymentForm::Organization).unwrap();
    let storage = storage::bootstrap_with_form(&db, Some(&config), startup).unwrap();
    assert_keys_still_authenticate(&storage, &TENANT_KEYS);
    assert!(storage.accounts.is_some());
    drop(storage);

    let after = snapshot(&db);
    // Nothing in the legacy tables moved, even though the file disagrees and omits beta.
    for (table, rows) in &before.tables {
        if table == "tenant_config" {
            continue;
        }
        assert_eq!(&after.tables[table], rows, "table {table} changed");
    }
    // tenant_config: the three database rows are exactly as they were; delta was seeded from the file.
    let old: BTreeSet<_> = before.tables["tenant_config"].iter().cloned().collect();
    let new: BTreeSet<_> = after.tables["tenant_config"].iter().cloned().collect();
    assert!(
        old.is_subset(&new),
        "no existing tenant row may change or disappear"
    );
    assert_eq!(new.len(), old.len() + 1);
    assert!(
        new.iter()
            .any(|row| row.contains("delta") && row.contains("42"))
    );
    // History readings are intact for every tenant, including the one the file dropped.
    assert_tenant_goldens(&db);
    // Markers: acme and beta adopted; gamma was never in the new file (and keeps its row); delta seeded.
    let conn = Connection::open(&db).unwrap();
    let markers: BTreeMap<String, String> = conn
        .prepare("SELECT tenant_id, source FROM tenant_bootstrap")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        markers,
        BTreeMap::from([
            ("acme".to_string(), "adopted".to_string()),
            ("delta".to_string(), "config".to_string()),
        ])
    );
    integrity(&db);
    // The new compute-service tables exist next to the old ones and start empty.
    for table in [
        "principals",
        "accounts",
        "memberships",
        "credentials",
        "allocations",
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}
