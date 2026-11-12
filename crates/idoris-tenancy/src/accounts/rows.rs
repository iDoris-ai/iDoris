//! 数据库行与 S0 契约类型之间的转换，以及按 id 取行的小函数。
//!
//! 约定：
//! - 枚举（账户种类、状态、角色……）在库里存的文本**就是 S0 的 serde 线上字符串**，这里通过 serde 转换，
//!   不另写一份映射表，所以 S0 的字符串是唯一真源（`accounts/tests.rs` 还会把迁移里的 CHECK 集合和它逐个对照）。
//! - 读出来的每一行都重新走 S0 的构造函数校验（id 字符集、`Grant::new`、`Credential::new`……），
//!   被绕过 Rust 层写进来的坏数据读出来是 [`AccountStoreError::Corrupt`] 或对应的契约错误，不会静默放行。

use std::collections::BTreeMap;
use std::str::FromStr;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, Allocation, Membership, MembershipRole, MembershipStatus,
    Principal, PrincipalStatus,
};
use idoris_compute_contracts::credential::{
    AdminScope, Credential, CredentialBinding, CredentialStatus,
};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, CredentialId, IdempotencyKey, MembershipId, PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use rusqlite::{Connection, OptionalExtension, Row};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::error::AccountStoreError;

/// 沿父账户向上走的最大步数，只是防止库被改坏出现环时死循环；正常的树最深 `MAX_ACCOUNT_DEPTH`。
const MAX_CHAIN_WALK: usize = 64;

/// 枚举 -> 库里的文本（S0 的 serde 线上字符串）。
pub(super) fn to_text<T: Serialize>(value: &T) -> Result<String, AccountStoreError> {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => Ok(text),
        Ok(other) => Err(AccountStoreError::Corrupt(format!(
            "expected a string-valued enum, got {other}"
        ))),
        Err(error) => Err(AccountStoreError::Corrupt(error.to_string())),
    }
}

/// 库里的文本 -> 枚举。认不得的值是 [`AccountStoreError::Corrupt`]。
pub(super) fn from_text<T: DeserializeOwned>(
    what: &'static str,
    text: &str,
) -> Result<T, AccountStoreError> {
    serde_json::from_value(Value::String(text.to_owned()))
        .map_err(|_| AccountStoreError::Corrupt(format!("unknown {what} value {text:?}")))
}

fn unit_from_text(text: &str) -> Result<Unit, AccountStoreError> {
    Unit::from_str(text)
        .map_err(|error| AccountStoreError::Corrupt(format!("unit {text:?}: {error}")))
}

// ---------- accounts ----------

pub(super) const ACCOUNT_COLUMNS: &str = "account_id, kind, parent_id, unit, status";

pub(super) struct RawAccount {
    id: String,
    kind: String,
    parent: Option<String>,
    unit: String,
    status: String,
}

impl RawAccount {
    pub(super) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            kind: row.get(1)?,
            parent: row.get(2)?,
            unit: row.get(3)?,
            status: row.get(4)?,
        })
    }

    pub(super) fn into_account(self) -> Result<Account, AccountStoreError> {
        Ok(Account {
            id: AccountId::new(self.id)?,
            kind: from_text::<AccountKind>("account kind", &self.kind)?,
            parent: self.parent.map(AccountId::new).transpose()?,
            unit: unit_from_text(&self.unit)?,
            status: from_text::<AccountStatus>("account status", &self.status)?,
        })
    }
}

pub(super) fn load_account(
    conn: &Connection,
    id: &AccountId,
) -> Result<Option<Account>, AccountStoreError> {
    conn.query_row(
        &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE account_id = ?1"),
        [id.as_str()],
        RawAccount::read,
    )
    .optional()?
    .map(RawAccount::into_account)
    .transpose()
}

pub(super) fn account_exists(conn: &Connection, id: &AccountId) -> Result<bool, AccountStoreError> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM accounts WHERE account_id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// `[自己, 父, ..., 根]`（与 S0 `AccountTree::ancestors` 同序）。账户不存在返回空。
pub(super) fn load_ancestors(
    conn: &Connection,
    id: &AccountId,
) -> Result<Vec<Account>, AccountStoreError> {
    let mut stmt = conn.prepare(&format!(
        "WITH RECURSIVE chain (account_id, kind, parent_id, unit, status, depth) AS (
             SELECT {ACCOUNT_COLUMNS}, 0 FROM accounts WHERE account_id = ?1
             UNION ALL
             SELECT a.account_id, a.kind, a.parent_id, a.unit, a.status, chain.depth + 1
               FROM accounts a JOIN chain ON a.account_id = chain.parent_id
              WHERE chain.depth < {MAX_CHAIN_WALK}
         )
         SELECT {ACCOUNT_COLUMNS} FROM chain ORDER BY depth"
    ))?;
    let raws = stmt
        .query_map([id.as_str()], RawAccount::read)?
        .collect::<Result<Vec<_>, _>>()?;
    if raws.len() > MAX_CHAIN_WALK {
        return Err(AccountStoreError::Corrupt(format!(
            "account {id} has an ancestor chain longer than {MAX_CHAIN_WALK}"
        )));
    }
    raws.into_iter().map(RawAccount::into_account).collect()
}

/// 把若干条祖先链合并成不重复的账户集合（它们都止于同一个平台根，所以合起来仍是一棵合法的树）。
pub(super) fn merge_chains(into: &mut BTreeMap<AccountId, Account>, chain: Vec<Account>) {
    for account in chain {
        into.entry(account.id.clone()).or_insert(account);
    }
}

pub(super) fn load_platform_roots(conn: &Connection) -> Result<Vec<Account>, AccountStoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE kind = 'platform' ORDER BY account_id"
    ))?;
    stmt.query_map([], RawAccount::read)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(RawAccount::into_account)
        .collect()
}

pub(super) fn load_all_accounts(conn: &Connection) -> Result<Vec<Account>, AccountStoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY account_id"
    ))?;
    stmt.query_map([], RawAccount::read)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(RawAccount::into_account)
        .collect()
}

// ---------- principals ----------

pub(super) fn load_principal(
    conn: &Connection,
    id: &PrincipalId,
) -> Result<Option<Principal>, AccountStoreError> {
    let raw: Option<(String, String)> = conn
        .query_row(
            "SELECT principal_id, status FROM principals WHERE principal_id = ?1",
            [id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    raw.map(|(id, status)| principal_from_raw(id, &status))
        .transpose()
}

fn principal_from_raw(id: String, status: &str) -> Result<Principal, AccountStoreError> {
    Ok(Principal {
        id: PrincipalId::new(id)?,
        status: from_text::<PrincipalStatus>("principal status", status)?,
    })
}

pub(super) fn load_all_principals(conn: &Connection) -> Result<Vec<Principal>, AccountStoreError> {
    let mut stmt =
        conn.prepare("SELECT principal_id, status FROM principals ORDER BY principal_id")?;
    stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?
    .collect::<Result<Vec<_>, _>>()?
    .into_iter()
    .map(|(id, status)| principal_from_raw(id, &status))
    .collect()
}

// ---------- memberships ----------

pub(super) const MEMBERSHIP_COLUMNS: &str =
    "membership_id, principal_id, org_id, role, account_id, status";

pub(super) struct RawMembership {
    id: String,
    principal: String,
    org: String,
    role: String,
    account: Option<String>,
    status: String,
}

impl RawMembership {
    pub(super) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            principal: row.get(1)?,
            org: row.get(2)?,
            role: row.get(3)?,
            account: row.get(4)?,
            status: row.get(5)?,
        })
    }

    pub(super) fn into_membership(self) -> Result<Membership, AccountStoreError> {
        Ok(Membership {
            id: MembershipId::new(self.id)?,
            principal: PrincipalId::new(self.principal)?,
            org: AccountId::new(self.org)?,
            role: from_text::<MembershipRole>("membership role", &self.role)?,
            account: self.account.map(AccountId::new).transpose()?,
            status: from_text::<MembershipStatus>("membership status", &self.status)?,
        })
    }
}

pub(super) fn load_membership(
    conn: &Connection,
    id: &MembershipId,
) -> Result<Option<Membership>, AccountStoreError> {
    conn.query_row(
        &format!("SELECT {MEMBERSHIP_COLUMNS} FROM memberships WHERE membership_id = ?1"),
        [id.as_str()],
        RawMembership::read,
    )
    .optional()?
    .map(RawMembership::into_membership)
    .transpose()
}

pub(super) fn load_memberships_of(
    conn: &Connection,
    principal: &PrincipalId,
) -> Result<Vec<Membership>, AccountStoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MEMBERSHIP_COLUMNS} FROM memberships WHERE principal_id = ?1 ORDER BY membership_id"
    ))?;
    stmt.query_map([principal.as_str()], RawMembership::read)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(RawMembership::into_membership)
        .collect()
}

pub(super) fn load_all_memberships(
    conn: &Connection,
) -> Result<Vec<Membership>, AccountStoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MEMBERSHIP_COLUMNS} FROM memberships ORDER BY membership_id"
    ))?;
    stmt.query_map([], RawMembership::read)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(RawMembership::into_membership)
        .collect()
}

// ---------- allocations ----------

pub(super) const ALLOCATION_COLUMNS: &str = "allocation_id, parent_id, child_id, ceiling_unit, \
     ceiling_micros, purpose, valid_from_ms, valid_until_ms, idempotency_key";

pub(super) struct RawAllocation {
    id: String,
    parent: String,
    child: String,
    unit: String,
    micros: i64,
    purpose: String,
    valid_from: i64,
    valid_until: i64,
    key: Option<String>,
}

impl RawAllocation {
    pub(super) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            parent: row.get(1)?,
            child: row.get(2)?,
            unit: row.get(3)?,
            micros: row.get(4)?,
            purpose: row.get(5)?,
            valid_from: row.get(6)?,
            valid_until: row.get(7)?,
            key: row.get(8)?,
        })
    }

    pub(super) fn into_allocation(self) -> Result<Allocation, AccountStoreError> {
        let ceiling = Grant::new(
            Money::new(unit_from_text(&self.unit)?, self.micros),
            from_text::<Purpose>("allocation purpose", &self.purpose)?,
            TimestampMs(self.valid_from),
            TimestampMs(self.valid_until),
        )?;
        Ok(Allocation {
            id: AllocationId::new(self.id)?,
            parent: AccountId::new(self.parent)?,
            child: AccountId::new(self.child)?,
            ceiling,
            idempotency_key: self.key.map(IdempotencyKey::new).transpose()?,
        })
    }
}

pub(super) fn load_allocation(
    conn: &Connection,
    id: &AllocationId,
) -> Result<Option<Allocation>, AccountStoreError> {
    conn.query_row(
        &format!("SELECT {ALLOCATION_COLUMNS} FROM allocations WHERE allocation_id = ?1"),
        [id.as_str()],
        RawAllocation::read,
    )
    .optional()?
    .map(RawAllocation::into_allocation)
    .transpose()
}

pub(super) fn load_allocation_by_key(
    conn: &Connection,
    key: &IdempotencyKey,
) -> Result<Option<Allocation>, AccountStoreError> {
    conn.query_row(
        &format!("SELECT {ALLOCATION_COLUMNS} FROM allocations WHERE idempotency_key = ?1"),
        [key.as_str()],
        RawAllocation::read,
    )
    .optional()?
    .map(RawAllocation::into_allocation)
    .transpose()
}

/// 某一列（`parent_id` 或 `child_id`，调用方写死，不是用户输入）等于 `account` 的全部额度，按 id 排序。
pub(super) fn load_allocations_where(
    conn: &Connection,
    column: &'static str,
    account: &AccountId,
) -> Result<Vec<Allocation>, AccountStoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ALLOCATION_COLUMNS} FROM allocations WHERE {column} = ?1 ORDER BY allocation_id"
    ))?;
    stmt.query_map([account.as_str()], RawAllocation::read)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(RawAllocation::into_allocation)
        .collect()
}

// ---------- credentials ----------

pub(super) fn load_credential(
    conn: &Connection,
    id: &CredentialId,
) -> Result<Option<Credential>, AccountStoreError> {
    type Raw = (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<i64>,
    );
    let raw: Option<Raw> = conn
        .query_row(
            "SELECT credential_id, binding_kind, account_id, membership_id, status, expires_at_ms \
             FROM credentials WHERE credential_id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((cred_id, kind, account, membership, status, expires_at)) = raw else {
        return Ok(None);
    };
    let binding = match (kind.as_str(), account, membership) {
        ("service", Some(account), None) => CredentialBinding::Service {
            account: AccountId::new(account)?,
        },
        ("node", Some(account), None) => CredentialBinding::Node {
            account: AccountId::new(account)?,
        },
        ("member", None, Some(membership)) => CredentialBinding::Member {
            membership: MembershipId::new(membership)?,
        },
        (other, _, _) => {
            return Err(AccountStoreError::Corrupt(format!(
                "credential {cred_id} has an inconsistent {other:?} binding"
            )));
        }
    };
    let mut stmt = conn.prepare(
        "SELECT op, subtree_id, max_amount_unit, max_amount_micros \
         FROM credential_admin_scopes WHERE credential_id = ?1 ORDER BY position",
    )?;
    let raw_scopes = stmt
        .query_map([id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut admin_scopes = Vec::with_capacity(raw_scopes.len());
    for (op, subtree, unit, micros) in raw_scopes {
        let max_amount = match (unit, micros) {
            (Some(unit), Some(micros)) => Some(Money::new(unit_from_text(&unit)?, micros)),
            (None, None) => None,
            _ => {
                return Err(AccountStoreError::Corrupt(format!(
                    "credential {cred_id} has a half-specified scope amount"
                )));
            }
        };
        admin_scopes.push(AdminScope {
            op: from_text("control op", &op)?,
            subtree: AccountId::new(subtree)?,
            max_amount,
        });
    }
    Ok(Some(Credential::new(
        CredentialId::new(cred_id)?,
        binding,
        from_text::<CredentialStatus>("credential status", &status)?,
        expires_at.map(TimestampMs),
        admin_scopes,
    )?))
}
