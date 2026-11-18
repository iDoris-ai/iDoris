//! 控制面契约（设计稿 §9 第 1 项，评审 H5）：写操作的稳定原因码，以及「操作 × 资源范围」的授权判定。
//!
//! 本模块和 [`crate::credential`] 一样只有类型、纯函数和测试，没有存储、没有 HTTP：存储在
//! `idoris-tenancy::accounts`，HTTP 在 `idoris-router::control`，两边都**调用这里的 [`authorize`]**，
//! 不在别处重新实现授权规则。
//!
//! # 授权模型：一个谓词，用在两个地方
//!
//! 设计稿 §9：「授权看『操作 × 资源范围』……下发权限不能超过授权者本人。」[`AdminScope`] 是一条权限：
//! 能对 `subtree`（含）之下的账户做 `op`，`grant_allocation` 还限定单笔最多下发多少。
//! [`check_scope_delegation`] 回答「一条要下发的权限，是不是被授权者自己的某一条权限完全覆盖」。
//!
//! **一次动作被授权，当且仅当调用者有资格把「做这个动作所需要的那条权限」下发出去**：
//! [`authorize`] 把动作翻译成需要的 `AdminScope { op, subtree: 目标账户, max_amount: 金额 }`，再交给
//! `check_scope_delegation`。所以「签发凭证时能下发什么」和「执行动作时能做什么」是同一个谓词，
//! 两处不会出现分歧（一处放行、一处拒绝）。
//!
//! 动作 -> 资源范围（`subtree` 必须覆盖的那个账户，称为「目标账户」）：
//!
//! | 动作 | [`ControlOp`] | 目标账户 | 备注 |
//! |---|---|---|---|
//! | 登记主体 | `create_principal` | 请求里指定的组织 | 主体是全局的假名 id，本身不属于任何子树，所以要说明「以谁的名义登记」 |
//! | 撤销主体 | `revoke_principal` | 该主体**每一个**成员关系所在的账户 | 必须全部被覆盖：一个组织不能撤销别的组织也在用的主体（那会越权影响别人）；只想撤销自己这边，撤销成员关系即可 |
//! | 建 / 撤销成员关系 | `manage_membership` | 成员关系的付款账户；管理员关系（没有账户）用组织 | 成员账户一定在组织子树里（`Directory::build` 校验） |
//! | 建账户 | `create_account` | **父账户** | 新账户落在父账户的子树里 |
//! | 冻结 / 解冻 / 关闭账户 | `set_account_status` | 账户自己 | 不能动调用者自己绑定账户及其祖先（见 [`would_disable_caller`]） |
//! | 签发凭证 | `issue_credential` | 新凭证的绑定账户（成员凭证取成员关系的付款账户） | 另有 [`check_credential_issuance`] 的「新权限是授权者权限的子集」 |
//! | 冻结 / 解冻凭证 | `freeze_credential` | 目标凭证的绑定账户 | |
//! | 撤销凭证 | `revoke_credential` | 目标凭证的绑定账户 | |
//! | 发放 / 撤销额度 | `grant_allocation` | **授权方（父）账户**，金额 = 额度上限 | 目标是父账户而不是子账户：否则「给自己所在的组织发额度」会因为子账户在范围内而通过。撤销一笔额度要有「发这笔额度」的资格 |
//! | 只读查询 | `read` | 被查询的账户 | |
//!
//! # 检查顺序（同一份坏请求永远报同一个原因）
//!
//! 1. 凭证本身可用：撤销 → 冻结 → 过期（[`Credential::check_usable_at`]）；
//! 2. 只有 `Service` / `Node` 凭证带管理权限，成员凭证一律 `op_not_granted`；
//! 3. 凭证绑定账户及其所有祖先都是 `Active`（冻结 / 关闭连带整个子树）：被冻结组织的管理凭证不能给自己解冻；
//! 4. 调用者有没有任何一条 `op` 相同的权限（没有 -> `op_not_granted`）；
//! 5. 目标账户存在（不存在和「不在范围内」是同一个 `out_of_scope`，不泄露别的租户有没有这个账户）；
//! 6. 目标在某条权限的子树里，金额不超过那条权限的上限。
//!
//! 第 4 步在第 5 步之前：没有这个操作权限的调用者，无论目标存在与否都得到同一个答案。
//!
//! # 原因码
//!
//! [`ControlReason`] 的线上字符串和数据面的 [`crate::reason::ReasonCode`] 是**两套**：控制面的结果不是「钱不够」
//! 这类请求体验分级，不能混进 `X-iDoris-Reason` 的取值里。凭证状态类（`credential_revoked` 等）故意用同样的字符串，
//! 方便调用方（eduai 后端）一个映射表认两处。线上字符串一经发布不可变，`tests/wire_snapshots.rs` 钉住了全部取值；
//! 新增只能加变体。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::account::{AccountStatus, Directory};
use crate::credential::{
    AdminScope, ControlOp, Credential, CredentialBinding, DelegationError, ResolveError,
    check_scope_delegation,
};
use crate::ids::{AccountId, TimestampMs};
use crate::money::{Money, MoneyError};

/// 控制面操作被拒绝、被驳回或失败的稳定原因码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlReason {
    // ---- 调用者本身不可用 ----
    /// 请求没有带凭证，或凭证（密钥）认不出来：未知、格式不对、Admin token……全部是这一个答案，
    /// 不区分原因（不让调用方探测哪些密钥存在）。**没有凭证 id，所以不进审计**。
    Unauthenticated,
    /// 凭证已撤销。
    CredentialRevoked,
    /// 凭证已冻结。
    CredentialFrozen,
    /// 凭证已过期。
    CredentialExpired,
    /// 凭证绑定账户本身或它的某个祖先被冻结 / 关闭。
    AccountInactive,
    /// 这是一个有效的虚拟 key，但它不是云凭证（`credentials` 里没有它）：本地数据密钥没有控制面权限。
    NotAControlCredential,
    // ---- 授权 ----
    /// 调用者没有任何一条这个操作的权限。
    OpNotGranted,
    /// 目标账户不在调用者权限的子树里（或者根本不存在：两者不区分）。
    OutOfScope,
    /// 金额超过了调用者这条权限的单笔上限。
    AmountExceedsAuthority,
    /// 金额的单位和权限上限的单位不同，无法比较。
    AmountUnitMismatch,
    /// 签发凭证时，新凭证的权限超过了授权者本人。
    ScopeExceedsGrantor,
    /// 新凭证的某条权限的子树在它自己绑定的账户之外。
    ScopeOutsideBinding,
    /// 新凭证的数据面上限（隐私等级、角色、租户）超过了授权者本人。
    CeilingExceedsGrantor,
    /// 新凭证的有效期超过了授权者本人（授权者有有效期，新凭证没有或更晚）。
    ExpiryExceedsGrantor,
    /// 这个操作会让调用者自己的凭证失效（冻结 / 关闭自己的绑定账户或祖先）。
    WouldDisableCaller,
    // ---- 请求本身不合法（授权通过之后）----
    /// 请求体不是合法的 JSON / 缺字段 / 多字段 / id 字符集不对。
    InvalidRequest,
    /// 账户不合法（父账户不存在、种类或单位不匹配、太深……）。
    InvalidAccount,
    /// 成员关系不合法（角色与账户种类对不上、账户不在组织子树里……）。
    InvalidMembership,
    /// 凭证不合法（成员凭证带了管理权限、绑定目标种类不对……）。
    InvalidCredential,
    /// 额度不合法（不是父子边、单位不匹配、有效期……）。
    InvalidAllocation,
    /// 金额不合法（负数、溢出）。
    InvalidAmount,
    /// 主体已被撤销，不能再给它建成员关系或配对。
    PrincipalRevoked,
    /// 成员关系已被撤销。
    MembershipRevoked,
    // ---- 状态冲突 ----
    /// 同一个 id 已经存在，而且内容不同（内容相同的重发是 `replayed`，不是错误）。
    AlreadyExists,
    /// 同一个幂等键已经被不同内容的额度用过。
    IdempotencyConflict,
    /// 不允许的状态转移（撤销、关闭是终态）。
    IllegalTransition,
    /// 额度超过了授权方（父账户）自己拿到的额度。
    AllocationExceedsGrantor,
    /// 授权方（父账户）自己没有覆盖这个有效期的额度，没有可下发的权限。
    GrantorHasNoAuthority,
    /// 这笔额度已经被撤销。
    AllocationRevoked,
    /// 免费额度的幂等键不是 `subsidy:<成员的主体>:<YYYY-MM>`。
    SubsidyKeyInvalid,
    // ---- 云账号配对 ----
    /// 配对码格式不对或查无此码（两者不区分）。
    PairingCodeInvalid,
    /// 配对码过期。
    PairingCodeExpired,
    /// 配对码已经被兑换过。
    PairingCodeUsed,
    /// 配对码已被撤销。
    PairingCodeRevoked,
    /// 兑换失败次数太多，暂时拒绝所有兑换。
    PairingRateLimited,
    /// 这个成员关系已经有太多设备凭证。
    DeviceLimitReached,
    /// 这张凭证不是配对出来的设备凭证。
    NotADevice,
    // ---- 基础设施 ----
    /// 数据库忙，可以重试。
    StoreBusy,
    /// 审计记录写不进去，所以这次操作没有执行（没有审计就没有写操作）。
    AuditUnavailable,
    /// 内部错误。细节只进日志，不进响应。
    InternalError,
}

/// 审计记录里的结果类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// 写入成功（新建或状态改变）。
    Success,
    /// 同样的请求早已生效，这次什么都没写（幂等重发）。
    Replayed,
    /// 目标已经处于要求的状态，什么都没改。
    Unchanged,
    /// 调用者没有资格（状态不可用或授权不通过）。
    Denied,
    /// 调用者有资格，但请求被驳回（不合法、冲突、配对码无效……）。
    Rejected,
    /// 基础设施错误，操作没有执行。
    Error,
}

impl AuditOutcome {
    pub const ALL: [AuditOutcome; 6] = [
        AuditOutcome::Success,
        AuditOutcome::Replayed,
        AuditOutcome::Unchanged,
        AuditOutcome::Denied,
        AuditOutcome::Rejected,
        AuditOutcome::Error,
    ];

    /// 线上字符串，也是 `control_audit.outcome` 的取值。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Success => "success",
            AuditOutcome::Replayed => "replayed",
            AuditOutcome::Unchanged => "unchanged",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Rejected => "rejected",
            AuditOutcome::Error => "error",
        }
    }
}

impl fmt::Display for AuditOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 成功的写操作的结果。线上字符串固定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteResult {
    /// 新建。
    Created,
    /// 同样的请求早已生效，返回已有的那一条（没有写任何东西）。
    Replayed,
    /// 状态改变了。
    Updated,
    /// 已经是要求的状态，什么都没改。
    Unchanged,
}

impl WriteResult {
    pub const ALL: [WriteResult; 4] = [
        WriteResult::Created,
        WriteResult::Replayed,
        WriteResult::Updated,
        WriteResult::Unchanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WriteResult::Created => "created",
            WriteResult::Replayed => "replayed",
            WriteResult::Updated => "updated",
            WriteResult::Unchanged => "unchanged",
        }
    }

    /// 对应的审计结果类别。
    pub fn audit_outcome(self) -> AuditOutcome {
        match self {
            WriteResult::Created | WriteResult::Updated => AuditOutcome::Success,
            WriteResult::Replayed => AuditOutcome::Replayed,
            WriteResult::Unchanged => AuditOutcome::Unchanged,
        }
    }
}

impl ControlReason {
    /// 全部原因码，顺序固定（= 变体声明顺序）。测试用它保证没有漏钉任何一个字符串。
    pub const ALL: [ControlReason; 40] = [
        ControlReason::Unauthenticated,
        ControlReason::CredentialRevoked,
        ControlReason::CredentialFrozen,
        ControlReason::CredentialExpired,
        ControlReason::AccountInactive,
        ControlReason::NotAControlCredential,
        ControlReason::OpNotGranted,
        ControlReason::OutOfScope,
        ControlReason::AmountExceedsAuthority,
        ControlReason::AmountUnitMismatch,
        ControlReason::ScopeExceedsGrantor,
        ControlReason::ScopeOutsideBinding,
        ControlReason::CeilingExceedsGrantor,
        ControlReason::ExpiryExceedsGrantor,
        ControlReason::WouldDisableCaller,
        ControlReason::InvalidRequest,
        ControlReason::InvalidAccount,
        ControlReason::InvalidMembership,
        ControlReason::InvalidCredential,
        ControlReason::InvalidAllocation,
        ControlReason::InvalidAmount,
        ControlReason::PrincipalRevoked,
        ControlReason::MembershipRevoked,
        ControlReason::AlreadyExists,
        ControlReason::IdempotencyConflict,
        ControlReason::IllegalTransition,
        ControlReason::AllocationExceedsGrantor,
        ControlReason::GrantorHasNoAuthority,
        ControlReason::AllocationRevoked,
        ControlReason::SubsidyKeyInvalid,
        ControlReason::PairingCodeInvalid,
        ControlReason::PairingCodeExpired,
        ControlReason::PairingCodeUsed,
        ControlReason::PairingCodeRevoked,
        ControlReason::PairingRateLimited,
        ControlReason::DeviceLimitReached,
        ControlReason::NotADevice,
        ControlReason::StoreBusy,
        ControlReason::AuditUnavailable,
        ControlReason::InternalError,
    ];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            ControlReason::Unauthenticated => "unauthenticated",
            ControlReason::CredentialRevoked => "credential_revoked",
            ControlReason::CredentialFrozen => "credential_frozen",
            ControlReason::CredentialExpired => "credential_expired",
            ControlReason::AccountInactive => "account_inactive",
            ControlReason::NotAControlCredential => "not_a_control_credential",
            ControlReason::OpNotGranted => "op_not_granted",
            ControlReason::OutOfScope => "out_of_scope",
            ControlReason::AmountExceedsAuthority => "amount_exceeds_authority",
            ControlReason::AmountUnitMismatch => "amount_unit_mismatch",
            ControlReason::ScopeExceedsGrantor => "scope_exceeds_grantor",
            ControlReason::ScopeOutsideBinding => "scope_outside_binding",
            ControlReason::CeilingExceedsGrantor => "ceiling_exceeds_grantor",
            ControlReason::ExpiryExceedsGrantor => "expiry_exceeds_grantor",
            ControlReason::WouldDisableCaller => "would_disable_caller",
            ControlReason::InvalidRequest => "invalid_request",
            ControlReason::InvalidAccount => "invalid_account",
            ControlReason::InvalidMembership => "invalid_membership",
            ControlReason::InvalidCredential => "invalid_credential",
            ControlReason::InvalidAllocation => "invalid_allocation",
            ControlReason::InvalidAmount => "invalid_amount",
            ControlReason::PrincipalRevoked => "principal_revoked",
            ControlReason::MembershipRevoked => "membership_revoked",
            ControlReason::AlreadyExists => "already_exists",
            ControlReason::IdempotencyConflict => "idempotency_conflict",
            ControlReason::IllegalTransition => "illegal_transition",
            ControlReason::AllocationExceedsGrantor => "allocation_exceeds_grantor",
            ControlReason::GrantorHasNoAuthority => "grantor_has_no_authority",
            ControlReason::AllocationRevoked => "allocation_revoked",
            ControlReason::SubsidyKeyInvalid => "subsidy_key_invalid",
            ControlReason::PairingCodeInvalid => "pairing_code_invalid",
            ControlReason::PairingCodeExpired => "pairing_code_expired",
            ControlReason::PairingCodeUsed => "pairing_code_used",
            ControlReason::PairingCodeRevoked => "pairing_code_revoked",
            ControlReason::PairingRateLimited => "pairing_rate_limited",
            ControlReason::DeviceLimitReached => "device_limit_reached",
            ControlReason::NotADevice => "not_a_device",
            ControlReason::StoreBusy => "store_busy",
            ControlReason::AuditUnavailable => "audit_unavailable",
            ControlReason::InternalError => "internal_error",
        }
    }

    /// 审计里的结果类别：调用者没资格（状态不可用或授权不通过）是 `denied`，有资格但请求被驳回是 `rejected`，
    /// 基础设施问题是 `error`。
    pub fn audit_outcome(self) -> AuditOutcome {
        match self {
            ControlReason::Unauthenticated
            | ControlReason::CredentialRevoked
            | ControlReason::CredentialFrozen
            | ControlReason::CredentialExpired
            | ControlReason::AccountInactive
            | ControlReason::NotAControlCredential
            | ControlReason::OpNotGranted
            | ControlReason::OutOfScope
            | ControlReason::AmountExceedsAuthority
            | ControlReason::AmountUnitMismatch
            | ControlReason::ScopeExceedsGrantor
            | ControlReason::ScopeOutsideBinding
            | ControlReason::CeilingExceedsGrantor
            | ControlReason::ExpiryExceedsGrantor
            | ControlReason::WouldDisableCaller => AuditOutcome::Denied,
            ControlReason::StoreBusy
            | ControlReason::AuditUnavailable
            | ControlReason::InternalError => AuditOutcome::Error,
            _ => AuditOutcome::Rejected,
        }
    }

    /// 是不是「调用者没有资格」这一类（凭证不可用或授权不通过）。
    pub fn is_denial(self) -> bool {
        self.audit_outcome() == AuditOutcome::Denied
    }
}

impl fmt::Display for ControlReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未知的控制面原因码字符串。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown control reason code {0:?}")]
pub struct UnknownControlReason(pub String);

impl FromStr for ControlReason {
    type Err = UnknownControlReason;
    fn from_str(s: &str) -> Result<Self, UnknownControlReason> {
        ControlReason::ALL
            .into_iter()
            .find(|code| code.as_str() == s)
            .ok_or_else(|| UnknownControlReason(s.to_string()))
    }
}

impl From<&ResolveError> for ControlReason {
    /// 凭证本身不可用的三种情况各有自己的码；其它（成员 id、账户状态……）按它们在数据面的含义映射。
    fn from(error: &ResolveError) -> Self {
        match error {
            ResolveError::CredentialRevoked(_) => ControlReason::CredentialRevoked,
            ResolveError::CredentialFrozen(_) => ControlReason::CredentialFrozen,
            ResolveError::CredentialExpired { .. } => ControlReason::CredentialExpired,
            ResolveError::PrincipalRevoked(_) => ControlReason::PrincipalRevoked,
            ResolveError::MembershipRevoked(_) => ControlReason::MembershipRevoked,
            ResolveError::AccountInactive { .. } => ControlReason::AccountInactive,
            _ => ControlReason::OutOfScope,
        }
    }
}

impl From<&MoneyError> for ControlReason {
    fn from(error: &MoneyError) -> Self {
        match error {
            MoneyError::UnitMismatch { .. } => ControlReason::AmountUnitMismatch,
            _ => ControlReason::InvalidAmount,
        }
    }
}

impl From<&DelegationError> for ControlReason {
    /// 签发凭证 / 下发权限失败的原因码。账户或成员关系不存在，和「不在范围内」是同一个 `out_of_scope`。
    fn from(error: &DelegationError) -> Self {
        match error {
            DelegationError::ExceedsGrantor { .. } => ControlReason::ScopeExceedsGrantor,
            DelegationError::ScopeOutsideBinding { .. } => ControlReason::ScopeOutsideBinding,
            DelegationError::UnknownAccount(_) | DelegationError::UnknownMembership(_) => {
                ControlReason::OutOfScope
            }
            DelegationError::IssuerNotUsable(resolve) => ControlReason::from(resolve),
            DelegationError::Money(money) => ControlReason::from(money),
        }
    }
}

/// 凭证绑定的那个账户：`Service` / `Node` 是绑定的组织账户，`Member` 是成员关系的付款账户。
/// 它是凭证的权限范围的外框，也是「这张凭证活在哪棵子树里」。
pub fn credential_anchor(dir: &Directory, credential: &Credential) -> Option<AccountId> {
    match credential.binding() {
        CredentialBinding::Service { account } | CredentialBinding::Node { account } => {
            Some(account.clone())
        }
        CredentialBinding::Member { membership } => dir
            .membership(membership)
            .and_then(|found| found.account.clone()),
    }
}

/// 把 `account` 冻结 / 关闭，会不会让调用者自己的凭证失效：`account` 是调用者绑定账户本身或它的祖先。
/// 这类操作一律拒绝（`would_disable_caller`）：否则一张被盗的管理凭证能把整个组织关掉、
/// 或者管理员把自己锁在门外而没有人能解冻（组织被冻结时，它自己的管理凭证也不可用）。
pub fn would_disable_caller(dir: &Directory, caller: &Credential, account: &AccountId) -> bool {
    match credential_anchor(dir, caller) {
        Some(anchor) => dir.tree().is_descendant_or_self(&anchor, account),
        None => false,
    }
}

/// 授权判定。检查顺序和每一步的原因码见模块文档。`target` 是动作的目标账户（见模块文档的表）；
/// `amount` 只给 `grant_allocation`：额度上限。
///
/// `dir` 必须至少包含调用者绑定账户的祖先链和 `target` 的祖先链（用 `AccountTree::ancestors` 取一小片即可）。
pub fn authorize(
    dir: &Directory,
    caller: &Credential,
    now: TimestampMs,
    op: ControlOp,
    target: &AccountId,
    amount: Option<&Money>,
) -> Result<(), ControlReason> {
    // 1. 凭证本身
    caller
        .check_usable_at(now)
        .map_err(|error| ControlReason::from(&error))?;
    // 2. 只有 service / node 带管理权限
    let anchor = match caller.binding() {
        CredentialBinding::Service { account } | CredentialBinding::Node { account } => account,
        CredentialBinding::Member { .. } => return Err(ControlReason::OpNotGranted),
    };
    // 3. 绑定账户链必须是 Active
    match dir.tree().inactive_cause(anchor) {
        Ok(None) => {}
        Ok(Some((_, status))) => {
            debug_assert_ne!(status, AccountStatus::Active);
            return Err(ControlReason::AccountInactive);
        }
        // 凭证绑到一个树里找不到的账户：库被改坏了。当作不在任何范围里，而不是放行。
        Err(_) => return Err(ControlReason::OutOfScope),
    }
    // 4. 有没有任何一条这个操作的权限
    let mut scopes = caller
        .admin_scopes()
        .iter()
        .filter(|scope| scope.op == op)
        .peekable();
    if scopes.peek().is_none() {
        return Err(ControlReason::OpNotGranted);
    }
    // 5. 目标存在
    if dir.tree().get(target).is_none() {
        return Err(ControlReason::OutOfScope);
    }
    if let Some(amount) = amount
        && amount.micros < 0
    {
        return Err(ControlReason::InvalidAmount);
    }
    // 6. 目标在子树里、金额不超上限：「需要的那条权限」被调用者的某一条权限完全覆盖
    let needed = AdminScope {
        op,
        subtree: target.clone(),
        max_amount: amount.cloned(),
    };
    match check_scope_delegation(dir, caller.admin_scopes(), &needed) {
        Ok(()) => Ok(()),
        Err(DelegationError::ExceedsGrantor { .. }) => {
            // 区分「子树不对」和「子树对，但金额超了」：有没有一条权限的子树覆盖了目标。
            let covers_target = caller.admin_scopes().iter().any(|scope| {
                scope.op == op && dir.tree().is_descendant_or_self(target, &scope.subtree)
            });
            Err(if covers_target {
                ControlReason::AmountExceedsAuthority
            } else {
                ControlReason::OutOfScope
            })
        }
        Err(other) => Err(ControlReason::from(&other)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::account::{AccountKind, AccountTree, MembershipRole};
    use crate::credential::CredentialStatus;
    use crate::ids::CredentialId;
    use crate::money::Unit;
    use crate::test_support::{
        account, active, aid, membership, principal, sample_accounts, sample_directory,
        sample_directory_with,
    };

    const NOW: TimestampMs = TimestampMs(1_000);

    fn thb(micros: i64) -> Money {
        Money::new(Unit::Thb, micros)
    }

    fn scope(op: ControlOp, subtree: &str, max: Option<i64>) -> AdminScope {
        AdminScope {
            op,
            subtree: aid(subtree),
            max_amount: max.map(thb),
        }
    }

    fn service(
        id: &str,
        anchor: &str,
        status: CredentialStatus,
        expires: Option<i64>,
        scopes: Vec<AdminScope>,
    ) -> Credential {
        Credential::new(
            CredentialId::new(id).unwrap(),
            CredentialBinding::Service {
                account: aid(anchor),
            },
            status,
            expires.map(TimestampMs),
            scopes,
        )
        .unwrap()
    }

    fn active_service(anchor: &str, scopes: Vec<AdminScope>) -> Credential {
        service("c-1", anchor, CredentialStatus::Active, None, scopes)
    }

    // ---------- 原因码 ----------

    #[test]
    fn every_reason_has_a_distinct_snake_case_string_that_round_trips() {
        let mut seen = std::collections::BTreeSet::new();
        for code in &ControlReason::ALL {
            let text = code.as_str();
            assert!(
                text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{text}"
            );
            assert!(seen.insert(text), "duplicate string {text}");
            assert_eq!(text.parse::<ControlReason>().unwrap(), *code);
            assert_eq!(serde_json::to_string(code).unwrap(), format!("\"{text}\""));
            assert_eq!(
                serde_json::from_str::<ControlReason>(&format!("\"{text}\"")).unwrap(),
                *code
            );
        }
        assert_eq!(seen.len(), ControlReason::ALL.len());
        assert!("nope".parse::<ControlReason>().is_err());
        // 状态类的码和数据面原因码同一个字符串（调用方一张映射表）
        for (control, data) in [
            (
                ControlReason::CredentialRevoked,
                crate::reason::ReasonCode::CredentialRevoked,
            ),
            (
                ControlReason::CredentialFrozen,
                crate::reason::ReasonCode::CredentialFrozen,
            ),
            (
                ControlReason::CredentialExpired,
                crate::reason::ReasonCode::CredentialExpired,
            ),
            (
                ControlReason::AccountInactive,
                crate::reason::ReasonCode::AccountInactive,
            ),
            (
                ControlReason::PrincipalRevoked,
                crate::reason::ReasonCode::PrincipalRevoked,
            ),
            (
                ControlReason::MembershipRevoked,
                crate::reason::ReasonCode::MembershipRevoked,
            ),
        ] {
            assert_eq!(control.as_str(), data.as_str());
        }
    }

    #[test]
    fn audit_outcome_classes_are_denied_rejected_or_error() {
        for code in ControlReason::ALL {
            let outcome = code.audit_outcome();
            assert!(
                matches!(
                    outcome,
                    AuditOutcome::Denied | AuditOutcome::Rejected | AuditOutcome::Error
                ),
                "{code}"
            );
            assert_eq!(code.is_denial(), outcome == AuditOutcome::Denied);
        }
        assert_eq!(
            ControlReason::OutOfScope.audit_outcome(),
            AuditOutcome::Denied
        );
        assert_eq!(
            ControlReason::InvalidAccount.audit_outcome(),
            AuditOutcome::Rejected
        );
        assert_eq!(
            ControlReason::StoreBusy.audit_outcome(),
            AuditOutcome::Error
        );
        for result in WriteResult::ALL {
            assert!(matches!(
                result.audit_outcome(),
                AuditOutcome::Success | AuditOutcome::Replayed | AuditOutcome::Unchanged
            ));
        }
    }

    // ---------- 授权矩阵（纯函数层）----------
    //
    // 夹具：平台 -> 补贴池 pool -> 租户 eduai -> 担保人 teacher -> 学生 s1；eduai -> 学生 s2；
    // 平台 -> 个人 p1；平台 -> 学校 school-b。

    /// 每个操作在「目标 = teacher 的子树里」的一个具体目标和（需要时）金额。
    fn target_in_scope(op: ControlOp) -> (AccountId, Option<Money>) {
        match op {
            ControlOp::GrantAllocation => (aid("teacher"), Some(thb(5_000_000))),
            _ => (aid("s1"), None),
        }
    }

    #[test]
    fn authorization_matrix_for_every_operation() {
        let dir = sample_directory();
        for op in ControlOp::ALL {
            let (inside, amount) = target_in_scope(op);
            let cap = matches!(op, ControlOp::GrantAllocation).then_some(10_000_000);
            let scopes = vec![scope(op, "teacher", cap)];

            // 范围内放行
            let ok = active_service("eduai", scopes.clone());
            assert_eq!(
                authorize(&dir, &ok, NOW, op, &inside, amount.as_ref()),
                Ok(()),
                "{op:?} in scope"
            );
            // 子树的根自己也算范围内（含）
            assert_eq!(
                authorize(&dir, &ok, NOW, op, &aid("teacher"), amount.as_ref()),
                Ok(()),
                "{op:?} subtree root"
            );

            // 范围外：同一个组织里、但不在 teacher 的子树里
            for outside in ["s2", "eduai", "pool", "platform", "school-b", "p1"] {
                assert_eq!(
                    authorize(&dir, &ok, NOW, op, &aid(outside), amount.as_ref()),
                    Err(ControlReason::OutOfScope),
                    "{op:?} outside {outside}"
                );
            }
            // 不存在的账户和「范围外」同一个答案（不泄露别的租户有没有这个账户）
            assert_eq!(
                authorize(&dir, &ok, NOW, op, &aid("ghost"), amount.as_ref()),
                Err(ControlReason::OutOfScope),
                "{op:?} ghost"
            );

            // 没有这个操作的权限：持有的是另一个操作
            let other = ControlOp::ALL
                .into_iter()
                .find(|candidate| *candidate != op && *candidate != ControlOp::GrantAllocation)
                .unwrap();
            let wrong_op = active_service("eduai", vec![scope(other, "teacher", None)]);
            assert_eq!(
                authorize(&dir, &wrong_op, NOW, op, &inside, amount.as_ref()),
                Err(ControlReason::OpNotGranted),
                "{op:?} wrong op"
            );
            // 没有任何权限
            let none = active_service("eduai", vec![]);
            assert_eq!(
                authorize(&dir, &none, NOW, op, &inside, amount.as_ref()),
                Err(ControlReason::OpNotGranted),
                "{op:?} no scopes"
            );
            // 没有这个操作权限的调用者，目标存在与否答案相同
            assert_eq!(
                authorize(&dir, &none, NOW, op, &aid("ghost"), amount.as_ref()),
                Err(ControlReason::OpNotGranted),
                "{op:?} no scopes, ghost"
            );

            // 被撤销 / 冻结 / 过期的调用者
            for (status, expires, expected) in [
                (
                    CredentialStatus::Revoked,
                    None,
                    ControlReason::CredentialRevoked,
                ),
                (
                    CredentialStatus::Frozen,
                    None,
                    ControlReason::CredentialFrozen,
                ),
                (
                    CredentialStatus::Active,
                    Some(1_000),
                    ControlReason::CredentialExpired,
                ),
                (
                    CredentialStatus::Active,
                    Some(999),
                    ControlReason::CredentialExpired,
                ),
            ] {
                let caller = service("c-1", "eduai", status, expires, scopes.clone());
                assert_eq!(
                    authorize(&dir, &caller, NOW, op, &inside, amount.as_ref()),
                    Err(expected),
                    "{op:?} {status:?} {expires:?}"
                );
            }
            // 刚好差一毫秒到期：仍然可用（右端开）
            let almost = service(
                "c-1",
                "eduai",
                CredentialStatus::Active,
                Some(1_001),
                scopes.clone(),
            );
            assert_eq!(
                authorize(&dir, &almost, NOW, op, &inside, amount.as_ref()),
                Ok(()),
                "{op:?} one ms before expiry"
            );

            // 成员凭证没有管理权限
            let member = Credential::new(
                CredentialId::new("c-m").unwrap(),
                CredentialBinding::Member {
                    membership: crate::test_support::mid("m-kid1"),
                },
                CredentialStatus::Active,
                None,
                vec![],
            )
            .unwrap();
            assert_eq!(
                authorize(&dir, &member, NOW, op, &inside, amount.as_ref()),
                Err(ControlReason::OpNotGranted),
                "{op:?} member credential"
            );
        }
    }

    #[test]
    fn a_frozen_or_closed_binding_account_disables_its_credentials_but_not_the_other_way() {
        for (anchor_status, expected_inactive) in [
            (AccountStatus::Frozen, true),
            (AccountStatus::Closed, true),
            (AccountStatus::Active, false),
        ] {
            let accounts: Vec<_> = sample_accounts()
                .into_iter()
                .map(|a| {
                    if a.id == aid("eduai") {
                        account("eduai", AccountKind::Org, Some("pool"), anchor_status)
                    } else {
                        a
                    }
                })
                .collect();
            let dir = sample_directory_with(AccountTree::build(accounts).unwrap(), |_| {});
            let caller = active_service(
                "eduai",
                vec![scope(ControlOp::SetAccountStatus, "eduai", None)],
            );
            let result = authorize(
                &dir,
                &caller,
                NOW,
                ControlOp::SetAccountStatus,
                &aid("s2"),
                None,
            );
            if expected_inactive {
                assert_eq!(
                    result,
                    Err(ControlReason::AccountInactive),
                    "{anchor_status:?}"
                );
            } else {
                assert_eq!(result, Ok(()));
            }
        }
        // 祖先被冻结同样连带：绑在 eduai 的凭证，在 pool 被冻结时不可用
        let accounts: Vec<_> = sample_accounts()
            .into_iter()
            .map(|a| {
                if a.id == aid("pool") {
                    account(
                        "pool",
                        AccountKind::SubsidyPool,
                        Some("platform"),
                        AccountStatus::Frozen,
                    )
                } else {
                    a
                }
            })
            .collect();
        let dir = sample_directory_with(AccountTree::build(accounts).unwrap(), |_| {});
        let caller = active_service("eduai", vec![scope(ControlOp::Read, "eduai", None)]);
        assert_eq!(
            authorize(&dir, &caller, NOW, ControlOp::Read, &aid("s1"), None),
            Err(ControlReason::AccountInactive)
        );
        // 目标子树里有冻结的账户不影响调用者：冻结的学生账户，组织的管理凭证照常能解冻它
        let accounts: Vec<_> = sample_accounts()
            .into_iter()
            .map(|a| {
                if a.id == aid("s1") {
                    account(
                        "s1",
                        AccountKind::Member,
                        Some("teacher"),
                        AccountStatus::Frozen,
                    )
                } else {
                    a
                }
            })
            .collect();
        let dir = sample_directory_with(AccountTree::build(accounts).unwrap(), |_| {});
        let caller = active_service(
            "eduai",
            vec![scope(ControlOp::SetAccountStatus, "eduai", None)],
        );
        assert_eq!(
            authorize(
                &dir,
                &caller,
                NOW,
                ControlOp::SetAccountStatus,
                &aid("s1"),
                None
            ),
            Ok(())
        );
    }

    #[test]
    fn grant_allocation_is_authorized_on_the_grantor_side_with_a_per_grant_cap() {
        let dir = sample_directory();
        let caller = active_service(
            "eduai",
            vec![scope(ControlOp::GrantAllocation, "eduai", Some(5_000_000))],
        );
        let grant = |parent: &str, micros: i64| {
            authorize(
                &dir,
                &caller,
                NOW,
                ControlOp::GrantAllocation,
                &aid(parent),
                Some(&thb(micros)),
            )
        };
        // 上限内、上限刚好、超 1 微单位
        assert_eq!(grant("eduai", 5_000_000), Ok(()));
        assert_eq!(grant("eduai", 1), Ok(()));
        assert_eq!(grant("eduai", 0), Ok(()));
        assert_eq!(
            grant("eduai", 5_000_001),
            Err(ControlReason::AmountExceedsAuthority)
        );
        // 授权方在子树里（teacher 给 s1），授权方在子树外（补贴池给 eduai，即「给自己加额度」）
        assert_eq!(grant("teacher", 1_000_000), Ok(()));
        assert_eq!(grant("pool", 1), Err(ControlReason::OutOfScope));
        assert_eq!(grant("platform", 1), Err(ControlReason::OutOfScope));
        // 超额又在范围外：先报范围外（不泄露「这个目标要多少额度才够」）
        assert_eq!(
            grant("school-b", 99_000_000),
            Err(ControlReason::OutOfScope)
        );
        // 负数金额
        assert_eq!(grant("eduai", -1), Err(ControlReason::InvalidAmount));
        // 不同单位：没法比较，拒绝而不是默默当成放行
        assert_eq!(
            authorize(
                &dir,
                &caller,
                NOW,
                ControlOp::GrantAllocation,
                &aid("eduai"),
                Some(&Money::new(Unit::Usd, 1)),
            ),
            Err(ControlReason::AmountUnitMismatch)
        );
        // 没有上限的权限：任意金额
        let unlimited = active_service(
            "eduai",
            vec![AdminScope {
                op: ControlOp::GrantAllocation,
                subtree: aid("eduai"),
                max_amount: None,
            }],
        );
        assert_eq!(
            authorize(
                &dir,
                &unlimited,
                NOW,
                ControlOp::GrantAllocation,
                &aid("eduai"),
                Some(&thb(i64::MAX)),
            ),
            Ok(())
        );
        // 多条权限：任意一条覆盖即可（子树不同、上限不同）
        let two = active_service(
            "eduai",
            vec![
                scope(ControlOp::GrantAllocation, "teacher", Some(1_000)),
                scope(ControlOp::GrantAllocation, "eduai", Some(9_000)),
            ],
        );
        let with_two = |parent: &str, micros: i64| {
            authorize(
                &dir,
                &two,
                NOW,
                ControlOp::GrantAllocation,
                &aid(parent),
                Some(&thb(micros)),
            )
        };
        assert_eq!(with_two("teacher", 9_000), Ok(())); // 第二条（更大的子树、更大的上限）覆盖
        assert_eq!(
            with_two("teacher", 9_001),
            Err(ControlReason::AmountExceedsAuthority)
        );
    }

    #[test]
    fn scopes_of_a_different_operation_never_cover_this_one() {
        let dir = sample_directory();
        // 最大的子树 + 没有上限，但是是别的操作
        let caller = active_service(
            "eduai",
            vec![
                scope(ControlOp::Read, "eduai", None),
                scope(ControlOp::CreateAccount, "eduai", None),
            ],
        );
        assert_eq!(
            authorize(
                &dir,
                &caller,
                NOW,
                ControlOp::SetAccountStatus,
                &aid("s1"),
                None
            ),
            Err(ControlReason::OpNotGranted)
        );
        assert_eq!(
            authorize(&dir, &caller, NOW, ControlOp::Read, &aid("s1"), None),
            Ok(())
        );
        // 有 grant_allocation 权限但没有 read：读不了
        let granter = active_service(
            "eduai",
            vec![scope(ControlOp::GrantAllocation, "eduai", Some(1))],
        );
        assert_eq!(
            authorize(&dir, &granter, NOW, ControlOp::Read, &aid("s1"), None),
            Err(ControlReason::OpNotGranted)
        );
    }

    #[test]
    fn would_disable_caller_covers_the_binding_account_and_its_ancestors_only() {
        let dir = sample_directory();
        let caller = active_service("eduai", vec![]);
        for (account, expected) in [
            ("eduai", true),
            ("pool", true),
            ("platform", true),
            ("teacher", false),
            ("s1", false),
            ("school-b", false),
            ("p1", false),
        ] {
            assert_eq!(
                would_disable_caller(&dir, &caller, &aid(account)),
                expected,
                "{account}"
            );
        }
        // 成员凭证：绑定账户是它自己的付款账户
        let member = Credential::new(
            CredentialId::new("c-m").unwrap(),
            CredentialBinding::Member {
                membership: crate::test_support::mid("m-kid1"),
            },
            CredentialStatus::Active,
            None,
            vec![],
        )
        .unwrap();
        assert!(would_disable_caller(&dir, &member, &aid("s1")));
        assert!(would_disable_caller(&dir, &member, &aid("teacher")));
        assert!(!would_disable_caller(&dir, &member, &aid("s2")));
    }

    #[test]
    fn credential_anchor_follows_the_binding() {
        let dir = sample_directory();
        let node = Credential::new(
            CredentialId::new("c-n").unwrap(),
            CredentialBinding::Node {
                account: aid("eduai"),
            },
            CredentialStatus::Active,
            None,
            vec![],
        )
        .unwrap();
        assert_eq!(credential_anchor(&dir, &node), Some(aid("eduai")));
        let member = Credential::new(
            CredentialId::new("c-m").unwrap(),
            CredentialBinding::Member {
                membership: crate::test_support::mid("m-kid1"),
            },
            CredentialStatus::Active,
            None,
            vec![],
        )
        .unwrap();
        assert_eq!(credential_anchor(&dir, &member), Some(aid("s1")));
        // 管理员关系没有付款账户，也没法绑成员凭证；成员关系不存在同样是 None
        let ghost = Credential::new(
            CredentialId::new("c-g").unwrap(),
            CredentialBinding::Member {
                membership: crate::test_support::mid("m-ghost"),
            },
            CredentialStatus::Active,
            None,
            vec![],
        )
        .unwrap();
        assert_eq!(credential_anchor(&dir, &ghost), None);
        assert!(!would_disable_caller(&dir, &ghost, &aid("eduai")));
    }

    #[test]
    fn delegation_errors_map_to_stable_reasons() {
        let unit_mismatch = DelegationError::Money(MoneyError::UnitMismatch {
            left: Unit::Thb,
            right: Unit::Usd,
        });
        assert_eq!(
            ControlReason::from(&unit_mismatch),
            ControlReason::AmountUnitMismatch
        );
        assert_eq!(
            ControlReason::from(&DelegationError::Money(MoneyError::Overflow)),
            ControlReason::InvalidAmount
        );
        assert_eq!(
            ControlReason::from(&DelegationError::UnknownAccount(aid("x"))),
            ControlReason::OutOfScope
        );
        assert_eq!(
            ControlReason::from(&DelegationError::ScopeOutsideBinding {
                subtree: aid("a"),
                binding: aid("b"),
            }),
            ControlReason::ScopeOutsideBinding
        );
        assert_eq!(
            ControlReason::from(&DelegationError::ExceedsGrantor {
                requested: scope(ControlOp::Read, "eduai", None),
            }),
            ControlReason::ScopeExceedsGrantor
        );
        assert_eq!(
            ControlReason::from(&DelegationError::IssuerNotUsable(
                ResolveError::CredentialRevoked(CredentialId::new("c").unwrap())
            )),
            ControlReason::CredentialRevoked
        );
    }

    // ---------- 一个和 sample_directory 不同形状的夹具：两个互不相干的组织 ----------

    #[test]
    fn two_organizations_under_the_platform_cannot_reach_each_other() {
        let tree = AccountTree::build(vec![
            active("platform", AccountKind::Platform, None),
            active("org-a", AccountKind::Org, Some("platform")),
            active("org-b", AccountKind::Org, Some("platform")),
            active("a-1", AccountKind::Member, Some("org-a")),
            active("b-1", AccountKind::Member, Some("org-b")),
        ])
        .unwrap();
        let dir = sample_directory_with(tree, |parts| {
            parts
                .principals
                .push(principal("pa", crate::account::PrincipalStatus::Active));
            parts.memberships.clear();
            parts.memberships.push(membership(
                "m-a",
                "pa",
                "org-a",
                MembershipRole::Member,
                Some("a-1"),
            ));
        });
        let a = active_service(
            "org-a",
            ControlOp::ALL
                .into_iter()
                .map(|op| AdminScope {
                    op,
                    subtree: aid("org-a"),
                    max_amount: None,
                })
                .collect(),
        );
        for op in ControlOp::ALL {
            assert_eq!(
                authorize(&dir, &a, NOW, op, &aid("a-1"), None),
                Ok(()),
                "{op:?}"
            );
            assert_eq!(
                authorize(&dir, &a, NOW, op, &aid("b-1"), None),
                Err(ControlReason::OutOfScope),
                "{op:?}"
            );
            assert_eq!(
                authorize(&dir, &a, NOW, op, &aid("org-b"), None),
                Err(ControlReason::OutOfScope),
                "{op:?}"
            );
            assert_eq!(
                authorize(&dir, &a, NOW, op, &aid("platform"), None),
                Err(ControlReason::OutOfScope),
                "{op:?}"
            );
        }
    }
}
