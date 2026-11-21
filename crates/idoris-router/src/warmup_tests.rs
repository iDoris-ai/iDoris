//! 预热里需要 crate 内部旋钮才能测的部分：代理的响应头超时（`ChatProxy::header_timeout`）是 `pub(crate)`。
//! 端到端的状态码矩阵、余量确认、key 范围、并发合并、超时不留围栏在 `tests/warmup.rs`。
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn resident(id: &str, endpoint: &str, capabilities: &str, roles: &str) -> ComponentCard {
    serde_yaml::from_str(&format!(
        r#"provider:
  id: {id}
  family: local
  tier: local
  capabilities: [{capabilities}]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "openai_compat@unknown"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
extensions:
  _degradation: "Removing these bindings disables aliases."
  idoris_role_models: {{ {roles} }}
  idoris_upstream: {{ kind: openai_compat, flavor: openai_compat }}
"#
    ))
    .unwrap()
}

async fn slow_upstream(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({
                    "choices": [{"message": {"content": "x"}}],
                    "data": [{"object": "embedding", "index": 0, "embedding": [0.1]}]
                })),
        )
        .mount(&server)
        .await;
    server
}

/// 聊天与 embeddings 的响应头超时都缩到 150 毫秒（缺省是 10 秒与 30 秒），其余照旧。
fn app_with_short_header_timeouts(cards: Vec<ComponentCard>) -> Router {
    let mut proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
    proxy.header_timeout = Duration::from_millis(150);
    proxy.embeddings_header_timeout = Duration::from_millis(150);
    build_app(AppState {
        cards,
        proxy: Arc::new(proxy),
        ..AppState::default()
    })
}

async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_resident_warmup_is_bounded_by_its_own_timeout_ms_not_by_the_chat_header_timeout() {
    let upstream = slow_upstream(Duration::from_millis(700)).await;
    let app = app_with_short_header_timeouts(vec![
        resident("lmstudio", &upstream.uri(), "chat", "daily: \"up-model\""),
        resident(
            "omlx-embed",
            &upstream.uri(),
            "embedding",
            "embed: \"bge-m3-mlx-fp16\"",
        ),
    ]);
    // 对照：真正的聊天与 embeddings 请求被缩短之后的响应头超时截断（504），说明夹具确实把超时缩短了。
    let chat = json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        post(&app, "/v1/chat/completions", chat).await.0,
        StatusCode::GATEWAY_TIMEOUT
    );
    let embed = json!({"model": "idoris/embed", "input": "hi"});
    assert_eq!(
        post(&app, "/v1/embeddings", embed).await.0,
        StatusCode::GATEWAY_TIMEOUT
    );
    // 预热：响应头超时是请求的 timeout_ms，不是上面那个 150 毫秒。
    for model in ["idoris/daily", "idoris/embed"] {
        let (status, body) = post(
            &app,
            warmup::WARMUP_PATH,
            json!({"model": model, "timeout_ms": 5_000}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{model}: {body}");
        assert_eq!(body["state"], "ready");
        assert!(
            body["load_ms"].as_u64().unwrap() >= 600,
            "等到了慢上游的回答: {body}"
        );
    }
}

#[tokio::test]
async fn timeout_ms_still_bounds_the_resident_request_when_it_is_shorter_than_the_upstream() {
    let upstream = slow_upstream(Duration::from_millis(5_000)).await;
    let app = app_with_short_header_timeouts(vec![resident(
        "lmstudio",
        &upstream.uri(),
        "chat",
        "daily: \"up-model\"",
    )]);
    let started = Instant::now();
    let (status, body) = post(
        &app,
        warmup::WARMUP_PATH,
        json!({"model": "idoris/daily", "timeout_ms": 1_000}),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error"]["type"], "warmup_timeout");
    assert!(
        started.elapsed() < Duration::from_millis(4_000),
        "gave up at timeout_ms, did not wait for the upstream: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_explicit_header_deadline_replaces_the_path_default_and_none_keeps_it() {
    let upstream = slow_upstream(Duration::from_millis(1_000)).await;
    let mut proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
    proxy.header_timeout = Duration::from_millis(150);
    proxy.embeddings_header_timeout = Duration::from_millis(150);
    let opts = proxy::ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "r",
        provider_id: "lmstudio",
        served_locality: idoris_contracts::provider::Locality::Loopback,
        privacy: PrivacyClass::LocalOnly,
        require_openai_usage: false,
        attach_omlx_key: false,
    };
    let body = json!({"model": "m", "input": "x"});
    for path in [
        proxy::BufferedUpstreamPath::EMBEDDINGS,
        proxy::BufferedUpstreamPath::CHAT_COMPLETIONS,
        proxy::BufferedUpstreamPath::RERANK,
        proxy::BufferedUpstreamPath::MESSAGES,
    ] {
        let default = proxy
            .forward_buffered_path_finalized_within(
                &upstream.uri(),
                path,
                &body,
                &opts,
                None,
                |_| Ok(()),
                (
                    || async { Ok::<(), std::convert::Infallible>(()) },
                    |_| async { Ok::<(), std::convert::Infallible>(()) },
                ),
            )
            .await
            .unwrap_or_else(|never| match never {});
        assert_eq!(default.status, 504, "None 就是该路径的缺省超时");
        let explicit = proxy
            .forward_buffered_path_finalized_within(
                &upstream.uri(),
                path,
                &body,
                &opts,
                Some(Duration::from_secs(5)),
                |_| Ok(()),
                (
                    || async { Ok::<(), std::convert::Infallible>(()) },
                    |_| async { Ok::<(), std::convert::Infallible>(()) },
                ),
            )
            .await
            .unwrap_or_else(|never| match never {});
        assert_eq!(explicit.status, 200, "Some 取代该路径的缺省超时");
    }
}
