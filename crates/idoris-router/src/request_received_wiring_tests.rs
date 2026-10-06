#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_contracts::DeployMode;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::{Connection, TransactionBehavior};
use tower::ServiceExt;
use wiremock::matchers::method;

use super::*;

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

#[tokio::test]
async fn request_received_persists_server_record_and_validated_correlation() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "upstream", "object": "chat.completion", "choices": []
            })),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "event-test",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"secret prompt"}]}"#,
            &[
                ("x-idoris-session", " session-1 "),
                ("x-idoris-trace-id", "trace-1"),
                ("x-idoris-parent-id", "parent-1"),
                (HEADER_RECORD_ID, "caller-record"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(record_id, "caller-record");

    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[1].event.event_type, EventType::Profiled);
    assert_eq!(events[2].event.event_type, EventType::Decided);
    let event = &events[0].event;
    assert_eq!(event.tenant_id, budget::PERSONAL_TENANT_ID);
    assert_eq!(event.record_id, record_id);
    assert_eq!(event.event_type, EventType::RequestReceived);
    assert_eq!(event.session_id.as_deref(), Some(" session-1 "));
    assert_eq!(event.trace_id.as_deref(), Some("trace-1"));
    assert_eq!(event.parent_id.as_deref(), Some("parent-1"));
    assert!(event.request_id.is_none());
    assert!(event.origin_record_id.is_none());
    assert!(event.metadata.is_empty());
    assert_eq!(
        uuid::Uuid::parse_str(&event.event_id)
            .unwrap()
            .get_version_num(),
        4
    );
    upstream.verify().await;
}

#[tokio::test]
async fn tenant_mode_uses_parsed_tenant_not_untrusted_metadata() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        deploy_mode: DeployMode::Tenant,
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            "{}",
            &[
                ("x-idoris-tenant", "acme"),
                ("x-idoris-event-tenant", "evil"),
            ],
        ))
        .await
        .unwrap();
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    let events = event_log
        .events_for_record(Some("acme"), record_id)
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event.event_type, EventType::RequestReceived);
    assert_eq!(events[1].event.event_type, EventType::Profiled);
    assert_eq!(events[2].event.event_type, EventType::Decided);
    assert!(
        event_log
            .events_for_record(Some("evil"), record_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn each_accepted_http_request_gets_fresh_record_and_event_ids() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let first = app
        .clone()
        .oneshot(super::tests::post_chat("{}", &[]))
        .await
        .unwrap();
    let second = app
        .oneshot(super::tests::post_chat("{}", &[]))
        .await
        .unwrap();
    let record = |response: &Response| {
        response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };
    let first_record = record(&first);
    let second_record = record(&second);
    assert_ne!(first_record, second_record);
    let first_event = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &first_record)
        .unwrap();
    let second_event = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &second_record)
        .unwrap();
    assert_ne!(
        first_event[0].event.event_id,
        second_event[0].event.event_id
    );
}

#[tokio::test]
async fn event_log_busy_fails_closed_before_upstream_or_budget() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("busy.sqlite3");
    let store_conn = Connection::open(&db).unwrap();
    store_conn.busy_timeout(Duration::from_millis(10)).unwrap();
    let event_log = Arc::new(EventLogStore::new(store_conn).unwrap());
    let mut blocker = Connection::open(&db).unwrap();
    let tx = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "event_log_unavailable");
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
    assert_eq!(json["error"]["remediation"], "event log unavailable");
    drop(tx);
    upstream.verify().await;
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.available_minor, before.available_minor);
}

#[tokio::test]
async fn join_failure_is_collapsed_and_none_event_log_preserves_legacy_chat() {
    assert!(
        run_event_log_write(|| -> Result<(), idoris_tenancy::event_log::EventLogError> {
            panic!("test join failure")
        })
        .await
        .is_err()
    );

    let app = build_app(AppState::default());
    let response = app
        .oneshot(super::tests::post_chat("{}", &[]))
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "local_only_unavailable");
}
