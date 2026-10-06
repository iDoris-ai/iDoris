#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_tenancy::event_log::{EventLogEvent, EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::matchers::method;

use super::*;

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

fn events_for_response(event_log: &EventLogStore, response: &Response) -> Vec<EventLogEvent> {
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
async fn selected_route_records_one_truthful_decided_event_before_proxy_execution() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "omlx",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"private prompt"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = events_for_response(&event_log, &response);
    assert_eq!(
        events
            .iter()
            .map(|event| event.event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::RequestReceived,
            EventType::Profiled,
            EventType::Decided,
        ]
    );
    let decided = &events[2].event;
    assert_eq!(
        decided.metadata.get("status"),
        Some(&serde_json::json!("selected"))
    );
    assert_eq!(
        decided.metadata.get("provider_id"),
        Some(&serde_json::json!("omlx"))
    );
    assert_eq!(
        decided.metadata.get("tier"),
        Some(&serde_json::json!("local"))
    );
    assert_eq!(
        decided.metadata.get("served_locality"),
        Some(&serde_json::json!("loopback"))
    );
    assert!(decided.metadata.contains_key("rule_id"));
    let rendered = serde_json::to_string(&decided.metadata).unwrap();
    assert!(!rendered.contains("private prompt"));
    upstream.verify().await;
}

#[tokio::test]
async fn empty_candidate_rejection_records_decided_before_returning() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"no backend"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let events = events_for_response(&event_log, &response);
    assert_eq!(events.len(), 3);
    assert_eq!(events[2].event.event_type, EventType::Decided);
    assert_eq!(
        events[2].event.metadata.get("status"),
        Some(&serde_json::json!("rejected"))
    );
    assert_eq!(
        events[2].event.metadata.get("reason"),
        Some(&serde_json::json!("local_only_unavailable"))
    );
    assert!(events[2].event.metadata.contains_key("rule_id"));
}

#[tokio::test]
async fn decided_append_failure_is_generic_503_before_budget_or_upstream() {
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
    let provider_id = "p".repeat(501);
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            &provider_id,
            &upstream.uri(),
        )],
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"must not leave"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let events = events_for_response(&event_log, &response);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.event_type, EventType::RequestReceived);
    assert_eq!(events[1].event.event_type, EventType::Profiled);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.available_minor, before.available_minor);
    upstream.verify().await;
}

#[tokio::test]
async fn supervisor_double_decide_path_emits_decided_only_once() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::sample_component_card("local-model")],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    let events = events_for_response(&event_log, &response);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type == EventType::Decided)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::RequestReceived,
            EventType::Profiled,
            EventType::Decided,
        ]
    );
}
