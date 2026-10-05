#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use idoris_contracts::ComponentCard;
use idoris_router::dispatch::BoundSupervisor;
use idoris_router::{AppState, build_app};
use tower::ServiceExt;
use wiremock::MockServer;

fn card(id: &str, endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.provider.id = id.to_string();
    card.endpoint = endpoint.to_string();
    card
}

async fn mismatch_is_blocked(same_provider: bool, same_endpoint: bool) {
    let upstream = MockServer::start().await;
    let mut bound_card = card("a", &upstream.uri());
    if same_endpoint {
        bound_card.provider.locality = idoris_contracts::provider::Locality::Remote;
    }
    let selected = card(
        if same_provider { "a" } else { "b" },
        if same_endpoint || !same_provider {
            &bound_card.endpoint
        } else {
            "http://127.0.0.1:9"
        },
    );
    let supervisor = BoundSupervisor::spawn_omlx(&bound_card).unwrap();
    let app = build_app(AppState {
        cards: vec![selected],
        runtimes: Some(supervisor).into(),
        ..AppState::default()
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("X-iDoris-Privacy", "local_only")
                .body(Body::from(
                    r#"{"model":"idoris/daily","messages":[{"role":"user","content":"private"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "mismatched Supervisor contacted upstream"
    );
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn k03_provider_mismatch_is_503_with_zero_upstream_requests() {
    mismatch_is_blocked(false, false).await;
}

#[tokio::test]
async fn k03_endpoint_mismatch_is_503_with_zero_upstream_requests() {
    mismatch_is_blocked(true, false).await;
}

#[tokio::test]
async fn k03_locality_mismatch_is_503_with_zero_upstream_requests() {
    mismatch_is_blocked(true, true).await;
}
