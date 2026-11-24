//! IDORIS-CORE S6（Codex 预审 1 与 3）：`OmlxAdapter` 在**它解析凭证的那一处**核对请求准入时的引擎代际，也不向封上的端点发任何
//! 东西。用 wiremock 数「到了引擎的请求」：被拒绝的请求一个都不该出现（请求体里有用户的提示词，凭证是新引擎的）。
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use idoris_backend::generation::scoped;
use idoris_backend::{BackendError, ChatMessage, ChatRequest, RuntimeAdapter};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::omlx::key::{
    EndpointKey, block_endpoint, endpoint_generation, forget_endpoint, govern_endpoint,
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
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
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
    forget_endpoint(&endpoint);
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
    forget_endpoint(&endpoint);
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
    .unwrap()
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path());
    assert_eq!(fixed.engine_generation(), None);
    govern_endpoint(&endpoint, EndpointKey::Private("engine".into()));
    block_endpoint(&endpoint);
    // Not per-request: the construction-time key is what it always was, bound generation or not.
    scoped(Some(0), fixed.list()).await.unwrap();
    assert_eq!(received(&server).await, 1);
    forget_endpoint(&endpoint);
}
