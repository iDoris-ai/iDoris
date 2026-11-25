//! Issue #582: a `resident` oMLX card goes through the direct HTTP proxy and
//! must carry the same `IDORIS_OMLX_API_KEY` the lifecycle (`on_demand`) path
//! uses — and only oMLX cards may receive it. Upstream 401/403 become a
//! structured iDoris error with no upstream text.
//!
//! The key lives in the process environment, so each case runs in a child
//! process (same pattern as `models_auth.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/state_isolation.rs"]
mod state_isolation;
use state_isolation::IsolateState;

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

/// `setup omlx --bind embed=…` 生成的伴生卡的形状：id `omlx-embed`，只有 embedding 能力，resident，绑定 embed 角色。
fn companion(endpoint: &str) -> ComponentCard {
    let mut card = card("omlx-embed", endpoint, LoadMode::Resident);
    card.provider.capabilities = vec![idoris_contracts::common::Capability::Embedding];
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"embed": "bge-m3-mlx-fp16"}
        }))
        .unwrap(),
    );
    card
}

/// 自定义 OpenAI 兼容上游（`setup upstream` 的形状：带 `idoris_upstream`），可以伪装 id、`flavor` 与 `version_pin`。
fn custom_upstream(id: &str, endpoint: &str, flavor: &str, version_pin: &str) -> ComponentCard {
    let mut card = card(id, endpoint, LoadMode::Resident);
    card.version_pin = version_pin.to_string();
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"daily": "Qwen3-0.6B-4bit", "fast": "Qwen3-0.6B-4bit"},
            "idoris_upstream": {"kind": "openai_compat", "flavor": flavor}
        }))
        .unwrap(),
    );
    card
}

fn embed(model: &str) -> Value {
    json!({"model": model, "input": "hi"})
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
            let mut other = card("other-local", &upstream.uri(), LoadMode::Resident);
            other.version_pin = "lmstudio@unknown".to_string();
            other
                .provider
                .capabilities
                .push(idoris_contracts::common::Capability::Rerank);
            let app = build_app(AppState {
                cards: vec![other],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.clone()
                    .oneshot(post("/v1/chat/completions", chat(false)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_mapped_auth_error(status, &body);
            // rerank 路径同样按卡判定：非 oMLX 卡不带 key（响应是什么不重要，要看的是线上有没有 Authorization）。
            let _ = app
                .oneshot(post(
                    "/v1/rerank",
                    json!({"model": "idoris/daily", "query": "q", "documents": ["a", "b"]}),
                ))
                .await
                .unwrap();
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2, "chat 与 rerank 都要到达上游");
            assert!(requests.iter().any(|r| r.url.path() == "/v1/rerank"));
            for request in &requests {
                assert!(
                    request.headers.get("authorization").is_none(),
                    "the oMLX key must not reach a non-oMLX card"
                );
            }
        }
        "companion_key" => {
            // 伴生卡 omlx-embed 与主卡 omlx 同一个 endpoint：idoris/embed 路由到伴生卡，带同一个 key。
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![
                    card("omlx", &upstream.uri(), LoadMode::OnDemand),
                    companion(&upstream.uri()),
                ],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.oneshot(post("/v1/embeddings", embed("idoris/embed")))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let requests = upstream.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url.path(), "/v1/embeddings");
            let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(
                sent["model"], "bge-m3-mlx-fp16",
                "角色绑定把 idoris/embed 换成具体模型"
            );
            assert_no_caller_key(&requests);
        }
        "companion_nokey" => {
            // 没有 key 时伴生卡与主卡一样，拿到上游的 401 并映射成结构化错误。
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![companion(&upstream.uri())],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.oneshot(post("/v1/embeddings", embed("idoris/embed")))
                    .await
                    .unwrap(),
            )
            .await;
            assert_mapped_auth_error(status, &body);
        }
        "disguised_upstream" => {
            // 自定义上游把 id、flavor、version_pin 都伪装成 oMLX、端点也是 loopback：仍然一个字节的 key 都不带。
            for (id, flavor, pin) in [
                ("omlx", "omlx", "omlx@0.6.4"),
                ("omlx-embed", "omlx", "omlx@0.6.4"),
                ("lmstudio-9", "omlx", "omlx@0.6.4"),
                ("lmstudio-9", "openai_compat", "omlx@0.6.4"),
            ] {
                let upstream = auth_backend().await;
                let mut c = custom_upstream(id, &upstream.uri(), flavor, pin);
                c.provider.capabilities = vec![
                    idoris_contracts::common::Capability::Chat,
                    idoris_contracts::common::Capability::Embedding,
                ];
                let app = build_app(AppState {
                    cards: vec![c],
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
                    app.oneshot(post("/v1/embeddings", embed("Qwen3-0.6B-4bit")))
                        .await
                        .unwrap(),
                )
                .await;
                assert_mapped_auth_error(status, &body);
                let requests = upstream.received_requests().await.unwrap();
                assert_eq!(requests.len(), 3, "{id}/{flavor}");
                for request in &requests {
                    assert!(
                        request.headers.get("authorization").is_none(),
                        "{id}/{flavor}/{pin}: a custom upstream must never get the oMLX key"
                    );
                }
            }
        }
        "id_alone_is_not_enough" => {
            // 只是叫 omlx：卡的引擎种类（version_pin）不是 omlx，就没有 key。
            let upstream = auth_backend().await;
            let mut c = card("omlx", &upstream.uri(), LoadMode::Resident);
            c.version_pin = "lmstudio@unknown".to_string();
            let app = build_app(AppState {
                cards: vec![c],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.oneshot(post("/v1/chat/completions", chat(false)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_mapped_auth_error(status, &body);
            for request in &upstream.received_requests().await.unwrap() {
                assert!(request.headers.get("authorization").is_none());
            }
        }
        "other_id_omlx_card_gets_key" => {
            // 按卡识别，不按 id：loopback、version_pin 是 omlx@ 的卡（比如手写的 omlx-secondary）带 key。
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![card("omlx-secondary", &upstream.uri(), LoadMode::Resident)],
                ..AppState::default()
            });
            let (status, body) = body_of(
                app.oneshot(post("/v1/chat/completions", chat(false)))
                    .await
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
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
                runtimes: Some(
                    BoundSupervisor::spawn_omlx_in_state_dir(
                        &c,
                        &state_isolation::fresh_state_dir(),
                    )
                    .unwrap(),
                )
                .into(),
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
        "breaker_sequence" => {
            // Upstream always 401; auth rejections must never trip the breaker.
            let upstream = auth_backend().await;
            let app = build_app(AppState {
                cards: vec![card("omlx", &upstream.uri(), LoadMode::Resident)],
                ..AppState::default()
            });
            let order = [false, false, false, false, true, false];
            for (i, stream) in order.into_iter().enumerate() {
                let (status, body) = body_of(
                    app.clone()
                        .oneshot(post("/v1/chat/completions", chat(stream)))
                        .await
                        .unwrap(),
                )
                .await;
                assert_mapped_auth_error(status, &body);
                assert!(!body.contains("circuit"), "request {i}: {body}");
                assert_eq!(
                    upstream.received_requests().await.unwrap().len(),
                    i + 1,
                    "request {i} must reach the upstream (breaker stayed closed)"
                );
            }
        }
        "oversized_401" => {
            // 401 head with a Content-Length above the body cap, then a dead body.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                for conn in listener.incoming() {
                    let mut conn = conn.unwrap();
                    let mut buf = [0u8; 4096];
                    let _ = conn.read(&mut buf);
                    let _ = conn.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
                          content-length: 8388609\r\n\r\n{\"error\":\"API key required\"",
                    );
                }
            });
            let app = build_app(AppState {
                cards: vec![card("omlx", &format!("http://{addr}"), LoadMode::Resident)],
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
        ("companion_key", Some(KEY)),
        ("companion_nokey", None),
        ("disguised_upstream", Some(KEY)),
        ("id_alone_is_not_enough", Some(KEY)),
        ("other_id_omlx_card_gets_key", Some(KEY)),
        ("on_demand", Some(KEY)),
        ("breaker_sequence", None),
        ("oversized_401", None),
    ] {
        let fake_home = std::env::temp_dir().join(format!(
            "idoris-resident-key-home-{}-{case}",
            std::process::id()
        ));
        std::fs::create_dir_all(&fake_home).unwrap();
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "resident_omlx_key_attachment_and_auth_error_mapping",
            "--nocapture",
        ])
        .env(CHILD, case)
        // §2.9：key 也可能来自 ~/.omlx/settings.json；子进程用空的临时家目录（on_demand 用例要往里写加载围栏），不受本机真实配置影响。
        .env("HOME", &fake_home)
        .env_remove(KEY_ENV);
        if let Some(key) = key {
            cmd.env(KEY_ENV, key);
        }
        let output = cmd.isolate_state().output().unwrap();
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
