//! 读：预留、分录、告警、宽限记录的视图（测试、对账、运维看账）。只读，不改任何东西。

use idoris_compute_contracts::ids::{AccountId, BillingMonth, TimestampMs};
use idoris_compute_contracts::money::{Money, Unit};
use rusqlite::{Connection, params};

use super::error::LedgerError;
use super::types::{AlertRow, EntryRow, GraceUseRow};

fn unit_of(text: &str) -> Result<Unit, LedgerError> {
    text.parse()
        .map_err(|e| LedgerError::Corrupt(format!("unit {text:?}: {e}")))
}

/// 一个预留的全部分录，按写入顺序。
pub(super) fn entries_of_hold(
    conn: &Connection,
    hold_id: &str,
) -> Result<Vec<EntryRow>, LedgerError> {
    entries_where(conn, "hold_id = ?1", hold_id)
}

/// 一个账户（或 `platform-loss`）的全部分录，按写入顺序。
pub(super) fn entries_of_account(
    conn: &Connection,
    account: &str,
) -> Result<Vec<EntryRow>, LedgerError> {
    entries_where(conn, "account_id = ?1", account)
}

fn entries_where(
    conn: &Connection,
    condition: &'static str,
    value: &str,
) -> Result<Vec<EntryRow>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT entry_id, account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros,
                liability_micros, window_at_ms, at_ms, idempotency_key
           FROM ledger_entries WHERE {condition} ORDER BY entry_id"
    ))?;
    let rows = stmt
        .query_map([value], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, String>(11)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(
            |(
                entry_id,
                account,
                hold,
                kind,
                unit,
                held,
                spent,
                funds,
                liability,
                window_at,
                at,
                key,
            )| {
                Ok(EntryRow {
                    entry_id,
                    account,
                    hold,
                    kind,
                    unit: unit_of(&unit)?,
                    held,
                    spent,
                    funds,
                    liability,
                    window_at: TimestampMs(window_at),
                    at: TimestampMs(at),
                    idempotency_key: key,
                })
            },
        )
        .collect()
}

/// 告警，按写入顺序；`kind` 给了就只要这一种。
pub(super) fn alerts_in(
    conn: &Connection,
    kind: Option<&str>,
) -> Result<Vec<AlertRow>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT alert_id, kind, account_id, guarantor_id, credential_id, hold_id, amount_micros, unit,
                detail, at_ms
           FROM ledger_alerts WHERE (?1 IS NULL OR kind = ?1) ORDER BY alert_id",
    )?;
    let rows = stmt
        .query_map(params![kind], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(
            |(alert_id, kind, account, guarantor, credential, hold, amount, unit, detail, at)| {
                Ok(AlertRow {
                    alert_id,
                    kind,
                    account,
                    guarantor,
                    credential,
                    hold,
                    amount: Money::new(unit_of(&unit)?, amount),
                    detail,
                    at: TimestampMs(at),
                })
            },
        )
        .collect()
}

/// 一个账户的全部宽限记录（含已作废的），按时间。
pub(super) fn grace_uses_in(
    conn: &Connection,
    account: &AccountId,
) -> Result<Vec<GraceUseRow>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT period, hold_id, unit, reserve_micros, shortfall_micros, used_at_ms, voided_at_ms
           FROM ledger_grace_uses WHERE account_id = ?1 ORDER BY use_id",
    )?;
    let rows = stmt
        .query_map([account.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(
            |(period, hold, unit, reserve, shortfall, used_at, voided_at)| {
                let unit = unit_of(&unit)?;
                Ok(GraceUseRow {
                    account: account.clone(),
                    period: period
                        .parse::<BillingMonth>()
                        .map_err(|e| LedgerError::Corrupt(format!("grace period: {e}")))?,
                    hold,
                    reserve: Money::new(unit.clone(), reserve),
                    shortfall: Money::new(unit, shortfall),
                    used_at: TimestampMs(used_at),
                    voided_at: voided_at.map(TimestampMs),
                })
            },
        )
        .collect()
}

/// 某一种分录的某个账户的总和（比如 `platform_loss` 在 `platform-loss` 上的 `liability`）。
pub(super) fn liability_total(
    conn: &Connection,
    account: &str,
    kind: &str,
) -> Result<i64, LedgerError> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(liability_micros), 0) FROM ledger_entries WHERE account_id = ?1 AND kind = ?2",
        params![account, kind],
        |row| row.get(0),
    )?)
}
