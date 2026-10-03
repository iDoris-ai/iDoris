#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{body::Body, http};
use http_body_util::BodyExt;
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn secured_card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.provider.id = "Qwen3-0.6B-4bit".into();
    card.endpoint = endpoint.into();
    card
}

#[tokio::test]
async fn models_auth_failure_is_an_observable_502_error_envelope() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&upstream)
        .await;
    let state = AppState {
        cards: vec![secured_card(&upstream.uri())],
        ..AppState::default()
    };
    let response = build_app(state)
        .oneshot(
            http::Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers()["x-idoris-served-locality"], "loopback");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"]["reason_code"],
        "upstream_authentication_failed"
    );
}

// Set credentials only in a child process; never mutate concurrent test env.
#[tokio::test]
async fn model_listing_uses_the_adapter_environment_credential() {
    if std::env::var("IDORIS_MODELS_AUTH_CHILD").is_err() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "model_listing_uses_the_adapter_environment_credential",
                "--nocapture",
            ])
            .env("IDORIS_MODELS_AUTH_CHILD", "1")
            .env("IDORIS_OMLX_API_KEY", "test-secret")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&upstream)
        .await;
    Mock::given(header("authorization", "Bearer test-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"data":[{"id":"Qwen3-0.6B-4bit"}]})),
        )
        .with_priority(1)
        .expect(1)
        .mount(&upstream)
        .await;
    let state = AppState {
        cards: vec![secured_card(&upstream.uri())],
        ..AppState::default()
    };
    let response = build_app(state)
        .oneshot(
            http::Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"][0]["id"], "Qwen3-0.6B-4bit");
}
