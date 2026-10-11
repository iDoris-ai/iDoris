#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_backend::{MockAdapter, ModelInfo};
use idoris_tenancy::event_log::{EventLogEvent, EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;

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
async fn paid_local_records_budget_settled_after_completed() {
    let event_log = memory_event_log();
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let card = super::tests::paid_component_card("paid-local");
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: "paid-local".to_string(),
        memory_gb: 1.0,
    }]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let response = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        budget_ledger: Some(Arc::new(ledger)),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"settle me"}]}"#,
        &[("x-idoris-trace-id", "settled-trace")],
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
            EventType::BudgetReserved,
            EventType::Dispatched,
            EventType::Completed,
            EventType::BudgetSettled,
        ]
    );
    let settled = &chain.last().unwrap().event;
    assert_eq!(
        settled.metadata.get("provider_id"),
        Some(&json!("paid-local"))
    );
    assert!(settled.metadata.contains_key("settled_minor"));
    assert_eq!(settled.metadata.len(), 2);
    assert_eq!(settled.trace_id.as_deref(), Some("settled-trace"));
    assert!(
        !serde_json::to_string(&settled.metadata)
            .unwrap()
            .contains("settle me")
    );
}

#[tokio::test]
async fn free_local_never_records_budget_settled() {
    let event_log = memory_event_log();
    let card = super::tests::sample_component_card("free-local");
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: "free-local".to_string(),
        memory_gb: 1.0,
    }]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let response = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"free"}]}"#,
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        events(&event_log, &response)
            .iter()
            .all(|event| event.event.event_type != EventType::BudgetSettled)
    );
}

#[tokio::test]
async fn budget_settled_append_failure_keeps_already_settled_success_response() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("events.sqlite3");
    let event_log = Arc::new(EventLogStore::open(&db).unwrap());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_budget_settled BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='budget.settled' BEGIN SELECT RAISE(ABORT,'fail settled'); END;",
        )
        .unwrap();
    let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let card = super::tests::paid_component_card("paid-local");
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: "paid-local".to_string(),
        memory_gb: 1.0,
    }]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let response = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    })
    .oneshot(super::tests::post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"still return me"}]}"#,
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(HEADER_COST_MINOR).is_some());
    let chain = events(&event_log, &response);
    assert!(
        chain
            .iter()
            .all(|event| event.event.event_type != EventType::BudgetSettled)
    );
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert!(after.spent_minor > before.spent_minor);
    assert_eq!(after.reserved_minor, before.reserved_minor);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["choices"][0]["message"]["content"].is_string());
}
