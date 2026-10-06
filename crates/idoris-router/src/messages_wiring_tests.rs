#![allow(clippy::unwrap_used)]

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::Capability;
use idoris_contracts::component_card::Egress;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn messages_card(id: &str, endpoint: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::resident_component_card(id, endpoint);
    card.provider.capabilities = vec![Capability::Chat];
    card
}

fn request(body: serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn messages_preserves_native_path_payload_and_response() {
    let server = MockServer::start().await;
    let payload = json!({
        "model": "claude-local",
        "max_tokens": 128,
        "system": [{"type":"text","text":"be concise"}],
        "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}]
    });
    let upstream_body = json!({
        "id":"msg_1",
        "type":"message",
        "role":"assistant",
        "content":[{"type":"text","text":"hi"}],
        "usage":{"input_tokens":4,"output_tokens":1}
    });
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .and(wiremock::matchers::body_json(payload.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        upstream_body
    );
    server.verify().await;
}

#[tokio::test]
async fn messages_streaming_preserves_native_sse_and_requires_message_stop() {
    let server = MockServer::start().await;
    let body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"stream":true,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_TYPE],
        HeaderValue::from_static("text/event-stream")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), body.as_bytes());
    server.verify().await;
}

#[tokio::test]
async fn messages_streaming_clean_eof_without_message_stop_is_body_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "event: message_delta\ndata: {\"type\":\"message_delta\"}\n\n",
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"stream":true,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
    server.verify().await;
}

#[tokio::test]
async fn messages_fails_closed_for_wrong_capability_and_untrusted_locality() {
    let server = MockServer::start().await;
    let payload = json!({"model":"claude-local","max_tokens":32,"messages":[]});

    let mut embed_only = super::tests::resident_component_card("embed", &server.uri());
    embed_only.provider.capabilities = vec![Capability::Embedding];
    let response = build_app(AppState {
        cards: vec![embed_only],
        ..AppState::default()
    })
    .oneshot(request(payload.clone(), &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut untrusted = messages_card("untrusted", &server.uri());
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
async fn paid_messages_fail_closed_before_upstream() {
    let server = MockServer::start().await;
    let mut card = messages_card("messages-paid", &server.uri());
    card.provider.cost.output_per_m = 1.0;
    let response = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}
