//! Native completions through the process-owned local HTTP runtime must expose
//! the configured iDoris model id, never the engine-side placeholder the
//! adapter sends (mlx_lm.server is addressed as `default_model`). The engine
//! must echo exactly the outbound engine model; a missing or different echo is
//! not evidence of what served the call and fails closed.
//!
//! Drives the public `LocalHttpRuntimeAdapter` (`load` → `chat` → `unload`)
//! against a real loopback HTTP fixture; the "runtime process" is a sleeping
//! shell script so readiness sees a live owned child.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use idoris_backend::{
    BackendError, ChatMessage, ChatRequest, ChatResponse, NativeCompletionRequest, RuntimeAdapter,
};
use idoris_upstream::{
    LocalHttpRuntimeAdapter, LocalHttpRuntimeConfig, LocalRuntimeKind, LocalRuntimeLaunch,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL_ID: &str = "qwen3-local-configured";

fn fake_runtime(dir: &Path) -> PathBuf {
    let script = dir.join("fake-runtime.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$0.args.tmp"
/bin/mv "$0.args.tmp" "$0.args"
exec sleep 30
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

async fn loaded_adapter(
    server: &MockServer,
    kind: LocalRuntimeKind,
    dir: &Path,
) -> LocalHttpRuntimeAdapter {
    loaded_adapter_configured(server, kind, dir, |_| {}).await
}

async fn loaded_adapter_configured(
    server: &MockServer,
    kind: LocalRuntimeKind,
    dir: &Path,
    configure: impl FnOnce(&mut LocalHttpRuntimeConfig),
) -> LocalHttpRuntimeAdapter {
    let model_path = match kind {
        LocalRuntimeKind::MlxLmServer => "/models/qwen-mlx",
        LocalRuntimeKind::LlamaCpp => "/models/qwen.gguf",
    };
    let launch =
        LocalRuntimeLaunch::new(kind, fake_runtime(dir), model_path, server.address().port())
            .unwrap();
    let mut config =
        LocalHttpRuntimeConfig::new(launch, MODEL_ID, 2.0, dir.join("load.pending")).unwrap();
    configure(&mut config);
    let adapter = LocalHttpRuntimeAdapter::new(config).unwrap();
    // A freshly written script can transiently hit ETXTBSY when another test
    // thread forks while its write fd is still open; retry briefly.
    let mut last = None;
    for _ in 0..20 {
        match adapter.load(MODEL_ID, None).await {
            Ok(()) => return adapter,
            Err(error) => last = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("fake runtime failed to spawn: {last:?}");
}

async fn mount_models(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "engine-side-name"}]
        })))
        .mount(server)
        .await;
}

async fn mount_completion(server: &MockServer, engine_model: &str, response: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": engine_model})))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
        .mount(server)
        .await;
}

fn native_request() -> ChatRequest {
    let mut parameters = serde_json::Map::new();
    parameters.insert(
        "tools".into(),
        json!([{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]),
    );
    parameters.insert("max_tokens".into(), json!(64));
    ChatRequest {
        native: Some(NativeCompletionRequest {
            messages: vec![json!({"role": "user", "content": "find it"})],
            parameters,
        }),
        model: MODEL_ID.into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "find it".into(),
        }],
    }
}

fn tool_completion(model: Option<&str>) -> Value {
    let mut body = json!({
        "id": "chatcmpl-engine-1",
        "object": "chat.completion",
        "created": 1_760_000_000,
        "system_fingerprint": "fp-engine",
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"it\"}"}
                }]
            }
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    });
    if let Some(model) = model {
        body["model"] = json!(model);
    }
    body
}

async fn native_chat(
    kind: LocalRuntimeKind,
    engine_model: &str,
    response: Value,
) -> Result<ChatResponse, BackendError> {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_models(&server).await;
    mount_completion(&server, engine_model, response).await;
    let adapter = loaded_adapter(&server, kind, dir.path()).await;
    let result = adapter
        .chat(native_request(), CancellationToken::new())
        .await;
    adapter.unload(MODEL_ID).await.unwrap();
    server.verify().await;
    result
}

fn assert_native_identity(response: &ChatResponse, original: &Value) {
    assert_eq!(response.model, MODEL_ID);
    let native = response.native.as_ref().expect("native body");
    assert_eq!(native["model"], json!(MODEL_ID));
    // Every other engine field passes through untouched.
    let mut expected = original.clone();
    expected["model"] = json!(MODEL_ID);
    assert_eq!(native, &expected);
    assert_eq!(native["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        native["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    assert_eq!(native["usage"]["total_tokens"], 18);
    assert_eq!(response.content, "");
}

#[tokio::test]
async fn mlx_native_response_exposes_configured_model_not_placeholder() {
    let original = tool_completion(Some("default_model"));
    let response = native_chat(
        LocalRuntimeKind::MlxLmServer,
        "default_model",
        original.clone(),
    )
    .await
    .unwrap();
    assert_native_identity(&response, &original);
}

#[tokio::test]
async fn mlx_native_wrong_engine_echo_fails_closed() {
    let error = native_chat(
        LocalRuntimeKind::MlxLmServer,
        "default_model",
        tool_completion(Some("some-other-model")),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, BackendError::Upstream { .. }), "{error:?}");
}

#[tokio::test]
async fn mlx_native_missing_engine_echo_fails_closed() {
    let error = native_chat(
        LocalRuntimeKind::MlxLmServer,
        "default_model",
        tool_completion(None),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, BackendError::Upstream { .. }), "{error:?}");
}

#[tokio::test]
async fn llama_native_with_launch_alias_requires_that_identity() {
    let original = tool_completion(Some(MODEL_ID));
    let response = native_chat(LocalRuntimeKind::LlamaCpp, MODEL_ID, original.clone())
        .await
        .unwrap();
    assert_native_identity(&response, &original);

    let error = native_chat(
        LocalRuntimeKind::LlamaCpp,
        MODEL_ID,
        tool_completion(Some("/models/qwen.gguf")),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, BackendError::Upstream { .. }), "{error:?}");
}

#[tokio::test]
async fn legacy_response_keeps_configured_model_without_echo_requirement() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_models(&server).await;
    mount_completion(
        &server,
        "default_model",
        json!({"choices": [{"message": {"content": "world"}}]}),
    )
    .await;
    let adapter = loaded_adapter(&server, LocalRuntimeKind::MlxLmServer, dir.path()).await;
    let response = adapter
        .chat(
            ChatRequest {
                native: None,
                model: MODEL_ID.into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hello".into(),
                }],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    adapter.unload(MODEL_ID).await.unwrap();
    server.verify().await;
    assert_eq!(response.model, MODEL_ID);
    assert_eq!(response.content, "world");
    assert!(response.native.is_none());
}

#[tokio::test]
async fn llama_load_passes_configured_alias_as_one_process_argument() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let adapter = loaded_adapter(&server, LocalRuntimeKind::LlamaCpp, dir.path()).await;
    let argv_path = dir.path().join("fake-runtime.sh.args");
    for _ in 0..500 {
        if argv_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let argv = std::fs::read_to_string(&argv_path).unwrap();
    adapter.unload(MODEL_ID).await.unwrap();
    let argv: Vec<_> = argv.lines().collect();
    assert_eq!(
        argv,
        vec![
            "--model",
            "/models/qwen.gguf",
            "--alias",
            MODEL_ID,
            "--host",
            "127.0.0.1",
            "--port",
            &server.address().port().to_string()
        ]
    );
}

#[tokio::test]
async fn local_native_default_chat_survives_an_eleven_second_response() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_models(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "default_model"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(11))
                .set_body_json(tool_completion(Some("default_model"))),
        )
        .expect(1)
        .mount(&server)
        .await;
    let adapter = loaded_adapter(&server, LocalRuntimeKind::MlxLmServer, dir.path()).await;
    let result = adapter
        .chat(native_request(), CancellationToken::new())
        .await;
    adapter.unload(MODEL_ID).await.unwrap();
    server.verify().await;
    assert!(
        result.is_ok(),
        "11-second native completion must outlive management timeout: {result:?}"
    );
}

async fn delayed_native(server: &MockServer, delay: Duration) {
    mount_models(server).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(tool_completion(Some("default_model"))),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn local_chat_deadline_is_independent_of_short_management_calls() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    delayed_native(&server, Duration::from_millis(100)).await;
    let adapter = loaded_adapter_configured(
        &server,
        LocalRuntimeKind::MlxLmServer,
        dir.path(),
        |config| {
            config.call_timeout = Duration::from_millis(40);
            config.chat_timeout = Duration::from_secs(2);
        },
    )
    .await;
    let result = adapter
        .chat(native_request(), CancellationToken::new())
        .await;
    adapter.unload(MODEL_ID).await.unwrap();
    server.verify().await;
    assert!(
        result.is_ok(),
        "chat must not use the 40ms management deadline: {result:?}"
    );
}

#[tokio::test]
async fn local_chat_short_deadline_rejects_after_post_and_allows_unload() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    delayed_native(&server, Duration::from_millis(300)).await;
    let adapter = loaded_adapter_configured(
        &server,
        LocalRuntimeKind::MlxLmServer,
        dir.path(),
        |config| {
            config.chat_timeout = Duration::from_millis(50);
        },
    )
    .await;
    let result = adapter
        .chat(native_request(), CancellationToken::new())
        .await;
    adapter.unload(MODEL_ID).await.unwrap();
    server.verify().await;
    assert!(
        matches!(result, Err(BackendError::Upstream { ref message }) if message == "local runtime chat timed out")
    );
}

#[tokio::test]
async fn local_chat_cancel_after_post_allows_unload() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    delayed_native(&server, Duration::from_secs(5)).await;
    let adapter = std::sync::Arc::new(
        loaded_adapter(&server, LocalRuntimeKind::MlxLmServer, dir.path()).await,
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let adapter = adapter.clone();
        let cancel = cancel.clone();
        async move { adapter.chat(native_request(), cancel).await }
    });
    for _ in 0..100 {
        if server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method == "POST")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.method == "POST")
    );
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    adapter.unload(MODEL_ID).await.unwrap();
    server.verify().await;
    assert!(matches!(result, Err(BackendError::Cancelled)));
}

#[test]
fn local_runtime_zero_management_or_chat_deadline_is_rejected() {
    for chat in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let launch = LocalRuntimeLaunch::new(
            LocalRuntimeKind::MlxLmServer,
            "/usr/bin/false",
            "/models/qwen-mlx",
            8081,
        )
        .unwrap();
        let mut config =
            LocalHttpRuntimeConfig::new(launch, MODEL_ID, 2.0, dir.path().join("load.pending"))
                .unwrap();
        assert_eq!(config.call_timeout, Duration::from_secs(10));
        assert_eq!(config.chat_timeout, Duration::from_secs(120));
        if chat {
            config.chat_timeout = Duration::ZERO;
        } else {
            config.call_timeout = Duration::ZERO;
        }
        assert!(LocalHttpRuntimeAdapter::new(config).is_err());
    }
}
