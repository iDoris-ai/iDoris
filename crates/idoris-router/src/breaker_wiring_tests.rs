//! M5-14: the provider breaker on the resident proxy path, seen from a
//! chat client — an open provider receives nothing and no dispatch is
//! recorded for the refused request.
#![allow(clippy::unwrap_used)]

use axum::http::StatusCode;
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
            StatusCode::SERVICE_UNAVAILABLE
        ]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
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
