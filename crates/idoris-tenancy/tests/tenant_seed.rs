//! 租户来源以数据库为准（设计稿 §9 第 2 项，S1）：`BudgetLedger::seed_tenant`。
//!
//! 组织、云形态启动时，配置文件里的租户只做**一次性初始化**：每个租户 id 在这个库的一生里最多从文件写入一次，
//! 之后以数据库为准——不覆盖数据库里改过的值、不复活被删的租户、也不会因为文件里没有就删它。
//! 对照：个人和存量 tenant 形态仍然用 `configure_tenant` + `retain_tenants`，它们的测试在 `ledger.rs`。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use idoris_tenancy::budget::{
    BudgetError, BudgetLedger, BudgetScope, SeedDrift, SeedOutcome, SpendGate, TenantSeed,
};
use idoris_tenancy::sqlite_conn::ConnectionProfile;
use rusqlite::Connection;

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-tenant-seed-{tag}-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )))
    }
    fn path(&self) -> &Path {
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

fn seed(tenant: &str, limit: i64, tz: &str, gate: SpendGate) -> TenantSeed {
    TenantSeed {
        tenant_id: tenant.into(),
        limit_minor: limit,
        billing_timezone: tz.into(),
        gate,
    }
}

fn marker_count(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT count(*) FROM tenant_bootstrap", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn the_first_seed_writes_the_tenant_and_marks_it_as_seeded() {
    let db = TempDb::new("first");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    let report = ledger
        .seed_tenant(&seed("acme", 1_000, "Asia/Bangkok", SpendGate::All))
        .unwrap();
    assert_eq!(report.outcome, SeedOutcome::Seeded);
    assert_eq!(report.drift, SeedDrift::InSync);
    let view = ledger.tenant_readview("acme").unwrap();
    assert_eq!(
        (view.limit_minor, view.billing_timezone.as_str(), view.scope),
        (1_000, "Asia/Bangkok", SpendGate::All)
    );
    let source: String = Connection::open(db.path())
        .unwrap()
        .query_row(
            "SELECT source FROM tenant_bootstrap WHERE tenant_id='acme'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source, "config");
}

#[test]
fn seeding_again_is_a_no_op_across_restarts() {
    let db = TempDb::new("again");
    for round in 0..3 {
        let ledger = BudgetLedger::open(db.path()).unwrap();
        let report = ledger
            .seed_tenant(&seed("acme", 1_000, "UTC", SpendGate::PaidOnly))
            .unwrap();
        let expected = if round == 0 {
            SeedOutcome::Seeded
        } else {
            SeedOutcome::Kept
        };
        assert_eq!(report.outcome, expected, "round {round}");
        assert_eq!(report.drift, SeedDrift::InSync);
    }
    assert_eq!(marker_count(db.path()), 1);
}

#[test]
fn the_file_never_overwrites_values_that_were_changed_in_the_database() {
    let db = TempDb::new("db-wins");
    {
        let ledger = BudgetLedger::open(db.path()).unwrap();
        ledger
            .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
            .unwrap();
        // 管理员（以后是控制面）在数据库里把限额调高，并改成对所有请求都检查。
        ledger
            .configure_tenant("acme", 999, "UTC", SpendGate::All)
            .unwrap();
    }
    for _ in 0..2 {
        let ledger = BudgetLedger::open(db.path()).unwrap();
        // 文件里还是旧值：数据库为准，并且报告「文件已经过时」。
        let report = ledger
            .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
            .unwrap();
        assert_eq!(report.outcome, SeedOutcome::Kept);
        assert_eq!(report.drift, SeedDrift::Changed);
        let view = ledger.tenant_readview("acme").unwrap();
        assert_eq!((view.limit_minor, view.scope), (999, SpendGate::All));
    }
    // 对照：`configure_tenant`（个人和存量 tenant 形态每次启动都调用的那条路）才会用文件的值覆盖。
    let ledger = BudgetLedger::open(db.path()).unwrap();
    ledger
        .configure_tenant("acme", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    assert_eq!(ledger.tenant_readview("acme").unwrap().limit_minor, 100);
}

#[test]
fn a_tenant_deleted_from_the_database_is_not_resurrected_by_the_file() {
    let db = TempDb::new("deleted");
    {
        let ledger = BudgetLedger::open(db.path()).unwrap();
        ledger
            .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
            .unwrap();
    }
    Connection::open(db.path())
        .unwrap()
        .execute("DELETE FROM tenant_config WHERE tenant_id = 'acme'", [])
        .unwrap();
    let ledger = BudgetLedger::open(db.path()).unwrap();
    let report = ledger
        .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
        .unwrap();
    assert_eq!(report.outcome, SeedOutcome::Kept);
    assert_eq!(
        report.drift,
        SeedDrift::MissingInDb,
        "the operator must be told the file is stale"
    );
    assert!(matches!(
        ledger.tenant_readview("acme"),
        Err(BudgetError::TenantNotConfigured { .. })
    ));
}

#[test]
fn a_tenant_configured_by_an_older_start_is_adopted_unchanged() {
    let db = TempDb::new("adopt");
    {
        // 旧版本（以及个人 / 存量 tenant 形态）每次启动都这样写。
        let ledger = BudgetLedger::open(db.path()).unwrap();
        ledger
            .configure_tenant("acme", 500, "Asia/Bangkok", SpendGate::All)
            .unwrap();
        let id = ledger
            .reserve(
                &BudgetScope::new("acme", "default", "omlx", "omlx"),
                idoris_tenancy::budget::Price::Known(100),
            )
            .unwrap();
        ledger.settle("acme", &id, 80).unwrap();
    }
    let ledger = BudgetLedger::open(db.path()).unwrap();
    let report = ledger
        .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
        .unwrap();
    assert_eq!(report.outcome, SeedOutcome::Adopted);
    assert_eq!(report.drift, SeedDrift::Changed);
    let view = ledger.tenant_readview("acme").unwrap();
    assert_eq!(
        (
            view.limit_minor,
            view.billing_timezone.as_str(),
            view.scope,
            view.spent_minor
        ),
        (500, "Asia/Bangkok", SpendGate::All, 80),
        "an adopted tenant keeps its row and its spend history"
    );
    let source: String = Connection::open(db.path())
        .unwrap()
        .query_row(
            "SELECT source FROM tenant_bootstrap WHERE tenant_id='acme'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source, "adopted");
    assert_eq!(
        ledger
            .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
            .unwrap()
            .outcome,
        SeedOutcome::Kept
    );
}

#[test]
fn invalid_seed_values_fail_and_leave_nothing_behind() {
    let db = TempDb::new("invalid");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    assert!(matches!(
        ledger.seed_tenant(&seed("  ", 1, "UTC", SpendGate::PaidOnly)),
        Err(BudgetError::InvalidScope { field: "tenant_id" })
    ));
    assert!(matches!(
        ledger.seed_tenant(&seed("acme", -1, "UTC", SpendGate::PaidOnly)),
        Err(BudgetError::InvalidLimit { limit_minor: -1 })
    ));
    assert!(matches!(
        ledger.seed_tenant(&seed("acme", 1, "Not/AZone", SpendGate::PaidOnly)),
        Err(BudgetError::InvalidTimeZone { .. })
    ));
    assert_eq!(marker_count(db.path()), 0);
    assert!(matches!(
        ledger.tenant_readview("acme"),
        Err(BudgetError::TenantNotConfigured { .. })
    ));
    // 已经初始化过的租户，文件里写坏了值也照样报错（笔误在所有形态下都让启动失败，不能因为「数据库为准」就悄悄放过）
    ledger
        .seed_tenant(&seed("acme", 1, "UTC", SpendGate::PaidOnly))
        .unwrap();
    assert!(matches!(
        ledger.seed_tenant(&seed("acme", 1, "Not/AZone", SpendGate::PaidOnly)),
        Err(BudgetError::InvalidTimeZone { .. })
    ));
}

#[test]
fn a_seed_cannot_disagree_with_the_tenants_existing_sub_scope_timezone() {
    let db = TempDb::new("tz");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    ledger
        .configure(&BudgetScope::new("acme", "k", "p", "m"), 10, "UTC")
        .unwrap();
    match ledger.seed_tenant(&seed("acme", 1, "Asia/Bangkok", SpendGate::PaidOnly)) {
        Err(BudgetError::InvalidConfig {
            existing,
            requested,
            ..
        }) => {
            assert_eq!(
                (existing.as_str(), requested.as_str()),
                ("UTC", "Asia/Bangkok")
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        marker_count(db.path()),
        0,
        "the failed seed must not leave a marker"
    );
    // 对照：时区一致就可以
    assert_eq!(
        ledger
            .seed_tenant(&seed("acme", 1, "UTC", SpendGate::PaidOnly))
            .unwrap()
            .outcome,
        SeedOutcome::Seeded
    );
}

#[test]
fn seeding_one_tenant_never_touches_the_others() {
    let db = TempDb::new("others");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    for tenant in ["acme", "beta", "gamma"] {
        ledger
            .seed_tenant(&seed(tenant, 100, "UTC", SpendGate::PaidOnly))
            .unwrap();
    }
    // 配置文件这次只列了 acme：beta 和 gamma 仍然在（组织 / 云形态不再删租户）
    let again = ledger
        .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
        .unwrap();
    assert_eq!(again.outcome, SeedOutcome::Kept);
    for tenant in ["acme", "beta", "gamma"] {
        assert!(
            ledger.tenant_readview(tenant).is_ok(),
            "{tenant} must still be configured"
        );
    }
    assert_eq!(marker_count(db.path()), 3);
    // 对照：`retain_tenants`（个人和存量 tenant 形态的老路径）才会把它们删掉，而且不碰标记
    let keep: BTreeSet<String> = ["acme".to_string()].into();
    assert_eq!(ledger.retain_tenants(&keep).unwrap(), 2);
    assert!(ledger.tenant_readview("beta").is_err());
    assert_eq!(marker_count(db.path()), 3);
}

#[test]
fn concurrent_seeds_of_the_same_tenant_write_exactly_once() {
    const THREADS: usize = 8;
    let db = TempDb::new("race");
    drop(BudgetLedger::open(db.path()).unwrap()); // 先建好表
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                // 每个线程自己的连接，所以靠的是 SQLite 的锁，不是进程内的互斥
                let ledger = BudgetLedger::open(&path).unwrap();
                barrier.wait();
                ledger
                    .seed_tenant(&seed("acme", 100, "UTC", SpendGate::PaidOnly))
                    .unwrap()
                    .outcome
            })
        })
        .collect();
    let outcomes: Vec<SeedOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == SeedOutcome::Seeded)
            .count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes.iter().filter(|o| **o == SeedOutcome::Kept).count(),
        THREADS - 1,
        "{outcomes:?}"
    );
    let conn = Connection::open(db.path()).unwrap();
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM tenant_config", [], |r| r.get(0))
        .unwrap();
    assert_eq!((rows, marker_count(db.path())), (1, 1));
}

#[test]
fn the_standard_profile_ledger_opens_and_seeds_like_the_legacy_one() {
    let db = TempDb::new("standard");
    let ledger = BudgetLedger::open_with_profile(db.path(), ConnectionProfile::Standard).unwrap();
    let pragmas = ledger.connection_pragmas().unwrap();
    assert_eq!(
        (
            pragmas.journal_mode.as_str(),
            pragmas.busy_timeout_ms,
            pragmas.foreign_keys
        ),
        ("wal", 5_000, true)
    );
    assert_eq!(
        ledger
            .seed_tenant(&seed("acme", 1, "UTC", SpendGate::PaidOnly))
            .unwrap()
            .outcome,
        SeedOutcome::Seeded
    );
    // 一个全新库上用 Standard 档位把五个迁移（含重建表的 0005）都跑一遍，外键打开也不出错
    let conn = Connection::open(db.path()).unwrap();
    let versions: i64 = conn
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(versions, 6);
}

#[test]
fn the_legacy_profile_ledger_is_exactly_what_it_was_before_s1() {
    // 个人和存量 tenant 形态走 `open` / `open_with*`：WAL 加 5 秒 busy_timeout，和 S1 之前一样。
    let db = TempDb::new("legacy");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    let pragmas = ledger.connection_pragmas().unwrap();
    assert_eq!(
        (pragmas.journal_mode.as_str(), pragmas.busy_timeout_ms),
        ("wal", 5_000)
    );
    let explicit = BudgetLedger::open_with_profile(db.path(), ConnectionProfile::Legacy).unwrap();
    let again = explicit.connection_pragmas().unwrap();
    assert_eq!(
        (again.journal_mode.as_str(), again.busy_timeout_ms),
        ("wal", 5_000)
    );
}
