//! 核心对象：主体、成员关系、付款账户、额度（设计稿 §3）。凭证见 [`crate::credential`]。
//!
//! 评审 H1：「一棵账户树扛不住身份、成员关系和付款三种关系」，所以拆成几个对象，各管各的：
//!
//! | 对象 | 类型 | 一句话 |
//! |---|---|---|
//! | 主体 Principal | [`Principal`] | 开通方给的稳定假名 id；撤销状态在这里 |
//! | 成员关系 Membership | [`Membership`] | 主体属于哪个组织、什么角色；**一个主体可以有多个** |
//! | 付款账户 Account | [`Account`] / [`AccountTree`] | 额度委派树：平台 → 组织 / 担保人 → 成员子账户 |
//! | 额度 Allocation | [`Allocation`] | 父给子的是**授权上限**，不是转移余额 |
//!
//! # 本模块校验的不变式（每条至少一个正例一个反例，见 tests）
//!
//! - **账户树**：恰好一个根（`Platform`）；id 唯一；父账户存在；父子的 [`AccountKind`] 组合合法；
//!   父子同一单位；**无环**；**深度不超过 [`MAX_ACCOUNT_DEPTH`]**。
//! - **成员关系**：主体存在；组织存在且是 `Org`/`Platform`；角色与账户种类对得上；成员账户在该组织的
//!   子树里；一个账户至多属于一个成员关系。
//! - **额度**：只能沿父子边下发；单位一致；**下发的上限不能超过授权者本人的上限**，有效期也不能超出；
//!   幂等键和 id 不重复。
//! - **配额不可转让、不可兑现**：额度金额是 [`crate::grant::Grant`]（`transferable` 恒为 false）；
//!   本 crate 没有任何「把额度从一个账户转给另一个账户」的函数；`QUOTA` 换不了货币
//!   （[`crate::money::FxRate`]）。
//!
//! # 额度是授权上限，不是转移余额
//!
//! 父子共用**一份**余额。`Allocation.ceiling` 只是「子账户最多能动用这么多」，所以没有「余额」字段，
//! 子账户的花费算在父账户的账上，准入沿所有祖先同时检查（[`crate::admission`]），不会被花两次。
//! 这也意味着同一个父账户下各子账户的上限之和**可以超过**父账户自己的上限（超卖是允许的，
//! 总量由祖先的余额、速率、在途、补贴池上限兜住）；不允许的是**单个**子额度超过授权者本人。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::grant::Grant;
use crate::ids::{
    AccountId, AllocationId, BillingMonth, IdError, IdempotencyKey, MembershipId, PrincipalId,
};
use crate::money::{Money, MoneyError, Unit};

/// 账户树最大深度（根为 0）。设计稿没有给数；这里取 5：平台(0) → 组织(1) → 组织(2) → 担保人(3)
/// → 成员(4) 已经覆盖「学区 → 学校 → 老师 → 学生」，还留 1 层余量。深度要有上限，因为准入要在
/// 一个事务里沿所有祖先检查，路径长度就是事务的工作量。
pub const MAX_ACCOUNT_DEPTH: usize = 5;

/// 付款账户的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    /// 平台根账户。恰好一个，且只有它没有父账户。
    Platform,
    /// 平台补贴池（有总上限，设计稿 §4、评审 H10）。
    SubsidyPool,
    /// 组织（学校 / 公司 / 租户）。
    Org,
    /// 担保人（家长 / 老师 / 团队负责人）。
    Sponsor,
    /// 成员子账户。叶子。
    Member,
}

impl AccountKind {
    /// 父子种类组合是否合法：
    ///
    /// - `Platform` → `Org` / `Sponsor` / `SubsidyPool` / `Member`（个人云用户直接挂平台）
    /// - `SubsidyPool` → `Org` / `Sponsor` / `Member`
    /// - `Org` → `Org`（学区套学校）/ `Sponsor` / `Member`
    /// - `Sponsor` → `Member`
    /// - `Member` → 无（叶子）
    pub fn may_parent(self, child: AccountKind) -> bool {
        use AccountKind::*;
        match self {
            Platform => matches!(child, Org | Sponsor | SubsidyPool | Member),
            SubsidyPool => matches!(child, Org | Sponsor | Member),
            Org => matches!(child, Org | Sponsor | Member),
            Sponsor => matches!(child, Member),
            Member => false,
        }
    }
}

/// 账户状态。冻结/关闭会连带整个子树（见 [`AccountTree::inactive_cause`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Active,
    Frozen,
    Closed,
}

/// 付款账户。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: AccountId,
    pub kind: AccountKind,
    /// 只有 `Platform` 为 `None`。
    pub parent: Option<AccountId>,
    /// 账户的计量单位。同一棵树里所有账户同一单位（父子必须相同）。
    pub unit: Unit,
    pub status: AccountStatus,
}

/// 账户树校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("account tree must not be empty")]
    Empty,
    #[error("duplicate account id {0}")]
    DuplicateId(AccountId),
    #[error("platform account {0} must not have a parent")]
    PlatformHasParent(AccountId),
    #[error("account {0} has no parent but is not the platform root")]
    NonPlatformRoot(AccountId),
    #[error("account tree has no platform root")]
    NoRoot,
    #[error("account tree has more than one platform root")]
    MultipleRoots,
    #[error("account {child} names missing parent {parent}")]
    MissingParent { child: AccountId, parent: AccountId },
    #[error("account {0} is its own parent")]
    SelfParent(AccountId),
    #[error("{parent_kind:?} account {parent} may not parent a {child_kind:?} account {child}")]
    KindNotAllowed {
        parent: AccountId,
        child: AccountId,
        parent_kind: AccountKind,
        child_kind: AccountKind,
    },
    #[error("account {child} has unit {child_unit} but its parent {parent} has unit {parent_unit}")]
    UnitMismatch {
        parent: AccountId,
        child: AccountId,
        parent_unit: Unit,
        child_unit: Unit,
    },
    #[error("account {0} is part of a parent cycle")]
    Cycle(AccountId),
    #[error("account {account} is at depth {depth}, deeper than the maximum {max}")]
    TooDeep {
        account: AccountId,
        depth: usize,
        max: usize,
    },
    #[error("unknown account {0}")]
    UnknownAccount(AccountId),
}

/// 经过校验的账户树。只能通过 [`AccountTree::build`]（或反序列化，同样校验）得到，所以拿到手的
/// 一定满足：单根、无环、深度有上限、父子种类与单位合法。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Account>", into = "Vec<Account>")]
pub struct AccountTree {
    root: Account,
    accounts: BTreeMap<AccountId, Account>,
}

impl TryFrom<Vec<Account>> for AccountTree {
    type Error = TreeError;
    fn try_from(accounts: Vec<Account>) -> Result<Self, TreeError> {
        AccountTree::build(accounts)
    }
}

impl From<AccountTree> for Vec<Account> {
    fn from(tree: AccountTree) -> Vec<Account> {
        tree.accounts.into_values().collect()
    }
}

impl AccountTree {
    /// 校验并构造。检查顺序固定，所以同一份坏数据永远报同一个错：
    /// 空 → id 重复 → 根的形状（平台有父 / 非平台无父）→ 平台个数 → 父账户缺失 → 自己当自己的父 →
    /// 父子种类 → 父子单位 → 环 → 深度。
    pub fn build(accounts: Vec<Account>) -> Result<Self, TreeError> {
        if accounts.is_empty() {
            return Err(TreeError::Empty);
        }
        let mut map: BTreeMap<AccountId, Account> = BTreeMap::new();
        for account in accounts {
            let id = account.id.clone();
            if map.insert(id.clone(), account).is_some() {
                return Err(TreeError::DuplicateId(id));
            }
        }
        let mut platforms: Vec<&AccountId> = Vec::new();
        for account in map.values() {
            match (account.kind, &account.parent) {
                (AccountKind::Platform, Some(_)) => {
                    return Err(TreeError::PlatformHasParent(account.id.clone()));
                }
                (AccountKind::Platform, None) => platforms.push(&account.id),
                (_, None) => return Err(TreeError::NonPlatformRoot(account.id.clone())),
                (_, Some(_)) => {}
            }
        }
        let root_id = match platforms.as_slice() {
            [] => return Err(TreeError::NoRoot),
            [only] => (*only).clone(),
            _ => return Err(TreeError::MultipleRoots),
        };
        let root = match map.get(&root_id) {
            Some(account) => account.clone(),
            None => return Err(TreeError::NoRoot), // 刚从 map 里取出的 id，不会走到这里
        };
        for account in map.values() {
            let Some(parent_id) = &account.parent else {
                continue;
            };
            if *parent_id == account.id {
                return Err(TreeError::SelfParent(account.id.clone()));
            }
            let Some(parent) = map.get(parent_id) else {
                return Err(TreeError::MissingParent {
                    child: account.id.clone(),
                    parent: parent_id.clone(),
                });
            };
            if !parent.kind.may_parent(account.kind) {
                return Err(TreeError::KindNotAllowed {
                    parent: parent.id.clone(),
                    child: account.id.clone(),
                    parent_kind: parent.kind,
                    child_kind: account.kind,
                });
            }
            if parent.unit != account.unit {
                return Err(TreeError::UnitMismatch {
                    parent: parent.id.clone(),
                    child: account.id.clone(),
                    parent_unit: parent.unit.clone(),
                    child_unit: account.unit.clone(),
                });
            }
        }
        // 先查环（不设深度上限地一直往上走，遇到重复就是环），再查深度。
        // 分两遍是为了让一个长环报 `Cycle`，而不是先撞上深度上限报 `TooDeep`。
        for account in map.values() {
            let mut seen: BTreeSet<&AccountId> = BTreeSet::new();
            seen.insert(&account.id);
            let mut current = account;
            while let Some(parent_id) = &current.parent {
                if !seen.insert(parent_id) {
                    return Err(TreeError::Cycle(account.id.clone()));
                }
                match map.get(parent_id) {
                    Some(parent) => current = parent,
                    None => break, // 上面已经保证父账户存在；这里只是不 panic 的兜底
                }
            }
        }
        let tree = Self {
            root,
            accounts: map,
        };
        for account in tree.accounts.values() {
            let depth = tree.depth(&account.id).unwrap_or(0);
            if depth > MAX_ACCOUNT_DEPTH {
                return Err(TreeError::TooDeep {
                    account: account.id.clone(),
                    depth,
                    max: MAX_ACCOUNT_DEPTH,
                });
            }
        }
        Ok(tree)
    }

    /// 返回一棵多了 `account` 的新树（原树不变），整体重新校验。
    pub fn with_account(&self, account: Account) -> Result<AccountTree, TreeError> {
        let mut all: Vec<Account> = self.accounts.values().cloned().collect();
        all.push(account);
        AccountTree::build(all)
    }

    pub fn root(&self) -> &Account {
        &self.root
    }

    pub fn get(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.get(id)
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }

    pub fn children<'a>(&'a self, id: &'a AccountId) -> impl Iterator<Item = &'a Account> {
        self.accounts
            .values()
            .filter(move |a| a.parent.as_ref() == Some(id))
    }

    /// 深度（根为 0）。账户不存在返回 `None`。
    pub fn depth(&self, id: &AccountId) -> Option<usize> {
        let mut depth = 0;
        let mut current = self.accounts.get(id)?;
        while let Some(parent_id) = &current.parent {
            current = self.accounts.get(parent_id)?;
            depth += 1;
            if depth > self.accounts.len() {
                return None; // 有环；构造时已排除，这里只是不死循环
            }
        }
        Some(depth)
    }

    /// `[自己, 父, ..., 根]`。准入要沿它检查五项（设计稿 §5）。
    pub fn ancestors(&self, id: &AccountId) -> Result<Vec<&Account>, TreeError> {
        let mut chain = Vec::new();
        let mut current = self
            .accounts
            .get(id)
            .ok_or_else(|| TreeError::UnknownAccount(id.clone()))?;
        chain.push(current);
        while let Some(parent_id) = &current.parent {
            current = self
                .accounts
                .get(parent_id)
                .ok_or_else(|| TreeError::UnknownAccount(parent_id.clone()))?;
            chain.push(current);
            if chain.len() > self.accounts.len() {
                return Err(TreeError::Cycle(id.clone()));
            }
        }
        Ok(chain)
    }

    /// `node` 是否在 `ancestor` 的子树里（含它自己）。任何一个账户不存在都返回 `false`。
    pub fn is_descendant_or_self(&self, node: &AccountId, ancestor: &AccountId) -> bool {
        self.ancestors(node)
            .map(|chain| chain.iter().any(|a| a.id == *ancestor))
            .unwrap_or(false)
    }

    /// 沿 `[自己, 父, ..., 根]` 找第一个不是 `Active` 的账户。冻结或关闭会连带整个子树：
    /// 返回 `Some((那个账户, 它的状态))`；整条链都 `Active` 返回 `None`。
    pub fn inactive_cause(
        &self,
        id: &AccountId,
    ) -> Result<Option<(AccountId, AccountStatus)>, TreeError> {
        Ok(self
            .ancestors(id)?
            .into_iter()
            .find(|a| a.status != AccountStatus::Active)
            .map(|a| (a.id.clone(), a.status)))
    }
}

/// 主体状态。`Revoked` = 开通方撤销了处理资格（同意撤回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalStatus {
    Active,
    Revoked,
}

/// 主体：开通方提供的稳定假名 id。iDoris 不存姓名、年龄这些个人信息（设计稿 §9），所以这里
/// 只有 id 和状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub id: PrincipalId,
    pub status: PrincipalStatus,
}

/// 成员关系里的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipRole {
    /// 消费者（学生 / Agent24 实例），有 `Member` 种类的子账户。
    Member,
    /// 担保人（家长 / 老师），有 `Sponsor` 种类的账户。担保人只管费用，不代替同意（设计稿 §7）。
    Sponsor,
    /// 组织管理员，没有付款账户。
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    Active,
    Revoked,
}

/// 成员关系：主体属于哪个组织、什么角色。一个主体可以有多个成员关系（老师同时在学校 A 和 B）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Membership {
    pub id: MembershipId,
    pub principal: PrincipalId,
    /// 所属组织（`Org`，个人云用户挂平台时是 `Platform`）。
    pub org: AccountId,
    pub role: MembershipRole,
    /// `Member` 角色 → 该成员的 `Member` 子账户；`Sponsor` 角色 → 该担保人的 `Sponsor` 账户；
    /// `Admin` → `None`。必须在 `org` 的子树里。
    pub account: Option<AccountId>,
    pub status: MembershipStatus,
}

/// 目录校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectoryError {
    #[error("duplicate principal {0}")]
    DuplicatePrincipal(PrincipalId),
    #[error("duplicate membership {0}")]
    DuplicateMembership(MembershipId),
    #[error("membership {membership} names unknown principal {principal}")]
    UnknownPrincipal {
        membership: MembershipId,
        principal: PrincipalId,
    },
    #[error("membership {membership} names unknown org account {org}")]
    UnknownOrg {
        membership: MembershipId,
        org: AccountId,
    },
    #[error("membership {membership}: account {org} is a {kind:?}, not an org")]
    NotAnOrg {
        membership: MembershipId,
        org: AccountId,
        kind: AccountKind,
    },
    #[error("membership {membership}: role {role:?} does not match account {account:?}")]
    RoleAccountMismatch {
        membership: MembershipId,
        role: MembershipRole,
        account: Option<AccountId>,
    },
    #[error("membership {membership} names unknown account {account}")]
    UnknownAccount {
        membership: MembershipId,
        account: AccountId,
    },
    #[error("membership {membership}: account {account} is not under org {org}")]
    AccountNotUnderOrg {
        membership: MembershipId,
        account: AccountId,
        org: AccountId,
    },
    #[error("account {account} is bound by both membership {first} and membership {second}")]
    AccountAlreadyBound {
        account: AccountId,
        first: MembershipId,
        second: MembershipId,
    },
}

/// 经过校验的目录：账户树 + 主体 + 成员关系。付款上下文的解析（[`crate::credential`]）只读它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directory {
    tree: AccountTree,
    principals: BTreeMap<PrincipalId, Principal>,
    memberships: BTreeMap<MembershipId, Membership>,
}

impl Directory {
    /// 校验并构造。
    pub fn build(
        tree: AccountTree,
        principals: Vec<Principal>,
        memberships: Vec<Membership>,
    ) -> Result<Self, DirectoryError> {
        let mut principal_map = BTreeMap::new();
        for principal in principals {
            let id = principal.id.clone();
            if principal_map.insert(id.clone(), principal).is_some() {
                return Err(DirectoryError::DuplicatePrincipal(id));
            }
        }
        let mut membership_map: BTreeMap<MembershipId, Membership> = BTreeMap::new();
        let mut bound: BTreeMap<AccountId, MembershipId> = BTreeMap::new();
        for membership in memberships {
            let id = membership.id.clone();
            if !principal_map.contains_key(&membership.principal) {
                return Err(DirectoryError::UnknownPrincipal {
                    membership: id,
                    principal: membership.principal,
                });
            }
            let Some(org) = tree.get(&membership.org) else {
                return Err(DirectoryError::UnknownOrg {
                    membership: id,
                    org: membership.org,
                });
            };
            if !matches!(org.kind, AccountKind::Org | AccountKind::Platform) {
                return Err(DirectoryError::NotAnOrg {
                    membership: id,
                    org: membership.org,
                    kind: org.kind,
                });
            }
            let expected_kind = match membership.role {
                MembershipRole::Member => Some(AccountKind::Member),
                MembershipRole::Sponsor => Some(AccountKind::Sponsor),
                MembershipRole::Admin => None,
            };
            match (&membership.account, expected_kind) {
                (None, None) => {}
                (Some(account_id), Some(kind)) => {
                    let Some(account) = tree.get(account_id) else {
                        return Err(DirectoryError::UnknownAccount {
                            membership: id,
                            account: account_id.clone(),
                        });
                    };
                    if account.kind != kind {
                        return Err(DirectoryError::RoleAccountMismatch {
                            membership: id,
                            role: membership.role,
                            account: membership.account.clone(),
                        });
                    }
                    if *account_id == membership.org
                        || !tree.is_descendant_or_self(account_id, &membership.org)
                    {
                        return Err(DirectoryError::AccountNotUnderOrg {
                            membership: id,
                            account: account_id.clone(),
                            org: membership.org.clone(),
                        });
                    }
                    if let Some(first) = bound.insert(account_id.clone(), id.clone()) {
                        return Err(DirectoryError::AccountAlreadyBound {
                            account: account_id.clone(),
                            first,
                            second: id,
                        });
                    }
                }
                _ => {
                    return Err(DirectoryError::RoleAccountMismatch {
                        membership: id,
                        role: membership.role,
                        account: membership.account.clone(),
                    });
                }
            }
            if membership_map.insert(id.clone(), membership).is_some() {
                return Err(DirectoryError::DuplicateMembership(id));
            }
        }
        Ok(Self {
            tree,
            principals: principal_map,
            memberships: membership_map,
        })
    }

    pub fn tree(&self) -> &AccountTree {
        &self.tree
    }

    pub fn principal(&self, id: &PrincipalId) -> Option<&Principal> {
        self.principals.get(id)
    }

    pub fn membership(&self, id: &MembershipId) -> Option<&Membership> {
        self.memberships.get(id)
    }

    /// 一个主体的全部成员关系（含已撤销的），按成员关系 id 排序。
    pub fn memberships_of<'a>(
        &'a self,
        principal: &'a PrincipalId,
    ) -> impl Iterator<Item = &'a Membership> {
        self.memberships
            .values()
            .filter(move |m| m.principal == *principal)
    }

    pub fn membership_for_account(&self, account: &AccountId) -> Option<&Membership> {
        self.memberships
            .values()
            .find(|m| m.account.as_ref() == Some(account))
    }
}

/// 额度：父账户给子账户的**授权上限**（不是转移余额）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub id: AllocationId,
    pub parent: AccountId,
    pub child: AccountId,
    /// 上限。注意叫 `ceiling`，不叫 `balance` / `amount`：它不是余额。
    pub ceiling: Grant,
    /// 发放幂等键；免费额度按 `(主体, 月份)`，见 [`subsidy_idempotency_key`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<IdempotencyKey>,
}

/// 免费额度发放的幂等键 `subsidy:<主体>:<YYYY-MM>`（设计稿 §4：`(成员, 月份)`）。
///
/// 这里「成员」取主体 id（假名 id），而不是成员关系 id：同一个主体在两个组织各有一个成员关系，
/// 但免费额度只该领一份，否则开两个成员关系就能薅两份（评审 H10 的防薅精神）。设计稿没有明说，
/// 见交付说明的含糊点。
pub fn subsidy_idempotency_key(
    principal: &PrincipalId,
    month: BillingMonth,
) -> Result<IdempotencyKey, IdError> {
    IdempotencyKey::new(format!("subsidy:{principal}:{month}"))
}

/// 额度校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllocationError {
    #[error("unknown account {0}")]
    UnknownAccount(AccountId),
    #[error("account {child} is not a child of {parent}: allocations only follow tree edges")]
    NotParentChild { parent: AccountId, child: AccountId },
    #[error("allocation unit {got} does not match account unit {expected}")]
    UnitMismatch { expected: Unit, got: Unit },
    #[error(
        "grantor {parent} has no allocation that covers this validity window: it has no authority to delegate"
    )]
    GrantorHasNoAuthority { parent: AccountId },
    #[error("allocation {requested:?} exceeds the grantor's own ceiling {available:?}")]
    ExceedsGrantor {
        parent: AccountId,
        requested: Money,
        available: Money,
    },
    #[error("duplicate allocation id {0}")]
    DuplicateId(AllocationId),
    #[error("duplicate idempotency key {0}")]
    DuplicateIdempotencyKey(IdempotencyKey),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// 校验：能不能再新增 `new` 这笔额度。`existing` 是已经生效的全部额度。
///
/// 规则（顺序固定）：
///
/// 1. `parent`、`child` 都在树里；
/// 2. `child` 的父账户就是 `parent`（只能沿树边下发，不能跨枝、不能越级）；
/// 3. 上限的单位 = 子账户的单位；
/// 4. id、幂等键都没出现过；
/// 5. **不能超过授权者本人**：`parent` 若是 `Platform` 则没有上限（平台自己的总量由补贴池等
///    准入检查兜）；否则 `existing` 里必须有一笔「流入 `parent`」且**有效期完全包住** `new` 的额度，
///    并且 `new` 的上限不超过其中最大的那笔。没有这样的额度，说明 `parent` 自己没有可下发的权限。
///
/// 不检查各子额度之和是否超过父额度（超卖是允许的，见模块文档）。
pub fn validate_allocation(
    tree: &AccountTree,
    existing: &[Allocation],
    new: &Allocation,
) -> Result<(), AllocationError> {
    let parent = tree
        .get(&new.parent)
        .ok_or_else(|| AllocationError::UnknownAccount(new.parent.clone()))?;
    let child = tree
        .get(&new.child)
        .ok_or_else(|| AllocationError::UnknownAccount(new.child.clone()))?;
    if child.parent.as_ref() != Some(&new.parent) {
        return Err(AllocationError::NotParentChild {
            parent: new.parent.clone(),
            child: new.child.clone(),
        });
    }
    if *new.ceiling.unit() != child.unit {
        return Err(AllocationError::UnitMismatch {
            expected: child.unit.clone(),
            got: new.ceiling.unit().clone(),
        });
    }
    if existing.iter().any(|a| a.id == new.id) {
        return Err(AllocationError::DuplicateId(new.id.clone()));
    }
    if let Some(key) = &new.idempotency_key
        && existing
            .iter()
            .any(|a| a.idempotency_key.as_ref() == Some(key))
    {
        return Err(AllocationError::DuplicateIdempotencyKey(key.clone()));
    }
    if parent.kind == AccountKind::Platform {
        return Ok(());
    }
    let mut best: Option<&Money> = None;
    for inbound in existing {
        if inbound.child != new.parent || !inbound.ceiling.window_covers(&new.ceiling) {
            continue;
        }
        let candidate = inbound.ceiling.amount();
        best = Some(match best {
            None => candidate,
            Some(current) => {
                if candidate.try_cmp(current)?.is_gt() {
                    candidate
                } else {
                    current
                }
            }
        });
    }
    let Some(available) = best else {
        return Err(AllocationError::GrantorHasNoAuthority {
            parent: new.parent.clone(),
        });
    };
    if new.ceiling.amount().try_cmp(available)?.is_gt() {
        return Err(AllocationError::ExceedsGrantor {
            parent: new.parent.clone(),
            requested: new.ceiling.amount().clone(),
            available: available.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::grant::Purpose;
    use crate::ids::{NodeId, TimestampMs};
    use crate::test_support::{
        active, aid as id, mid, pid, principal, sample_accounts, sample_directory, sample_tree,
    };
    use proptest::prelude::*;

    // ---------- 账户树 ----------

    #[test]
    fn valid_tree_builds_and_answers_structural_queries() {
        let tree = sample_tree();
        assert_eq!(tree.len(), 8);
        assert!(!tree.is_empty());
        assert_eq!(tree.root().id, id("platform"));
        assert_eq!(tree.depth(&id("platform")), Some(0));
        assert_eq!(tree.depth(&id("s1")), Some(4));
        assert_eq!(tree.depth(&id("p1")), Some(1));
        assert_eq!(tree.depth(&id("nope")), None);
        let chain: Vec<_> = tree
            .ancestors(&id("s1"))
            .unwrap()
            .iter()
            .map(|a| a.id.as_str().to_string())
            .collect();
        assert_eq!(chain, ["s1", "teacher", "eduai", "pool", "platform"]);
        assert!(tree.is_descendant_or_self(&id("s1"), &id("eduai")));
        assert!(tree.is_descendant_or_self(&id("s1"), &id("s1")));
        assert!(!tree.is_descendant_or_self(&id("s1"), &id("school-b")));
        assert!(!tree.is_descendant_or_self(&id("eduai"), &id("s1")));
        assert!(!tree.is_descendant_or_self(&id("nope"), &id("eduai")));
        let eduai = id("eduai");
        let kids: Vec<_> = tree.children(&eduai).map(|a| a.id.as_str()).collect();
        assert_eq!(kids, ["s2", "teacher"]);
        assert_eq!(tree.iter().count(), 8);
        assert_eq!(
            tree.ancestors(&id("nope")).unwrap_err(),
            TreeError::UnknownAccount(id("nope"))
        );
    }

    #[test]
    fn invalid_trees_are_rejected_with_the_expected_error_table() {
        use AccountKind::*;
        let a = active;
        let cases: Vec<(&str, Vec<Account>, TreeError)> = vec![
            ("empty", vec![], TreeError::Empty),
            (
                "duplicate id",
                vec![
                    a("p", Platform, None),
                    a("x", Org, Some("p")),
                    a("x", Org, Some("p")),
                ],
                TreeError::DuplicateId(id("x")),
            ),
            (
                "platform with parent",
                vec![a("p", Platform, Some("q"))],
                TreeError::PlatformHasParent(id("p")),
            ),
            (
                "non-platform root",
                vec![a("p", Platform, None), a("x", Org, None)],
                TreeError::NonPlatformRoot(id("x")),
            ),
            (
                "no platform",
                vec![a("x", Org, Some("y")), a("y", Org, Some("x"))],
                TreeError::NoRoot,
            ),
            (
                "two platforms",
                vec![a("p", Platform, None), a("q", Platform, None)],
                TreeError::MultipleRoots,
            ),
            (
                "missing parent",
                vec![a("p", Platform, None), a("x", Org, Some("ghost"))],
                TreeError::MissingParent {
                    child: id("x"),
                    parent: id("ghost"),
                },
            ),
            (
                "self parent",
                vec![a("p", Platform, None), a("x", Org, Some("x"))],
                TreeError::SelfParent(id("x")),
            ),
            (
                "member under member",
                vec![
                    a("p", Platform, None),
                    a("m1", Member, Some("p")),
                    a("m2", Member, Some("m1")),
                ],
                TreeError::KindNotAllowed {
                    parent: id("m1"),
                    child: id("m2"),
                    parent_kind: Member,
                    child_kind: Member,
                },
            ),
            (
                "org under sponsor",
                vec![
                    a("p", Platform, None),
                    a("s", Sponsor, Some("p")),
                    a("o", Org, Some("s")),
                ],
                TreeError::KindNotAllowed {
                    parent: id("s"),
                    child: id("o"),
                    parent_kind: Sponsor,
                    child_kind: Org,
                },
            ),
            (
                "second platform-kind child is rejected as platform-with-parent",
                vec![a("p", Platform, None), a("q", Platform, Some("p"))],
                TreeError::PlatformHasParent(id("q")),
            ),
        ];
        for (name, accounts, expected) in cases {
            assert_eq!(
                AccountTree::build(accounts).unwrap_err(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn unit_must_match_along_every_edge() {
        use AccountKind::*;
        let mut child = active("o", Org, Some("p"));
        child.unit = Unit::Usd;
        assert_eq!(
            AccountTree::build(vec![active("p", Platform, None), child]).unwrap_err(),
            TreeError::UnitMismatch {
                parent: id("p"),
                child: id("o"),
                parent_unit: Unit::Thb,
                child_unit: Unit::Usd
            }
        );
        // 不同节点的 QUOTA 也不行
        let q = |n: &str| Unit::Quota(NodeId::new(n).unwrap());
        let mut root = active("p", Platform, None);
        root.unit = q("a");
        let mut other = active("o", Org, Some("p"));
        other.unit = q("b");
        assert!(AccountTree::build(vec![root, other]).is_err());
    }

    #[test]
    fn cycles_are_rejected_even_when_longer_than_the_depth_limit() {
        use AccountKind::*;
        // 平台 + 一个 3 环（org 之间可以互为父）
        let accounts = vec![
            active("p", Platform, None),
            active("a", Org, Some("c")),
            active("b", Org, Some("a")),
            active("c", Org, Some("b")),
        ];
        assert!(matches!(
            AccountTree::build(accounts).unwrap_err(),
            TreeError::Cycle(_)
        ));
        // 一个比深度上限还长的环：仍然报 Cycle，不是 TooDeep
        let n = MAX_ACCOUNT_DEPTH + 4;
        let mut accounts = vec![active("p", Platform, None)];
        for i in 0..n {
            let parent = format!("n{}", (i + 1) % n);
            accounts.push(active(&format!("n{i}"), Org, Some(&parent)));
        }
        assert!(matches!(
            AccountTree::build(accounts).unwrap_err(),
            TreeError::Cycle(_)
        ));
    }

    #[test]
    fn depth_limit_is_exact() {
        use AccountKind::*;
        // 平台(0) → org1(1) → ... → orgN(N)，N = MAX 刚好合法，N = MAX+1 不合法
        let chain = |n: usize| {
            let mut accounts = vec![active("p", Platform, None)];
            for i in 1..=n {
                let parent = if i == 1 {
                    "p".to_string()
                } else {
                    format!("o{}", i - 1)
                };
                accounts.push(active(&format!("o{i}"), Org, Some(&parent)));
            }
            accounts
        };
        assert!(AccountTree::build(chain(MAX_ACCOUNT_DEPTH)).is_ok());
        assert_eq!(
            AccountTree::build(chain(MAX_ACCOUNT_DEPTH + 1)).unwrap_err(),
            TreeError::TooDeep {
                account: id(&format!("o{}", MAX_ACCOUNT_DEPTH + 1)),
                depth: MAX_ACCOUNT_DEPTH + 1,
                max: MAX_ACCOUNT_DEPTH
            }
        );
    }

    #[test]
    fn with_account_revalidates_and_leaves_the_original_alone() {
        use AccountKind::*;
        let tree = sample_tree();
        let bigger = tree
            .with_account(active("s3", Member, Some("eduai")))
            .unwrap();
        assert_eq!(bigger.len(), 9);
        assert_eq!(tree.len(), 8);
        assert!(
            tree.with_account(active("s1", Member, Some("eduai")))
                .is_err()
        ); // 重复 id
        assert!(tree.with_account(active("x", Member, Some("s1"))).is_err()); // 叶子不能有孩子
    }

    #[test]
    fn inactive_cause_walks_the_whole_chain() {
        use AccountKind::*;
        let mut accounts = sample_accounts();
        assert_eq!(sample_tree().inactive_cause(&id("s1")).unwrap(), None);
        // 冻结中间的 eduai：它的整个子树都受影响，兄弟分支不受影响
        for a in &mut accounts {
            if a.id == id("eduai") {
                a.status = AccountStatus::Frozen;
            }
        }
        let tree = AccountTree::build(accounts.clone()).unwrap();
        assert_eq!(
            tree.inactive_cause(&id("s1")).unwrap(),
            Some((id("eduai"), AccountStatus::Frozen))
        );
        assert_eq!(
            tree.inactive_cause(&id("eduai")).unwrap(),
            Some((id("eduai"), AccountStatus::Frozen))
        );
        assert_eq!(tree.inactive_cause(&id("p1")).unwrap(), None);
        assert_eq!(tree.inactive_cause(&id("school-b")).unwrap(), None);
        // 最近的非 Active 先报：学生自己关了，先报学生自己
        for a in &mut accounts {
            if a.id == id("s1") {
                a.status = AccountStatus::Closed;
            }
        }
        let tree = AccountTree::build(accounts).unwrap();
        assert_eq!(
            tree.inactive_cause(&id("s1")).unwrap(),
            Some((id("s1"), AccountStatus::Closed))
        );
        assert!(tree.inactive_cause(&id("nope")).is_err());
        let _ = Platform;
    }

    #[test]
    fn tree_serializes_as_an_account_array_and_validates_on_load() {
        let tree = sample_tree();
        let json = serde_json::to_value(&tree).unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 8);
        let back: AccountTree = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, tree);
        // 往数组里塞一个环：加载失败
        let mut bad = json.as_array().unwrap().clone();
        for a in &mut bad {
            if a["id"] == "eduai" {
                a["parent"] = serde_json::json!("s1");
            }
        }
        assert!(serde_json::from_value::<AccountTree>(serde_json::Value::Array(bad)).is_err());
        assert!(serde_json::from_str::<AccountTree>("[]").is_err());
    }

    #[test]
    fn account_wire_shape_and_strictness() {
        let a = active("acct-1", AccountKind::Org, Some("platform"));
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"id":"acct-1","kind":"org","parent":"platform","unit":"THB","status":"active"}"#
        );
        let with_extra =
            r#"{"id":"a","kind":"org","parent":null,"unit":"THB","status":"active","owner":"x"}"#;
        assert!(serde_json::from_str::<Account>(with_extra).is_err());
        let bad_kind = r#"{"id":"a","kind":"tenant","parent":null,"unit":"THB","status":"active"}"#;
        assert!(serde_json::from_str::<Account>(bad_kind).is_err());
        for kind in ["platform", "subsidy_pool", "org", "sponsor", "member"] {
            let j = format!(
                r#"{{"id":"a","kind":"{kind}","parent":null,"unit":"THB","status":"active"}}"#
            );
            assert!(serde_json::from_str::<Account>(&j).is_ok(), "{kind}");
        }
    }

    proptest! {
        /// 随机生成「每个新账户随机挂到前面某个允许当父亲的账户下」的树：一定合法，
        /// 祖先链长度 = 深度 + 1，且以根结尾。
        #[test]
        fn randomly_grown_trees_are_valid_and_ancestor_chains_end_at_the_root(
            picks in proptest::collection::vec((any::<u16>(), 0u8..3), 1..40),
        ) {
            use AccountKind::*;
            let mut accounts = vec![active("root", Platform, None)];
            for (i, (pick, kind_pick)) in picks.iter().enumerate() {
                let child_kind = [Org, Sponsor, Member][usize::from(*kind_pick)];
                // 候选父账户：允许当该种类父亲、且深度还没到上限
                let tree_so_far = AccountTree::build(accounts.clone()).unwrap();
                let candidates: Vec<&Account> = accounts
                    .iter()
                    .filter(|a| a.kind.may_parent(child_kind))
                    .filter(|a| tree_so_far.depth(&a.id).unwrap() < MAX_ACCOUNT_DEPTH)
                    .collect();
                prop_assume!(!candidates.is_empty());
                let parent = candidates[usize::from(*pick) % candidates.len()].id.clone();
                accounts.push(Account {
                    id: id(&format!("n{i}")),
                    kind: child_kind,
                    parent: Some(parent),
                    unit: Unit::Thb,
                    status: AccountStatus::Active,
                });
            }
            let tree = AccountTree::build(accounts).unwrap();
            for account in tree.iter() {
                let chain = tree.ancestors(&account.id).unwrap();
                prop_assert_eq!(chain.len(), tree.depth(&account.id).unwrap() + 1);
                prop_assert_eq!(&chain.last().unwrap().id, &id("root"));
                prop_assert!(tree.is_descendant_or_self(&account.id, &id("root")));
            }
        }
    }

    // ---------- 父子种类矩阵 ----------

    #[test]
    fn kind_matrix_is_exactly_the_documented_one() {
        use AccountKind::*;
        let all = [Platform, SubsidyPool, Org, Sponsor, Member];
        let allowed: &[(AccountKind, &[AccountKind])] = &[
            (Platform, &[Org, Sponsor, SubsidyPool, Member]),
            (SubsidyPool, &[Org, Sponsor, Member]),
            (Org, &[Org, Sponsor, Member]),
            (Sponsor, &[Member]),
            (Member, &[]),
        ];
        for (parent, children) in allowed {
            for child in all {
                assert_eq!(
                    parent.may_parent(child),
                    children.contains(&child),
                    "{parent:?} -> {child:?}"
                );
            }
        }
    }

    // ---------- 成员关系 / 目录 ----------

    use crate::test_support::membership;

    #[test]
    fn valid_directory_builds_and_answers_lookups() {
        let dir = sample_directory();
        assert_eq!(dir.tree().len(), 8);
        assert_eq!(
            dir.principal(&pid("kid1")).unwrap().status,
            PrincipalStatus::Active
        );
        assert!(dir.principal(&pid("ghost")).is_none());
        assert_eq!(
            dir.membership(&mid("m-kid1")).unwrap().account,
            Some(id("s1"))
        );
        // 一个主体有多个成员关系
        let tch = pid("tch");
        let mine: Vec<_> = dir.memberships_of(&tch).map(|m| m.id.as_str()).collect();
        assert_eq!(mine, ["m-adm", "m-tch"]);
        assert_eq!(
            dir.membership_for_account(&id("s2")).unwrap().id,
            mid("m-kid2")
        );
        assert!(dir.membership_for_account(&id("pool")).is_none());
    }

    #[test]
    fn directory_rejects_inconsistent_data() {
        use MembershipRole::*;
        let principals = || {
            vec![
                principal("a", PrincipalStatus::Active),
                principal("b", PrincipalStatus::Active),
            ]
        };
        let build = |memberships: Vec<Membership>| {
            Directory::build(sample_tree(), principals(), memberships)
        };
        // 重复主体
        assert_eq!(
            Directory::build(
                sample_tree(),
                vec![
                    principal("a", PrincipalStatus::Active),
                    principal("a", PrincipalStatus::Revoked)
                ],
                vec![]
            )
            .unwrap_err(),
            DirectoryError::DuplicatePrincipal(pid("a"))
        );
        // 重复成员关系
        assert_eq!(
            build(vec![
                membership("m1", "a", "eduai", Member, Some("s1")),
                membership("m1", "b", "eduai", Member, Some("s2")),
            ])
            .unwrap_err(),
            DirectoryError::DuplicateMembership(mid("m1"))
        );
        // 未知主体 / 未知组织 / 组织不是组织
        assert_eq!(
            build(vec![membership("m1", "ghost", "eduai", Member, Some("s1"))]).unwrap_err(),
            DirectoryError::UnknownPrincipal {
                membership: mid("m1"),
                principal: pid("ghost")
            }
        );
        assert_eq!(
            build(vec![membership("m1", "a", "ghost", Member, Some("s1"))]).unwrap_err(),
            DirectoryError::UnknownOrg {
                membership: mid("m1"),
                org: id("ghost")
            }
        );
        assert_eq!(
            build(vec![membership("m1", "a", "teacher", Member, Some("s1"))]).unwrap_err(),
            DirectoryError::NotAnOrg {
                membership: mid("m1"),
                org: id("teacher"),
                kind: AccountKind::Sponsor
            }
        );
        // 角色和账户对不上：Member 角色配了 Sponsor 账户；Admin 配了账户；Member 没有账户
        for bad in [
            membership("m1", "a", "eduai", Member, Some("teacher")),
            membership("m1", "a", "eduai", Sponsor, Some("s1")),
            membership("m1", "a", "eduai", Admin, Some("s1")),
            membership("m1", "a", "eduai", Member, None),
            membership("m1", "a", "eduai", Sponsor, None),
        ] {
            assert!(
                matches!(
                    build(vec![bad.clone()]).unwrap_err(),
                    DirectoryError::RoleAccountMismatch { .. }
                ),
                "{bad:?}"
            );
        }
        // 账户不存在 / 不在组织子树里 / 就是组织自己
        assert_eq!(
            build(vec![membership("m1", "a", "eduai", Member, Some("ghost"))]).unwrap_err(),
            DirectoryError::UnknownAccount {
                membership: mid("m1"),
                account: id("ghost")
            }
        );
        assert_eq!(
            build(vec![membership("m1", "a", "school-b", Member, Some("s1"))]).unwrap_err(),
            DirectoryError::AccountNotUnderOrg {
                membership: mid("m1"),
                account: id("s1"),
                org: id("school-b")
            }
        );
        // 一个账户被两个成员关系占用
        assert_eq!(
            build(vec![
                membership("m1", "a", "eduai", Member, Some("s1")),
                membership("m2", "b", "eduai", Member, Some("s1")),
            ])
            .unwrap_err(),
            DirectoryError::AccountAlreadyBound {
                account: id("s1"),
                first: mid("m1"),
                second: mid("m2")
            }
        );
    }

    // ---------- 额度 ----------

    fn grant(micros: i64, from: i64, until: i64) -> Grant {
        Grant::new(
            Money::new(Unit::Thb, micros),
            Purpose::FreeTier,
            TimestampMs(from),
            TimestampMs(until),
        )
        .unwrap()
    }

    fn alloc(name: &str, parent: &str, child: &str, ceiling: Grant) -> Allocation {
        Allocation {
            id: AllocationId::new(name).unwrap(),
            parent: id(parent),
            child: id(child),
            ceiling,
            idempotency_key: None,
        }
    }

    #[test]
    fn allocation_from_the_platform_has_no_ceiling() {
        let tree = sample_tree();
        // 平台 → 补贴池（池的总上限）：平台自己没有上限
        let pool = alloc("a1", "platform", "pool", grant(1_000_000_000, 0, 100));
        assert_eq!(validate_allocation(&tree, &[], &pool), Ok(()));
    }

    #[test]
    fn delegated_ceiling_may_not_exceed_the_grantors_own() {
        let tree = sample_tree();
        let existing = vec![alloc("a-pool", "platform", "pool", grant(1_000, 0, 100))];
        // 池 → eduai：不超过池自己的上限，通过；刚好等于也通过
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("a-edu", "pool", "eduai", grant(1_000, 0, 100))
            ),
            Ok(())
        );
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("a-edu", "pool", "eduai", grant(999, 0, 100))
            ),
            Ok(())
        );
        // 多 1 微单位：拒绝，并说明授权者本人的上限
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("a-edu", "pool", "eduai", grant(1_001, 0, 100))
            ),
            Err(AllocationError::ExceedsGrantor {
                parent: id("pool"),
                requested: Money::new(Unit::Thb, 1_001),
                available: Money::new(Unit::Thb, 1_000),
            })
        );
    }

    #[test]
    fn delegated_validity_window_may_not_outlive_the_grantors() {
        let tree = sample_tree();
        let existing = vec![alloc("a-pool", "platform", "pool", grant(1_000, 10, 100))];
        for (from, until, ok) in [
            (10, 100, true),
            (20, 50, true),
            (9, 100, false),
            (10, 101, false),
            (0, 5, false),
        ] {
            let result = validate_allocation(
                &tree,
                &existing,
                &alloc("a-edu", "pool", "eduai", grant(500, from, until)),
            );
            if ok {
                assert_eq!(result, Ok(()), "[{from},{until})");
            } else {
                assert_eq!(
                    result,
                    Err(AllocationError::GrantorHasNoAuthority { parent: id("pool") }),
                    "[{from},{until})"
                );
            }
        }
    }

    #[test]
    fn a_grantor_with_no_inbound_allocation_has_no_authority() {
        let tree = sample_tree();
        assert_eq!(
            validate_allocation(&tree, &[], &alloc("a1", "eduai", "s2", grant(1, 0, 10))),
            Err(AllocationError::GrantorHasNoAuthority {
                parent: id("eduai")
            })
        );
    }

    #[test]
    fn the_best_covering_inbound_allocation_is_used_not_the_first() {
        let tree = sample_tree();
        let existing = vec![
            alloc("a-small", "platform", "pool", grant(100, 0, 100)),
            alloc("a-big", "platform", "pool", grant(900, 0, 100)),
            // 更大但窗口不覆盖：不算
            alloc("a-huge-late", "platform", "pool", grant(9_999, 200, 300)),
        ];
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("n1", "pool", "eduai", grant(900, 0, 100))
            ),
            Ok(())
        );
        assert!(matches!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("n1", "pool", "eduai", grant(901, 0, 100))
            ),
            Err(AllocationError::ExceedsGrantor { .. })
        ));
    }

    #[test]
    fn oversubscription_across_siblings_is_allowed_only_each_child_is_capped() {
        // 共用一份余额：两个孩子各拿到不超过父上限的额度，加起来超过父上限，是允许的。
        let tree = sample_tree();
        let existing = vec![
            alloc("a-edu", "pool", "eduai", grant(1_000, 0, 100)),
            alloc("a-s1", "eduai", "teacher", grant(800, 0, 100)),
        ];
        let second = alloc("a-s2", "eduai", "s2", grant(800, 0, 100));
        assert_eq!(validate_allocation(&tree, &existing, &second), Ok(()));
    }

    #[test]
    fn allocations_only_follow_tree_edges() {
        let tree = sample_tree();
        let existing = vec![alloc("a-edu", "pool", "eduai", grant(1_000, 0, 100))];
        // 跨枝 / 越级 / 反向 / 自己给自己
        for (parent, child) in [
            ("eduai", "school-b"),
            ("pool", "s1"),
            ("teacher", "eduai"),
            ("eduai", "eduai"),
            ("eduai", "p1"),
        ] {
            assert_eq!(
                validate_allocation(
                    &tree,
                    &existing,
                    &alloc("x", parent, child, grant(1, 0, 10))
                ),
                Err(AllocationError::NotParentChild {
                    parent: id(parent),
                    child: id(child)
                }),
                "{parent} -> {child}"
            );
        }
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("x", "ghost", "eduai", grant(1, 0, 10))
            ),
            Err(AllocationError::UnknownAccount(id("ghost")))
        );
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("x", "eduai", "ghost", grant(1, 0, 10))
            ),
            Err(AllocationError::UnknownAccount(id("ghost")))
        );
    }

    #[test]
    fn allocation_unit_must_equal_the_child_account_unit() {
        let tree = sample_tree();
        let usd = Grant::new(
            Money::new(Unit::Usd, 1),
            Purpose::FreeTier,
            TimestampMs(0),
            TimestampMs(10),
        )
        .unwrap();
        assert_eq!(
            validate_allocation(&tree, &[], &alloc("x", "platform", "pool", usd)),
            Err(AllocationError::UnitMismatch {
                expected: Unit::Thb,
                got: Unit::Usd
            })
        );
    }

    #[test]
    fn duplicate_ids_and_idempotency_keys_are_rejected() {
        let tree = sample_tree();
        let month: BillingMonth = "2026-11".parse().unwrap();
        let key = subsidy_idempotency_key(&pid("kid1"), month).unwrap();
        assert_eq!(key.as_str(), "subsidy:kid1:2026-11");
        let mut first = alloc("a1", "platform", "pool", grant(100, 0, 10));
        first.idempotency_key = Some(key.clone());
        let existing = vec![first];
        // 相同 id
        assert_eq!(
            validate_allocation(
                &tree,
                &existing,
                &alloc("a1", "platform", "pool", grant(1, 0, 10))
            ),
            Err(AllocationError::DuplicateId(
                AllocationId::new("a1").unwrap()
            ))
        );
        // 相同幂等键、不同 id
        let mut dup = alloc("a2", "platform", "pool", grant(1, 0, 10));
        dup.idempotency_key = Some(key.clone());
        assert_eq!(
            validate_allocation(&tree, &existing, &dup),
            Err(AllocationError::DuplicateIdempotencyKey(key))
        );
        // 不同月份的键不冲突
        let mut other = alloc("a3", "platform", "pool", grant(1, 0, 10));
        other.idempotency_key =
            Some(subsidy_idempotency_key(&pid("kid1"), "2026-12".parse().unwrap()).unwrap());
        assert_eq!(validate_allocation(&tree, &existing, &other), Ok(()));
    }

    #[test]
    fn subsidy_key_is_per_principal_and_month_and_always_constructible() {
        let m1: BillingMonth = "2026-11".parse().unwrap();
        let m2: BillingMonth = "2026-12".parse().unwrap();
        let a = subsidy_idempotency_key(&pid("a"), m1).unwrap();
        assert_ne!(a, subsidy_idempotency_key(&pid("b"), m1).unwrap());
        assert_ne!(a, subsidy_idempotency_key(&pid("a"), m2).unwrap());
        assert_eq!(a, subsidy_idempotency_key(&pid("a"), m1).unwrap());
        // 最长的主体 id 也放得下
        let longest = PrincipalId::new("x".repeat(crate::ids::MAX_ID_LEN)).unwrap();
        assert!(subsidy_idempotency_key(&longest, m1).is_ok());
    }

    #[test]
    fn allocation_is_a_ceiling_not_a_balance_on_the_wire() {
        let a = alloc("a1", "platform", "pool", grant(5_000_000, 0, 10));
        let json = serde_json::to_value(&a).unwrap();
        assert!(json.get("ceiling").is_some());
        assert!(json.get("balance").is_none() && json.get("amount").is_none());
        assert_eq!(
            serde_json::from_value::<Allocation>(json.clone()).unwrap(),
            a
        );
        let mut with_balance = json;
        with_balance["balance"] = serde_json::json!(1);
        assert!(serde_json::from_value::<Allocation>(with_balance).is_err());
    }
}
