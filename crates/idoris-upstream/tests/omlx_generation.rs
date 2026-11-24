//! IDORIS-CORE S6（Codex 预审 1 与 3）：`OmlxAdapter` 在**它解析凭证的那一处**核对请求准入时的引擎代际，也不向封上的端点发任何
//! 东西。用 wiremock 数「到了引擎的请求」：被拒绝的请求一个都不该出现（请求体里有用户的提示词，凭证是新引擎的）。
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use idoris_backend::generation::scoped;
use idoris_backend::{BackendError, ChatMessage, ChatRequest, RuntimeAdapter};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::omlx::key::{
    EndpointKey, block_endpoint, endpoint_generation, govern_endpoint, release_endpoint,
};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn adapter(endpoint: &str) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: endpoint.to_owned(),
        api_key: Some("constructed".to_owned()),
        call_timeout: Duration::from_millis(500),
    })
    .unwrap()
    .with_per_request_key()
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hi"}}]
        })))
        .mount(&server)
        .await;
    server
}

async fn received(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

fn chat() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "a secret prompt".into(),
        }],
        native: None,
    }
}

#[tokio::test]
async fn work_admitted_for_a_replaced_engine_sends_nothing_to_the_new_one() {
    let server = server().await;
    let endpoint = server.uri();
    let adapter = adapter(&endpoint);
    govern_endpoint(&endpoint, EndpointKey::Private("first-engine".into()));
    let admitted = adapter.engine_generation().unwrap();
    assert_eq!(admitted, endpoint_generation(&endpoint));

    // The engine it was admitted for is there: the request goes out with that engine's key.
    scoped(Some(admitted), adapter.list()).await.unwrap();
    assert_eq!(received(&server).await, 1);

    // The engine is replaced and the new one's key registered. Every call of the old work is refused before it is
    // sent: a list, a load (a model name), a chat (a prompt), a status poll.
    govern_endpoint(&endpoint, EndpointKey::Private("second-engine".into()));
    let refused = [
        scoped(Some(admitted), adapter.list()).await.map(|_| ()),
        scoped(Some(admitted), adapter.load("m", Some(&policy()))).await,
        scoped(
            Some(admitted),
            adapter.chat(chat(), CancellationToken::new()),
        )
        .await
        .map(|_| ()),
        scoped(Some(admitted), adapter.status()).await.map(|_| ()),
    ];
    for result in refused {
        let error = result.unwrap_err();
        assert!(matches!(error, BackendError::Upstream { .. }), "{error:?}");
        assert!(error.to_string().contains("was replaced"), "{error}");
        assert!(!error.to_string().contains("second-engine"));
    }
    assert_eq!(received(&server).await, 1, "nothing reached the new engine");

    // Work admitted for the new engine, and work that was never bound, go out with the new key.
    let current = adapter.engine_generation().unwrap();
    scoped(Some(current), adapter.list()).await.unwrap();
    adapter.list().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(
            request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer second-engine"
        );
    }
    release_endpoint(&endpoint);
}

#[tokio::test]
async fn a_blocked_endpoint_is_sent_nothing_at_all() {
    let server = server().await;
    let endpoint = server.uri();
    let adapter = adapter(&endpoint);
    govern_endpoint(&endpoint, EndpointKey::Private("engine".into()));
    assert!(block_endpoint(&endpoint));
    // Bound or not, every kind of request is refused: no key goes out, and neither does the body.
    let current = adapter.engine_generation().unwrap();
    for bound in [None, Some(current)] {
        for result in [
            scoped(bound, adapter.list()).await.map(|_| ()),
            scoped(bound, adapter.load("m", Some(&policy()))).await,
            scoped(bound, adapter.chat(chat(), CancellationToken::new()))
                .await
                .map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("blocked"), "{error}");
        }
    }
    assert_eq!(received(&server).await, 0);
    // A verified listener's registration lifts it.
    govern_endpoint(&endpoint, EndpointKey::Private("next-engine".into()));
    adapter.list().await.unwrap();
    assert_eq!(received(&server).await, 1);
    release_endpoint(&endpoint);
}

#[tokio::test]
async fn an_adapter_with_a_fixed_key_has_no_generation_and_ignores_the_registry() {
    let server = server().await;
    let endpoint = server.uri();
    let fixed = OmlxAdapter::new(OmlxAdapterConfig {
        base_url: endpoint.clone(),
        api_key: Some("constructed".to_owned()),
        call_timeout: Duration::from_millis(500),
    })
    .unwrap();
    assert_eq!(fixed.engine_generation(), None);
    govern_endpoint(&endpoint, EndpointKey::Private("engine".into()));
    block_endpoint(&endpoint);
    // Not per-request: the construction-time key is what it always was, bound generation or not.
    scoped(Some(0), fixed.list()).await.unwrap();
    assert_eq!(received(&server).await, 1);
    release_endpoint(&endpoint);
}

/// Codex 预审 H2：缓存的管理会话 cookie 不经过登录，登录那一处的核对管不到它。加载的 POST 返回时引擎被换代，随后的钉住（PUT）
/// 不能带着旧 cookie 发给新引擎；加载按「结果未知」报告，调用方据此保留加载围栏。
#[tokio::test]
async fn a_cached_admin_session_is_not_used_after_the_engine_was_replaced_mid_load() {
    let server = MockServer::start().await;
    let endpoint = server.uri();
    Mock::given(method("POST"))
        .and(path("/v1/models/m/load"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "omlx_admin_session=cached; HttpOnly")
                .set_body_json(serde_json::json!({"success": true})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/admin/api/models/m/settings"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "m", "loaded": true, "pinned": true, "estimated_size": 1 << 30}]
        })))
        .mount(&server)
        .await;
    let adapter = adapter(&endpoint);
    let resident = LoadPolicy {
        mode: LoadMode::Resident,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    };
    govern_endpoint(&endpoint, EndpointKey::Private("first-engine".into()));
    let admitted = adapter.engine_generation().unwrap();
    // The first load logs in and caches the session cookie.
    scoped(Some(admitted), adapter.load("m", Some(&resident)))
        .await
        .unwrap();
    let puts = |requests: &[wiremock::Request]| {
        requests
            .iter()
            .filter(|r| r.method.as_str() == "PUT")
            .count()
    };
    assert_eq!(puts(&server.received_requests().await.unwrap()), 1);

    // The second load's POST is in flight when the engine is replaced; its pin must not be sent.
    let swap_endpoint = endpoint.clone();
    let swap = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        govern_endpoint(&swap_endpoint, EndpointKey::Private("second-engine".into()));
    });
    let error = scoped(Some(admitted), adapter.load("m", Some(&resident)))
        .await
        .unwrap_err();
    swap.await.unwrap();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    assert!(error.to_string().contains("was replaced"), "{error}");
    assert_eq!(
        puts(&server.received_requests().await.unwrap()),
        1,
        "the cached cookie was not sent to the replacement"
    );
    release_endpoint(&endpoint);
}
