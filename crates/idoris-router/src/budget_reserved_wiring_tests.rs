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

fn budget_event_count(events: &[EventLogEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.event.event_type == EventType::BudgetReserved)
        .count()
}

#[tokio::test]
async fn paid_local_records_one_budget_reservation_after_decision_before_execution() {
    let event_log = memory_event_log();
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let card = super::tests::paid_component_card("paid-local");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "paid-local".to_string(),
            memory_gb: 1.0,
        },
    ]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        budget_ledger: Some(ledger),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"budget event"}]}"#,
            &[
                ("x-idoris-session", "session-budget"),
                ("x-idoris-trace-id", "trace-budget"),
                ("x-idoris-parent-id", "parent-budget"),
            ],
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
            EventType::BudgetReserved,
            EventType::Dispatched,
            EventType::Completed,
            EventType::BudgetSettled,
        ]
    );
    assert_eq!(budget_event_count(&events), 1);
    let reserved = &events[3].event;
    assert_eq!(
        reserved.metadata.get("reserved_minor"),
        Some(&json!(budget::estimate_cost_minor(
            &card.provider.cost,
            "budget event"
        )))
    );
    assert_eq!(
        reserved.metadata.get("provider_id"),
        Some(&json!("paid-local"))
    );
    assert_eq!(reserved.metadata.len(), 2);
    assert_eq!(reserved.session_id.as_deref(), Some("session-budget"));
    assert_eq!(reserved.trace_id.as_deref(), Some("trace-budget"));
    assert_eq!(reserved.parent_id.as_deref(), Some("parent-budget"));
    assert!(
        !serde_json::to_string(&reserved.metadata)
            .unwrap()
            .contains("budget event")
    );
}

#[tokio::test]
async fn free_and_no_ledger_paths_do_not_emit_budget_reserved() {
    let event_log = memory_event_log();
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let free_card = super::tests::sample_component_card("free-local");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "free-local".to_string(),
            memory_gb: 1.0,
        },
    ]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let free_response = build_app(AppState {
        cards: vec![free_card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&free_card, supervisor).into(),
        budget_ledger: Some(Arc::new(ledger)),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"free"}]}"#,
        &[],
    ))
    .await
    .unwrap();
    assert_eq!(free_response.status(), StatusCode::OK);
    assert_eq!(
        budget_event_count(&events_for_response(&event_log, &free_response)),
        0
    );

    let no_ledger_log = memory_event_log();
    let no_ledger_response = build_app(AppState {
        cards: vec![super::tests::paid_component_card("paid-no-ledger")],
        event_log: Some(no_ledger_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"paid"}]}"#,
        &[],
    ))
    .await
    .unwrap();
    assert_eq!(
        no_ledger_response.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        budget_event_count(&events_for_response(&no_ledger_log, &no_ledger_response)),
        0
    );
}

#[tokio::test]
async fn budget_event_append_failure_releases_reservation_before_backend_execution() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("event-log.sqlite3");
    let event_log = Arc::new(EventLogStore::open(&db).unwrap());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_budget_reserved BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='budget.reserved' BEGIN SELECT RAISE(ABORT,'fail budget'); END;",
        )
        .unwrap();
    let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let card = super::tests::paid_component_card("blocked-local");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "blocked-local".to_string(),
            memory_gb: 1.0,
        },
    ]));
    let supervisor =
        idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
    let response = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"must not run"}]}"#,
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let events = events_for_response(&event_log, &response);
    assert_eq!(budget_event_count(&events), 0);
    assert_eq!(events.len(), 3);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.reserved_minor, before.reserved_minor);
    assert_eq!(after.available_minor, before.available_minor);
    assert_eq!(adapter.load_call_count("blocked-local"), 0);
}

#[tokio::test]
async fn paid_proxy_records_budget_reservation_before_dispatch() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-budget-event",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 11, "total_tokens": 18}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let mut card = super::tests::resident_component_card("paid-proxy", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid-proxy")
        .provider
        .cost;
    let response = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"proxy"}]}"#,
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
            EventType::BudgetReserved,
            EventType::Dispatched,
        ]
    );
    assert_eq!(budget_event_count(&events), 1);
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert!(after.spent_minor > before.spent_minor);
    assert_eq!(after.reserved_minor, before.reserved_minor);
    assert!(after.available_minor < before.available_minor);
    upstream.verify().await;
}
