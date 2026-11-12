//! 集成测试共用的夹具：**只用公开 API**，就像 S1–S7 的实现者那样。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, AccountTree, Directory, Membership, MembershipRole,
    MembershipStatus, Principal, PrincipalStatus,
};
use idoris_compute_contracts::credential::{
    Credential, CredentialBinding, CredentialStatus, PaymentContext, resolve_payment_context,
};
use idoris_compute_contracts::ids::{
    AccountId, CredentialId, MembershipId, NodeId, PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::Unit;
use serde::Serialize;

pub fn aid(s: &str) -> AccountId {
    AccountId::new(s).unwrap()
}

pub fn pid(s: &str) -> PrincipalId {
    PrincipalId::new(s).unwrap()
}

pub fn mid(s: &str) -> MembershipId {
    MembershipId::new(s).unwrap()
}

pub fn cid(s: &str) -> CredentialId {
    CredentialId::new(s).unwrap()
}

pub fn quota_unit(node: &str) -> Unit {
    Unit::Quota(NodeId::new(node).unwrap())
}

fn account(name: &str, kind: AccountKind, parent: Option<&str>) -> Account {
    Account {
        id: aid(name),
        kind,
        parent: parent.map(aid),
        unit: Unit::Thb,
        status: AccountStatus::Active,
    }
}

/// 平台 → 补贴池 → 租户 eduai → 担保人 teacher → 学生 s1；eduai → 学生 s2；
/// 平台 → 个人 p1；平台 → 学校 school-b。单位泰铢。
pub fn tree() -> AccountTree {
    use AccountKind::*;
    AccountTree::build(vec![
        account("platform", Platform, None),
        account("pool", SubsidyPool, Some("platform")),
        account("eduai", Org, Some("pool")),
        account("teacher", Sponsor, Some("eduai")),
        account("s1", Member, Some("teacher")),
        account("s2", Member, Some("eduai")),
        account("p1", Member, Some("platform")),
        account("school-b", Org, Some("platform")),
    ])
    .unwrap()
}

fn membership(
    id: &str,
    who: &str,
    org: &str,
    role: MembershipRole,
    acct: Option<&str>,
) -> Membership {
    Membership {
        id: mid(id),
        principal: pid(who),
        org: aid(org),
        role,
        account: acct.map(aid),
        status: MembershipStatus::Active,
    }
}

pub fn directory() -> Directory {
    use MembershipRole::*;
    let principals = ["kid1", "kid2", "tch", "adult"]
        .iter()
        .map(|p| Principal {
            id: pid(p),
            status: PrincipalStatus::Active,
        })
        .collect();
    Directory::build(
        tree(),
        principals,
        vec![
            membership("m-kid1", "kid1", "eduai", Member, Some("s1")),
            membership("m-kid2", "kid2", "eduai", Member, Some("s2")),
            membership("m-tch", "tch", "eduai", Sponsor, Some("teacher")),
            membership("m-adult", "adult", "platform", Member, Some("p1")),
            membership("m-adm", "tch", "eduai", Admin, None),
        ],
    )
    .unwrap()
}

pub fn credential(name: &str, binding: CredentialBinding) -> Credential {
    Credential::new(cid(name), binding, CredentialStatus::Active, None, vec![]).unwrap()
}

/// 学生 kid1 的付款上下文（付款账户 s1）。
pub fn payment_context() -> PaymentContext {
    let credential = credential(
        "c-kid1",
        CredentialBinding::Member {
            membership: mid("m-kid1"),
        },
    );
    resolve_payment_context(&directory(), &credential, None, TimestampMs(0)).unwrap()
}

// ---------------------------------------------------------------------------
// 快照
// ---------------------------------------------------------------------------

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/snapshots")
}

/// 把 `value` 序列化成 JSON，与 `testdata/snapshots/<name>.json` 比较（按 JSON 值比较，不比较格式）。
///
/// 快照是线上形状的**固定记录**：字段名、取值、结构一变，这里就会失败。需要有意更新时，
/// 设置环境变量 `UPDATE_SNAPSHOTS=1` 重新生成，然后**逐行审阅 diff**。
pub fn assert_snapshot<T: Serialize>(name: &str, value: &T) {
    let actual = serde_json::to_value(value).unwrap();
    let path = snapshot_dir().join(format!("{name}.json"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(snapshot_dir()).unwrap();
        let mut text = serde_json::to_string_pretty(&actual).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        return;
    }
    let expected_text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {}: {e}; run with UPDATE_SNAPSHOTS=1 to create it",
            path.display()
        )
    });
    let expected: serde_json::Value = serde_json::from_str(&expected_text).unwrap();
    assert_eq!(
        actual,
        expected,
        "snapshot {name} changed: if intentional, rerun with UPDATE_SNAPSHOTS=1 and review the diff\nactual:\n{}",
        serde_json::to_string_pretty(&actual).unwrap()
    );
}

/// 文本快照（YAML 等）。比较时统一行尾。
pub fn assert_text_snapshot(file: &str, actual: &str) {
    let path = snapshot_dir().join(file);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(snapshot_dir()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {}: {e}; run with UPDATE_SNAPSHOTS=1 to create it",
            path.display()
        )
    });
    assert_eq!(
        actual.replace("\r\n", "\n"),
        expected.replace("\r\n", "\n"),
        "text snapshot {file} changed: if intentional, rerun with UPDATE_SNAPSHOTS=1 and review the diff"
    );
}
