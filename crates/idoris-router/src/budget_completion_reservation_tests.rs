#![allow(clippy::unwrap_used)]

use super::*;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn a_declared_completion_cap_that_exceeds_budget_is_denied_before_post() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"result"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":4096,"total_tokens":4097}
        })))
        .expect(0)
        .mount(&upstream)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(3000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"max_tokens":4096,"messages":[{"role":"user","content":"cap test"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "unaffordable declared completion reached the paid upstream"
    );
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "budget_exceeded");
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.reserved_minor, before.reserved_minor);
    upstream.verify().await;
}

#[tokio::test]
async fn small_completion_cap_preserves_payload_and_legacy_admission() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"answer"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":4,"total_tokens":5}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(3000);
    let ledger = Arc::new(ledger);
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"max_tokens":8,"messages":[{"role":"user","content":"small cap"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().unwrap()["max_tokens"],
        8
    );
    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(view.spent_minor, 9);
    assert_eq!(view.reserved_minor, 0);
    upstream.verify().await;
}
