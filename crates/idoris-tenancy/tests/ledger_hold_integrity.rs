//! 预留的完整性：**已结算的预留一定记着实际消费额**，读到没有金额的已结算预留是库损坏，失败即关闭，不能当成「收了 0」。
//!
//! 来历（pre-pr-check 对 `holds.rs` 的追问）：重放结算时，旧代码把已结算预留里 NULL 的 `charged_micros` 读成 0（`unwrap_or(0)`）。
//! 如果这样的行存在，重放 `settle(0)` 会得到 `AlreadySettled`，把一笔其实没有写下的结算（少收）当成已经做完。
//!
//! 这样的行**写不进去**：`ledger_holds` 的状态 CHECK（`migrations/0001_ledger.sql`）要求 `settled` 时 `charged_micros IS NOT NULL`，
//! 新预留只能以 `open` 开始，终态不可再改（触发器），唯一把预留关成 `settled` 的写入路径 `close_hold` 在同一条 UPDATE 里写下状态和金额。
//! 第一个测试用真实的库钉住这些约束；第二个测试**关掉约束检查**（`PRAGMA ignore_check_constraints`）伪造一行损坏的预留，
//! 证明读到它时账本失败即关闭、对账把它报出来，而不是当成 0。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::ids::IdempotencyKey;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{LedgerError, SettleResult};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

/// 一个已准入、还开着的预留（`r-1#0`，预留 210）。
fn open_hold(db: &TempDb) -> (AccountStore, Req) {
    let store = world(db);
    store
        .fund_account(&aid("emp"), &thb(1_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(store.admit_attempt(&req.input()).unwrap().is_admitted());
    (store, req)
}

#[test]
fn the_database_cannot_store_a_settled_or_released_hold_without_a_charged_amount() {
    let db = TempDb::new("hold-integrity-schema");
    let (store, req) = open_hold(&db);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    // 把开着的预留直接改成 settled、不给金额：状态 CHECK 拒绝
    for sql in [
        "UPDATE ledger_holds SET state = 'settled', closed_at_ms = 1 WHERE hold_id = 'r-1#0'",
        "UPDATE ledger_holds SET state = 'settled', closed_at_ms = 1, charged_micros = NULL WHERE hold_id = 'r-1#0'",
        "UPDATE ledger_holds SET state = 'released', closed_at_ms = 1 WHERE hold_id = 'r-1#0'",
    ] {
        let err = conn.execute(sql, []).unwrap_err().to_string();
        assert!(err.contains("CHECK constraint failed"), "{sql}: {err}");
    }
    // 新预留只能以 open 开始：不能直接插一个 settled 的
    let err = conn
        .execute(
            "INSERT INTO ledger_holds (hold_id, attempt_id, component, request_id, attempt_index, payer_account,
                                       credential_id, unit, reserved_micros, period, created_at_ms, expires_at_ms,
                                       state, closed_at_ms, charged_micros)
             SELECT 'r-9#0', 'r-9#0', 'model', 'r-9', 0, payer_account, credential_id, unit, reserved_micros, period,
                    created_at_ms, expires_at_ms, 'settled', 1, NULL
               FROM ledger_holds WHERE hold_id = 'r-1#0'",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("must start in the open state"), "{err}");
    // 经 Rust 层结算：状态和金额同时写下，金额就是结算的金额
    store
        .settle_attempt(&req.attempt, &price(70), ts(T0 + 1_000))
        .unwrap();
    let charged: Option<i64> = conn
        .query_row(
            "SELECT charged_micros FROM ledger_holds WHERE hold_id = 'r-1#0'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(charged, Some(70));
    // 重放同一笔：幂等
    let replay = store
        .settle_attempt(&req.attempt, &price(70), ts(T0 + 2_000))
        .unwrap();
    assert_eq!(replay.result, SettleResult::AlreadySettled);
    assert_clean(&store);
}

#[test]
fn a_settled_hold_without_a_charged_amount_is_corruption_not_a_charge_of_zero() {
    let db = TempDb::new("hold-integrity-corrupt");
    let (store, req) = open_hold(&db);
    // 伪造损坏：关掉这条连接的 CHECK 检查，把开着的预留改成「已结算、没有金额」。
    // （Rust 层、触发器、约束都写不出这样的行；只有关掉约束检查或直接改库文件才行。）
    {
        let conn = rusqlite::Connection::open(db.path()).unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        conn.execute(
            "UPDATE ledger_holds SET state = 'settled', closed_at_ms = 1 WHERE hold_id = 'r-1#0'",
            [],
        )
        .unwrap();
    }
    // 旧代码把 NULL 读成 0：重放 settle(0) 得到 AlreadySettled，掩盖一笔没有写下的结算
    for charged in [0, 70] {
        let err = store
            .settle_attempt(&req.attempt, &price(charged), ts(T0 + 1_000))
            .unwrap_err();
        assert!(
            matches!(&err, LedgerError::Corrupt(detail) if detail.contains("records no charged amount")),
            "charge {charged}: {err:?}"
        );
    }
    // 对账不把「没有金额」读成 0 悄悄对上
    let report = store.reconcile_with_attempts().unwrap();
    assert!(
        report
            .mismatches
            .iter()
            .any(|m| m.subject == "r-1#0" && m.detail.contains("records no charged amount")),
        "{:#?}",
        report.mismatches
    );
}
