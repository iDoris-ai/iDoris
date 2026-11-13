//! 控制面的线上形状：请求体（严格：未知字段一律拒绝）、成功与失败的响应信封、原因码到 HTTP 状态的映射。
//!
//! # 响应信封
//!
//! 成功（写操作）：
//!
//! ```json
//! { "result": "created", "reason_code": "ok", "request_id": "…", "data": { … } }
//! ```
//!
//! `result` 是 `created`（201）/ `replayed` / `updated` / `unchanged`（200）之一；`reason_code` 成功时恒为 `ok`。
//! 失败：
//!
//! ```json
//! { "error": { "type": "forbidden", "reason_code": "out_of_scope", "message": "…", "request_id": "…" } }
//! ```
//!
//! `reason_code` 是 `idoris-compute-contracts::control::ControlReason` 的稳定字符串；`type` 是 HTTP 状态的粗分类。
//! 每个响应都带 `Cache-Control: no-store`（有的响应里有明文密钥）和 `X-iDoris-Record-Id`（= `request_id`，
//! 与审计行的 `request_id` 对得上）。

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use idoris_compute_contracts::account::{
    AccountKind, AccountStatus, MembershipRole, Principal, PrincipalStatus,
};
use idoris_compute_contracts::control::{ControlReason, WriteResult};
use idoris_compute_contracts::credential::{AdminScope, CredentialBinding, CredentialStatus};
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, BillingMonth, MembershipId, PrincipalId,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::accounts::{AllocationRecord, Failure, PairingRecord};
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) const HEADER_RECORD_ID: &str = "X-iDoris-Record-Id";

// ------------------------------------------------------------------------------------------------
// 请求体
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreatePrincipalBody {
    pub principal_id: PrincipalId,
    pub org: AccountId,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevokePrincipalBody {
    pub org: AccountId,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateAccountBody {
    pub account_id: AccountId,
    pub kind: AccountKind,
    pub parent: AccountId,
    pub unit: Unit,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccountStatusBody {
    pub status: AccountStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateMembershipBody {
    pub membership_id: MembershipId,
    pub principal_id: PrincipalId,
    pub org: AccountId,
    pub role: MembershipRole,
    #[serde(default)]
    pub account: Option<AccountId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IssueCredentialBody {
    pub binding: CredentialBinding,
    #[serde(default)]
    pub admin_scopes: Vec<AdminScope>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub owner: Option<String>,
    /// 缺省 = 继承授权者的租户绑定。
    #[serde(default)]
    pub tenant_id: Option<String>,
    pub allowed_privacy: Vec<PrivacyClass>,
    #[serde(default)]
    pub allowed_roles: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialStatusBody {
    pub status: CredentialStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GrantAllocationBody {
    pub allocation_id: AllocationId,
    pub parent: AccountId,
    pub child: AccountId,
    pub amount: Money,
    pub purpose: Purpose,
    /// 有效窗口 `[valid_from_ms, valid_until_ms)`。**免费额度**（`free_tier`）的窗口必须恰好是 `month` 在账本时区里的窗口，
    /// 对不上是 400 `invalid_allocation`（错误信息里写着应有的两个值）：月份和窗口不能脱钩，否则同一个主体换个月份标签
    /// 就能在同一段时间里拿到两份。
    pub valid_from_ms: i64,
    pub valid_until_ms: i64,
    /// 免费额度（`free_tier`）的账期 `YYYY-MM`；幂等键 `(成员, 月份)` 由服务端生成。免费额度在同一个事务里还要过补贴池
    /// 该账期的总上限（没设上限 = 0，发不出去；超出是 409 `allocation_exceeds_grantor`）并登记承诺。
    #[serde(default)]
    pub month: Option<BillingMonth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreatePairingBody {
    pub membership: MembershipId,
    #[serde(default)]
    pub ttl_seconds: Option<u32>,
    pub allowed_privacy: Vec<PrivacyClass>,
    #[serde(default)]
    pub allowed_roles: Vec<String>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub device_ttl_days: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RedeemBody {
    pub code: String,
    pub device_label: String,
}

/// 空对象 `{}`（或空请求体）：没有参数的写操作（解绑自己、撤销某个资源）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EmptyBody {}

/// 解析请求体。空请求体当作 `{}`（对没有参数的操作方便），其余必须是合法的、没有多余字段的 JSON。
pub(super) fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, String> {
    let body = if body.iter().all(u8::is_ascii_whitespace) {
        b"{}".as_slice()
    } else {
        body
    };
    serde_json::from_slice(body).map_err(|error| truncate(error.to_string()))
}

fn truncate(mut text: String) -> String {
    const MAX: usize = 200;
    if text.len() > MAX {
        let mut cut = MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// 数据面上限的形状：隐私等级至少一个、不重复；角色必须是已知角色、不重复。
pub(super) fn validate_ceilings(privacy: &[PrivacyClass], roles: &[String]) -> Result<(), String> {
    if privacy.is_empty() {
        return Err("allowed_privacy must contain at least one privacy class".into());
    }
    if privacy
        .iter()
        .enumerate()
        .any(|(index, class)| privacy[..index].contains(class))
    {
        return Err("allowed_privacy must not repeat a privacy class".into());
    }
    for (index, role) in roles.iter().enumerate() {
        if !idoris_policy::ROLES
            .iter()
            .any(|known| known.as_str() == role)
        {
            return Err("allowed_roles contains an unknown role".into());
        }
        if roles[..index].contains(role) {
            return Err("allowed_roles must not repeat a role".into());
        }
    }
    Ok(())
}

/// `owner` 标签（`virtual_keys.owner`）：1 到 128 字节、没有首尾空白、没有控制字符。
pub(super) fn validate_owner(owner: &str) -> Result<(), String> {
    if owner.is_empty()
        || owner.len() > 128
        || owner.trim() != owner
        || owner.chars().any(char::is_control)
    {
        return Err(
            "owner must be 1 to 128 bytes without control characters or surrounding space".into(),
        );
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// 响应
// ------------------------------------------------------------------------------------------------

/// 原因码对应的 HTTP 状态。
pub(super) fn status_for(reason: ControlReason) -> StatusCode {
    use ControlReason::*;
    match reason {
        Unauthenticated | CredentialRevoked | CredentialFrozen | CredentialExpired => {
            StatusCode::UNAUTHORIZED
        }
        NotAControlCredential
        | AccountInactive
        | OpNotGranted
        | OutOfScope
        | AmountExceedsAuthority
        | AmountUnitMismatch
        | ScopeExceedsGrantor
        | ScopeOutsideBinding
        | CeilingExceedsGrantor
        | ExpiryExceedsGrantor
        | WouldDisableCaller
        | NotADevice => StatusCode::FORBIDDEN,
        InvalidRequest | InvalidAccount | InvalidMembership | InvalidCredential
        | InvalidAllocation | InvalidAmount | SubsidyKeyInvalid | PairingCodeInvalid => {
            StatusCode::BAD_REQUEST
        }
        AlreadyExists
        | IdempotencyConflict
        | IllegalTransition
        | AllocationRevoked
        | AllocationExceedsGrantor
        | GrantorHasNoAuthority
        | PrincipalRevoked
        | MembershipRevoked
        | PairingCodeUsed
        | DeviceLimitReached => StatusCode::CONFLICT,
        PairingCodeExpired | PairingCodeRevoked => StatusCode::GONE,
        PairingRateLimited => StatusCode::TOO_MANY_REQUESTS,
        StoreBusy | AuditUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        InternalError => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::CONFLICT => "conflict",
        StatusCode::GONE => "gone",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        StatusCode::NOT_FOUND => "not_found",
        _ => "internal_error",
    }
}

/// 给调用方看的一句话（固定文案，不含任何资源信息）。需要细节的驳回由 `Failure::detail` 补充。
fn message_for(reason: ControlReason) -> &'static str {
    use ControlReason::*;
    match reason {
        Unauthenticated => "authentication failed",
        CredentialRevoked => "this credential has been revoked",
        CredentialFrozen => "this credential is frozen",
        CredentialExpired => "this credential has expired",
        AccountInactive => "the account this credential is bound to is frozen or closed",
        NotAControlCredential => "this key is not a control-plane credential",
        OpNotGranted => "this credential has no permission for this operation",
        OutOfScope => "the target is outside this credential's scope",
        AmountExceedsAuthority => "the amount exceeds what this credential may grant at once",
        AmountUnitMismatch => "the amount's unit differs from the unit of this credential's limit",
        ScopeExceedsGrantor => "the new credential would carry more authority than its issuer",
        ScopeOutsideBinding => {
            "a scope of the new credential lies outside the account it is bound to"
        }
        CeilingExceedsGrantor => "the new credential's data-plane ceilings exceed its issuer's",
        ExpiryExceedsGrantor => "the new credential would outlive its issuer",
        WouldDisableCaller => "this would disable the calling credential itself",
        InvalidRequest => "the request is not valid",
        InvalidAccount => "the account is not valid",
        InvalidMembership => "the membership is not valid",
        InvalidCredential => "the credential is not valid",
        InvalidAllocation => "the allocation is not valid",
        InvalidAmount => "the amount is not valid",
        PrincipalRevoked => "the principal has been revoked",
        MembershipRevoked => "the membership has been revoked",
        AlreadyExists => "a different resource with this id already exists",
        IdempotencyConflict => "this idempotency key was already used for a different allocation",
        IllegalTransition => "this status change is not allowed",
        AllocationExceedsGrantor => "the allocation exceeds what the granting account itself holds",
        GrantorHasNoAuthority => "the granting account holds no allocation that covers this one",
        AllocationRevoked => "this allocation has been revoked",
        SubsidyKeyInvalid => "the free-tier allocation cannot be keyed to a member",
        PairingCodeInvalid => "the pairing code is not valid",
        PairingCodeExpired => "the pairing code has expired",
        PairingCodeUsed => "the pairing code has already been used",
        PairingCodeRevoked => "the pairing code has been revoked",
        PairingRateLimited => "too many failed pairing attempts; try again later",
        DeviceLimitReached => "this account already has the maximum number of devices",
        NotADevice => "this credential is not a paired device",
        StoreBusy => "the service is busy; retry",
        AuditUnavailable => "the audit log is unavailable, so nothing was changed; retry",
        InternalError => "the service cannot complete this request right now",
    }
}

fn finish(mut response: Response, request_id: &str) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert(HEADER_RECORD_ID, value);
    }
    response
}

/// 失败响应。`Failure::detail` 只在驳回类（请求不合法、冲突）里带上；授权类一律只有固定文案。
pub(super) fn failure_response(request_id: &str, failure: &Failure) -> Response {
    let status = status_for(failure.reason);
    let show_detail = !failure.reason.is_denial()
        && failure.reason != ControlReason::InternalError
        && failure.reason != ControlReason::Unauthenticated
        && !failure.detail.is_empty();
    let message = if show_detail {
        failure.detail.clone()
    } else {
        message_for(failure.reason).to_string()
    };
    let body = json!({
        "error": {
            "type": error_type(status),
            "reason_code": failure.reason.as_str(),
            "message": message,
            "request_id": request_id,
        }
    });
    let mut response = (status, Json(body)).into_response();
    if matches!(
        failure.reason,
        ControlReason::PairingRateLimited
            | ControlReason::StoreBusy
            | ControlReason::AuditUnavailable
    ) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    finish(response, request_id)
}

/// 路由不存在（控制面前缀下的任何别的路径或方法）。
pub(super) fn not_found_response(request_id: &str) -> Response {
    let body = json!({
        "error": {
            "type": error_type(StatusCode::NOT_FOUND),
            "reason_code": "not_found",
            "message": "no such control-plane route",
            "request_id": request_id,
        }
    });
    finish(
        (StatusCode::NOT_FOUND, Json(body)).into_response(),
        request_id,
    )
}

/// 写操作成功。
pub(super) fn written_response(request_id: &str, result: WriteResult, data: Value) -> Response {
    let status = if result == WriteResult::Created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let body = json!({
        "result": result.as_str(),
        "reason_code": "ok",
        "request_id": request_id,
        "data": data,
    });
    finish((status, Json(body)).into_response(), request_id)
}

/// 读操作成功。
pub(super) fn read_response(request_id: &str, data: Value) -> Response {
    let body = json!({ "request_id": request_id, "data": data });
    finish((StatusCode::OK, Json(body)).into_response(), request_id)
}

// ------------------------------------------------------------------------------------------------
// 数据的 JSON 视图（S0 契约类型自己的 serde 形状；永远没有密钥或验证器哈希）
// ------------------------------------------------------------------------------------------------

pub(super) fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

pub(super) fn principal_json(principal: &Principal) -> Value {
    json!({
        "principal_id": principal.id,
        "status": match principal.status {
            PrincipalStatus::Active => "active",
            PrincipalStatus::Revoked => "revoked",
        },
    })
}

pub(super) fn allocation_json(record: &AllocationRecord) -> Value {
    json!({
        "allocation": record.allocation,
        "revoked_at_ms": record.revoked_at_ms,
    })
}

pub(super) fn pairing_json(record: &PairingRecord) -> Value {
    json!({
        "pairing_id": record.pairing_id,
        "membership": record.membership,
        "issued_by": record.issued_by,
        "created_at_ms": record.created_at_ms,
        "expires_at_ms": record.expires_at_ms,
        "status": record.status.as_str(),
        "redeemed_credential": record.redeemed_credential,
        "device_ceilings": {
            "allowed_privacy": record.device_privacy,
            "allowed_roles": record.device_roles,
            "tenant_id": record.device_tenant_id,
            "device_ttl_ms": record.device_ttl_ms,
        },
    })
}
