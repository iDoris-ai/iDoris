//! 免费额度发放（设计稿 §4）：补贴池按月给成员**授权**额度，发放带幂等键 `subsidy:<主体>:<YYYY-MM>`，
//! 发放受补贴池该账期的总上限约束。
//!
//! # 怎么调用
//!
//! ```text
//! // 1. 运维先给补贴池设这个月的总上限（没设就是 0，什么都发不出去）：
//! store.set_pool_cap(&pool, month, &cap, now)?;
//! // 2. 对每个成员，每月调用一次（或者调用任意多次，幂等）：
//! match store.issue_free_tier(&FreeTierIssue { principal, member_account, month, amount, now })? {
//!     FreeTierOutcome::Granted(allocation) => ...,   // 这次新发的
//!     FreeTierOutcome::Replayed(allocation) => ...,  // 这个月早已发过，同样内容，什么都没写
//! }
//! ```
//!
//! 没有定时任务：谁在每月 1 日（账本时区）调用、对哪些成员调用，是运维 / S2 的事；这个函数只保证
//! **可以安全地重复调用**——并发重试、进程崩溃后重来、两个组织里各有同一个主体的成员关系，同一个
//! `(主体, 月份)` 只会发出一份额度，也只占一份补贴池的额度。
//!
//! # 为什么不走 `AccountStore::create_allocation`
//!
//! S1 的 `create_allocation` 用 S0 的 `validate_allocation` 把「下发不超过授权者本人」落到「授权者有一笔
//! 流入它的、有效期包住这笔的额度」。免费额度不是某个组织把自己的额度转给成员：钱是平台补贴池出的，
//! **权威是补贴池该账期的总上限**。更重要的是，发放要和池的总上限检查在**同一个事务**里（检查和写入之间
//! 有缝，两个并发的发放就能一起通过检查、把池发超），而 `create_allocation` 自己开事务，没法嵌进来。
//! 所以这里在同一个写事务里：核对成员关系、查池的剩余、写额度行、写池的承诺行。额度行的结构规则
//! （父账户存在、子账户是父账户的直接子账户、单位一致、幂等键唯一）仍由 S1 迁移里的触发器兜底。
//!
//! 池的承诺和花钱是两层：**发放时**一个池一个账期发出去的免费额度之和不能超过总上限（不许承诺发不起的钱）；
//! **花钱时**准入的第五项检查再按池账户上的真实消耗卡一次（总量永远不会超，不管额度是怎么发出去的）。

use idoris_compute_contracts::account::{
    AccountKind, AccountStatus, Allocation, subsidy_idempotency_key,
};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{AllocationId, TimestampMs};
use idoris_compute_contracts::money::Money;
use rusqlite::{Connection, OptionalExtension, params};

use super::error::{LedgerError, sub};
use super::period::period_bounds;
use super::project::{load_config, pool_cap_micros};
use super::types::{FreeTierIssue, FreeTierOutcome};
use crate::accounts::AccountStoreError;
use crate::accounts::rows::{load_account, load_allocation_by_key, load_ancestors};

/// 在调用方已经开着的写事务里发放一份免费额度。语义见模块文档。
pub(super) fn issue_free_tier_in(
    conn: &Connection,
    input: &FreeTierIssue<'_>,
) -> Result<FreeTierOutcome, LedgerError> {
    let config = load_config(conn)?;
    let now_ms = input.now.millis();
    let key = subsidy_idempotency_key(input.principal, input.month)?;
    if input.amount.micros <= 0 {
        return Err(LedgerError::Invalid(
            "a free-tier grant must be a positive amount".into(),
        ));
    }
    let member = load_account(conn, input.member_account)?
        .ok_or_else(|| AccountStoreError::not_found("account", input.member_account.as_str()))?;
    if member.kind != AccountKind::Member {
        return Err(LedgerError::Invalid(format!(
            "account {} is a {:?}; free-tier allowances go to member accounts",
            member.id, member.kind
        )));
    }
    if input.amount.unit != member.unit {
        return Err(LedgerError::Invalid(format!(
            "grant is in {} but account {} is in {}",
            input.amount.unit, member.id, member.unit
        )));
    }
    let parent = member.parent.clone().ok_or_else(|| {
        LedgerError::Invalid(format!("account {} has no parent account", member.id))
    })?;
    let (start_ms, end_ms) = period_bounds(input.month, &config.billing_timezone)?;

    // 1. 幂等：这个 (主体, 月份) 已经发过了吗
    if let Some(existing) = load_allocation_by_key(conn, &key)? {
        let ceiling = &existing.ceiling;
        let same = existing.parent == parent
            && existing.child == member.id
            && ceiling.amount() == &input.amount
            && ceiling.purpose() == Purpose::FreeTier
            && ceiling.valid_from().millis() == start_ms
            && ceiling.valid_until().millis() == end_ms;
        return if same {
            Ok(FreeTierOutcome::Replayed(existing))
        } else {
            Err(LedgerError::GrantConflict(format!(
                "{key} was already granted with a different amount, member or window"
            )))
        };
    }

    // 2. 过去的月份没有意义：额度一发出来就已经过期
    if end_ms <= now_ms {
        return Err(LedgerError::Invalid(format!(
            "month {} has already ended; a free-tier allowance for it would be expired on arrival",
            input.month
        )));
    }

    // 3. 成员关系与主体：这个账户确实属于这个主体，两者都还有效
    let membership: Option<(String, String)> = conn
        .query_row(
            "SELECT principal_id, status FROM memberships WHERE account_id = ?1",
            [member.id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((principal, membership_status)) = membership else {
        return Err(LedgerError::Invalid(format!(
            "account {} is not bound to any membership",
            member.id
        )));
    };
    if principal != input.principal.as_str() {
        return Err(LedgerError::Invalid(format!(
            "account {} does not belong to principal {}",
            member.id, input.principal
        )));
    }
    if membership_status != "active" {
        return Err(LedgerError::NotActive {
            what: "membership",
            detail: format!(
                "the membership of account {} is {membership_status}",
                member.id
            ),
        });
    }
    let principal_status: Option<String> = conn
        .query_row(
            "SELECT status FROM principals WHERE principal_id = ?1",
            [input.principal.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if principal_status.as_deref() != Some("active") {
        return Err(LedgerError::NotActive {
            what: "principal",
            detail: format!("principal {} is not active", input.principal),
        });
    }

    // 4. 账户链：必须全部有效，并且上面有一个补贴池
    let chain = load_ancestors(conn, &member.id)?;
    if let Some(inactive) = chain
        .iter()
        .find(|account| account.status != AccountStatus::Active)
    {
        return Err(LedgerError::NotActive {
            what: "account",
            detail: format!("{} is {:?}", inactive.id, inactive.status),
        });
    }
    let pool = chain
        .iter()
        .find(|account| account.kind == AccountKind::SubsidyPool)
        .ok_or_else(|| LedgerError::NoSubsidyPool(member.id.clone()))?;

    // 5. 补贴池该账期的总上限：发出去的免费额度之和不能超过它
    let period = input.month.to_string();
    let cap = pool_cap_micros(conn, pool.id.as_str(), &period)?;
    let granted: i64 = conn.query_row(
        "SELECT COALESCE(SUM(amount_micros), 0) FROM ledger_subsidy_grants WHERE pool_id = ?1 AND period = ?2",
        params![pool.id.as_str(), period],
        |row| row.get(0),
    )?;
    let remaining = sub(cap, granted)?;
    if input.amount.micros > remaining {
        return Err(LedgerError::PoolCapExceeded {
            pool: pool.id.clone(),
            period: input.month,
            requested: input.amount.micros,
            remaining: remaining.max(0),
        });
    }

    // 6. 写额度行和池的承诺行（同一个事务）
    let allocation = Allocation {
        id: AllocationId::new(format!("fta-{}", uuid::Uuid::new_v4().simple()))?,
        parent: parent.clone(),
        child: member.id.clone(),
        ceiling: Grant::new(
            input.amount.clone(),
            Purpose::FreeTier,
            TimestampMs(start_ms),
            TimestampMs(end_ms),
        )
        .map_err(AccountStoreError::from)?,
        idempotency_key: Some(key.clone()),
    };
    conn.execute(
        "INSERT INTO allocations
            (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose,
             valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 'free_tier', ?6, ?7, 0, ?8, ?9)",
        params![
            allocation.id.as_str(),
            parent.as_str(),
            member.id.as_str(),
            input.amount.unit.to_string(),
            input.amount.micros,
            start_ms,
            end_ms,
            key.as_str(),
            now_ms,
        ],
    )?;
    conn.execute(
        "INSERT INTO ledger_subsidy_grants
            (idempotency_key, pool_id, principal_id, member_account, period, unit, amount_micros,
             allocation_id, granted_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            key.as_str(),
            pool.id.as_str(),
            input.principal.as_str(),
            member.id.as_str(),
            period,
            input.amount.unit.to_string(),
            input.amount.micros,
            allocation.id.as_str(),
            now_ms,
        ],
    )?;
    Ok(FreeTierOutcome::Granted(allocation))
}

/// 补贴池某个账期已经发出去多少免费额度、总上限多少、花了多少（报表用：免费档每月花了多少一眼可见）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolReport {
    pub cap: Money,
    /// 发出去的免费额度之和（承诺）。
    pub granted: Money,
    /// 该账期池账户上的消耗：已结算 + 在途（沿池账户的分录，所有成员的花费都记在池上）。
    pub consumed: Money,
}

pub(super) fn pool_report_in(
    conn: &Connection,
    pool: &idoris_compute_contracts::ids::AccountId,
    month: idoris_compute_contracts::ids::BillingMonth,
) -> Result<PoolReport, LedgerError> {
    let config = load_config(conn)?;
    let account = load_account(conn, pool)?
        .ok_or_else(|| AccountStoreError::not_found("account", pool.as_str()))?;
    if account.kind != AccountKind::SubsidyPool {
        return Err(LedgerError::Invalid(format!(
            "account {pool} is not a subsidy pool"
        )));
    }
    let (start_ms, end_ms) = period_bounds(month, &config.billing_timezone)?;
    let period = month.to_string();
    let cap = pool_cap_micros(conn, pool.as_str(), &period)?;
    let granted: i64 = conn.query_row(
        "SELECT COALESCE(SUM(amount_micros), 0) FROM ledger_subsidy_grants WHERE pool_id = ?1 AND period = ?2",
        params![pool.as_str(), period],
        |row| row.get(0),
    )?;
    let consumed: i64 = conn.query_row(
        "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries
          WHERE account_id = ?1 AND window_at_ms >= ?2 AND window_at_ms < ?3",
        params![pool.as_str(), start_ms, end_ms],
        |row| row.get(0),
    )?;
    Ok(PoolReport {
        cap: Money::new(account.unit.clone(), cap),
        granted: Money::new(account.unit.clone(), granted),
        consumed: Money::new(account.unit, consumed),
    })
}
