//! M5-14: the provider breaker on the resident proxy path, seen from a
//! chat client — an open provider receives nothing and no dispatch is
//! recorded for the refused request.
#![allow(clippy::unwrap_used)]

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::tests::{post_chat, resident_component_card};
use super::*;

#[tokio::test]
async fn failing_local_upstream_is_not_contacted_once_its_breaker_opens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .mount(&server)
        .await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![resident_component_card("flaky-local", &server.uri())],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

    let mut last_record = String::new();
    let mut statuses = Vec::new();
    let mut refusals = Vec::new();
    for _ in 0..3 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        statuses.push(response.status());
        last_record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        refusals.push(serde_json::from_slice::<serde_json::Value>(&body_bytes).ok());
    }
    assert_eq!(
        statuses,
        [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE
        ]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    // The all-open case must stay the retryable `provider_circuit_open` from the
    // proxy send gate. Marking every open provider `Blocked` during selection
    // turned it into `no_eligible_candidate`, which clients read as "no such
    // model" rather than "cooling down, retry" - a contract change.
    let Some(refusal) = refusals[2].as_ref() else {
        panic!("refusal body is not JSON");
    };
    assert_eq!(
        refusal["error"]["type"], "provider_circuit_open",
        "all-open chat must answer the retryable breaker reason, got {refusal}"
    );
    let refused = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &last_record)
        .unwrap();
    assert!(
        refused
            .iter()
            .all(|e| e.event.event_type != EventType::Dispatched),
        "a breaker refusal must not be recorded as a dispatch"
    );
}

#[tokio::test]
async fn selection_moves_to_a_healthy_candidate_once_a_breaker_opens() {
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .mount(&failing)
        .await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"marker": "healthy"})))
        .mount(&healthy)
        .await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![
            resident_component_card("a-failing", &failing.uri()),
            resident_component_card("b-healthy", &healthy.uri()),
        ],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

    let mut statuses = Vec::new();
    let mut last_record = String::new();
    for _ in 0..3 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        statuses.push(response.status());
        last_record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
    }
    assert_eq!(
        statuses,
        [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::OK
        ]
    );
    assert_eq!(failing.received_requests().await.unwrap().len(), 2);
    assert_eq!(healthy.received_requests().await.unwrap().len(), 1);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &last_record)
        .unwrap();
    let decided = events
        .iter()
        .find(|e| e.event.event_type == EventType::Decided)
        .unwrap();
    assert_eq!(decided.event.metadata["provider_id"], "b-healthy");
}
