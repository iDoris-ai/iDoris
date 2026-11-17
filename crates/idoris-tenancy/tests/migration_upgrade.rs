//! 已经发布过的编号迁移**不能原地改**，补保护只能追加新的编号（PR #600 第四轮复审：账户 / 尝试 / 账本的迁移）。
//!
//! 迁移器对版本表里已经登记的编号直接跳过。第二轮修复把「部分唯一索引也要挡住 REPLACE」的触发器直接写进了
//! `attempts/0002` 和 `ledger/0002`：**新库**有这些触发器，但已经跑过旧 0002 的开发 / 预发库（版本表里已经有 2）
//! 升级之后一条都得不到，更换幂等键的 `INSERT OR REPLACE` 仍然能把已经落定的 18_000 分录整行换掉。
//!
//! 这个文件做三件事：
//!
//! 1. **旧 v2 库 -> HEAD 的升级测试**：用首次发布时的 0002（`tests/fixtures/*_first_release.sql`，原样取自 git 历史）建库、写入数据，
//!    先证明旧库确实缺这些保护（REPLACE 能改写），再用 HEAD 的代码打开，断言新触发器装上了、REPLACE 被拒、数据一行没动、
//!    版本号往后走。另有一份「已经被原地改过一次的 0002」（`*_with_inplace_*.sql`，第二轮之后建出来的库长这样）：
//!    升级同样成功，每个保护只有一份；
//! 2. **账本分录序号的回填**（账本迁移 0004）：旧库里已有的额度、撤销按时间折算出序号，新写入的由触发器记下；
//! 3. **已发布的迁移文件按内容钉死**：以后谁再改它们，这里立刻红，提示改成追加新编号。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::attempts::AttemptStore;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-upgrade-{tag}-{}-{}.sqlite3",
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

fn exec(conn: &Connection, sql: &str) {
    conn.execute_batch(sql)
        .unwrap_or_else(|e| panic!("{e}\n--- sql ---\n{sql}"));
}

/// 一条普通连接：不开外键（任何别的连接默认就是这样，REPLACE 删旧行时没有外键挡着）。
fn open_raw(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn
}

fn versions(path: &Path, table: &str) -> Vec<i64> {
    let conn = open_raw(path);
    let mut stmt = conn
        .prepare(&format!("SELECT version FROM {table} ORDER BY version"))
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn names(path: &Path, sql: &str) -> Vec<String> {
    let conn = open_raw(path);
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn scalar(path: &Path, sql: &str) -> i64 {
    open_raw(path).query_row(sql, [], |row| row.get(0)).unwrap()
}

fn trigger_count(path: &Path, name: &str) -> i64 {
    scalar(
        path,
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = '{name}'"),
    )
}

// ------------------------------------------------------------------------------------------
// 已发布的迁移文件按内容钉死
// ------------------------------------------------------------------------------------------

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// 已经发布过的迁移文件（开发 / 预发库已经跑过它们）。账户 0001 / 0002、账本 0001 / 0002、尝试 0001 / 0002 的内容在这里钉死；
/// 要改行为，追加新的编号（账户 0003、账本 0005、尝试 0004……），**不要改这些文件**：迁移器对已登记的版本直接跳过，
/// 已有的库得不到你的改动。
///
/// 账本 0002 钉的是「迁出的授信额度 id 不撞已有额度」那一版（`c31a874d`）：旧文本在撞 id 时整个迁移失败、库打不开，
/// 失败的迁移不会登记版本，所以改它不会让任何已经成功升到 2 的库和新文本不一致；见账本 0002 的文件头。
const RELEASED: &[(&str, &str, &str)] = &[
    (
        "accounts/0001_accounts.sql",
        include_str!("../src/accounts/migrations/0001_accounts.sql"),
        "24a7378e193bc143d073501128cbbe093f5942a85a242d110f4378ce7ea6e24e",
    ),
    (
        "accounts/0002_control.sql",
        include_str!("../src/accounts/migrations/0002_control.sql"),
        "f8b176a5303bb31c02ee3cfb54aab25e87dc5d0795bda0bf04aff6c472027394",
    ),
    (
        "ledger/0001_ledger.sql",
        include_str!("../src/accounts/ledger/migrations/0001_ledger.sql"),
        "b59c253a8faa388cce4aa0191d62c074de9ec5d06e39629bb57963ab0ac2798d",
    ),
    (
        "ledger/0002_credit_lines_are_allocations.sql",
        include_str!("../src/accounts/ledger/migrations/0002_credit_lines_are_allocations.sql"),
        "87a6fe9a055258e99619cc084bc7d846aa589b0ec450e5d5732f597105bf9df9",
    ),
    (
        "attempts/0001_compute_attempts.sql",
        include_str!("../src/attempts/migrations/0001_compute_attempts.sql"),
        "b79a5cdf21cb9601f340f5587d166c3a97af490411eb5812dac95cf01aa36c21",
    ),
    (
        "attempts/0002_no_replace.sql",
        include_str!("../src/attempts/migrations/0002_no_replace.sql"),
        "c5e690b10239c026405caaa79b626e47ff098a231e57bf7b86a499e9410a2abf",
    ),
];

#[test]
fn released_migrations_are_never_edited_in_place() {
    let changed: Vec<String> = RELEASED
        .iter()
        .filter(|(_, text, pinned)| digest(text) != *pinned)
        .map(|(name, text, pinned)| format!("{name}: pinned {pinned}, now {}", digest(text)))
        .collect();
    assert!(
        changed.is_empty(),
        "a migration that databases have already applied was edited in place; the migrator skips \
         registered versions, so existing databases would never get the change. Add a NEW numbered \
         migration instead:\n{}",
        changed.join("\n")
    );
    // 首次发布的 attempts/0002 逐字等于 fixtures 里取自 git 历史的那份（`ebf2cf21`），不只是哈希碰巧一样
    assert_eq!(
        RELEASED[5].1,
        include_str!("fixtures/attempts_0002_first_release.sql")
    );
}

// ------------------------------------------------------------------------------------------
// 尝试记录：旧 v2 库 -> HEAD
// ------------------------------------------------------------------------------------------

const FIRST_ATTEMPTS_0002: &str = include_str!("fixtures/attempts_0002_first_release.sql");
const INPLACE_ATTEMPTS_0002: &str =
    include_str!("fixtures/attempts_0002_with_inplace_triggers.sql");

/// 第三个迁移补的三个触发器。
const ATTEMPT_GUARDS: [&str; 3] = [
    "compute_postings_one_user_price_no_conflict",
    "compute_attempts_one_payer_insert_no_conflict",
    "compute_attempts_one_payer_update_no_conflict",
];

/// 一个已经应用到版本 2 的尝试记录库（`second` 是当时的 0002 文本），里面有一个已经落定的请求：一笔 18_000 的 `user_price`，
/// 两个尝试（0 号是向用户收费的那个）。
fn attempts_database_at_version_2(tag: &str, second: &str) -> TempDb {
    let db = TempDb::new(tag);
    let conn = open_raw(db.path());
    exec(
        &conn,
        "CREATE TABLE compute_attempt_schema_migrations (
            version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL)",
    );
    exec(
        &conn,
        include_str!("../src/attempts/migrations/0001_compute_attempts.sql"),
    );
    exec(&conn, second);
    exec(
        &conn,
        "INSERT INTO compute_attempt_schema_migrations (version, applied_at_ms) VALUES (1, 0), (2, 0)",
    );
    exec(
        &conn,
        "INSERT INTO compute_requests
            (request_id, payment_json, payer_account, credential_id, price_unit, form, billing, stream, data_classes, created_at_ms)
         VALUES ('req-1', '{}', 'acct-1', 'cred-1', 'THB', 'organization', 'paid', 0, '', 1);
         INSERT INTO compute_attempts
            (attempt_id, request_id, attempt_index, provider, model, endpoint, price_version, effective_max_tokens,
             reserved_price_micros, reserved_price_unit, reserved_cost_micros, reserved_cost_unit, snapshot_json,
             state, state_json, pays_user, created_at_ms, updated_at_ms)
         VALUES
            ('req-1#0', 'req-1', 0, 'p', 'm', 'e', 'v1', 100, 18000, 'THB', 0, 'USD', '{}',
             'settled', '{\"state\":\"settled\"}', 1, 1, 1),
            ('req-1#1', 'req-1', 1, 'p', 'm2', 'e', 'v1', 100, 18000, 'THB', 0, 'USD', '{}',
             'not_sent', '{\"state\":\"not_sent\"}', 0, 1, 1);
         INSERT INTO compute_postings
            (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
         VALUES ('req-1', 'req-1#0', 'user_price', 'answering', 'actual_usage', 'THB', 18000, 'key-user-price', 2)",
    );
    db
}

/// 复审的探测：换一个新的幂等键（posting_id、幂等键都不冲突，只撞「每请求一笔正 user_price」的部分唯一索引）。
const REPLACE_THE_USER_PRICE: &str =
    "INSERT OR REPLACE INTO compute_postings
        (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
     SELECT request_id, attempt_id, 'user_price', role, basis, unit, 1, 'forged-key', created_at_ms
       FROM compute_postings WHERE kind = 'user_price'";

/// 把另一个尝试改成收费的：撞上「每请求一个收费尝试」的部分唯一索引，REPLACE 把原来收费的整行删掉。
const REPLACE_THE_PAYING_ATTEMPT: &str =
    "UPDATE OR REPLACE compute_attempts SET pays_user = 1 WHERE attempt_id = 'req-1#1'";

fn user_price_total(path: &Path) -> i64 {
    scalar(
        path,
        "SELECT COALESCE(SUM(amount_micros), 0) FROM compute_postings WHERE kind = 'user_price'",
    )
}

fn paying_attempts(path: &Path) -> Vec<String> {
    names(
        path,
        "SELECT attempt_id FROM compute_attempts WHERE pays_user = 1 ORDER BY attempt_id",
    )
}

#[test]
fn the_first_release_of_attempts_0002_really_lacks_the_partial_index_guards() {
    // 前提：旧 v2 库缺这些保护。没有这条，下面「升级之后被拒」就可能只是夹具和真实旧库不一样。
    let db = attempts_database_at_version_2("attempts-premise", FIRST_ATTEMPTS_0002);
    for guard in ATTEMPT_GUARDS {
        assert_eq!(trigger_count(db.path(), guard), 0, "{guard}");
    }
    let conn = open_raw(db.path());
    assert_eq!(user_price_total(db.path()), 18_000);
    conn.execute(REPLACE_THE_USER_PRICE, [])
        .expect("an old v2 database lets this REPLACE through");
    assert_eq!(user_price_total(db.path()), 1, "18_000 became 1");
    conn.execute(REPLACE_THE_PAYING_ATTEMPT, [])
        .expect("an old v2 database lets this REPLACE through too");
    assert_eq!(
        paying_attempts(db.path()),
        ["req-1#1"],
        "the attempt that was paying was deleted"
    );
    assert_eq!(
        scalar(db.path(), "SELECT COUNT(*) FROM compute_attempts"),
        1
    );
}

#[test]
fn an_attempts_database_that_already_ran_the_first_0002_gets_the_guards_from_0003() {
    let db = attempts_database_at_version_2("attempts-upgrade", FIRST_ATTEMPTS_0002);
    assert_eq!(
        versions(db.path(), "compute_attempt_schema_migrations"),
        [1, 2]
    );

    drop(AttemptStore::open(db.path()).unwrap());

    assert_eq!(
        versions(db.path(), "compute_attempt_schema_migrations"),
        [1, 2, 3],
        "the new guards are a new numbered migration, not an edit of 0002"
    );
    for guard in ATTEMPT_GUARDS {
        assert_eq!(trigger_count(db.path(), guard), 1, "{guard} is installed");
    }
    let conn = open_raw(db.path());
    for sql in [
        REPLACE_THE_USER_PRICE,
        "REPLACE INTO compute_postings
            (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
         SELECT request_id, attempt_id, 'user_price', role, basis, unit, 999999, 'forged-key-2', created_at_ms
           FROM compute_postings WHERE kind = 'user_price'",
        REPLACE_THE_PAYING_ATTEMPT,
    ] {
        let error = conn.execute(sql, []).expect_err(sql).to_string();
        assert!(
            error.contains("at most one"),
            "refused by the new guard: {error}"
        );
    }
    // 数据一行没动
    assert_eq!(user_price_total(db.path()), 18_000);
    assert_eq!(paying_attempts(db.path()), ["req-1#0"]);
    assert_eq!(
        scalar(db.path(), "SELECT COUNT(*) FROM compute_attempts"),
        2
    );
    // 负数冲正不在索引范围内，不能被新触发器误伤
    conn.execute(
        "INSERT INTO compute_postings
            (request_id, attempt_id, kind, role, basis, unit, amount_micros, idempotency_key, created_at_ms)
         SELECT request_id, attempt_id, 'user_price', role, basis, unit, -18000, 'reversal-key', created_at_ms
           FROM compute_postings WHERE kind = 'user_price' AND amount_micros > 0",
        [],
    )
    .unwrap();

    // 再打开一次：什么都不变
    drop(AttemptStore::open(db.path()).unwrap());
    assert_eq!(
        versions(db.path(), "compute_attempt_schema_migrations"),
        [1, 2, 3]
    );
}

#[test]
fn an_attempts_database_created_after_the_in_place_edit_upgrades_cleanly_with_one_copy_of_each_guard()
 {
    // 第二轮之后、这次修复之前建出来的库：0002 里已经有这三个触发器，版本表是 [1, 2]。
    let db = attempts_database_at_version_2("attempts-inplace", INPLACE_ATTEMPTS_0002);
    for guard in ATTEMPT_GUARDS {
        assert_eq!(trigger_count(db.path(), guard), 1, "{guard} came with 0002");
    }
    drop(AttemptStore::open(db.path()).unwrap());
    assert_eq!(
        versions(db.path(), "compute_attempt_schema_migrations"),
        [1, 2, 3]
    );
    for guard in ATTEMPT_GUARDS {
        assert_eq!(
            trigger_count(db.path(), guard),
            1,
            "{guard}: still one copy"
        );
    }
    let conn = open_raw(db.path());
    assert!(conn.execute(REPLACE_THE_USER_PRICE, []).is_err());
    assert_eq!(user_price_total(db.path()), 18_000);
}

#[test]
fn a_fresh_attempts_database_has_the_same_guards_as_an_upgraded_one() {
    let fresh = TempDb::new("attempts-fresh");
    drop(AttemptStore::open(fresh.path()).unwrap());
    let upgraded = attempts_database_at_version_2("attempts-same", FIRST_ATTEMPTS_0002);
    drop(AttemptStore::open(upgraded.path()).unwrap());
    let objects = |path: &Path| {
        names(
            path,
            "SELECT type || ' ' || name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY 1",
        )
    };
    assert_eq!(objects(fresh.path()), objects(upgraded.path()));
    assert_eq!(
        versions(fresh.path(), "compute_attempt_schema_migrations"),
        [1, 2, 3]
    );
}

// ------------------------------------------------------------------------------------------
// 账本：旧 v2 库 -> HEAD
// ------------------------------------------------------------------------------------------

const FIRST_LEDGER_0002: &str = include_str!("fixtures/ledger_0002_first_release.sql");
const INPLACE_LEDGER_0002: &str = include_str!("fixtures/ledger_0002_with_inplace_changes.sql");

const GRACE_GUARD: &str = "ledger_grace_one_active_insert_no_conflict";

/// 一个已经应用到账户迁移 0002、账本迁移 0002 的库（`second` 是当时的账本 0002 文本），里面有：
///
/// * 三个预留（`r1#0` 在时刻 1000、`r2#0` 在 1100、`r3#0` 在 3000），`emp` 上各一条 reserve 分录（分录号 1、2、3），
///   `r1#0` 占了本账期唯一一次宽限；
/// * `emp` 名下三笔额度：`a-first` 创建于 500、`a-same-ms` 创建于 1100、`a-mid` 创建于 2000，`a-mid` 在 3500 被撤销。
fn ledger_database_at_version_2(tag: &str, second: &str) -> TempDb {
    let db = TempDb::new(tag);
    let conn = open_raw(db.path());
    exec(
        &conn,
        include_str!("../src/accounts/migrations/0001_accounts.sql"),
    );
    exec(
        &conn,
        include_str!("../src/accounts/migrations/0002_control.sql"),
    );
    exec(
        &conn,
        "CREATE TABLE account_schema_migrations (
            version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT;
         INSERT INTO account_schema_migrations (version, applied_at_ms) VALUES (1, 0), (2, 0)",
    );
    exec(
        &conn,
        include_str!("../src/accounts/ledger/migrations/0001_ledger.sql"),
    );
    exec(&conn, second);
    exec(
        &conn,
        "CREATE TABLE ledger_schema_migrations (
            version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT;
         INSERT INTO ledger_schema_migrations (version, applied_at_ms) VALUES (1, 0), (2, 0)",
    );
    exec(
        &conn,
        "INSERT INTO accounts (account_id, kind, parent_id, unit, status, created_at_ms) VALUES
            ('platform', 'platform', NULL, 'THB', 'active', 0),
            ('paid', 'org', 'platform', 'THB', 'active', 0),
            ('emp', 'member', 'paid', 'THB', 'active', 0);
         INSERT INTO ledger_holds
            (hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id,
             unit, reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros, state)
         VALUES
            ('r1#0', 'r1#0', 'model', 'r1', 0, 'emp', 'cred-e', 'THB', 100, '2026-11', 1000, 9000000, 0, 'open'),
            ('r2#0', 'r2#0', 'model', 'r2', 0, 'emp', 'cred-e', 'THB', 100, '2026-11', 1100, 9000000, 0, 'open'),
            ('r3#0', 'r3#0', 'model', 'r3', 0, 'emp', 'cred-e', 'THB', 100, '2026-11', 3000, 9000000, NULL, 'open');
         INSERT INTO ledger_entries
            (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
             window_at_ms, at_ms, idempotency_key)
         VALUES
            ('emp', 'r1#0', 'reserve', 'THB', 100, 0, 0, 0, 1000, 1000, 'k1'),
            ('emp', 'r2#0', 'reserve', 'THB', 100, 0, 0, 0, 1100, 1100, 'k2'),
            ('emp', 'r3#0', 'reserve', 'THB', 100, 0, 0, 0, 3000, 3000, 'k3');
         INSERT INTO ledger_grace_uses
            (account_id, period, hold_id, unit, reserve_micros, shortfall_micros, floor_micros, used_at_ms)
         VALUES ('emp', '2026-11', 'r1#0', 'THB', 100, 50, 0, 1000);
         INSERT INTO allocations
            (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
             valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms)
         VALUES
            ('a-first',   'paid', 'emp', 'THB', 100, 'prepaid', 0, 9000000, 0, NULL, 500),
            ('a-same-ms', 'paid', 'emp', 'THB', 100, 'prepaid', 0, 9000000, 0, NULL, 1100),
            ('a-mid',     'paid', 'emp', 'THB', 100, 'prepaid', 0, 9000000, 0, NULL, 2000);
         INSERT INTO allocation_revocations (allocation_id, revoked_at_ms, revoked_by_kind, revoked_by_id)
         VALUES ('a-mid', 3500, 'operator', 'upgrade-test')",
    );
    db
}

/// 复审指出的缺口：另一个预留的宽限只撞「每账户每账期至多一行没作废的宽限」这个部分唯一索引，REPLACE 把已经记下的那行换掉。
const REPLACE_THE_GRACE_USE: &str =
    "INSERT OR REPLACE INTO ledger_grace_uses
        (account_id, period, hold_id, unit, reserve_micros, shortfall_micros, floor_micros, used_at_ms)
     VALUES ('emp', '2026-11', 'r2#0', 'THB', 100, 60, 0, 1100)";

fn grace_holders(path: &Path) -> Vec<String> {
    names(
        path,
        "SELECT hold_id FROM ledger_grace_uses ORDER BY use_id",
    )
}

#[test]
fn the_first_release_of_ledger_0002_really_lacks_the_grace_guard() {
    let db = ledger_database_at_version_2("ledger-premise", FIRST_LEDGER_0002);
    assert_eq!(trigger_count(db.path(), GRACE_GUARD), 0);
    assert_eq!(grace_holders(db.path()), ["r1#0"]);
    open_raw(db.path())
        .execute(REPLACE_THE_GRACE_USE, [])
        .expect("an old v2 ledger lets this REPLACE through");
    assert_eq!(
        grace_holders(db.path()),
        ["r2#0"],
        "the recorded grace use was swapped for another hold's"
    );
}

#[test]
fn a_ledger_database_that_already_ran_the_first_0002_gets_the_grace_guard_from_0003() {
    let db = ledger_database_at_version_2("ledger-upgrade", FIRST_LEDGER_0002);
    assert_eq!(versions(db.path(), "ledger_schema_migrations"), [1, 2]);

    drop(AccountStore::open(db.path()).unwrap());

    assert_eq!(
        versions(db.path(), "ledger_schema_migrations"),
        [1, 2, 3, 4],
        "0003 carries the REPLACE guard, 0004 the allocation order marks; 0002 is untouched"
    );
    assert_eq!(
        versions(db.path(), "account_schema_migrations"),
        [1, 2],
        "the account migrations did not change"
    );
    assert_eq!(trigger_count(db.path(), GRACE_GUARD), 1);
    let error = open_raw(db.path())
        .execute(REPLACE_THE_GRACE_USE, [])
        .expect_err("the grace guard refuses the REPLACE")
        .to_string();
    assert!(
        error.contains("UNIQUE constraint failed: ledger_grace_uses"),
        "{error}"
    );
    assert_eq!(
        grace_holders(db.path()),
        ["r1#0"],
        "the recorded grace use stayed"
    );

    drop(AccountStore::open(db.path()).unwrap());
    assert_eq!(
        versions(db.path(), "ledger_schema_migrations"),
        [1, 2, 3, 4]
    );
}

#[test]
fn a_ledger_database_created_after_the_in_place_edit_upgrades_cleanly_with_one_copy_of_the_guard() {
    let db = ledger_database_at_version_2("ledger-inplace", INPLACE_LEDGER_0002);
    assert_eq!(
        trigger_count(db.path(), GRACE_GUARD),
        1,
        "it came with 0002"
    );
    drop(AccountStore::open(db.path()).unwrap());
    assert_eq!(
        versions(db.path(), "ledger_schema_migrations"),
        [1, 2, 3, 4]
    );
    assert_eq!(trigger_count(db.path(), GRACE_GUARD), 1);
    assert!(
        open_raw(db.path())
            .execute(REPLACE_THE_GRACE_USE, [])
            .is_err()
    );
}

#[test]
fn a_fresh_ledger_database_has_the_same_objects_as_an_upgraded_one() {
    let fresh = TempDb::new("ledger-fresh");
    drop(AccountStore::open(fresh.path()).unwrap());
    let upgraded = ledger_database_at_version_2("ledger-same", FIRST_LEDGER_0002);
    drop(AccountStore::open(upgraded.path()).unwrap());
    let objects = |path: &Path| {
        names(
            path,
            "SELECT type || ' ' || name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY 1",
        )
    };
    assert_eq!(objects(fresh.path()), objects(upgraded.path()));
    assert_eq!(
        versions(fresh.path(), "ledger_schema_migrations"),
        [1, 2, 3, 4]
    );
}

// ------------------------------------------------------------------------------------------
// 账本 0004：额度创建 / 撤销在账本里的先后
// ------------------------------------------------------------------------------------------

fn marks(path: &Path) -> Vec<(String, String, i64)> {
    let conn = open_raw(path);
    let mut stmt = conn
        .prepare(
            "SELECT allocation_id, event, after_entry FROM ledger_allocation_marks
              ORDER BY allocation_id, event",
        )
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn allocations_that_predate_the_marks_get_their_order_worked_out_from_the_old_timestamps() {
    let db = ledger_database_at_version_2("ledger-marks", FIRST_LEDGER_0002);
    drop(AccountStore::open(db.path()).unwrap());
    // 旧库只有毫秒：额度创建时刻之前 `emp` 上已经有的最大分录号，就是它的序号（严格早于：同一毫秒的分录算在它之后，
    // 和旧口径「窗口从创建时刻起、含这一毫秒」一致）。撤销同理
    assert_eq!(
        marks(db.path()),
        [
            ("a-first".to_string(), "created".to_string(), 0),
            ("a-mid".to_string(), "created".to_string(), 2),
            ("a-mid".to_string(), "revoked".to_string(), 3),
            ("a-same-ms".to_string(), "created".to_string(), 1),
        ]
    );
}

#[test]
fn an_allocation_written_after_the_upgrade_is_stamped_with_the_ledger_position_not_its_timestamp() {
    let db = ledger_database_at_version_2("ledger-stamp", FIRST_LEDGER_0002);
    drop(AccountStore::open(db.path()).unwrap());
    let conn = open_raw(db.path());
    // 创建时刻写成 100（比所有分录都早）：序号仍然是此刻账本里最后一条分录（3）
    conn.execute(
        "INSERT INTO allocations
            (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
             valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms)
         VALUES ('a-new', 'paid', 'emp', 'THB', 100, 'prepaid', 0, 9000000, 0, NULL, 100)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO allocation_revocations (allocation_id, revoked_at_ms, revoked_by_kind, revoked_by_id)
         VALUES ('a-new', 1, 'operator', 'upgrade-test')",
        [],
    )
    .unwrap();
    let new: Vec<_> = marks(db.path())
        .into_iter()
        .filter(|(id, _, _)| id == "a-new")
        .collect();
    assert_eq!(
        new,
        [
            ("a-new".to_string(), "created".to_string(), 3),
            ("a-new".to_string(), "revoked".to_string(), 3),
        ]
    );
    // 往账本里再追加一条分录，之后创建的额度序号跟着走
    conn.execute(
        "INSERT INTO ledger_entries
            (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
             window_at_ms, at_ms, idempotency_key)
         VALUES ('emp', 'r1#0', 'release', 'THB', -100, 0, 0, 0, 1000, 5, 'k4')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO allocations
            (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
             valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms)
         VALUES ('a-newer', 'paid', 'emp', 'THB', 100, 'prepaid', 0, 9000000, 0, NULL, 0)",
        [],
    )
    .unwrap();
    assert!(marks(db.path()).contains(&("a-newer".to_string(), "created".to_string(), 4)));
}

#[test]
fn the_marks_are_append_only_and_cannot_be_replaced_or_forged() {
    let db = ledger_database_at_version_2("ledger-marks-immutable", FIRST_LEDGER_0002);
    drop(AccountStore::open(db.path()).unwrap());
    let conn = open_raw(db.path());
    for sql in [
        "UPDATE ledger_allocation_marks SET after_entry = 0 WHERE allocation_id = 'a-mid'",
        "DELETE FROM ledger_allocation_marks WHERE allocation_id = 'a-mid'",
        "INSERT OR REPLACE INTO ledger_allocation_marks (allocation_id, event, after_entry)
         VALUES ('a-mid', 'created', 0)",
        "REPLACE INTO ledger_allocation_marks (allocation_id, event, after_entry)
         VALUES ('a-first', 'created', 99)",
        // 不存在的额度；撤销序号记在没被撤销的额度上
        "INSERT INTO ledger_allocation_marks (allocation_id, event, after_entry)
         VALUES ('no-such-allocation', 'created', 0)",
        "INSERT INTO ledger_allocation_marks (allocation_id, event, after_entry)
         VALUES ('a-first', 'revoked', 0)",
    ] {
        assert!(conn.execute(sql, []).is_err(), "must be refused: {sql}");
    }
    assert_eq!(marks(db.path()).len(), 4, "nothing changed");
}
