#![allow(clippy::unwrap_used)]

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::Capability;
use idoris_contracts::component_card::Egress;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn embedding_card(id: &str, endpoint: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::resident_component_card(id, endpoint);
    card.provider.capabilities = vec![Capability::Embedding];
    card
}

fn request(body: serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn embeddings_preserves_upstream_path_payload_and_response() {
    let server = MockServer::start().await;
    let payload = json!({
        "model": "embed-model",
        "input": ["hello", "world"],
        "encoding_format": "float"
    });
    let upstream_body = json!({
        "object": "list",
        "data": [{"object":"embedding","index":0,"embedding":[0.1,0.2]}]
    });
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .and(wiremock::matchers::body_json(payload.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let response = build_app(AppState {
        cards: vec![embedding_card("embed-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[HEADER_SERVED_LOCALITY],
        HeaderValue::from_static("loopback")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        upstream_body
    );
    server.verify().await;
}

#[tokio::test]
async fn embeddings_fails_closed_before_upstream_for_wrong_capability_and_untrusted_locality() {
    let server = MockServer::start().await;
    let payload = json!({"model":"embed-model","input":"secret"});

    let chat_only = super::tests::resident_component_card("chat-only", &server.uri());
    let response = build_app(AppState {
        cards: vec![chat_only],
        ..AppState::default()
    })
    .oneshot(request(payload.clone(), &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut untrusted = embedding_card("untrusted", &server.uri());
    untrusted.allowed_egress = vec![Egress::Internet];
    let response = build_app(AppState {
        cards: vec![untrusted],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn tenant_embeddings_requires_tenant_before_upstream() {
    let server = MockServer::start().await;
    let response = build_app(AppState {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        cards: vec![embedding_card("embed-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"embed-model","input":"tenant data"}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "tenant_missing");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn paid_embeddings_fail_closed_before_upstream() {
    let server = MockServer::start().await;
    let mut card = embedding_card("embed-paid", &server.uri());
    card.provider.cost.input_per_m = 1.0;
    let response = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"embed-model","input":"paid data"}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "paid_proxy_unavailable");
    assert!(server.received_requests().await.unwrap().is_empty());
}
