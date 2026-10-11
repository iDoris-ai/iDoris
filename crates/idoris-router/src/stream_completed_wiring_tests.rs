#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use http_body_util::BodyExt;
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

#[tokio::test]
async fn streaming_proxy_records_completed_only_after_done_is_consumed() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw("data: token\n\ndata: [DONE]\n\n", "text/event-stream"),
        )
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
            r#"{"stream":true,"messages":[{"role":"user","content":"stream secret"}]}"#,
            &[("x-idoris-trace-id", "stream-completed-trace")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        events(&event_log, &response)
            .iter()
            .filter(|event| event.event.event_type == EventType::Completed)
            .count(),
        0
    );
    let record_id = response.headers().get(HEADER_RECORD_ID).unwrap().clone();
    let _ = response.into_body().collect().await.unwrap();
    let chain = event_log
        .events_for_record(
            Some(budget::PERSONAL_TENANT_ID),
            record_id.to_str().unwrap(),
        )
        .unwrap();
    let completed = chain
        .iter()
        .filter(|event| event.event.event_type == EventType::Completed)
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].event.metadata.get("status"),
        Some(&json!("success"))
    );
    assert_eq!(
        completed[0].event.metadata.get("provider_id"),
        Some(&json!("stream"))
    );
    assert_eq!(
        completed[0].event.trace_id.as_deref(),
        Some("stream-completed-trace")
    );
}

#[tokio::test]
async fn unterminated_stream_records_completed_failure() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw("data: token\n\n", "text/event-stream"),
        )
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
            r#"{"stream":true,"messages":[{"role":"user","content":"truncated"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    let record_id = response.headers().get(HEADER_RECORD_ID).unwrap().clone();
    assert!(response.into_body().collect().await.is_err());
    let chain = event_log
        .events_for_record(
            Some(budget::PERSONAL_TENANT_ID),
            record_id.to_str().unwrap(),
        )
        .unwrap();
    let completed = chain
        .iter()
        .filter(|event| event.event.event_type == EventType::Completed)
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].event.metadata.get("status"),
        Some(&json!("failure"))
    );
}
