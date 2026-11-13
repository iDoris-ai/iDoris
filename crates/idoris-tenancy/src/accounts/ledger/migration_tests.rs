//! 账本迁移 0002（授信的权威数据源统一到 `allocations`，补登控制面发出的免费额度的承诺）对**旧库**的影响。
//!
//! 这个功能尚未上线，没有存量生产数据；这里用一个「只应用到 0001」的库证明：开发 / 预发库里如果有旧数据，
//! 升级之后授信和承诺都对得上，旧表删掉，没有第二份记录。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rusqlite::Connection;

use super::run_migrations;

/// 一个只应用到账户迁移 0001 / 0002 和账本迁移 0001 的库（账本版本表里只有版本 1）。
fn database_at_ledger_version_1() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../migrations/0001_accounts.sql"))
        .unwrap();
    conn.execute_batch(include_str!("../migrations/0002_control.sql"))
        .unwrap();
    conn.execute_batch(include_str!("migrations/0001_ledger.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE ledger_schema_migrations (version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT;
         INSERT INTO ledger_schema_migrations (version, applied_at_ms) VALUES (1, 0);",
    )
    .unwrap();
    conn
}

fn exec(conn: &Connection, sql: &str) {
    conn.execute_batch(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )
    .unwrap()
}

/// platform ── pool(补贴池) ── school ── teacher ── {emp, emp2, emp3, kid}
///          └─ loose(组织，上面没有补贴池) ── orphan
fn legacy_tree(conn: &Connection) {
    exec(
        conn,
        "INSERT INTO accounts (account_id, kind, parent_id, unit, status, created_at_ms) VALUES
            ('platform', 'platform', NULL, 'THB', 'active', 0),
            ('pool', 'subsidy_pool', 'platform', 'THB', 'active', 0),
            ('school', 'org', 'pool', 'THB', 'active', 0),
            ('teacher', 'sponsor', 'school', 'THB', 'active', 0),
            ('emp', 'member', 'teacher', 'THB', 'active', 0),
            ('emp2', 'member', 'teacher', 'THB', 'active', 0),
            ('emp3', 'member', 'teacher', 'THB', 'active', 0),
            ('kid', 'member', 'teacher', 'THB', 'active', 0),
            ('loose', 'org', 'platform', 'THB', 'active', 0),
            ('orphan', 'member', 'loose', 'THB', 'active', 0)",
    );
    exec(
        conn,
        "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) VALUES
            ('p-kid', 'active', 0, NULL), ('p-orphan', 'active', 0, NULL)",
    );
    exec(
        conn,
        "INSERT INTO memberships (membership_id, principal_id, org_id, role, account_id, status, created_at_ms, revoked_at_ms) VALUES
            ('m-kid', 'p-kid', 'school', 'member', 'kid', 'active', 0, NULL),
            ('m-orphan', 'p-orphan', 'loose', 'member', 'orphan', 'active', 0, NULL)",
    );
}

#[test]
fn an_old_database_keeps_its_latest_enabled_credit_lines_as_allocations_and_loses_the_old_table() {
    let mut conn = database_at_ledger_version_1();
    legacy_tree(&conn);
    // 旧表：emp 先 100（不到期）再 200（到 5000）——最新一条生效；emp2 先 50 再关掉；emp3 开过一条已经过期的
    exec(
        &conn,
        "INSERT INTO ledger_credit_lines (account_id, guarantor_id, enabled, limit_micros, valid_until_ms, set_at_ms) VALUES
            ('emp', 'teacher', 1, 100, NULL, 10),
            ('emp', 'teacher', 1, 200, 5000, 20),
            ('emp2', 'teacher', 1, 50, NULL, 10),
            ('emp2', 'teacher', 0, 0, NULL, 30),
            ('emp3', 'teacher', 1, 70, 11, 10)",
    );

    run_migrations(&mut conn).unwrap();

    assert!(
        !table_exists(&conn, "ledger_credit_lines"),
        "the old table is gone: one source of truth"
    );
    let lines: Vec<(String, String, String, i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT allocation_id, parent_id, child_id, ceiling_micros, valid_from_ms, valid_until_ms
                   FROM allocations WHERE purpose = 'credit_line' ORDER BY child_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    assert_eq!(
        lines,
        [
            // emp：只迁最新一条（200，到 5000）；旧的 100 本来就被替换了
            (
                "cl-v2".to_string(),
                "teacher".to_string(),
                "emp".to_string(),
                200,
                20,
                5000
            ),
            // emp2 的最新一条是「关掉」：没有迁
            // emp3：已经过期的也迁（窗口检查会让它读不到，无害）
            (
                "cl-v5".to_string(),
                "teacher".to_string(),
                "emp3".to_string(),
                70,
                10,
                11
            ),
        ]
    );
    // 版本记上了
    let applied: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT version FROM ledger_schema_migrations ORDER BY version")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(applied, [1, 2]);
}

#[test]
fn an_open_ended_old_credit_line_gets_a_far_future_expiry_because_an_allocation_must_end() {
    let mut conn = database_at_ledger_version_1();
    legacy_tree(&conn);
    exec(
        &conn,
        "INSERT INTO ledger_credit_lines (account_id, guarantor_id, enabled, limit_micros, valid_until_ms, set_at_ms) VALUES
            ('emp', 'teacher', 1, 100, NULL, 10)",
    );
    run_migrations(&mut conn).unwrap();
    let until: i64 = conn
        .query_row(
            "SELECT valid_until_ms FROM allocations WHERE purpose = 'credit_line' AND child_id = 'emp'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(until, super::limits::OPEN_ENDED_UNTIL_MS);
}

#[test]
fn free_tier_allocations_issued_through_the_old_control_plane_get_their_pool_commitment() {
    let mut conn = database_at_ledger_version_1();
    legacy_tree(&conn);
    // 旧控制面发出的免费额度：有额度行，没有承诺行。kid 在补贴池下面；orphan 上面没有池
    exec(
        &conn,
        "INSERT INTO allocations
            (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
             valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms) VALUES
            ('fta-kid', 'teacher', 'kid', 'THB', 60, 'free_tier', 100, 200, 0, 'subsidy:p-kid:2026-11', 50),
            ('fta-orphan', 'loose', 'orphan', 'THB', 40, 'free_tier', 100, 200, 0, 'subsidy:p-orphan:2026-11', 50)",
    );
    run_migrations(&mut conn).unwrap();
    let grants: Vec<(String, String, String, String, String, i64, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT idempotency_key, pool_id, principal_id, member_account, period, amount_micros, allocation_id
                   FROM ledger_subsidy_grants ORDER BY idempotency_key",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    assert_eq!(
        grants,
        [(
            "subsidy:p-kid:2026-11".to_string(),
            "pool".to_string(),
            "p-kid".to_string(),
            "kid".to_string(),
            "2026-11".to_string(),
            60,
            "fta-kid".to_string()
        )],
        "the allocation without a pool above it has nothing to be committed against and is skipped"
    );
}

#[test]
fn a_fresh_database_applies_both_ledger_migrations_and_has_no_old_credit_table() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../migrations/0001_accounts.sql"))
        .unwrap();
    conn.execute_batch(include_str!("../migrations/0002_control.sql"))
        .unwrap();
    run_migrations(&mut conn).unwrap();
    assert!(!table_exists(&conn, "ledger_credit_lines"));
    assert!(table_exists(&conn, "ledger_subsidy_grants"));
    // 再跑一次什么都不变（幂等）
    run_migrations(&mut conn).unwrap();
}
