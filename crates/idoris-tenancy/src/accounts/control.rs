//! S2 控制面的存储层（设计稿 §9 第 1 项）：开户、主体、成员关系、凭证、额度的**写操作**，每一个都是
//! 「认证调用者 -> 授权 -> 校验 -> 写入 -> 审计」在**同一个** `BEGIN IMMEDIATE` 事务里完成。
//!
//! 写操作的 HTTP 接口在 `idoris-router::control`，本地运维命令（`idoris control bootstrap`）也调用这里；
//! 授权规则在 `idoris-compute-contracts::control::authorize`（纯函数，两边共用，不在这里重新实现）。
//!
//! # 为什么认证、授权、写入、审计必须在一个事务里
//!
//! - **撤销即时生效没有缝**：调用者的凭证状态、权限范围、绑定账户的状态是在写操作的事务里读的。
//!   另一个请求同时撤销了这张凭证（或冻结了它的组织），要么它的事务先提交（这里就读到已撤销、拒绝），
//!   要么这里先提交（这次写操作发生在撤销之前）；不存在「授权时还有效、写入时已撤销」。
//! - **没有无审计的写**：成功的写操作和它的审计行一起提交；审计行写不进去，整个事务回滚，调用方得到
//!   `audit_unavailable`，什么都没有发生。
//! - **被拒的写也有审计**：授权失败 / 请求被驳回时，主体的写入在保存点里回滚，审计行照常提交。
//!
//! # 谁会进审计
//!
//! 调用者**被认出是哪张凭证**（密钥哈希匹配了 `virtual_keys` 里的一行，不论这行现在是不是有效）才有审计：
//! 已撤销、已过期、已冻结的凭证再来调用，也留一行（这正是「被盗的密钥有人在用」的线索）。
//! **认不出的密钥（未知、格式不对、Admin token）没有凭证 id，不写审计行**：否则任何人往公网上发垃圾请求
//! 都能把库写满，而这个库同时是账本。这类请求只返回 `unauthenticated`。
//!
//! 审计行里只有标识符（凭证 id、账户 id……）、操作名、结果和原因码，**没有任何密钥、哈希、token、
//! 配对码、设备显示名或请求正文**。
//!
//! # 本机运维人员
//!
//! `Who::Operator` 是本机命令行的调用者（拥有库文件的人本来就是最高权限）：不做授权判定，其余照旧——
//! 同样的校验、同样的审计（`actor_kind = operator`）。它是创建平台根、第一批组织和第一张管理凭证的唯一方式：
//! 平台账户不是 `Org`，S1 的约束又要求凭证只能绑 `Org`，所以没有任何一张凭证能管到平台根。

use std::collections::BTreeMap;

use idoris_compute_contracts::account::{
    Account, AccountStatus, AccountTree, Allocation, AllocationError, Directory, DirectoryError,
    Membership, MembershipStatus, Principal, PrincipalStatus, subsidy_idempotency_key,
};
use idoris_compute_contracts::control::{
    AuditOutcome, ControlReason, WriteResult, authorize, would_disable_caller,
};
use idoris_compute_contracts::credential::{
    AdminScope, ControlOp, Credential, CredentialBinding, CredentialStatus,
    check_credential_issuance,
};
use idoris_compute_contracts::grant::{Grant, GrantError, Purpose};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, BillingMonth, CredentialId, IdempotencyKey, MembershipId, PrincipalId,
    TimestampMs,
};
use idoris_compute_contracts::money::{Money, MoneyError};
use idoris_contracts::common::PrivacyClass;
use rusqlite::{Connection, OptionalExtension};

use super::error::AccountStoreError;
use super::ledger::{self, LedgerError};
use super::pairing::{
    MAX_ACTIVE_DEVICES_PER_MEMBERSHIP, PairingRow, occupied_device_slots, resolve_pairing_caller,
};
use super::rows::{
    AllocationRecord, load_account, load_allocation_record, load_ancestors, load_credential,
    load_membership, load_membership_within, load_memberships_of, load_platform_roots,
    load_principal, merge_chains,
};
use super::store::{
    AccountStore, AllocationOutcome, check_new_membership_in, create_account_in,
    create_allocation_in, create_credential_in, create_membership_in, ensure_principal_in,
    revoke_membership_in, revoke_principal_in, set_account_status_in, set_credential_status_in,
};
use crate::virtual_key::VirtualKeyHash;

// ================================================================================================
// 公共类型
// ================================================================================================

/// 谁在调用。
#[derive(Debug, Clone, Copy)]
pub enum Who<'a> {
    /// 凭证的密钥哈希（HTTP 层从 `Authorization` 头解析出密钥后算出来；明文密钥不进存储层）。
    Secret(&'a VirtualKeyHash),
    /// 本机运维命令：不做授权判定（见模块文档）。`label` 进审计的 `actor_id`，必须是标识符字符集。
    Operator(&'a str),
    /// 配对码的哈希（只给兑换用，见 `pairing.rs`）：码本身就是凭据，没有任何管理权限。
    PairingCode(&'a [u8; 32]),
}

/// 一次调用的上下文：时间由调用方给（和整个计算服务一样，存储层不读系统时钟），请求号进审计行。
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    pub now: TimestampMs,
    pub request_id: &'a str,
}

/// 控制面操作没有成功：稳定原因码 + 给运维看的细节。
///
/// `detail` 只对「调用者有资格、但请求不合法 / 冲突」这类驳回有意义；授权不通过（`out_of_scope` 等）、
/// 凭证状态一律留空，调用方**不能**拿它探测别的租户的资源。它不含任何密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub reason: ControlReason,
    pub detail: String,
}

impl Failure {
    pub fn new(reason: ControlReason) -> Self {
        Self {
            reason,
            detail: String::new(),
        }
    }

    pub fn with_detail(reason: ControlReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    pub(super) fn internal(detail: impl Into<String>) -> Self {
        Self::with_detail(ControlReason::InternalError, detail)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.detail.is_empty() {
            f.write_str(self.reason.as_str())
        } else {
            write!(f, "{}: {}", self.reason, self.detail)
        }
    }
}

impl std::error::Error for Failure {}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Self::from(AccountStoreError::from(error))
    }
}

impl From<AccountStoreError> for Failure {
    fn from(error: AccountStoreError) -> Self {
        Failure::from_store(error, Ctx::Generic)
    }
}

/// 操作成功：结果类别 + 操作的对象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written<T> {
    pub result: WriteResult,
    pub value: T,
}

/// 把存储层错误映射成控制面原因码时，要知道调用方正在做哪一类操作（同一个 `TreeError` 在建账户时是
/// `invalid_account`，在建成员关系时是 `invalid_membership`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ctx {
    Generic,
    Account,
    Membership,
    Credential,
    Allocation,
}

impl Failure {
    /// 账本（S3）的错误映射成控制面原因码。细节里**不放**补贴池的余量等数字：池是平台级的、被很多组织共用，
    /// 剩余额度会泄露别的组织的用量。
    pub(super) fn from_ledger(error: LedgerError) -> Self {
        match error {
            LedgerError::Store(inner) => Failure::from_store(inner, Ctx::Allocation),
            LedgerError::PoolCapExceeded { .. } => Failure::with_detail(
                ControlReason::AllocationExceedsGrantor,
                "the subsidy pool's cap for this month cannot cover this free-tier allocation",
            ),
            LedgerError::NoSubsidyPool(_) => Failure::with_detail(
                ControlReason::InvalidAllocation,
                "no subsidy pool sits above the member account, so there is no cap to grant a free-tier allowance against",
            ),
            LedgerError::GrantConflict(_) => Failure::with_detail(
                ControlReason::IdempotencyConflict,
                "this free-tier idempotency key was already used for a different allocation",
            ),
            LedgerError::Invalid(detail) => {
                Failure::with_detail(ControlReason::InvalidAllocation, detail)
            }
            LedgerError::Money(money) => {
                Failure::with_detail(ControlReason::from(&money), money.to_string())
            }
            other => Failure::internal(other.to_string()),
        }
    }

    pub(super) fn from_store(error: AccountStoreError, ctx: Ctx) -> Self {
        let detail = error.to_string();
        let invalid = |generic: ControlReason| match ctx {
            Ctx::Account => ControlReason::InvalidAccount,
            Ctx::Membership => ControlReason::InvalidMembership,
            Ctx::Credential => ControlReason::InvalidCredential,
            Ctx::Allocation => ControlReason::InvalidAllocation,
            Ctx::Generic => generic,
        };
        match error {
            AccountStoreError::Busy => Failure::new(ControlReason::StoreBusy),
            AccountStoreError::AlreadyExists { .. } => {
                Failure::with_detail(ControlReason::AlreadyExists, detail)
            }
            AccountStoreError::IllegalTransition { .. } => {
                Failure::with_detail(ControlReason::IllegalTransition, detail)
            }
            AccountStoreError::AllocationRevoked { .. } => {
                Failure::with_detail(ControlReason::AllocationRevoked, detail)
            }
            AccountStoreError::SubsidyKey(_) => {
                Failure::with_detail(ControlReason::SubsidyKeyInvalid, detail)
            }
            AccountStoreError::InvalidBinding(_) | AccountStoreError::Credential(_) => {
                Failure::with_detail(ControlReason::InvalidCredential, detail)
            }
            AccountStoreError::Id(_) => Failure::with_detail(ControlReason::InvalidRequest, detail),
            AccountStoreError::Tree(_) => {
                Failure::with_detail(invalid(ControlReason::InvalidAccount), detail)
            }
            AccountStoreError::Directory(_) => {
                Failure::with_detail(invalid(ControlReason::InvalidMembership), detail)
            }
            AccountStoreError::Grant(GrantError::Money(MoneyError::Negative { .. })) => {
                Failure::with_detail(ControlReason::InvalidAmount, detail)
            }
            AccountStoreError::Grant(_) => {
                Failure::with_detail(ControlReason::InvalidAllocation, detail)
            }
            AccountStoreError::Money(ref money) => {
                Failure::with_detail(ControlReason::from(money), detail)
            }
            AccountStoreError::Allocation(ref allocation) => {
                let reason = match allocation {
                    AllocationError::ExceedsGrantor { .. } => {
                        ControlReason::AllocationExceedsGrantor
                    }
                    AllocationError::GrantorHasNoAuthority { .. } => {
                        ControlReason::GrantorHasNoAuthority
                    }
                    AllocationError::DuplicateId(_) => ControlReason::AlreadyExists,
                    AllocationError::DuplicateIdempotencyKey(_) => {
                        ControlReason::IdempotencyConflict
                    }
                    AllocationError::Money(money) => ControlReason::from(money),
                    AllocationError::UnknownAccount(_)
                    | AllocationError::NotParentChild { .. }
                    | AllocationError::UnitMismatch { .. } => ControlReason::InvalidAllocation,
                };
                Failure::with_detail(reason, detail)
            }
            // 授权通过之后找不到东西（竞态之外不会发生）：当作不在范围内，不泄露细节。
            AccountStoreError::NotFound { .. } => Failure::new(ControlReason::OutOfScope),
            AccountStoreError::Sql(_)
            | AccountStoreError::Setup(_)
            | AccountStoreError::Constraint(_)
            | AccountStoreError::Corrupt(_) => Failure::internal(detail),
        }
    }
}

/// 凭证的数据面上限（它在 `virtual_keys` 里的那一行）：隐私等级、角色、租户。签发新凭证时，
/// 新凭证的上限不能超过授权者本人的（设计稿 §9：下发权限不能超过授权者本人）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyCeilings {
    pub owner: String,
    pub tenant_id: Option<String>,
    pub allowed_privacy: Vec<PrivacyClass>,
    pub allowed_roles: Vec<String>,
    pub expires_at_ms: Option<i64>,
}

/// 认出来的调用者，给 `whoami` 用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerInfo {
    pub credential: Credential,
    pub ceilings: KeyCeilings,
}

/// 目标资源的种类（审计里的 `target_kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Account,
    Principal,
    Membership,
    Credential,
    Allocation,
    PairingCode,
    None,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Account => "account",
            TargetKind::Principal => "principal",
            TargetKind::Membership => "membership",
            TargetKind::Credential => "credential",
            TargetKind::Allocation => "allocation",
            TargetKind::PairingCode => "pairing_code",
            TargetKind::None => "none",
        }
    }
}

/// 一条审计记录要写什么（除了结果和原因码，那两个由操作的结局决定）。操作的函数体可以在读到目标之后补充
/// `scope_account` 等字段（所以是 `&mut`）；操作失败时审计行用补到那一步为止的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSpec {
    /// API 层的操作名（`create_account`、`issue_credential`……），`[a-z_]`。
    pub op: &'static str,
    pub target_kind: TargetKind,
    pub target_id: Option<String>,
    /// 做授权判定时用的目标账户。
    pub scope_account: Option<String>,
    /// 第二个相关 id：额度的子账户、成员关系的主体……
    pub related_id: Option<String>,
    pub amount: Option<Money>,
}

impl AuditSpec {
    pub fn new(op: &'static str, target_kind: TargetKind) -> Self {
        Self {
            op,
            target_kind,
            target_id: None,
            scope_account: None,
            related_id: None,
            amount: None,
        }
    }

    pub fn target(mut self, id: impl ToString) -> Self {
        self.target_id = Some(id.to_string());
        self
    }

    pub fn scope(mut self, account: impl ToString) -> Self {
        self.scope_account = Some(account.to_string());
        self
    }

    pub fn related(mut self, id: impl ToString) -> Self {
        self.related_id = Some(id.to_string());
        self
    }

    pub fn amount(mut self, amount: Money) -> Self {
        self.amount = Some(amount);
        self
    }
}

/// 谁的审计行：凭证（id）、配对码（配对记录 id，**不是码**）、本机运维。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Actor {
    Credential(String),
    PairingCode(String),
    Operator(String),
}

impl Actor {
    fn kind(&self) -> &'static str {
        match self {
            Actor::Credential(_) => "credential",
            Actor::PairingCode(_) => "pairing_code",
            Actor::Operator(_) => "operator",
        }
    }

    fn id(&self) -> &str {
        match self {
            Actor::Credential(id) | Actor::PairingCode(id) | Actor::Operator(id) => id,
        }
    }
}

/// 调用者是什么。**授权只对 `Credential` 做判定，`Operator` 免判定，`PairingCode` 没有任何授权**
/// （配对码只能兑换，它的权力来自签发它的凭证，兑换时重新验证）：所以三种身份用枚举区分，
/// 不能用「`credential` 是不是 `None`」这种隐式约定。
pub(super) enum Identity {
    Credential {
        credential: Box<Credential>,
        ceilings: KeyCeilings,
    },
    Operator,
    PairingCode(Box<PairingRow>),
}

/// 认出来的、此刻可用的调用者。
pub(super) struct Caller {
    pub(super) actor: Actor,
    pub(super) identity: Identity,
}

impl Caller {
    pub(super) fn is_operator(&self) -> bool {
        matches!(self.identity, Identity::Operator)
    }

    pub(super) fn credential(&self) -> Option<&Credential> {
        match &self.identity {
            Identity::Credential { credential, .. } => Some(credential),
            _ => None,
        }
    }

    pub(super) fn ceilings(&self) -> Option<&KeyCeilings> {
        match &self.identity {
            Identity::Credential { ceilings, .. } => Some(ceilings),
            _ => None,
        }
    }
}

// ================================================================================================
// 运行器：认证 -> （保存点里）操作 -> 审计 -> 提交
// ================================================================================================

impl AccountStore {
    /// 写操作的统一入口：一个 `BEGIN IMMEDIATE` 事务，里面依次是认证调用者、在保存点里执行 `body`、写审计行、提交。
    /// 见模块文档。
    pub(super) fn control_write<T>(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        mut spec: AuditSpec,
        body: impl FnOnce(&Connection, &Caller, &mut AuditSpec) -> Result<Written<T>, Failure>,
    ) -> Result<Written<T>, Failure> {
        // 先在只读事务里认人：认不出是哪张凭证（未知密钥、格式不对、查无此码）的请求直接返回，**不碰写锁**。
        // 这个库同时是账本，公网上的垃圾请求不能靠「先 BEGIN IMMEDIATE 再认人」和真实流量抢写锁。
        // 这只是挡在前面的一道快速拒绝：认出来的调用者在下面的写事务里**重新**认一遍，撤销依然没有缝。
        {
            let mut conn = self.lock();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
                .map_err(Failure::from)?;
            if let Err((None, failure)) = resolve_caller(&tx, who, meta.now) {
                return Err(failure);
            }
        }
        let mut conn = self.lock();
        let mut tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(Failure::from)?;
        let caller = match resolve_caller(&tx, who, meta.now) {
            Ok(caller) => caller,
            // 认不出是哪张凭证：不审计（见模块文档）。事务随 `tx` 丢弃回滚。
            Err((None, failure)) => return Err(failure),
            // 认出来了但此刻不可用（已撤销、冻结、过期……）：留一行审计，然后拒绝。
            Err((Some(actor), failure)) => {
                write_audit(&tx, &actor, meta, &spec, &Err(failure.clone()))?;
                tx.commit().map_err(Failure::from)?;
                return Err(failure);
            }
        };
        let outcome = {
            let mut savepoint = tx.savepoint().map_err(Failure::from)?;
            match body(&savepoint, &caller, &mut spec) {
                Ok(written) => match savepoint.commit() {
                    Ok(()) => Ok(written),
                    Err(error) => Err(Failure::from(error)),
                },
                Err(failure) => {
                    // 回滚这个操作写了一半的东西；审计行不在保存点里，照常提交。
                    let _ = savepoint.rollback();
                    drop(savepoint);
                    Err(failure)
                }
            }
        };
        let summary = outcome
            .as_ref()
            .map(|written| written.result)
            .map_err(Clone::clone);
        // 审计行写不进去：整个事务不提交（随 `tx` 丢弃回滚），没有审计就没有写操作。
        write_audit(&tx, &caller.actor, meta, &spec, &summary)?;
        tx.commit().map_err(Failure::from)?;
        outcome
    }

    /// 请求体在 HTTP 层就解析失败了（不是 JSON、缺字段、多字段、id 字符集不对……），没有走到任何操作：
    /// 调用者被认出之后，仍然留一行 `rejected / invalid_request` 的审计（操作名由路由决定，没有目标）；
    /// 调用者此刻不可用时留的是它不可用的原因（和别的写操作一致）。认不出的密钥不写审计。
    /// 返回应该回给调用方的失败。
    pub fn control_reject_invalid_request(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        op: &'static str,
    ) -> Failure {
        let spec = AuditSpec::new(op, TargetKind::None);
        match self.control_write::<()>(who, meta, spec, |_, _, _| {
            Err(Failure::new(ControlReason::InvalidRequest))
        }) {
            Err(failure) => failure,
            // body 永远返回 Err；走到这里是代码错误，当作内部错误而不是放行。
            Ok(_) => Failure::internal("an invalid request was accepted"),
        }
    }

    /// 只读操作的统一入口：认证调用者、在一个读事务里执行 `body`。读不写审计（见 `control_reads.rs`）。
    pub(super) fn control_read<T>(
        &self,
        who: &Who<'_>,
        now: TimestampMs,
        body: impl FnOnce(&Connection, &Caller) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .map_err(Failure::from)?;
        let caller = resolve_caller(&tx, who, now).map_err(|(_, failure)| failure)?;
        let value = body(&tx, &caller)?;
        tx.rollback().map_err(Failure::from)?;
        Ok(value)
    }
}

/// 写审计行。写不进去返回 `audit_unavailable`。
pub(super) fn write_audit(
    conn: &Connection,
    actor: &Actor,
    meta: &Meta<'_>,
    spec: &AuditSpec,
    result: &Result<WriteResult, Failure>,
) -> Result<(), Failure> {
    let (outcome, reason) = match result {
        Ok(write) => (write.audit_outcome(), "ok"),
        Err(failure) => (failure.reason.audit_outcome(), failure.reason.as_str()),
    };
    insert_audit(conn, actor, meta, spec, outcome, reason)
        .map_err(|_| Failure::new(ControlReason::AuditUnavailable))
}

pub(super) fn insert_audit(
    conn: &Connection,
    actor: &Actor,
    meta: &Meta<'_>,
    spec: &AuditSpec,
    outcome: AuditOutcome,
    reason: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO control_audit \
            (at_ms, request_id, actor_kind, actor_id, op, target_kind, target_id, scope_account, \
             related_id, amount_unit, amount_micros, outcome, reason_code) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            meta.now.0,
            meta.request_id,
            actor.kind(),
            actor.id(),
            spec.op,
            spec.target_kind.as_str(),
            spec.target_id,
            spec.scope_account,
            spec.related_id,
            spec.amount.as_ref().map(|money| money.unit.to_string()),
            spec.amount.as_ref().map(|money| money.micros),
            outcome.as_str(),
            reason,
        ],
    )?;
    Ok(())
}

// ================================================================================================
// 认证：密钥哈希 -> 凭证
// ================================================================================================

/// `virtual_keys` 里的一行（只取控制面要用的列）。
pub(super) struct KeyRow {
    pub(super) key_id: String,
    pub(super) active: bool,
    pub(super) ceilings: KeyCeilings,
}

pub(super) fn load_key_by_hash(
    conn: &Connection,
    hash: &VirtualKeyHash,
) -> Result<Option<KeyRow>, AccountStoreError> {
    load_key(conn, "key_hash", &hash.as_bytes()[..])
}

/// 按凭证 id（= `virtual_keys.key_id`）取验证器那一行。配对码兑换时要重新验证签发者的验证器用它。
pub(super) fn load_key_by_id(
    conn: &Connection,
    key_id: &str,
) -> Result<Option<KeyRow>, AccountStoreError> {
    load_key(conn, "key_id", key_id)
}

fn load_key(
    conn: &Connection,
    column: &'static str,
    value: impl rusqlite::ToSql,
) -> Result<Option<KeyRow>, AccountStoreError> {
    type Raw = (
        String,
        String,
        Option<String>,
        String,
        String,
        Option<i64>,
        String,
    );
    let raw: Option<Raw> = conn
        .query_row(
            &format!(
                "SELECT key_id, owner, tenant_id, allowed_privacy, allowed_roles, expires_at_ms, status \
                 FROM virtual_keys WHERE {column} = ?1"
            ),
            [value],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((key_id, owner, tenant_id, privacy, roles, expires_at_ms, status)) = raw else {
        return Ok(None);
    };
    let corrupt = |what: &str| AccountStoreError::Corrupt(format!("virtual key {key_id}: {what}"));
    let allowed_privacy = serde_json::from_str(&privacy)
        .map_err(|_| corrupt("allowed_privacy is not a list of privacy classes"))?;
    let allowed_roles = serde_json::from_str(&roles)
        .map_err(|_| corrupt("allowed_roles is not a list of strings"))?;
    Ok(Some(KeyRow {
        active: status == "active",
        ceilings: KeyCeilings {
            owner,
            tenant_id,
            allowed_privacy,
            allowed_roles,
            expires_at_ms,
        },
        key_id,
    }))
}

/// 认出调用者。失败时返回（认出来的是谁，原因）：认不出是哪张凭证时第一项是 `None`，调用方不写审计。
pub(super) fn resolve_caller(
    conn: &Connection,
    who: &Who<'_>,
    now: TimestampMs,
) -> Result<Caller, (Option<Actor>, Failure)> {
    let hash = match who {
        Who::Operator(label) => {
            return Ok(Caller {
                actor: Actor::Operator((*label).to_string()),
                identity: Identity::Operator,
            });
        }
        Who::PairingCode(code_hash) => return resolve_pairing_caller(conn, code_hash, now),
        Who::Secret(hash) => hash,
    };
    let key = match load_key_by_hash(conn, hash) {
        Ok(Some(key)) => key,
        Ok(None) => return Err((None, Failure::new(ControlReason::Unauthenticated))),
        Err(error) => return Err((None, Failure::from(error))),
    };
    let actor = Actor::Credential(key.key_id.clone());
    let deny = |reason: ControlReason| Err((Some(actor.clone()), Failure::new(reason)));
    if !key.active {
        return deny(ControlReason::CredentialRevoked);
    }
    if key
        .ceilings
        .expires_at_ms
        .is_some_and(|expires| expires <= now.0)
    {
        return deny(ControlReason::CredentialExpired);
    }
    let Ok(credential_id) = CredentialId::new(key.key_id.clone()) else {
        return deny(ControlReason::NotAControlCredential);
    };
    let credential = match load_credential(conn, &credential_id) {
        Ok(Some(credential)) => credential,
        Ok(None) => return deny(ControlReason::NotAControlCredential),
        Err(error) => return Err((Some(actor), Failure::from(error))),
    };
    if let Err(error) = credential.check_usable_at(now) {
        return deny(ControlReason::from(&error));
    }
    Ok(Caller {
        actor,
        identity: Identity::Credential {
            credential: Box::new(credential),
            ceilings: key.ceilings,
        },
    })
}

// ================================================================================================
// 授权：一小片目录 + S0 的 `authorize`
// ================================================================================================

/// 取一小片目录：各个账户的祖先链，再加上给定的成员关系（连同它们的主体、组织链、账户链）。
/// 所有链都止于同一个平台根，所以合起来是一棵合法的树。
pub(super) fn slice_directory(
    conn: &Connection,
    anchors: &[&AccountId],
    memberships: &[Membership],
    principals: Vec<Principal>,
) -> Result<Directory, Failure> {
    let mut accounts: BTreeMap<AccountId, Account> = BTreeMap::new();
    for anchor in anchors {
        // 不存在的账户得到空链：不加进树里，`authorize` 会把它判成 `out_of_scope`。
        merge_chains(&mut accounts, load_ancestors(conn, anchor)?);
    }
    for membership in memberships {
        merge_chains(&mut accounts, load_ancestors(conn, &membership.org)?);
        if let Some(account) = &membership.account {
            merge_chains(&mut accounts, load_ancestors(conn, account)?);
        }
    }
    if accounts.is_empty() {
        merge_chains(&mut accounts, load_platform_roots(conn)?);
    }
    if accounts.is_empty() {
        return Err(Failure::internal("no accounts exist yet"));
    }
    let tree = AccountTree::build(accounts.into_values().collect())
        .map_err(|error| Failure::internal(format!("account slice is not a tree: {error}")))?;
    Directory::build(tree, principals, memberships.to_vec())
        .map_err(|error| Failure::internal(format!("directory slice is inconsistent: {error}")))
}

/// 绑定对应的账户：`Service` / `Node` 是绑定的账户，`Member` 是成员关系的付款账户（成员关系不存在是 `None`）。
pub(super) fn binding_anchor(
    conn: &Connection,
    binding: &CredentialBinding,
) -> Result<Option<AccountId>, AccountStoreError> {
    match binding {
        CredentialBinding::Service { account } | CredentialBinding::Node { account } => {
            Ok(Some(account.clone()))
        }
        CredentialBinding::Member { membership } => {
            Ok(load_membership(conn, membership)?.and_then(|found| found.account))
        }
    }
}

/// 授权：调用者能不能对 `target` 做 `op`（`grant_allocation` 带金额）。本机运维直接放行。
/// 返回用来判定的那片目录（运维返回 `None`），调用方还要做别的检查（比如 `would_disable_caller`）时复用。
pub(super) fn authorize_op(
    conn: &Connection,
    caller: &Caller,
    now: TimestampMs,
    op: ControlOp,
    target: &AccountId,
    amount: Option<&Money>,
) -> Result<Option<Directory>, Failure> {
    let credential = match &caller.identity {
        Identity::Operator => return Ok(None),
        // 配对码没有任何授权：它只能兑换（兑换时按签发它的凭证重新授权）。
        Identity::PairingCode(_) => return Err(Failure::new(ControlReason::OpNotGranted)),
        Identity::Credential { credential, .. } => credential,
    };
    let anchor = binding_anchor(conn, credential.binding())?;
    let mut anchors: Vec<&AccountId> = vec![target];
    if let Some(anchor) = &anchor {
        anchors.push(anchor);
    }
    let dir = slice_directory(conn, &anchors, &[], Vec::new())?;
    authorize(&dir, credential, now, op, target, amount).map_err(Failure::new)?;
    Ok(Some(dir))
}

/// 与操作**目标**无关的那几步授权判定：凭证此刻可用、绑定账户没被冻结、调用者有没有 `op` 这个操作（任何范围里）。
///
/// 按 id 查的写操作（撤销成员关系 / 额度、改凭证状态、签发成员凭证、创建 / 撤销配对码）在碰目标之前先做：没有这个操作权限的调用者，无论目标存在与否
/// 都得到同一个答案（契约 `authorize` 的检查顺序：有没有这个操作的权限先于目标存不存在）。否则已有的目标得到
/// `op_not_granted`，不存在的目标在授权判定之前就得到 `out_of_scope`，靠它能探测别的组织有哪些 id。
/// 范围问题（`op` 的子树不含自己的绑定账户）不在这里判，交给后面按目标判。本机运维不受授权约束。
pub(super) fn require_op_capability(
    conn: &Connection,
    caller: &Caller,
    now: TimestampMs,
    op: ControlOp,
) -> Result<(), Failure> {
    let Identity::Credential { credential, .. } = &caller.identity else {
        // 运维不受约束；配对码没有任何授权（按目标判时同样得到 `op_not_granted`）
        return match &caller.identity {
            Identity::PairingCode(_) => Err(Failure::new(ControlReason::OpNotGranted)),
            _ => Ok(()),
        };
    };
    let Some(anchor) = binding_anchor(conn, credential.binding())? else {
        return Ok(());
    };
    match authorize_op(conn, caller, now, op, &anchor, None) {
        // 范围、金额都是按目标 / 按请求判的，不在这里
        Err(failure)
            if matches!(
                failure.reason,
                ControlReason::OutOfScope | ControlReason::AmountExceedsAuthority
            ) =>
        {
            Ok(())
        }
        Err(failure) => Err(failure),
        Ok(_) => Ok(()),
    }
}

// ================================================================================================
// 主体
// ================================================================================================

/// 最早登记这个主体的组织（没有登记记录——存储层直接建的老主体——是 `None`）。
pub(super) fn registrant_of(
    conn: &Connection,
    principal: &PrincipalId,
) -> Result<Option<AccountId>, Failure> {
    let registrant: Option<String> = conn
        .query_row(
            "SELECT org_id FROM principal_registrations WHERE principal_id = ?1",
            [principal.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    registrant
        .map(|org| {
            AccountId::new(org).map_err(|error| Failure::from(AccountStoreError::from(error)))
        })
        .transpose()
}

/// 主体的一个归属方：谁有资格对它的足迹负责。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrincipalOwner {
    /// 归属方所在的组织：新的成员关系就在它的子树里（它是新成员关系所属组织的祖先或自己）时，它自己的管理员仍然管得到。
    pub(super) org: AccountId,
    /// 判定「调用者管不管得到归属方」用的资源范围：登记它的组织，或者已有成员关系的付款账户（管理员关系用组织）。
    pub(super) target: AccountId,
}

/// 一个主体已有的归属方。
///
/// * **有登记记录**的主体：登记它的组织（`principal_registrations`，只追加，之后不会变）；
/// * **没有登记记录**的老主体（存储层直接建的，控制面出现之前的数据）：没有别的可信线索，所以**已经在用它的组织就是它的
///   归属方**——它已有的每个成员关系（不管现在有没有被撤销：和 `control_revoke_principal` 检查的范围一致）所在的组织，
///   授权看那个成员关系的付款账户。一个老主体还没有任何成员关系，就没有归属方（第一个挂上去的组织成为它的归属方）。
///
/// 用它来回答「把这个主体关联到别的组织需要谁的授权」：谁能让主体的足迹扩大，谁就该能把它整体撤销，
/// 否则任何组织猜到一个主体 id 就能挂一个成员关系上去，让原来的归属方再也撤销不了它
/// （Codex r1 #1 / PR-Daemon S2 F3 / PR #600 第二轮 S2 F3：老主体曾经被整个跳过）。
pub(super) fn principal_owners(
    conn: &Connection,
    principal: &PrincipalId,
) -> Result<Vec<PrincipalOwner>, Failure> {
    if let Some(registrant) = registrant_of(conn, principal)? {
        return Ok(vec![PrincipalOwner {
            org: registrant.clone(),
            target: registrant,
        }]);
    }
    let mut owners: Vec<PrincipalOwner> = Vec::new();
    for membership in load_memberships_of(conn, principal)? {
        let owner = PrincipalOwner {
            org: membership.org.clone(),
            target: membership_target(&membership).clone(),
        };
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    Ok(owners)
}

/// 登记主体。`org` 是以谁的名义登记（授权的资源范围）；同时记下最早登记它的组织。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePrincipal {
    pub principal: PrincipalId,
    pub org: AccountId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokePrincipal {
    pub principal: PrincipalId,
    pub org: AccountId,
}

impl AccountStore {
    /// 登记主体（幂等：已经存在就原样返回，已撤销的主体不会被「再次登记」复活）。
    pub fn control_create_principal(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &CreatePrincipal,
    ) -> Result<Written<Principal>, Failure> {
        let spec = AuditSpec::new("create_principal", TargetKind::Principal)
            .target(&request.principal)
            .scope(&request.org);
        self.control_write(who, meta, spec, |conn, caller, _spec| {
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::CreatePrincipal,
                &request.org,
                None,
            )?;
            if let Some(existing) = load_principal(conn, &request.principal)? {
                // 已经有这个主体了。主体 id 是全局的假名，只有登记它的组织（或对登记组织有授权的调用者）能看到它——
                // 别的组织重复登记同一个 id，得到的是固定的「已存在」，**不带**主体、状态或登记人：
                // 不能靠重复登记去探测别的组织的主体有没有被撤销（PR-Daemon S2 F3）。
                // 没有登记记录的老主体（存储层直接建的）只有本机运维能「再次登记」。
                let visible = match registrant_of(conn, &request.principal)? {
                    Some(registrant) => {
                        registrant == request.org
                            || authority_is_visible(authorize_op(
                                conn,
                                caller,
                                meta.now,
                                ControlOp::CreatePrincipal,
                                &registrant,
                                None,
                            ))?
                    }
                    None => caller.is_operator(),
                };
                if !visible {
                    return Err(Failure::new(ControlReason::AlreadyExists));
                }
                return Ok(Written {
                    result: WriteResult::Replayed,
                    value: existing,
                });
            }
            let principal = ensure_principal_in(conn, &request.principal, meta.now)?;
            conn.execute(
                "INSERT INTO principal_registrations (principal_id, org_id, registered_at_ms) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![request.principal.as_str(), request.org.as_str(), meta.now.0],
            )?;
            Ok(Written {
                result: WriteResult::Created,
                value: principal,
            })
        })
    }

    /// 撤销主体（同意撤回，设计稿 §7：撤销后相关凭证和成员立即失效）。
    ///
    /// 范围规则：调用者必须对**登记它的组织**和**该主体每一个成员关系所在的账户**都有 `revoke_principal` 权限
    /// （一个组织不能撤销别的组织也在用的主体）。登记组织**永远**要检查，不管主体现在有没有成员关系：
    /// 别的组织（或旧数据）给主体加一个自己的成员关系，不能靠它绕开登记组织的授权（Codex r1 #1）。
    /// 没有登记记录的老主体（存储层直接建的）：没有成员关系时只有本机运维能撤销，有成员关系时看成员关系。
    pub fn control_revoke_principal(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &RevokePrincipal,
    ) -> Result<Written<Principal>, Failure> {
        let spec = AuditSpec::new("revoke_principal", TargetKind::Principal)
            .target(&request.principal)
            .scope(&request.org);
        self.control_write(who, meta, spec, |conn, caller, _spec| {
            let out_of_scope = || Failure::new(ControlReason::OutOfScope);
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::RevokePrincipal,
                &request.org,
                None,
            )?;
            let Some(principal) = load_principal(conn, &request.principal)? else {
                return Err(out_of_scope());
            };
            let memberships = load_memberships_of(conn, &principal.id)?;
            match registrant_of(conn, &principal.id)? {
                // 登记它的组织永远要检查（见上面的范围规则）
                Some(registrant) => {
                    authorize_op(
                        conn,
                        caller,
                        meta.now,
                        ControlOp::RevokePrincipal,
                        &registrant,
                        None,
                    )?;
                }
                // 没人登记过、也没有任何成员关系：不是任何组织的主体，只有本机运维能动它。
                None if memberships.is_empty() && !caller.is_operator() => {
                    return Err(out_of_scope());
                }
                None => {}
            }
            for membership in &memberships {
                let target = membership_target(membership);
                authorize_op(
                    conn,
                    caller,
                    meta.now,
                    ControlOp::RevokePrincipal,
                    target,
                    None,
                )?;
            }
            if principal.status == PrincipalStatus::Revoked {
                return Ok(Written {
                    result: WriteResult::Unchanged,
                    value: principal,
                });
            }
            revoke_principal_in(conn, &principal.id, meta.now)?;
            Ok(Written {
                result: WriteResult::Updated,
                value: Principal {
                    id: principal.id,
                    status: PrincipalStatus::Revoked,
                },
            })
        })
    }
}

// ================================================================================================
// 账户
// ================================================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateAccount {
    /// 新账户（`status` 必须是 `Active`：新账户不能一出生就是冻结或关闭的）。
    pub account: Account,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAccountStatus {
    pub account: AccountId,
    pub status: AccountStatus,
}

impl AccountStore {
    /// 建账户。授权看**父账户**；同一个 id 已经存在且内容相同是 `replayed`，内容不同是 `already_exists`。
    pub fn control_create_account(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &CreateAccount,
    ) -> Result<Written<Account>, Failure> {
        let account = &request.account;
        let mut spec = AuditSpec::new("create_account", TargetKind::Account).target(&account.id);
        if let Some(parent) = &account.parent {
            spec = spec.scope(parent);
        }
        self.control_write(who, meta, spec, |conn, caller, _spec| {
            let Some(parent) = &account.parent else {
                // 没有父账户 = 平台根。HTTP 调用者没有任何办法建它（没有凭证能绑平台）：只有本机运维能。
                if !caller.is_operator() {
                    return Err(Failure::new(ControlReason::OutOfScope));
                }
                return create_account_row(conn, account, meta.now);
            };
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::CreateAccount,
                parent,
                None,
            )?;
            create_account_row(conn, account, meta.now)
        })
    }

    /// 冻结 / 解冻 / 关闭账户（冻结和关闭连带整个子树）。授权看账户自己；不能动调用者自己绑定账户及其祖先。
    pub fn control_set_account_status(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &SetAccountStatus,
    ) -> Result<Written<Account>, Failure> {
        let spec = AuditSpec::new("set_account_status", TargetKind::Account)
            .target(&request.account)
            .scope(&request.account);
        self.control_write(who, meta, spec, |conn, caller, _spec| {
            let dir = authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::SetAccountStatus,
                &request.account,
                None,
            )?;
            if let (Some(dir), Some(credential)) = (&dir, caller.credential())
                && would_disable_caller(dir, credential, &request.account)
            {
                return Err(Failure::new(ControlReason::WouldDisableCaller));
            }
            let Some(current) = load_account(conn, &request.account)? else {
                return Err(Failure::new(ControlReason::OutOfScope));
            };
            if current.status == request.status {
                return Ok(Written {
                    result: WriteResult::Unchanged,
                    value: current,
                });
            }
            set_account_status_in(conn, &request.account, request.status)
                .map_err(|error| Failure::from_store(error, Ctx::Account))?;
            Ok(Written {
                result: WriteResult::Updated,
                value: Account {
                    status: request.status,
                    ..current
                },
            })
        })
    }
}

fn create_account_row(
    conn: &Connection,
    account: &Account,
    now: TimestampMs,
) -> Result<Written<Account>, Failure> {
    if account.status != AccountStatus::Active {
        return Err(Failure::with_detail(
            ControlReason::InvalidAccount,
            "a new account must be active",
        ));
    }
    if let Some(existing) = load_account(conn, &account.id)? {
        return if existing.kind == account.kind
            && existing.parent == account.parent
            && existing.unit == account.unit
        {
            Ok(Written {
                result: WriteResult::Replayed,
                value: existing,
            })
        } else {
            Err(Failure::with_detail(
                ControlReason::AlreadyExists,
                format!(
                    "account {} already exists with a different definition",
                    account.id
                ),
            ))
        };
    }
    // 不在已经冻结或关闭的账户下面建新账户：它从出生起就不可用，只会制造困惑。
    if let Some(parent) = &account.parent
        && load_account(conn, parent)?.is_some_and(|found| found.status != AccountStatus::Active)
    {
        return Err(Failure::with_detail(
            ControlReason::InvalidAccount,
            "the parent account is frozen or closed",
        ));
    }
    create_account_in(conn, account, now)
        .map_err(|error| Failure::from_store(error, Ctx::Account))?;
    Ok(Written {
        result: WriteResult::Created,
        value: account.clone(),
    })
}

// ================================================================================================
// 成员关系
// ================================================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMembership {
    /// 新成员关系（`status` 必须是 `Active`）。
    pub membership: Membership,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeMembership {
    pub membership: MembershipId,
}

/// 成员关系的资源范围：它的付款账户；管理员关系没有付款账户，用所属组织。
pub(super) fn membership_target(membership: &Membership) -> &AccountId {
    membership.account.as_ref().unwrap_or(&membership.org)
}

/// 授权判定的结果折成「调用者有没有资格看到」。
///
/// 授权类的失败（[`ControlReason::is_denial`]：没有这个操作的权限、不在范围内、凭证 / 账户状态不可用……）一律是「没有」，
/// 调用方回无细节的 `out_of_scope`：内部授权判定用的是哪个原因码（`op_not_granted` 还是 `out_of_scope`）取决于调用者自己有哪些
/// 权限，原样透传就等于告诉调用者「这里有一个你看不见的主体」。数据库忙、内部错误这类基础设施问题**不是**「看不见」，
/// 原样往上走，让调用方重试 / 告警，不能被悄悄当成「主体不在范围内」。
pub(super) fn authority_is_visible<T>(result: Result<T, Failure>) -> Result<bool, Failure> {
    match result {
        Ok(_) => Ok(true),
        Err(failure) if failure.reason.is_denial() => Ok(false),
        Err(failure) => Err(failure),
    }
}

/// 成员关系 `id`，读的时候限定在调用者对 `manage_membership` 管得到的范围里：管不到的和不存在的一样返回 `None`
/// （一条语句，见 [`load_membership_within`]）。本机运维管得到全部；凭证管得到的范围是它身上 `manage_membership` 权限的子树
/// （带额度上限的权限不算：契约的 `scope_covers` 对没有额度的请求不认带上限的授权）；配对码身份没有任何授权。
///
/// 管得到的成员关系，行为和以前的「读出来再授权」完全一样；管不到的不再被读出来，所以后面没有已有 id 独有的授权查询
/// （issue #603 N7 残余，Codex 预审 H1 / M3）。
fn load_manageable_membership(
    conn: &Connection,
    caller: &Caller,
    id: &MembershipId,
) -> Result<Option<Membership>, Failure> {
    let roots: Vec<AccountId> = match &caller.identity {
        Identity::Operator => return Ok(load_membership(conn, id)?),
        Identity::Credential { credential, .. } => credential
            .admin_scopes()
            .iter()
            .filter(|scope| scope.op == ControlOp::ManageMembership && scope.max_amount.is_none())
            .map(|scope| scope.subtree.clone())
            .collect(),
        Identity::PairingCode(_) => Vec::new(),
    };
    Ok(load_membership_within(conn, id, &roots)?)
}

/// 调用者有没有资格知道这个主体现在是什么状态。
///
/// 没有资格时，活跃、已撤销、不存在必须是同一个 `out_of_scope`（细节为空）。状态如果在归属方授权之前就返回，
/// 外部组织就能靠原因码区分这三种情况（r1-M2）。本机运维始终看得到。没有归属方的活跃老主体仍允许第一个挂上去
/// （挂上去本身就会成功，不靠失败原因泄露状态）；没有归属方的已撤销主体和根本不存在的主体同一个答案。
fn caller_may_know_principal(
    conn: &Connection,
    caller: &Caller,
    now: TimestampMs,
    principal: Option<&Principal>,
    owners: &[PrincipalOwner],
    membership_org: &AccountId,
) -> Result<bool, Failure> {
    if caller.is_operator() {
        return Ok(true);
    }
    if owners.is_empty() {
        return Ok(matches!(
            principal,
            Some(found) if found.status == PrincipalStatus::Active
        ));
    }
    // `membership_org` 在这里已经被 `check_membership_structure` 验过：它确实是付款账户的祖先（或管理员关系自己的组织），
    // 所以「归属方就在新成员关系所属组织的上面」是一个可信的结论，不是客户端随手填的 `org` 说了算。
    let ancestors = load_ancestors(conn, membership_org)?;
    for owner in owners {
        let inside = ancestors.iter().any(|account| account.id == owner.org);
        // 授权失败（没有 `revoke_principal` 这个操作、归属方不在范围里……）一律是「看不见」，不把内部判定的原因码透出去
        if !inside
            && !authority_is_visible(authorize_op(
                conn,
                caller,
                now,
                ControlOp::RevokePrincipal,
                &owner.target,
                None,
            ))?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 成员关系自己声明的结构：`org` 必须是付款账户的祖先（管理员关系没有付款账户，`org` 就是授权的那个组织），
/// 角色和账户种类对得上。同一套规则 S0 的 `Directory::build` 在 `create_membership_in` 里会再校验一遍，
/// 这里**提前到任何主体状态的判断之前**。
///
/// 为什么要提前：`org` 是客户端提交的，授权只看付款账户。外部组织 B 的管理员提交 `account = B 自己的账户、org = A、
/// principal = A 登记的主体`：授权通过（付款账户是 B 的），归属检查却会相信 `org = A`，把「归属方 A 就是新成员关系所属组织」
/// 当成不需要对 A 授权，随后才报主体状态——活跃、已撤销、不存在得到三种不同的失败（PR #600 第四轮复审新问题 2）。
/// 结构错只取决于请求里的账户、组织和库里的账户树，和主体无关（这里用一个占位主体跑目录校验，主体是否登记、是否撤销一概不看），
/// 所以三种主体得到逐字节相同的失败。
///
/// 组织不存在、不是组织、账户不在它下面是同一个失败、同一句话（不带任何 id）：不泄露别的租户有没有这个账户
/// （和授权判定里「不存在和不在范围内是同一个 `out_of_scope`」一个规矩）。
fn check_membership_structure(conn: &Connection, membership: &Membership) -> Result<(), Failure> {
    let not_inside = || {
        Failure::with_detail(
            ControlReason::InvalidMembership,
            "the membership's account is not inside its org",
        )
    };
    let mut accounts: BTreeMap<AccountId, Account> = BTreeMap::new();
    merge_chains(&mut accounts, load_ancestors(conn, &membership.org)?);
    if !accounts.contains_key(&membership.org) {
        return Err(not_inside());
    }
    if let Some(account) = &membership.account {
        merge_chains(&mut accounts, load_ancestors(conn, account)?);
    }
    let tree = AccountTree::build(accounts.into_values().collect())
        .map_err(|error| Failure::from_store(error.into(), Ctx::Membership))?;
    let placeholder = Principal {
        id: membership.principal.clone(),
        status: PrincipalStatus::Active,
    };
    match Directory::build(tree, vec![placeholder], vec![membership.clone()]) {
        Ok(_) => Ok(()),
        Err(
            DirectoryError::UnknownOrg { .. }
            | DirectoryError::NotAnOrg { .. }
            | DirectoryError::AccountNotUnderOrg { .. },
        ) => Err(not_inside()),
        Err(other) => Err(Failure::from_store(other.into(), Ctx::Membership)),
    }
}

impl AccountStore {
    /// 建成员关系。授权看成员关系的付款账户（管理员关系看组织）。
    ///
    /// 顺序：授权（付款账户）-> 请求自己声明的结构（`org` 必须是付款账户的祖先，见 [`check_membership_structure`]）-> 成员关系 id 的重放 /
    /// 冲突（调用者管不到的成员关系对调用者按「不存在」走，最后才以 `out_of_scope` 拒绝）-> 主体。主体状态（没登记、已撤销）只告诉**有资格知道的人**：本机运维，或者这个主体的归属方（登记组织，或老主体已经在用它的
    /// 组织）自己的管理员。其他人——包括调用者对目标账户有 `manage_membership`、但对归属方没有 `revoke_principal` 的外部组织，
    /// 和伪造 `org` 字段的外部组织——对活跃、已撤销、不存在拿到同一个失败（结构错是固定的 `invalid_membership`，可见性授权失败是
    /// 无细节的 `out_of_scope`）。没有归属方的活跃老主体仍可以由第一个组织挂上去。
    pub fn control_create_membership(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &CreateMembership,
    ) -> Result<Written<Membership>, Failure> {
        let membership = &request.membership;
        let target = membership_target(membership).clone();
        let spec = AuditSpec::new("create_membership", TargetKind::Membership)
            .target(&membership.id)
            .scope(&target)
            .related(&membership.principal);
        self.control_write(who, meta, spec, |conn, caller, _spec| {
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::ManageMembership,
                &target,
                None,
            )?;
            if membership.status != MembershipStatus::Active {
                return Err(Failure::with_detail(
                    ControlReason::InvalidMembership,
                    "a new membership must be active",
                ));
            }
            // 先验请求自己声明的结构（`org` 与付款账户的关系），再碰别人的数据（成员关系 id、主体）：见 `check_membership_structure`
            check_membership_structure(conn, membership)?;
            // 先判「调用者是不是归属方」（可见性），再谈成员关系 id 是否已存在、主体状态。顺序反了的话，外部组织能靠
            // `already_exists` / `invalid_membership` / `principal_revoked` / `out_of_scope` 区分不存在、已撤销、活跃（r1-M2），
            // 也能拿一个别的组织的成员关系 id 来探测它存在与否（issue #603 N7）。
            // 归属方 = 登记它的组织；没有登记记录的老主体 = 已经在用它的组织（见 `principal_owners`）。
            // 把已有归属方的主体关联到别的组织，每个归属方要么就是这个成员关系所属组织的祖先（或它自己），
            // 要么调用者对它有 `revoke_principal` 权限。否则任何组织猜到一个主体 id 就能挂一个成员关系上去
            // （Codex r1 #1 / PR-Daemon S2 F3 / PR #600 第二轮 S2 F3）。
            let found = load_principal(conn, &membership.principal)?;
            let owners = principal_owners(conn, &membership.principal)?;
            if !caller_may_know_principal(
                conn,
                caller,
                meta.now,
                found.as_ref(),
                &owners,
                &membership.org,
            )? {
                return Err(Failure::new(ControlReason::OutOfScope));
            }
            // 已有的成员关系读的时候限定在调用者管得到的范围里（一条语句里按子树过滤，见 `load_manageable_membership`）：管不到的和
            // 不存在的一样得到空结果，这个 id 对调用者按「不存在」走，既不泄露它存在，也不让「读到之后再去做授权」多出一串已有 id
            // 独有的查询（数据库工作量和响应时间都会把两种 id 区分开，Codex 预审 M3）。replayed / already_exists 留给管得到它的人。
            if let Some(existing) = load_manageable_membership(conn, caller, &membership.id)? {
                return if existing == *membership {
                    Ok(Written {
                        result: WriteResult::Replayed,
                        value: existing,
                    })
                } else {
                    Err(Failure::with_detail(
                        ControlReason::AlreadyExists,
                        format!(
                            "membership {} already exists with a different definition",
                            membership.id
                        ),
                    ))
                };
            }
            match found {
                None => {
                    return Err(Failure::with_detail(
                        ControlReason::InvalidMembership,
                        format!("principal {} is not registered", membership.principal),
                    ));
                }
                Some(principal) if principal.status == PrincipalStatus::Revoked => {
                    return Err(Failure::with_detail(
                        ControlReason::PrincipalRevoked,
                        format!("principal {} has been revoked", membership.principal),
                    ));
                }
                Some(_) => {}
            }
            // 新成员关系的结构与付款账户占用校验：调用者管不到的已有 id 和不存在的 id 一起先过这一关，两者的失败和数据库执行的
            // 语句都一样。否则付款账户已被占用时，不存在的 id 走到 `AccountAlreadyBound`（`invalid_membership`），别的组织已有的
            // id 却是 `out_of_scope`，调用者借一个被占用的付款账户就能探测别的组织有哪些成员关系 id（Codex 预审 H1）。
            check_new_membership_in(conn, membership)
                .map_err(|error| Failure::from_store(error, Ctx::Membership))?;
            // 到这里才区分：id 已经存在（只可能是调用者管不到的，管得到的上面已经处理）就以同样的 `out_of_scope` 拒绝，
            // 不存在才创建。创建成功与否本身仍可探测（成员关系 id 是客户端自选的全局命名空间），这个残余消除不了，见设计稿 S2-10。
            if load_membership(conn, &membership.id)?.is_some() {
                return Err(Failure::new(ControlReason::OutOfScope));
            }
            create_membership_in(conn, membership, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Membership))?;
            Ok(Written {
                result: WriteResult::Created,
                value: membership.clone(),
            })
        })
    }

    /// 撤销成员关系（撤销不可逆）。授权看成员关系的付款账户。撤销后，这个成员关系的所有凭证在数据面立即被拒
    /// （数据面认证会沿成员关系检查，见 `credential_gate`），不需要逐张去撤销。
    pub fn control_revoke_membership(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &RevokeMembership,
    ) -> Result<Written<Membership>, Failure> {
        let spec =
            AuditSpec::new("revoke_membership", TargetKind::Membership).target(&request.membership);
        self.control_write(who, meta, spec, |conn, caller, spec| {
            // 先做与目标无关的授权判定，再按 id 去查（见 `require_op_capability`）
            require_op_capability(conn, caller, meta.now, ControlOp::ManageMembership)?;
            let Some(found) = load_membership(conn, &request.membership)? else {
                return Err(Failure::new(ControlReason::OutOfScope));
            };
            spec.scope_account = Some(membership_target(&found).to_string());
            spec.related_id = Some(found.principal.to_string());
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::ManageMembership,
                membership_target(&found),
                None,
            )?;
            if found.status == MembershipStatus::Revoked {
                return Ok(Written {
                    result: WriteResult::Unchanged,
                    value: found,
                });
            }
            revoke_membership_in(conn, &found.id, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Membership))?;
            Ok(Written {
                result: WriteResult::Updated,
                value: Membership {
                    status: MembershipStatus::Revoked,
                    ..found
                },
            })
        })
    }
}

// ================================================================================================
// 凭证
// ================================================================================================

/// 签发凭证。密钥由调用方（HTTP 层 / 运维命令）用 `MintedVirtualKey::mint` 生成：存储层只拿到 id 和哈希，
/// 明文只在响应里出现一次。
#[derive(Debug, Clone)]
pub struct IssueCredential {
    pub key_id: String,
    pub key_hash: VirtualKeyHash,
    pub binding: CredentialBinding,
    pub admin_scopes: Vec<AdminScope>,
    pub expires_at: Option<TimestampMs>,
    /// `virtual_keys.owner`：谁用这张凭证的显示名，不进审计。
    pub owner: String,
    /// `None` = 继承授权者的租户绑定。
    pub tenant_id: Option<String>,
    pub allowed_privacy: Vec<PrivacyClass>,
    pub allowed_roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCredentialStatus {
    pub credential: CredentialId,
    pub status: CredentialStatus,
}

/// 新凭证的数据面上限不能超过授权者本人；授权者没有的租户绑定也不能凭空给出（设计稿 §9）。
/// `tenant` 是继承之后的结果（请求没写就是授权者自己的）。
pub(super) fn check_ceilings(
    issuer: &KeyCeilings,
    privacy: &[PrivacyClass],
    roles: &[String],
    tenant: &Option<String>,
) -> Result<(), Failure> {
    let exceeds = || {
        Failure::with_detail(
            ControlReason::CeilingExceedsGrantor,
            "the new credential's data-plane ceilings must be within the issuer's",
        )
    };
    if privacy
        .iter()
        .any(|class| !issuer.allowed_privacy.contains(class))
    {
        return Err(exceeds());
    }
    if roles
        .iter()
        .any(|role| !issuer.allowed_roles.contains(role))
    {
        return Err(exceeds());
    }
    // 租户：授权者有绑定，新凭证只能是同一个；授权者没有，新凭证也不能有。
    if issuer.tenant_id != *tenant {
        return Err(exceeds());
    }
    Ok(())
}

/// 新凭证不能比授权者活得久：授权者有有效期时，新凭证必须有、且不晚于它。
pub(super) fn check_expiry(
    issuer: &Credential,
    requested: Option<TimestampMs>,
) -> Result<(), Failure> {
    match (issuer.expires_at(), requested) {
        (None, _) => Ok(()),
        (Some(cap), Some(wanted)) if wanted <= cap => Ok(()),
        (Some(_), _) => Err(Failure::with_detail(
            ControlReason::ExpiryExceedsGrantor,
            "the new credential must expire no later than its issuer",
        )),
    }
}

/// 数据面验证器的内容（一行 `virtual_keys`）。
pub(super) struct NewKey<'a> {
    pub(super) key_id: &'a str,
    pub(super) hash: &'a VirtualKeyHash,
    pub(super) owner: &'a str,
    pub(super) tenant_id: Option<&'a str>,
    pub(super) privacy: &'a [PrivacyClass],
    pub(super) roles: &'a [String],
    pub(super) expires_at: Option<TimestampMs>,
}

/// 写一行 `virtual_keys`（和 `VirtualKeyStore::insert_active` 同一张表、同一个形状），明文密钥永远不在这里。
pub(super) fn insert_virtual_key(conn: &Connection, key: &NewKey<'_>) -> Result<(), Failure> {
    let plain_text = |text: &str| {
        !text.is_empty()
            && text.len() <= 128
            && text.trim() == text
            && !text.chars().any(char::is_control)
    };
    if !plain_text(key.owner)
        || key.tenant_id.is_some_and(|tenant| !plain_text(tenant))
        || key.privacy.is_empty()
        || key.roles.iter().any(|role| role.trim().is_empty())
        || key.expires_at.is_some_and(|at| at.0 <= 0)
    {
        return Err(Failure::with_detail(
            ControlReason::InvalidCredential,
            "owner, tenant, privacy classes, roles or expiry are not acceptable for a virtual key",
        ));
    }
    let privacy_json = serde_json::to_string(key.privacy)
        .map_err(|error| Failure::internal(format!("cannot encode privacy classes: {error}")))?;
    let roles_json = serde_json::to_string(key.roles)
        .map_err(|error| Failure::internal(format!("cannot encode roles: {error}")))?;
    // `admin_scopes` 旧列永远写 `[]`：控制面权限的权威是 `credential_admin_scopes`（结构化的「操作 × 子树」），
    // 旧列是 M4 留下的自由字符串，没有授权语义。
    conn.execute(
        "INSERT INTO virtual_keys \
            (key_id, key_hash, owner, tenant_id, allowed_privacy, allowed_roles, budget_ref, \
             expires_at_ms, admin_scopes, status, revoked_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, '[]', 'active', NULL)",
        rusqlite::params![
            key.key_id,
            &key.hash.as_bytes()[..],
            key.owner,
            key.tenant_id,
            privacy_json,
            roles_json,
            key.expires_at.map(|at| at.0),
        ],
    )?;
    Ok(())
}

/// 记录凭证由谁签发。
pub(super) fn insert_issuance(
    conn: &Connection,
    credential: &str,
    issued_by: &Actor,
    now: TimestampMs,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO credential_issuance (credential_id, issued_by_kind, issued_by_id, issued_at_ms) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![credential, issued_by.kind(), issued_by.id(), now.0],
    )?;
    Ok(())
}

impl AccountStore {
    /// 签发凭证。授权看新凭证的绑定账户（成员凭证看成员关系的付款账户）；另外：
    ///
    /// - 新凭证的每条管理权限都必须被授权者自己的权限覆盖，且在新凭证自己的绑定账户之内
    ///   （`check_credential_issuance`，S0）；
    /// - 数据面上限（隐私等级、角色、租户绑定）和有效期都不能超过授权者本人；
    /// - 凭证元数据、`virtual_keys` 里的验证器、签发来源在**同一个事务**里写入（「先入库再交付」：
    ///   密钥在响应里出现时，库里已经有它的验证器；响应丢了，最多留下一张没人知道密钥的凭证，可以按
    ///   `credential_issuance` 找到并撤销）。
    pub fn control_issue_credential(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &IssueCredential,
    ) -> Result<Written<Credential>, Failure> {
        let spec =
            AuditSpec::new("issue_credential", TargetKind::Credential).target(&request.key_id);
        self.control_write(who, meta, spec, |conn, caller, spec| {
            // 先做与目标无关的授权判定，再按成员关系 id 去取付款账户（见 `require_op_capability`）：没有这个操作权限、或绑定账户
            // 被冻结的调用者，对已有的和不存在的成员关系得到同一个答案，没法靠错误码探测别的组织有哪些成员关系 id（issue #603 N7 同类）
            require_op_capability(conn, caller, meta.now, ControlOp::IssueCredential)?;
            // 目标账户 = 新凭证的绑定账户。绑定的成员关系不存在，和「不在范围内」是同一个答案。
            let anchor = binding_anchor(conn, &request.binding)?
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            spec.scope_account = Some(anchor.to_string());
            // **先授权、后校验**：没有资格的调用者永远得到授权类的答案，不会因为请求体写得不对而得到另一种答案。
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::IssueCredential,
                &anchor,
                None,
            )?;
            let id = CredentialId::new(request.key_id.clone()).map_err(|error| {
                Failure::with_detail(ControlReason::InvalidRequest, error.to_string())
            })?;
            let credential = Credential::new(
                id,
                request.binding.clone(),
                CredentialStatus::Active,
                request.expires_at,
                request.admin_scopes.clone(),
            )
            .map_err(|error| {
                Failure::with_detail(ControlReason::InvalidCredential, error.to_string())
            })?;
            let mut tenant = request.tenant_id.clone();
            if let (Some(issuer), Some(ceilings)) = (caller.credential(), caller.ceilings()) {
                // 新凭证的管理权限是授权者权限的子集、且不越出自己绑定的账户（S0）
                let membership = match &request.binding {
                    CredentialBinding::Member { membership } => load_membership(conn, membership)?,
                    _ => None,
                };
                let principals: Vec<Principal> = match &membership {
                    Some(found) => load_principal(conn, &found.principal)?
                        .into_iter()
                        .collect(),
                    None => Vec::new(),
                };
                let issuer_anchor = binding_anchor(conn, issuer.binding())?
                    .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
                let mut anchors: Vec<&AccountId> = vec![&anchor, &issuer_anchor];
                anchors.extend(request.admin_scopes.iter().map(|scope| &scope.subtree));
                let dir = slice_directory(conn, &anchors, membership.as_slice(), principals)?;
                check_credential_issuance(&dir, issuer, &credential, meta.now)
                    .map_err(|error| Failure::new(ControlReason::from(&error)))?;
                // 3. 数据面上限和有效期不超过授权者本人
                if tenant.is_none() {
                    tenant = ceilings.tenant_id.clone();
                }
                check_ceilings(
                    ceilings,
                    &request.allowed_privacy,
                    &request.allowed_roles,
                    &tenant,
                )?;
                check_expiry(issuer, request.expires_at)?;
            }
            if request.expires_at.is_some_and(|at| at <= meta.now) {
                return Err(Failure::with_detail(
                    ControlReason::InvalidCredential,
                    "the new credential would already be expired",
                ));
            }
            // 成员凭证和配对出来的设备共用同一个名额上限：直接给同一个成员关系签发成员凭证不能绕过它
            if let CredentialBinding::Member { membership } = &request.binding
                && occupied_device_slots(conn, membership, meta.now)?
                    >= MAX_ACTIVE_DEVICES_PER_MEMBERSHIP
            {
                return Err(Failure::new(ControlReason::DeviceLimitReached));
            }
            insert_virtual_key(
                conn,
                &NewKey {
                    key_id: &request.key_id,
                    hash: &request.key_hash,
                    owner: &request.owner,
                    tenant_id: tenant.as_deref(),
                    privacy: &request.allowed_privacy,
                    roles: &request.allowed_roles,
                    expires_at: request.expires_at,
                },
            )?;
            create_credential_in(conn, &credential, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Credential))?;
            insert_issuance(conn, &request.key_id, &caller.actor, meta.now)?;
            Ok(Written {
                result: WriteResult::Created,
                value: credential,
            })
        })
    }

    /// 冻结 / 解冻 / 撤销凭证。授权看目标凭证的绑定账户；冻结和解冻要 `freeze_credential`，撤销要 `revoke_credential`。
    /// 撤销同时把 `virtual_keys` 里的验证器标成已撤销（同一个事务）：数据面认证从此认不出这个密钥，
    /// 不依赖任何缓存。撤销是终态。
    pub fn control_set_credential_status(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &SetCredentialStatus,
    ) -> Result<Written<Credential>, Failure> {
        let op_name = match request.status {
            CredentialStatus::Revoked => "revoke_credential",
            CredentialStatus::Frozen => "freeze_credential",
            CredentialStatus::Active => "unfreeze_credential",
        };
        let spec = AuditSpec::new(op_name, TargetKind::Credential).target(&request.credential);
        let op = match request.status {
            CredentialStatus::Revoked => ControlOp::RevokeCredential,
            CredentialStatus::Frozen | CredentialStatus::Active => ControlOp::FreezeCredential,
        };
        self.control_write(who, meta, spec, |conn, caller, spec| {
            // 先做与目标无关的授权判定，再按 id 去查（见 `require_op_capability`）
            require_op_capability(conn, caller, meta.now, op)?;
            let Some(found) = load_credential(conn, &request.credential)? else {
                return Err(Failure::new(ControlReason::OutOfScope));
            };
            let anchor = binding_anchor(conn, found.binding())?
                .ok_or_else(|| Failure::new(ControlReason::OutOfScope))?;
            spec.scope_account = Some(anchor.to_string());
            authorize_op(conn, caller, meta.now, op, &anchor, None)?;
            if found.status() == request.status {
                return Ok(Written {
                    result: WriteResult::Unchanged,
                    value: found,
                });
            }
            set_credential_status_in(conn, &request.credential, request.status, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Credential))?;
            if request.status == CredentialStatus::Revoked {
                revoke_virtual_key(conn, request.credential.as_str(), meta.now)?;
            }
            let updated = load_credential(conn, &request.credential)?.ok_or_else(|| {
                Failure::internal("credential vanished inside its own transaction")
            })?;
            Ok(Written {
                result: WriteResult::Updated,
                value: updated,
            })
        })
    }
}

/// 把 `virtual_keys` 里这张凭证的验证器标成已撤销（已经撤销的不动）。
pub(super) fn revoke_virtual_key(
    conn: &Connection,
    key_id: &str,
    now: TimestampMs,
) -> Result<(), Failure> {
    conn.execute(
        "UPDATE virtual_keys SET status = 'revoked', revoked_at_ms = ?2 \
         WHERE key_id = ?1 AND status = 'active'",
        rusqlite::params![key_id, now.0.max(1)],
    )?;
    Ok(())
}

// ================================================================================================
// 额度
// ================================================================================================

/// 发放额度。授权看**授权方（父）账户**，金额 = 额度上限。免费额度的幂等键由服务端按
/// `subsidy:<子账户所属成员的主体>:<month>` 生成（调用方只给 `month`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantAllocation {
    pub id: AllocationId,
    pub parent: AccountId,
    pub child: AccountId,
    pub amount: Money,
    pub purpose: Purpose,
    pub valid_from: TimestampMs,
    pub valid_until: TimestampMs,
    /// 只有 `Purpose::FreeTier` 要（也只有它用）：免费额度的账期，幂等键 `(成员, 月份)` 的一半。
    pub month: Option<BillingMonth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeAllocation {
    pub id: AllocationId,
}

impl AccountStore {
    /// 发放额度（授权上限，不是转移余额）。授权看授权方（父）账户和金额上限；额度本身的合法性由 S0 的
    /// `validate_allocation` 判定（只沿树边、单位一致、不超过授权者本人、id 与幂等键不重复）。
    pub fn control_grant_allocation(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &GrantAllocation,
    ) -> Result<Written<AllocationRecord>, Failure> {
        let spec = AuditSpec::new("grant_allocation", TargetKind::Allocation)
            .target(&request.id)
            .scope(&request.parent)
            .related(&request.child)
            .amount(request.amount.clone());
        self.control_write(who, meta, spec, |conn, caller, _spec| {
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::GrantAllocation,
                &request.parent,
                Some(&request.amount),
            )?;
            let ceiling = Grant::new(
                request.amount.clone(),
                request.purpose,
                request.valid_from,
                request.valid_until,
            )
            .map_err(|error| {
                Failure::from_store(AccountStoreError::Grant(error), Ctx::Allocation)
            })?;
            if request.valid_until <= meta.now {
                return Err(Failure::with_detail(
                    ControlReason::InvalidAllocation,
                    "the allocation's validity window has already ended",
                ));
            }
            // 免费额度：补贴池的承诺要在同一个事务里登记（见下面 `commit_free_tier`）
            let mut free_tier: Option<(PrincipalId, BillingMonth)> = None;
            let idempotency_key = match (request.purpose, request.month) {
                (Purpose::FreeTier, Some(month)) => {
                    let principal: Option<String> = conn
                        .query_row(
                            "SELECT principal_id FROM memberships WHERE account_id = ?1",
                            [request.child.as_str()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let Some(principal) = principal else {
                        return Err(Failure::with_detail(
                            ControlReason::SubsidyKeyInvalid,
                            "the child account is not bound to a membership, so there is no member to grant to",
                        ));
                    };
                    let principal = PrincipalId::new(principal).map_err(AccountStoreError::from)?;
                    // 账期和有效窗口不能脱钩：幂等键 `(成员, 月份)` 只管「这个月只发一份」，要真的只有一份，
                    // 窗口就必须恰好是这个月（账本时区）。否则同一个主体换个 `month` 标签、配同一段窗口，
                    // 就能在同一段时间里拿到两份（PR #600 评审 #8）。
                    let (month_start, month_end) =
                        ledger::free_tier_window(conn, month).map_err(Failure::from_ledger)?;
                    if request.valid_from.0 != month_start || request.valid_until.0 != month_end {
                        return Err(Failure::with_detail(
                            ControlReason::InvalidAllocation,
                            format!(
                                "a free-tier allocation for {month} must be valid exactly for that month in the billing time zone: valid_from_ms {month_start}, valid_until_ms {month_end}"
                            ),
                        ));
                    }
                    let key = subsidy_idempotency_key(&principal, month)
                        .map_err(AccountStoreError::from)?;
                    free_tier = Some((principal, month));
                    Some(key)
                }
                (Purpose::FreeTier, None) => {
                    return Err(Failure::with_detail(
                        ControlReason::InvalidRequest,
                        "a free-tier allocation needs `month` (YYYY-MM)",
                    ));
                }
                (_, Some(_)) => {
                    return Err(Failure::with_detail(
                        ControlReason::InvalidRequest,
                        "`month` is only meaningful for free-tier allocations",
                    ));
                }
                (_, None) => None::<IdempotencyKey>,
            };
            let allocation = Allocation {
                id: request.id.clone(),
                parent: request.parent.clone(),
                child: request.child.clone(),
                ceiling,
                idempotency_key,
            };
            // 同一个 id 重发：内容相同是 replayed，内容不同是 already_exists；被撤销的不会复活。
            if let Some(existing) = load_allocation_record(conn, &allocation.id)? {
                let same = existing.allocation.parent == allocation.parent
                    && existing.allocation.child == allocation.child
                    && existing.allocation.ceiling == allocation.ceiling;
                return match (same, existing.revoked_at_ms.is_some()) {
                    (true, false) => Ok(Written {
                        result: WriteResult::Replayed,
                        value: existing,
                    }),
                    (true, true) => Err(Failure::with_detail(
                        ControlReason::AllocationRevoked,
                        format!(
                            "allocation {} was already issued and has been revoked",
                            allocation.id
                        ),
                    )),
                    (false, _) => Err(Failure::with_detail(
                        ControlReason::AlreadyExists,
                        format!(
                            "allocation {} already exists with a different definition",
                            allocation.id
                        ),
                    )),
                };
            }
            let outcome = create_allocation_in(conn, &allocation, meta.now)
                .map_err(|error| Failure::from_store(error, Ctx::Allocation))?;
            let (result, stored) = match outcome {
                AllocationOutcome::Created => (WriteResult::Created, allocation.clone()),
                AllocationOutcome::Replayed(existing) => (WriteResult::Replayed, existing),
            };
            // 免费额度：同一个事务里检查补贴池该账期的总上限并登记承诺（超出就返回错误，保存点回滚，额度行随之撤掉）。
            // 幂等重放也走这一步：已经登记过的什么都不写，没登记过的（旧数据）补登。
            if let Some((principal, month)) = &free_tier {
                ledger::commit_free_tier(conn, &stored, principal, *month, meta.now)
                    .map_err(Failure::from_ledger)?;
            }
            let id = stored.id;
            let record = load_allocation_record(conn, &id)?.ok_or_else(|| {
                Failure::internal("allocation vanished inside its own transaction")
            })?;
            Ok(Written {
                result,
                value: record,
            })
        })
    }

    /// 撤销一笔额度（「关授信」，不可逆）。要有「发这笔额度」的资格：授权方（父）账户在调用者的子树里，
    /// 并且这笔额度的金额不超过调用者权限的上限。撤销后它不再给任何人授权（`allocations_into` 等读函数排除它，
    /// `validate_allocation` 也不再把它当作授权方的额度）。
    pub fn control_revoke_allocation(
        &self,
        who: &Who<'_>,
        meta: &Meta<'_>,
        request: &RevokeAllocation,
    ) -> Result<Written<AllocationRecord>, Failure> {
        let spec = AuditSpec::new("revoke_allocation", TargetKind::Allocation).target(&request.id);
        self.control_write(who, meta, spec, |conn, caller, spec| {
            // 先做与目标无关的授权判定，再按 id 去查（见 `require_op_capability`）
            require_op_capability(conn, caller, meta.now, ControlOp::GrantAllocation)?;
            let Some(record) = load_allocation_record(conn, &request.id)? else {
                return Err(Failure::new(ControlReason::OutOfScope));
            };
            spec.scope_account = Some(record.allocation.parent.to_string());
            spec.related_id = Some(record.allocation.child.to_string());
            spec.amount = Some(record.allocation.ceiling.amount().clone());
            authorize_op(
                conn,
                caller,
                meta.now,
                ControlOp::GrantAllocation,
                &record.allocation.parent,
                Some(record.allocation.ceiling.amount()),
            )?;
            if record.revoked_at_ms.is_some() {
                return Ok(Written {
                    result: WriteResult::Unchanged,
                    value: record,
                });
            }
            conn.execute(
                "INSERT INTO allocation_revocations \
                    (allocation_id, revoked_at_ms, revoked_by_kind, revoked_by_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    request.id.as_str(),
                    meta.now.0,
                    caller.actor.kind(),
                    caller.actor.id(),
                ],
            )?;
            let updated = load_allocation_record(conn, &request.id)?.ok_or_else(|| {
                Failure::internal("allocation vanished inside its own transaction")
            })?;
            Ok(Written {
                result: WriteResult::Updated,
                value: updated,
            })
        })
    }
}
