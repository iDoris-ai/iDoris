//! Issue #582: a `resident` oMLX card goes through the direct HTTP proxy and
//! must carry the same `IDORIS_OMLX_API_KEY` the lifecycle (`on_demand`) path
//! uses — and only oMLX cards may receive it. Upstream 401/403 become a
//! structured iDoris error with no upstream text.
//!
//! The key lives in the process environment, so each case runs in a child
//! process (same pattern as `models_auth.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use idoris_router::{AppState, build_app, dispatch::BoundSupervisor};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHILD: &str = "IDORIS_RESIDENT_KEY_CASE";
const KEY_ENV: &str = "IDORIS_OMLX_API_KEY";
const KEY: &str = "omlx-secret-key-582";
const CALLER_KEY: &str = "idoris-virtual-key-caller";
const UPSTREAM_TEXT: &str = "API key required";

fn card(id: &str, endpoint: &str, mode: LoadMode) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = endpoint.to_string();
    card.provider.id = id.to_string();
    card.load_policy.as_mut().unwrap().mode = mode;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"daily": "Qwen3-0.6B-4bit", "fast": "Qwen3-0.6B-4bit"}
        }))
        .unwrap(),
    );
    card
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("X-iDoris-Privacy", "local_only")
        .header("authorization", format!("Bearer {CALLER_KEY}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A backend with oMLX auth enabled: 200 only with the exact Bearer, else a
/// 401 carrying distinctive text that must never reach the caller.
async fn auth_backend() -> MockServer {
    let server = MockServer::start().await;
    let ok = ResponseTemplate::new(200).set_body_json(json!({
        "model": "Qwen3-0.6B-4bit",
        "choices": [{"message": {"content": "answer"}}],
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1]}]
    }));
    let sse = ResponseTemplate::new(200).set_body_raw(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
        "text/event-stream",
    );
    for (p, tpl) in [
        ("/v1/chat/completions", ok.clone()),
        ("/v1/embeddings", ok.clone()),
    ] {
        Mock::given(method("POST"))
            .and(path(p))
            .and(header("authorization", format!("Bearer {KEY}").as_str()))
            .respond_with(tpl)
            .with_priority(2)
            .mount(&server)
            .await;
    }
    // Streaming requests carry `"stream": true`; matched by body below.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(wiremock::matchers::body_partial_json(
            json!({"stream": true}),
        ))
        .respond_with(sse)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": UPSTREAM_TEXT, "type": "authentication_error"}
        })))
        .with_priority(5)
        .mount(&server)
        .await;
    server
}

async fn body_of(response: axum::response::Response) -> (StatusCode, String) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn chat(stream: bool) -> Value {
    json!({"model": "idoris/daily", "stream": stream,
           "messages": [{"role": "user", "content": "hi"}]})
}

fn assert_no_caller_key(requests: &[wiremock::Request]) {
    for request in requests {
        let auth = request
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string());
        assert_ne!(auth.as_deref(), Some(&*format!("Bearer {CALLER_KEY}")));
        assert!(!auth.unwrap_or_default().contains(CALLER_KEY));
    }
}

fn assert_mapped_auth_error(status: StatusCode, body: &str) {
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let json: Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["error"]["type"], "upstream_auth_failed");
    assert_eq!(json["error"]["reason_code"], "upstream_auth_failed");
    assert!(!body.contains(UPSTREAM_TEXT), "{body}");
    assert!(!body.contains(KEY) && !body.contains(CALLER_KEY), "{body}");
}

async fn run_case(case: &str) {
    match case {
        "resident_key" => {
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![card("omlx", &upstream.uri(), LoadMode::Resident)],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.clone()
                    .oneshot(post("/v1/chat/completions", chat(false)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(body.contains("answer"));
            let (status, body) = body_of(
                app.clone()
                    .oneshot(post("/v1/chat/completions", chat(true)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(body.contains("[DONE]"));
            let (status, body) = body_of(
                app.oneshot(post(
                    "/v1/embeddings",
                    json!({"model": "Qwen3-0.6B-4bit", "input": "hi"}),
                ))
                .await
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 3);
            assert_no_caller_key(&requests);
        }
        "resident_nokey" => {
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![card("omlx", &upstream.uri(), LoadMode::Resident)],
                ..AppState::default()
            });
            for stream in [false, true] {
                let (status, body) = body_of(
                    app.clone()
                        .oneshot(post("/v1/chat/completions", chat(stream)))
                        .await
                        .unwrap(),
                )
                .await;
                assert_mapped_auth_error(status, &body);
            }
            let (status, body) = body_of(
                app.oneshot(post(
                    "/v1/embeddings",
                    json!({"model": "Qwen3-0.6B-4bit", "input": "hi"}),
                ))
                .await
                .unwrap(),
            )
            .await;
            assert_mapped_auth_error(status, &body);
            assert_no_caller_key(&upstream.received_requests().await.unwrap());
        }
        "non_omlx" => {
            // Same oMLX key in the env, but the card is not the oMLX provider.
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![card("other-local", &upstream.uri(), LoadMode::Resident)],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.oneshot(post("/v1/chat/completions", chat(false)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_mapped_auth_error(status, &body);
            let requests = upstream.received_requests().await.unwrap();
            assert!(!requests.is_empty());
            for request in &requests {
                assert!(
                    request.headers.get("authorization").is_none(),
                    "the oMLX key must not reach a non-oMLX card"
                );
            }
        }
        "on_demand" => {
            let upstream = auth_backend().await;
            Mock::given(method("GET"))
                .and(path("/api/status"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "loaded_models": [], "model_memory_used": 0,
                    "model_memory_max": 16, "pressure": "ok"
                })))
                .with_priority(1)
                .mount(&upstream)
                .await;
            Mock::given(method("POST"))
                .and(path("/v1/models/Qwen3-0.6B-4bit/load"))
                .respond_with(ResponseTemplate::new(200))
                .with_priority(1)
                .mount(&upstream)
                .await;
            Mock::given(method("GET"))
                .and(path("/v1/models/status"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "models": [{"id": "Qwen3-0.6B-4bit", "loaded": true, "pinned": false,
                                "estimated_size": 1024_u64 * 1024 * 1024}]
                })))
                .with_priority(1)
                .mount(&upstream)
                .await;
            let c = card("omlx", &upstream.uri(), LoadMode::OnDemand);
            let app = build_app(AppState {
                runtimes: Some(BoundSupervisor::spawn_omlx(&c).unwrap()).into(),
                cards: vec![c],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.oneshot(post("/v1/chat/completions", chat(false)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_no_caller_key(&upstream.received_requests().await.unwrap());
        }
        other => panic!("unknown case {other}"),
    }
}

#[tokio::test]
async fn resident_omlx_key_attachment_and_auth_error_mapping() {
    if let Ok(case) = std::env::var(CHILD) {
        run_case(&case).await;
        return;
    }
    for (case, key) in [
        ("resident_key", Some(KEY)),
        ("resident_nokey", None),
        ("non_omlx", Some(KEY)),
        ("on_demand", Some(KEY)),
    ] {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "resident_omlx_key_attachment_and_auth_error_mapping",
            "--nocapture",
        ])
        .env(CHILD, case)
        .env_remove(KEY_ENV);
        if let Some(key) = key {
            cmd.env(KEY_ENV, key);
        }
        let output = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "case {case}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        // The key is never logged.
        assert!(
            !stdout.contains(KEY) && !stderr.contains(KEY),
            "case {case}"
        );
    }
}
