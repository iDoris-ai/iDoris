//! 只给单元测试共用的夹具（`#[cfg(test)]`）。不属于公开 API。
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use crate::account::{
    Account, AccountKind, AccountStatus, AccountTree, Directory, Membership, MembershipRole,
    MembershipStatus, Principal, PrincipalStatus,
};
use crate::ids::{AccountId, MembershipId, PrincipalId};
use crate::money::Unit;

pub(crate) fn aid(s: &str) -> AccountId {
    AccountId::new(s).unwrap()
}

pub(crate) fn pid(s: &str) -> PrincipalId {
    PrincipalId::new(s).unwrap()
}

pub(crate) fn mid(s: &str) -> MembershipId {
    MembershipId::new(s).unwrap()
}

pub(crate) fn account(
    name: &str,
    kind: AccountKind,
    parent: Option<&str>,
    status: AccountStatus,
) -> Account {
    Account {
        id: aid(name),
        kind,
        parent: parent.map(aid),
        unit: Unit::Thb,
        status,
    }
}

pub(crate) fn active(name: &str, kind: AccountKind, parent: Option<&str>) -> Account {
    account(name, kind, parent, AccountStatus::Active)
}

/// 平台 → 补贴池 → 租户 eduai → 担保人 teacher → 学生 s1；
/// 租户 eduai → 学生 s2；平台 → 个人 p1；平台 → 学校 school-b。
pub(crate) fn sample_accounts() -> Vec<Account> {
    use AccountKind::*;
    vec![
        active("platform", Platform, None),
        active("pool", SubsidyPool, Some("platform")),
        active("eduai", Org, Some("pool")),
        active("teacher", Sponsor, Some("eduai")),
        active("s1", Member, Some("teacher")),
        active("s2", Member, Some("eduai")),
        active("p1", Member, Some("platform")),
        active("school-b", Org, Some("platform")),
    ]
}

pub(crate) fn sample_tree() -> AccountTree {
    AccountTree::build(sample_accounts()).unwrap()
}

pub(crate) fn principal(name: &str, status: PrincipalStatus) -> Principal {
    Principal {
        id: pid(name),
        status,
    }
}

pub(crate) fn membership(
    mem: &str,
    who: &str,
    org: &str,
    role: MembershipRole,
    account: Option<&str>,
) -> Membership {
    Membership {
        id: mid(mem),
        principal: pid(who),
        org: aid(org),
        role,
        account: account.map(aid),
        status: MembershipStatus::Active,
    }
}

/// 主体 kid1/kid2（学生）、tch（老师，同时是担保人和管理员）、adult（个人用户）。
pub(crate) fn sample_directory() -> Directory {
    sample_directory_with(sample_tree(), |_| {})
}

/// 同上，但允许换账户树、并在构造前修改主体/成员关系（测试撤销、冻结等状态）。
pub(crate) fn sample_directory_with(
    tree: AccountTree,
    tweak: impl FnOnce(&mut DirectoryParts),
) -> Directory {
    use MembershipRole::*;
    let mut parts = DirectoryParts {
        principals: vec![
            principal("kid1", PrincipalStatus::Active),
            principal("kid2", PrincipalStatus::Active),
            principal("tch", PrincipalStatus::Active),
            principal("adult", PrincipalStatus::Active),
        ],
        memberships: vec![
            membership("m-kid1", "kid1", "eduai", Member, Some("s1")),
            membership("m-kid2", "kid2", "eduai", Member, Some("s2")),
            membership("m-tch", "tch", "eduai", Sponsor, Some("teacher")),
            membership("m-adult", "adult", "platform", Member, Some("p1")),
            membership("m-adm", "tch", "eduai", Admin, None),
        ],
    };
    tweak(&mut parts);
    Directory::build(tree, parts.principals, parts.memberships).unwrap()
}

pub(crate) struct DirectoryParts {
    pub principals: Vec<Principal>,
    pub memberships: Vec<Membership>,
}
