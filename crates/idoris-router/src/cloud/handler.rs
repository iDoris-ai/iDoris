//! 云上游的 HTTP 处理：`POST /v1/chat/completions`（仅当云上游开关打开时由 `build_app` 挂载，替代本地流水线）；
//! 云形态下由云入口（S5）挂在 `POST /api/v1/chat/completions`。
//!
//! **调用方**（付款上下文、租户数据处理要求、计费性质）有两个来源：
//!
//! - **云入口（S5，生产）**：`ingress` 的中间件已经认证了凭证、从凭证推导出付款上下文和租户，并把 [`CallerPolicy`]
//!   放进请求扩展；本处理函数直接用它，不看请求里的任何东西（`X-iDoris-Tenant` 在入口最外层就被删了）。
//! - **静态调用方簿（S4 的测试夹具）**：没有云入口、用 `build_app` 直接挂这个处理函数的测试，沿用既有的中间件
//!   （`remote_bind_auth_middleware`）放进扩展里的 `AuthenticatedVirtualKey.tenant_id` 在簿里找
//!   （见 [`super::config::CallerBook`]）。生产里簿是空的（`CloudIngress::new` 拒绝非空的簿），而且云入口总会设置扩展，
//!   所以走不到这里；走到了也只会得到 403 `caller_not_configured`（失败即关闭）。
//!
//! 响应（稳定的原因码，设计稿 §5）：
//!
//! | 情形 | 状态码 | `error.type` / 头 |
//! |---|---|---|
//! | 成功（非流式），正常 | 200 | 上游响应体原样；`X-iDoris-Tier: normal`，没有原因码 |
//! | 成功，压低了 `max_tokens`（第 2 级） | 200 | `X-iDoris-Reason: budget_degraded`、`X-iDoris-Degraded: true`、`X-iDoris-Tier: degraded`、`X-iDoris-Max-Tokens: <发给上游的值>` |
//! | 成功，宽限一轮（第 3 级） | 200 | `X-iDoris-Reason: grace_used`、`X-iDoris-Tier: grace`（担保人在账本里有一条可查询的告警） |
//! | 成功（流式） | 200 `text/event-stream` | 同上；上游的流内 usage 块只在客户端要了 `include_usage` 时转发 |
//! | 没有合规候选 | 503 | `no_compliant_endpoint` |
//! | 准入拒绝：钱不够（第 4 级） | 402 | `budget_exhausted`；`X-iDoris-Reason: budget_exhausted`、`X-iDoris-Tier: exhausted` |
//! | 准入拒绝：平台保护 | 429 `rate_limited` / `in_flight_limited` / `subsidy_pool_exhausted`；其余 403 | 原因码（`X-iDoris-Reason`），没有 `X-iDoris-Tier`（不在四级阶梯上） |
//! | 所有上游都失败 | 502（超时 504） | `upstream_error`，`reason_code` 是最后一次失败的稳定代码 |
//! | 流中途失败（首字节之后） | 状态码已经是 200 | 流以一个 `{"error":{"type":"upstream_error",...}}` 事件结束，**没有** `[DONE]` |
//!
//! 体验分级的头全部列在 [`EXPOSED_HEADERS`] 里，并写进每个带它们的响应的 `Access-Control-Expose-Headers`，浏览器的 JS
//! 读得到（设计稿 §5，评审 L2）；跨域放行本身（`Access-Control-Allow-Origin`、预检）是云入口 S5 的事（`ingress::cors`，复用这个列表）。
//!
//! edu（`child_edu`）租户在 P0 **一律非流式**（设计稿 §6）：流式请求在任何上游调用之前就被拒绝
//! （400 `streaming_not_allowed`）。儿童安全管线（S6）在执行器里：输入检查 -> 上游非流式调用 -> 完整输出检查，
//! 通过之后才有可发布的回答；这里只负责把结果翻译成 HTTP：
//!
//! | 情形 | 状态码 | `error.type` / `reason_code` / 头 |
//! |---|---|---|
//! | 输入检查拒绝（上游没有收到任何字节，用户付 0） | 422 | `safety_input_blocked`；`stage` / `category`；`X-iDoris-Reason` |
//! | 完整输出检查拒绝（回答没有发给客户端，用户付 0，成本由平台承担） | 422 | `safety_output_blocked`；同上 |
//! | 安全检查没能给出结论（超时、出错、返回无法解析、没有配置……），失败即关闭 | 503 | `safety_unavailable`；`stage` / `cause`；`X-iDoris-Reason` |
//!
//! 给 eduai 的是稳定的码，**不回显被拒的内容**；`category`（自伤、成人内容……）只在这次响应里给，让 eduai 选对给孩子的话术，
//! 不写进任何账本、日志或审计。码同时在 `X-iDoris-Reason` 头里，和体验分级的头同一套 [`EXPOSED_HEADERS`]（浏览器读得到）。
//! 安全检查本身的成本被准入拒绝（余额、速率、在途、补贴池……）时，返回准入类的原因码（402 / 429 / 403，同上表），不检查、不出网。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use idoris_compute_contracts::ids::RequestId;
use idoris_compute_contracts::reason::{ExperienceTier, ReasonCode};
use idoris_compute_contracts::safety::{
    SafetyCategory, SafetyReasonCode, SafetyStage, SafetyUnavailableCause,
};
use idoris_tenancy::virtual_key::store::AuthenticatedVirtualKey;

use super::answer::{AnswerMeta, CloudOutcome};
use super::config::CallerPolicy;
use super::executor::{CloudCall, CloudError};
use super::request::{InvalidPrivacy, RequestProblem, declared_privacy, parse_client_request};
use crate::connection::RequestLifecycle;
use crate::{
    AppState, HEADER_DEGRADED, HEADER_REASON, HEADER_RECORD_ID, RequestRecordId, error_envelope,
    error_envelope_with_reason,
};

pub(crate) async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    Extension(lifecycle): Extension<RequestLifecycle>,
    identity: Option<Extension<AuthenticatedVirtualKey>>,
    resolved: Option<Extension<CallerPolicy>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(cloud) = state.cloud.as_ref() else {
        // build_app 只在 cloud 存在时才挂这个处理函数
        return error_envelope(StatusCode::NOT_FOUND, "not_found", "route not available");
    };
    let request = match parse_client_request(&body) {
        Ok(request) => request,
        Err(problem) => return problem_response(problem),
    };
    // 隐私声明（`X-iDoris-Privacy` 或请求体里的等价字段）：不合法就拒绝（失败即关闭，不当作没声明）。
    // 显式 `local_only` 交给执行器：它在任何候选选择、准入、安全检查和上游请求之前拒绝（并留一行审计记录）。
    let privacy = match declared_privacy(&headers, &request.body) {
        Ok(privacy) => privacy,
        Err(InvalidPrivacy) => {
            return error_envelope_with_reason(
                StatusCode::BAD_REQUEST,
                "invalid_privacy",
                "invalid_privacy",
                "X-iDoris-Privacy must be a single, non-blank value: any|local_only",
            );
        }
    };
    let caller = match resolved {
        // 云入口（S5）从凭证推导出来的调用方
        Some(Extension(caller)) => caller,
        // 静态调用方簿：测试夹具（生产里是空的）
        None => {
            let tenant_id = identity
                .as_ref()
                .and_then(|Extension(identity)| identity.scope.tenant_id.as_deref());
            match cloud.callers.resolve(tenant_id) {
                Some(caller) => caller.clone(),
                None => {
                    return error_envelope(
                        StatusCode::FORBIDDEN,
                        "caller_not_configured",
                        "this credential has no cloud payment context",
                    );
                }
            }
        }
    };
    let Some(candidates) = cloud.routes.candidates(&request.model) else {
        return error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "unsupported_field",
            "unsupported_model",
            "the requested model is not served by this endpoint",
        );
    };
    // edu 一律非流式（设计稿 §6）：先于任何上游调用拒绝（执行器里还有一道同样的检查）
    if request.stream && caller.tenant.handles_child_data() {
        return error_response(&CloudError::StreamingNotAllowed);
    }
    let Ok(request_id) = RequestId::new(record_id.clone()) else {
        return error_envelope(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "request id is invalid",
        );
    };
    let call = CloudCall {
        request_id,
        payment: caller.payment.clone(),
        tenant: caller.tenant.clone(),
        billing: caller.billing,
        privacy,
        client_body: request.body,
        stream: request.stream,
        client_wants_usage_chunk: request.wants_usage_chunk,
        requested_max_tokens: request.requested_max_tokens,
        input_tokens_estimate: request.input_tokens_estimate,
        candidates: candidates.to_vec(),
        needs: request.needs,
        cancel: lifecycle.cancellation_token(),
    };
    match cloud.executor.execute(call).await {
        Err(error) => error_response(&error),
        Ok(CloudOutcome::Complete(pending)) => match pending.publish() {
            Ok(answer) => {
                let mut response = (StatusCode::OK, answer.body).into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                apply_reason_headers(&mut response, &answer.meta);
                response
            }
            Err(error) => error_response(&error),
        },
        Ok(CloudOutcome::Stream(stream)) => {
            let meta = stream.meta().clone();
            let mut response = Response::new(Body::from_stream(stream));
            *response.status_mut() = StatusCode::OK;
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream; charset=utf-8"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
            apply_reason_headers(&mut response, &meta);
            response
        }
    }
}

/// 体验分级（设计稿 §5）的级别，响应头 `X-iDoris-Tier`：`normal` / `degraded` / `grace` / `exhausted`。
/// 不在四级阶梯上的拒绝（`rate_limited`、`credential_frozen`……）没有这个头：它们不是「钱不够」。
pub const HEADER_TIER: &str = "X-iDoris-Tier";

/// 钱不够压低了 `max_tokens` 时，实际发给上游的 `max_tokens`（回答可能因为长度被截断），响应头
/// `X-iDoris-Max-Tokens`。没压低就没有这个头。
pub const HEADER_MAX_TOKENS: &str = "X-iDoris-Max-Tokens";

/// 浏览器要读到的响应头，即 CORS 的 `Access-Control-Expose-Headers`（设计稿 §5 评审 L2：降级响应头是契约，
/// 浏览器能读）。浏览器默认只暴露 CORS 安全的几个头，不列在这里的头前端的 JS 读不到。
///
/// 这个列表会写进**每个带原因码 / 级别头的云响应**的 `Access-Control-Expose-Headers`（对不是跨域的调用无害）。
/// 真正的跨域放行（`Access-Control-Allow-Origin`、预检）是云入口（S5）的事，S5 的 CORS 层应当用这个列表，
/// 不要另列一份。
pub const EXPOSED_HEADERS: &[&str] = &[
    HEADER_REASON,
    HEADER_DEGRADED,
    HEADER_TIER,
    HEADER_MAX_TOKENS,
    HEADER_RECORD_ID,
];

fn expose_headers(response: &mut Response) {
    if let Ok(value) = HeaderValue::from_str(&EXPOSED_HEADERS.join(", ")) {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
}

fn set_tier(response: &mut Response, tier: ExperienceTier) {
    let text = match tier {
        ExperienceTier::Normal => "normal",
        ExperienceTier::Degraded => "degraded",
        ExperienceTier::Grace => "grace",
        ExperienceTier::Exhausted => "exhausted",
    };
    response
        .headers_mut()
        .insert(HEADER_TIER, HeaderValue::from_static(text));
}

fn apply_reason_headers(response: &mut Response, meta: &AnswerMeta) {
    // 被服务的请求总有级别：没有原因码就是正常（第 1 级），否则取原因码里最高的那一级
    // （降级 < 宽限；首字节前切换是降级）。
    let tier = meta
        .reasons
        .iter()
        .filter_map(|code| code.tier())
        .max()
        .unwrap_or(ExperienceTier::Normal);
    set_tier(response, tier);
    if let Some(max_tokens) = meta.max_tokens_reduced_to
        && let Ok(value) = HeaderValue::from_str(&max_tokens.to_string())
    {
        response.headers_mut().insert(HEADER_MAX_TOKENS, value);
    }
    expose_headers(response);
    if meta.reasons.is_empty() {
        return;
    }
    let joined = meta
        .reasons
        .iter()
        .map(|code| code.as_str())
        .collect::<Vec<_>>()
        .join(",");
    if let Ok(value) = HeaderValue::from_str(&joined) {
        response.headers_mut().insert(HEADER_REASON, value);
    }
    if meta
        .reasons
        .iter()
        .any(|code| code.tier() == Some(ExperienceTier::Degraded))
    {
        response
            .headers_mut()
            .insert(HEADER_DEGRADED, HeaderValue::from_static("true"));
    }
}

fn problem_response(problem: RequestProblem) -> Response {
    match problem {
        RequestProblem::MissingModel => error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "unsupported_field",
            "unsupported_model",
            problem.message(),
        ),
        _ => error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            problem_type(problem),
            problem.code(),
            problem.message(),
        ),
    }
}

fn problem_type(problem: RequestProblem) -> &'static str {
    match problem {
        RequestProblem::NotJson => "invalid_json",
        RequestProblem::NotAnObject => "invalid_body",
        RequestProblem::UnsupportedCompletionCount => "unsupported_completion_count",
        RequestProblem::InvalidMaxTokens(_) => "invalid_completion_cap",
        _ => "invalid_request",
    }
}

/// 准入拒绝对应的 HTTP 状态：钱不够是 402，限流类是 429，其余 403。
fn denial_status(code: ReasonCode) -> StatusCode {
    match code {
        ReasonCode::BudgetExhausted => StatusCode::PAYMENT_REQUIRED,
        ReasonCode::RateLimited
        | ReasonCode::InFlightLimited
        | ReasonCode::SubsidyPoolExhausted => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::FORBIDDEN,
    }
}

/// 拒绝类原因码也放进响应头：浏览器读不到响应体里的 `error.reason_code`，但能读响应头（设计稿 §5，评审 L2）。
/// 在四级阶梯上的拒绝（`budget_exhausted` = 第 4 级）同时带 `X-iDoris-Tier: exhausted`。
fn with_reason_header(mut response: Response, code: ReasonCode) -> Response {
    response
        .headers_mut()
        .insert(HEADER_REASON, HeaderValue::from_static(code.as_str()));
    if let Some(tier) = code.tier() {
        set_tier(&mut response, tier);
    }
    expose_headers(&mut response);
    response
}

pub(crate) fn error_response(error: &CloudError) -> Response {
    match error {
        CloudError::NoCompliantEndpoint(_) => with_reason_header(
            error_envelope(
                StatusCode::SERVICE_UNAVAILABLE,
                ReasonCode::NoCompliantEndpoint.as_str(),
                "no upstream endpoint is permitted to process this request",
            ),
            ReasonCode::NoCompliantEndpoint,
        ),
        CloudError::Denied(code) => with_reason_header(
            error_envelope(
                denial_status(*code),
                code.as_str(),
                "the request was not admitted",
            ),
            *code,
        ),
        CloudError::NoPricedEndpoint => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_priced_endpoint",
            "no permitted upstream endpoint is priced for this account",
        ),
        CloudError::EndpointNotConfigured => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "endpoint_not_configured",
            "a permitted upstream endpoint is not configured",
        ),
        CloudError::InvalidRequest(_) => error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid_request_body",
            "the request body cannot be sent upstream",
        ),
        CloudError::UpstreamRejected { status } => {
            let (code, reason) = if matches!(*status, 400 | 413 | 422) {
                (StatusCode::BAD_REQUEST, "upstream_rejected_request")
            } else {
                (StatusCode::BAD_GATEWAY, "upstream_rejected_request")
            };
            error_envelope_with_reason(
                code,
                "upstream_error",
                reason,
                "the upstream refused this request",
            )
        }
        CloudError::UpstreamUnavailable { last, .. } => {
            let timeout = last.is_some_and(|l| l.timeout);
            error_envelope_with_reason(
                if timeout {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "upstream_error",
                last.map_or("upstream_unavailable", |l| l.code),
                "no upstream endpoint could serve this request",
            )
        }
        CloudError::DeadlineExceeded => error_envelope_with_reason(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_error",
            "deadline_exceeded",
            "the request deadline was exceeded",
        ),
        CloudError::Cancelled => error_envelope(
            StatusCode::from_u16(499).unwrap_or(StatusCode::BAD_REQUEST),
            "client_cancelled",
            "the client closed the request",
        ),
        CloudError::StreamingNotAllowed => error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "streaming_not_allowed",
            "streaming_not_allowed",
            "streaming is not available for this tenant; send a non-streaming request",
        ),
        CloudError::LocalOnlyRefused => {
            // 调用方说这份数据不许离开它的机器；云是远端，永远不处理。码同时在 `X-iDoris-Reason` 里（浏览器读得到）
            let mut response = error_envelope_with_reason(
                StatusCode::FORBIDDEN,
                "local_only_refused",
                "local_only_refused",
                "this request declares privacy local_only; the cloud service never processes it and nothing was sent to any model",
            );
            response.headers_mut().insert(
                HEADER_REASON,
                HeaderValue::from_static("local_only_refused"),
            );
            expose_headers(&mut response);
            response
        }
        CloudError::SafetyBlocked { stage, category } => safety_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SafetyReasonCode::blocked_at(*stage),
            *stage,
            Some(*category),
            None,
        ),
        CloudError::SafetyUnavailable { stage, cause } => safety_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SafetyReasonCode::SafetyUnavailable,
            *stage,
            None,
            Some(*cause),
        ),
        CloudError::Admission(_) | CloudError::Ledger(_) | CloudError::Internal(_) => {
            // 内部细节只进日志（日志里没有请求内容，也没有密钥），不进响应
            eprintln!("[idoris] cloud: request failed closed: {error}");
            error_envelope(
                StatusCode::SERVICE_UNAVAILABLE,
                "cloud_unavailable",
                "the service cannot accept this request right now",
            )
        }
    }
}

/// 儿童安全的拒绝响应（S6）：稳定的码 + 阶段 + 类别或原因细节。**没有任何被拒的内容。**
/// 码同时放进 `X-iDoris-Reason` 响应头（浏览器读不到响应体里的 `error.reason_code`，但能读响应头，设计稿 §5）。
fn safety_response(
    status: StatusCode,
    code: SafetyReasonCode,
    stage: SafetyStage,
    category: Option<SafetyCategory>,
    cause: Option<SafetyUnavailableCause>,
) -> Response {
    let remediation = match (code, stage) {
        (SafetyReasonCode::SafetyInputBlocked, _) => {
            "the request did not pass the child-safety check; nothing was sent to a model"
        }
        (SafetyReasonCode::SafetyOutputBlocked, _) => {
            "the reply did not pass the child-safety check and was withheld; nothing was delivered"
        }
        (SafetyReasonCode::SafetyUnavailable, SafetyStage::Input) => {
            "the child-safety check could not be completed; nothing was sent to a model"
        }
        (SafetyReasonCode::SafetyUnavailable, SafetyStage::Output) => {
            "the child-safety check could not be completed; the reply was withheld and nothing was delivered"
        }
    };
    let mut error = serde_json::json!({
        "type": code.as_str(),
        "rule_id": null,
        "reason_code": code.as_str(),
        "evidence": null,
        "remediation": remediation,
        "stage": stage.as_str(),
    });
    if let Some(category) = category {
        error["category"] = serde_json::Value::String(category.as_str().to_string());
    }
    if let Some(cause) = cause {
        error["cause"] = serde_json::Value::String(cause.as_str().to_string());
    }
    let mut response = (status, axum::Json(serde_json::json!({ "error": error }))).into_response();
    response
        .headers_mut()
        .insert(HEADER_REASON, HeaderValue::from_static(code.as_str()));
    // 和 S3 的体验分级头同一套：`X-iDoris-Reason` 在 [`EXPOSED_HEADERS`] 里，浏览器的 JS 读得到；不另列一份。
    // 安全拒绝不在四级阶梯上（不是「钱不够」），所以没有 `X-iDoris-Tier`。
    expose_headers(&mut response);
    response
}

/// 云形态打开时挂在旧推理路由（`/v1/messages`、`/v1/embeddings`、`/v1/rerank`）前面的守卫（S6）：
/// 这些路由是旧的本地流水线，**没有儿童安全管线**。处理儿童数据的租户走到这里一律 403，
/// 否则孩子的 key 换个路径就绕过了输入检查和完整输出检查（也绕过了 edu 关缓存）。
///
/// 解析方式和 [`chat_completions`] 完全一样：看已验明的虚拟 key 上的租户，在调用方簿里找数据策略；
/// 不看任何请求头、请求体字段。簿里没有的租户（不是云调用方）照旧由旧流水线处理，不在这里改变任何行为。
pub(crate) async fn child_route_guard(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(cloud) = state.cloud.as_ref() {
        let tenant_id = request
            .extensions()
            .get::<AuthenticatedVirtualKey>()
            .and_then(|identity| identity.scope.tenant_id.as_deref());
        if cloud
            .callers
            .resolve(tenant_id)
            .is_some_and(|caller| caller.tenant.handles_child_data())
        {
            return error_envelope_with_reason(
                StatusCode::FORBIDDEN,
                "child_route_unavailable",
                "child_route_unavailable",
                "this endpoint is not available for tenants that process children's data; use POST /v1/chat/completions",
            );
        }
    }
    next.run(request).await
}
