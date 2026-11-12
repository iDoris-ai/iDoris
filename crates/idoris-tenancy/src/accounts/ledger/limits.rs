//! 账户的限额与资金：授信（担保人的开关）、速率闸门与在途上限、补贴池的账期总上限、充值。
//!
//! 这些是 S2（控制面）要接 HTTP 的存储层函数。**这里只校验结构**（账户存在、单位一致、数额非负、
//! 授信的担保人就是父账户），**不做授权**：谁有权给哪个账户开授信、改限额，是 S2 的「操作 × 资源范围」
//! 检查和审计的事。每一次设置都是追加一行新版本（最新一行生效，只追加不可改），所以历史永远查得到。

use idoris_compute_contracts::account::{Account, AccountKind};
use idoris_compute_contracts::ids::{AccountId, BillingMonth, IdempotencyKey, TimestampMs};
use idoris_compute_contracts::money::Money;
use rusqlite::{Connection, OptionalExtension, params};

use super::error::LedgerError;
use super::holds::{EntryDraft, insert_entry};
use super::types::{AccountLimits, CreditLine, CreditLineSetting, FundOutcome};
use crate::accounts::AccountStoreError;
use crate::accounts::rows::load_account;

fn load_existing(conn: &Connection, account: &AccountId) -> Result<Account, LedgerError> {
    load_account(conn, account)?
        .ok_or_else(|| AccountStoreError::not_found("account", account.as_str()).into())
}

fn require_non_negative(what: &str, money: &Money) -> Result<(), LedgerError> {
    if money.micros < 0 {
        return Err(LedgerError::Invalid(format!(
            "{what} must not be negative: {} micro-{}",
            money.micros, money.unit
        )));
    }
    Ok(())
}

fn require_unit(account: &Account, what: &str, money: &Money) -> Result<(), LedgerError> {
    if money.unit != account.unit {
        return Err(LedgerError::Invalid(format!(
            "{what} is in {} but account {} is in {}",
            money.unit, account.id, account.unit
        )));
    }
    Ok(())
}

/// 担保人给 `account` 开授信（`Some`），或把授信关掉（`None`）。担保人就是账户在树里的父账户，不由调用方声明。
///
/// 授信是「有担保的负值」：账户可用额度不够时，最多再透支 `limit`。关掉授信不会改已经发生的透支，
/// 只是之后不再允许新的透支。
pub(super) fn set_credit_line_in(
    conn: &Connection,
    account: &AccountId,
    setting: Option<&CreditLineSetting>,
    now: TimestampMs,
) -> Result<CreditLine, LedgerError> {
    let target = load_existing(conn, account)?;
    let guarantor = target.parent.clone().ok_or_else(|| {
        LedgerError::Invalid(format!(
            "account {account} is the platform root and has no guarantor"
        ))
    })?;
    let (enabled, limit, valid_until) = match setting {
        Some(setting) => {
            require_non_negative("credit limit", &setting.limit)?;
            require_unit(&target, "credit limit", &setting.limit)?;
            if let Some(until) = setting.valid_until
                && until <= now
            {
                return Err(LedgerError::Invalid(
                    "a credit line must be valid beyond now".into(),
                ));
            }
            (true, setting.limit.clone(), setting.valid_until)
        }
        None => (false, Money::zero(target.unit.clone()), None),
    };
    conn.execute(
        "INSERT INTO ledger_credit_lines (account_id, guarantor_id, enabled, limit_micros, valid_until_ms, set_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            account.as_str(),
            guarantor.as_str(),
            i64::from(enabled),
            limit.micros,
            valid_until.map(|until| until.millis()),
            now.millis(),
        ],
    )?;
    let version = conn.last_insert_rowid();
    Ok(CreditLine {
        account: account.clone(),
        guarantor,
        enabled,
        limit,
        valid_until,
        version,
    })
}

/// 账户此刻生效的授信设置（没设置过返回 `None`；已经过期的返回 `enabled = true` 但 `valid_until` 已过，
/// 准入时按 0 处理，见 `project::credit_limit_micros`）。
pub(super) fn credit_line_in(
    conn: &Connection,
    account: &AccountId,
) -> Result<Option<CreditLine>, LedgerError> {
    let target = load_existing(conn, account)?;
    let latest: Option<(i64, String, i64, i64, Option<i64>)> = conn
        .query_row(
            "SELECT version, guarantor_id, enabled, limit_micros, valid_until_ms FROM ledger_credit_lines
              WHERE account_id = ?1 ORDER BY version DESC LIMIT 1",
            [account.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    latest
        .map(|(version, guarantor, enabled, limit, valid_until)| {
            Ok(CreditLine {
                account: account.clone(),
                guarantor: AccountId::new(guarantor)?,
                enabled: enabled == 1,
                limit: Money::new(target.unit.clone(), limit),
                valid_until: valid_until.map(TimestampMs),
                version,
            })
        })
        .transpose()
}

/// 设置账户的速率闸门与在途上限。三项各自 `None` = 不设限；每次设置都是完整的一份（不是只改某一项）。
pub(super) fn set_account_limits_in(
    conn: &Connection,
    account: &AccountId,
    limits: &AccountLimits,
    now: TimestampMs,
) -> Result<(), LedgerError> {
    let target = load_existing(conn, account)?;
    for (what, cap) in [
        ("in-flight cap", &limits.inflight_cap),
        ("per-minute cap", &limits.minute_cap),
        ("per-hour cap", &limits.hour_cap),
    ] {
        if let Some(cap) = cap {
            require_non_negative(what, cap)?;
            require_unit(&target, what, cap)?;
        }
    }
    conn.execute(
        "INSERT INTO ledger_account_limits
            (account_id, inflight_cap_micros, minute_cap_micros, hour_cap_micros, set_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            account.as_str(),
            limits.inflight_cap.as_ref().map(|m| m.micros),
            limits.minute_cap.as_ref().map(|m| m.micros),
            limits.hour_cap.as_ref().map(|m| m.micros),
            now.millis(),
        ],
    )?;
    Ok(())
}

/// 账户当前生效的速率闸门与在途上限。
pub(super) fn account_limits_in(
    conn: &Connection,
    account: &AccountId,
) -> Result<AccountLimits, LedgerError> {
    let target = load_existing(conn, account)?;
    let stored = super::project::limits_micros(conn, account.as_str())?;
    let money = |micros: Option<i64>| micros.map(|m| Money::new(target.unit.clone(), m));
    Ok(AccountLimits {
        inflight_cap: money(stored.inflight),
        minute_cap: money(stored.minute),
        hour_cap: money(stored.hour),
    })
}

/// 设置补贴池某个账期的总上限。池必须是补贴池账户、单位一致。没有设置过的账期上限是 0（失败即关闭）。
pub(super) fn set_pool_cap_in(
    conn: &Connection,
    pool: &AccountId,
    period: BillingMonth,
    cap: &Money,
    now: TimestampMs,
) -> Result<(), LedgerError> {
    let target = load_existing(conn, pool)?;
    if target.kind != AccountKind::SubsidyPool {
        return Err(LedgerError::Invalid(format!(
            "account {pool} is a {:?}, not a subsidy pool",
            target.kind
        )));
    }
    require_non_negative("pool cap", cap)?;
    require_unit(&target, "pool cap", cap)?;
    conn.execute(
        "INSERT INTO ledger_pool_caps (pool_id, period, unit, cap_micros, set_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            pool.as_str(),
            period.to_string(),
            cap.unit.to_string(),
            cap.micros,
            now.millis(),
        ],
    )?;
    Ok(())
}

/// 给账户充值（自有资金）。幂等键相同、账户和金额相同的重放返回 [`FundOutcome::Replayed`]，不记第二次；
/// 同一个键对应不同的账户或金额是 [`LedgerError::GrantConflict`]。
///
/// 补贴池和平台根不能充值（池的钱是账期总上限，平台是账本的源头）。P0 的免费档不用它；预付（P2 接支付之后）会。
pub(super) fn fund_account_in(
    conn: &Connection,
    account: &AccountId,
    amount: &Money,
    key: &IdempotencyKey,
    now: TimestampMs,
) -> Result<FundOutcome, LedgerError> {
    let target = load_existing(conn, account)?;
    if matches!(
        target.kind,
        AccountKind::Platform | AccountKind::SubsidyPool
    ) {
        return Err(LedgerError::Invalid(format!(
            "account {account} is a {:?} and cannot be funded",
            target.kind
        )));
    }
    if amount.micros <= 0 {
        return Err(LedgerError::Invalid(
            "a top-up must be a positive amount".into(),
        ));
    }
    require_unit(&target, "top-up", amount)?;
    let ledger_key = format!("fund|{key}");
    let existing: Option<(String, i64)> = conn
        .query_row(
            "SELECT account_id, funds_micros FROM ledger_entries WHERE idempotency_key = ?1",
            [&ledger_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((existing_account, existing_amount)) = existing {
        return if existing_account == account.as_str() && existing_amount == amount.micros {
            Ok(FundOutcome::Replayed)
        } else {
            Err(LedgerError::GrantConflict(format!(
                "top-up key {key} was used for a different account or amount"
            )))
        };
    }
    insert_entry(
        conn,
        &EntryDraft {
            account: account.as_str(),
            hold: None,
            kind: "fund",
            unit: &target.unit,
            held: 0,
            spent: 0,
            funds: amount.micros,
            liability: 0,
            window_at_ms: now.millis(),
            at_ms: now.millis(),
            key: ledger_key,
        },
    )?;
    Ok(FundOutcome::Funded)
}
