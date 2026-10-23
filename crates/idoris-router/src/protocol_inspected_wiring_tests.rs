//! M5-07 `INSPECTED` on `/v1/embeddings`, `/v1/rerank` and `/v1/messages`:
//! a declared-`any` body carrying an identifier never reaches a card that
//! is not trusted local.
#![allow(clippy::unwrap_used)]

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::{Capability, PrivacyClass};
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

const ID: &str = "11010519491231002X";

fn request(path: &str, body: &serde_json::Value, privacy: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-idoris-privacy", privacy)
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

/// Sends `body` to a fresh `any`-only card with `capability` and returns the
/// status, error type (if any) and how many requests the upstream received.
async fn send(
    path: &str,
    capability: Capability,
    body: &serde_json::Value,
) -> (StatusCode, serde_json::Value, usize) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    let mut card = super::tests::resident_component_card("any-only", &server.uri());
    card.provider.capabilities = vec![capability];
    card.provider.privacy_class = PrivacyClass::Any;
    card.privacy_class = PrivacyClass::Any;
    let response = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    })
    .oneshot(request(path, body, "any"))
    .await
    .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (
        status,
        json,
        server.received_requests().await.unwrap().len(),
    )
}

fn cases() -> [(
    &'static str,
    Capability,
    serde_json::Value,
    serde_json::Value,
); 3] {
    [
        (
            "/v1/embeddings",
            Capability::Embedding,
            json!({"model": "embed-model", "input": ["hello", "world"]}),
            json!({"model": "embed-model", "input": ["hello", format!("身份证 {ID}")]}),
        ),
        (
            "/v1/rerank",
            Capability::Rerank,
            json!({"model": "rerank-model", "query": "best", "documents": ["alpha", "beta"]}),
            json!({"model": "rerank-model", "query": "best", "documents": ["alpha", "电话 13812345678"]}),
        ),
        (
            "/v1/messages",
            Capability::Chat,
            json!({"model": "claude-local", "max_tokens": 16,
                   "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]}),
            json!({"model": "claude-local", "max_tokens": 16,
                   "system": [{"type": "text", "text": format!("用户证件 {ID}")}],
                   "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]}),
        ),
    ]
}

#[tokio::test]
async fn clean_any_bodies_still_reach_the_card() {
    for (path, capability, clean, _) in cases() {
        let (status, _, received) = send(path, capability, &clean).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(received, 1, "{path}");
    }
}

#[tokio::test]
async fn identifiers_pin_protocol_bodies_local_with_zero_egress() {
    for (path, capability, _, dirty) in cases() {
        let (status, error, received) = send(path, capability, &dirty).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {error}");
        assert_eq!(error["error"]["type"], "local_only_unavailable", "{path}");
        assert_eq!(received, 0, "remote egress must be zero for {path}");
    }
}

#[tokio::test]
async fn large_protocol_bodies_are_fully_inspected_before_routing() {
    for (path, capability, _, dirty) in cases() {
        let mut large = dirty;
        large["inspection_probe"] = json!(format!("{} {ID}", "x".repeat(32 * 1024)));
        let (status, error, received) = send(path, capability, &large).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {error}");
        assert_eq!(error["error"]["type"], "local_only_unavailable", "{path}");
        assert_eq!(received, 0, "remote egress must be zero for {path}");
    }
}
