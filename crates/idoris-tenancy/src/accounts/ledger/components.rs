//! 附加成本的预留与结算（给 S6 儿童安全用）：一次尝试在模型调用之外还有别的、也要计入准入和结算的成本，
//! 比如输入检查、完整输出检查要调用的安全服务（设计稿 §7：「安全检查的成本计入准入和结算」）。
//!
//! # 接口
//!
//! 每个附加成本是同一个尝试下的一个 **component**（稳定的小写字符串，`[a-z0-9_]{1,32}`，不能叫 `model`，
//! 比如 `safety_input`、`safety_output`），有自己的预留，和主要预留（`model`）走同一套机制：
//!
//! ```text
//! // 上游调用之前（或之后）要调用安全服务：先预留它的预估成本。五项检查沿付款账户的所有祖先，
//! // 和主要预留一样原子；不过就返回拒绝原因码，没有占任何额度——调用方此时应当放弃这次请求（失败即拒绝）。
//! match store.reserve_component(&attempt, "safety_input", &estimate, now)? {
//!     ComponentDecision::Reserved { .. } => { /* 调用安全服务 */ }
//!     ComponentDecision::Rejected(code) => { /* 拒绝，原因码给响应头 */ }
//! }
//! // 调用完之后按实际成本结算（0 = 释放）。幂等：同一个金额重放什么都不写。
//! store.settle_component(&attempt, "safety_input", &actual, now)?;
//! ```
//!
//! 附加成本的预留只会放行或拒绝，**不**降级、**不**用宽限：它是平台要做的检查，不是用户能用更少 token 换来的东西。
//! 附加成本的 overage（实际超过预留）和主要预留同一条规则（担保人承担 / 平台损失 / 冻结凭证）。
//!
//! 请求收尾时，该尝试下还没结算的附加成本按 0 释放，并留一条 `extra_unsettled` 告警（见 `hook.rs`）：
//! S6 应当在收尾之前自己结算；没结算就是少收，告警是让这件事看得见。

use idoris_compute_contracts::ids::{AttemptId, TimestampMs};
use idoris_compute_contracts::pricing::Price;

use super::admit::check_extra;
use super::error::{LedgerError, add};
use super::holds::{
    NewHold, hold_id_of, insert_hold_and_reserve, load_hold, settle_hold_in, sweep_expired_in,
};
use super::project::load_config;
use super::types::{ComponentDecision, HoldState, MODEL_COMPONENT, SettleOutcome};
use rusqlite::Connection;

/// component 名：小写字母、数字、下划线，1 到 32 个字符，不能是主要预留的名字。
pub(super) fn validate_component(component: &str) -> Result<(), LedgerError> {
    let valid = !component.is_empty()
        && component.len() <= 32
        && component
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && component != MODEL_COMPONENT;
    if valid {
        Ok(())
    } else {
        Err(LedgerError::Invalid(format!(
            "component {component:?} must be 1-32 characters of [a-z0-9_] and not {MODEL_COMPONENT:?}"
        )))
    }
}

pub(super) fn reserve_component_in(
    conn: &Connection,
    attempt: &AttemptId,
    component: &str,
    amount: &Price,
    now: TimestampMs,
) -> Result<ComponentDecision, LedgerError> {
    validate_component(component)?;
    let config = load_config(conn)?;
    let now_ms = now.millis();
    sweep_expired_in(conn, now_ms)?;
    let main_id = hold_id_of(attempt, MODEL_COMPONENT);
    let main =
        load_hold(conn, &main_id)?.ok_or_else(|| LedgerError::HoldNotFound(main_id.clone()))?;
    if main.state != HoldState::Open {
        return Err(LedgerError::Invalid(format!(
            "the reservation of attempt {attempt} is {}; an extra cost can only be reserved while it is open",
            main.state.as_str()
        )));
    }
    if amount.unit() != &main.unit {
        return Err(LedgerError::Invalid(format!(
            "the extra cost is in {} but attempt {attempt} pays in {}",
            amount.unit(),
            main.unit
        )));
    }
    let hold_id = hold_id_of(attempt, component);
    if load_hold(conn, &hold_id)?.is_some() {
        return Err(LedgerError::HoldExists(hold_id));
    }
    let (chain, rejection) = check_extra(conn, &main.payer, amount, now_ms, &config)?;
    if let Some(code) = rejection {
        return Ok(ComponentDecision::Rejected(code));
    }
    insert_hold_and_reserve(
        conn,
        &chain,
        &NewHold {
            attempt,
            component,
            payer: &main.payer,
            credential: &main.credential,
            unit: &main.unit,
            reserved: amount.micros(),
            period: &main.period,
            now_ms,
            expires_at_ms: add(now_ms, config.reservation_ttl_ms)?,
            grace_floor: None,
        },
    )?;
    Ok(ComponentDecision::Reserved { hold_id })
}

pub(super) fn settle_component_in(
    conn: &Connection,
    attempt: &AttemptId,
    component: &str,
    charged: &Price,
    now: TimestampMs,
) -> Result<SettleOutcome, LedgerError> {
    validate_component(component)?;
    let hold_id = hold_id_of(attempt, component);
    let hold =
        load_hold(conn, &hold_id)?.ok_or_else(|| LedgerError::HoldNotFound(hold_id.clone()))?;
    if charged.unit() != &hold.unit {
        return Err(LedgerError::Invalid(format!(
            "the charge is in {} but the reservation is in {}",
            charged.unit(),
            hold.unit
        )));
    }
    let config = load_config(conn)?;
    settle_hold_in(conn, &hold_id, charged.micros(), now.millis(), &config)
}
