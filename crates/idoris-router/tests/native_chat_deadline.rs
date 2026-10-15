//! Real HTTP + native router + Supervisor path with an eleven-second chat.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app, dispatch::BoundSupervisor};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn native_router_survives_an_eleven_second_chat() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 16, "pressure": "ok"
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
            "models": [{"id": "omlx", "loaded": true, "pinned": false,
                        "estimated_size": 1024_u64 * 1024 * 1024}]
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(11))
                .set_body_json(json!({"choices": [{"message": {"content": "late answer"}}]})),
        )
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
    let body =
        json!({"model":"idoris/daily","messages":[{"role":"user","content":"hi"}],"stream":false});
    let start = Instant::now();
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let posts = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/v1/chat/completions")
        .count();
    assert_eq!(
        status,
        StatusCode::OK,
        "elapsed={:?}; POSTs={posts}; response={}",
        start.elapsed(),
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(posts, 1);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["choices"][0]["message"]["content"], "late answer");
}
