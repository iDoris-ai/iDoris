//! 凭证、付款上下文、权限下发（设计稿 §3、§9 控制面、评审 H1/H5）。
//!
//! # 付款上下文由凭证决定，请求不能自己指定付款方
//!
//! 设计稿 §3：「一次请求的付款上下文由**凭证**决定，再加上开通方提交的、经过校验的成员 id。
//! 请求本身不能自己指定由谁付款。」落到类型上：
//!
//! - [`Credential`] 只有**一个**绑定（[`CredentialBinding`]），所以「每次请求只落到一个付款上下文」
//!   是结构保证，不是约定。
//! - [`resolve_payment_context`] 是得到 [`PaymentContext`] 的唯一入口。请求侧能提供的**唯一**输入
//!   是 `asserted_member: Option<&PrincipalId>`，而且它只对 `Service` 凭证有意义，并且要在
//!   [`Directory`] 里验证过才算数。想指定一个账户 id？类型不允许：
//!
//! ```compile_fail
//! use idoris_compute_contracts::account::Directory;
//! use idoris_compute_contracts::credential::{Credential, resolve_payment_context};
//! use idoris_compute_contracts::ids::{AccountId, TimestampMs};
//! fn f(dir: &Directory, cred: &Credential) {
//!     let wanted_payer = AccountId::new("someone-elses-account").unwrap();
//!     // 第三个参数是 `Option<&PrincipalId>`，传 `&AccountId` 编译不过
//!     let _ = resolve_payment_context(dir, cred, Some(&wanted_payer), TimestampMs(0));
//! }
//! ```
//!
//! 三种凭证（设计稿 §3 例子）：
//!
//! | [`CredentialBinding`] | 例子 | 付款账户 |
//! |---|---|---|
//! | `Service { account }` | eduai 后端的服务凭证 | 请求必须带一个成员（主体）id；付款账户是该主体在 `account`（须是 `Org`）子树里**唯一**的、有效的可付款账户（成员子账户，或担保人自己的账户） |
//! | `Node { account }` | 组织节点调用云端档的凭证 | 就是 `account`（须是 `Org`）本身；**不允许**带成员 id（云端只认组织付款账户，设计稿 §9） |
//! | `Member { membership }` | 成员子凭证 / Agent24 个人云账号 | 该成员关系的付款账户（成员角色或担保人角色；管理员关系没有付款账户，不能绑）；若带了成员 id，必须就是自己 |
//!
//! # 下发的权限不能超过授权者本人
//!
//! [`AdminScope`] 是「操作 × 资源范围」（设计稿 §9 控制面）：能对哪棵子树做什么，以及（对额度下发）
//! 最多能下发多少。[`check_scope_delegation`] 保证下发出去的范围是授权者范围的**子集**：
//! 操作相同、子树在授权者子树之内、额度上限不高于授权者。[`check_credential_issuance`] 把它用在
//! 「签发一张新凭证」上，这正是 `key_issue.rs:122` 现在拒绝非空 `admin_scopes` 的原因要解决的问题。

use serde::{Deserialize, Serialize};

use crate::account::{
    AccountKind, AccountStatus, Directory, MembershipRole, MembershipStatus, PrincipalStatus,
};
use crate::ids::{AccountId, CredentialId, MembershipId, PrincipalId, TimestampMs};
use crate::money::{Money, MoneyError, Unit};
use crate::reason::ReasonCode;

/// 凭证状态。`Frozen` 是 overage 超出授信时系统自动冻结（设计稿 §5）；`Revoked` 是撤销。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Active,
    Frozen,
    Revoked,
}

/// 凭证绑定到什么。只有一个，所以每次请求只落到一个付款上下文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialBinding {
    /// 开通方的服务凭证（eduai 后端）：绑定到租户 `Org`，每次请求提交假名成员 id。
    Service { account: AccountId },
    /// 组织节点凭证：绑定到组织付款账户，直接付款。
    Node { account: AccountId },
    /// 成员子凭证：绑定到一个成员关系。
    Member { membership: MembershipId },
}

/// 控制面操作。S2 可以按需**加**变体，不能改已有的线上字符串（前 7 个是 S0 冻结的，后 3 个是 S2 加的）。
///
/// 每个操作的资源范围（它的 `subtree` 要覆盖哪个账户）见 [`crate::control`] 模块文档的表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOp {
    CreateAccount,
    SetAccountStatus,
    ManageMembership,
    IssueCredential,
    RevokeCredential,
    FreezeCredential,
    GrantAllocation,
    /// S2：登记主体（开通方提供的假名 id）。资源范围 = 主体登记在哪个组织名下。
    CreatePrincipal,
    /// S2：撤销主体（同意撤回，设计稿 §7）。资源范围 = 该主体全部成员关系所在的账户。
    RevokePrincipal,
    /// S2：只读查询。资源范围 = 被查询的账户。
    Read,
}

impl ControlOp {
    /// 全部操作，顺序固定（= 变体声明顺序）。测试用它保证授权矩阵没有漏掉任何一个操作。
    pub const ALL: [ControlOp; 10] = [
        ControlOp::CreateAccount,
        ControlOp::SetAccountStatus,
        ControlOp::ManageMembership,
        ControlOp::IssueCredential,
        ControlOp::RevokeCredential,
        ControlOp::FreezeCredential,
        ControlOp::GrantAllocation,
        ControlOp::CreatePrincipal,
        ControlOp::RevokePrincipal,
        ControlOp::Read,
    ];

    /// 线上字符串（与 `serde` 形式一致，也是数据库里 `credential_admin_scopes.op` 的写法）。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            ControlOp::CreateAccount => "create_account",
            ControlOp::SetAccountStatus => "set_account_status",
            ControlOp::ManageMembership => "manage_membership",
            ControlOp::IssueCredential => "issue_credential",
            ControlOp::RevokeCredential => "revoke_credential",
            ControlOp::FreezeCredential => "freeze_credential",
            ControlOp::GrantAllocation => "grant_allocation",
            ControlOp::CreatePrincipal => "create_principal",
            ControlOp::RevokePrincipal => "revoke_principal",
            ControlOp::Read => "read",
        }
    }
}

/// 控制面权限：能对 `subtree`（含）下的账户做 `op`。`op == GrantAllocation` 时还可以带
/// `max_amount`：这个权限一次最多能下发多少额度（`None` = 不设上限）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminScope {
    pub op: ControlOp,
    pub subtree: AccountId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_amount: Option<Money>,
}

/// 凭证构造/反序列化时的校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("member credentials are data-plane only and may not carry admin scopes")]
    MemberCredentialWithAdminScopes,
    #[error("admin scope max_amount is only meaningful for grant_allocation, not {op:?}")]
    AmountOnNonGrantOp { op: ControlOp },
    #[error("admin scope max_amount must not be negative: {micros} micro-{unit}")]
    NegativeScopeAmount { unit: Unit, micros: i64 },
}

/// 凭证。注意这里**没有密钥**：密钥只在签发时出现一次，之后只存哈希，不属于契约类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "CredentialWire")]
pub struct Credential {
    id: CredentialId,
    binding: CredentialBinding,
    status: CredentialStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<TimestampMs>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    admin_scopes: Vec<AdminScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialWire {
    id: CredentialId,
    binding: CredentialBinding,
    status: CredentialStatus,
    #[serde(default)]
    expires_at: Option<TimestampMs>,
    #[serde(default)]
    admin_scopes: Vec<AdminScope>,
}

impl TryFrom<CredentialWire> for Credential {
    type Error = CredentialError;
    fn try_from(wire: CredentialWire) -> Result<Self, CredentialError> {
        Credential::new(
            wire.id,
            wire.binding,
            wire.status,
            wire.expires_at,
            wire.admin_scopes,
        )
    }
}

impl Credential {
    /// 校验：成员凭证不带管理权限；`max_amount` 只给 `grant_allocation`、且非负。
    pub fn new(
        id: CredentialId,
        binding: CredentialBinding,
        status: CredentialStatus,
        expires_at: Option<TimestampMs>,
        admin_scopes: Vec<AdminScope>,
    ) -> Result<Self, CredentialError> {
        if matches!(binding, CredentialBinding::Member { .. }) && !admin_scopes.is_empty() {
            return Err(CredentialError::MemberCredentialWithAdminScopes);
        }
        for scope in &admin_scopes {
            if let Some(amount) = &scope.max_amount {
                if scope.op != ControlOp::GrantAllocation {
                    return Err(CredentialError::AmountOnNonGrantOp { op: scope.op });
                }
                if amount.micros < 0 {
                    return Err(CredentialError::NegativeScopeAmount {
                        unit: amount.unit.clone(),
                        micros: amount.micros,
                    });
                }
            }
        }
        Ok(Self {
            id,
            binding,
            status,
            expires_at,
            admin_scopes,
        })
    }

    pub fn id(&self) -> &CredentialId {
        &self.id
    }

    pub fn binding(&self) -> &CredentialBinding {
        &self.binding
    }

    pub fn status(&self) -> CredentialStatus {
        self.status
    }

    pub fn expires_at(&self) -> Option<TimestampMs> {
        self.expires_at
    }

    pub fn admin_scopes(&self) -> &[AdminScope] {
        &self.admin_scopes
    }

    /// 凭证本身（不看账户、成员）此刻是否可用。检查顺序：撤销 → 冻结 → 过期。
    /// 过期边界：`now >= expires_at` 即过期（右端开）。
    pub fn check_usable_at(&self, now: TimestampMs) -> Result<(), ResolveError> {
        match self.status {
            CredentialStatus::Revoked => {
                return Err(ResolveError::CredentialRevoked(self.id.clone()));
            }
            CredentialStatus::Frozen => {
                return Err(ResolveError::CredentialFrozen(self.id.clone()));
            }
            CredentialStatus::Active => {}
        }
        if let Some(expires_at) = self.expires_at
            && now >= expires_at
        {
            return Err(ResolveError::CredentialExpired {
                id: self.id.clone(),
                expires_at,
            });
        }
        Ok(())
    }
}

/// 解析出来的、这一次请求唯一的付款上下文。只能由 [`resolve_payment_context`] 产生
/// （字段私有，没有公开构造函数）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentContext {
    credential: CredentialId,
    payer: AccountId,
    unit: Unit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    principal: Option<PrincipalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    membership: Option<MembershipId>,
}

impl PaymentContext {
    pub fn credential(&self) -> &CredentialId {
        &self.credential
    }

    /// 付款账户：准入沿它的所有祖先检查（[`crate::admission`]）。
    pub fn payer(&self) -> &AccountId {
        &self.payer
    }

    /// 付款账户的单位。预留、计价的金额必须是这个单位。
    pub fn unit(&self) -> &Unit {
        &self.unit
    }

    pub fn principal(&self) -> Option<&PrincipalId> {
        self.principal.as_ref()
    }

    pub fn membership(&self) -> Option<&MembershipId> {
        self.membership.as_ref()
    }
}

/// 付款上下文解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("credential {0} is revoked")]
    CredentialRevoked(CredentialId),
    #[error("credential {0} is frozen")]
    CredentialFrozen(CredentialId),
    #[error("credential {id} expired at {expires_at:?}")]
    CredentialExpired {
        id: CredentialId,
        expires_at: TimestampMs,
    },
    #[error("credential is bound to unknown account {0}")]
    UnknownAccount(AccountId),
    #[error("credential is bound to unknown membership {0}")]
    UnknownMembership(MembershipId),
    #[error("{binding} credential cannot be bound to {target}")]
    InvalidBinding {
        binding: &'static str,
        target: String,
    },
    #[error("a service credential must assert a member id")]
    MemberRequired,
    #[error("this credential kind may not assert a member id")]
    MemberAssertionNotAllowed,
    #[error("the asserted member is not in this credential's scope")]
    MemberNotInScope,
    #[error("the asserted member is not the credential's own member")]
    MemberMismatch,
    #[error("the asserted member maps to {count} payment accounts; refusing to guess")]
    AmbiguousPaymentContext { count: usize },
    #[error("principal {0} has been revoked")]
    PrincipalRevoked(PrincipalId),
    #[error("membership {0} has been revoked")]
    MembershipRevoked(MembershipId),
    #[error("account {account} is {status:?}")]
    AccountInactive {
        account: AccountId,
        status: AccountStatus,
    },
}

impl ResolveError {
    /// 对应的稳定原因码。撤销、冻结、过期各有自己的码；其余（成员 id 缺失/越界/歧义、绑定不合法等）
    /// 都是「付款上下文无法唯一确定」，统一 `payment_context_invalid`。
    pub fn reason_code(&self) -> ReasonCode {
        match self {
            ResolveError::CredentialRevoked(_) => ReasonCode::CredentialRevoked,
            ResolveError::CredentialFrozen(_) => ReasonCode::CredentialFrozen,
            ResolveError::CredentialExpired { .. } => ReasonCode::CredentialExpired,
            ResolveError::PrincipalRevoked(_) => ReasonCode::PrincipalRevoked,
            ResolveError::MembershipRevoked(_) => ReasonCode::MembershipRevoked,
            ResolveError::AccountInactive { .. } => ReasonCode::AccountInactive,
            ResolveError::UnknownAccount(_)
            | ResolveError::UnknownMembership(_)
            | ResolveError::InvalidBinding { .. }
            | ResolveError::MemberRequired
            | ResolveError::MemberAssertionNotAllowed
            | ResolveError::MemberNotInScope
            | ResolveError::MemberMismatch
            | ResolveError::AmbiguousPaymentContext { .. } => ReasonCode::PaymentContextInvalid,
        }
    }
}

/// 成员关系的角色是否有付款账户：成员（学生、Agent24 实例）和担保人（老师自己备课也要花钱）有，
/// 管理员没有。
fn is_payable_role(role: MembershipRole) -> bool {
    matches!(role, MembershipRole::Member | MembershipRole::Sponsor)
}

/// 由凭证（加上经校验的成员 id）得到这次请求唯一的付款上下文。
///
/// 检查顺序固定（所以同一份坏数据永远报同一个错）：
///
/// 1. 凭证本身：撤销 → 冻结 → 过期（[`Credential::check_usable_at`]）；
/// 2. 按绑定种类解析付款账户（见模块文档的表），其中：成员 id 缺失/多余/越界/不匹配，
///    主体已撤销，成员关系已撤销，同一主体在范围内有多个有效的成员子账户（歧义）。
///    服务凭证**先判范围、再看主体状态**：不在凭证范围里的主体（不管存在与否、是否已撤销）一律是
///    `MemberNotInScope`，所以一个组织的凭证探测不到别的组织的主体有没有被撤销；
/// 3. 付款账户本身及其**所有祖先**都必须是 `Active`（冻结/关闭连带整个子树）。
///
/// 请求侧能提供的唯一输入是 `asserted_member`；它没有任何办法选择别的付款账户。
pub fn resolve_payment_context(
    dir: &Directory,
    credential: &Credential,
    asserted_member: Option<&PrincipalId>,
    now: TimestampMs,
) -> Result<PaymentContext, ResolveError> {
    credential.check_usable_at(now)?;
    let tree = dir.tree();

    let (payer, principal, membership) = match credential.binding() {
        CredentialBinding::Service { account } => {
            let anchor = tree
                .get(account)
                .ok_or_else(|| ResolveError::UnknownAccount(account.clone()))?;
            if anchor.kind != AccountKind::Org {
                return Err(ResolveError::InvalidBinding {
                    binding: "service",
                    target: format!("{:?} account {}", anchor.kind, anchor.id),
                });
            }
            let who = asserted_member.ok_or(ResolveError::MemberRequired)?;
            let Some(principal) = dir.principal(who) else {
                // 不区分「主体不存在」和「不在范围内」，避免被拿来探测主体是否存在。
                return Err(ResolveError::MemberNotInScope);
            };
            // 该主体在 `anchor` 子树里的全部「可付款」成员关系（成员或担保人角色，都有付款账户）
            let candidates: Vec<_> = dir
                .memberships_of(who)
                .filter(|m| is_payable_role(m.role))
                .filter(|m| {
                    m.account
                        .as_ref()
                        .is_some_and(|a| tree.is_descendant_or_self(a, &anchor.id))
                })
                .collect();
            // **先判范围，再看主体有没有被撤销**：不在这张凭证范围里的主体，不管存在与否、有没有被撤销，答案都是
            // 同一个 `MemberNotInScope`。否则一个组织的服务凭证断言别的组织的主体，就能靠 `principal_revoked`
            // 和 `payment_context_invalid` 的区别探测它有没有被撤销（跨租户的状态预言）。
            if candidates.is_empty() {
                return Err(ResolveError::MemberNotInScope);
            }
            if principal.status == PrincipalStatus::Revoked {
                return Err(ResolveError::PrincipalRevoked(principal.id.clone()));
            }
            let active: Vec<_> = candidates
                .iter()
                .filter(|m| m.status == MembershipStatus::Active)
                .collect();
            match active.as_slice() {
                [] => {
                    // 范围内有成员关系，但没有一个有效：全部已撤销
                    return Err(match candidates.first() {
                        Some(revoked) => ResolveError::MembershipRevoked(revoked.id.clone()),
                        None => ResolveError::MemberNotInScope, // 上面已排除空，不会走到
                    });
                }
                [only] => match &only.account {
                    Some(payer) => (payer.clone(), Some(who.clone()), Some(only.id.clone())),
                    None => return Err(ResolveError::MemberNotInScope), // 上面已过滤，不会走到
                },
                many => {
                    return Err(ResolveError::AmbiguousPaymentContext { count: many.len() });
                }
            }
        }
        CredentialBinding::Node { account } => {
            let anchor = tree
                .get(account)
                .ok_or_else(|| ResolveError::UnknownAccount(account.clone()))?;
            if anchor.kind != AccountKind::Org {
                return Err(ResolveError::InvalidBinding {
                    binding: "node",
                    target: format!("{:?} account {}", anchor.kind, anchor.id),
                });
            }
            if asserted_member.is_some() {
                return Err(ResolveError::MemberAssertionNotAllowed);
            }
            (anchor.id.clone(), None, None)
        }
        CredentialBinding::Member { membership } => {
            let m = dir
                .membership(membership)
                .ok_or_else(|| ResolveError::UnknownMembership(membership.clone()))?;
            let Some(payer) = &m.account else {
                return Err(ResolveError::InvalidBinding {
                    binding: "member",
                    target: format!("membership {} without a payment account", m.id),
                });
            };
            if !is_payable_role(m.role) {
                return Err(ResolveError::InvalidBinding {
                    binding: "member",
                    target: format!("{:?} membership {}", m.role, m.id),
                });
            }
            if let Some(who) = asserted_member
                && *who != m.principal
            {
                return Err(ResolveError::MemberMismatch);
            }
            if let Some(principal) = dir.principal(&m.principal)
                && principal.status == PrincipalStatus::Revoked
            {
                return Err(ResolveError::PrincipalRevoked(principal.id.clone()));
            }
            if m.status == MembershipStatus::Revoked {
                return Err(ResolveError::MembershipRevoked(m.id.clone()));
            }
            (payer.clone(), Some(m.principal.clone()), Some(m.id.clone()))
        }
    };

    if let Some((account, status)) = tree
        .inactive_cause(&payer)
        .map_err(|_| ResolveError::UnknownAccount(payer.clone()))?
    {
        return Err(ResolveError::AccountInactive { account, status });
    }
    let unit = tree
        .get(&payer)
        .map(|a| a.unit.clone())
        .ok_or_else(|| ResolveError::UnknownAccount(payer.clone()))?;
    Ok(PaymentContext {
        credential: credential.id().clone(),
        payer,
        unit,
        principal,
        membership,
    })
}

/// 权限下发错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegationError {
    #[error("requested scope {requested:?} is not covered by the grantor's own scopes")]
    ExceedsGrantor { requested: AdminScope },
    #[error("scope subtree {subtree} is outside the credential's own binding {binding}")]
    ScopeOutsideBinding {
        subtree: AccountId,
        binding: AccountId,
    },
    #[error("unknown account {0}")]
    UnknownAccount(AccountId),
    #[error("unknown membership {0}")]
    UnknownMembership(MembershipId),
    #[error("issuer credential is not usable: {0}")]
    IssuerNotUsable(ResolveError),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// `grantor` 这一条权限是否完全覆盖 `requested`（操作相同、子树包含、额度上限不低于）。
/// 额度上限的单位不同返回 `Err`（不同单位不可比）。
fn scope_covers(
    dir: &Directory,
    grantor: &AdminScope,
    requested: &AdminScope,
) -> Result<bool, MoneyError> {
    if grantor.op != requested.op {
        return Ok(false);
    }
    if !dir
        .tree()
        .is_descendant_or_self(&requested.subtree, &grantor.subtree)
    {
        return Ok(false);
    }
    match (&requested.max_amount, &grantor.max_amount) {
        (_, None) => Ok(true),
        (None, Some(_)) => Ok(false),
        (Some(wanted), Some(allowed)) => Ok(wanted.try_cmp(allowed)?.is_le()),
    }
}

/// 下发的权限必须是授权者权限的**子集**。
///
/// `requested` 被授权者 `grantor_scopes` 中任意一条覆盖即可；都不覆盖返回
/// [`DelegationError::ExceedsGrantor`]。如果因为额度上限单位不同而无法比较，且没有任何一条能覆盖，
/// 返回那个单位不一致错误，而不是默默当作「不覆盖」。
pub fn check_scope_delegation(
    dir: &Directory,
    grantor_scopes: &[AdminScope],
    requested: &AdminScope,
) -> Result<(), DelegationError> {
    if dir.tree().get(&requested.subtree).is_none() {
        return Err(DelegationError::UnknownAccount(requested.subtree.clone()));
    }
    let mut first_error: Option<MoneyError> = None;
    for grantor in grantor_scopes {
        match scope_covers(dir, grantor, requested) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    match first_error {
        Some(err) => Err(DelegationError::Money(err)),
        None => Err(DelegationError::ExceedsGrantor {
            requested: requested.clone(),
        }),
    }
}

/// 签发一张新凭证：授权者必须有资格，新凭证的权限不得超过授权者本人。
///
/// 1. 授权者凭证此刻可用（未撤销、未冻结、未过期）；
/// 2. 授权者有 `issue_credential` 权限，覆盖新凭证绑定的账户；
/// 3. 新凭证每条 `admin_scopes` 都是授权者权限的子集（[`check_scope_delegation`]）；
/// 4. 新凭证每条权限的子树都在它自己绑定的账户之内（`Service`/`Node`）——凭证不能管到自己绑定范围之外。
///
/// 成员凭证不带管理权限，由 [`Credential::new`] 保证。
pub fn check_credential_issuance(
    dir: &Directory,
    issuer: &Credential,
    new_credential: &Credential,
    now: TimestampMs,
) -> Result<(), DelegationError> {
    issuer
        .check_usable_at(now)
        .map_err(DelegationError::IssuerNotUsable)?;
    let anchor: AccountId = match new_credential.binding() {
        CredentialBinding::Service { account } | CredentialBinding::Node { account } => {
            account.clone()
        }
        CredentialBinding::Member { membership } => dir
            .membership(membership)
            .ok_or_else(|| DelegationError::UnknownMembership(membership.clone()))?
            .account
            .clone()
            .ok_or_else(|| DelegationError::UnknownMembership(membership.clone()))?,
    };
    let issue = AdminScope {
        op: ControlOp::IssueCredential,
        subtree: anchor.clone(),
        max_amount: None,
    };
    check_scope_delegation(dir, issuer.admin_scopes(), &issue)?;
    for scope in new_credential.admin_scopes() {
        check_scope_delegation(dir, issuer.admin_scopes(), scope)?;
        if !dir.tree().is_descendant_or_self(&scope.subtree, &anchor) {
            return Err(DelegationError::ScopeOutsideBinding {
                subtree: scope.subtree.clone(),
                binding: anchor,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::account::AccountTree;
    use crate::test_support::{
        DirectoryParts, active, aid, mid, pid, principal, sample_accounts, sample_directory,
        sample_directory_with, sample_tree,
    };

    fn cid(s: &str) -> CredentialId {
        CredentialId::new(s).unwrap()
    }

    fn cred(name: &str, binding: CredentialBinding) -> Credential {
        Credential::new(cid(name), binding, CredentialStatus::Active, None, vec![]).unwrap()
    }

    fn service(name: &str, account: &str) -> Credential {
        cred(
            name,
            CredentialBinding::Service {
                account: aid(account),
            },
        )
    }

    fn node(name: &str, account: &str) -> Credential {
        cred(
            name,
            CredentialBinding::Node {
                account: aid(account),
            },
        )
    }

    fn member(name: &str, membership: &str) -> Credential {
        cred(
            name,
            CredentialBinding::Member {
                membership: mid(membership),
            },
        )
    }

    const NOW: TimestampMs = TimestampMs(1_000);

    fn resolve(
        dir: &Directory,
        credential: &Credential,
        asserted: Option<&str>,
    ) -> Result<PaymentContext, ResolveError> {
        let asserted = asserted.map(pid);
        resolve_payment_context(dir, credential, asserted.as_ref(), NOW)
    }

    // ---------- 每次请求只落到一个付款上下文，由凭证决定 ----------

    #[test]
    fn service_credential_resolves_the_asserted_members_account() {
        let dir = sample_directory();
        let c = service("c-edu", "eduai");
        let ctx = resolve(&dir, &c, Some("kid1")).unwrap();
        assert_eq!(ctx.payer(), &aid("s1")); // 学生挂在担保人 teacher 下，仍在 eduai 子树里
        assert_eq!(ctx.credential(), &cid("c-edu"));
        assert_eq!(ctx.principal(), Some(&pid("kid1")));
        assert_eq!(ctx.membership(), Some(&mid("m-kid1")));
        assert_eq!(ctx.unit(), &Unit::Thb);
        let ctx2 = resolve(&dir, &c, Some("kid2")).unwrap();
        assert_eq!(ctx2.payer(), &aid("s2"));
        // 担保人（老师）自己用，从她自己的担保人账户付；她同时还有一个没有付款账户的管理员关系，不影响
        let teacher = resolve(&dir, &c, Some("tch")).unwrap();
        assert_eq!(teacher.payer(), &aid("teacher"));
        assert_eq!(teacher.membership(), Some(&mid("m-tch")));
    }

    #[test]
    fn service_credential_cannot_reach_members_outside_its_subtree() {
        let dir = sample_directory();
        let c = service("c-edu", "eduai");
        // adult 的成员子账户 p1 挂在平台下，不在 eduai 子树里
        assert_eq!(
            resolve(&dir, &c, Some("adult")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
        // 只有管理员关系、没有付款账户的主体：不是付款方
        let admin_only = sample_directory_with(sample_tree(), |parts| {
            parts
                .principals
                .push(principal("boss", PrincipalStatus::Active));
            parts.memberships.push(crate::test_support::membership(
                "m-boss",
                "boss",
                "eduai",
                MembershipRole::Admin,
                None,
            ));
        });
        assert_eq!(
            resolve(&admin_only, &c, Some("boss")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
        // 不存在的主体，和「不在范围」同一个错误（不泄露存在性）
        assert_eq!(
            resolve(&dir, &c, Some("ghost")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
        // 把账户 id 当主体 id 塞进来：不会被当成账户用，只是一个不存在的主体
        assert_eq!(
            resolve(&dir, &c, Some("s2")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
        assert_eq!(
            resolve(&dir, &c, Some("platform")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
    }

    #[test]
    fn service_credential_must_assert_a_member() {
        let dir = sample_directory();
        assert_eq!(
            resolve(&dir, &service("c", "eduai"), None).unwrap_err(),
            ResolveError::MemberRequired
        );
    }

    #[test]
    fn node_credential_pays_from_the_org_account_and_rejects_member_assertions() {
        let dir = sample_directory();
        let c = node("c-node", "eduai");
        let ctx = resolve(&dir, &c, None).unwrap();
        assert_eq!(ctx.payer(), &aid("eduai"));
        assert_eq!(ctx.principal(), None);
        assert_eq!(ctx.membership(), None);
        // 云端只认组织付款账户：带了成员 id 就是错，而不是悄悄忽略
        assert_eq!(
            resolve(&dir, &c, Some("kid1")).unwrap_err(),
            ResolveError::MemberAssertionNotAllowed
        );
    }

    #[test]
    fn member_credential_pays_from_its_own_member_account() {
        let dir = sample_directory();
        let c = member("c-kid", "m-kid1");
        assert_eq!(resolve(&dir, &c, None).unwrap().payer(), &aid("s1"));
        assert_eq!(resolve(&dir, &c, Some("kid1")).unwrap().payer(), &aid("s1"));
        // 冒用别人的成员 id：拒绝，不会改付款方
        assert_eq!(
            resolve(&dir, &c, Some("kid2")).unwrap_err(),
            ResolveError::MemberMismatch
        );
        // 个人云账号：成员挂在平台下
        let p = member("c-adult", "m-adult");
        assert_eq!(resolve(&dir, &p, None).unwrap().payer(), &aid("p1"));
        // 担保人的成员子凭证：从担保人账户付
        let t = member("c-tch", "m-tch");
        assert_eq!(
            resolve(&dir, &t, Some("tch")).unwrap().payer(),
            &aid("teacher")
        );
    }

    #[test]
    fn invalid_bindings_are_rejected() {
        let dir = sample_directory();
        // Service / Node 只能绑 Org
        for account in ["teacher", "s1", "platform", "pool"] {
            assert!(
                matches!(
                    resolve(&dir, &service("c", account), Some("kid1")).unwrap_err(),
                    ResolveError::InvalidBinding {
                        binding: "service",
                        ..
                    }
                ),
                "{account}"
            );
            assert!(
                matches!(
                    resolve(&dir, &node("c", account), None).unwrap_err(),
                    ResolveError::InvalidBinding {
                        binding: "node",
                        ..
                    }
                ),
                "{account}"
            );
        }
        // Member 凭证只能绑「可付款」的成员关系（成员或担保人角色）；管理员关系没有付款账户
        assert!(matches!(
            resolve(&dir, &member("c", "m-adm"), None).unwrap_err(),
            ResolveError::InvalidBinding {
                binding: "member",
                ..
            }
        ));
        // 未知账户 / 未知成员关系
        assert_eq!(
            resolve(&dir, &service("c", "ghost"), Some("kid1")).unwrap_err(),
            ResolveError::UnknownAccount(aid("ghost"))
        );
        assert_eq!(
            resolve(&dir, &node("c", "ghost"), None).unwrap_err(),
            ResolveError::UnknownAccount(aid("ghost"))
        );
        assert_eq!(
            resolve(&dir, &member("c", "ghost"), None).unwrap_err(),
            ResolveError::UnknownMembership(mid("ghost"))
        );
    }

    // ---------- 同一个主体属于两个组织 ----------

    /// 主体 dual 同时是 eduai 和 school-b 的成员，各有一个成员子账户。
    fn two_org_directory() -> Directory {
        use MembershipRole::Member;
        let mut accounts = sample_accounts();
        accounts.push(active("dual-edu", AccountKind::Member, Some("eduai")));
        accounts.push(active("dual-b", AccountKind::Member, Some("school-b")));
        let tree = AccountTree::build(accounts).unwrap();
        sample_directory_with(tree, |parts: &mut DirectoryParts| {
            parts
                .principals
                .push(principal("dual", PrincipalStatus::Active));
            parts.memberships.push(crate::test_support::membership(
                "m-dual-edu",
                "dual",
                "eduai",
                Member,
                Some("dual-edu"),
            ));
            parts.memberships.push(crate::test_support::membership(
                "m-dual-b",
                "dual",
                "school-b",
                Member,
                Some("dual-b"),
            ));
        })
    }

    #[test]
    fn one_principal_in_two_orgs_pays_from_the_account_of_the_credentials_org() {
        let dir = two_org_directory();
        let via_edu = resolve(&dir, &service("c-edu", "eduai"), Some("dual")).unwrap();
        let via_b = resolve(&dir, &service("c-b", "school-b"), Some("dual")).unwrap();
        assert_eq!(via_edu.payer(), &aid("dual-edu"));
        assert_eq!(via_b.payer(), &aid("dual-b"));
        assert_ne!(via_edu.payer(), via_b.payer());
        // 各自的成员子凭证也一样
        assert_eq!(
            resolve(&dir, &member("m1", "m-dual-edu"), None)
                .unwrap()
                .payer(),
            &aid("dual-edu")
        );
        assert_eq!(
            resolve(&dir, &member("m2", "m-dual-b"), None)
                .unwrap()
                .payer(),
            &aid("dual-b")
        );
        // 一个组织的服务凭证够不到另一个组织里的这个主体的账户
        let only_b_member = service("c-b", "school-b");
        assert_eq!(
            resolve(&dir, &only_b_member, Some("kid1")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
    }

    #[test]
    fn two_active_member_accounts_for_one_principal_in_one_subtree_is_ambiguous_not_guessed() {
        use MembershipRole::Member;
        let mut accounts = sample_accounts();
        accounts.push(active("s3", AccountKind::Member, Some("eduai")));
        let tree = AccountTree::build(accounts).unwrap();
        let dir = sample_directory_with(tree.clone(), |parts| {
            parts.memberships.push(crate::test_support::membership(
                "m-kid1-b",
                "kid1",
                "eduai",
                Member,
                Some("s3"),
            ));
        });
        assert_eq!(
            resolve(&dir, &service("c", "eduai"), Some("kid1")).unwrap_err(),
            ResolveError::AmbiguousPaymentContext { count: 2 }
        );
        // 其中一个被撤销：只剩一个有效的，不再歧义
        let dir = sample_directory_with(tree.clone(), |parts| {
            let mut extra =
                crate::test_support::membership("m-kid1-b", "kid1", "eduai", Member, Some("s3"));
            extra.status = MembershipStatus::Revoked;
            parts.memberships.push(extra);
        });
        assert_eq!(
            resolve(&dir, &service("c", "eduai"), Some("kid1"))
                .unwrap()
                .payer(),
            &aid("s1")
        );
        // 全都被撤销：报成员关系已撤销
        let dir = sample_directory_with(tree, |parts| {
            for m in &mut parts.memberships {
                if m.principal == pid("kid1") {
                    m.status = MembershipStatus::Revoked;
                }
            }
            let mut extra =
                crate::test_support::membership("m-kid1-b", "kid1", "eduai", Member, Some("s3"));
            extra.status = MembershipStatus::Revoked;
            parts.memberships.push(extra);
        });
        assert!(matches!(
            resolve(&dir, &service("c", "eduai"), Some("kid1")).unwrap_err(),
            ResolveError::MembershipRevoked(_)
        ));
    }

    // ---------- 撤销 / 冻结 / 过期 ----------

    #[test]
    fn credential_status_and_expiry_boundaries() {
        let dir = sample_directory();
        let mk = |status, expires: Option<i64>| {
            Credential::new(
                cid("c"),
                CredentialBinding::Member {
                    membership: mid("m-kid1"),
                },
                status,
                expires.map(TimestampMs),
                vec![],
            )
            .unwrap()
        };
        assert_eq!(
            resolve(&dir, &mk(CredentialStatus::Revoked, None), None).unwrap_err(),
            ResolveError::CredentialRevoked(cid("c"))
        );
        assert_eq!(
            resolve(&dir, &mk(CredentialStatus::Frozen, None), None).unwrap_err(),
            ResolveError::CredentialFrozen(cid("c"))
        );
        // 过期：now = 1_000
        assert!(resolve(&dir, &mk(CredentialStatus::Active, Some(1_001)), None).is_ok());
        assert_eq!(
            resolve(&dir, &mk(CredentialStatus::Active, Some(1_000)), None).unwrap_err(),
            ResolveError::CredentialExpired {
                id: cid("c"),
                expires_at: TimestampMs(1_000)
            }
        );
        assert!(resolve(&dir, &mk(CredentialStatus::Active, Some(999)), None).is_err());
        // 优先级：撤销 > 冻结 > 过期
        assert!(matches!(
            resolve(&dir, &mk(CredentialStatus::Revoked, Some(1)), None).unwrap_err(),
            ResolveError::CredentialRevoked(_)
        ));
        assert!(matches!(
            resolve(&dir, &mk(CredentialStatus::Frozen, Some(1)), None).unwrap_err(),
            ResolveError::CredentialFrozen(_)
        ));
    }

    #[test]
    fn a_service_credential_cannot_tell_a_revoked_foreign_principal_from_one_that_is_out_of_scope()
    {
        // PR-Daemon S0 F1：服务凭证断言了一个不在它范围里的主体。旧代码先看主体有没有被撤销：已撤销的返回
        // `principal_revoked`，没撤销的 / 不存在的返回 `payment_context_invalid`——对别的组织的主体是一个存在性 /
        // 状态预言（跨租户）。现在先判范围：不在范围里的主体，不管存在与否、有没有撤销，都是同一个答案。
        let revoked = sample_directory_with(sample_tree(), |parts| {
            for p in &mut parts.principals {
                if p.id == pid("adult") {
                    p.status = PrincipalStatus::Revoked;
                }
            }
        });
        let active = sample_directory();
        // school-b 的服务凭证；adult 只有 platform 下的成员关系，不在 school-b 的子树里
        let foreign = service("c-b", "school-b");
        let answers = [
            resolve(&revoked, &foreign, Some("adult")).unwrap_err(),
            resolve(&active, &foreign, Some("adult")).unwrap_err(),
            resolve(&active, &foreign, Some("nobody-at-all")).unwrap_err(),
        ];
        for answer in &answers {
            assert_eq!(answer, &ResolveError::MemberNotInScope);
            assert_eq!(answer.reason_code(), ReasonCode::PaymentContextInvalid);
        }
        // 范围里的主体被撤销，照旧是 principal_revoked（组织自己知道自己的学生）
        let in_scope = sample_directory_with(sample_tree(), |parts| {
            for p in &mut parts.principals {
                if p.id == pid("kid1") {
                    p.status = PrincipalStatus::Revoked;
                }
            }
        });
        assert_eq!(
            resolve(&in_scope, &service("c-edu", "eduai"), Some("kid1")).unwrap_err(),
            ResolveError::PrincipalRevoked(pid("kid1"))
        );
        // 同一个主体，别的组织的服务凭证：不在范围里
        assert_eq!(
            resolve(&in_scope, &foreign, Some("kid1")).unwrap_err(),
            ResolveError::MemberNotInScope
        );
    }

    #[test]
    fn revoking_the_principal_or_membership_invalidates_every_path_to_it() {
        // 主体被撤销（同意撤回）：服务凭证和成员凭证两条路都拒绝
        let dir = sample_directory_with(sample_tree(), |parts| {
            for p in &mut parts.principals {
                if p.id == pid("kid1") {
                    p.status = PrincipalStatus::Revoked;
                }
            }
        });
        assert_eq!(
            resolve(&dir, &service("c", "eduai"), Some("kid1")).unwrap_err(),
            ResolveError::PrincipalRevoked(pid("kid1"))
        );
        assert_eq!(
            resolve(&dir, &member("c", "m-kid1"), None).unwrap_err(),
            ResolveError::PrincipalRevoked(pid("kid1"))
        );
        // 别的学生不受影响
        assert!(resolve(&dir, &service("c", "eduai"), Some("kid2")).is_ok());
        // 成员关系被撤销
        let dir = sample_directory_with(sample_tree(), |parts| {
            for m in &mut parts.memberships {
                if m.id == mid("m-kid1") {
                    m.status = MembershipStatus::Revoked;
                }
            }
        });
        assert_eq!(
            resolve(&dir, &member("c", "m-kid1"), None).unwrap_err(),
            ResolveError::MembershipRevoked(mid("m-kid1"))
        );
        assert_eq!(
            resolve(&dir, &service("c", "eduai"), Some("kid1")).unwrap_err(),
            ResolveError::MembershipRevoked(mid("m-kid1"))
        );
    }

    #[test]
    fn a_frozen_or_closed_ancestor_blocks_the_whole_subtree() {
        let set = |name: &str, status: AccountStatus| {
            let mut accounts = sample_accounts();
            for a in &mut accounts {
                if a.id == aid(name) {
                    a.status = status;
                }
            }
            sample_directory_with(AccountTree::build(accounts).unwrap(), |_| {})
        };
        for (frozen, status) in [
            ("eduai", AccountStatus::Frozen),
            ("teacher", AccountStatus::Closed),
            ("s1", AccountStatus::Frozen),
            ("pool", AccountStatus::Frozen),
            ("platform", AccountStatus::Closed),
        ] {
            let dir = set(frozen, status);
            let err = resolve(&dir, &member("c", "m-kid1"), None).unwrap_err();
            assert_eq!(
                err,
                ResolveError::AccountInactive {
                    account: aid(frozen),
                    status
                },
                "{frozen}"
            );
            let err = resolve(&dir, &service("c", "eduai"), Some("kid1")).unwrap_err();
            assert_eq!(
                err,
                ResolveError::AccountInactive {
                    account: aid(frozen),
                    status
                },
                "{frozen}"
            );
        }
        // 冻结 teacher 子树，不影响 s2（直接挂 eduai）
        let dir = set("teacher", AccountStatus::Frozen);
        assert!(resolve(&dir, &service("c", "eduai"), Some("kid2")).is_ok());
        // 冻结的组织，节点凭证也不行
        let dir = set("eduai", AccountStatus::Frozen);
        assert!(matches!(
            resolve(&dir, &node("c", "eduai"), None).unwrap_err(),
            ResolveError::AccountInactive { .. }
        ));
    }

    #[test]
    fn every_resolve_error_maps_to_a_stable_reason_code() {
        let acct = aid("a");
        let cases: Vec<(ResolveError, ReasonCode)> = vec![
            (
                ResolveError::CredentialRevoked(cid("c")),
                ReasonCode::CredentialRevoked,
            ),
            (
                ResolveError::CredentialFrozen(cid("c")),
                ReasonCode::CredentialFrozen,
            ),
            (
                ResolveError::CredentialExpired {
                    id: cid("c"),
                    expires_at: TimestampMs(0),
                },
                ReasonCode::CredentialExpired,
            ),
            (
                ResolveError::PrincipalRevoked(pid("p")),
                ReasonCode::PrincipalRevoked,
            ),
            (
                ResolveError::MembershipRevoked(mid("m")),
                ReasonCode::MembershipRevoked,
            ),
            (
                ResolveError::AccountInactive {
                    account: acct.clone(),
                    status: AccountStatus::Frozen,
                },
                ReasonCode::AccountInactive,
            ),
            (
                ResolveError::UnknownAccount(acct),
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::UnknownMembership(mid("m")),
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::InvalidBinding {
                    binding: "node",
                    target: String::new(),
                },
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::MemberRequired,
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::MemberAssertionNotAllowed,
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::MemberNotInScope,
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::MemberMismatch,
                ReasonCode::PaymentContextInvalid,
            ),
            (
                ResolveError::AmbiguousPaymentContext { count: 2 },
                ReasonCode::PaymentContextInvalid,
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.reason_code(), code, "{error}");
            assert!(!error.reason_code().is_served());
        }
    }

    // ---------- 凭证类型本身 ----------

    #[test]
    fn credential_wire_shape_is_pinned() {
        let c = Credential::new(
            cid("c-edu"),
            CredentialBinding::Service {
                account: aid("eduai"),
            },
            CredentialStatus::Active,
            Some(TimestampMs(5)),
            vec![AdminScope {
                op: ControlOp::GrantAllocation,
                subtree: aid("eduai"),
                max_amount: Some(Money::new(Unit::Thb, 10_000_000)),
            }],
        )
        .unwrap();
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"id":"c-edu","binding":{"kind":"service","account":"eduai"},"status":"active","expires_at":5,"admin_scopes":[{"op":"grant_allocation","subtree":"eduai","max_amount":{"unit":"THB","micros":10000000}}]}"#
        );
        assert_eq!(serde_json::from_str::<Credential>(&json).unwrap(), c);
        let minimal = member("c-m", "m-kid1");
        assert_eq!(
            serde_json::to_string(&minimal).unwrap(),
            r#"{"id":"c-m","binding":{"kind":"member","membership":"m-kid1"},"status":"active"}"#
        );
        assert_eq!(
            serde_json::to_string(&node("c-n", "eduai")).unwrap(),
            r#"{"id":"c-n","binding":{"kind":"node","account":"eduai"},"status":"active"}"#
        );
    }

    #[test]
    fn credential_can_only_have_one_binding_and_no_unknown_fields() {
        // 想同时绑多个账户：没有这个字段
        let multi = r#"{"id":"c","binding":{"kind":"node","account":"a"},"accounts":["a","b"],"status":"active"}"#;
        assert!(serde_json::from_str::<Credential>(multi).is_err());
        // binding 里同时给 account 和 membership：字段多余
        let both = r#"{"id":"c","binding":{"kind":"node","account":"a","membership":"m"},"status":"active"}"#;
        assert!(serde_json::from_str::<Credential>(both).is_err());
        // 未知的绑定种类、未知状态
        assert!(
            serde_json::from_str::<Credential>(
                r#"{"id":"c","binding":{"kind":"root","account":"a"},"status":"active"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Credential>(
                r#"{"id":"c","binding":{"kind":"node","account":"a"},"status":"suspended"}"#
            )
            .is_err()
        );
        // 密钥不是契约的一部分：塞一个 secret 字段会被拒绝
        assert!(serde_json::from_str::<Credential>(
            r#"{"id":"c","binding":{"kind":"node","account":"a"},"status":"active","secret":"vk_x"}"#
        ).is_err());
    }

    #[test]
    fn member_credentials_cannot_carry_admin_scopes() {
        let scope = AdminScope {
            op: ControlOp::CreateAccount,
            subtree: aid("eduai"),
            max_amount: None,
        };
        assert_eq!(
            Credential::new(
                cid("c"),
                CredentialBinding::Member {
                    membership: mid("m")
                },
                CredentialStatus::Active,
                None,
                vec![scope.clone()],
            )
            .unwrap_err(),
            CredentialError::MemberCredentialWithAdminScopes
        );
        assert!(
            Credential::new(
                cid("c"),
                CredentialBinding::Service {
                    account: aid("eduai")
                },
                CredentialStatus::Active,
                None,
                vec![scope],
            )
            .is_ok()
        );
        let json = r#"{"id":"c","binding":{"kind":"member","membership":"m"},"status":"active","admin_scopes":[{"op":"create_account","subtree":"a"}]}"#;
        assert!(serde_json::from_str::<Credential>(json).is_err());
    }

    #[test]
    fn scope_amounts_are_only_for_grants_and_never_negative() {
        let mk = |op, amount: Money| {
            Credential::new(
                cid("c"),
                CredentialBinding::Service {
                    account: aid("eduai"),
                },
                CredentialStatus::Active,
                None,
                vec![AdminScope {
                    op,
                    subtree: aid("eduai"),
                    max_amount: Some(amount),
                }],
            )
        };
        assert_eq!(
            mk(ControlOp::CreateAccount, Money::new(Unit::Thb, 1)).unwrap_err(),
            CredentialError::AmountOnNonGrantOp {
                op: ControlOp::CreateAccount
            }
        );
        assert_eq!(
            mk(ControlOp::GrantAllocation, Money::new(Unit::Thb, -1)).unwrap_err(),
            CredentialError::NegativeScopeAmount {
                unit: Unit::Thb,
                micros: -1
            }
        );
        assert!(mk(ControlOp::GrantAllocation, Money::new(Unit::Thb, 0)).is_ok());
    }

    #[test]
    fn control_op_wire_strings_are_pinned() {
        for (op, text) in [
            (ControlOp::CreateAccount, "create_account"),
            (ControlOp::SetAccountStatus, "set_account_status"),
            (ControlOp::ManageMembership, "manage_membership"),
            (ControlOp::IssueCredential, "issue_credential"),
            (ControlOp::RevokeCredential, "revoke_credential"),
            (ControlOp::FreezeCredential, "freeze_credential"),
            (ControlOp::GrantAllocation, "grant_allocation"),
            // S2 新增
            (ControlOp::CreatePrincipal, "create_principal"),
            (ControlOp::RevokePrincipal, "revoke_principal"),
            (ControlOp::Read, "read"),
        ] {
            assert_eq!(serde_json::to_string(&op).unwrap(), format!("\"{text}\""));
            assert_eq!(op.as_str(), text);
            assert_eq!(
                serde_json::from_str::<ControlOp>(&format!("\"{text}\"")).unwrap(),
                op
            );
        }
        // ALL 与变体一一对应：没有重复，也没有遗漏（`as_str` 的 match 是穷尽的，所以新增变体会先在这里编译失败）
        let mut seen = std::collections::BTreeSet::new();
        for op in ControlOp::ALL {
            assert!(seen.insert(op.as_str()), "{op:?} listed twice");
        }
        assert_eq!(seen.len(), 10);
        assert!(serde_json::from_str::<ControlOp>("\"read_ledger\"").is_err());
    }

    #[test]
    fn an_amount_cap_is_only_allowed_on_grant_allocation_even_for_the_new_operations() {
        for op in ControlOp::ALL {
            let scope = AdminScope {
                op,
                subtree: aid("eduai"),
                max_amount: Some(Money::new(Unit::Thb, 1)),
            };
            let result = Credential::new(
                CredentialId::new("c-x").unwrap(),
                CredentialBinding::Service {
                    account: aid("eduai"),
                },
                CredentialStatus::Active,
                None,
                vec![scope],
            );
            assert_eq!(result.is_ok(), op == ControlOp::GrantAllocation, "{op:?}");
        }
    }

    #[test]
    fn payment_context_wire_shape() {
        let dir = sample_directory();
        let ctx = resolve(&dir, &service("c-edu", "eduai"), Some("kid1")).unwrap();
        assert_eq!(
            serde_json::to_string(&ctx).unwrap(),
            r#"{"credential":"c-edu","payer":"s1","unit":"THB","principal":"kid1","membership":"m-kid1"}"#
        );
        let node_ctx = resolve(&dir, &node("c-n", "eduai"), None).unwrap();
        assert_eq!(
            serde_json::to_string(&node_ctx).unwrap(),
            r#"{"credential":"c-n","payer":"eduai","unit":"THB"}"#
        );
        assert_eq!(
            serde_json::from_str::<PaymentContext>(&serde_json::to_string(&ctx).unwrap()).unwrap(),
            ctx
        );
    }

    // ---------- 下发的权限不能超过授权者本人 ----------

    fn scope(op: ControlOp, subtree: &str, max: Option<i64>) -> AdminScope {
        AdminScope {
            op,
            subtree: aid(subtree),
            max_amount: max.map(|m| Money::new(Unit::Thb, m)),
        }
    }

    #[test]
    fn delegated_scope_must_be_a_subset_of_the_grantors_table() {
        use ControlOp::*;
        let dir = sample_directory();
        let grantor = vec![
            scope(GrantAllocation, "eduai", Some(1_000)),
            scope(CreateAccount, "eduai", None),
            scope(IssueCredential, "teacher", None),
        ];
        // (请求, 是否允许)
        let cases = [
            (scope(GrantAllocation, "eduai", Some(1_000)), true), // 刚好等于
            (scope(GrantAllocation, "eduai", Some(999)), true),
            (scope(GrantAllocation, "eduai", Some(0)), true),
            (scope(GrantAllocation, "eduai", Some(1_001)), false), // 额度多 1
            (scope(GrantAllocation, "eduai", None), false), // 授权者有上限，不能下发「无上限」
            (scope(GrantAllocation, "teacher", Some(500)), true), // 子树更小
            (scope(GrantAllocation, "s1", Some(500)), true),
            (scope(GrantAllocation, "school-b", Some(1)), false), // 别的子树
            (scope(GrantAllocation, "platform", Some(1)), false), // 子树更大（越权）
            (scope(CreateAccount, "eduai", None), true),
            (scope(CreateAccount, "s2", None), true),
            (scope(CreateAccount, "platform", None), false),
            (scope(RevokeCredential, "eduai", None), false), // 授权者没有这个操作
            (scope(IssueCredential, "s1", None), true),      // teacher 子树内
            (scope(IssueCredential, "s2", None), false),     // 不在 teacher 子树内
        ];
        for (requested, allowed) in cases {
            let result = check_scope_delegation(&dir, &grantor, &requested);
            assert_eq!(result.is_ok(), allowed, "{requested:?} => {result:?}");
            if !allowed {
                assert_eq!(
                    result.unwrap_err(),
                    DelegationError::ExceedsGrantor { requested }
                );
            }
        }
    }

    #[test]
    fn unlimited_grantor_covers_any_amount_and_empty_grantor_covers_nothing() {
        let dir = sample_directory();
        let unlimited = vec![scope(ControlOp::GrantAllocation, "eduai", None)];
        for max in [None, Some(0), Some(i64::MAX)] {
            assert!(
                check_scope_delegation(
                    &dir,
                    &unlimited,
                    &scope(ControlOp::GrantAllocation, "s1", max)
                )
                .is_ok()
            );
        }
        assert!(matches!(
            check_scope_delegation(&dir, &[], &scope(ControlOp::CreateAccount, "s1", None))
                .unwrap_err(),
            DelegationError::ExceedsGrantor { .. }
        ));
    }

    #[test]
    fn scope_amount_in_another_unit_is_rejected_not_silently_compared() {
        let dir = sample_directory();
        let grantor = vec![scope(ControlOp::GrantAllocation, "eduai", Some(1_000))];
        let usd = AdminScope {
            op: ControlOp::GrantAllocation,
            subtree: aid("s1"),
            max_amount: Some(Money::new(Unit::Usd, 1)),
        };
        assert_eq!(
            check_scope_delegation(&dir, &grantor, &usd).unwrap_err(),
            DelegationError::Money(MoneyError::UnitMismatch {
                left: Unit::Usd,
                right: Unit::Thb
            })
        );
        // 如果另一条权限能覆盖，单位不一致的那条不影响结果
        let two = vec![
            scope(ControlOp::GrantAllocation, "eduai", Some(1_000)),
            AdminScope {
                op: ControlOp::GrantAllocation,
                subtree: aid("eduai"),
                max_amount: Some(Money::new(Unit::Usd, 5)),
            },
        ];
        assert!(check_scope_delegation(&dir, &two, &usd).is_ok());
    }

    #[test]
    fn unknown_subtree_is_an_error() {
        let dir = sample_directory();
        assert_eq!(
            check_scope_delegation(&dir, &[], &scope(ControlOp::CreateAccount, "ghost", None))
                .unwrap_err(),
            DelegationError::UnknownAccount(aid("ghost"))
        );
    }

    fn admin_credential(name: &str, account: &str, scopes: Vec<AdminScope>) -> Credential {
        Credential::new(
            cid(name),
            CredentialBinding::Service {
                account: aid(account),
            },
            CredentialStatus::Active,
            None,
            scopes,
        )
        .unwrap()
    }

    #[test]
    fn issuing_a_credential_cannot_exceed_the_issuers_own_authority() {
        use ControlOp::*;
        let dir = sample_directory();
        let issuer = admin_credential(
            "issuer",
            "eduai",
            vec![
                scope(IssueCredential, "eduai", None),
                scope(GrantAllocation, "eduai", Some(1_000)),
                scope(CreateAccount, "eduai", None),
            ],
        );
        // 合法：下发一张只能给 teacher 子树发 500 的凭证
        let ok = admin_credential(
            "new1",
            "teacher",
            vec![scope(GrantAllocation, "teacher", Some(500))],
        );
        assert_eq!(check_credential_issuance(&dir, &issuer, &ok, NOW), Ok(()));
        // 额度超过授权者本人
        let too_much = admin_credential(
            "new2",
            "teacher",
            vec![scope(GrantAllocation, "teacher", Some(1_001))],
        );
        assert_eq!(
            check_credential_issuance(&dir, &issuer, &too_much, NOW),
            Err(DelegationError::ExceedsGrantor {
                requested: scope(GrantAllocation, "teacher", Some(1_001))
            })
        );
        // 子树超出授权者
        let wider = admin_credential(
            "new3",
            "eduai",
            vec![scope(CreateAccount, "platform", None)],
        );
        assert!(matches!(
            check_credential_issuance(&dir, &issuer, &wider, NOW).unwrap_err(),
            DelegationError::ExceedsGrantor { .. }
        ));
        // 授权者没有的操作
        let other_op = admin_credential(
            "new4",
            "teacher",
            vec![scope(RevokeCredential, "teacher", None)],
        );
        assert!(matches!(
            check_credential_issuance(&dir, &issuer, &other_op, NOW).unwrap_err(),
            DelegationError::ExceedsGrantor { .. }
        ));
        // 新凭证绑到授权者管不到的账户（school-b 不在 eduai 子树里）
        let outside = admin_credential("new5", "school-b", vec![]);
        assert!(matches!(
            check_credential_issuance(&dir, &issuer, &outside, NOW).unwrap_err(),
            DelegationError::ExceedsGrantor { .. }
        ));
        // 凭证不能管到自己绑定范围之外：绑 teacher，却要管整个 eduai
        let escapes =
            admin_credential("new6", "teacher", vec![scope(CreateAccount, "eduai", None)]);
        assert_eq!(
            check_credential_issuance(&dir, &issuer, &escapes, NOW),
            Err(DelegationError::ScopeOutsideBinding {
                subtree: aid("eduai"),
                binding: aid("teacher")
            })
        );
        // 成员凭证：授权者必须能管到该成员的账户
        let member_ok = member("new7", "m-kid1");
        assert_eq!(
            check_credential_issuance(&dir, &issuer, &member_ok, NOW),
            Ok(())
        );
        let member_outside = member("new8", "m-adult");
        assert!(matches!(
            check_credential_issuance(&dir, &issuer, &member_outside, NOW).unwrap_err(),
            DelegationError::ExceedsGrantor { .. }
        ));
        assert_eq!(
            check_credential_issuance(&dir, &issuer, &member("new9", "ghost"), NOW),
            Err(DelegationError::UnknownMembership(mid("ghost")))
        );
        // 没有 issue_credential 权限的授权者
        let no_issue = admin_credential("weak", "eduai", vec![scope(CreateAccount, "eduai", None)]);
        assert!(matches!(
            check_credential_issuance(&dir, &no_issue, &ok, NOW).unwrap_err(),
            DelegationError::ExceedsGrantor { .. }
        ));
    }

    #[test]
    fn an_unusable_issuer_cannot_issue() {
        let dir = sample_directory();
        let scopes = vec![scope(ControlOp::IssueCredential, "eduai", None)];
        let target = admin_credential("t", "teacher", vec![]);
        for (status, expires) in [
            (CredentialStatus::Revoked, None),
            (CredentialStatus::Frozen, None),
            (CredentialStatus::Active, Some(TimestampMs(1_000))),
        ] {
            let issuer = Credential::new(
                cid("issuer"),
                CredentialBinding::Service {
                    account: aid("eduai"),
                },
                status,
                expires,
                scopes.clone(),
            )
            .unwrap();
            assert!(matches!(
                check_credential_issuance(&dir, &issuer, &target, NOW).unwrap_err(),
                DelegationError::IssuerNotUsable(_)
            ));
        }
    }

    #[test]
    fn grant_amount_limits_are_per_credential_and_chain_downward() {
        // 授权链：平台级 → 租户级 → 担保人级，每一级上限只降不升
        use ControlOp::*;
        let dir = sample_directory();
        let root = vec![scope(GrantAllocation, "eduai", Some(10_000))];
        let mid_scope = scope(GrantAllocation, "teacher", Some(1_000));
        let low_scope = scope(GrantAllocation, "s1", Some(100));
        assert!(check_scope_delegation(&dir, &root, &mid_scope).is_ok());
        assert!(check_scope_delegation(&dir, std::slice::from_ref(&mid_scope), &low_scope).is_ok());
        // 反过来往上抬：拒绝
        assert!(
            check_scope_delegation(&dir, std::slice::from_ref(&low_scope), &mid_scope).is_err()
        );
    }
}
