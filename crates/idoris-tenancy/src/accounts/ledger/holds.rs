//! 预留（hold）的生命周期：创建、过期、结算、释放，以及它们写下的分录。
//!
//! 所有函数都在**调用方已经开着的写事务**里跑（收一个 `&Connection`，`Transaction` 可以解引用成它），
//! 自己不开事务、不提交。所以同一套逻辑既能在 `AccountStore` 自己的连接上跑（准入、单独结算），
//! 也能在 S4 `AttemptStore` 的连接上跑（和结算分录、请求关闭同一个事务，见 `hook.rs`）。
//!
//! # 一个预留的一生
//!
//! ```text
//! reserve ──▶ open ──┬─▶ settled   (结算：在途 -R，已花 +A)
//!                    ├─▶ released  (释放：在途 -R)
//!                    └─▶ expired   (TTL 到了：在途 -R，额度放回去)
//!                              ├─▶ settled  (迟到的结算：已花 +A，不再有在途可冲)
//!                              └─▶ released (迟到的释放：什么都不用写)
//! ```
//!
//! 每一步对付款账户链上的每个账户各写一条分录（沿所有祖先），每条分录的幂等键是
//! `<预留>|<类别>|<账户>`，同一步重放写不进第二次。预留一旦 `settled` / `released` 就冻结。
//!
//! **迟到是由时间决定的，不由有没有人扫过决定的**：`open` 的预留在 `now >= expires_at` 之后收到结算，
//! 先补一次过期（写 `expire` 分录），再按迟到处理。否则同一笔结算会因为扫描恰好有没有跑过而得到
//! 不同的归类（担保人承担 / 平台损失 / 冻结凭证），那是不可接受的不确定性。

use idoris_compute_contracts::account::Account;
use idoris_compute_contracts::ids::{AccountId, AttemptId, CredentialId, RequestId, TimestampMs};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::pricing::Price;
use idoris_compute_contracts::request::split_overage;
use rusqlite::{Connection, OptionalExtension, params};

use super::error::{LedgerError, sub};
use super::project::{credit_headroom, project_chain};
use super::types::{
    AlertKind, HoldRow, HoldState, LedgerConfig, PLATFORM_LOSS_ACCOUNT, SettleOutcome, SettleResult,
};
use crate::accounts::rows::load_ancestors;

const HOLD_COLUMNS: &str = "hold_id, attempt_id, component, request_id, attempt_index, payer_account, \
     credential_id, unit, reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros, \
     state, expired_at_ms, closed_at_ms, charged_micros, late";

/// 库里一个预留的内部视图（金额是 `i64` 微单位，单位在 `unit`）。
#[derive(Debug, Clone)]
pub(super) struct HoldRecord {
    pub hold_id: String,
    pub attempt_id: String,
    pub component: String,
    pub request_id: String,
    pub attempt_index: i64,
    pub payer: AccountId,
    pub credential: CredentialId,
    pub unit: Unit,
    pub reserved: i64,
    pub period: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub grace_floor: Option<i64>,
    pub state: HoldState,
    pub expired_at: Option<i64>,
    pub closed_at: Option<i64>,
    pub charged: Option<i64>,
    pub late: bool,
}

impl HoldRecord {
    pub(super) fn to_row(&self) -> Result<HoldRow, LedgerError> {
        let money = |micros: i64| Money::new(self.unit.clone(), micros);
        let attempt = self
            .attempt_id
            .parse::<AttemptId>()
            .map_err(|e| LedgerError::Corrupt(format!("hold attempt id: {e}")))?;
        if attempt.request().as_str() != self.request_id
            || i64::from(attempt.index().0) != self.attempt_index
        {
            return Err(LedgerError::Corrupt(format!(
                "hold {} names request {} index {} but its attempt id is {}",
                self.hold_id, self.request_id, self.attempt_index, self.attempt_id
            )));
        }
        Ok(HoldRow {
            hold_id: self.hold_id.clone(),
            attempt,
            component: self.component.clone(),
            payer: self.payer.clone(),
            credential: self.credential.clone(),
            reserved: money(self.reserved),
            period: self
                .period
                .parse()
                .map_err(|e| LedgerError::Corrupt(format!("hold period: {e}")))?,
            created_at: TimestampMs(self.created_at),
            expires_at: TimestampMs(self.expires_at),
            grace_floor: self.grace_floor.map(money),
            state: self.state,
            expired_at: self.expired_at.map(TimestampMs),
            closed_at: self.closed_at.map(TimestampMs),
            charged: self.charged.map(money),
            late: self.late,
        })
    }
}

fn read_hold(row: &rusqlite::Row<'_>) -> rusqlite::Result<HoldRecordRaw> {
    Ok(HoldRecordRaw {
        hold_id: row.get(0)?,
        attempt_id: row.get(1)?,
        component: row.get(2)?,
        request_id: row.get(3)?,
        attempt_index: row.get(4)?,
        payer: row.get(5)?,
        credential: row.get(6)?,
        unit: row.get(7)?,
        reserved: row.get(8)?,
        period: row.get(9)?,
        created_at: row.get(10)?,
        expires_at: row.get(11)?,
        grace_floor: row.get(12)?,
        state: row.get(13)?,
        expired_at: row.get(14)?,
        closed_at: row.get(15)?,
        charged: row.get(16)?,
        late: row.get::<_, i64>(17)? == 1,
    })
}

struct HoldRecordRaw {
    hold_id: String,
    attempt_id: String,
    component: String,
    request_id: String,
    attempt_index: i64,
    payer: String,
    credential: String,
    unit: String,
    reserved: i64,
    period: String,
    created_at: i64,
    expires_at: i64,
    grace_floor: Option<i64>,
    state: String,
    expired_at: Option<i64>,
    closed_at: Option<i64>,
    charged: Option<i64>,
    late: bool,
}

impl HoldRecordRaw {
    fn into_record(self) -> Result<HoldRecord, LedgerError> {
        let corrupt = |what: &str, e: &dyn std::fmt::Display| {
            LedgerError::Corrupt(format!("hold {}: {what}: {e}", self.hold_id))
        };
        Ok(HoldRecord {
            payer: AccountId::new(self.payer.clone()).map_err(|e| corrupt("payer", &e))?,
            credential: CredentialId::new(self.credential.clone())
                .map_err(|e| corrupt("credential", &e))?,
            unit: self.unit.parse().map_err(|e| corrupt("unit", &e))?,
            state: HoldState::parse(&self.state)?,
            hold_id: self.hold_id,
            attempt_id: self.attempt_id,
            component: self.component,
            request_id: self.request_id,
            attempt_index: self.attempt_index,
            reserved: self.reserved,
            period: self.period,
            created_at: self.created_at,
            expires_at: self.expires_at,
            grace_floor: self.grace_floor,
            expired_at: self.expired_at,
            closed_at: self.closed_at,
            charged: self.charged,
            late: self.late,
        })
    }
}

/// 按预留 id 取。
pub(super) fn load_hold(
    conn: &Connection,
    hold_id: &str,
) -> Result<Option<HoldRecord>, LedgerError> {
    conn.query_row(
        &format!("SELECT {HOLD_COLUMNS} FROM ledger_holds WHERE hold_id = ?1"),
        [hold_id],
        read_hold,
    )
    .optional()?
    .map(HoldRecordRaw::into_record)
    .transpose()
}

/// 一个请求下、序号小于 `below_index` 的、还没收尾的主要预留（切换候选时释放它们）。
fn open_model_holds_below(
    conn: &Connection,
    request: &str,
    below_index: i64,
) -> Result<Vec<String>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT hold_id FROM ledger_holds
          WHERE request_id = ?1 AND attempt_index < ?2 AND component = 'model'
            AND state IN ('open', 'expired')
          ORDER BY attempt_index",
    )?;
    let rows = stmt.query_map(params![request, below_index], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 一个尝试下还没收尾的附加成本预留。
pub(super) fn open_extra_holds(
    conn: &Connection,
    attempt: &str,
) -> Result<Vec<String>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT hold_id FROM ledger_holds
          WHERE attempt_id = ?1 AND component <> 'model' AND state IN ('open', 'expired')
          ORDER BY hold_id",
    )?;
    let rows = stmt.query_map([attempt], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 新预留的内容。
pub(super) struct NewHold<'a> {
    pub attempt: &'a AttemptId,
    pub component: &'a str,
    pub payer: &'a AccountId,
    pub credential: &'a CredentialId,
    pub unit: &'a Unit,
    pub reserved: i64,
    pub period: &'a str,
    pub now_ms: i64,
    pub expires_at_ms: i64,
    pub grace_floor: Option<i64>,
}

impl NewHold<'_> {
    pub(super) fn hold_id(&self) -> String {
        hold_id_of(self.attempt, self.component)
    }
}

/// 预留 id：主要预留就是尝试 id；附加成本是 `<尝试 id>~<component>`。
pub(super) fn hold_id_of(attempt: &AttemptId, component: &str) -> String {
    if component == super::types::MODEL_COMPONENT {
        attempt.to_string()
    } else {
        format!("{attempt}~{component}")
    }
}

/// 写预留行，并对付款账户链上的每个账户各写一条 `reserve` 分录（在途 `+R`，记在创建的时刻上）。
pub(super) fn insert_hold_and_reserve(
    conn: &Connection,
    chain: &[Account],
    hold: &NewHold<'_>,
) -> Result<(), LedgerError> {
    let hold_id = hold.hold_id();
    conn.execute(
        "INSERT INTO ledger_holds
            (hold_id, attempt_id, component, request_id, attempt_index, payer_account, credential_id,
             unit, reserved_micros, period, created_at_ms, expires_at_ms, grace_floor_micros, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'open')",
        params![
            hold_id,
            hold.attempt.to_string(),
            hold.component,
            hold.attempt.request().as_str(),
            i64::from(hold.attempt.index().0),
            hold.payer.as_str(),
            hold.credential.as_str(),
            hold.unit.to_string(),
            hold.reserved,
            hold.period,
            hold.now_ms,
            hold.expires_at_ms,
            hold.grace_floor,
        ],
    )?;
    for account in chain {
        insert_entry(
            conn,
            &EntryDraft {
                account: account.id.as_str(),
                hold: Some(&hold_id),
                kind: "reserve",
                unit: hold.unit,
                held: hold.reserved,
                spent: 0,
                funds: 0,
                liability: 0,
                window_at_ms: hold.now_ms,
                at_ms: hold.now_ms,
                key: format!("{hold_id}|reserve|{}", account.id),
            },
        )?;
    }
    Ok(())
}

/// 一条待写的分录。
pub(super) struct EntryDraft<'a> {
    pub account: &'a str,
    pub hold: Option<&'a str>,
    pub kind: &'static str,
    pub unit: &'a Unit,
    pub held: i64,
    pub spent: i64,
    pub funds: i64,
    pub liability: i64,
    pub window_at_ms: i64,
    pub at_ms: i64,
    pub key: String,
}

pub(super) fn insert_entry(conn: &Connection, entry: &EntryDraft<'_>) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO ledger_entries
            (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
             window_at_ms, at_ms, idempotency_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            entry.account,
            entry.hold,
            entry.kind,
            entry.unit.to_string(),
            entry.held,
            entry.spent,
            entry.funds,
            entry.liability,
            entry.window_at_ms,
            entry.at_ms,
            entry.key,
        ],
    )?;
    Ok(())
}

/// 沿付款账户链要写的一步：每个账户一条分录，数额相同。
struct ChainPosting {
    kind: &'static str,
    held: i64,
    spent: i64,
    window_at_ms: i64,
    at_ms: i64,
}

/// 对付款账户链上的每个账户各写一条分录（沿所有祖先）。
fn post_chain(
    conn: &Connection,
    chain: &[Account],
    hold: &HoldRecord,
    posting: &ChainPosting,
) -> Result<(), LedgerError> {
    for account in chain {
        insert_entry(
            conn,
            &EntryDraft {
                account: account.id.as_str(),
                hold: Some(&hold.hold_id),
                kind: posting.kind,
                unit: &hold.unit,
                held: posting.held,
                spent: posting.spent,
                funds: 0,
                liability: 0,
                window_at_ms: posting.window_at_ms,
                at_ms: posting.at_ms,
                key: format!("{}|{}|{}", hold.hold_id, posting.kind, account.id),
            },
        )?;
    }
    Ok(())
}

/// 告警记录。
pub(super) struct NewAlert<'a> {
    pub kind: AlertKind,
    pub account: &'a str,
    pub guarantor: Option<&'a str>,
    pub credential: Option<&'a str>,
    pub hold: Option<&'a str>,
    pub amount: i64,
    pub unit: &'a Unit,
    pub detail: &'a str,
    pub at_ms: i64,
}

pub(super) fn insert_alert(conn: &Connection, alert: &NewAlert<'_>) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO ledger_alerts
            (kind, account_id, guarantor_id, credential_id, hold_id, amount_micros, unit, detail, at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            alert.kind.as_str(),
            alert.account,
            alert.guarantor,
            alert.credential,
            alert.hold,
            alert.amount,
            alert.unit.to_string(),
            alert.detail,
            alert.at_ms,
        ],
    )?;
    Ok(())
}

/// 预留过期：额度放回去（在途 `-R`），状态改成 `expired`。之后仍可能有迟到的结算。
pub(super) fn expire_hold_in(
    conn: &Connection,
    hold: &HoldRecord,
    now_ms: i64,
) -> Result<(), LedgerError> {
    if hold.state != HoldState::Open {
        return Ok(());
    }
    let chain = load_ancestors(conn, &hold.payer)?;
    let release = hold.reserved.checked_neg().ok_or(LedgerError::Overflow)?;
    post_chain(
        conn,
        &chain,
        hold,
        &ChainPosting {
            kind: "expire",
            held: release,
            spent: 0,
            window_at_ms: hold.created_at,
            at_ms: now_ms,
        },
    )?;
    conn.execute(
        "UPDATE ledger_holds SET state = 'expired', expired_at_ms = ?2 WHERE hold_id = ?1 AND state = 'open'",
        params![hold.hold_id, hold.expires_at],
    )?;
    Ok(())
}

/// 扫掉所有已过期、还没收尾的预留（把它们占的额度放回去）。返回扫掉了几个。
pub(super) fn sweep_expired_in(conn: &Connection, now_ms: i64) -> Result<usize, LedgerError> {
    let ids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT hold_id FROM ledger_holds WHERE state = 'open' AND expires_at_ms <= ?1
              ORDER BY expires_at_ms, hold_id",
        )?;
        let rows = stmt.query_map([now_ms], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut swept = 0;
    for id in ids {
        if let Some(hold) = load_hold(conn, &id)? {
            expire_hold_in(conn, &hold, now_ms)?;
            swept += 1;
        }
    }
    Ok(swept)
}

fn close_hold(
    conn: &Connection,
    hold_id: &str,
    state: HoldState,
    now_ms: i64,
    charged: i64,
    late: bool,
) -> Result<(), LedgerError> {
    let changed = conn.execute(
        "UPDATE ledger_holds SET state = ?2, closed_at_ms = ?3, charged_micros = ?4, late = ?5
          WHERE hold_id = ?1 AND state IN ('open', 'expired')",
        params![hold_id, state.as_str(), now_ms, charged, i64::from(late)],
    )?;
    if changed != 1 {
        return Err(LedgerError::Corrupt(format!(
            "hold {hold_id} could not be closed (it was not open or expired)"
        )));
    }
    Ok(())
}

/// 宽限放行的预留最后没有收费：这次宽限作废，用户本周期还能再用。
fn void_grace_use(conn: &Connection, hold_id: &str, now_ms: i64) -> Result<(), LedgerError> {
    conn.execute(
        "UPDATE ledger_grace_uses SET voided_at_ms = ?2 WHERE hold_id = ?1 AND voided_at_ms IS NULL",
        params![hold_id, now_ms],
    )?;
    Ok(())
}

/// 冻结凭证。返回这次是否真的冻结了（凭证不在库里、已撤销、已冻结都返回 `false`）。
fn freeze_credential(conn: &Connection, credential: &str) -> Result<bool, LedgerError> {
    let changed = conn.execute(
        "UPDATE credentials SET status = 'frozen' WHERE credential_id = ?1 AND status = 'active'",
        [credential],
    )?;
    Ok(changed == 1)
}

fn blank_outcome(unit: &Unit, result: SettleResult, charged: i64, late: bool) -> SettleOutcome {
    let zero = || Money::zero(unit.clone());
    SettleOutcome {
        result,
        late,
        charged: Money::new(unit.clone(), charged),
        overage: zero(),
        sponsor_covered: zero(),
        platform_loss: zero(),
        grace_absorbed: zero(),
        credential_frozen: false,
        logs: Vec::new(),
    }
}

/// 结算一个预留（`charged` 微单位，0 = 释放）。**幂等**：同一个金额重放返回 `AlreadySettled` /
/// `AlreadyReleased`，什么都不写；金额对不上是错误，不会悄悄覆盖。
///
/// - 预留还活着（`open` 且没到 TTL）：在途 `-R`、已花 `+A`；`A > R` 的部分是 overage。
/// - 预留已经过期（`expired`，或 `open` 但已过 TTL）：迟到的结算。额度早已放回去，所以**整笔** `A` 都当 overage。
/// - overage 按 S0 的 [`split_overage`] 分：链上「剩余额度加授信」还能吸收的由担保人承担，超出的记入平台损失账户，
///   告警，并冻结这张凭证。能吸收多少是事务里现读的整条链在「钱」这个维度上的余量（见 [`credit_headroom`]）。
/// - 宽限放行的预留：账户自己的可用额度加授信之内的部分和普通请求一样；超出这个下限的部分（至多一次预留）
///   由平台承担，记 `grace_absorbed`，不冻结凭证（那是平台批准的一次例外，不是失控）。
pub(super) fn settle_hold_in(
    conn: &Connection,
    hold_id: &str,
    charged: i64,
    now_ms: i64,
    config: &LedgerConfig,
) -> Result<SettleOutcome, LedgerError> {
    if charged < 0 {
        return Err(LedgerError::Invalid(format!(
            "a charge must not be negative: {charged}"
        )));
    }
    let mut hold =
        load_hold(conn, hold_id)?.ok_or_else(|| LedgerError::HoldNotFound(hold_id.to_string()))?;
    match hold.state {
        HoldState::Settled => {
            let existing = hold.charged.unwrap_or(0);
            return if existing == charged {
                Ok(blank_outcome(
                    &hold.unit,
                    SettleResult::AlreadySettled,
                    existing,
                    hold.late,
                ))
            } else {
                Err(LedgerError::SettleConflict {
                    hold: hold_id.to_string(),
                    existing,
                    requested: charged,
                })
            };
        }
        HoldState::Released => {
            return if charged == 0 {
                Ok(blank_outcome(
                    &hold.unit,
                    SettleResult::AlreadyReleased,
                    0,
                    false,
                ))
            } else {
                Err(LedgerError::ChargeAfterRelease {
                    hold: hold_id.to_string(),
                    requested: charged,
                })
            };
        }
        HoldState::Open | HoldState::Expired => {}
    }

    // 迟到由时间决定：到了 TTL 的预留先补一次过期。
    if hold.state == HoldState::Open && now_ms >= hold.expires_at {
        expire_hold_in(conn, &hold, now_ms)?;
        hold.state = HoldState::Expired;
        hold.expired_at = Some(hold.expires_at);
    }
    let late = hold.state == HoldState::Expired;
    let chain = load_ancestors(conn, &hold.payer)?;
    if chain.is_empty() {
        return Err(LedgerError::Corrupt(format!(
            "payer account {} of hold {hold_id} no longer exists",
            hold.payer
        )));
    }

    // ---- 释放：没有钱要收 ----
    if charged == 0 {
        if hold.state == HoldState::Open {
            let release = hold.reserved.checked_neg().ok_or(LedgerError::Overflow)?;
            post_chain(
                conn,
                &chain,
                &hold,
                &ChainPosting {
                    kind: "release",
                    held: release,
                    spent: 0,
                    window_at_ms: hold.created_at,
                    at_ms: now_ms,
                },
            )?;
        }
        close_hold(conn, hold_id, HoldState::Released, now_ms, 0, false)?;
        if hold.grace_floor.is_some() {
            void_grace_use(conn, hold_id, now_ms)?;
        }
        return Ok(blank_outcome(&hold.unit, SettleResult::Released, 0, false));
    }

    // ---- 结算：先读（预留还占着或已放回去，正是 overage 要面对的状态），再写 ----
    let unit = hold.unit.clone();
    let views = project_chain(conn, &chain, now_ms, config)?;
    let headroom = credit_headroom(&views)?;
    let overage = if late {
        charged
    } else {
        sub(charged, hold.reserved)?.max(0)
    };
    // 整条链在「钱」这个维度上一点限制都没有（付款账户就是平台根）：没有谁需要承担，也没有损失。
    let absorbable = headroom.map_or(overage, |room| room.max(0));
    let split = split_overage(
        &Price::from_micros(unit.clone(), overage)?,
        &Price::from_micros(unit.clone(), absorbable)?,
    )?;
    let sponsor = split.sponsor.micros();
    let loss = split.platform_loss.micros();

    if late {
        post_chain(
            conn,
            &chain,
            &hold,
            &ChainPosting {
                kind: "late_settle",
                held: 0,
                spent: charged,
                window_at_ms: now_ms,
                at_ms: now_ms,
            },
        )?;
    } else {
        let release = hold.reserved.checked_neg().ok_or(LedgerError::Overflow)?;
        post_chain(
            conn,
            &chain,
            &hold,
            &ChainPosting {
                kind: "settle",
                held: release,
                spent: charged,
                window_at_ms: hold.created_at,
                at_ms: now_ms,
            },
        )?;
    }

    let guarantor = chain.get(1).map(|account| account.id.as_str());
    let mut outcome = blank_outcome(&unit, SettleResult::Settled, charged, late);
    outcome.overage = Money::new(unit.clone(), overage);
    outcome.sponsor_covered = Money::new(unit.clone(), sponsor);
    outcome.platform_loss = Money::new(unit.clone(), loss);

    if sponsor > 0
        && let Some(guarantor) = guarantor
    {
        insert_entry(
            conn,
            &EntryDraft {
                account: guarantor,
                hold: Some(&hold.hold_id),
                kind: "sponsor_covered",
                unit: &unit,
                held: 0,
                spent: 0,
                funds: 0,
                liability: sponsor,
                window_at_ms: now_ms,
                at_ms: now_ms,
                key: format!("{}|sponsor_covered|{guarantor}", hold.hold_id),
            },
        )?;
    }

    if loss > 0 {
        insert_entry(
            conn,
            &EntryDraft {
                account: PLATFORM_LOSS_ACCOUNT,
                hold: Some(&hold.hold_id),
                kind: "platform_loss",
                unit: &unit,
                held: 0,
                spent: 0,
                funds: 0,
                liability: loss,
                window_at_ms: now_ms,
                at_ms: now_ms,
                key: format!("{}|platform_loss|{PLATFORM_LOSS_ACCOUNT}", hold.hold_id),
            },
        )?;
        insert_alert(
            conn,
            &NewAlert {
                kind: AlertKind::PlatformLoss,
                account: hold.payer.as_str(),
                guarantor,
                credential: Some(hold.credential.as_str()),
                hold: Some(&hold.hold_id),
                amount: loss,
                unit: &unit,
                detail: if late {
                    "late_charge_beyond_credit"
                } else {
                    "overage_beyond_credit"
                },
                at_ms: now_ms,
            },
        )?;
        let frozen = freeze_credential(conn, hold.credential.as_str())?;
        outcome.credential_frozen = frozen;
        insert_alert(
            conn,
            &NewAlert {
                kind: if frozen {
                    AlertKind::CredentialFrozen
                } else {
                    AlertKind::FreezeNotApplied
                },
                account: hold.payer.as_str(),
                guarantor,
                credential: Some(hold.credential.as_str()),
                hold: Some(&hold.hold_id),
                amount: loss,
                unit: &unit,
                detail: if frozen {
                    "frozen_after_platform_loss"
                } else {
                    "credential_missing_revoked_or_already_frozen"
                },
                at_ms: now_ms,
            },
        )?;
        outcome.logs.push(format!(
            "[idoris] ledger ALERT platform_loss: payer={} credential={} hold={} loss={} {} credential_frozen={}",
            hold.payer, hold.credential, hold.hold_id, loss, unit, frozen
        ));
    }

    if late {
        insert_alert(
            conn,
            &NewAlert {
                kind: AlertKind::LateSettlement,
                account: hold.payer.as_str(),
                guarantor,
                credential: Some(hold.credential.as_str()),
                hold: Some(&hold.hold_id),
                amount: charged,
                unit: &unit,
                detail: "settled_after_reservation_expired",
                at_ms: now_ms,
            },
        )?;
        outcome.logs.push(format!(
            "[idoris] ledger late settlement: payer={} hold={} charged={} {}",
            hold.payer, hold.hold_id, charged, unit
        ));
    }

    if let Some(floor) = hold.grace_floor {
        let used = charged.min(hold.reserved);
        let absorbed = sub(used, floor)?.max(0);
        if absorbed > 0 {
            insert_entry(
                conn,
                &EntryDraft {
                    account: PLATFORM_LOSS_ACCOUNT,
                    hold: Some(&hold.hold_id),
                    kind: "grace_absorbed",
                    unit: &unit,
                    held: 0,
                    spent: 0,
                    funds: 0,
                    liability: absorbed,
                    window_at_ms: now_ms,
                    at_ms: now_ms,
                    key: format!("{}|grace_absorbed|{PLATFORM_LOSS_ACCOUNT}", hold.hold_id),
                },
            )?;
            outcome.grace_absorbed = Money::new(unit.clone(), absorbed);
        }
    }

    close_hold(conn, hold_id, HoldState::Settled, now_ms, charged, late)?;
    Ok(outcome)
}

/// 释放一个请求里序号小于 `below_index` 的、还没收尾的主要预留（候选切换：它们不会再是作答的那一次，
/// 用户对它们付 0）。在新候选的准入事务里调用，所以腾出来的额度立刻对新候选可见，不会让孩子因为上一次
/// 失败的尝试还占着额度而被拒。
pub(super) fn release_earlier_attempts(
    conn: &Connection,
    request: &RequestId,
    below_index: u8,
    now_ms: i64,
    config: &LedgerConfig,
) -> Result<usize, LedgerError> {
    let ids = open_model_holds_below(conn, request.as_str(), i64::from(below_index))?;
    let mut released = 0;
    for id in ids {
        settle_hold_in(conn, &id, 0, now_ms, config)?;
        released += 1;
    }
    Ok(released)
}
