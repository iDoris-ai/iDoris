//! 准入账本的错误。
//!
//! 和 [`AccountStoreError`] 分开定义：S2 也会往账户存储的错误里加变体，两边各管各的，不会在同一个枚举上撞车。
//! 数据库层的错误（等锁超时、约束拒绝……）原样带出来（[`LedgerError::Store`]），领域错误（预留不存在、
//! 同一笔结算金额对不上、补贴池总上限）各有自己的变体。

use idoris_compute_contracts::admission::AdmissionError;
use idoris_compute_contracts::ids::{AccountId, BillingMonth, IdError};
use idoris_compute_contracts::money::MoneyError;
use idoris_compute_contracts::reserve::ReserveError;

use crate::accounts::AccountStoreError;

/// 账本读写失败。
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// 数据库层的错误（含等锁超时 [`AccountStoreError::Busy`]、约束拒绝 [`AccountStoreError::Constraint`]）。
    #[error(transparent)]
    Store(#[from] AccountStoreError),
    #[error(transparent)]
    Money(#[from] MoneyError),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    Reserve(#[from] ReserveError),
    #[error(transparent)]
    Id(#[from] IdError),
    /// 没有这个预留。
    #[error("hold {0} not found")]
    HoldNotFound(String),
    /// 这个尝试（或附加成本）已经有预留了：同一个尝试只能准入一次。
    #[error("hold {0} already exists")]
    HoldExists(String),
    /// 同一个预留已经按另一个金额结算过。重放同一次结算必须带同样的金额；不会悄悄覆盖。
    #[error(
        "hold {hold} was already settled for {existing} micro-units; a different charge of {requested} arrived"
    )]
    SettleConflict {
        hold: String,
        existing: i64,
        requested: i64,
    },
    /// 预留已经释放（宣告「没有钱要收」），之后又来了一笔要收的钱。
    #[error("hold {hold} was already released; a charge of {requested} micro-units cannot follow")]
    ChargeAfterRelease { hold: String, requested: i64 },
    /// 调用方给的输入不合法。
    #[error("invalid ledger input: {0}")]
    Invalid(String),
    /// 金额运算溢出（所有溢出都是错误，不回绕、不饱和）。
    #[error("ledger amount overflow")]
    Overflow,
    /// 补贴池该账期发放的免费额度之和会超过总上限。
    #[error(
        "subsidy pool {pool} cap for {period} would be exceeded: requested {requested}, remaining {remaining} micro-units"
    )]
    PoolCapExceeded {
        pool: AccountId,
        period: BillingMonth,
        requested: i64,
        remaining: i64,
    },
    /// 成员账户的祖先里没有补贴池，没有谁来出这笔免费额度。
    #[error("no subsidy pool above account {0}")]
    NoSubsidyPool(AccountId),
    /// 同一个幂等键已经发放过内容不同的免费额度。
    #[error("a different free-tier grant already exists for this idempotency key: {0}")]
    GrantConflict(String),
    /// 账户、主体或成员关系不在可用状态（冻结、关闭、已撤销）。
    #[error("{what} is not active: {detail}")]
    NotActive { what: &'static str, detail: String },
    /// 库里存的东西读出来不合法。
    #[error("stored ledger row is corrupt: {0}")]
    Corrupt(String),
}

impl LedgerError {
    /// 是不是等锁超时（可以重试，和其它错误区分开）。
    pub fn is_busy(&self) -> bool {
        matches!(self, LedgerError::Store(AccountStoreError::Busy))
    }

    /// 是不是数据库层的约束拒绝（正常走 Rust 层校验时不会遇到，遇到说明有人绕过 Rust 层写数据）。
    pub fn is_constraint(&self) -> bool {
        matches!(self, LedgerError::Store(AccountStoreError::Constraint(_)))
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(error: rusqlite::Error) -> Self {
        LedgerError::Store(AccountStoreError::from(error))
    }
}

/// 两个 `i64` 相加，溢出是错误。
pub(super) fn add(left: i64, right: i64) -> Result<i64, LedgerError> {
    left.checked_add(right).ok_or(LedgerError::Overflow)
}

/// 两个 `i64` 相减，溢出是错误。
pub(super) fn sub(left: i64, right: i64) -> Result<i64, LedgerError> {
    left.checked_sub(right).ok_or(LedgerError::Overflow)
}
