//! 从 v0.5.0 的库升级（issue #601：充过值的账户也按方案 C 记账）。
//!
//! 记账口径是账本分录、额度、额度序号上的**投影**（`ledger/project.rs`），没有新的列、表、触发器，所以记账口径本身不需要迁移：
//! v0.5.0 写下的库用新代码打开，已写的每一行都不变，变的只是同一批分录怎么读。唯一的结构变化是迁移 0005（Codex 四复审 Low）：
//! 一条只加索引的迁移（`settle` 按结算时刻，补贴池一个账期的消耗不用把池账户的全部历史读一遍），不改任何列、表、触发器。
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
use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AlertKind, CreditLineSetting};
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
fn opening_a_v0_5_0_ledger_adds_one_index_and_rewrites_no_row() {
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
        [1, 2, 3, 4, 5],
        "the funded books are a projection; the one new migration is the settlement-time index"
    );
    assert_eq!(versions(db.path(), "account_schema_migrations"), [1, 2]);
    let mut expected_objects = objects_before;
    expected_objects.push("index ledger_entries_settled_by_account_at".to_string());
    expected_objects.sort();
    assert_eq!(
        objects(db.path()),
        expected_objects,
        "the only schema change is that one index: no table, column or trigger was added or dropped"
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

#[test]
fn an_unexpired_overage_that_v0_5_0_booked_whole_at_the_reservation_is_split_when_it_is_read() {
    // v0.5.0 把没迟到的结算里超出预留的部分和预留之内的部分一起记在预留创建的时刻（一条 `settle`，已花是整笔）。现在的代码把超出的部分
    // 另记在结算那一刻（一条 `late_settle`，Codex 复审 2026-11-22 High 1）。**已经写下的行一行都不改**，但读它们的时候按同一个口径解释：
    // 旧格式 `settle` 里 `charged - reserved` 的正差是一笔单独的逻辑花费，用这条 `settle` 自己的 `at_ms`（结算时刻）和 `entry_id`
    // （结算的提交序号），预留之内的部分仍在预留创建的时刻。不迁移，不改写历史。
    // 夹具里没有 overage 的结算（所有结算都等于预留），所以这里用现在的代码写一笔，再按 v0.5.0 的写法改回去（拆掉只追加的触发器：
    // 把 `settle` 的已花改成整笔 300，删掉 `late_settle`）。
    //
    // 充值 100、授信 100，额度 100 的窗口从 T0 + 10 起（T0 + 5 创建）；T0 + 9 预留 200，T0 + 10 按 300 结算（超出 100）。
    // 读的时候：预留之内的 300 - 100 = 200 在 T0 + 9，额度够不着：自有资金 100 + 授信债务 100；超出的 100 在 T0 + 10（这条 `settle`
    // 的 `at_ms`），额度的窗口包含它、额度是在它之前创建的：额度覆盖，剩余 0。净头寸 0 + 0 - 100 = -100，能花的 -100 + 授信 100 = 0。
    // （只按整笔 300 记在 T0 + 9 来读：债务 200、额度还剩 100，净头寸 0，能花 100，下一笔 100 照样放行——额度被吸收一次又被花一次。）
    let db = TempDb::new("v050-legacy-overage");
    let store = world(&db);
    store
        .fund_account(
            &aid("paid"),
            &thb(1_000_000),
            &IdempotencyKey::new("f-paid").unwrap(),
            t0(),
        )
        .unwrap();
    allocate(&store, "e-0", "platform", "paid", 9_000_000);
    store
        .fund_account(
            &aid("emp"),
            &thb(100),
            &IdempotencyKey::new("f-emp").unwrap(),
            t0(),
        )
        .unwrap();
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(100),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    store
        .create_allocation(
            &Allocation {
                id: AllocationId::new("emp-new").unwrap(),
                parent: aid("paid"),
                child: aid("emp"),
                ceiling: grant(100, Purpose::Prepaid, T0 + 10, T0 + 10 + 30 * DAY),
                idempotency_key: None,
            },
            ts(T0 + 5),
        )
        .unwrap();
    let first = Req::new(&store, "r-1", 0, "cred-e", 95).at(T0 + 9);
    assert_eq!(
        store
            .admit_attempt(&first.input())
            .unwrap()
            .admitted()
            .map(|admitted| admitted.reservation.price.micros()),
        Some(200)
    );
    store
        .settle_attempt(&first.attempt, &price(300), ts(T0 + 10))
        .unwrap();
    drop(store);

    // 改回 v0.5.0 写下的样子
    rewrite_overage_as_v0_5_0(&db, "r-1#0");

    let store = AccountStore::open(db.path()).unwrap();
    let kinds: Vec<(String, i64, i64, i64)> = store
        .entries_of_hold(&first.attempt, "model")
        .unwrap()
        .into_iter()
        .filter(|entry| entry.account == "emp")
        .map(|entry| {
            (
                entry.kind,
                entry.held,
                entry.spent,
                entry.window_at.millis(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("reserve".to_string(), 200, 0, T0 + 9),
            ("settle".to_string(), -200, 300, T0 + 9),
        ],
        "the v0.5.0 shape: the overage is inside the settle entry, at the reservation's moment"
    );
    let v = store.account_view(&aid("emp"), ts(T0 + 11)).unwrap();
    assert_eq!(v.balance, None);
    assert_eq!(
        v.ceiling_remaining.map(|m| m.micros),
        Some(-100),
        "the 100 beyond the reservation is a spend of its own at T0 + 10: allowance 0 + funds 0 - debt 100"
    );
    assert_eq!(v.credit_limit, thb(100));
    // 端到端：能花的是 0，任何一笔正的预留都过不了（旧读法：能花 100，一笔 100 的预留照样放行）
    let next = Req::new(&store, "r-2", 0, "cred-e", 45).at(T0 + 12);
    let decision = store.admit_attempt(&next.input()).unwrap();
    assert!(
        matches!(&decision, AdmissionDecision::Reject { reason } if reason.code() == ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
    let smallest = Req::new(&store, "r-3", 0, "cred-e", 10).at(T0 + 13);
    let decision = store.admit_attempt(&smallest.input()).unwrap();
    assert!(
        matches!(&decision, AdmissionDecision::Reject { reason } if reason.code() == ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
    let laddered = Req::new(&store, "r-4", 0, "cred-e", 10).at(T0 + 14).grace();
    let decision = store.admit_attempt(&laddered.input()).unwrap();
    assert!(
        matches!(&decision, AdmissionDecision::Reject { reason } if reason.code() == ReasonCode::BudgetExhausted),
        "an account that owes credit gets no grace round: {decision:?}"
    );
    assert_eq!(
        store
            .entries_of_hold(&first.attempt, "model")
            .unwrap()
            .into_iter()
            .filter(|entry| entry.account == "emp")
            .map(|entry| (
                entry.kind,
                entry.held,
                entry.spent,
                entry.window_at.millis()
            ))
            .collect::<Vec<_>>(),
        [
            ("reserve".to_string(), 200, 0, T0 + 9),
            ("settle".to_string(), -200, 300, T0 + 9),
        ],
        "reading the old rows rewrote none of them"
    );
    assert_clean(&store);
}
