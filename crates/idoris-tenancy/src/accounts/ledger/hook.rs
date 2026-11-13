//! 准入账本和 S4 尝试记录的对接：[`AttemptLedgerHook`] 的实现。
//!
//! # 两张账的对应关系
//!
//! 尝试记录（`compute_attempts` / `compute_postings`，S4）记**每次尝试**的预留和结算分录；账本（`ledger_holds` /
//! `ledger_entries`）记**付款账户链上每个账户**的在途与已花。两者一一对应，对应的键是尝试 id：
//!
//! | S4 | 账本 | 对应关系 |
//! |---|---|---|
//! | `compute_attempts.attempt_id` | `ledger_holds.hold_id`（`component = 'model'`） | 每个尝试恰好一个预留。尝试创建时由 [`attempt_created`](AttemptLedgerHook::attempt_created) 核对它存在、没过期、金额与付款账户一致，没有就拒绝创建 |
//! | `compute_attempts.reserved_price_micros` | `ledger_holds.reserved_micros` | 相等 |
//! | `compute_postings` 的 `user_price`（只有发布了的那次） | 预留的 `charged_micros`（`settled`） | 相等；没有 `user_price` 的尝试预留是 `released`（用户付 0） |
//! | `compute_postings` 的 `release`（价预留的释放） | 预留 `R - min(charged, R)` | 相等（发布了的那次释放没用掉的部分，其余尝试释放整笔） |
//!
//! 注意 `charged`（= S4 的 `user_price`）是这次的**实际消费**。宽限放行的预留里平台承担的那部分（`grace_absorbed`）
//! 不记到付款账户链上：链上每个账户的已花是 `charged - grace_absorbed`，平台承担的部分在平台损失账户里有一条
//! `grace_absorbed` 分录（`reconcile` 逐个预留核对这条守恒，见 `reconcile.rs`）。
//!
//! 收尾时两边**同一个事务**：[`request_finalized`](AttemptLedgerHook::request_finalized) 在 `finalize_request` 的事务里按
//! 同一份 `SettlementPlan` 结算账本，任何一边失败都整体回滚。账本一侧按预留幂等，所以重放同一次收尾不会重复扣，
//! 崩溃后用 S4 的恢复接口重新收尾也能把账本补上（那时预留可能已经过期，走迟到结算的规则）。
//!
//! 附加成本（比如安全检查，S6）是同一次尝试下的另一个 component；收尾时还没结算的附加成本预留按 0 释放并留一条
//! `extra_unsettled` 告警——调用方应当在收尾之前自己结算（见 `AccountStore::settle_component`）。
//!
//! 对账见 [`super::reconcile`]：它按上表逐项核对，测试里每个场景之后都跑一遍。

use idoris_compute_contracts::ids::TimestampMs;
use idoris_compute_contracts::request::{Attempt, SettlementPlan};
use rusqlite::{Connection, OptionalExtension};

use super::error::LedgerError;
use super::holds::{
    HoldRecord, NewAlert, insert_alert, load_hold, open_extra_holds, settle_hold_in,
};
use super::project::load_config;
use super::types::{AlertKind, HoldState};
use crate::attempts::AttemptLedgerHook;

/// 账本的结算钩子。没有状态：配置（账期时区、TTL）每次从库里读，所以安装在哪条连接上都一样。
#[derive(Debug, Default, Clone, Copy)]
pub struct LedgerAttemptHook;

impl AttemptLedgerHook for LedgerAttemptHook {
    fn attempt_created(
        &self,
        tx: &rusqlite::Transaction<'_>,
        attempt: &Attempt,
        at: TimestampMs,
    ) -> Result<(), String> {
        bind_attempt(tx, attempt, at).map_err(|e| e.to_string())
    }

    fn request_finalized(
        &self,
        tx: &rusqlite::Transaction<'_>,
        plan: &SettlementPlan,
        at: TimestampMs,
    ) -> Result<Vec<String>, String> {
        settle_plan(tx, plan, at).map_err(|e| e.to_string())
    }
}

/// 创建尝试时核对账本里有一个对得上的预留。
fn bind_attempt(conn: &Connection, attempt: &Attempt, at: TimestampMs) -> Result<(), LedgerError> {
    let attempt_id = attempt.id().to_string();
    let hold: HoldRecord = load_hold(conn, &attempt_id)?
        .ok_or_else(|| LedgerError::HoldNotFound(attempt_id.clone()))?;
    if hold.state != HoldState::Open {
        return Err(LedgerError::Invalid(format!(
            "the reservation for attempt {attempt_id} is {}, not open",
            hold.state.as_str()
        )));
    }
    if at.millis() >= hold.expires_at {
        return Err(LedgerError::Invalid(format!(
            "the reservation for attempt {attempt_id} expired before the attempt was created"
        )));
    }
    let price = &attempt.reservation().price;
    if hold.unit != *price.unit() || hold.reserved != price.micros() {
        return Err(LedgerError::Invalid(format!(
            "the reservation for attempt {attempt_id} ({} micro-{}) does not match the attempt's reserved price ({} micro-{})",
            hold.reserved,
            hold.unit,
            price.micros(),
            price.unit()
        )));
    }
    let request_payer: Option<String> = conn
        .query_row(
            "SELECT payer_account FROM compute_requests WHERE request_id = ?1",
            [attempt.id().request().as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if request_payer.as_deref() != Some(hold.payer.as_str()) {
        return Err(LedgerError::Invalid(format!(
            "the reservation for attempt {attempt_id} was taken on account {}, not the request's payer",
            hold.payer
        )));
    }
    Ok(())
}

/// 按结算计划把每个尝试的预留结算 / 释放：发布了的那次按向用户收的钱结算，其余释放。幂等。
pub(super) fn settle_plan(
    conn: &Connection,
    plan: &SettlementPlan,
    at: TimestampMs,
) -> Result<Vec<String>, LedgerError> {
    let config = load_config(conn)?;
    let mut logs = Vec::new();
    for line in &plan.lines {
        let attempt_id = line.attempt.to_string();
        let charged = match &plan.user_charge {
            Some(charge) if charge.attempt == line.attempt => Some(charge.charged.clone()),
            _ => None,
        };
        let hold = load_hold(conn, &attempt_id)?
            .ok_or_else(|| LedgerError::HoldNotFound(attempt_id.clone()))?;
        if let Some(charged) = &charged
            && charged.unit() != &hold.unit
        {
            return Err(LedgerError::Invalid(format!(
                "the charge for attempt {attempt_id} is in {} but its reservation is in {}",
                charged.unit(),
                hold.unit
            )));
        }
        let outcome = settle_hold_in(
            conn,
            &attempt_id,
            charged.as_ref().map_or(0, |c| c.micros()),
            at.millis(),
            &config,
        )?;
        logs.extend(outcome.logs);
        for extra in open_extra_holds(conn, &attempt_id)? {
            let Some(extra_hold) = load_hold(conn, &extra)? else {
                continue;
            };
            let outcome = settle_hold_in(conn, &extra, 0, at.millis(), &config)?;
            logs.extend(outcome.logs);
            insert_alert(
                conn,
                &NewAlert {
                    kind: AlertKind::ExtraUnsettled,
                    account: extra_hold.payer.as_str(),
                    guarantor: None,
                    credential: Some(extra_hold.credential.as_str()),
                    hold: Some(&extra),
                    amount: extra_hold.reserved,
                    unit: &extra_hold.unit,
                    detail: "extra_component_unsettled_released",
                    at_ms: at.millis(),
                },
            )?;
            logs.push(format!(
                "[idoris] ledger ALERT extra_unsettled: hold={extra} reserved={} {} (released unsettled at request close)",
                extra_hold.reserved, extra_hold.unit
            ));
        }
    }
    Ok(logs)
}
