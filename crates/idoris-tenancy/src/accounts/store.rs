//! `AccountStore`：账户存储的读写函数（S1，供 S2 控制面和 S3 准入调用）。
//!
//! # 写入路径
//!
//! 每个写函数都在**一个** `BEGIN IMMEDIATE` 事务里完成「读出相关的一小片已有数据 -> 用 S0 的校验函数校验 ->
//! 写入」，所以两个线程 / 进程同时写同一片数据，不可能都通过检查再都写进去（校验和写入之间没有缝）。
//! 校验用的是契约层的函数，不在这里重新实现规则：
//!
//! | 写入 | 校验（S0） |
//! |---|---|
//! | [`AccountStore::create_account`] | [`AccountTree::build`]：单根、id 唯一、父账户存在、父子种类与单位合法、无环、深度上限 |
//! | [`AccountStore::create_membership`] | [`Directory::build`]：主体存在、组织存在且是 org / platform、角色与账户种类对得上、账户在组织子树里、一个账户至多一个成员关系 |
//! | [`AccountStore::create_credential`] | [`Credential::new`]（成员凭证不带管理权限、额度上限只给 `grant_allocation`），外加绑定目标种类与「权限范围不越出自己绑定的账户」 |
//! | [`AccountStore::create_allocation`] | [`validate_allocation`]：只沿树边下发、单位一致、不超过授权者本人、id 与幂等键不重复 |
//!
//! 「只取一小片」是 S0 文档给 S1–S3 的指引：账户树校验只需要被写账户的祖先链（含根），
//! 不需要把整棵树读进内存；被校验的小片和整棵树对同一次写入给出同样的结论（`tests` 里有对照）。
//!
//! 数据库层（`migrations/0001_accounts.sql`）再拦一遍：外键、唯一索引、CHECK、触发器。
//! 这一层只在有人绕过本文件的函数写数据时才会被触发。
//!
//! # 读取路径
//!
//! 读出来的每一行都重新走 S0 的构造函数（见 `rows.rs`），库里被改坏的数据读出来是错误，不会静默放行。
//!
//! # 时间
//!
//! 和 S0 一样，没有任何函数读系统时钟：需要「现在」的函数都显式收 `now: TimestampMs`。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, AccountTree, Allocation, AllocationError, Directory,
    DirectoryError, Membership, MembershipRole, MembershipStatus, Principal, PrincipalStatus,
    TreeError, subsidy_idempotency_key, validate_allocation,
};
use idoris_compute_contracts::credential::{
    Credential, CredentialBinding, CredentialStatus, PaymentContext, resolve_payment_context,
};
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, BillingMonth, CredentialId, IdempotencyKey, MembershipId, PrincipalId,
    TimestampMs,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::error::{AccountStoreError, PaymentContextError};
use super::rows::{
    AllocationRecord, account_exists, from_text, load_account, load_all_accounts,
    load_all_memberships, load_all_principals, load_allocation, load_allocation_record,
    load_allocation_record_by_key, load_allocations_where, load_ancestors, load_credential,
    load_membership, load_memberships_of, load_platform_roots, load_principal, load_status_text,
    merge_chains, to_text,
};
use crate::sqlite_conn::{
    PragmaSnapshot, STANDARD_BUSY_TIMEOUT, apply_standard, snapshot as pragma_snapshot,
};

/// 迁移按顺序执行，版本号 = 下标 + 1。0001 是 S1 的账户存储；0002 是 S2 的控制面（审计、凭证来源、主体登记、
/// 额度撤销、配对）。**只能在末尾追加**，已经发布的文件不能改。
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_accounts.sql"),
    include_str!("migrations/0002_control.sql"),
];

/// A credential carries at most this many control-plane scopes (also a CHECK on
/// `credentials.scope_count` in the migration).
const MAX_ADMIN_SCOPES_PER_CREDENTIAL: usize = 64;

/// 创建额度的结果。带幂等键的额度重发时返回已有的那一条，不再写第二条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocationOutcome {
    /// 新写入的额度。
    Created,
    /// 同一个幂等键、同样内容的额度早已存在；这是那一条（没有写任何东西）。
    /// 内容相同 = 父账户、子账户、金额 / 单位 / 用途 / 有效期都相同，不比较 `id`
    /// （重试的调用方每次可以换一个新 id，幂等键才是这次发放的身份）。
    Replayed(Allocation),
}

/// 账户存储。内部一条连接加互斥锁，和 `BudgetLedger` 同样的做法；写入靠 `BEGIN IMMEDIATE` 跨连接、跨进程串行。
pub struct AccountStore {
    conn: Mutex<Connection>,
}

impl AccountStore {
    /// 打开（必要时创建）账户存储。连接按 [`crate::sqlite_conn::ConnectionProfile::Standard`] 设置：
    /// WAL（已验证）、5 秒 `busy_timeout`、`foreign_keys=ON`（已回读）。账户存储的完整性依赖外键，所以没有宽松档位。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AccountStoreError> {
        Self::open_with_busy_timeout(path, STANDARD_BUSY_TIMEOUT)
    }

    /// 同 [`open`](Self::open)，显式指定 `busy_timeout`（测试里用短的）。
    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, AccountStoreError> {
        let mut conn = Connection::open(path.as_ref())?;
        apply_standard(&conn, busy_timeout)?;
        run_migrations(&mut conn)?;
        // S3 准入账本的表（自己的版本表，和上面的账户迁移互不共用）。
        super::ledger::run_migrations(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 这条连接当前的 `journal_mode` / `busy_timeout` / `foreign_keys`，用来做断言和诊断。
    pub fn connection_pragmas(&self) -> Result<PragmaSnapshot, AccountStoreError> {
        let conn = self.lock();
        pragma_snapshot(&conn).map_err(AccountStoreError::from)
    }

    /// 中毒的锁只会在上一次调用中途 panic 时出现。每个写函数要么提交要么随事务一起回滚，库本身不会被写坏，所以直接接着用。
    pub(super) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 一个写事务：`BEGIN IMMEDIATE`，`f` 返回 `Ok` 才提交，返回 `Err` 随事务丢弃而回滚。
    pub(super) fn write<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, AccountStoreError>,
    ) -> Result<T, AccountStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// 一个只读事务：同一个快照里读多张表，读完回滚。
    pub(super) fn read<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, AccountStoreError>,
    ) -> Result<T, AccountStoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let value = f(&tx)?;
        tx.rollback()?;
        Ok(value)
    }

    // ------------------------------------------------------------------ 主体

    /// 幂等地登记一个主体：不存在就以 `Active` 写入；已经存在就**原样返回**，不改任何东西。
    ///
    /// 特别地，已被撤销的主体不会被「再次登记」复活（开通方重试不能把撤回的同意变回有效）；
    /// 调用方拿到返回值后要自己看 `status`。
    pub fn ensure_principal(
        &self,
        id: &PrincipalId,
        now: TimestampMs,
    ) -> Result<Principal, AccountStoreError> {
        self.write(|tx| ensure_principal_in(tx, id, now))
    }

    pub fn get_principal(&self, id: &PrincipalId) -> Result<Option<Principal>, AccountStoreError> {
        self.read(|tx| load_principal(tx, id))
    }

    /// 撤销主体（开通方撤回了处理资格）。返回这次是否真的改了状态（已经撤销过返回 `false`）。
    /// 撤销不可逆。主体不存在是 [`AccountStoreError::NotFound`]。
    pub fn revoke_principal(
        &self,
        id: &PrincipalId,
        now: TimestampMs,
    ) -> Result<bool, AccountStoreError> {
        self.write(|tx| revoke_principal_in(tx, id, now))
    }

    // ------------------------------------------------------------------ 账户

    /// 新增一个付款账户。整个祖先链加上新账户一起过 [`AccountTree::build`]，所以单根、无环、深度上限、
    /// 父子种类与单位都由 S0 的函数判定。
    ///
    /// 第一个账户必须是 `Platform` 根；之后再加 `Platform` 是 `TreeError::MultipleRoots`。
    /// 父账户不存在是 `TreeError::MissingParent`；id 已存在是 [`AccountStoreError::AlreadyExists`]。
    pub fn create_account(
        &self,
        account: &Account,
        now: TimestampMs,
    ) -> Result<(), AccountStoreError> {
        self.write(|tx| create_account_in(tx, account, now))
    }

    pub fn get_account(&self, id: &AccountId) -> Result<Option<Account>, AccountStoreError> {
        self.read(|tx| load_account(tx, id))
    }

    /// `[自己, 父, ..., 根]`（与 `AccountTree::ancestors` 同序）。准入（S3）要沿它检查。账户不存在是 `NotFound`。
    pub fn ancestors(&self, id: &AccountId) -> Result<Vec<Account>, AccountStoreError> {
        self.read(|tx| {
            let chain = load_ancestors(tx, id)?;
            if chain.is_empty() {
                return Err(AccountStoreError::not_found("account", id.as_str()));
            }
            Ok(chain)
        })
    }

    /// 直接子账户，按 id 排序。
    pub fn children(&self, id: &AccountId) -> Result<Vec<Account>, AccountStoreError> {
        self.read(|tx| {
            let mut stmt = tx.prepare(
                "SELECT account_id, kind, parent_id, unit, status FROM accounts \
                 WHERE parent_id = ?1 ORDER BY account_id",
            )?;
            stmt.query_map([id.as_str()], super::rows::RawAccount::read)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(super::rows::RawAccount::into_account)
                .collect()
        })
    }

    /// 改账户状态。冻结和关闭会连带整个子树（`AccountTree::inactive_cause`）。关闭是终态，
    /// 从 `Closed` 改成别的状态是 [`AccountStoreError::IllegalTransition`]；改成当前状态是空操作。
    pub fn set_account_status(
        &self,
        id: &AccountId,
        status: AccountStatus,
    ) -> Result<(), AccountStoreError> {
        self.write(|tx| set_account_status_in(tx, id, status))
    }

    /// 整棵树（校验过）。库里还没有任何账户返回 `None`。这是 O(账户数) 的，给控制面和对照测试用，不要放进热路径；
    /// 热路径用 [`ancestors`](Self::ancestors) 和 [`payment_context`](Self::payment_context)。
    pub fn load_tree(&self) -> Result<Option<AccountTree>, AccountStoreError> {
        self.read(|tx| {
            let accounts = load_all_accounts(tx)?;
            if accounts.is_empty() {
                return Ok(None);
            }
            Ok(Some(AccountTree::build(accounts)?))
        })
    }

    // ------------------------------------------------------------------ 成员关系

    /// 新增成员关系。被校验的小片 = 主体 + 组织的祖先链 + 付款账户的祖先链，交给 [`Directory::build`]。
    /// 一个账户已经属于别的成员关系是 `DirectoryError::AccountAlreadyBound`。
    pub fn create_membership(
        &self,
        membership: &Membership,
        now: TimestampMs,
    ) -> Result<(), AccountStoreError> {
        self.write(|tx| create_membership_in(tx, membership, now))
    }

    pub fn get_membership(
        &self,
        id: &MembershipId,
    ) -> Result<Option<Membership>, AccountStoreError> {
        self.read(|tx| load_membership(tx, id))
    }

    /// 一个主体的全部成员关系（含已撤销的），按成员关系 id 排序。
    pub fn memberships_of(
        &self,
        principal: &PrincipalId,
    ) -> Result<Vec<Membership>, AccountStoreError> {
        self.read(|tx| load_memberships_of(tx, principal))
    }

    /// 撤销成员关系。返回这次是否真的改了状态。撤销不可逆。
    pub fn revoke_membership(
        &self,
        id: &MembershipId,
        now: TimestampMs,
    ) -> Result<bool, AccountStoreError> {
        self.write(|tx| revoke_membership_in(tx, id, now))
    }

    // ------------------------------------------------------------------ 凭证

    /// 新增凭证（只有元数据，没有密钥；见迁移文件里的说明）。
    ///
    /// 除了 [`Credential::new`] 的校验，还要求：
    /// - `Service` / `Node` 绑定的账户存在且是 `Org`；`Member` 绑定的成员关系存在且有付款账户（成员或担保人，不是管理员）；
    /// - 每条管理权限的子树存在，并且在凭证自己绑定的账户之内（凭证不能管到自己绑定范围之外）。
    ///
    /// 「下发的权限不能超过授权者本人」要知道授权者，所以不在这里：S2 先用 S0 的
    /// `check_credential_issuance` 校验，再调这个函数落库。
    pub fn create_credential(
        &self,
        credential: &Credential,
        now: TimestampMs,
    ) -> Result<(), AccountStoreError> {
        self.write(|tx| create_credential_in(tx, credential, now))
    }

    pub fn get_credential(
        &self,
        id: &CredentialId,
    ) -> Result<Option<Credential>, AccountStoreError> {
        self.read(|tx| load_credential(tx, id))
    }

    /// 改凭证状态：`Active` <-> `Frozen` 可以来回；任何状态都可以到 `Revoked`；撤销不可逆
    /// （从 `Revoked` 改成别的是 [`AccountStoreError::IllegalTransition`]）。改成当前状态是空操作。
    pub fn set_credential_status(
        &self,
        id: &CredentialId,
        status: CredentialStatus,
        now: TimestampMs,
    ) -> Result<(), AccountStoreError> {
        self.write(|tx| set_credential_status_in(tx, id, status, now))
    }

    // ------------------------------------------------------------------ 额度

    /// 新增一笔额度（授权上限，不是转移余额），或者在幂等键重发时返回已有的那一笔。
    ///
    /// 顺序：
    /// 1. 有幂等键且库里已有同键的额度：内容相同 -> [`AllocationOutcome::Replayed`]，不写任何东西；
    ///    内容不同 -> `AllocationError::DuplicateIdempotencyKey`；
    /// 2. 免费额度必须带幂等键 `subsidy:<子账户所属成员的主体>:<YYYY-MM>`（设计稿 §4：幂等键 `(成员, 月份)`），
    ///    别的用途不许用 `subsidy:` 键，否则是 [`AccountStoreError::SubsidyKey`]；
    /// 3. 子账户和父账户的祖先链过 [`AccountTree::build`]，再连同「流入父账户的全部额度」交给 [`validate_allocation`]：
    ///    只沿树边下发、单位一致、不超过授权者本人、id 不重复。
    ///
    /// 同一个 `(主体, 月份)` 的补贴只能发一次：两个组织各有该主体的成员关系，也只有第一笔写得进去。
    pub fn create_allocation(
        &self,
        allocation: &Allocation,
        now: TimestampMs,
    ) -> Result<AllocationOutcome, AccountStoreError> {
        self.write(|tx| create_allocation_in(tx, allocation, now))
    }

    /// 按 id 取一笔**有效的**额度。被撤销的额度读不到（它不再给任何人授权）；要连同撤销时间一起看，用
    /// [`allocation_record`](Self::allocation_record)。
    pub fn get_allocation(
        &self,
        id: &AllocationId,
    ) -> Result<Option<Allocation>, AccountStoreError> {
        self.read(|tx| load_allocation(tx, id))
    }

    /// 按 id 取一笔额度和它的撤销时间，不管有没有被撤销（S2：`allocations.revoked_at_ms`）。
    pub fn allocation_record(
        &self,
        id: &AllocationId,
    ) -> Result<Option<AllocationRecord>, AccountStoreError> {
        self.read(|tx| load_allocation_record(tx, id))
    }

    /// 流入 `child` 的全部**有效的**额度（谁给了它多少上限），按 id 排序。被撤销的不算：它们不再给任何人授权。
    pub fn allocations_into(
        &self,
        child: &AccountId,
    ) -> Result<Vec<Allocation>, AccountStoreError> {
        self.read(|tx| load_allocations_where(tx, "child_id", child))
    }

    /// `parent` 下发出去的全部**有效的**额度，按 id 排序。被撤销的不算。
    pub fn allocations_from(
        &self,
        parent: &AccountId,
    ) -> Result<Vec<Allocation>, AccountStoreError> {
        self.read(|tx| load_allocations_where(tx, "parent_id", parent))
    }

    // ------------------------------------------------------------------ 目录与付款上下文

    /// 整个目录（账户树 + 主体 + 成员关系，校验过）。库里还没有任何账户返回 `None`。
    /// O(库大小)：给控制面、对照测试和人工检查用，不要放进热路径。
    pub fn load_directory(&self) -> Result<Option<Directory>, AccountStoreError> {
        self.read(|tx| {
            let accounts = load_all_accounts(tx)?;
            if accounts.is_empty() {
                return Ok(None);
            }
            let tree = AccountTree::build(accounts)?;
            Ok(Some(Directory::build(
                tree,
                load_all_principals(tx)?,
                load_all_memberships(tx)?,
            )?))
        })
    }

    /// 由凭证（加上开通方提交的、经过校验的成员 id）得到这一次请求**唯一**的付款上下文。
    ///
    /// 只读取这次解析需要的一小片（凭证绑定的账户链，加上该主体全部成员关系各自的组织链和账户链），
    /// 交给 S0 的 [`resolve_payment_context`]。拿整个目录解析得到同样的结果（`tests` 里逐场景对照）。
    /// 解析失败的原因码见 `ResolveError::reason_code`。
    pub fn payment_context(
        &self,
        credential: &CredentialId,
        asserted_member: Option<&PrincipalId>,
        now: TimestampMs,
    ) -> Result<PaymentContext, PaymentContextError> {
        let loaded = self
            .read(|tx| {
                let Some(found) = load_credential(tx, credential)? else {
                    return Ok(None);
                };
                let directory = directory_slice(tx, &found, asserted_member)?;
                Ok(Some((found, directory)))
            })
            .map_err(PaymentContextError::Store)?;
        let Some((found, directory)) = loaded else {
            return Err(PaymentContextError::UnknownCredential(credential.clone()));
        };
        resolve_payment_context(&directory, &found, asserted_member, now)
            .map_err(PaymentContextError::Resolve)
    }
}

// ---------------------------------------------------------------------------------------------
// 写入的实现：每个函数在**调用方已经开好的**事务（或保存点）里做「读出一小片 -> S0 校验 -> 写入」。
// 公开的 `AccountStore::*` 方法各开一个 `BEGIN IMMEDIATE` 事务调用它们；S2 控制面（`control.rs`）在自己的事务里
// 先授权、再调用它们、最后写审计，所以「授权 + 写入 + 审计」是同一个事务。
// ---------------------------------------------------------------------------------------------

/// 幂等地登记一个主体，见 [`AccountStore::ensure_principal`]。
pub(super) fn ensure_principal_in(
    tx: &Connection,
    id: &PrincipalId,
    now: TimestampMs,
) -> Result<Principal, AccountStoreError> {
    // Check, then insert, inside one `BEGIN IMMEDIATE` transaction. Not `INSERT OR IGNORE`: the
    // database refuses every insert that would collide with an existing row (see the
    // `*_insert_no_conflict` triggers), which is also what keeps `INSERT OR REPLACE` from
    // reviving a revoked principal.
    if load_principal(tx, id)?.is_none() {
        tx.execute(
            "INSERT INTO principals (principal_id, status, created_at_ms, revoked_at_ms) \
             VALUES (?1, 'active', ?2, NULL)",
            rusqlite::params![id.as_str(), now.0],
        )?;
    }
    load_principal(tx, id)?.ok_or_else(|| AccountStoreError::not_found("principal", id.as_str()))
}

/// 撤销主体，见 [`AccountStore::revoke_principal`]。
pub(super) fn revoke_principal_in(
    tx: &Connection,
    id: &PrincipalId,
    now: TimestampMs,
) -> Result<bool, AccountStoreError> {
    let current = load_status_text(tx, "principals", "principal_id", id.as_str())?
        .ok_or_else(|| AccountStoreError::not_found("principal", id.as_str()))?;
    if from_text::<PrincipalStatus>("principal status", &current)? == PrincipalStatus::Revoked {
        return Ok(false);
    }
    tx.execute(
        "UPDATE principals SET status = 'revoked', revoked_at_ms = ?2 WHERE principal_id = ?1",
        rusqlite::params![id.as_str(), now.0],
    )?;
    Ok(true)
}

/// 新增账户，见 [`AccountStore::create_account`]。
pub(super) fn create_account_in(
    tx: &Connection,
    account: &Account,
    now: TimestampMs,
) -> Result<(), AccountStoreError> {
    if account_exists(tx, &account.id)? {
        return Err(AccountStoreError::already_exists(
            "account",
            account.id.as_str(),
        ));
    }
    let mut slice = match &account.parent {
        Some(parent) => {
            let chain = load_ancestors(tx, parent)?;
            if chain.is_empty() {
                return Err(TreeError::MissingParent {
                    child: account.id.clone(),
                    parent: parent.clone(),
                }
                .into());
            }
            chain
        }
        None => load_platform_roots(tx)?,
    };
    slice.push(account.clone());
    AccountTree::build(slice)?;
    tx.execute(
        "INSERT INTO accounts (account_id, kind, parent_id, unit, status, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            account.id.as_str(),
            to_text(&account.kind)?,
            account.parent.as_ref().map(AccountId::as_str),
            account.unit.to_string(),
            to_text(&account.status)?,
            now.0,
        ],
    )?;
    Ok(())
}

/// 改账户状态，见 [`AccountStore::set_account_status`]。
pub(super) fn set_account_status_in(
    tx: &Connection,
    id: &AccountId,
    status: AccountStatus,
) -> Result<(), AccountStoreError> {
    let current = load_status_text(tx, "accounts", "account_id", id.as_str())?
        .ok_or_else(|| AccountStoreError::not_found("account", id.as_str()))?;
    let current = from_text::<AccountStatus>("account status", &current)?;
    if current == status {
        return Ok(());
    }
    if current == AccountStatus::Closed {
        return Err(AccountStoreError::IllegalTransition {
            kind: "account",
            id: id.to_string(),
            from: to_text(&current)?,
            to: to_text(&status)?,
        });
    }
    tx.execute(
        "UPDATE accounts SET status = ?2 WHERE account_id = ?1",
        rusqlite::params![id.as_str(), to_text(&status)?],
    )?;
    Ok(())
}

/// 新增成员关系，见 [`AccountStore::create_membership`]。
pub(super) fn create_membership_in(
    tx: &Connection,
    membership: &Membership,
    now: TimestampMs,
) -> Result<(), AccountStoreError> {
    if load_membership(tx, &membership.id)?.is_some() {
        return Err(AccountStoreError::already_exists(
            "membership",
            membership.id.as_str(),
        ));
    }
    let principal = load_principal(tx, &membership.principal)?.ok_or_else(|| {
        DirectoryError::UnknownPrincipal {
            membership: membership.id.clone(),
            principal: membership.principal.clone(),
        }
    })?;
    let mut accounts = BTreeMap::new();
    let org_chain = load_ancestors(tx, &membership.org)?;
    if org_chain.is_empty() {
        return Err(DirectoryError::UnknownOrg {
            membership: membership.id.clone(),
            org: membership.org.clone(),
        }
        .into());
    }
    merge_chains(&mut accounts, org_chain);
    if let Some(account) = &membership.account {
        merge_chains(&mut accounts, load_ancestors(tx, account)?);
    }
    let tree = AccountTree::build(accounts.into_values().collect())?;
    // 结构错误（角色与账户种类对不上、账户不在组织子树里……）先由 S0 报；
    // 通过之后才查「这个账户是不是已经属于别的成员关系」，那是 S0 看一小片数据看不到的跨成员关系的规则。
    Directory::build(tree, vec![principal], vec![membership.clone()])?;
    if let Some(account) = &membership.account {
        let bound: Option<String> = tx
            .query_row(
                "SELECT membership_id FROM memberships WHERE account_id = ?1",
                [account.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(first) = bound {
            return Err(DirectoryError::AccountAlreadyBound {
                account: account.clone(),
                first: MembershipId::new(first)?,
                second: membership.id.clone(),
            }
            .into());
        }
    }
    let revoked_at = (membership.status == MembershipStatus::Revoked).then_some(now.0);
    tx.execute(
        "INSERT INTO memberships \
            (membership_id, principal_id, org_id, role, account_id, status, created_at_ms, revoked_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            membership.id.as_str(),
            membership.principal.as_str(),
            membership.org.as_str(),
            to_text(&membership.role)?,
            membership.account.as_ref().map(AccountId::as_str),
            to_text(&membership.status)?,
            now.0,
            revoked_at,
        ],
    )?;
    Ok(())
}

/// 撤销成员关系，见 [`AccountStore::revoke_membership`]。
pub(super) fn revoke_membership_in(
    tx: &Connection,
    id: &MembershipId,
    now: TimestampMs,
) -> Result<bool, AccountStoreError> {
    let current = load_status_text(tx, "memberships", "membership_id", id.as_str())?
        .ok_or_else(|| AccountStoreError::not_found("membership", id.as_str()))?;
    if from_text::<MembershipStatus>("membership status", &current)? == MembershipStatus::Revoked {
        return Ok(false);
    }
    tx.execute(
        "UPDATE memberships SET status = 'revoked', revoked_at_ms = ?2 WHERE membership_id = ?1",
        rusqlite::params![id.as_str(), now.0],
    )?;
    Ok(true)
}

/// 新增凭证（只有元数据），见 [`AccountStore::create_credential`]。
pub(super) fn create_credential_in(
    tx: &Connection,
    credential: &Credential,
    now: TimestampMs,
) -> Result<(), AccountStoreError> {
    if load_status_text(tx, "credentials", "credential_id", credential.id().as_str())?.is_some() {
        return Err(AccountStoreError::already_exists(
            "credential",
            credential.id().as_str(),
        ));
    }
    if credential.admin_scopes().len() > MAX_ADMIN_SCOPES_PER_CREDENTIAL {
        return Err(AccountStoreError::InvalidBinding(format!(
            "a credential carries at most {MAX_ADMIN_SCOPES_PER_CREDENTIAL} admin scopes, got {}",
            credential.admin_scopes().len()
        )));
    }
    let (kind, account, membership, anchor) = match credential.binding() {
        CredentialBinding::Service { account } => {
            ("service", Some(account), None, org_anchor(tx, account)?)
        }
        CredentialBinding::Node { account } => {
            ("node", Some(account), None, org_anchor(tx, account)?)
        }
        CredentialBinding::Member { membership } => {
            let found = load_membership(tx, membership)?
                .ok_or_else(|| AccountStoreError::not_found("membership", membership.as_str()))?;
            let payer = match (found.role, found.account) {
                (MembershipRole::Member | MembershipRole::Sponsor, Some(payer)) => payer,
                _ => {
                    return Err(AccountStoreError::InvalidBinding(format!(
                        "membership {membership} has no payment account (administrators cannot be bound)"
                    )));
                }
            };
            ("member", None, Some(membership), payer)
        }
    };
    for scope in credential.admin_scopes() {
        let chain = load_ancestors(tx, &scope.subtree)?;
        if chain.is_empty() {
            return Err(AccountStoreError::not_found(
                "account",
                scope.subtree.as_str(),
            ));
        }
        if !chain.iter().any(|ancestor| ancestor.id == anchor) {
            return Err(AccountStoreError::InvalidBinding(format!(
                "admin scope subtree {} is outside the credential's own binding {anchor}",
                scope.subtree
            )));
        }
    }
    let status = credential.status();
    // `scope_count` is fixed here, at issuance: the database refuses admin-scope rows beyond it
    // (and refuses to change it), so scopes can never be appended to an existing credential.
    tx.execute(
        "INSERT INTO credentials \
            (credential_id, binding_kind, account_id, membership_id, status, expires_at_ms, \
             created_at_ms, revoked_at_ms, scope_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            credential.id().as_str(),
            kind,
            account.map(AccountId::as_str),
            membership.map(MembershipId::as_str),
            to_text(&status)?,
            credential.expires_at().map(|at| at.0),
            now.0,
            (status == CredentialStatus::Revoked).then_some(now.0),
            i64::try_from(credential.admin_scopes().len()).unwrap_or(i64::MAX),
        ],
    )?;
    for (position, scope) in credential.admin_scopes().iter().enumerate() {
        tx.execute(
            "INSERT INTO credential_admin_scopes \
                (credential_id, position, op, subtree_id, max_amount_unit, max_amount_micros) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                credential.id().as_str(),
                i64::try_from(position).unwrap_or(i64::MAX),
                to_text(&scope.op)?,
                scope.subtree.as_str(),
                scope
                    .max_amount
                    .as_ref()
                    .map(|money| money.unit.to_string()),
                scope.max_amount.as_ref().map(|money| money.micros),
            ],
        )?;
    }
    Ok(())
}

/// 改凭证状态，见 [`AccountStore::set_credential_status`]。
pub(super) fn set_credential_status_in(
    tx: &Connection,
    id: &CredentialId,
    status: CredentialStatus,
    now: TimestampMs,
) -> Result<(), AccountStoreError> {
    // Status column only (not the whole credential): revoking must keep working even if the
    // credential's scope rows are unreadable.
    let current = load_status_text(tx, "credentials", "credential_id", id.as_str())?
        .ok_or_else(|| AccountStoreError::not_found("credential", id.as_str()))?;
    let current = from_text::<CredentialStatus>("credential status", &current)?;
    if current == status {
        return Ok(());
    }
    if current == CredentialStatus::Revoked {
        return Err(AccountStoreError::IllegalTransition {
            kind: "credential",
            id: id.to_string(),
            from: to_text(&current)?,
            to: to_text(&status)?,
        });
    }
    tx.execute(
        "UPDATE credentials SET status = ?2, revoked_at_ms = ?3 WHERE credential_id = ?1",
        rusqlite::params![
            id.as_str(),
            to_text(&status)?,
            (status == CredentialStatus::Revoked).then_some(now.0),
        ],
    )?;
    Ok(())
}

/// 新增额度（或幂等重发），见 [`AccountStore::create_allocation`]。
pub(super) fn create_allocation_in(
    tx: &Connection,
    allocation: &Allocation,
    now: TimestampMs,
) -> Result<AllocationOutcome, AccountStoreError> {
    if let Some(key) = &allocation.idempotency_key
        && let Some(existing) = load_allocation_record_by_key(tx, key)?
    {
        return if !same_issuance(&existing.allocation, allocation) {
            Err(AllocationError::DuplicateIdempotencyKey(key.clone()).into())
        } else if existing.revoked_at_ms.is_some() {
            // 同一次发放，但这笔额度已经被撤销：重发不能把它「复活」，也不能假装它还有效。
            Err(AccountStoreError::AllocationRevoked {
                id: existing.allocation.id.to_string(),
            })
        } else {
            Ok(AllocationOutcome::Replayed(existing.allocation))
        };
    }
    check_subsidy_key(tx, allocation)?;
    let mut accounts = BTreeMap::new();
    merge_chains(&mut accounts, load_ancestors(tx, &allocation.child)?);
    merge_chains(&mut accounts, load_ancestors(tx, &allocation.parent)?);
    if !accounts.contains_key(&allocation.child) {
        return Err(AllocationError::UnknownAccount(allocation.child.clone()).into());
    }
    if !accounts.contains_key(&allocation.parent) {
        return Err(AllocationError::UnknownAccount(allocation.parent.clone()).into());
    }
    let tree = AccountTree::build(accounts.into_values().collect())?;
    // `validate_allocation` 读 `existing` 的三种信息：同 id、同幂等键、流入父账户的额度。
    // 流入父账户的额度只算**有效的**（被撤销的不再给父账户授权）；同 id 的要连被撤销的一起看：
    // id 一旦用过就不能换一个含义再用。
    let mut existing = load_allocations_where(tx, "child_id", &allocation.parent)?;
    if let Some(same_id) = load_allocation_record(tx, &allocation.id)?
        && !existing.iter().any(|a| a.id == same_id.allocation.id)
    {
        existing.push(same_id.allocation);
    }
    validate_allocation(&tree, &existing, allocation)?;
    let ceiling = &allocation.ceiling;
    tx.execute(
        "INSERT INTO allocations \
            (allocation_id, parent_id, child_id, ceiling_unit, ceiling_micros, purpose, \
             valid_from_ms, valid_until_ms, transferable, idempotency_key, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10)",
        rusqlite::params![
            allocation.id.as_str(),
            allocation.parent.as_str(),
            allocation.child.as_str(),
            ceiling.unit().to_string(),
            ceiling.amount().micros,
            to_text(&ceiling.purpose())?,
            ceiling.valid_from().0,
            ceiling.valid_until().0,
            allocation
                .idempotency_key
                .as_ref()
                .map(IdempotencyKey::as_str),
            now.0,
        ],
    )?;
    Ok(AllocationOutcome::Created)
}

/// `Service` / `Node` 凭证的绑定账户必须存在且是 `Org`。返回它（也是权限范围的外框）。
fn org_anchor(tx: &Connection, account: &AccountId) -> Result<AccountId, AccountStoreError> {
    let found = load_account(tx, account)?
        .ok_or_else(|| AccountStoreError::not_found("account", account.as_str()))?;
    if found.kind != AccountKind::Org {
        return Err(AccountStoreError::InvalidBinding(format!(
            "service and node credentials must be bound to an org account, but {account} is {:?}",
            found.kind
        )));
    }
    Ok(found.id)
}

/// 两笔额度是不是同一次发放：父、子、金额 / 单位 / 用途 / 有效期都相同；不比较 `id`。
fn same_issuance(left: &Allocation, right: &Allocation) -> bool {
    left.parent == right.parent && left.child == right.child && left.ceiling == right.ceiling
}

/// 免费额度的幂等键规则（见 [`AccountStore::create_allocation`] 第 2 条）。
fn check_subsidy_key(tx: &Connection, allocation: &Allocation) -> Result<(), AccountStoreError> {
    const PREFIX: &str = "subsidy:";
    let key = allocation.idempotency_key.as_ref();
    if allocation.ceiling.purpose() != Purpose::FreeTier {
        return match key {
            Some(key) if key.as_str().starts_with(PREFIX) => Err(AccountStoreError::SubsidyKey(
                format!("key {key} is reserved for free-tier allocations"),
            )),
            _ => Ok(()),
        };
    }
    let key = key.ok_or_else(|| {
        AccountStoreError::SubsidyKey("a free-tier allocation needs an idempotency key".into())
    })?;
    let principal: Option<String> = tx
        .query_row(
            "SELECT principal_id FROM memberships WHERE account_id = ?1",
            [allocation.child.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let principal = principal.ok_or_else(|| {
        AccountStoreError::SubsidyKey(format!(
            "child account {} is not bound to any membership, so there is no member to grant to",
            allocation.child
        ))
    })?;
    let principal = PrincipalId::new(principal)?;
    let month = key
        .as_str()
        .rsplit_once(':')
        .and_then(|(_, month)| month.parse::<BillingMonth>().ok())
        .ok_or_else(|| {
            AccountStoreError::SubsidyKey(format!("key {key} does not end in a YYYY-MM month"))
        })?;
    let expected = subsidy_idempotency_key(&principal, month)?;
    if expected != *key {
        return Err(AccountStoreError::SubsidyKey(format!(
            "key {key} does not name the child's member: expected {expected}"
        )));
    }
    Ok(())
}

/// 解析一次付款上下文需要的那一小片目录。
///
/// - `Service` / `Node`：绑定账户的祖先链；`Service` 再加上被断言的主体、它的全部成员关系，
///   以及每个成员关系的组织链和账户链；
/// - `Member`：该成员关系、它的主体，以及它的组织链和账户链。
///
/// 所有链都止于同一个平台根，所以合起来是一棵合法的树；`Directory::build` 会再校验一遍。
pub(super) fn directory_slice(
    tx: &Connection,
    credential: &Credential,
    asserted_member: Option<&PrincipalId>,
) -> Result<Directory, AccountStoreError> {
    let mut accounts = BTreeMap::new();
    let mut principals: Vec<Principal> = Vec::new();
    let mut memberships: Vec<Membership> = Vec::new();
    let mut add_membership = |tx: &Connection,
                              accounts: &mut BTreeMap<AccountId, Account>,
                              membership: Membership|
     -> Result<(), AccountStoreError> {
        merge_chains(accounts, load_ancestors(tx, &membership.org)?);
        if let Some(account) = &membership.account {
            merge_chains(accounts, load_ancestors(tx, account)?);
        }
        memberships.push(membership);
        Ok(())
    };
    match credential.binding() {
        CredentialBinding::Service { account } => {
            merge_chains(&mut accounts, load_ancestors(tx, account)?);
            if let Some(who) = asserted_member
                && let Some(principal) = load_principal(tx, who)?
            {
                for membership in load_memberships_of(tx, who)? {
                    add_membership(tx, &mut accounts, membership)?;
                }
                principals.push(principal);
            }
        }
        CredentialBinding::Node { account } => {
            merge_chains(&mut accounts, load_ancestors(tx, account)?);
        }
        CredentialBinding::Member { membership } => {
            if let Some(found) = load_membership(tx, membership)? {
                if let Some(principal) = load_principal(tx, &found.principal)? {
                    principals.push(principal);
                }
                add_membership(tx, &mut accounts, found)?;
            }
        }
    }
    if accounts.is_empty() {
        // 凭证指向的东西不存在：给 S0 一棵非空的树（平台根），让它自己报「未知账户 / 成员关系」。
        merge_chains(&mut accounts, load_platform_roots(tx)?);
    }
    if accounts.is_empty() {
        return Err(AccountStoreError::Corrupt(
            "no accounts exist yet, so no payment context can be resolved".into(),
        ));
    }
    let tree = AccountTree::build(accounts.into_values().collect())?;
    Ok(Directory::build(tree, principals, memberships)?)
}

fn run_migrations(conn: &mut Connection) -> Result<(), AccountStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_schema_migrations (\
            version INTEGER PRIMARY KEY, applied_at_ms INTEGER NOT NULL) STRICT",
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).unwrap_or(i64::MAX);
        let applied: Option<i64> = tx
            .query_row(
                "SELECT version FROM account_schema_migrations WHERE version = ?1",
                [version],
                |row| row.get(0),
            )
            .optional()?;
        if applied.is_some() {
            continue;
        }
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO account_schema_migrations (version, applied_at_ms) VALUES (?1, ?2)",
            rusqlite::params![version, wall_clock_ms()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// 只用来给迁移记录打时间戳（和账本迁移同样的做法），不参与任何业务判断。
fn wall_clock_ms() -> i64 {
    use crate::budget::{Clock, SystemClock};
    SystemClock.now_ms()
}
