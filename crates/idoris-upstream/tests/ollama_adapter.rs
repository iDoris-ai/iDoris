//! Wiremock coverage for `OllamaAdapter`. No real Ollama daemon.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use idoris_backend::{BackendStatus, ChatMessage, ChatRequest, Pressure, RuntimeAdapter};
use idoris_contracts::LoadPolicy;
use idoris_contracts::common::Capability;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::{OllamaAdapter, OllamaAdapterConfig};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const GIB: u64 = 1024 * 1024 * 1024;

fn adapter_for(server: &MockServer) -> OllamaAdapter {
    OllamaAdapter::new(OllamaAdapterConfig {
        base_url: server.uri(),
        call_timeout: Duration::from_millis(500),
    })
    .unwrap()
}

fn fast_adapter(server: &MockServer) -> OllamaAdapter {
    OllamaAdapter::new(OllamaAdapterConfig {
        base_url: server.uri(),
        call_timeout: Duration::from_millis(200),
    })
    .unwrap()
}

fn idle(seconds: u32) -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl {
            idle_ttl_s: seconds,
        },
        admission: Admission::Coexist,
    }
}

fn pinned() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::Resident,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    }
}

fn unpinned() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::Pinned { pinned: false },
        admission: Admission::Coexist,
    }
}

fn done() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"done": true}))
}

fn running(name: &str, size: u64, vram: u64) -> Value {
    json!({
        "name": name,
        "model": name,
        "size": size,
        "size_vram": vram
    })
}

async fn mount(
    server: &MockServer,
    method_name: &str,
    path_name: &str,
    response: ResponseTemplate,
) {
    Mock::given(method(method_name))
        .and(path(path_name))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

fn paths(recorded: &[Request]) -> Vec<&str> {
    recorded.iter().map(|request| request.url.path()).collect()
}

fn body(request: &Request) -> Value {
    request.body_json().unwrap()
}

fn chat_request() -> ChatRequest {
    ChatRequest {
        model: "qwen3:8b".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }],
    }
}

#[test]
fn capability_table_is_chat_without_rerank_or_hard_pin() {
    assert_eq!(idoris_upstream::OLLAMA_CAPABILITIES, &[Capability::Chat]);
}

#[test]
fn load_fence_path_is_stable_for_the_same_engine() {
    let make = |base_url: &str| {
        OllamaAdapter::new(OllamaAdapterConfig {
            base_url: base_url.into(),
            call_timeout: Duration::from_secs(1),
        })
        .unwrap()
    };
    let first = make("http://localhost:11434").load_fence_path().unwrap();
    let restarted = make("http://LOCALHOST:11434/").load_fence_path().unwrap();
    assert_eq!(first, restarted);
    assert!(first.ends_with("load.pending"));
    assert!(first.components().any(|part| part.as_os_str() == "ollama"));
    assert_ne!(
        first,
        make("http://localhost:11435").load_fence_path().unwrap()
    );
    let explicit = idoris_backend::mock::temporary_load_fence_path();
    assert_eq!(
        make("http://localhost:11434")
            .with_load_fence_path(explicit.clone())
            .load_fence_path()
            .unwrap(),
        explicit
    );
    let relative = make("http://localhost:11434")
        .with_load_fence_path(std::path::PathBuf::from("relative-fence"))
        .load_fence_path()
        .unwrap_err();
    assert_eq!(relative.reason_code(), "invalid_request");
}

#[tokio::test]
async fn list_and_status_read_installed_and_resident_bytes() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/tags",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "qwen3:8b", "size": 2 * GIB},
                {"name": "tiny", "size": 0}
            ]
        })),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                running("qwen3:8b", 2 * GIB, GIB),
                {
                    "name": "alias-name",
                    "model": "alias-model",
                    "size": GIB,
                    "size_vram": 0
                }
            ]
        })),
    )
    .await;
    let adapter = adapter_for(&server);
    let models = adapter.list().await.unwrap();
    assert_eq!(models[0].id, "qwen3:8b");
    assert_eq!(models[0].memory_gb, 2.0);
    assert_eq!(models[1].memory_gb, 0.0);
    let status = adapter.status().await.unwrap();
    assert_eq!(
        status,
        BackendStatus {
            pressure: Pressure::Unknown,
            used_gb: 3.0,
            model_memory_max_gb: 0.0,
            loaded: vec![
                "qwen3:8b".to_string(),
                "alias-name".to_string(),
                "alias-model".to_string(),
            ],
        }
    );
}

#[tokio::test]
async fn status_rejects_a_ps_payload_without_vram_or_models() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "qwen3:8b", "size": GIB}]
        })),
    )
    .await;
    let err = adapter_for(&server).status().await.unwrap_err();
    assert_eq!(err.reason_code(), "upstream_error");
    assert!(err.to_string().contains("size_vram"));

    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({})),
    )
    .await;
    let err = adapter_for(&server).status().await.unwrap_err();
    assert!(err.to_string().contains("models"));
}

#[tokio::test]
async fn load_sends_an_empty_prompt_and_a_finite_keep_alive() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    let adapter = adapter_for(&server);
    adapter.load("qwen3:8b", Some(&idle(90))).await.unwrap();
    let recorded = requests(&server).await;
    assert_eq!(paths(&recorded), ["/api/generate"]);
    assert_eq!(
        body(&recorded[0]),
        json!({"model": "qwen3:8b", "prompt": "", "stream": false, "keep_alive": 90})
    );
}

#[tokio::test]
async fn load_without_a_policy_omits_keep_alive() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    adapter_for(&server).load("qwen3:8b", None).await.unwrap();
    let recorded = requests(&server).await;
    let payload = body(&recorded[0]);
    assert!(payload.get("keep_alive").is_none());
    assert_eq!(payload["prompt"], "");
}

#[tokio::test]
async fn unpinned_load_uses_the_documented_idle_default_not_negative_keep_alive() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    adapter_for(&server)
        .load("qwen3:8b", Some(&unpinned()))
        .await
        .unwrap();
    let recorded = requests(&server).await;
    assert_eq!(body(&recorded[0])["keep_alive"], 300);
}

#[tokio::test]
async fn pinned_load_is_refused_before_any_request() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    let err = adapter_for(&server)
        .load("qwen3:8b", Some(&pinned()))
        .await
        .unwrap_err();
    assert_eq!(err.reason_code(), "invalid_request");
    let message = err.to_string();
    assert!(message.contains("keep_alive -1"));
    assert!(message.contains("memory pressure"));
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn resident_mode_with_a_ttl_does_not_become_a_hard_pin() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    let policy = LoadPolicy {
        mode: LoadMode::Resident,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 15 },
        admission: Admission::Coexist,
    };
    adapter_for(&server)
        .load("qwen3:8b", Some(&policy))
        .await
        .unwrap();
    assert_eq!(body(&requests(&server).await[0])["keep_alive"], 15);
}

#[tokio::test]
async fn concurrent_loads_keep_their_own_finite_timers() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    let adapter = adapter_for(&server);
    let first = idle(10);
    let second = idle(20);
    let (left, right) = tokio::join!(
        adapter.load("alpha", Some(&first)),
        adapter.load("beta", Some(&second)),
    );
    left.unwrap();
    right.unwrap();
    let mut timers = requests(&server)
        .await
        .iter()
        .map(|request| {
            let payload = body(request);
            assert!(payload.get("keep_alive").unwrap().as_i64().unwrap() > 0);
            (
                payload["model"].as_str().unwrap().to_string(),
                payload["keep_alive"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    timers.sort();
    assert_eq!(
        timers,
        vec![("alpha".to_string(), 10), ("beta".to_string(), 20)]
    );
}

#[tokio::test]
async fn load_timeout_or_incomplete_done_is_unconfirmed() {
    for response in [
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
        ResponseTemplate::new(500).set_body_json(json!({"done": true})),
        ResponseTemplate::new(200).set_body_json(json!({"done": false})),
        ResponseTemplate::new(200).set_body_string("not-json"),
    ] {
        let server = MockServer::start().await;
        mount(&server, "POST", "/api/generate", response).await;
        let err = fast_adapter(&server)
            .load("qwen3:8b", Some(&idle(30)))
            .await
            .unwrap_err();
        assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
        assert!(!err.to_string().contains("not-json"));
    }
}

#[tokio::test]
async fn load_client_errors_are_definite() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", ResponseTemplate::new(404)).await;
    let missing = adapter_for(&server)
        .load("qwen3:8b", Some(&idle(30)))
        .await
        .unwrap_err();
    assert_eq!(missing.reason_code(), "model_not_found");

    let server = MockServer::start().await;
    mount(
        &server,
        "POST",
        "/api/generate",
        ResponseTemplate::new(400).set_body_string("secret-body"),
    )
    .await;
    let rejected = adapter_for(&server)
        .load("qwen3:8b", Some(&idle(30)))
        .await
        .unwrap_err();
    assert_eq!(rejected.reason_code(), "upstream_error");
    assert!(rejected.to_string().contains("400"));
    assert!(!rejected.to_string().contains("secret-body"));
}

#[tokio::test]
async fn unload_confirms_the_model_left_ps() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", done()).await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [running("other", GIB, GIB)]
        })),
    )
    .await;
    adapter_for(&server).unload("qwen3:8b").await.unwrap();
    let recorded = requests(&server).await;
    assert_eq!(paths(&recorded), ["/api/generate", "/api/ps"]);
    assert_eq!(
        body(&recorded[0]),
        json!({"model": "qwen3:8b", "prompt": "", "stream": false, "keep_alive": 0})
    );
}

#[tokio::test]
async fn unload_is_ok_when_generate_404s_and_ps_does_not_list_the_model() {
    let server = MockServer::start().await;
    mount(&server, "POST", "/api/generate", ResponseTemplate::new(404)).await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({"models": []})),
    )
    .await;
    adapter_for(&server).unload("qwen3:8b").await.unwrap();
    assert_eq!(
        paths(&requests(&server).await),
        ["/api/generate", "/api/ps"]
    );
}

#[tokio::test]
async fn unload_confirmation_failure_is_unconfirmed() {
    let cases = [
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [running("qwen3:8b", GIB, GIB)]
        })),
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
        ResponseTemplate::new(500),
        ResponseTemplate::new(200).set_body_json(json!({})),
    ];
    for response in cases {
        let server = MockServer::start().await;
        mount(&server, "POST", "/api/generate", done()).await;
        mount(&server, "GET", "/api/ps", response).await;
        let err = fast_adapter(&server).unload("qwen3:8b").await.unwrap_err();
        assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
        assert_eq!(paths(&requests(&server).await)[0], "/api/generate");
    }
}

#[tokio::test]
async fn unload_generate_timeout_does_not_pretend_ps_confirmed_release() {
    let server = MockServer::start().await;
    mount(
        &server,
        "POST",
        "/api/generate",
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({"models": []})),
    )
    .await;
    let err = fast_adapter(&server).unload("qwen3:8b").await.unwrap_err();
    assert_eq!(err.reason_code(), "load_unconfirmed");
    assert_eq!(paths(&requests(&server).await), ["/api/generate"]);
}

#[tokio::test]
async fn probe_ready_uses_ps_and_absorbs_transient_failures() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [running("qwen3:8b", GIB, 0)]
        })),
    )
    .await;
    let adapter = adapter_for(&server);
    assert!(adapter.probe_ready("qwen3:8b").await.unwrap());
    assert!(!adapter.probe_ready("other").await.unwrap());

    for response in [
        ResponseTemplate::new(500),
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
    ] {
        let server = MockServer::start().await;
        mount(&server, "GET", "/api/ps", response).await;
        assert!(!fast_adapter(&server).probe_ready("qwen3:8b").await.unwrap());
    }

    let down = OllamaAdapter::new(OllamaAdapterConfig {
        base_url: "http://127.0.0.1:1".into(),
        call_timeout: Duration::from_millis(200),
    })
    .unwrap();
    assert!(!down.probe_ready("qwen3:8b").await.unwrap());

    let server = MockServer::start().await;
    mount(&server, "GET", "/api/ps", ResponseTemplate::new(401)).await;
    let err = adapter_for(&server)
        .probe_ready("qwen3:8b")
        .await
        .unwrap_err();
    assert_eq!(err.reason_code(), "upstream_error");
    assert!(err.to_string().contains("401"));
}

#[tokio::test]
async fn chat_requires_a_loaded_model_and_does_not_change_keep_alive() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({"models": []})),
    )
    .await;
    mount(
        &server,
        "POST",
        "/v1/chat/completions",
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "should-not-run"}}]
        })),
    )
    .await;
    let missing = adapter_for(&server)
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(missing.reason_code(), "model_unavailable");
    assert_eq!(paths(&requests(&server).await), ["/api/ps"]);

    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [running("qwen3:8b", GIB, GIB)]
        })),
    )
    .await;
    mount(
        &server,
        "POST",
        "/v1/chat/completions",
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "hello there"}}]
        })),
    )
    .await;
    let adapter = adapter_for(&server);
    let response = adapter
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.model, "qwen3:8b");
    assert_eq!(response.content, "hello there");
    let recorded = requests(&server).await;
    assert_eq!(paths(&recorded), ["/api/ps", "/v1/chat/completions"]);
    let payload = body(&recorded[1]);
    assert!(payload.get("keep_alive").is_none());
    assert_eq!(payload["messages"][0]["content"], "hi");
}

#[tokio::test]
async fn chat_cancellation_and_upstream_errors_do_not_leak_bodies() {
    let server = MockServer::start().await;
    let adapter = adapter_for(&server);
    let token = CancellationToken::new();
    token.cancel();
    let err = adapter.chat(chat_request(), token).await.unwrap_err();
    assert_eq!(err.reason_code(), "cancelled");
    assert!(requests(&server).await.is_empty());

    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [running("qwen3:8b", GIB, 0)]
        })),
    )
    .await;
    mount(
        &server,
        "POST",
        "/v1/chat/completions",
        ResponseTemplate::new(200)
            .set_body_string("slow")
            .set_delay(Duration::from_secs(5)),
    )
    .await;
    let adapter = OllamaAdapter::new(OllamaAdapterConfig {
        base_url: server.uri(),
        call_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        child.cancel();
    });
    let err = tokio::time::timeout(Duration::from_secs(1), adapter.chat(chat_request(), token))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(err.reason_code(), "cancelled");

    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/ps",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [running("qwen3:8b", GIB, 0)]
        })),
    )
    .await;
    mount(
        &server,
        "POST",
        "/v1/chat/completions",
        ResponseTemplate::new(400).set_body_string("secret-body"),
    )
    .await;
    let err = adapter_for(&server)
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.reason_code(), "upstream_error");
    assert!(err.to_string().contains("400"));
    assert!(!err.to_string().contains("secret-body"));
}

#[tokio::test]
async fn dyn_adapter_lists_through_the_trait() {
    let server = MockServer::start().await;
    mount(
        &server,
        "GET",
        "/api/tags",
        ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "qwen3:8b", "size": GIB}]
        })),
    )
    .await;
    let adapter: Box<dyn RuntimeAdapter> = Box::new(adapter_for(&server));
    let models = adapter.list().await.unwrap();
    assert_eq!(models[0].id, "qwen3:8b");
    assert_eq!(models[0].memory_gb, 1.0);
}
