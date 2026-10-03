#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::dispatch::BoundSupervisor;
use idoris_router::{AppState, build_app};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.provider.id = "Qwen3-0.6B-4bit".into();
    card.endpoint = endpoint.into();
    card
}

async fn app(server: &MockServer) -> axum::Router {
    let card = card(&server.uri());
    let supervisor = BoundSupervisor::spawn_omlx(&card).unwrap();
    build_app(AppState {
        cards: vec![card],
        supervisor: Some(supervisor),
        ..AppState::default()
    })
}

fn request(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("X-iDoris-Privacy", "local_only")
        .body(Body::from(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#
        )))
        .unwrap()
}

#[tokio::test]
async fn rejects_unselected_concrete_model_after_selection_without_backend_calls() {
    let upstream = MockServer::start().await;
    let response = app(&upstream)
        .await
        .oneshot(request("gpt-4o"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["X-iDoris-Served-Locality"], "loopback");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["type"], "unsupported_field");
    assert!(
        body["error"]["remediation"]
            .as_str()
            .unwrap()
            .contains("idoris/<role>")
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_present_but_empty_or_non_string_model_after_selection() {
    let upstream = MockServer::start().await;
    let app = app(&upstream).await;
    for model_value in ["null", "\"\"", "42"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("X-iDoris-Privacy", "local_only")
                    .body(Body::from(format!(
                        r#"{{"model":{model_value},"messages":[{{"role":"user","content":"hi"}}]}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{model_value}");
        assert_eq!(response.headers()["X-iDoris-Served-Locality"], "loopback");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

async fn mount_omlx_success(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 0
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/Qwen3-0.6B-4bit/load"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id":"Qwen3-0.6B-4bit", "loaded":true, "pinned":false}]
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message":{"content":"actual response"}}]
        })))
        .mount(server)
        .await;
}

async fn assert_served_model(model: &str) {
    let upstream = MockServer::start().await;
    mount_omlx_success(&upstream).await;
    let response = app(&upstream).await.oneshot(request(model)).await.unwrap();
    let status = response.status();
    let response_headers = response.headers().clone();
    assert_eq!(response_headers["X-iDoris-Served-Locality"], "loopback");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::OK, "{response_headers:?}: {body:?}");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["model"], "Qwen3-0.6B-4bit");
    assert_eq!(body["choices"][0]["message"]["content"], "actual response");
    let requests = upstream.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|r| r.url.path() == "/v1/chat/completions")
        .unwrap();
    assert_eq!(
        chat.body_json::<serde_json::Value>().unwrap()["model"],
        body["model"]
    );
}

#[tokio::test]
async fn role_alias_reports_actual_model_identity() {
    assert_served_model("idoris/daily").await;
}

#[tokio::test]
async fn uppercase_role_prefix_reports_actual_model_identity() {
    assert_served_model("IDORIS/daily").await;
}

#[tokio::test]
async fn mixed_case_role_prefix_reports_actual_model_identity() {
    assert_served_model("Idoris/fast").await;
}

#[tokio::test]
async fn whitespace_padded_role_alias_reports_actual_model_identity() {
    assert_served_model("  idoris/daily  ").await;
}

#[tokio::test]
async fn matching_concrete_model_is_accepted_and_reported() {
    assert_served_model("Qwen3-0.6B-4bit").await;
}
