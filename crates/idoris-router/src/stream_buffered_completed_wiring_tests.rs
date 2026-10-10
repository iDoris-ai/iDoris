#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use idoris_tenancy::event_log::{EventLogEvent, EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

fn events(event_log: &EventLogStore, response: &Response) -> Vec<EventLogEvent> {
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), record_id)
        .unwrap()
}

fn completed_count(events: &[EventLogEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.event.event_type == EventType::Completed)
        .count()
}

#[tokio::test]
async fn streaming_non_2xx_records_completed_failure() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "stream",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"stream":true,"messages":[{"role":"user","content":"fail"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let chain = events(&event_log, &response);
    assert_eq!(completed_count(&chain), 1);
    assert_eq!(
        chain.last().unwrap().event.metadata.get("status"),
        Some(&json!("failure"))
    );
}

#[tokio::test]
async fn streaming_connect_failure_does_not_record_completed() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "stream",
            "http://127.0.0.1:1",
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"stream":true,"messages":[{"role":"user","content":"never sent"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(completed_count(&events(&event_log, &response)), 0);
}
