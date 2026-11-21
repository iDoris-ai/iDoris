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

/// 等到合并表里没有在途请求、也没有 key 占着名额。名额在请求的结果被丢掉（并发许可还回）之后一刻才还，所以「许可全回来」之后
/// 立刻看表，可能还差那么一点点。
async fn wait_for_idle(proxy: &proxy::ChatProxy) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !proxy.warmups.is_idle() {
        assert!(
            Instant::now() < deadline,
            "the warmup registry never went idle"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_resident_warmup_may_wait_longer_than_any_ordinary_call_through_the_proxy() {
    // 它有自己的期限，正是因为冷加载比普通调用慢：比聊天（缺省 10 秒）与 embeddings（缺省 30 秒）的响应头超时都长。具体取多少是
    // 个取舍（规范里写的是 180 秒）；这里只守住「比普通调用长」这个关系，不把实现里的那个数再抄一遍。
    let proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
    assert!(proxy.warmup_header_timeout > proxy.header_timeout);
    assert!(proxy.warmup_header_timeout > proxy.embeddings_header_timeout);
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

/// 等到上游收到了 `count` 个请求（请求是在任务里异步发出去的）。
async fn wait_for_requests(server: &MockServer, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.received_requests().await.unwrap().len() < count {
        assert!(
            Instant::now() < deadline,
            "the upstream never received {count} request(s)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// W-1（PR-Daemon 对 #634 的预审）：同一模型的并发预热合并成一个请求；每把数据 key 自己发起的在途请求有上限
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn thirty_two_concurrent_warmups_of_one_resident_model_share_one_request_and_one_permit() {
    let upstream = slow_upstream(Duration::from_millis(1_500)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_secs(30),
    );
    let total = proxy.available_permits_for_test();
    let mut warmups = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let app = app.clone();
        warmups.spawn(async move {
            post(&app, warmup::WARMUP_PATH, json!({"model": "idoris/daily"})).await
        });
    }
    // 三十二个都在途：只有领头的那一个请求到了上游，只占一个并发许可，其余三十一个在等它的结果。
    wait_for_requests(&upstream, 1).await;
    assert_eq!(proxy.available_permits_for_test(), total - 1);
    let mut answered = 0;
    while let Some(done) = warmups.join_next().await {
        let (status, body) = done.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "ready");
        assert_eq!(body["already_loaded"], false);
        answered += 1;
    }
    assert_eq!(answered, 32);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "thirty-two warmups, one request"
    );
    wait_for_all_permits(&proxy, total, Duration::from_secs(10)).await;
    // 没有留下在途的请求，也没有 key 占着名额。
    wait_for_idle(&proxy).await;
}

#[tokio::test]
async fn warmups_of_different_models_each_send_their_own_request_and_hold_their_own_permit() {
    let upstream = slow_upstream(Duration::from_millis(1_200)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_secs(30),
    );
    let total = proxy.available_permits_for_test();
    let mut warmups = tokio::task::JoinSet::new();
    for model in ["model-one", "model-two"] {
        let app = app.clone();
        warmups
            .spawn(async move { post(&app, warmup::WARMUP_PATH, json!({"model": model})).await });
    }
    // 两个模型的请求都到了上游，各占一个并发许可。
    wait_for_requests(&upstream, 2).await;
    assert_eq!(proxy.available_permits_for_test(), total - 2);
    while let Some(done) = warmups.join_next().await {
        let (status, body) = done.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let models: Vec<String> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body).unwrap()["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(models.len(), 2);
    assert!(models.contains(&"model-one".to_string()), "{models:?}");
    assert!(models.contains(&"model-two".to_string()), "{models:?}");
    wait_for_all_permits(&proxy, total, Duration::from_secs(10)).await;
    wait_for_idle(&proxy).await;
}

#[tokio::test]
async fn the_same_model_name_on_two_cards_is_two_requests() {
    // 合并认的是（卡、端点、最小请求、模型）：两张卡后面各有一个叫 `shared` 的模型，是两个请求，不是一个。
    let first_upstream = slow_upstream(Duration::from_millis(800)).await;
    let second_upstream = slow_upstream(Duration::from_millis(800)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![
            resident("first", &first_upstream.uri(), "chat", "daily: \"shared\""),
            resident("second", &second_upstream.uri(), "chat", "fast: \"shared\""),
        ],
        Duration::from_secs(30),
    );
    let total = proxy.available_permits_for_test();
    let (daily, fast) = tokio::join!(
        post(&app, warmup::WARMUP_PATH, json!({"model": "idoris/daily"})),
        post(&app, warmup::WARMUP_PATH, json!({"model": "idoris/fast"})),
    );
    assert_eq!(daily.0, StatusCode::OK, "{}", daily.1);
    assert_eq!(fast.0, StatusCode::OK, "{}", fast.1);
    assert_eq!(daily.1["provider_id"], "first");
    assert_eq!(fast.1["provider_id"], "second");
    assert_eq!(first_upstream.received_requests().await.unwrap().len(), 1);
    assert_eq!(second_upstream.received_requests().await.unwrap().len(), 1);
    wait_for_all_permits(&proxy, total, Duration::from_secs(10)).await;
}

#[tokio::test]
async fn the_chat_and_the_embedding_minimal_request_of_one_model_are_two_requests() {
    let upstream = slow_upstream(Duration::from_millis(800)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "multi",
            &upstream.uri(),
            "chat, embedding",
            "daily: \"both\", embed: \"both\"",
        )],
        Duration::from_secs(30),
    );
    let total = proxy.available_permits_for_test();
    let (chat, embed) = tokio::join!(
        post(&app, warmup::WARMUP_PATH, json!({"model": "idoris/daily"})),
        post(&app, warmup::WARMUP_PATH, json!({"model": "idoris/embed"})),
    );
    assert_eq!(chat.0, StatusCode::OK, "{}", chat.1);
    assert_eq!(embed.0, StatusCode::OK, "{}", embed.1);
    let paths: Vec<String> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(
        paths.contains(&"/v1/chat/completions".to_string()),
        "{paths:?}"
    );
    assert!(paths.contains(&"/v1/embeddings".to_string()), "{paths:?}");
    wait_for_all_permits(&proxy, total, Duration::from_secs(10)).await;
}

#[tokio::test]
async fn a_slot_is_held_until_the_request_ends_not_until_the_caller_stops_waiting() {
    // 没有认证的调用方算一个整体，名额 2。请求自己的期限 3 秒，调用方只等 1 秒：两个调用方走了之后，两个请求还在上游那一侧。
    let upstream = slow_upstream(Duration::from_secs(30)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_secs(3),
    );
    // 这条只看名额：两个 504 会让这张卡的断路器断开（本地卡连续 2 次），断路器另有用例。
    proxy.set_breakers_enabled(false);
    let total = proxy.available_permits_for_test();
    let timed = |model: &'static str| json!({"model": model, "timeout_ms": 1_000});
    let (one, two) = tokio::join!(
        post(&app, warmup::WARMUP_PATH, timed("model-one")),
        post(&app, warmup::WARMUP_PATH, timed("model-two")),
    );
    assert_eq!(one.0, StatusCode::GATEWAY_TIMEOUT, "{}", one.1);
    assert_eq!(two.0, StatusCode::GATEWAY_TIMEOUT, "{}", two.1);
    assert_eq!(proxy.warmups.leading_for(None), 2);
    // 第三个模型：名额还被两个没结束的请求占着，429，什么也没发。
    let third = post(&app, warmup::WARMUP_PATH, timed("model-three")).await;
    assert_eq!(third.0, StatusCode::TOO_MANY_REQUESTS, "{}", third.1);
    assert_eq!(third.1["error"]["type"], "warmup_in_flight_limited");
    assert_eq!(third.1["error"]["reason_code"], "warmup_in_flight_limited");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    // 并入一个已经在途的模型不占名额：不是 429，等它的结果，等到自己的 `timeout_ms` 到点。
    let joined = post(&app, warmup::WARMUP_PATH, timed("model-one")).await;
    assert_eq!(joined.0, StatusCode::GATEWAY_TIMEOUT, "{}", joined.1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    // 请求自己结束（期限到点）之后名额还回来，第三个模型可以领头了。
    wait_for_all_permits(&proxy, total, Duration::from_secs(15)).await;
    wait_for_idle(&proxy).await;
    assert_eq!(proxy.warmups.leading_for(None), 0);
    let again = post(&app, warmup::WARMUP_PATH, timed("model-three")).await;
    assert_eq!(again.0, StatusCode::GATEWAY_TIMEOUT, "{}", again.1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
    wait_for_all_permits(&proxy, total, Duration::from_secs(15)).await;
    wait_for_idle(&proxy).await;
}

#[tokio::test]
async fn once_a_request_has_ended_the_next_warmup_of_that_model_sends_a_new_one() {
    let upstream = slow_upstream(Duration::from_millis(300)).await;
    let (app, proxy) = app_with_warmup_deadline(
        vec![resident(
            "lmstudio",
            &upstream.uri(),
            "chat",
            "daily: \"up-model\"",
        )],
        Duration::from_secs(30),
    );
    let total = proxy.available_permits_for_test();
    for round in 1..=3 {
        let (status, body) =
            post(&app, warmup::WARMUP_PATH, json!({"model": "idoris/daily"})).await;
        assert_eq!(status, StatusCode::OK, "round {round}: {body}");
        wait_for_all_permits(&proxy, total, Duration::from_secs(10)).await;
        wait_for_idle(&proxy).await;
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            round,
            "a finished request is not joined by later warmups"
        );
    }
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
