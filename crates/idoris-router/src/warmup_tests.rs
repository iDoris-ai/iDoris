//! 预热里需要 crate 内部旋钮才能测的部分：代理的响应头超时（`ChatProxy::header_timeout`）与 resident 预热请求自己的期限
//! （`ChatProxy::warmup_header_timeout`）是 `pub(crate)`，并发许可的余量只有测试访问器能看到。
//! 端到端的状态码矩阵、余量确认、key 范围、并发合并、`timeout_ms` 与客户端断开不留围栏、审计在 `tests/warmup.rs`。
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
async fn a_resident_warmup_waits_for_a_slow_upstream_instead_of_the_chat_header_timeout() {
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
    // 预热：最小请求的响应头期限是它自己的（缺省 180 秒），不是上面那个 150 毫秒。
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
async fn timeout_ms_bounds_how_long_a_resident_warmup_waits_for_a_slow_upstream() {
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

/// 只缩短 resident 预热请求自己的期限（缺省 180 秒），聊天的响应头超时照旧。同时交出代理，测试要看它的并发许可与熔断。
fn app_with_warmup_deadline(
    cards: Vec<ComponentCard>,
    deadline: Duration,
) -> (Router, Arc<proxy::ChatProxy>) {
    let mut proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
    proxy.warmup_header_timeout = deadline;
    let proxy = Arc::new(proxy);
    let app = build_app(AppState {
        cards,
        proxy: proxy.clone(),
        ..AppState::default()
    });
    (app, proxy)
}

/// 等到代理的并发许可全部回来：每个在途的缓冲请求都已结束、熔断的准入也已结算（许可随结果一起放，结算在放之前）。
async fn wait_for_all_permits(proxy: &proxy::ChatProxy, total: usize, patience: Duration) {
    let deadline = Instant::now() + patience;
    while proxy.available_permits_for_test() != total {
        assert!(
            Instant::now() < deadline,
            "the in-flight requests never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_default_resident_warmup_deadline_is_180_seconds_and_is_not_the_chat_header_timeout() {
    let proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
    assert_eq!(proxy::WARMUP_HEADER_TIMEOUT, Duration::from_secs(180));
    assert_eq!(proxy.warmup_header_timeout, Duration::from_secs(180));
    assert_ne!(proxy.warmup_header_timeout, proxy.header_timeout);
}

#[tokio::test]
async fn a_timed_out_resident_warmup_keeps_its_request_and_its_permit_until_the_requests_own_deadline()
 {
    let upstream = slow_upstream(Duration::from_secs(30)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_millis(4_000),
    );
    let total = proxy.available_permits_for_test();
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
        started.elapsed() < Duration::from_millis(3_000),
        "the caller was answered at timeout_ms, not at the request's own deadline: {:?}",
        started.elapsed()
    );
    // 调用方走了，请求没有：它在自己的任务里，还占着代理的并发许可，上游那一侧的连接还开着。
    assert_eq!(
        proxy.available_permits_for_test(),
        total - 1,
        "the abandoned request still holds its concurrency permit"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    // 它在自己的期限（4 秒）到点时收尾，不早于它。
    wait_for_all_permits(&proxy, total, Duration::from_secs(15)).await;
    assert!(
        started.elapsed() >= Duration::from_millis(3_900),
        "the request ran to its own deadline: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_requests_own_deadline_can_end_the_wait_before_timeout_ms_and_the_message_says_how_long_it_waited()
 {
    // 请求自己的期限比调用方愿意等的短：先到点的是它（代理的 504，翻成 `warmup_timeout`），不是 `timeout_ms`。
    let upstream = slow_upstream(Duration::from_secs(30)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_millis(600),
    );
    let total = proxy.available_permits_for_test();
    let started = Instant::now();
    let (status, body) = post(
        &app,
        warmup::WARMUP_PATH,
        json!({"model": "idoris/daily", "timeout_ms": 20_000}),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error"]["type"], "warmup_timeout");
    assert_eq!(body["error"]["reason_code"], "warmup_timeout");
    assert!(
        started.elapsed() < Duration::from_millis(8_000),
        "the request's own deadline ended the wait, not timeout_ms (20 s): {:?}",
        started.elapsed()
    );
    let message = body["error"]["remediation"].as_str().unwrap();
    let waited: u64 = message
        .split("within ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no waited time in {message:?}"));
    assert!(
        (500..8_000).contains(&waited),
        "the message says how long it really waited, not timeout_ms: {message}"
    );
    wait_for_all_permits(&proxy, total, Duration::from_secs(15)).await;
}

#[tokio::test]
async fn timeouts_of_resident_warmups_are_settled_into_the_breaker_after_the_caller_has_gone() {
    // 上游一直不答。调用方只等 1 秒，请求自己的期限是 1.5 秒：调用方走了之后请求还在跑，到期时代理按既有规则结算——504 是
    // 提供方故障，本地卡连续 2 次就断开（`CredentialClass::Local`）。
    let upstream = slow_upstream(Duration::from_secs(30)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_millis(1_500),
    );
    let total = proxy.available_permits_for_test();
    let body = json!({"model": "idoris/daily", "timeout_ms": 1_000});
    let first = post(&app, warmup::WARMUP_PATH, body.clone()).await;
    assert_eq!(first.0, StatusCode::GATEWAY_TIMEOUT, "{}", first.1);
    wait_for_all_permits(&proxy, total, Duration::from_secs(15)).await;
    // 一次故障不够：断路器还是闭合的，第二次预热照常发到上游。
    let second = post(&app, warmup::WARMUP_PATH, body.clone()).await;
    assert_eq!(second.0, StatusCode::GATEWAY_TIMEOUT, "{}", second.1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    wait_for_all_permits(&proxy, total, Duration::from_secs(15)).await;
    // 第二次故障到了阈值：这张卡冷却，第三次预热连上游都不碰。
    let third = post(&app, warmup::WARMUP_PATH, body).await;
    assert_eq!(third.0, StatusCode::SERVICE_UNAVAILABLE, "{}", third.1);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        2,
        "the open breaker kept the third warmup away from the upstream"
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
