//! 云上游的 HTTP 处理：`POST /v1/chat/completions`（仅当云上游开关打开时由 `build_app` 挂载，替代本地流水线）。
//!
//! **认证**沿用既有的中间件（`remote_bind_auth_middleware`）：它已经验过虚拟 key，并把 `AuthenticatedVirtualKey`
//! 放进请求扩展；本处理函数只读它，不碰认证代码。拿到的 `tenant_id` 用来在静态调用方簿里找付款上下文与数据处理要求
//! （S1/S2/S5 落地后换成账户库里按凭证解析的真实付款上下文，见 [`super::config::CallerBook`]）。
//!
//! 响应（稳定的原因码，设计稿 §5）：
//!
//! | 情形 | 状态码 | `error.type` / 头 |
//! |---|---|---|
//! | 成功（非流式） | 200 | 上游响应体原样；有降级时带 `X-iDoris-Reason` 与 `X-iDoris-Degraded: true` |
//! | 成功（流式） | 200 `text/event-stream` | 同上；上游的流内 usage 块只在客户端要了 `include_usage` 时转发 |
//! | 没有合规候选 | 503 | `no_compliant_endpoint` |
//! | 准入拒绝 | 402 `budget_exhausted`；429 `rate_limited` / `in_flight_limited` / `subsidy_pool_exhausted`；其余 403 | 原因码 |
//! | 所有上游都失败 | 502（超时 504） | `upstream_error`，`reason_code` 是最后一次失败的稳定代码 |
//! | 流中途失败（首字节之后） | 状态码已经是 200 | 流以一个 `{"error":{"type":"upstream_error",...}}` 事件结束，**没有** `[DONE]` |
//!
//! edu（`child_edu`）租户在 P0 **一律非流式**（设计稿 §6）：流式请求在任何上游调用之前就被拒绝
//! （400 `streaming_not_allowed`）。完整输出检查本身是 S6 的事。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use idoris_compute_contracts::ids::RequestId;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::virtual_key::store::AuthenticatedVirtualKey;

use super::answer::{AnswerMeta, CloudOutcome};
use super::executor::{CloudCall, CloudError};
use super::request::{RequestProblem, parse_client_request};
use crate::connection::RequestLifecycle;
use crate::{
    AppState, HEADER_DEGRADED, HEADER_REASON, RequestRecordId, error_envelope,
    error_envelope_with_reason,
};

pub(crate) async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    Extension(lifecycle): Extension<RequestLifecycle>,
    identity: Option<Extension<AuthenticatedVirtualKey>>,
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
    let tenant_id = identity
        .as_ref()
        .and_then(|Extension(identity)| identity.scope.tenant_id.as_deref());
    let Some(caller) = cloud.callers.resolve(tenant_id) else {
        return error_envelope(
            StatusCode::FORBIDDEN,
            "caller_not_configured",
            "this credential has no cloud payment context",
        );
    };
    let Some(candidates) = cloud.routes.candidates(&request.model) else {
        return error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "unsupported_field",
            "unsupported_model",
            "the requested model is not served by this endpoint",
        );
    };
    // edu 一律非流式（设计稿 §6）：先于任何上游调用拒绝
    if request.stream && caller.tenant.handles_child_data() {
        return error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "streaming_not_allowed",
            "streaming_not_allowed",
            "streaming is not available for this tenant; send a non-streaming request",
        );
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

fn apply_reason_headers(response: &mut Response, meta: &AnswerMeta) {
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
        .any(|code| code.tier() == Some(idoris_compute_contracts::reason::ExperienceTier::Degraded))
    {
        response
            .headers_mut()
            .insert(HEADER_DEGRADED, HeaderValue::from_static("true"));
    }
}

fn problem_response(problem: RequestProblem) -> Response {
    let status = match problem {
        RequestProblem::MissingModel => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_REQUEST,
    };
    match problem {
        RequestProblem::MissingModel => error_envelope_with_reason(
            status,
            "unsupported_field",
            "unsupported_model",
            problem.message(),
        ),
        _ => error_envelope_with_reason(
            status,
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

pub(crate) fn error_response(error: &CloudError) -> Response {
    match error {
        CloudError::NoCompliantEndpoint(_) => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            ReasonCode::NoCompliantEndpoint.as_str(),
            "no upstream endpoint is permitted to process this request",
        ),
        CloudError::Denied(code) => error_envelope(
            denial_status(*code),
            code.as_str(),
            "the request was not admitted",
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
