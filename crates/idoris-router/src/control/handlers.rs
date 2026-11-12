//! 控制面的 HTTP 处理函数。每个都很薄：认证凭证 -> 解析请求体 -> 调用存储层的 `control_*`（授权、校验、写入、审计在
//! 那里的一个事务里完成）-> 渲染响应。存储层是阻塞的（SQLite），所以都在 `spawn_blocking` 里调用。
//!
//! 这里**不做任何授权判断**，也不看来源地址、`Host`、`X-Forwarded-For` 之类的头：授权只由凭证和它的权限范围决定。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use idoris_compute_contracts::account::{Account, AccountStatus, Membership, MembershipStatus};
use idoris_compute_contracts::credential::{Credential, CredentialStatus};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, CredentialId, MembershipId, PrincipalId, TimestampMs,
};
use idoris_tenancy::accounts::{
    AccountStore, CreateAccount, CreateMembership, CreatePairingCode, CreatePrincipal, Failure,
    GrantAllocation, IssueCredential, Meta, RedeemPairingCode, RevokeAllocation, RevokeMembership,
    RevokePairingCode, RevokePrincipal, SetAccountStatus, SetCredentialStatus, Who, Written,
};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::VirtualKeyHash;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::wire::{
    AccountStatusBody, CreateAccountBody, CreateMembershipBody, CreatePairingBody,
    CreatePrincipalBody, CredentialStatusBody, EmptyBody, GrantAllocationBody, IssueCredentialBody,
    RedeemBody, RevokePrincipalBody, allocation_json, failure_response, not_found_response,
    pairing_json, parse_body, principal_json, read_response, to_json, validate_ceilings,
    validate_owner, written_response,
};
use super::{ControlPlane, pairing_codes};
use idoris_compute_contracts::control::ControlReason;

type Plane = State<Arc<ControlPlane>>;

fn new_request_id() -> String {
    Uuid::new_v4().to_string()
}

/// 从 `Authorization` 头认出密钥的哈希。头缺失、重复、格式不对（包括 Admin token：没有 `idk_` 前缀）一律返回 `None`，
/// 调用方回 `unauthenticated`，不区分原因。
fn key_hash(headers: &HeaderMap) -> Option<VirtualKeyHash> {
    crate::auth::bearer_secret(headers)
        .ok()
        .map(|secret| secret.hash())
}

fn unauthenticated(request_id: &str) -> Response {
    failure_response(request_id, &Failure::new(ControlReason::Unauthenticated))
}

fn blocking_failed(request_id: &str) -> Response {
    failure_response(request_id, &Failure::new(ControlReason::InternalError))
}

/// 写操作的共同骨架。`parse` 失败时：调用者被认出就留一行 `rejected / invalid_request` 审计，然后 400。
async fn write_op<R, T, P, E, V>(
    plane: Arc<ControlPlane>,
    headers: HeaderMap,
    body: Bytes,
    op: &'static str,
    parse: P,
    exec: E,
    render: V,
) -> Response
where
    R: Send + 'static,
    T: Send + 'static,
    P: FnOnce(&[u8]) -> Result<R, String>,
    E: FnOnce(&AccountStore, &Who<'_>, &Meta<'_>, R) -> Result<Written<T>, Failure>
        + Send
        + 'static,
    V: FnOnce(&T) -> Value,
{
    let request_id = new_request_id();
    let Some(hash) = key_hash(&headers) else {
        return unauthenticated(&request_id);
    };
    let parsed = parse(&body);
    let now = plane.clock.now();
    let accounts = plane.accounts.clone();
    let rid = request_id.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let who = Who::Secret(&hash);
        let meta = Meta {
            now,
            request_id: &rid,
        };
        match parsed {
            Ok(request) => exec(&accounts, &who, &meta, request),
            Err(detail) => {
                let mut failure = accounts.control_reject_invalid_request(&who, &meta, op);
                if failure.reason == ControlReason::InvalidRequest {
                    failure.detail = detail;
                }
                Err(failure)
            }
        }
    })
    .await;
    match outcome {
        Ok(Ok(written)) => written_response(&request_id, written.result, render(&written.value)),
        Ok(Err(failure)) => failure_response(&request_id, &failure),
        Err(_) => blocking_failed(&request_id),
    }
}

/// 读操作的共同骨架：认证 -> 解析参数 -> 查询。读不写审计。
async fn read_op<R, T, E, V>(
    plane: Arc<ControlPlane>,
    headers: HeaderMap,
    params: Result<R, String>,
    exec: E,
    render: V,
) -> Response
where
    R: Send + 'static,
    T: Send + 'static,
    E: FnOnce(&AccountStore, &Who<'_>, TimestampMs, R) -> Result<T, Failure> + Send + 'static,
    V: FnOnce(T) -> Value,
{
    let request_id = new_request_id();
    let Some(hash) = key_hash(&headers) else {
        return unauthenticated(&request_id);
    };
    let params = match params {
        Ok(params) => params,
        Err(detail) => {
            return failure_response(
                &request_id,
                &Failure::with_detail(ControlReason::InvalidRequest, detail),
            );
        }
    };
    let now = plane.clock.now();
    let accounts = plane.accounts.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let who = Who::Secret(&hash);
        exec(&accounts, &who, now, params)
    })
    .await;
    match outcome {
        Ok(Ok(value)) => read_response(&request_id, render(value)),
        Ok(Err(failure)) => failure_response(&request_id, &failure),
        Err(_) => blocking_failed(&request_id),
    }
}

fn id<T, E: std::fmt::Display>(
    what: &str,
    raw: String,
    parse: impl FnOnce(String) -> Result<T, E>,
) -> Result<T, String> {
    parse(raw).map_err(|error| format!("{what}: {error}"))
}

fn account_id(raw: String) -> Result<AccountId, String> {
    id("account id", raw, AccountId::new)
}

// ================================================================================================
// 写
// ================================================================================================

pub(super) async fn create_principal(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "create_principal",
        parse_body::<CreatePrincipalBody>,
        |store, who, meta, request| {
            store.control_create_principal(
                who,
                meta,
                &CreatePrincipal {
                    principal: request.principal_id,
                    org: request.org,
                },
            )
        },
        |principal| json!({ "principal": principal_json(principal) }),
    )
    .await
}

pub(super) async fn revoke_principal(
    State(plane): Plane,
    Path(principal): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "revoke_principal",
        move |body| {
            let parsed = parse_body::<RevokePrincipalBody>(body)?;
            let principal = id("principal id", principal, PrincipalId::new)?;
            Ok((principal, parsed.org))
        },
        |store, who, meta, (principal, org)| {
            store.control_revoke_principal(who, meta, &RevokePrincipal { principal, org })
        },
        |principal| json!({ "principal": principal_json(principal) }),
    )
    .await
}

pub(super) async fn create_account(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "create_account",
        parse_body::<CreateAccountBody>,
        |store, who, meta, request| {
            store.control_create_account(
                who,
                meta,
                &CreateAccount {
                    account: Account {
                        id: request.account_id,
                        kind: request.kind,
                        parent: Some(request.parent),
                        unit: request.unit,
                        status: AccountStatus::Active,
                    },
                },
            )
        },
        |account| json!({ "account": to_json(account) }),
    )
    .await
}

pub(super) async fn set_account_status(
    State(plane): Plane,
    Path(account): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "set_account_status",
        move |body| {
            let parsed = parse_body::<AccountStatusBody>(body)?;
            Ok((account_id(account)?, parsed.status))
        },
        |store, who, meta, (account, status)| {
            store.control_set_account_status(who, meta, &SetAccountStatus { account, status })
        },
        |account| json!({ "account": to_json(account) }),
    )
    .await
}

pub(super) async fn create_membership(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "create_membership",
        parse_body::<CreateMembershipBody>,
        |store, who, meta, request| {
            store.control_create_membership(
                who,
                meta,
                &CreateMembership {
                    membership: Membership {
                        id: request.membership_id,
                        principal: request.principal_id,
                        org: request.org,
                        role: request.role,
                        account: request.account,
                        status: MembershipStatus::Active,
                    },
                },
            )
        },
        |membership| json!({ "membership": to_json(membership) }),
    )
    .await
}

pub(super) async fn revoke_membership(
    State(plane): Plane,
    Path(membership): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "revoke_membership",
        move |body| {
            parse_body::<EmptyBody>(body)?;
            id("membership id", membership, MembershipId::new)
        },
        |store, who, meta, membership| {
            store.control_revoke_membership(who, meta, &RevokeMembership { membership })
        },
        |membership| json!({ "membership": to_json(membership) }),
    )
    .await
}

/// 签发出来的凭证，连同只出现一次的明文密钥。
struct Issued {
    credential: Credential,
    secret: String,
}

pub(super) async fn issue_credential(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 密钥在这里生成（不在存储层）：存储层只拿到 id 和哈希，明文只出现在这个函数和响应里。
    let key = MintedVirtualKey::mint();
    write_op(
        plane,
        headers,
        body,
        "issue_credential",
        |body| {
            let parsed = parse_body::<IssueCredentialBody>(body)?;
            validate_ceilings(&parsed.allowed_privacy, &parsed.allowed_roles)?;
            let owner = parsed
                .owner
                .clone()
                .unwrap_or_else(|| "control-plane".to_string());
            validate_owner(&owner)?;
            Ok((parsed, owner))
        },
        move |store, who, meta, (parsed, owner)| {
            let secret = key.secret.expose_secret().to_string();
            let written = store.control_issue_credential(
                who,
                meta,
                &IssueCredential {
                    key_id: key.key_id.clone(),
                    key_hash: key.hash,
                    binding: parsed.binding,
                    admin_scopes: parsed.admin_scopes,
                    expires_at: parsed.expires_at_ms.map(TimestampMs),
                    owner,
                    tenant_id: parsed.tenant_id,
                    allowed_privacy: parsed.allowed_privacy,
                    allowed_roles: parsed.allowed_roles,
                },
            )?;
            Ok(Written {
                result: written.result,
                value: Issued {
                    credential: written.value,
                    secret,
                },
            })
        },
        |issued| json!({ "credential": to_json(&issued.credential), "secret": issued.secret }),
    )
    .await
}

pub(super) async fn set_credential_status(
    State(plane): Plane,
    Path(credential): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "set_credential_status",
        move |body| {
            let parsed = parse_body::<CredentialStatusBody>(body)?;
            Ok((
                id("credential id", credential, CredentialId::new)?,
                parsed.status,
            ))
        },
        |store, who, meta, (credential, status): (CredentialId, CredentialStatus)| {
            store.control_set_credential_status(
                who,
                meta,
                &SetCredentialStatus { credential, status },
            )
        },
        |credential| json!({ "credential": to_json(credential) }),
    )
    .await
}

pub(super) async fn grant_allocation(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "grant_allocation",
        parse_body::<GrantAllocationBody>,
        |store, who, meta, request| {
            store.control_grant_allocation(
                who,
                meta,
                &GrantAllocation {
                    id: request.allocation_id,
                    parent: request.parent,
                    child: request.child,
                    amount: request.amount,
                    purpose: request.purpose,
                    valid_from: TimestampMs(request.valid_from_ms),
                    valid_until: TimestampMs(request.valid_until_ms),
                    month: request.month,
                },
            )
        },
        allocation_json,
    )
    .await
}

pub(super) async fn revoke_allocation(
    State(plane): Plane,
    Path(allocation): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "revoke_allocation",
        move |body| {
            parse_body::<EmptyBody>(body)?;
            id("allocation id", allocation, AllocationId::new)
        },
        |store, who, meta, allocation| {
            store.control_revoke_allocation(who, meta, &RevokeAllocation { id: allocation })
        },
        allocation_json,
    )
    .await
}

// ------------------------------------------------------------------------------------------------
// 配对
// ------------------------------------------------------------------------------------------------

/// 配对码记录连同只出现一次的明文码。
struct CreatedCode {
    record: idoris_tenancy::accounts::PairingRecord,
    code: String,
}

pub(super) async fn create_pairing_code(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let code = pairing_codes::mint();
    write_op(
        plane,
        headers,
        body,
        "create_pairing_code",
        |body| {
            let parsed = parse_body::<CreatePairingBody>(body)?;
            validate_ceilings(&parsed.allowed_privacy, &parsed.allowed_roles)?;
            Ok(parsed)
        },
        move |store, who, meta, parsed| {
            let ttl_seconds = i64::from(
                parsed.ttl_seconds.unwrap_or(
                    u32::try_from(idoris_tenancy::accounts::DEFAULT_PAIRING_TTL_MS / 1000)
                        .unwrap_or(600),
                ),
            );
            let device_ttl_ms = parsed
                .device_ttl_days
                .map(|days| i64::from(days).saturating_mul(86_400_000));
            let written = store.control_create_pairing_code(
                who,
                meta,
                &CreatePairingCode {
                    pairing_id: code.pairing_id.clone(),
                    code_hash: code.hash,
                    membership: parsed.membership,
                    ttl_ms: ttl_seconds.saturating_mul(1000),
                    allowed_privacy: parsed.allowed_privacy,
                    allowed_roles: parsed.allowed_roles,
                    tenant_id: parsed.tenant_id,
                    device_ttl_ms,
                },
            )?;
            Ok(Written {
                result: written.result,
                value: CreatedCode {
                    record: written.value,
                    code: code.display,
                },
            })
        },
        |created| json!({ "pairing": pairing_json(&created.record), "code": created.code }),
    )
    .await
}

fn pairing_id(raw: String) -> Result<String, String> {
    let valid = raw.len() == 19
        && raw.starts_with("pc_")
        && raw[3..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(raw)
    } else {
        Err("pairing id is not valid".to_string())
    }
}

pub(super) async fn revoke_pairing_code(
    State(plane): Plane,
    Path(pairing): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "revoke_pairing_code",
        move |body| {
            parse_body::<EmptyBody>(body)?;
            pairing_id(pairing)
        },
        |store, who, meta, pairing_id| {
            store.control_revoke_pairing_code(who, meta, &RevokePairingCode { pairing_id })
        },
        |record| json!({ "pairing": pairing_json(record) }),
    )
    .await
}

/// 设备自己解绑：撤销调用者自己的凭证。
pub(super) async fn unpair_device(
    State(plane): Plane,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    write_op(
        plane,
        headers,
        body,
        "unpair_device",
        parse_body::<EmptyBody>,
        |store, who, meta, EmptyBody {}| store.control_unpair_device(who, meta),
        |credential| json!({ "credential": to_json(credential) }),
    )
    .await
}

/// 兑换配对码：**没有凭证**，码本身就是凭据。先过全局的失败限速，再把输入规范化、哈希、交给存储层。
pub(super) async fn redeem_pairing_code(
    State(plane): Plane,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = new_request_id();
    let now = plane.clock.now();
    if !plane.redeem_limiter.allow(now.0) {
        return failure_response(
            &request_id,
            &Failure::new(ControlReason::PairingRateLimited),
        );
    }
    let parsed = match parse_body::<RedeemBody>(&body) {
        Ok(parsed) => parsed,
        Err(detail) => {
            return failure_response(
                &request_id,
                &Failure::with_detail(ControlReason::InvalidRequest, detail),
            );
        }
    };
    // 长度或字符集不对的输入当作「猜错了」：同样计入失败限速，并且和「查无此码」同一个答案。
    let Some(code_hash) = pairing_codes::hash_input(&parsed.code) else {
        plane.redeem_limiter.record_failure(now.0);
        return failure_response(
            &request_id,
            &Failure::new(ControlReason::PairingCodeInvalid),
        );
    };
    let key = MintedVirtualKey::mint();
    let accounts = plane.accounts.clone();
    let rid = request_id.clone();
    let label = parsed.device_label;
    let outcome = tokio::task::spawn_blocking(move || {
        let secret = key.secret.expose_secret().to_string();
        let meta = Meta {
            now,
            request_id: &rid,
        };
        accounts
            .control_redeem_pairing_code(
                &meta,
                &RedeemPairingCode {
                    code_hash,
                    key_id: key.key_id.clone(),
                    key_hash: key.hash,
                    label,
                },
            )
            .map(|written| (written, secret))
    })
    .await;
    match outcome {
        Ok(Ok((written, secret))) => written_response(
            &request_id,
            written.result,
            json!({
                "credential": to_json(&written.value.credential),
                "secret": secret,
                "membership": written.value.membership,
                "pairing_id": written.value.pairing_id,
            }),
        ),
        Ok(Err(failure)) => {
            if failure.reason == ControlReason::PairingCodeInvalid {
                plane.redeem_limiter.record_failure(now.0);
            }
            failure_response(&request_id, &failure)
        }
        Err(_) => blocking_failed(&request_id),
    }
}

// ================================================================================================
// 读
// ================================================================================================

pub(super) async fn whoami(State(plane): Plane, headers: HeaderMap) -> Response {
    read_op(
        plane,
        headers,
        Ok(()),
        |store, who, now, ()| store.control_whoami(who, now),
        |info| {
            json!({
                "credential": to_json(&info.credential),
                "data_plane": {
                    "owner": info.ceilings.owner,
                    "tenant_id": info.ceilings.tenant_id,
                    "allowed_privacy": info.ceilings.allowed_privacy,
                    "allowed_roles": info.ceilings.allowed_roles,
                    "expires_at_ms": info.ceilings.expires_at_ms,
                },
            })
        },
    )
    .await
}

pub(super) async fn read_account(
    State(plane): Plane,
    Path(account): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_op(
        plane,
        headers,
        account_id(account),
        |store, who, now, account| store.control_read_account(who, now, &account),
        |view| json!({ "account": to_json(&view.account), "children": view.children }),
    )
    .await
}

pub(super) async fn read_subtree(
    State(plane): Plane,
    Path(account): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_op(
        plane,
        headers,
        account_id(account),
        |store, who, now, account| store.control_read_subtree(who, now, &account),
        |tree| json!({ "accounts": to_json(&tree.accounts), "truncated": tree.truncated }),
    )
    .await
}

pub(super) async fn read_allocations(
    State(plane): Plane,
    Path(account): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_op(
        plane,
        headers,
        account_id(account),
        |store, who, now, account| store.control_read_allocations(who, now, &account),
        |view| {
            json!({
                "inbound": view.inbound.iter().map(allocation_json).collect::<Vec<_>>(),
                "outbound": view.outbound.iter().map(allocation_json).collect::<Vec<_>>(),
                "truncated": view.truncated,
            })
        },
    )
    .await
}

pub(super) async fn read_credentials(
    State(plane): Plane,
    Path(account): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_op(
        plane,
        headers,
        account_id(account),
        |store, who, now, account| store.control_read_credentials(who, now, &account),
        |view| {
            let credentials: Vec<Value> = view
                .credentials
                .iter()
                .map(|entry| {
                    json!({
                        "credential": to_json(&entry.credential),
                        "key_status": entry.key_status,
                        "issued_by": entry.issued_by.as_ref().map(|(kind, id)| json!({ "kind": kind, "id": id })),
                        "device": entry.device.as_ref().map(|device| json!({
                            "pairing_id": device.pairing_id,
                            "label": device.label,
                            "paired_at_ms": device.paired_at_ms,
                        })),
                    })
                })
                .collect();
            json!({ "credentials": credentials, "truncated": view.truncated })
        },
    )
    .await
}

pub(super) async fn read_memberships(
    State(plane): Plane,
    Path(account): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_op(
        plane,
        headers,
        account_id(account),
        |store, who, now, account| store.control_read_memberships(who, now, &account),
        |view| json!({ "memberships": to_json(&view.memberships), "truncated": view.truncated }),
    )
    .await
}

pub(super) async fn read_principal(
    State(plane): Plane,
    Path(principal): Path<String>,
    headers: HeaderMap,
) -> Response {
    read_op(
        plane,
        headers,
        id("principal id", principal, PrincipalId::new),
        |store, who, now, principal| store.control_read_principal(who, now, &principal),
        |principal| json!({ "principal": principal_json(&principal) }),
    )
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AuditQuery {
    account: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    before: Option<i64>,
}

pub(super) async fn read_audit(
    State(plane): Plane,
    query: Result<Query<AuditQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let params = match query {
        Ok(Query(query)) => account_id(query.account)
            .map(|account| (account, query.limit.unwrap_or(50), query.before)),
        Err(_) => Err("query must be `account=<id>[&limit=<n>][&before=<audit_id>]`".to_string()),
    };
    read_op(
        plane,
        headers,
        params,
        |store, who, now, (account, limit, before)| {
            store.control_read_audit(who, now, &account, limit, before)
        },
        |view| {
            let rows: Vec<Value> = view
                .rows
                .iter()
                .map(|row| {
                    json!({
                        "audit_id": row.audit_id,
                        "at_ms": row.at_ms,
                        "request_id": row.request_id,
                        "actor": { "kind": row.actor_kind, "id": row.actor_id },
                        "op": row.op,
                        "target": { "kind": row.target_kind, "id": row.target_id },
                        "scope_account": row.scope_account,
                        "related_id": row.related_id,
                        "amount": row.amount,
                        "outcome": row.outcome,
                        "reason_code": row.reason_code,
                    })
                })
                .collect();
            json!({ "rows": rows, "has_more": view.has_more })
        },
    )
    .await
}

/// 控制面前缀下没有的路径和方法。
pub(super) async fn not_found_handler() -> Response {
    not_found_response(&new_request_id())
}
