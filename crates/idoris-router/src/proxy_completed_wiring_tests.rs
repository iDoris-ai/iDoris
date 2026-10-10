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

fn completed(events: &[EventLogEvent]) -> Vec<&EventLogEvent> {
    events
        .iter()
        .filter(|event| event.event.event_type == EventType::Completed)
        .collect()
}

#[tokio::test]
async fn buffered_proxy_records_completed_success_after_dispatch() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "proxy",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"proxy secret"}]}"#,
            &[("x-idoris-trace-id", "proxy-completed-trace")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let chain = events(&event_log, &response);
    assert_eq!(
        chain
            .iter()
            .map(|event| event.event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::RequestReceived,
            EventType::Profiled,
            EventType::Decided,
            EventType::Dispatched,
            EventType::Completed,
        ]
    );
    let event = &completed(&chain)[0].event;
    assert_eq!(event.metadata.get("status"), Some(&json!("success")));
    assert_eq!(event.metadata.get("provider_id"), Some(&json!("proxy")));
    assert_eq!(event.trace_id.as_deref(), Some("proxy-completed-trace"));
    assert!(
        !serde_json::to_string(&event.metadata)
            .unwrap()
            .contains("proxy secret")
    );
}

#[tokio::test]
async fn buffered_proxy_failure_records_completed_failure() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "proxy",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"fail"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let chain = events(&event_log, &response);
    let completed = completed(&chain);
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].event.metadata.get("status"),
        Some(&json!("failure"))
    );
}

#[tokio::test]
async fn buffered_proxy_cache_replay_does_not_record_completed() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "proxy",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let headers = [(HEADER_REQUEST_ID, "proxy-completed-cache")];
    let first = app
        .clone()
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(completed(&events(&event_log, &first)).len(), 1);
    let cached = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::OK);
    assert_eq!(completed(&events(&event_log, &cached)).len(), 0);
}

#[tokio::test]
async fn paid_completed_append_failure_preserves_settled_response_with_and_without_request_id() {
    for request_id in [None, Some("paid-completed-failure")] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": "charged reply"}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 11, "total_tokens": 18}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("events.sqlite3");
        let event_log = Arc::new(EventLogStore::open(&db).unwrap());
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_completed BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='completed' BEGIN SELECT RAISE(ABORT,'failure'); END;",
            )
            .unwrap();
        let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
        let ledger = Arc::new(ledger);
        let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        let mut card = super::tests::resident_component_card("paid", &upstream.uri());
        card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(event_log.clone()),
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let headers = request_id
            .map(|id| vec![("X-iDoris-Request-Id", id)])
            .unwrap_or_default();
        let response = app
            .oneshot(super::tests::post_chat(
                r#"{"messages":[{"role":"user","content":"bill this"}]}"#,
                &headers,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(HEADER_COST_MINOR));
        let chain = events(&event_log, &response);
        assert_eq!(completed(&chain).len(), 0);
        assert_eq!(
            chain.last().unwrap().event.event_type,
            EventType::Dispatched
        );
        let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert!(after.spent_minor > before.spent_minor);
        assert_eq!(after.reserved_minor, before.reserved_minor);
        upstream.verify().await;
    }
}
