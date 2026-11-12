//! 向云端发一次 `POST /api/v1/chat/completions`：非流式，一次，不重试。
//!
//! # 安全立场（和云上游适配器 `idoris-upstream::cloud::client` 同一套）
//!
//! - 请求是**从零构造**的：只发 `Authorization`、`Content-Type`、`Accept`、`Accept-Encoding`、`User-Agent` 五个头和一个 JSON 请求体。
//!   不转发调用方的任何头（尤其是调用方的本地虚拟 key、`X-iDoris-Tenant`、`X-iDoris-Request-Id`、会话头），也**不带** `X-iDoris-Member`：
//!   Node 凭证不能带成员头，设备凭证带了也只能是自己（S5 的交接说明），所以一律不带；
//! - **不跟随重定向**（重定向会把 `Authorization` 带去别的主机）、**不走代理**（`idoris_upstream::cloud::build_http_client`）；
//! - 响应体有大小上限；总期限由配置给；调用方断开（取消令牌）立刻丢弃整个请求，云端看到的是客户端断开；
//! - 密钥只在 `Authorization` 头里（标记为 sensitive），不进日志、不进任何错误。
//!
//! # 不重试，不重放
//!
//! 一次请求最多向云端发一次。失败不重试：请求一旦可能已经到达云端，再发一次就是第二笔账（云端的「用户只付一次」
//! 是对**一个请求**说的，两个请求是两次）。设计稿 §9：恢复连接后不补发、不重放离线期间的请求——这里没有队列，
//! 也没有任何东西会在以后替这个请求重新发。
//!
//! # 云端的答复怎么归类
//!
//! | 云端 | 本机归类（[`CallOutcome`]） |
//! |---|---|
//! | 2xx + JSON 对象 + 有 `X-iDoris-Record-Id` | `served` |
//! | 2xx 但不是可用的回答 | `invalid_response` |
//! | 401 | `unauthorized`（凭证被撤销、过期、不认识） |
//! | 402 | `budget_exhausted`（云账户额度用完） |
//! | 403 | `forbidden`（租户没开通、账户停用、成员关系撤销……） |
//! | 429 | `rate_limited` |
//! | 400 / 404 / 413 / 422 和其它 4xx | `rejected`（请求形状或模型名被拒） |
//! | 408 | `timeout` |
//! | 5xx、3xx、其它 | `cloud_error` |
//! | 连接不上（DNS、连接被拒、TLS） | `unreachable` |
//! | 总期限内没读完 | `timeout` |

use axum::body::Bytes;
use futures_util::StreamExt;
use idoris_tenancy::cloud_tier::CallOutcome;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::config::CloudTierConfig;
use super::credential::CloudCredential;
use super::gate::EgressPermit;

/// 出网的 `User-Agent`：固定，不含主机名、用户名、版本以外的任何信息。
pub const USER_AGENT_VALUE: &str = concat!("idoris-cloud-tier/", env!("CARGO_PKG_VERSION"));

/// 非 2xx 响应体读取上限：只为取原因码。
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// 云端答了、本机能交给调用方的回答。
#[derive(Debug)]
pub struct CloudAnswer {
    /// 云端的响应体，**原样**（不重新序列化）。
    pub body: Bytes,
    pub status: u16,
    /// 云端的 `X-iDoris-Record-Id`（它那边的请求 id）：对账指针。
    pub cloud_record_id: String,
    /// 云端的 `X-iDoris-Reason`（体验分级 / 失败切换的原因码，逗号连接）。
    pub reason: Option<String>,
    /// 云端的 `X-iDoris-Tier`（normal / degraded / grace）。
    pub service_tier: Option<String>,
    /// 云端压低 `max_tokens` 时发给上游的值（`X-iDoris-Max-Tokens`）。
    pub max_tokens: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

/// 没能拿到可用回答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudFailure {
    /// 连接不上：请求没有到达云端。
    Unreachable,
    /// 总期限内没有收到完整的回答。
    Timeout,
    /// 调用方在云端答复之前断开了。
    Cancelled,
    /// 云端答了，但不是可用的回答（带它的状态码、原因码和 record id，对账用）。
    Answered {
        outcome: CallOutcome,
        status: u16,
        reason: Option<String>,
        cloud_record_id: Option<String>,
    },
    /// 传输层在请求发出之后出了别的错（连接中途被重置……）：请求可能已经到达。
    Transport,
}

impl CloudFailure {
    /// 归到调用记录的结局。
    pub fn outcome(&self) -> CallOutcome {
        match self {
            CloudFailure::Unreachable => CallOutcome::Unreachable,
            CloudFailure::Timeout => CallOutcome::Timeout,
            CloudFailure::Cancelled => CallOutcome::Cancelled,
            CloudFailure::Answered { outcome, .. } => *outcome,
            CloudFailure::Transport => CallOutcome::CloudError,
        }
    }
}

/// 状态码 -> 结局（非 2xx）。纯函数，表驱动测试钉死。
pub fn outcome_for_status(status: u16) -> CallOutcome {
    match status {
        401 => CallOutcome::Unauthorized,
        402 => CallOutcome::BudgetExhausted,
        403 => CallOutcome::Forbidden,
        408 => CallOutcome::Timeout,
        429 => CallOutcome::RateLimited,
        400..=499 => CallOutcome::Rejected,
        _ => CallOutcome::CloudError,
    }
}

/// 云端响应头里的短字符串：可见 ASCII、不长。不合格就当没有（不转发、不记录）。
fn short_header(headers: &reqwest::header::HeaderMap, name: &str, max: usize) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    (!value.is_empty()
        && value.len() <= max
        && value.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
    .then(|| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

/// 云端的 record id：和调用记录表里的标识符形状一致。
fn record_id_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = short_header(headers, "x-idoris-record-id", 128)?;
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
        .then_some(value)
}

/// 原因码形状（和调用记录表一致）：小写字母、数字、`_`、`,`。
fn reason_shape(text: &str) -> Option<String> {
    (!text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b',')))
    .then(|| text.to_string())
}

/// 从云端的错误体里取稳定原因码（`error.reason_code`）；取不到就看 `X-iDoris-Reason` 头。
fn error_reason(body: &[u8], header: Option<&str>) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/reason_code")
                .and_then(Value::as_str)
                .and_then(reason_shape)
        })
        .or_else(|| header.and_then(reason_shape))
}

fn usage_of(body: &Value) -> (Option<u64>, Option<u64>) {
    let usage = body.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    (count("prompt_tokens"), count("completion_tokens"))
}

fn classify_transport(error: &reqwest::Error) -> CloudFailure {
    if error.is_connect() {
        CloudFailure::Unreachable
    } else if error.is_timeout() {
        CloudFailure::Timeout
    } else {
        CloudFailure::Transport
    }
}

/// 发请求。`_permit` 是出网凭条（只有 `gate::grant` 能造）：没有它就发不了。
pub async fn send(
    client: &reqwest::Client,
    config: &CloudTierConfig,
    _permit: EgressPermit,
    credential: &CloudCredential,
    body: &Value,
    cancel: &CancellationToken,
) -> Result<CloudAnswer, CloudFailure> {
    let bytes = serde_json::to_vec(body).map_err(|_| CloudFailure::Transport)?;
    let request = client
        .post(config.chat_url.clone())
        .header(AUTHORIZATION, credential.authorization())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .header(ACCEPT_ENCODING, "identity")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .body(bytes);
    let run = async {
        let response = request
            .send()
            .await
            .map_err(|error| classify_transport(&error))?;
        read_answer(response, config.max_response_bytes).await
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CloudFailure::Cancelled),
        outcome = tokio::time::timeout(config.total_timeout, run) => match outcome {
            Ok(result) => result,
            Err(_) => Err(CloudFailure::Timeout),
        },
    }
}

async fn read_answer(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> Result<CloudAnswer, CloudFailure> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let success = (200..300).contains(&status);
    let cap = if success {
        max_response_bytes
    } else {
        MAX_ERROR_BODY_BYTES.min(max_response_bytes)
    };
    let cloud_record_id = record_id_header(&headers);
    let reason_header = short_header(&headers, "x-idoris-reason", 128);
    let answered = |outcome: CallOutcome, reason: Option<String>| CloudFailure::Answered {
        outcome,
        status,
        reason,
        cloud_record_id: cloud_record_id.clone(),
    };

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) if error.is_timeout() => return Err(CloudFailure::Timeout),
            // 云端已经答了，但回答没有完整地到本机：用不了
            Err(_) => return Err(answered(CallOutcome::InvalidResponse, None)),
        };
        if body.len().saturating_add(chunk.len()) > cap {
            return Err(answered(
                if success {
                    CallOutcome::InvalidResponse
                } else {
                    outcome_for_status(status)
                },
                None,
            ));
        }
        body.extend_from_slice(&chunk);
    }

    if !success {
        let reason = error_reason(&body, reason_header.as_deref());
        return Err(answered(outcome_for_status(status), reason));
    }
    // 2xx：必须是 JSON、必须是一个对象、必须有 choices、必须有云端 record id（对账指针）
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"));
    let parsed: Option<Value> = is_json
        .then(|| serde_json::from_slice(&body).ok())
        .flatten();
    let usable = parsed
        .as_ref()
        .is_some_and(|value| value.get("choices").is_some_and(Value::is_array));
    let (Some(parsed), true, Some(cloud_record_id)) = (parsed, usable, cloud_record_id.clone())
    else {
        return Err(answered(CallOutcome::InvalidResponse, None));
    };
    let (tokens_in, tokens_out) = usage_of(&parsed);
    Ok(CloudAnswer {
        body: Bytes::from(body),
        status,
        cloud_record_id,
        reason: reason_header.as_deref().and_then(reason_shape),
        service_tier: short_header(&headers, "x-idoris-tier", 32),
        max_tokens: short_header(&headers, "x-idoris-max-tokens", 16)
            .filter(|value| value.bytes().all(|b| b.is_ascii_digit())),
        tokens_in,
        tokens_out,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::cloud_tier::gate::{Consent, GateInput, LocalFailure, PrivacyFacts, grant};
    use idoris_backend::BackendError;
    use idoris_contracts::common::PrivacyClass;

    fn secret() -> String {
        format!("idk_{}", "a".repeat(64))
    }

    fn config(server: &MockServer, extra: &str) -> CloudTierConfig {
        CloudTierConfig::parse(
            &format!(
                "schema_version: 1\nbase_url: {}/api\nmodel: idoris/agent\n{extra}",
                server.uri()
            ),
            Path::new("."),
        )
        .unwrap()
    }

    /// 造一张凭条：和生产一样走 `grant`（第 2 层用一个现成的 `Selected`）。
    fn permit() -> EgressPermit {
        let error = BackendError::SupervisorUnavailable;
        let input = GateInput {
            privacy: PrivacyFacts {
                effective: Some(PrivacyClass::Any),
                pinned_local_by_content: false,
                kill_switch: false,
            },
            dev_no_key: false,
            consent: Consent::Given,
            stream: false,
            failure: LocalFailure::Backend(&error),
            cloud_model: Some("m"),
        };
        let card = serde_yaml::from_str(include_str!(
            "../../../../config/components/subscription.yaml"
        ))
        .unwrap();
        grant(&input, || {
            Ok(crate::dispatch::Selected {
                decision: idoris_policy::Decision {
                    chosen_id: "x".into(),
                    reason_codes: Vec::new(),
                    degradations: Vec::new(),
                },
                served_locality: idoris_contracts::provider::Locality::Remote,
                card,
                load_policy: idoris_contracts::load_policy::LoadPolicy {
                    mode: idoris_contracts::load_policy::LoadMode::Resident,
                    keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
                    admission: idoris_contracts::load_policy::Admission::Coexist,
                },
                estimated_cost_minor: 0,
            })
        })
        .unwrap()
        .0
    }

    fn client(config: &CloudTierConfig) -> reqwest::Client {
        idoris_upstream::cloud::build_http_client(config.connect_timeout).unwrap()
    }

    async fn call(server: &MockServer, extra: &str) -> Result<CloudAnswer, CloudFailure> {
        let config = config(server, extra);
        send(
            &client(&config),
            &config,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "hi"}]}),
            &CancellationToken::new(),
        )
        .await
    }

    fn ok_body() -> serde_json::Value {
        json!({"id": "c", "choices": [{"message": {"role": "assistant", "content": "hello"}}],
               "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42}})
    }

    #[tokio::test]
    async fn a_good_answer_is_passed_through_byte_for_byte_with_its_reconciliation_facts() {
        let server = MockServer::start().await;
        let raw = br#"{"id":"c","choices":[{"message":{"content":"x"}}],"usage":{"prompt_tokens":12,"completion_tokens":30}}"#;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("x-idoris-record-id", "0b9f5c1e-3d71-4f2a-8f55-bbbbbbbbbbbb")
                    .insert_header("x-idoris-tier", "degraded")
                    .insert_header("x-idoris-reason", "budget_degraded")
                    .insert_header("x-idoris-max-tokens", "512")
                    .set_body_raw(raw.as_slice(), "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let answer = call(&server, "").await.unwrap();
        assert_eq!(
            answer.body.as_ref(),
            raw.as_slice(),
            "forwarded verbatim, not re-serialized"
        );
        assert_eq!(answer.status, 200);
        assert_eq!(
            answer.cloud_record_id,
            "0b9f5c1e-3d71-4f2a-8f55-bbbbbbbbbbbb"
        );
        assert_eq!((answer.tokens_in, answer.tokens_out), (Some(12), Some(30)));
        assert_eq!(answer.service_tier.as_deref(), Some("degraded"));
        assert_eq!(answer.reason.as_deref(), Some("budget_degraded"));
        assert_eq!(answer.max_tokens.as_deref(), Some("512"));
    }

    #[tokio::test]
    async fn only_five_headers_and_the_json_body_go_out_and_never_the_member_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "rec-1")
                    .set_body_raw(ok_body().to_string(), "application/json"),
            )
            .mount(&server)
            .await;
        call(&server, "").await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let mut names: Vec<String> = requests[0]
            .headers
            .iter()
            .map(|(name, _)| name.as_str().to_ascii_lowercase())
            .collect();
        names.sort();
        names.dedup();
        // reqwest 自己会加 host / content-length
        for allowed in [
            "accept",
            "accept-encoding",
            "authorization",
            "content-length",
            "content-type",
            "host",
            "user-agent",
        ] {
            names.retain(|name| name != allowed);
        }
        assert!(names.is_empty(), "unexpected headers leaked: {names:?}");
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", secret())
        );
        assert_eq!(
            requests[0]
                .headers
                .get("accept-encoding")
                .unwrap()
                .to_str()
                .unwrap(),
            "identity"
        );
        assert!(
            requests[0]
                .headers
                .get("user-agent")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("idoris-cloud-tier/")
        );
        assert!(requests[0].headers.get("x-idoris-member").is_none());
        assert_eq!(requests[0].method.as_str(), "POST");
        assert_eq!(requests[0].url.path(), "/api/v1/chat/completions");
    }

    #[test]
    fn the_status_table_is_pinned() {
        for (status, outcome) in [
            (401, CallOutcome::Unauthorized),
            (402, CallOutcome::BudgetExhausted),
            (403, CallOutcome::Forbidden),
            (408, CallOutcome::Timeout),
            (429, CallOutcome::RateLimited),
            (400, CallOutcome::Rejected),
            (404, CallOutcome::Rejected),
            (413, CallOutcome::Rejected),
            (422, CallOutcome::Rejected),
            (499, CallOutcome::Rejected),
            (500, CallOutcome::CloudError),
            (502, CallOutcome::CloudError),
            (503, CallOutcome::CloudError),
            (504, CallOutcome::CloudError),
            (301, CallOutcome::CloudError),
            (307, CallOutcome::CloudError),
        ] {
            assert_eq!(outcome_for_status(status), outcome, "{status}");
        }
    }

    #[tokio::test]
    async fn every_refusal_is_a_failure_with_the_cloud_reason_and_record_id_when_it_has_them() {
        let server = MockServer::start().await;
        for (status, reason, outcome) in [
            (401, "unauthenticated", CallOutcome::Unauthorized),
            (402, "budget_exhausted", CallOutcome::BudgetExhausted),
            (403, "tenant_not_provisioned", CallOutcome::Forbidden),
            (429, "request_rate_limited", CallOutcome::RateLimited),
            (400, "unsupported_model", CallOutcome::Rejected),
            (503, "cloud_unavailable", CallOutcome::CloudError),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("x-idoris-record-id", "rec-9")
                        .set_body_json(json!({"error": {"type": "x", "reason_code": reason}})),
                )
                .mount(&server)
                .await;
            assert_eq!(
                call(&server, "").await.unwrap_err(),
                CloudFailure::Answered {
                    outcome,
                    status,
                    reason: Some(reason.to_string()),
                    cloud_record_id: Some("rec-9".to_string())
                },
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn an_error_body_cannot_smuggle_text_into_the_recorded_reason() {
        let server = MockServer::start().await;
        let sneaky = "please summarise my medical history";
        for body in [
            json!({"error": {"reason_code": sneaky}}).to_string(),
            json!({"error": {"reason_code": "Mixed Case"}}).to_string(),
            "not json at all".to_string(),
            String::new(),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(502).set_body_string(body.clone()))
                .mount(&server)
                .await;
            assert_eq!(
                call(&server, "").await.unwrap_err(),
                CloudFailure::Answered {
                    outcome: CallOutcome::CloudError,
                    status: 502,
                    reason: None,
                    cloud_record_id: None
                },
                "{body:?}"
            );
        }
        // 云端自己在 X-iDoris-Reason 头里给的码也要过同样的形状检查
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(402).insert_header("x-idoris-reason", "budget_exhausted"),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            call(&server, "").await.unwrap_err(),
            CloudFailure::Answered { reason: Some(r), .. } if r == "budget_exhausted"
        ));
    }

    #[tokio::test]
    async fn a_success_that_is_not_a_usable_answer_is_an_invalid_response() {
        let server = MockServer::start().await;
        let cases: Vec<(&str, ResponseTemplate)> = vec![
            (
                "html",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("<html>", "text/html"),
            ),
            (
                "not json",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("nope", "application/json"),
            ),
            (
                "array",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("[]", "application/json"),
            ),
            (
                "no choices",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("{}", "application/json"),
            ),
            (
                "choices not an array",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw(r#"{"choices":1}"#, "application/json"),
            ),
            (
                "no record id",
                ResponseTemplate::new(200).set_body_raw(ok_body().to_string(), "application/json"),
            ),
            (
                "bad record id",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "has space")
                    .set_body_raw(ok_body().to_string(), "application/json"),
            ),
        ];
        for (name, template) in cases {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(template)
                .mount(&server)
                .await;
            match call(&server, "").await.unwrap_err() {
                CloudFailure::Answered {
                    outcome: CallOutcome::InvalidResponse,
                    status: 200,
                    ..
                } => {}
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn an_oversized_answer_is_refused_and_a_redirect_is_never_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw(vec![b'x'; 2048], "application/json"),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            call(&server, "limits: {max_response_bytes: 1024}\n")
                .await
                .unwrap_err(),
            CloudFailure::Answered {
                outcome: CallOutcome::InvalidResponse,
                ..
            }
        ));

        let outside = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&outside)
            .await;
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/capture", outside.uri())),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            call(&server, "").await.unwrap_err(),
            CloudFailure::Answered {
                outcome: CallOutcome::CloudError,
                status: 307,
                ..
            }
        ));
        assert!(outside.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_listening_is_unreachable_and_a_stalled_answer_times_out() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dead = CloudTierConfig::parse(
            &format!("schema_version: 1\nbase_url: http://127.0.0.1:{port}/api\nmodel: m\n"),
            Path::new("."),
        )
        .unwrap();
        let outcome = send(
            &client(&dead),
            &dead,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({}),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Unreachable);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let slow = config(
            &server,
            "limits: {connect_timeout_ms: 100, total_timeout_ms: 200}\n",
        );
        let started = std::time::Instant::now();
        let outcome = send(
            &client(&slow),
            &slow,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({}),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Timeout);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn the_caller_going_away_drops_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let config = config(&server, "");
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let outcome = send(
            &client(&config),
            &config,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({}),
            &cancel,
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(CloudFailure::Cancelled.outcome(), CallOutcome::Cancelled);
    }

    #[test]
    fn failures_map_to_recorded_outcomes() {
        assert_eq!(
            CloudFailure::Unreachable.outcome(),
            CallOutcome::Unreachable
        );
        assert_eq!(CloudFailure::Timeout.outcome(), CallOutcome::Timeout);
        assert_eq!(CloudFailure::Transport.outcome(), CallOutcome::CloudError);
        assert_eq!(
            CloudFailure::Answered {
                outcome: CallOutcome::RateLimited,
                status: 429,
                reason: None,
                cloud_record_id: None
            }
            .outcome(),
            CallOutcome::RateLimited
        );
    }
}
