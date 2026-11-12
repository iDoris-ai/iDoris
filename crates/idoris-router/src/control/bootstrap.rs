//! 本机运维命令 `idoris control bootstrap --spec-stdin`（只在组织、云形态可用）：创建平台根、第一批组织、
//! 补贴池 / 组织之间的额度、以及第一张管理凭证。
//!
//! # 为什么需要它
//!
//! 控制面的 HTTP 接口只认带 `admin_scopes` 的云凭证，而 S1 的约束要求凭证只能绑在 `Org` 账户上——**没有任何一张
//! 凭证能管到平台根**（平台账户不是 `Org`）。所以「谁来建第一个组织、谁来给平台补贴池发额度、谁来签发第一张管理凭证」
//! 只能是拥有库文件的人：本机运维。这是有意的：平台层面的写操作永远不暴露在网络上，被盗的任何一张云凭证最多只能动
//! 它自己组织的子树。
//!
//! 运维走的是和 HTTP 同一套存储层代码（同样的校验、同样的审计），只是免于授权判定；审计行的 `actor_kind` 是
//! `operator`，`actor_id` 是 `cli`。
//!
//! # 请求体（stdin，JSON，上限 64 KiB，未知字段一律拒绝）
//!
//! ```json
//! {
//!   "accounts":    [{ "account_id": "platform", "kind": "platform", "unit": "THB" },
//!                   { "account_id": "eduai", "kind": "org", "parent": "platform", "unit": "THB" }],
//!   "allocations": [{ "allocation_id": "a-edu", "parent": "platform", "child": "eduai",
//!                     "amount": { "unit": "THB", "micros": 1000000000 }, "purpose": "prepaid",
//!                     "valid_from_ms": 1793534400000, "valid_until_ms": 1796126400000 }],
//!   "credentials": [{ "binding": { "kind": "service", "account": "eduai" },
//!                     "admin_scopes": [{ "op": "create_account", "subtree": "eduai" }],
//!                     "owner": "eduai-backend", "tenant_id": "eduai",
//!                     "allowed_privacy": ["local_only"], "allowed_roles": ["fast"] }]
//! }
//! ```
//!
//! 顺序固定：账户 -> 额度 -> 凭证。先把整份请求体解析、校验完才开始写；之后每一项各自一个事务，**逐项输出一行 JSON**
//! （签发的凭证带只出现一次的明文密钥，先入库、后输出）。某一项失败就停下，已经完成的不回滚（账户和额度重跑是幂等的，
//! 已经输出过的密钥不会丢）。**凭证不幂等**：重跑同一份请求体会再签发一批新凭证。

use std::io::{Read, Write};

use idoris_compute_contracts::account::{Account, AccountKind, AccountStatus};
use idoris_compute_contracts::control::WriteResult;
use idoris_compute_contracts::credential::CredentialBinding;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AccountId, AllocationId, BillingMonth, TimestampMs};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_tenancy::accounts::{
    AccountStore, CreateAccount, GrantAllocation, IssueCredential, Meta, Who,
};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::ControlClock;
use super::wire::{validate_ceilings, validate_owner};

const MAX_SPEC_BYTES: u64 = 64 * 1024;
/// 审计里 `actor_id` 的值。
pub const OPERATOR_LABEL: &str = "cli";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    #[serde(default)]
    accounts: Vec<AccountSpec>,
    #[serde(default)]
    allocations: Vec<AllocationSpec>,
    #[serde(default)]
    credentials: Vec<CredentialSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountSpec {
    account_id: AccountId,
    kind: AccountKind,
    #[serde(default)]
    parent: Option<AccountId>,
    unit: Unit,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllocationSpec {
    allocation_id: AllocationId,
    parent: AccountId,
    child: AccountId,
    amount: Money,
    purpose: Purpose,
    valid_from_ms: i64,
    valid_until_ms: i64,
    #[serde(default)]
    month: Option<BillingMonth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialSpec {
    binding: CredentialBinding,
    #[serde(default)]
    admin_scopes: Vec<idoris_compute_contracts::credential::AdminScope>,
    #[serde(default)]
    expires_at_ms: Option<i64>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    allowed_privacy: Vec<idoris_contracts::common::PrivacyClass>,
    #[serde(default)]
    allowed_roles: Vec<String>,
}

fn emit(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    writeln!(writer, "{value}")
        .and_then(|()| writer.flush())
        .map_err(|_| "无法输出结果".to_string())
}

fn written(result: WriteResult) -> &'static str {
    result.as_str()
}

/// 读取请求体、校验、逐项执行。成功返回 `Ok`；失败的错误信息不含任何密钥。
pub fn run(
    accounts: &AccountStore,
    reader: &mut impl Read,
    writer: &mut impl Write,
    clock: &dyn ControlClock,
) -> Result<(), String> {
    let mut raw = Vec::new();
    reader
        .take(MAX_SPEC_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "无法读取 bootstrap spec".to_string())?;
    if raw.is_empty() || raw.len() as u64 > MAX_SPEC_BYTES {
        return Err("bootstrap spec 为空或超过 64 KiB".to_string());
    }
    let spec: Spec = serde_json::from_slice(&raw)
        .map_err(|error| format!("bootstrap spec JSON 无效：{error}"))?;
    // 先把能在写库之前发现的问题都发现：权限上限的形状、标签。
    for (index, credential) in spec.credentials.iter().enumerate() {
        validate_ceilings(&credential.allowed_privacy, &credential.allowed_roles)
            .map_err(|error| format!("credentials[{index}]：{error}"))?;
        if let Some(owner) = &credential.owner {
            validate_owner(owner).map_err(|error| format!("credentials[{index}]：{error}"))?;
        }
    }
    let who = Who::Operator(OPERATOR_LABEL);

    for (index, item) in spec.accounts.iter().enumerate() {
        let request_id = Uuid::new_v4().to_string();
        let meta = Meta {
            now: clock.now(),
            request_id: &request_id,
        };
        let outcome = accounts
            .control_create_account(
                &who,
                &meta,
                &CreateAccount {
                    account: Account {
                        id: item.account_id.clone(),
                        kind: item.kind,
                        parent: item.parent.clone(),
                        unit: item.unit.clone(),
                        status: AccountStatus::Active,
                    },
                },
            )
            .map_err(|failure| format!("accounts[{index}] 失败：{failure}"))?;
        emit(
            writer,
            &json!({
                "kind": "account",
                "account_id": item.account_id,
                "result": written(outcome.result),
                "request_id": request_id,
            }),
        )?;
    }

    for (index, item) in spec.allocations.iter().enumerate() {
        let request_id = Uuid::new_v4().to_string();
        let meta = Meta {
            now: clock.now(),
            request_id: &request_id,
        };
        let outcome = accounts
            .control_grant_allocation(
                &who,
                &meta,
                &GrantAllocation {
                    id: item.allocation_id.clone(),
                    parent: item.parent.clone(),
                    child: item.child.clone(),
                    amount: item.amount.clone(),
                    purpose: item.purpose,
                    valid_from: TimestampMs(item.valid_from_ms),
                    valid_until: TimestampMs(item.valid_until_ms),
                    month: item.month,
                },
            )
            .map_err(|failure| format!("allocations[{index}] 失败：{failure}"))?;
        emit(
            writer,
            &json!({
                "kind": "allocation",
                "allocation_id": item.allocation_id,
                "result": written(outcome.result),
                "request_id": request_id,
            }),
        )?;
    }

    for (index, item) in spec.credentials.iter().enumerate() {
        let request_id = Uuid::new_v4().to_string();
        let meta = Meta {
            now: clock.now(),
            request_id: &request_id,
        };
        let key = MintedVirtualKey::mint();
        let outcome = accounts
            .control_issue_credential(
                &who,
                &meta,
                &IssueCredential {
                    key_id: key.key_id.clone(),
                    key_hash: key.hash,
                    binding: item.binding.clone(),
                    admin_scopes: item.admin_scopes.clone(),
                    expires_at: item.expires_at_ms.map(TimestampMs),
                    owner: item
                        .owner
                        .clone()
                        .unwrap_or_else(|| "operator-bootstrap".to_string()),
                    tenant_id: item.tenant_id.clone(),
                    allowed_privacy: item.allowed_privacy.clone(),
                    allowed_roles: item.allowed_roles.clone(),
                },
            )
            .map_err(|failure| format!("credentials[{index}] 失败：{failure}"))?;
        // 先入库（上面已提交），后输出：输出失败时凭证已经存在但没人知道密钥，运维可以用 `idoris key revoke` 撤销。
        emit(
            writer,
            &json!({
                "kind": "credential",
                "credential_id": key.key_id,
                "secret": key.secret.expose_secret(),
                "credential": outcome.value,
                "request_id": request_id,
            }),
        )?;
    }
    Ok(())
}
