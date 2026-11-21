//! `POST /v1/models/warmup`（IDORIS-CORE S8；接口规范 §2.11.2，修订 1.6 勘误八）：让一个模型进入「可用」状态，
//! 之后第一次真正的请求不用再等加载。数据面、数据 key（不是 Admin）。
//!
//! - **鉴权与 key 范围同聊天**：聊天在该模型上会被拒的 key，预热也被拒（`auth::enforce_scope`，与 embeddings 同一个入口
//!   `enforce_protocol_key_scope`）。
//! - **选卡同聊天**（同一套 `policy_cards` + `select_with_gate_fallback`，含角色绑定、隐私、熔断的就绪快照），只多两条：
//!   订阅中转卡不参与（没有「加载」可言），请求的能力由角色定（`embed` 是 embedding，其余是 chat）。
//! - **生命周期卡（oMLX 等）**：走 [`dispatch::ensure_loaded`]——和聊天的按需加载是同一段代码（读 catalog、拒未知模型、
//!   Supervisor 的幂等 load：余量确认与驱逐都在里面）；加载完成即返回，**不生成任何 token**。同一模型的并发预热与并发的
//!   首个聊天请求由 Supervisor 合并成一次加载。
//! - **resident 卡**：没有生命周期可管，按能力发一个最小的真实请求（chat：`max_tokens: 1`；embedding：输入 `"warmup"`），
//!   响应头超时是请求的 `timeout_ms`（不是聊天的 10 秒）。
//! - **`timeout_ms` 到点只是放弃等待**：返回 504 `warmup_timeout`，**不取消、不缩短**引擎里的加载——Supervisor 的加载任务与请求
//!   的 future 无关，照常跑完并清掉 `load.pending`，之后的预热或聊天直接看到「已就绪」。引擎自己的加载超时是另一回事
//!   （`RuntimeAdapter::load_timeout`，缺省 180 秒），只有它才会把结果变成「未知」并留下围栏。
//! - 不进用量与预算记录（本地免费；付费卡直接拒绝），写一条 `intent: warmup` 的审计事件（见 `record_id_middleware`）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use idoris_backend::BackendError;
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_policy::Role;
use serde_json::{Map, Value, json};

use crate::dispatch::{
    self, ChatOutcome, DispatchError, DispatchFailure, EnsureLoadedError, Selected,
};
use crate::profile::parse_profile;
use crate::proxy::{BufferedUpstreamPath, ForwardOpts};
use crate::{
    AppState, RequestRecordId, apply_selected_headers, backend_error_response, error_envelope,
    error_envelope_with_reason, headroom_breach_confirmed, managed_unavailable_response, models,
};

/// 路径（`record_id_middleware` 与鉴权中间件的路径名单也用它）。
pub(crate) const WARMUP_PATH: &str = "/v1/models/warmup";
/// 审计事件的 `intent`。
pub(crate) const AUDIT_INTENT: &str = "warmup";

/// `timeout_ms` 的范围与缺省（毫秒）。
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// resident 卡的最小请求里那一句话 / 那一个输入。
const MINIMAL_INPUT: &str = "warmup";

/// 请求体：只有 `model`（必填）与 `timeout_ms`（可选）；别的字段一律拒绝（G-6「不静默」）。
#[derive(Debug)]
struct WarmupRequest {
    model: String,
    timeout: Duration,
}

/// 请求体里一个不被接受的字段：`error.type` 恒为 `unsupported_field`，`reason_code` 说明是哪个字段、怎么不对
/// （`unsupported_parameter`：多出来的字段；`unsupported_model`：`model` 缺失、不是非空字符串，或是没有最小请求的角色；
/// `invalid_timeout_ms`：`timeout_ms` 不是范围内的整数）。
#[derive(Debug, PartialEq, Eq)]
struct BadField {
    reason_code: &'static str,
    remediation: String,
}

impl BadField {
    fn new(reason_code: &'static str, remediation: impl Into<String>) -> Self {
        Self {
            reason_code,
            remediation: remediation.into(),
        }
    }

    fn into_response(self) -> Response {
        error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "unsupported_field",
            self.reason_code,
            self.remediation,
        )
    }
}

/// 把调用方给的字段名放进错误信息之前：截断、控制字符换成 `U+FFFD`（同 `RoleParseError`，防日志注入）。
fn printable(text: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut shown: String = text
        .chars()
        .take(MAX_CHARS)
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    if text.chars().count() > MAX_CHARS {
        shown.push('…');
    }
    shown
}

fn parse_request(object: &Map<String, Value>) -> Result<WarmupRequest, BadField> {
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "model" | "timeout_ms"))
    {
        return Err(BadField::new(
            "unsupported_parameter",
            format!(
                "unknown field \"{}\": {WARMUP_PATH} accepts only model and timeout_ms",
                printable(unknown)
            ),
        ));
    }
    let model = match object.get("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => model.clone(),
        _ => {
            return Err(BadField::new(
                "unsupported_model",
                "model is required and must be a non-empty string: a concrete model id or an idoris/<role> alias",
            ));
        }
    };
    let timeout_ms = match object.get("timeout_ms") {
        None => DEFAULT_TIMEOUT_MS,
        // `as_u64` is `None` for a float, a negative number, a string and `null`: the value must be an integer.
        Some(value) => match value
            .as_u64()
            .filter(|ms| (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(ms))
        {
            Some(ms) => ms,
            None => {
                return Err(BadField::new(
                    "invalid_timeout_ms",
                    format!(
                        "timeout_ms must be an integer between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} (default {DEFAULT_TIMEOUT_MS})"
                    ),
                ));
            }
        },
    };
    Ok(WarmupRequest {
        model,
        timeout: Duration::from_millis(timeout_ms),
    })
}

/// 预热哪种能力：`embed` 角色是 embedding，其余（含没有角色的具体模型 id）是 chat，与聊天的缺省选卡一致。
/// `rerank` 没有定义最小请求（规范只定义了 chat 与 embedding），明确拒绝，不静默当成 chat。
fn warm_capability(role: Option<Role>) -> Result<Capability, BadField> {
    match role {
        Some(Role::Embed) => Ok(Capability::Embedding),
        Some(Role::Rerank) => Err(BadField::new(
            "unsupported_model",
            "warmup supports chat and embedding models only; idoris/rerank has no minimal request",
        )),
        _ => Ok(Capability::Chat),
    }
}

/// 选定候选之后的失败都带上候选的 served-locality / reason 头（同聊天）。
fn with_headers(mut response: Response, selected: &Selected) -> Response {
    apply_selected_headers(&mut response, selected.served_locality, &selected.decision);
    response
}

/// 一个后端错误，沿用聊天的信封与代码（`backend_error_response`：`headroom_confirmation_required`、`local_only_unavailable`
/// 加后端自己的 `reason_code`，例如 `supervisor_busy`），只有「模型不存在」按规范改成 404 `model_not_found`。
fn backend_failure(selected: &Selected, err: BackendError) -> Response {
    if let BackendError::ModelNotFound { model_id } = &err {
        return with_headers(
            error_envelope(
                StatusCode::NOT_FOUND,
                "model_not_found",
                format!(
                    "model \"{}\" is not known to the engine behind this card",
                    printable(model_id)
                ),
            ),
            selected,
        );
    }
    let outcome = ChatOutcome {
        decision: selected.decision.clone(),
        served_locality: selected.served_locality,
        result: Err(DispatchFailure::Backend(err.clone())),
        actual_cost_minor: None,
    };
    backend_error_response(&err, &outcome)
}

/// `timeout_ms` 到点：504，类型与原因码都是 `warmup_timeout`。加载可能仍在引擎里继续，不等于失败。
fn timeout_response(selected: &Selected, timeout: Duration) -> Response {
    with_headers(
        error_envelope(
            StatusCode::GATEWAY_TIMEOUT,
            "warmup_timeout",
            format!(
                "the model was not ready within {} ms; the engine may still be loading it, so this is not a failure: retry the warmup later",
                timeout.as_millis()
            ),
        ),
        selected,
    )
}

/// 生命周期卡：取 Supervisor，让模型就绪。返回 `already_loaded`：调用时 Supervisor 的账本里它已经是就绪的。
async fn warm_lifecycle(
    state: &AppState,
    selected: &Selected,
    target: &str,
    headroom_confirmed: bool,
) -> Result<bool, Response> {
    let provider_id = selected.card.provider.id.as_str();
    // 没有这张卡的 Supervisor（或绑定的不是这张卡）：和聊天一样，失败即关闭。
    let Some(bound) = state
        .runtimes
        .get(provider_id)
        .filter(|bound| bound.matches(&selected.card))
    else {
        return Err(backend_failure(
            selected,
            BackendError::supervisor_unavailable(),
        ));
    };
    // 只是给调用方的信息：`false` 时照常走加载，Supervisor 不可用之类的真错误由 `ensure_loaded` 自己报出来。
    let already_loaded = bound.is_ready(target).await.unwrap_or(false);
    dispatch::ensure_loaded(
        bound,
        provider_id,
        target,
        selected.load_policy,
        headroom_confirmed,
    )
    .await
    .map(|()| already_loaded)
    .map_err(|failure: EnsureLoadedError| backend_failure(selected, failure.into_backend()))
}

/// 一次预热里与具体卡无关的那些：调用方是谁、预热哪种能力、预热哪个具体模型。
struct Job<'a> {
    privacy: PrivacyClass,
    tenant_id: Option<&'a str>,
    record_id: &'a str,
    capability: Capability,
    target: &'a str,
    timeout: Duration,
}

/// resident 卡：发一个最小的真实请求，上游答 2xx 就算可用。响应头超时是 `timeout`（调用方的 `timeout_ms`）。
///
/// 上游的回答按预热的错误矩阵翻译：404 是 `model_not_found`，504（响应头或正文超时）是 `warmup_timeout`，其余非 2xx 与聊天的
/// resident 路径一样原样透传（含代理自己的 502 `upstream_auth_failed` 与熔断的 503）。不带幂等键：预热每次都是真请求，不重放缓存。
async fn warm_resident(
    state: &AppState,
    selected: &Selected,
    job: &Job<'_>,
) -> Result<(), Response> {
    let Job {
        privacy,
        tenant_id,
        record_id,
        capability,
        target,
        timeout,
    } = *job;
    let (path, body) = match capability {
        Capability::Embedding => (
            BufferedUpstreamPath::EMBEDDINGS,
            json!({"model": target, "input": MINIMAL_INPUT}),
        ),
        _ => (
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            json!({
                "model": target,
                "messages": [{"role": "user", "content": MINIMAL_INPUT}],
                "max_tokens": 1,
            }),
        ),
    };
    let opts = ForwardOpts {
        request_id: None,
        tenant_id,
        record_id,
        provider_id: selected.card.provider.id.as_str(),
        served_locality: selected.served_locality,
        privacy,
        require_openai_usage: false,
        attach_omlx_key: models::is_loopback_omlx(&selected.card),
    };
    let outcome = match state
        .proxy
        .forward_buffered_path_finalized_within(
            &selected.card.endpoint,
            path,
            &body,
            &opts,
            Some(timeout),
            |_| Ok(()),
            (
                || async { Ok::<(), std::convert::Infallible>(()) },
                |_| async { Ok::<(), std::convert::Infallible>(()) },
            ),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(never) => match never {},
    };
    match outcome.status {
        200..=299 => Ok(()),
        404 => Err(backend_failure(
            selected,
            BackendError::model_not_found(target),
        )),
        504 => Err(timeout_response(selected, timeout)),
        status => {
            let mut response = (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                outcome.body,
            )
                .into_response();
            let content_type = outcome
                .content_type
                .as_deref()
                .unwrap_or("application/json");
            if let Ok(value) = HeaderValue::from_str(content_type) {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, value);
            }
            Err(with_headers(response, selected))
        }
    }
}

/// `POST /v1/models/warmup`。
pub(crate) async fn warmup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    remote_identity: Option<Extension<idoris_tenancy::virtual_key::store::AuthenticatedVirtualKey>>,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    body: Bytes,
) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return error_envelope(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "request body is not valid JSON",
            );
        }
    };
    let Some(object) = value.as_object() else {
        return error_envelope(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "request body must be a JSON object",
        );
    };
    let request = match parse_request(object) {
        Ok(request) => request,
        Err(bad) => return bad.into_response(),
    };
    let mut parsed = match parse_profile(&headers, Some(&request.model), state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    if let Err(response) = crate::enforce_protocol_key_scope(
        &state,
        &headers,
        remote_identity.as_ref().map(|identity| &identity.0),
        &parsed,
    )
    .await
    {
        return response;
    }
    let capability = match warm_capability(parsed.role) {
        Ok(capability) => capability,
        Err(bad) => return bad.into_response(),
    };
    // The endpoint, not the caller's `X-iDoris-Capabilities`, decides what is being warmed.
    parsed.task.capabilities = Some(vec![capability]);
    // 隐私：和其他协议端点一样过一遍（远程熔断开关、内容收紧）。预热的请求体里没有内容，但「开关一合上，一切都只走本地」
    // 不能因为这条路径没有内容就绕过。
    let inspection = crate::protocol_inspection(
        &state,
        &value,
        body.len(),
        parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
    )
    .await;
    parsed.task.privacy = Some(inspection.effective);

    let policy = dispatch::policy_cards(&state.cards, &state.routing_policy, &parsed);
    let cards: Vec<_> = policy
        .cards
        .into_iter()
        .filter(|card| !idoris_policy::is_subscription_provider_id(&card.provider.id))
        .collect();
    let blocked = state.proxy.blocked_cards(&cards);
    let selected = match dispatch::select_with_gate_fallback(
        &cards,
        &parsed,
        "",
        None,
        &Default::default(),
        &blocked,
        inspection.tightened && !inspection.killed,
    ) {
        Ok(selected) => selected,
        Err(DispatchError::Rejection(rejection)) => return crate::rejection_response(rejection),
        Err(DispatchError::Internal(message)) => {
            return error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message);
        }
    };
    // 门控降级放行、没就绪的托管卡：不碰引擎，和聊天一样 503 带原因。
    if let Some(response) = managed_unavailable_response(&state, &selected) {
        return response;
    }
    // 本地免费才预热：付费卡的最小请求也会花钱，而预热不进用量与预算记录。
    let cost = &selected.card.provider.cost;
    if cost.input_per_m != 0.0 || cost.output_per_m != 0.0 {
        return with_headers(
            error_envelope(
                StatusCode::SERVICE_UNAVAILABLE,
                "paid_proxy_unavailable",
                "warmup is only available for free local models; the selected card has a price",
            ),
            &selected,
        );
    }
    let target = match dispatch::backend_model_id(&parsed, Some(&request.model), &selected.card) {
        Ok(target) => target,
        Err(error) => return crate::model_resolution_error(error),
    };

    let started = Instant::now();
    let job = Job {
        privacy: parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
        tenant_id: parsed.tenant_id.as_deref(),
        record_id: &record_id,
        capability,
        target: &target,
        timeout: request.timeout,
    };
    let work = async {
        if dispatch::is_resident_http_service(&selected.card) {
            // 没有账本可问：resident 卡的 `already_loaded` 恒为 false，`load_ms` 是那一个最小请求的耗时。
            warm_resident(&state, &selected, &job).await.map(|()| false)
        } else {
            warm_lifecycle(
                &state,
                &selected,
                &target,
                headroom_breach_confirmed(&headers),
            )
            .await
        }
    };
    let already_loaded = match tokio::time::timeout(request.timeout, work).await {
        Ok(Ok(already_loaded)) => already_loaded,
        Ok(Err(response)) => return response,
        // 只是放弃等待：`work` 被丢弃，引擎里（Supervisor 的后台任务里）的加载照常跑完。
        Err(_elapsed) => return timeout_response(&selected, request.timeout),
    };
    with_headers(
        Json(json!({
            "model": target,
            "provider_id": selected.card.provider.id,
            "state": "ready",
            "already_loaded": already_loaded,
            "load_ms": crate::elapsed_ms(started),
        }))
        .into_response(),
        &selected,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use http_body_util::BodyExt;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn model_alone_gets_the_default_timeout_of_two_minutes() {
        let request = parse_request(&object(json!({"model": "idoris/daily"}))).unwrap();
        assert_eq!(request.model, "idoris/daily");
        assert_eq!(request.timeout, Duration::from_millis(120_000));
    }

    #[test]
    fn timeout_ms_is_taken_as_given_across_the_whole_inclusive_range() {
        for ms in [1_000u64, 1_001, 120_000, 299_999, 300_000] {
            let request = parse_request(&object(json!({"model": "m", "timeout_ms": ms}))).unwrap();
            assert_eq!(request.timeout, Duration::from_millis(ms));
        }
    }

    #[tokio::test]
    async fn a_bad_field_renders_as_a_400_unsupported_field_envelope() {
        let bad = BadField::new("invalid_timeout_ms", "nope");
        let response = bad.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "unsupported_field");
        assert_eq!(body["error"]["reason_code"], "invalid_timeout_ms");
        assert_eq!(body["error"]["remediation"], "nope");
    }

    #[test]
    fn every_bad_request_is_rejected_with_its_own_reason_code() {
        let cases = [
            (json!({"model": "m", "extra": 1}), "unsupported_parameter"),
            (json!({"extra": 1}), "unsupported_parameter"),
            // The unknown field is reported before a missing model or a bad timeout.
            (
                json!({"model": 5, "timeout_ms": "x", "extra": 1}),
                "unsupported_parameter",
            ),
            (json!({}), "unsupported_model"),
            (json!({"model": ""}), "unsupported_model"),
            (json!({"model": " \t"}), "unsupported_model"),
            (json!({"model": null}), "unsupported_model"),
            (json!({"model": 7}), "unsupported_model"),
            (
                json!({"model": "m", "timeout_ms": 999}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": 300_001}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": -1000}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": 1500.5}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": 2000.0}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": "2000"}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": null}),
                "invalid_timeout_ms",
            ),
        ];
        for (body, expected) in cases {
            let rejected = parse_request(&object(body.clone())).unwrap_err();
            assert_eq!(rejected.reason_code, expected, "{body}");
        }
    }

    #[test]
    fn printable_truncates_at_64_characters_and_neutralizes_control_characters() {
        assert_eq!(printable("model"), "model");
        let exact = "x".repeat(64);
        assert_eq!(printable(&exact), exact, "64 个字符原样保留");
        let long = "x".repeat(65);
        assert_eq!(printable(&long), format!("{}…", "x".repeat(64)));
        assert_eq!(printable("a\nb\u{1b}c"), "a\u{FFFD}b\u{FFFD}c");
        // Characters, not bytes: 64 multi-byte characters are still not truncated.
        let wide = "模".repeat(64);
        assert_eq!(printable(&wide), wide);
    }

    #[test]
    fn embed_is_warmed_as_embedding_rerank_is_refused_and_everything_else_is_chat() {
        assert_eq!(
            warm_capability(Some(Role::Embed)).ok(),
            Some(Capability::Embedding)
        );
        assert_eq!(
            warm_capability(Some(Role::Rerank)).unwrap_err().reason_code,
            "unsupported_model"
        );
        for role in [
            None,
            Some(Role::Fast),
            Some(Role::Daily),
            Some(Role::Deep),
            Some(Role::Vision),
            Some(Role::Decide),
            Some(Role::Auto),
        ] {
            assert_eq!(
                warm_capability(role).ok(),
                Some(Capability::Chat),
                "{role:?}"
            );
        }
    }
}
