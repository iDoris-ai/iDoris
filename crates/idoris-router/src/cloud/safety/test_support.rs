//! 安全模块单元测试共用的夹具（只在 `cfg(test)` 下编译）。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, AccountTree, Directory,
};
use idoris_compute_contracts::credential::{
    Credential, CredentialBinding, CredentialStatus, PaymentContext, resolve_payment_context,
};
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::{AccountId, CredentialId, EndpointKey, RequestId, TimestampMs};
use idoris_compute_contracts::money::Unit;
use idoris_compute_contracts::permit::{Billing, DataClass, Retention, TenantDataPolicy};

/// 一个静态的付款上下文（平台 -> acct-edu，节点凭证），云形态、THB。
pub(crate) fn payment_context() -> PaymentContext {
    let platform = AccountId::new("platform").unwrap();
    let payer = AccountId::new("acct-edu").unwrap();
    let tree = AccountTree::build(vec![
        Account {
            id: platform.clone(),
            kind: AccountKind::Platform,
            parent: None,
            unit: Unit::Thb,
            status: AccountStatus::Active,
        },
        Account {
            id: payer.clone(),
            kind: AccountKind::Org,
            parent: Some(platform),
            unit: Unit::Thb,
            status: AccountStatus::Active,
        },
    ])
    .unwrap();
    let directory = Directory::build(tree, Vec::new(), Vec::new()).unwrap();
    let credential = Credential::new(
        CredentialId::new("cred-edu").unwrap(),
        CredentialBinding::Node { account: payer },
        CredentialStatus::Active,
        None,
        Vec::new(),
    )
    .unwrap();
    resolve_payment_context(&directory, &credential, None, TimestampMs(0)).unwrap()
}

/// 处理儿童数据的租户（含 `child_edu`，固定 ZDR + 已签 DPA）。
pub(crate) fn child_tenant() -> TenantDataPolicy {
    TenantDataPolicy::new(
        BTreeSet::from([DataClass::ChildEdu]),
        None,
        Retention::Zdr,
        true,
    )
    .unwrap()
}

pub(crate) fn adult_tenant() -> TenantDataPolicy {
    TenantDataPolicy::new(
        BTreeSet::from([DataClass::Adult]),
        None,
        Retention::WithExceptions,
        false,
    )
    .unwrap()
}

pub(crate) fn request_id(text: &str) -> RequestId {
    RequestId::new(text).unwrap()
}

pub(crate) fn endpoint_key() -> EndpointKey {
    EndpointKey::new("example-judge", "example-org/judge", "openai-compat").unwrap()
}

pub(crate) const FORM: DeploymentForm = DeploymentForm::Cloud;
pub(crate) const BILLING: Billing = Billing::Paid;
