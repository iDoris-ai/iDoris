#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::matchers::method;

use super::*;

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

#[tokio::test]
async fn profiled_follows_request_received_with_resolved_safe_metadata() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"fix this bug"}]}"#,
            &[
                ("x-idoris-session", "session-1"),
                ("x-idoris-trace-id", "trace-1"),
                ("x-idoris-parent-id", "parent-1"),
            ],
        ))
        .await
        .unwrap();
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.event_type, EventType::RequestReceived);
    assert_eq!(events[1].event.event_type, EventType::Profiled);
    assert!(events[0].sequence < events[1].sequence);
    assert_eq!(events[1].event.record_id, record_id);
    assert_eq!(events[1].event.tenant_id, budget::PERSONAL_TENANT_ID);
    assert_eq!(events[1].event.session_id.as_deref(), Some("session-1"));
    assert_eq!(events[1].event.trace_id.as_deref(), Some("trace-1"));
    assert_eq!(events[1].event.parent_id.as_deref(), Some("parent-1"));
    assert_eq!(
        events[1].event.metadata.get("intent"),
        Some(&json!("coding"))
    );
    assert_eq!(
        events[1].event.metadata.get("privacy"),
        Some(&json!("local_only"))
    );
    assert_eq!(events[1].event.metadata.len(), 2);
}

#[tokio::test]
async fn explicit_intent_is_profiled_without_inspecting_message_content() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"secret customer text"}]}"#,
            &[("x-idoris-intent", "email"), ("x-idoris-privacy", "any")],
        ))
        .await
        .unwrap();
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let profiled = &events[1].event;
    assert_eq!(profiled.metadata.get("intent"), Some(&json!("email")));
    assert_eq!(profiled.metadata.get("privacy"), Some(&json!("any")));
    let rendered = serde_json::to_string(&profiled.metadata).unwrap();
    assert!(!rendered.contains("secret customer text"));
}

#[tokio::test]
async fn profiled_append_failure_is_generic_503_before_budget_or_upstream() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let long_intent = "x".repeat(501);
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
            &[("x-idoris-intent", long_intent.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.event_type, EventType::RequestReceived);
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.available_minor, before.available_minor);
    upstream.verify().await;
}

#[tokio::test]
async fn none_event_log_preserves_legacy_resolved_profile_path() {
    let app = build_app(AppState::default());
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"fix this bug"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "local_only_unavailable");
}
