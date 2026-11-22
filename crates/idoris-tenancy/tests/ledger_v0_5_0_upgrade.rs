//! 从 v0.5.0 的库升级（issue #601：充过值的账户也按方案 C 记账）。
//!
//! 记账口径是账本分录、额度、额度序号上的**投影**（`ledger/project.rs`），没有新的列、表、触发器，所以不需要迁移：
//! v0.5.0 写下的库用新代码打开，版本表、表结构、已写的每一行都不变，变的只是同一批分录怎么读。
//!
//! 夹具 `fixtures/ledger_v0_5_0_funded_books.sql` 是 v0.5.0 的账户 / 账本代码真实跑出来的库（头注释写了来历和旧代码读到的数，
//! 那些数是旧代码当时打印的，不是被测代码算的）。组织 `paid` 下五个成员：
//!
//! | 成员 | 历史 | 方案 C 之前读到 | 这次 |
//! |---|---|---|---|
//! | m-a | 没充值，授信 200 花光，之后发额度 100 | 余额 `None`，额度剩余 -100，授信 200 | 不变 |
//! | m-b | 充值 100、授信 100，花了 200（余额 -100），之后发额度 100 | 余额 -100，额度剩余 100，授信 100：新额度花不出去 | 新额度可花 |
//! | m-c | 充值 100、额度 100（窗口 `[T0 - 1 天, T0 + 30 天)`），没有授信，花了 60 | 余额 40，额度剩余 40 | 不变（没有授信，额度是上限） |
//! | m-d | 充值 100、授信 100，没有额度，花了 150（余额 -50） | 余额 -50，额度 `None`，授信 100 | 不变 |
//! | m-e | 没充值，授信 200 花光，发额度 100，之后充值 50 | 余额 -150，额度剩余 100，授信 200：能花的 50 | 能花的 150 |
//!
//! 「不变」的三个成员，视图和旧代码读到的逐字一样；「新额度可花」「能花的 150」是这次要改的（issue 的两个场景）。
//! 在升级后的库上继续准入，账本守恒（对账干净）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::path::Path;

use fixture::*;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::AlertKind;
use rusqlite::Connection;

const FIXTURE: &str = include_str!("fixtures/ledger_v0_5_0_funded_books.sql");

/// 旧代码读夹具时的时刻（`T0 + 11 秒`：所有额度的窗口都已开始）。
const READ_AT: i64 = T0 + 11_000;

/// 把夹具装进一个新库（v0.5.0 的账本是 WAL 模式，先复刻）。
fn load(tag: &str) -> TempDb {
    let db = TempDb::new(tag);
    let conn = Connection::open(db.path()).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    // 转储按表名的字母序一张一张建表、插数据，`credentials` 在它引用的 `memberships` 之前：装载时关掉外键（数据本来就是在外键开着时写下的）
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.execute_batch(FIXTURE).unwrap();
    db
}

fn raw(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    conn
}

/// 库里所有对象（表、索引、触发器）的名字，按字母序。
fn objects(path: &Path) -> Vec<String> {
    let conn = raw(path);
    let mut stmt = conn
        .prepare("SELECT type || ' ' || name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY 1")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn versions(path: &Path, table: &str) -> Vec<i64> {
    let conn = raw(path);
    let mut stmt = conn
        .prepare(&format!("SELECT version FROM {table} ORDER BY version"))
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 一张表所有行的内容，拼成一条文本（任何一行、任何一列变了都会不同）。
fn table_text(path: &Path, table: &str, order_by: &str) -> String {
    let conn = raw(path);
    let columns: Vec<String> = {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
            ))
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let expression = columns
        .iter()
        .map(|c| format!("quote({c})"))
        .collect::<Vec<_>>()
        .join(" || '|' || ");
    conn.query_row(
        &format!("SELECT COALESCE(group_concat(row_text, char(10)), '') FROM (SELECT {expression} AS row_text FROM {table} ORDER BY {order_by})"),
        [],
        |row| row.get(0),
    )
    .unwrap()
}

/// 夹具里有数据的表（不含版本表：升级时迁移器可能登记；也不含 `sqlite_sequence`）。
const DATA_TABLES: &[(&str, &str)] = &[
    ("accounts", "account_id"),
    ("allocations", "rowid"),
    ("ledger_allocation_marks", "allocation_id, event"),
    ("ledger_entries", "entry_id"),
    ("ledger_holds", "hold_id"),
    ("ledger_alerts", "alert_id"),
    ("ledger_config", "key"),
];

fn view_of(store: &AccountStore, name: &str) -> (Option<i64>, Option<i64>, i64) {
    let v = store.account_view(&aid(name), ts(READ_AT)).unwrap();
    (
        v.balance.map(|m| m.micros),
        v.ceiling_remaining.map(|m| m.micros),
        v.credit_limit.micros,
    )
}

/// 一个预留额 `10 + 2 × max_tokens` 的请求，放行了就按预留额结算。返回预留额（拒绝是 `None`）。
fn spend(
    store: &AccountStore,
    member: &str,
    request: &str,
    at: i64,
    max_tokens: u64,
) -> Option<i64> {
    let req = Req::new(store, request, 0, &format!("cred-{member}"), max_tokens).at(at);
    let decision = store.admit_attempt(&req.input()).unwrap();
    let charged = decision
        .admitted()
        .map(|admitted| admitted.reservation.price.micros());
    if let Some(charged) = charged {
        store
            .settle_attempt(&req.attempt, &price(charged), ts(at + 500))
            .unwrap();
    }
    charged
}

fn denied(store: &AccountStore, member: &str, request: &str, at: i64, max_tokens: u64) -> bool {
    let req = Req::new(store, request, 0, &format!("cred-{member}"), max_tokens).at(at);
    matches!(
        store.admit_attempt(&req.input()).unwrap(),
        AdmissionDecision::Reject { reason } if reason.code() == ReasonCode::BudgetExhausted
    )
}

fn alerts(store: &AccountStore) -> Vec<(String, i64)> {
    store
        .alerts(Some(AlertKind::CreditUsed))
        .unwrap()
        .into_iter()
        .map(|alert| (alert.account, alert.amount.micros))
        .collect()
}

#[test]
fn opening_a_v0_5_0_ledger_changes_no_schema_and_rewrites_no_row() {
    let db = load("v050-open");
    assert_eq!(
        versions(db.path(), "ledger_schema_migrations"),
        [1, 2, 3, 4]
    );
    assert_eq!(versions(db.path(), "account_schema_migrations"), [1, 2]);
    let objects_before = objects(db.path());
    let rows_before: Vec<String> = DATA_TABLES
        .iter()
        .map(|(table, order)| table_text(db.path(), table, order))
        .collect();
    assert!(
        rows_before.iter().all(|text| !text.is_empty()),
        "the fixture has rows in every table checked"
    );

    drop(AccountStore::open(db.path()).unwrap());

    assert_eq!(
        versions(db.path(), "ledger_schema_migrations"),
        [1, 2, 3, 4],
        "the funded books are a projection: no new migration"
    );
    assert_eq!(versions(db.path(), "account_schema_migrations"), [1, 2]);
    assert_eq!(
        objects(db.path()),
        objects_before,
        "no table, index or trigger was added or dropped"
    );
    let rows_after: Vec<String> = DATA_TABLES
        .iter()
        .map(|(table, order)| table_text(db.path(), table, order))
        .collect();
    assert_eq!(rows_after, rows_before, "opening rewrote nothing");
}

#[test]
fn accounts_that_plan_c_did_not_change_read_exactly_as_the_old_code_read_them() {
    let db = load("v050-unchanged");
    let store = AccountStore::open(db.path()).unwrap();
    // 旧代码当时打印的读数（夹具头注释）
    assert_eq!(
        view_of(&store, "m-a"),
        (None, Some(-100), 200),
        "credit-only, unchanged"
    );
    assert_eq!(
        view_of(&store, "m-c"),
        (Some(40), Some(40), 0),
        "funded, no credit line: allowance is a cap"
    );
    assert_eq!(
        view_of(&store, "m-d"),
        (Some(-50), None, 100),
        "funded, no allowance"
    );
    // 它们的准入也和旧口径一样：能花的恰好是视图里的 `可用额度 + 授信`，花完之后再来最小的一笔就拒绝
    for (member, request, max_tokens, room) in [
        ("m-a", "a-x", 45_u64, 100_i64), // 额度 100 + 授信可用 0
        ("m-c", "c-x", 15, 40),          // min(余额 40, 额度 40)，没有授信
        ("m-d", "d-x", 20, 50),          // 余额 -50 + 授信 100
    ] {
        assert_eq!(
            spend(&store, member, request, READ_AT, max_tokens),
            Some(room),
            "{member}"
        );
        assert!(
            denied(
                &store,
                member,
                &format!("{request}-more"),
                READ_AT + 1_000,
                10
            ),
            "{member}: nothing is left after {room}"
        );
    }
    assert_clean(&store);
}

#[test]
fn a_funded_member_that_spent_past_its_balance_gets_the_new_allowance_back() {
    let db = load("v050-m-b");
    let store = AccountStore::open(db.path()).unwrap();
    // 旧代码：余额 -100、额度剩余 100、授信 100，能花 min(-100, 100) + 100 = 0
    assert_eq!(
        view_of(&store, "m-b"),
        (None, Some(0), 100),
        "separate books: the 100 allowance minus the 100 of debt on the 100 credit line"
    );
    assert_eq!(
        spend(&store, "m-b", "b-new", READ_AT, 45),
        Some(100),
        "the new allowance is spendable"
    );
    assert!(
        denied(&store, "m-b", "b-more", READ_AT + 1_000, 10),
        "the credit line stays full"
    );
    assert_clean(&store);
}

#[test]
fn a_top_up_the_old_code_recorded_pays_the_debt_and_leaves_the_allowance_alone() {
    let db = load("v050-m-e");
    let store = AccountStore::open(db.path()).unwrap();
    // 旧代码：余额 -150、额度剩余 100、授信 200，能花 min(-150, 100) + 200 = 50
    assert_eq!(
        view_of(&store, "m-e"),
        (None, Some(-50), 200),
        "100 of allowance minus the 150 of debt that is left after the 50 top-up"
    );
    assert_eq!(
        spend(&store, "m-e", "e-new", READ_AT, 70),
        Some(150),
        "the allowance 100 plus the 50 of credit the top-up gave back: more than the old 50"
    );
    assert!(denied(&store, "m-e", "e-more", READ_AT + 1_000, 10));
    assert_clean(&store);
}

#[test]
fn credit_used_alerts_written_by_the_old_code_stay_and_covered_spends_add_none() {
    let db = load("v050-alerts");
    let store = AccountStore::open(db.path()).unwrap();
    let written_by_v0_5_0 = [
        ("m-a", 100),
        ("m-a", 200),
        ("m-b", 100),
        ("m-d", 50),
        ("m-e", 100),
        ("m-e", 200),
    ]
    .map(|(account, amount)| (account.to_string(), amount));
    assert_eq!(
        alerts(&store),
        written_by_v0_5_0,
        "read back as they were written"
    );

    // m-b 的新额度 100 覆盖的这一笔（预留 50）：没动授信，不再记告警；旧代码会记金额 150 的
    assert_eq!(spend(&store, "m-b", "b-cover", READ_AT, 20), Some(50));
    assert_eq!(alerts(&store), written_by_v0_5_0);
}
