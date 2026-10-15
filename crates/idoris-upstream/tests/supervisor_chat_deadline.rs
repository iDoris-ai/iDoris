//! Native chat deadline, end to end: a real `OmlxAdapter` behind a real
//! `Supervisor`, oMLX simulated by a loopback `wiremock` fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use idoris_backend::{ChatMessage, ChatRequest, Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn fixture(chat: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": true, "pinned": false,
                        "estimated_size": 1_073_741_824_u64}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 16
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat)
        .mount(&server)
        .await;
    server
}

async fn loaded_supervisor(
    server: &MockServer,
    adapter: OmlxAdapter,
    config: SupervisorConfig,
) -> idoris_backend::SupervisorHandle {
    let _ = server;
    let handle = Supervisor::spawn(
        Arc::new(adapter.with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())),
        config,
    )
    .expect("spawn");
    handle
        .load(
            "qwen3-8b",
            1.0,
            LoadPolicy {
                mode: LoadMode::OnDemand,
                keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
                admission: Admission::Coexist,
            },
        )
        .await
        .expect("load");
    handle
}

fn adapter(server: &MockServer) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        ..OmlxAdapterConfig::default()
    })
    .expect("adapter")
}

fn request() -> ChatRequest {
    ChatRequest {
        native: None,
        model: "qwen3-8b".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }],
    }
}

async fn assert_unload_succeeds(server: &MockServer, handle: &idoris_backend::SupervisorHandle) {
    // The real adapter must reach its unload endpoint after the chat releases
    // inflight; an error or a skipped adapter call must fail this check.
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;
    tokio::time::timeout(Duration::from_secs(5), handle.unload("qwen3-8b"))
        .await
        .expect("inflight must be released promptly")
        .expect("the real adapter unload must succeed after chat ends");
    server.verify().await;
}

/// Default configs, 11s generation: the old 10s adapter deadline failed it.
#[tokio::test]
async fn default_supervisor_path_survives_an_eleven_second_chat() {
    let server = fixture(
        ResponseTemplate::new(200)
            .set_delay(Duration::from_secs(11))
            .set_body_json(serde_json::json!({
                "model": "qwen3-8b",
                "choices": [{"message": {"content": "late answer"}}]
            })),
    )
    .await;
    let handle = loaded_supervisor(&server, adapter(&server), SupervisorConfig::default()).await;
    let start = Instant::now();
    let result = handle.chat(request(), CancellationToken::new()).await;
    let posts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count();
    assert_eq!(posts, 1);
    assert_eq!(
        result
            .unwrap_or_else(|e| panic!("elapsed={:?} err={e}", start.elapsed()))
            .content,
        "late answer"
    );
}

/// Outer-layer positive control: the adapter's own chat deadline is long but
/// the Supervisor's chat deadline is short, so only the outer layer can fire.
#[tokio::test]
async fn supervisor_chat_deadline_rejects_and_frees_the_slot() {
    let server = fixture(ResponseTemplate::new(200).set_delay(Duration::from_secs(30))).await;
    let config = SupervisorConfig {
        chat_call_timeout: Duration::from_millis(200),
        ..SupervisorConfig::default()
    };
    let handle = loaded_supervisor(&server, adapter(&server), config).await;
    let start = Instant::now();
    let err = handle
        .chat(request(), CancellationToken::new())
        .await
        .expect_err("outer chat deadline must fire");
    assert_eq!(err.reason_code(), "adapter_timed_out");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_unload_succeeds(&server, &handle).await;
}

/// Caller cancellation after the POST reached the fixture converges promptly,
/// far inside the 120s chat deadline.
#[tokio::test]
async fn cancel_after_post_sent_converges_through_the_supervisor() {
    let server = fixture(ResponseTemplate::new(200).set_delay(Duration::from_secs(60))).await;
    let handle = loaded_supervisor(&server, adapter(&server), SupervisorConfig::default()).await;
    let token = CancellationToken::new();
    let chat = tokio::spawn({
        let handle = handle.clone();
        let token = token.clone();
        async move { handle.chat(request(), token).await }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sent = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/v1/chat/completions");
        if sent {
            break;
        }
        assert!(Instant::now() < deadline, "POST never reached the fixture");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let start = Instant::now();
    token.cancel();
    let err = tokio::time::timeout(Duration::from_secs(2), chat)
        .await
        .expect("cancel must converge, not wait out the 60s response")
        .unwrap()
        .expect_err("cancelled");
    assert_eq!(err.reason_code(), "cancelled");
    assert!(start.elapsed() < Duration::from_secs(2));
    // The in-flight slot is released: an unload is not wedged behind the chat.
    assert_unload_succeeds(&server, &handle).await;
}
