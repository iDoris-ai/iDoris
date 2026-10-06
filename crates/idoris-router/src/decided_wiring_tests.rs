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

fn response_record_id(response: &Response) -> String {
    response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn decided(events: &[EventLogEvent]) -> &idoris_tenancy::event_log::NewEvent {
    let decided = events
        .iter()
        .filter(|event| event.event.event_type == EventType::Decided)
        .collect::<Vec<_>>();
    assert_eq!(decided.len(), 1, "exactly one decided event per request");
    &decided[0].event
}

#[tokio::test]
async fn selected_proxy_records_one_content_free_decision() {
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
        routing_policy: serde_yaml::from_str(
            "routing_policy:\n  version: 1\n  rules:\n    - if: { privacy: local_only }\n      then: { tiers: [local], fail_closed: true }\n  default: { tiers: [remote], fail_closed: false }\n",
        )
        .unwrap(),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"secret proxy prompt"}]}"#,
            &[
                ("x-idoris-session", "session-7"),
                ("x-idoris-trace-id", "trace-7"),
                ("x-idoris-parent-id", "parent-7"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record_id = response_record_id(&response);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    assert_eq!(events.len(), 3);
    let event = decided(&events);
    assert_eq!(event.metadata.get("status"), Some(&json!("selected")));
    assert_eq!(event.metadata.get("provider_id"), Some(&json!("proxy")));
    assert_eq!(event.metadata.get("tier"), Some(&json!("local")));
    assert_eq!(
        event.metadata.get("served_locality"),
        Some(&json!("loopback"))
    );
    assert_eq!(event.metadata.get("rule_id"), Some(&json!("rule:0")));
    assert_eq!(event.metadata.len(), 5);
    assert_eq!(event.record_id, record_id);
    assert_eq!(event.tenant_id, budget::PERSONAL_TENANT_ID);
    assert_eq!(event.session_id.as_deref(), Some("session-7"));
    assert_eq!(event.trace_id.as_deref(), Some("trace-7"));
    assert_eq!(event.parent_id.as_deref(), Some("parent-7"));
    let rendered = serde_json::to_string(&event.metadata).unwrap();
    assert!(!rendered.contains("secret proxy prompt"));
    upstream.verify().await;
}

#[tokio::test]
async fn selected_local_supervisor_records_one_decision_before_execution() {
    let card = super::tests::sample_component_card("local-1");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "local-1".to_string(),
            memory_gb: 1.0,
        },
    ]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"local secret"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record_id = response_record_id(&response);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let event = decided(&events);
    assert_eq!(event.metadata.get("provider_id"), Some(&json!("local-1")));
    assert_eq!(event.metadata.get("status"), Some(&json!("selected")));
    assert_eq!(event.metadata.get("rule_id"), Some(&json!("default")));
    assert!(
        !serde_json::to_string(&event.metadata)
            .unwrap()
            .contains("local secret")
    );
}

#[tokio::test]
async fn no_candidate_and_selection_rejection_each_record_one_decision() {
    let cases = [
        (
            AppState {
                event_log: Some(memory_event_log()),
                ..AppState::default()
            },
            vec![],
            "local_only_unavailable",
        ),
        (
            AppState {
                cards: vec![super::tests::sample_component_card("chat-only")],
                event_log: Some(memory_event_log()),
                ..AppState::default()
            },
            vec![("x-idoris-capabilities", "vision")],
            "no_eligible_candidate",
        ),
    ];
    for (state, headers, expected_reason) in cases {
        let event_log = state.event_log.clone().unwrap();
        let response = build_app(state)
            .oneshot(super::tests::post_chat("{}", &headers))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let record_id = response_record_id(&response);
        let events = event_log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
            .unwrap();
        let event = decided(&events);
        assert_eq!(event.metadata.get("status"), Some(&json!("rejected")));
        assert_eq!(event.metadata.get("reason"), Some(&json!(expected_reason)));
        assert_eq!(event.metadata.get("rule_id"), Some(&json!("default")));
    }
}

#[tokio::test]
async fn decided_append_failure_is_503_before_budget_or_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("event-log.sqlite3");
    let event_log = Arc::new(EventLogStore::open(&db).unwrap());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_decided BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='decided' BEGIN SELECT RAISE(ABORT,'fail decided'); END;",
        )
        .unwrap();
    let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
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
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"never sent"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let record_id = response_record_id(&response);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event.event_type, EventType::RequestReceived);
    assert_eq!(events[1].event.event_type, EventType::Profiled);
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(after.spent_minor, before.spent_minor);
    assert_eq!(after.available_minor, before.available_minor);
    upstream.verify().await;
}
