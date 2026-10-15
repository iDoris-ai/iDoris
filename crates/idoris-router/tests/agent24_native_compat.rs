#![allow(clippy::unwrap_used, clippy::expect_used)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app, dispatch::BoundSupervisor};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
async fn check(field: &str, value: Value) {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "loaded_models": [],
            "model_memory_used": 0,
            "model_memory_max": 16,
            "pressure": "ok"
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/omlx/load"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{
                "id": "omlx",
                "loaded": true,
                "pinned": false,
                "estimated_size": 1024_u64 * 1024 * 1024
            }]
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "omlx",
            "choices": [{"message": {"content": "buffered answer"}}]
        })))
        .mount(&upstream)
        .await;
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = upstream.uri();
    let app = build_app(AppState {
        runtimes: Some(BoundSupervisor::spawn_omlx(&card).unwrap()).into(),
        cards: vec![card],
        ..AppState::default()
    });
    let mut body =
        json!({"model":"idoris/daily","messages":[{"role":"user","content":"hi"}],"stream":false});
    body[field] = value.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("X-iDoris-Privacy", "local_only")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response = response.into_body().collect().await.unwrap().to_bytes();
    let requests = upstream.received_requests().await.unwrap();
    let posts: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .collect();
    assert_eq!(
        status,
        StatusCode::OK,
        "field={field}; POSTs={}; response={}",
        posts.len(),
        String::from_utf8_lossy(&response)
    );
    assert_eq!(posts.len(), 1);
    let forwarded: Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(forwarded[field], value);
}
#[tokio::test]
async fn max_tokens_reaches_native_backend() {
    check("max_tokens", json!(512)).await;
}
#[tokio::test]
async fn tools_reach_native_backend() {
    check(
        "tools",
        json!([{"type":"function","function":{"name":"probe","parameters":{"type":"object"}}}]),
    )
    .await;
}
#[tokio::test]
async fn response_format_reaches_native_backend() {
    check("response_format", json!({"type":"json_object"})).await;
}
#[tokio::test]
async fn thinking_reaches_native_backend() {
    check("chat_template_kwargs", json!({"enable_thinking":false})).await;
}

#[tokio::test]
async fn native_completion_survives_a_valid_eleven_second_generation() {
    use idoris_backend::{ChatMessage, ChatRequest};
    use std::time::{Duration, Instant};
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(11))
                .set_body_json(
                    json!({"model":"omlx","choices":[{"message":{"content":"late answer"}}]}),
                ),
        )
        .mount(&upstream)
        .await;
    let adapter = idoris_upstream::OmlxAdapter::new(idoris_upstream::OmlxAdapterConfig {
        base_url: upstream.uri(),
        ..Default::default()
    })
    .unwrap();
    let start = Instant::now();
    let result = adapter
        .chat(
            ChatRequest {
                native: None,
                model: "omlx".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert!(
        result.is_ok(),
        "actual_elapsed={:?}; result={result:?}",
        start.elapsed()
    );
    assert_eq!(result.unwrap().content, "late answer");
}
