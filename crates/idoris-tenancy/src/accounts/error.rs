//! 账户存储的错误。

use idoris_compute_contracts::account::{AllocationError, DirectoryError, TreeError};
use idoris_compute_contracts::credential::{CredentialError, ResolveError};
use idoris_compute_contracts::grant::GrantError;
use idoris_compute_contracts::ids::{CredentialId, IdError};
use idoris_compute_contracts::money::MoneyError;

use crate::sqlite_conn::ConnectionSetupError;

/// 账户存储的写入或读取失败。
///
/// 契约层（S0）的校验错误原样带出来（`Tree` / `Directory` / `Allocation` / `Credential` / `Grant` / `Money`），
/// 这样 S2 可以直接对它们做原因码映射；数据库层拦下的坏数据是 [`AccountStoreError::Constraint`]，
/// 它带着 SQLite 的原话（约束名或触发器里写的说明）。
#[derive(Debug, thiserror::Error)]
pub enum AccountStoreError {
    /// 等锁超时（`busy_timeout` 到了）。可以重试，和其它存储错误区分开。
    #[error("sqlite busy: timed out waiting for a lock")]
    Busy,
    #[error("sqlite error: {0}")]
    Sql(rusqlite::Error),
    #[error("connection setup failed: {0}")]
    Setup(#[from] ConnectionSetupError),
    /// 数据库层的约束（外键、唯一、CHECK、触发器）拒绝了这次写入。
    /// 正常走 Rust 层校验时不会走到这里，它是「绕过 Rust 层的坏数据」的最后一道门。
    #[error("database constraint rejected the write: {0}")]
    Constraint(String),
    #[error("{kind} {id} not found")]
    NotFound { kind: &'static str, id: String },
    #[error("{kind} {id} already exists")]
    AlreadyExists { kind: &'static str, id: String },
    #[error("illegal status change for {kind} {id}: {from} -> {to}")]
    IllegalTransition {
        kind: &'static str,
        id: String,
        from: String,
        to: String,
    },
    /// 凭证的绑定目标不合法（service / node 必须绑 org 账户；member 必须绑有付款账户的成员关系），
    /// 或管理权限的范围越出了它自己绑定的账户。
    #[error("credential binding is not allowed: {0}")]
    InvalidBinding(String),
    /// 免费额度的幂等键不是 `subsidy:<子账户所属成员的主体>:<YYYY-MM>`，或别的用途用了 `subsidy:` 键。
    #[error("subsidy idempotency key rejected: {0}")]
    SubsidyKey(String),
    /// 库里存的东西读出来不合法（被绕过 Rust 层改坏了，或版本不匹配）。
    #[error("stored row is corrupt: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Id(#[from] IdError),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error(transparent)]
    Allocation(#[from] AllocationError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Grant(#[from] GrantError),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

impl From<rusqlite::Error> for AccountStoreError {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(failure, message) = &error {
            match failure.code {
                rusqlite::ErrorCode::DatabaseBusy => return Self::Busy,
                rusqlite::ErrorCode::ConstraintViolation => {
                    return Self::Constraint(message.clone().unwrap_or_else(|| error.to_string()));
                }
                _ => {}
            }
        }
        Self::Sql(error)
    }
}

impl AccountStoreError {
    /// 是不是数据库层的约束拒绝。
    pub fn is_constraint(&self) -> bool {
        matches!(self, Self::Constraint(_))
    }

    pub(crate) fn already_exists(kind: &'static str, id: impl Into<String>) -> Self {
        Self::AlreadyExists {
            kind,
            id: id.into(),
        }
    }

    pub(crate) fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            id: id.into(),
        }
    }
}

/// 由凭证解析付款上下文失败（[`crate::accounts::AccountStore::payment_context`]）。
#[derive(Debug, thiserror::Error)]
pub enum PaymentContextError {
    /// 库里没有这张凭证。
    #[error("unknown credential {0}")]
    UnknownCredential(CredentialId),
    /// S0 的 `resolve_payment_context` 拒绝了（撤销、冻结、过期、成员越界、歧义……）；
    /// `ResolveError::reason_code` 给出稳定原因码。
    #[error(transparent)]
    Resolve(ResolveError),
    #[error(transparent)]
    Store(AccountStoreError),
}
