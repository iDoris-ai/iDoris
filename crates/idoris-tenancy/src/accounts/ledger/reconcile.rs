//! 账本和 S4 尝试记录的对账，以及账本自己的守恒检查。
//!
//! 对账必须能同时回答两个问题：
//!
//! 1. **两张账对得上吗**（账本 ↔ S4 的 `compute_attempts` / `compute_postings`，对应关系见 `hook.rs` 的表）；
//! 2. **账本自己守恒吗**（每个预留在付款账户链上每个账户的分录之和，和预留的状态、金额一致）。
//!
//! 它只读，不修复任何东西：发现不一致只是列出来（`Mismatch`），怎么处理（补账、冲正、人工核对）是运维的事，
//! 和 `docs/design/compute-service-ops.md` §7 的恢复后对账是同一个思路——**不自动改账，不靠把旧分录再放一遍**。
//! 恢复后（从备份恢复、WAL 回放之后）先跑它：重复结算、漏记、预留和尝试对不上，都在这里现形。

use std::collections::BTreeSet;

use idoris_compute_contracts::ids::AccountId;
use rusqlite::Connection;

use super::error::LedgerError;
use super::types::{Mismatch, ReconcileReport};
use crate::accounts::rows::load_ancestors;

fn mismatch(subject: &str, detail: impl Into<String>) -> Mismatch {
    Mismatch {
        subject: subject.to_string(),
        detail: detail.into(),
    }
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, LedgerError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?)
}

/// 对账。账本自己的守恒检查总是做；和 S4 尝试记录的对照只在尝试表存在时做。
pub(super) fn reconcile_in(conn: &Connection) -> Result<ReconcileReport, LedgerError> {
    let mut report = ReconcileReport::default();
    check_conservation(conn, &mut report)?;
    check_grace(conn, &mut report)?;
    if table_exists(conn, "compute_attempts")? {
        check_against_attempts(conn, &mut report)?;
    }
    Ok(report)
}

/// 每个预留在付款账户链上每个账户的分录之和：在途 = （open ? R : 0），已花 = （settled ? 实际金额 : 0）；
/// 并且每个预留的分录恰好覆盖付款账户链上的每个账户。
fn check_conservation(conn: &Connection, report: &mut ReconcileReport) -> Result<(), LedgerError> {
    struct Row {
        hold: String,
        state: String,
        reserved: i64,
        charged: i64,
        payer: String,
        account: String,
        held: i64,
        spent: i64,
    }
    let rows: Vec<Row> = {
        let mut stmt = conn.prepare(
            "SELECT h.hold_id, h.state, h.reserved_micros, COALESCE(h.charged_micros, 0), h.payer_account,
                    e.account_id, SUM(e.held_micros), SUM(e.spent_micros)
               FROM ledger_holds h
               JOIN ledger_entries e ON e.hold_id = h.hold_id
              WHERE e.kind IN ('reserve', 'settle', 'release', 'expire', 'late_settle')
              GROUP BY h.hold_id, e.account_id
              ORDER BY h.hold_id, e.account_id",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok(Row {
                hold: row.get(0)?,
                state: row.get(1)?,
                reserved: row.get(2)?,
                charged: row.get(3)?,
                payer: row.get(4)?,
                account: row.get(5)?,
                held: row.get(6)?,
                spent: row.get(7)?,
            })
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    let mut accounts_by_hold: std::collections::BTreeMap<String, (String, BTreeSet<String>)> =
        std::collections::BTreeMap::new();
    for row in &rows {
        let expected_held = if row.state == "open" { row.reserved } else { 0 };
        let expected_spent = if row.state == "settled" {
            row.charged
        } else {
            0
        };
        if row.held != expected_held {
            report.mismatches.push(mismatch(
                &row.hold,
                format!(
                    "account {}: in-flight entries sum to {} but a {} hold of {} should leave {}",
                    row.account, row.held, row.state, row.reserved, expected_held
                ),
            ));
        }
        if row.spent != expected_spent {
            report.mismatches.push(mismatch(
                &row.hold,
                format!(
                    "account {}: spent entries sum to {} but a {} hold charged {} should leave {}",
                    row.account, row.spent, row.state, row.charged, expected_spent
                ),
            ));
        }
        accounts_by_hold
            .entry(row.hold.clone())
            .or_insert_with(|| (row.payer.clone(), BTreeSet::new()))
            .1
            .insert(row.account.clone());
    }
    for (hold, (payer, posted)) in &accounts_by_hold {
        let expected: BTreeSet<String> = match AccountId::new(payer.clone()) {
            Ok(payer) => load_ancestors(conn, &payer)?
                .into_iter()
                .map(|account| account.id.to_string())
                .collect(),
            Err(_) => BTreeSet::new(),
        };
        if *posted != expected {
            report.mismatches.push(mismatch(
                hold,
                format!("entries cover accounts {posted:?} but the payer chain is {expected:?}"),
            ));
        }
    }
    report.holds_checked = accounts_by_hold.len();
    Ok(())
}

/// 宽限：没作废的宽限，预留不能是已释放的；作废了的，预留必须是已释放的。
fn check_grace(conn: &Connection, report: &mut ReconcileReport) -> Result<(), LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT g.hold_id, g.voided_at_ms IS NOT NULL, h.state, COALESCE(h.charged_micros, 0)
           FROM ledger_grace_uses g JOIN ledger_holds h ON h.hold_id = g.hold_id",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (hold, voided, state, charged) in rows {
        let consistent = if voided {
            state == "released"
        } else {
            state != "released" && !(state == "settled" && charged == 0)
        };
        if !consistent {
            report.mismatches.push(mismatch(
                &hold,
                format!(
                    "grace use is {} but the hold is {state} (charged {charged})",
                    if voided { "voided" } else { "active" }
                ),
            ));
        }
    }
    Ok(())
}

/// 账本 ↔ S4：每个尝试恰好一个预留；金额、状态、结算额、释放额对得上（对应关系见 `hook.rs`）。
fn check_against_attempts(
    conn: &Connection,
    report: &mut ReconcileReport,
) -> Result<(), LedgerError> {
    {
        let mut stmt = conn.prepare(
            "SELECT a.attempt_id FROM compute_attempts a
               LEFT JOIN ledger_holds h ON h.hold_id = a.attempt_id AND h.component = 'model'
              WHERE h.hold_id IS NULL ORDER BY a.attempt_id",
        )?;
        let missing = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for attempt in missing {
            report.mismatches.push(mismatch(
                &attempt,
                "the attempt has no reservation in the ledger",
            ));
        }
    }
    struct Row {
        attempt: String,
        reserved_price: i64,
        price_unit: String,
        closed: bool,
        hold_state: String,
        hold_reserved: i64,
        hold_unit: String,
        hold_charged: i64,
        user_price: i64,
        released: i64,
    }
    let rows: Vec<Row> = {
        let mut stmt = conn.prepare(
            "SELECT a.attempt_id, a.reserved_price_micros, a.reserved_price_unit, r.outcome IS NOT NULL,
                    h.state, h.reserved_micros, h.unit, COALESCE(h.charged_micros, 0),
                    COALESCE((SELECT SUM(p.amount_micros) FROM compute_postings p
                               WHERE p.attempt_id = a.attempt_id AND p.kind = 'user_price'), 0),
                    COALESCE((SELECT SUM(p.amount_micros) FROM compute_postings p
                               WHERE p.attempt_id = a.attempt_id AND p.kind = 'release'), 0)
               FROM compute_attempts a
               JOIN compute_requests r ON r.request_id = a.request_id
               JOIN ledger_holds h ON h.hold_id = a.attempt_id AND h.component = 'model'
              ORDER BY a.attempt_id",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok(Row {
                attempt: row.get(0)?,
                reserved_price: row.get(1)?,
                price_unit: row.get(2)?,
                closed: row.get(3)?,
                hold_state: row.get(4)?,
                hold_reserved: row.get(5)?,
                hold_unit: row.get(6)?,
                hold_charged: row.get(7)?,
                user_price: row.get(8)?,
                released: row.get(9)?,
            })
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    for row in rows {
        if row.reserved_price != row.hold_reserved || row.price_unit != row.hold_unit {
            report.mismatches.push(mismatch(
                &row.attempt,
                format!(
                    "the attempt reserved {} micro-{} but the ledger hold is {} micro-{}",
                    row.reserved_price, row.price_unit, row.hold_reserved, row.hold_unit
                ),
            ));
        }
        if !row.closed {
            // 请求还开着：预留不能已经按金额结算（释放可能发生：候选切换时上一次尝试的预留已经放掉了）。
            if row.hold_state == "settled" {
                report.mismatches.push(mismatch(
                    &row.attempt,
                    "the request is still open but the ledger already settled the reservation",
                ));
            }
            continue;
        }
        if !matches!(row.hold_state.as_str(), "settled" | "released") {
            report.mismatches.push(mismatch(
                &row.attempt,
                format!(
                    "the request is closed but the ledger reservation is still {}",
                    row.hold_state
                ),
            ));
            continue;
        }
        if row.hold_charged != row.user_price {
            report.mismatches.push(mismatch(
                &row.attempt,
                format!(
                    "the ledger charged {} but the attempt records a user_price of {}",
                    row.hold_charged, row.user_price
                ),
            ));
        }
        let expected_release = row.reserved_price - row.user_price.min(row.reserved_price);
        if row.released != expected_release {
            report.mismatches.push(mismatch(
                &row.attempt,
                format!(
                    "the attempt records releases of {} but reserved {} minus charged {} leaves {}",
                    row.released, row.reserved_price, row.user_price, expected_release
                ),
            ));
        }
    }
    Ok(())
}
