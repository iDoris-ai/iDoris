//! **S3 准入账本**（计算服务设计稿 §4、§5、§12 的 S3 一行）：沿付款账户**所有祖先**原子地检查五项并占住预留，
//! 有担保的负值与独立宽限，免费额度按幂等发放，降级原因码。防超发的核心——并发下多扣或少扣都是真金白银的错账，
//! 所以正确性优先于速度。
//!
//! 入口是 [`AccountStore`] 上的一组方法（和 S1 的账户存储同一个对象、同一条连接，S2 手里已经有它），
//! 表结构与约束见 `migrations/0001_ledger.sql` 的注释。
//!
//! # 一次请求的账
//!
//! ```text
//! admit_attempt ──▶ 一个事务：扫过期 · 释放这个请求之前失败的候选 · 核对凭证/账户链 · 读整条链的投影 ·
//!                   五项检查 · 压低 max_tokens · 宽限 · 写预留与沿链的 reserve 分录
//!        │
//!        ▼   （上游调用）
//! 尝试收尾（S4 的 finalize_request，同一个事务里）──▶ settle_attempt：结算 / 释放 / 迟到结算；overage 的归属；冻结凭证
//! ```
//!
//! # 为什么不会超发
//!
//! 1. **检查和写预留在同一个 `BEGIN IMMEDIATE` 事务里。** `BEGIN IMMEDIATE` 一开始就拿 SQLite 的写锁，
//!    跨连接、跨进程都互斥，所以第二个准入读到的一定是第一个已经提交的预留，不可能两个都「看到余额还够」再都写进去。
//!    线程互斥只是同一连接上的小保护，真正的保证来自数据库锁（并发测试每个线程各开一条连接）。
//! 2. **可用额是分录的投影，不是余额字段。** 没有任何可以被两个并发路径各改一半的计数器；
//!    预留、结算、释放、过期都是 `ledger_entries` 里追加的分录（触发器禁止改和删，幂等键唯一），可用额每次从分录求和
//!    （`project.rs`）。
//! 3. **沿所有祖先**：一个预留对付款账户链上的每个账户各记一条分录，所以每个祖先的在途、已花、速率窗口、补贴池消耗
//!    都有自己的一列分录，五项检查在同一个事务里对每个祖先各做一遍。
//! 4. **数据库层兜底**：同一个预留在同一个账户上的在途净额必须落在 `[0, 预留额]`（触发器），所以就算 Rust 层有
//!    bug 也写不进「重复预留」或「多释放」。
//! 5. **失败即关闭**：账户既没充值也没发过额度，可用额是 0，不是「不设限」；补贴池没设账期上限，上限是 0；
//!    凭证不在账户库里、账户链里有冻结的账户，一律拒绝。SQLite 的 `SUM` 溢出是错误，所有金额运算 checked。
//!
//! # 给 S2（控制面）的存储层函数
//!
//! 都是 `AccountStore` 的方法，只校验结构（账户存在、单位一致、非负），**不做授权**（谁有权调用是 S2 的
//! 「操作 × 资源范围」检查，审计也在 S2）：
//!
//! | 函数 | 做什么 |
//! |---|---|
//! | [`set_credit_line`](AccountStore::set_credit_line) / [`disable_credit_line`](AccountStore::disable_credit_line) | 担保人（账户的父账户）给下级开 / 关授信（有担保的负值）。**授信的权威数据源是 `allocations`（purpose = `credit_line`）**，S2 控制面开通的授信和这里开的是同一份记录：设置 = 撤销还生效的旧额度、写一条新的，关掉 = 撤销 |
//! | [`issue_free_tier`](AccountStore::issue_free_tier) | 免费额度发放，幂等键 `subsidy:<主体>:<YYYY-MM>`，受补贴池该账期总上限约束，可以安全地重复调用。S2 控制面发放免费额度走同样的上限检查和承诺登记（同一个事务里），窗口必须恰好是这个月 |
//! | [`set_pool_cap`](AccountStore::set_pool_cap) | 补贴池某个账期的总上限（没设 = 0） |
//! | [`set_account_limits`](AccountStore::set_account_limits) | 速率闸门（分钟 / 小时花费）与在途总额上限 |
//! | [`fund_account`](AccountStore::fund_account) | 充值（自有资金，幂等键）。P0 免费档不用；预付（P2）会用 |
//! | [`set_ledger_config`](AccountStore::set_ledger_config) | 账期时区（泰国首发 `Asia/Bangkok`）与预留 TTL |
//!
//! # 给 S6（儿童安全）的存储层函数
//!
//! 安全检查的成本是附加成本，计入准入和结算。两个阶段，两个入口：
//!
//! | 阶段 | 预留 | 说明 |
//! |---|---|---|
//! | 输入检查（**任何尝试之前**） | [`reserve_request_component`](AccountStore::reserve_request_component) | 没有尝试，付款人取自付款上下文；挂在 `<请求 id>#0~<component>` 下 |
//! | 完整输出检查（尝试的主要预留还开着） | [`reserve_component`](AccountStore::reserve_component) | 挂在作答的那次尝试名下 |
//!
//! 结算 / 释放都是 [`settle_component`](AccountStore::settle_component) /
//! [`release_component`](AccountStore::release_component)（`attempt` 传预留挂靠的那个尝试 id）。用法见 `components.rs`。
//!
//! # 给云执行器（S4）的接线
//!
//! - 准入：[`admit_attempt`](AccountStore::admit_attempt)（router 的 `LedgerAdmission` 包了它）；
//! - 候选没被采用：[`release_attempt`](AccountStore::release_attempt)；
//! - 结算：[`attempt_ledger_hook`](AccountStore::attempt_ledger_hook) 装进 `AttemptStore`，
//!   尝试记录的收尾和账本的结算同一个事务（`hook.rs` 有两张账对应关系的表）；
//! - 对账：[`reconcile_with_attempts`](AccountStore::reconcile_with_attempts)。
//!
//! # 设计稿含糊处的理解（没有悄悄拍板，列在交付说明里等 owner 确认）
//!
//! 1. **宽限那一轮超出可用额度的部分由谁承担**（S0 含糊点 6）：账户自己的可用额度加担保人授信之内的部分，
//!    和普通请求一样；**超出这个下限的部分（至多一次预留）由平台承担**，记 `grace_absorbed`，不冻结凭证，
//!    不转嫁给担保人或用户：这一块**不记到付款账户链上**（用户和担保人的余额只扣 `charged - grace_absorbed`，
//!    后续充值不会先去填它）。备选：让担保人承担（要担保人事先同意，不保守）；让账户变成负数等充值
//!    （P0 没有充值）。分摊守恒：`charged = user_borne + sponsor_covered + platform_loss + grace_absorbed`。
//! 2. 宽限的「周期」是账本时区的**自然月**（和免费额度同一个账期）；作废规则：宽限放行的预留最后没有向用户收费，
//!    这次宽限作废（候选失败切换不会烧掉孩子唯一的宽限）。
//! 3. 补贴池的「总上限」是**每个账期**的上限（按月授权，每月花了多少一眼可见），没设置的账期是 0。
//! 4. 授权额度（`Allocation.ceiling`）是**窗口内的消耗上限**：此刻有效的花费类额度之和，减去从最早有效起点算起的消耗。
//!    同一个账户上多个有效额度是**相加**（保守的反面是少算消耗，这里窗口取最早起点，消耗只会多算不会少算）。
//! 5. 速率闸门是**滑动窗口**（最近 60 秒 / 3600 秒内准入的金额），不是日历桶。
//! 6. overage 的「担保人授信范围内还能吸收多少」取整条链在**钱**这个维度上的余量（可用额度加授信的最小值），
//!    速率、在途、补贴池上限不算进来（那些是平台的节流，不是授信）。
//! 7. 混合拒绝时（钱不够同时速率也超）报 `budget_exhausted`（S0 `blocking_codes` 的顺序），因为它是不会因为等一会儿
//!    就好的那一个。

mod admit;
mod components;
mod error;
mod holds;
mod hook;
mod limits;
mod period;
mod project;
mod read;
mod reconcile;
mod subsidy;
mod types;

#[cfg(feature = "mutation-test-hooks")]
pub mod hooks;

#[cfg(test)]
mod migration_tests;

pub use error::LedgerError;
pub use hook::LedgerAttemptHook;
pub use subsidy::PoolReport;
pub use types::{
    AccountLimits, AdmitInput, AlertKind, AlertRow, ComponentDecision, CreditLine,
    CreditLineSetting, EntryRow, FreeTierIssue, FreeTierOutcome, FundOutcome, GraceUseRow, HoldRow,
    HoldState, LedgerConfig, MODEL_COMPONENT, Mismatch, PLATFORM_LOSS_ACCOUNT, ReconcileReport,
    SettleOutcome, SettleResult,
};

use std::sync::Arc;

use idoris_compute_contracts::admission::{AdmissionDecision, AncestorView};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{
    AccountId, AttemptId, BillingMonth, IdempotencyKey, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::Money;
use idoris_compute_contracts::pricing::Price;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::{AccountStore, AccountStoreError};
use crate::attempts::AttemptLedgerHook;

/// 迁移按顺序执行，版本号 = 下标 + 1。**只能在末尾追加**，已经发布的文件不能改。
/// 0001 是 S3 的账本；0002 把授信的权威数据源统一到 `allocations`（并补登控制面发出的免费额度的承诺）。
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_ledger.sql"),
    include_str!("migrations/0002_credit_lines_are_allocations.sql"),
];

/// 应用账本迁移（在账户迁移之后、同一条连接上）。版本记在 `ledger_schema_migrations`。
pub(super) fn run_migrations(conn: &mut Connection) -> Result<(), AccountStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS ledger_schema_migrations (\
            version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT",
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).unwrap_or(i64::MAX);
        let applied: Option<i64> = tx
            .query_row(
                "SELECT version FROM ledger_schema_migrations WHERE version = ?1",
                [version],
                |row| row.get(0),
            )
            .optional()?;
        if applied.is_some() {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO ledger_schema_migrations (version, applied_at_ms) VALUES (?1, ?2)",
            rusqlite::params![version, migration_stamp()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 只给迁移记录打时间戳，不参与任何业务判断。
fn migration_stamp() -> i64 {
    use crate::budget::{Clock, SystemClock};
    SystemClock.now_ms()
}

/// 事务提交之后把告警写进日志（日志里没有请求内容，也没有密钥）。
fn emit(outcome: &SettleOutcome) {
    for line in &outcome.logs {
        eprintln!("{line}");
    }
}

/// 免费额度在账本时区里这个账期的窗口 `[start_ms, end_ms)`。给 S2 控制面用：控制面发放的免费额度窗口必须恰好是它。
pub(super) fn free_tier_window(
    conn: &Connection,
    month: BillingMonth,
) -> Result<(i64, i64), LedgerError> {
    subsidy::free_tier_window_in(conn, month)
}

/// 给 S2 控制面用（在控制面自己的事务里）：一笔刚写进 `allocations` 的（或幂等重放的）免费额度，检查补贴池该账期的
/// 总上限并登记承诺（`ledger_subsidy_grants`）。已经登记过的什么都不写；超出上限返回 [`LedgerError::PoolCapExceeded`]，
/// 调用方让事务（保存点）回滚，额度行随之撤掉。
pub(super) fn commit_free_tier(
    conn: &Connection,
    allocation: &idoris_compute_contracts::account::Allocation,
    principal: &idoris_compute_contracts::ids::PrincipalId,
    month: BillingMonth,
    now: TimestampMs,
) -> Result<(), LedgerError> {
    subsidy::commit_free_tier_in(conn, allocation, principal, month, now)
}

impl AccountStore {
    /// 一个写事务：`BEGIN IMMEDIATE`，`f` 返回 `Ok` 才提交，返回 `Err` 随事务丢弃而回滚。
    fn ledger_write<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// 一个只读事务：同一个快照里读多张表，读完回滚。
    fn ledger_read<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, LedgerError>,
    ) -> Result<T, LedgerError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = f(&tx)?;
        tx.rollback()?;
        Ok(value)
    }

    // ------------------------------------------------------------------ 准入

    /// **准入**：沿付款账户链原子地检查五项并占住预留，返回 S0 的 [`AdmissionDecision`]
    /// （放行 / 降级 / 宽限 / 拒绝）。放行类的决定意味着预留已经占住（在途分录已提交）；
    /// 拒绝没有占任何额度。
    ///
    /// 同一个尝试只能准入一次（[`LedgerError::HoldExists`]）。尝试序号大于 0（切换候选）时，同一个请求里之前
    /// 失败的候选的预留会在**同一个事务里**先释放（用户对它们付 0），新候选的检查才看得到腾出来的额度。
    ///
    /// 数据库错误（含等锁超时 [`LedgerError::is_busy`]）一律是 `Err`：调用方必须失败即关闭，不能当成放行。
    pub fn admit_attempt(&self, input: &AdmitInput<'_>) -> Result<AdmissionDecision, LedgerError> {
        let mut conn = self.lock();
        #[allow(unused_mut)]
        let mut tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let plan = admit::decide(&tx, input)?;
        // 变异测试钩子（见 `hooks.rs`）：未启用时这个分支什么都不做，特性关闭时编译掉。
        #[cfg(feature = "mutation-test-hooks")]
        if let Some(barrier) = hooks::barrier() {
            tx.commit()?;
            barrier.wait();
            tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        }
        admit::apply(&tx, input, &plan)?;
        tx.commit()?;
        Ok(plan.decision)
    }

    /// 结算一个尝试的预留：`charged` 是最终向付款账户记的金额，0 等于释放。幂等：同一个金额重放什么都不写；
    /// 金额对不上是错误。预留已经过期（TTL 到了）是**迟到结算**，整笔按 overage 的规则走（担保人承担 / 平台损失 /
    /// 冻结凭证）。
    ///
    /// 请求收尾时不用手工调它：装了 [`attempt_ledger_hook`](Self::attempt_ledger_hook) 之后，S4 的 `finalize_request`
    /// 在同一个事务里替你调。
    pub fn settle_attempt(
        &self,
        attempt: &AttemptId,
        charged: &Price,
        now: TimestampMs,
    ) -> Result<SettleOutcome, LedgerError> {
        let outcome = self.ledger_write(|tx| {
            let id = holds::hold_id_of(attempt, MODEL_COMPONENT);
            let hold =
                holds::load_hold(tx, &id)?.ok_or_else(|| LedgerError::HoldNotFound(id.clone()))?;
            if charged.unit() != &hold.unit {
                return Err(LedgerError::Invalid(format!(
                    "the charge is in {} but the reservation is in {}",
                    charged.unit(),
                    hold.unit
                )));
            }
            let config = project::load_config(tx)?;
            holds::settle_hold_in(tx, &id, charged.micros(), now.millis(), &config)
        })?;
        emit(&outcome);
        Ok(outcome)
    }

    /// 释放一个尝试的预留（没有钱要收）。预留不存在返回 `Ok(None)`（没有东西可放），已经释放过也是幂等的
    /// （`AlreadyReleased`）。**候选没被采用**时调用它（比如准入放行了、创建尝试却失败了）。
    pub fn release_attempt(
        &self,
        attempt: &AttemptId,
        now: TimestampMs,
    ) -> Result<Option<SettleOutcome>, LedgerError> {
        let outcome = self.ledger_write(|tx| {
            let id = holds::hold_id_of(attempt, MODEL_COMPONENT);
            if holds::load_hold(tx, &id)?.is_none() {
                return Ok(None);
            }
            let config = project::load_config(tx)?;
            holds::settle_hold_in(tx, &id, 0, now.millis(), &config).map(Some)
        })?;
        if let Some(outcome) = &outcome {
            emit(outcome);
        }
        Ok(outcome)
    }

    /// 扫掉所有已经过期、还没收尾的预留（把它们占的额度放回去）。准入和结算每次都会先扫，所以不调用它
    /// 也不会超发；它是给运维 / 测试在没有流量时也能看到过期生效用的。返回扫掉了几个。
    pub fn sweep_expired_holds(&self, now: TimestampMs) -> Result<usize, LedgerError> {
        self.ledger_write(|tx| holds::sweep_expired_in(tx, now.millis()))
    }

    // ------------------------------------------------------------------ 附加成本（S6）

    /// 为一个尝试预留附加成本（比如安全检查）。见 `components.rs`。
    pub fn reserve_component(
        &self,
        attempt: &AttemptId,
        component: &str,
        amount: &Price,
        now: TimestampMs,
    ) -> Result<ComponentDecision, LedgerError> {
        self.ledger_write(|tx| {
            components::reserve_component_in(tx, attempt, component, amount, now)
        })
    }

    /// 在**任何尝试出现之前**预留附加成本（比如输入检查：先检查，通过了才去调用模型）。付款人取自付款上下文，
    /// 预留挂在这个请求的第 0 号尝试名下（`<请求 id>#0~<component>`），结算 / 释放用
    /// [`settle_component`](Self::settle_component) / [`release_component`](Self::release_component)，`attempt` 参数传
    /// `AttemptId::new(request, AttemptIndex(0))`。见 `components.rs`。
    pub fn reserve_request_component(
        &self,
        request: &RequestId,
        component: &str,
        payment: &PaymentContext,
        amount: &Price,
        now: TimestampMs,
    ) -> Result<ComponentDecision, LedgerError> {
        self.ledger_write(|tx| {
            components::reserve_request_component_in(tx, request, component, payment, amount, now)
        })
    }

    /// 按实际成本结算附加成本（0 = 释放）。幂等。
    pub fn settle_component(
        &self,
        attempt: &AttemptId,
        component: &str,
        charged: &Price,
        now: TimestampMs,
    ) -> Result<SettleOutcome, LedgerError> {
        let outcome = self.ledger_write(|tx| {
            components::settle_component_in(tx, attempt, component, charged, now)
        })?;
        emit(&outcome);
        Ok(outcome)
    }

    /// 释放附加成本的预留。
    pub fn release_component(
        &self,
        attempt: &AttemptId,
        component: &str,
        now: TimestampMs,
    ) -> Result<SettleOutcome, LedgerError> {
        let outcome = self.ledger_write(|tx| {
            components::validate_component(component)?;
            let id = holds::hold_id_of(attempt, component);
            let config = project::load_config(tx)?;
            holds::settle_hold_in(tx, &id, 0, now.millis(), &config)
        })?;
        emit(&outcome);
        Ok(outcome)
    }

    // ------------------------------------------------------------------ 免费额度、授信、限额、充值（给 S2）

    /// 发放一份免费额度。语义、幂等、补贴池约束见 `subsidy.rs`。
    pub fn issue_free_tier(
        &self,
        input: &FreeTierIssue<'_>,
    ) -> Result<FreeTierOutcome, LedgerError> {
        self.ledger_write(|tx| subsidy::issue_free_tier_in(tx, input))
    }

    /// 设置补贴池某个账期的总上限。
    pub fn set_pool_cap(
        &self,
        pool: &AccountId,
        month: BillingMonth,
        cap: &Money,
        now: TimestampMs,
    ) -> Result<(), LedgerError> {
        self.ledger_write(|tx| limits::set_pool_cap_in(tx, pool, month, cap, now))
    }

    /// 补贴池某个账期的报表：总上限、发出去的免费额度、已经花掉（含在途）的。
    pub fn pool_report(
        &self,
        pool: &AccountId,
        month: BillingMonth,
    ) -> Result<PoolReport, LedgerError> {
        self.ledger_read(|tx| subsidy::pool_report_in(tx, pool, month))
    }

    /// 担保人（账户的父账户）给 `account` 开授信（有担保的负值）：账户可用额度不够时最多再透支 `limit`。
    pub fn set_credit_line(
        &self,
        account: &AccountId,
        setting: &CreditLineSetting,
        now: TimestampMs,
    ) -> Result<CreditLine, LedgerError> {
        self.ledger_write(|tx| limits::set_credit_line_in(tx, account, Some(setting), now))
    }

    /// 关掉授信。不改已经发生的透支，只是之后不再允许新的。
    pub fn disable_credit_line(
        &self,
        account: &AccountId,
        now: TimestampMs,
    ) -> Result<CreditLine, LedgerError> {
        self.ledger_write(|tx| limits::set_credit_line_in(tx, account, None, now))
    }

    /// 账户当前生效的授信设置（没设置过是 `None`）。
    pub fn credit_line(&self, account: &AccountId) -> Result<Option<CreditLine>, LedgerError> {
        self.ledger_read(|tx| limits::credit_line_in(tx, account))
    }

    /// 设置账户的速率闸门（分钟、小时花费上限）与在途总额上限；每次是完整的一份，`None` = 不设限。
    pub fn set_account_limits(
        &self,
        account: &AccountId,
        limits: &AccountLimits,
        now: TimestampMs,
    ) -> Result<(), LedgerError> {
        self.ledger_write(|tx| limits::set_account_limits_in(tx, account, limits, now))
    }

    /// 账户当前生效的限额。
    pub fn account_limits(&self, account: &AccountId) -> Result<AccountLimits, LedgerError> {
        self.ledger_read(|tx| limits::account_limits_in(tx, account))
    }

    /// 充值（自有资金）。幂等键相同的重放返回 [`FundOutcome::Replayed`]。
    pub fn fund_account(
        &self,
        account: &AccountId,
        amount: &Money,
        key: &IdempotencyKey,
        now: TimestampMs,
    ) -> Result<FundOutcome, LedgerError> {
        self.ledger_write(|tx| limits::fund_account_in(tx, account, amount, key, now))
    }

    /// 账本配置（账期时区、预留 TTL）。
    pub fn ledger_config(&self) -> Result<LedgerConfig, LedgerError> {
        self.ledger_read(|tx| project::load_config(tx))
    }

    /// 修改账本配置。时区必须是合法的 IANA 名字，TTL 必须是正数，并且要长于一次请求可能持续的最长时间
    /// （含流式）：预留在上游还在计费的时候过期，额度就提前放回去了。已经存在的预留不受影响（它们的过期时间
    /// 在创建时就定了）。
    pub fn set_ledger_config(&self, config: &LedgerConfig) -> Result<(), LedgerError> {
        period::validate_timezone(&config.billing_timezone)?;
        if config.reservation_ttl_ms <= 0 {
            return Err(LedgerError::Invalid(
                "the reservation TTL must be positive".into(),
            ));
        }
        self.ledger_write(|tx| {
            tx.execute(
                "UPDATE ledger_config SET value = ?1 WHERE key = 'billing_timezone'",
                [&config.billing_timezone],
            )?;
            tx.execute(
                "UPDATE ledger_config SET value = ?1 WHERE key = 'reservation_ttl_ms'",
                [config.reservation_ttl_ms.to_string()],
            )?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ 读（投影与记录）

    /// 一个账户此刻的投影（S0 的 [`AncestorView`]）：余额、授权额度剩余、授信、速率窗口、在途、补贴池剩余。
    pub fn account_view(
        &self,
        account: &AccountId,
        now: TimestampMs,
    ) -> Result<AncestorView, LedgerError> {
        self.ledger_read(|tx| {
            let found = crate::accounts::rows::load_account(tx, account)?
                .ok_or_else(|| AccountStoreError::not_found("account", account.as_str()))?;
            let config = project::load_config(tx)?;
            project::project_chain(tx, std::slice::from_ref(&found), now.millis(), &config)?
                .into_iter()
                .next()
                .ok_or_else(|| LedgerError::Corrupt("projection returned no view".into()))
        })
    }

    /// 付款账户链 `[自己, 父, ..., 根]` 的投影。
    pub fn chain_views(
        &self,
        payer: &AccountId,
        now: TimestampMs,
    ) -> Result<Vec<AncestorView>, LedgerError> {
        self.ledger_read(|tx| {
            let chain = crate::accounts::rows::load_ancestors(tx, payer)?;
            if chain.is_empty() {
                return Err(AccountStoreError::not_found("account", payer.as_str()).into());
            }
            let config = project::load_config(tx)?;
            project::project_chain(tx, &chain, now.millis(), &config)
        })
    }

    /// 一个预留的当前状态（主要预留用 [`MODEL_COMPONENT`]）。
    pub fn hold(
        &self,
        attempt: &AttemptId,
        component: &str,
    ) -> Result<Option<HoldRow>, LedgerError> {
        self.ledger_read(|tx| {
            holds::load_hold(tx, &holds::hold_id_of(attempt, component))?
                .map(|record| record.to_row())
                .transpose()
        })
    }

    /// 一个预留的全部分录，按写入顺序。
    pub fn entries_of_hold(
        &self,
        attempt: &AttemptId,
        component: &str,
    ) -> Result<Vec<EntryRow>, LedgerError> {
        self.ledger_read(|tx| read::entries_of_hold(tx, &holds::hold_id_of(attempt, component)))
    }

    /// 一个账户（或 [`PLATFORM_LOSS_ACCOUNT`]）的全部分录，按写入顺序。
    pub fn entries_of_account(&self, account: &str) -> Result<Vec<EntryRow>, LedgerError> {
        self.ledger_read(|tx| read::entries_of_account(tx, account))
    }

    /// 告警记录（可以查询的那一份；日志是另一份）。`kind` 给了就只要这一种。
    pub fn alerts(&self, kind: Option<AlertKind>) -> Result<Vec<AlertRow>, LedgerError> {
        self.ledger_read(|tx| read::alerts_in(tx, kind.map(AlertKind::as_str)))
    }

    /// 一个账户的宽限记录（含已作废的）。
    pub fn grace_uses(&self, account: &AccountId) -> Result<Vec<GraceUseRow>, LedgerError> {
        self.ledger_read(|tx| read::grace_uses_in(tx, account))
    }

    /// 平台损失账户上某一种分录（`platform_loss` / `grace_absorbed`）的累计。
    pub fn loss_total(&self, kind: &str) -> Result<i64, LedgerError> {
        self.ledger_read(|tx| read::liability_total(tx, PLATFORM_LOSS_ACCOUNT, kind))
    }

    // ------------------------------------------------------------------ 与 S4 对接、对账

    /// 装进 `AttemptStore::install_ledger_hook`：尝试记录的收尾和账本的结算同一个事务。
    pub fn attempt_ledger_hook(&self) -> Arc<dyn AttemptLedgerHook> {
        Arc::new(LedgerAttemptHook)
    }

    /// 对账：账本自己的守恒检查，以及（尝试记录表存在时）和 S4 尝试记录的逐项对照。只读，不修复。
    /// 返回的 `mismatches` 为空 = 一致。恢复后（备份恢复、WAL 回放）先跑它。
    pub fn reconcile_with_attempts(&self) -> Result<ReconcileReport, LedgerError> {
        self.ledger_read(|tx| reconcile::reconcile_in(tx))
    }
}
