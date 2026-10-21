//! M5-06 `INSPECTED` on the chat path: content findings pin a declared-`any`
//! request to `local_only` before routing, so a card that is not trusted
//! local never receives a byte of it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use http_body_util::BodyExt;
use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use serde_json::json;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::tests::{post_chat, resident_component_card};
use super::{AppState, HEADER_RECORD_ID, budget, build_app};

const ID: &str = "11010519491231002X";

/// A resident upstream whose card only carries `any` traffic: it is not
/// trusted-local, so a `local_only` request can never be routed to it.
async fn any_only_upstream() -> (MockServer, AppState, Arc<EventLogStore>) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"marker": "egress"})))
        .mount(&upstream)
        .await;
    let mut card = resident_component_card("any-only", &upstream.uri());
    card.provider.privacy_class = PrivacyClass::Any;
    card.privacy_class = PrivacyClass::Any;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let state = AppState {
        cards: vec![card],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    };
    (upstream, state, event_log)
}

async fn chat(
    state: AppState,
    body: serde_json::Value,
) -> (axum::http::StatusCode, String, String) {
    let response = build_app(state)
        .oneshot(post_chat(&body.to_string(), &[("x-idoris-privacy", "any")]))
        .await
        .unwrap();
    let status = response.status();
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        record_id,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

#[tokio::test]
async fn clean_any_request_still_egresses() {
    let (upstream, state, event_log) = any_only_upstream().await;
    let (status, record_id, _) = chat(
        state,
        json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "今天天气不错"}]}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let inspected = &events[1].event;
    assert_eq!(inspected.event_type, EventType::Inspected);
    assert_eq!(inspected.metadata["privacy"], "any");
    assert_eq!(inspected.metadata["sensitivity"], "S0");
    assert_eq!(inspected.metadata["labels"], json!([]));
    assert!(!inspected.metadata.contains_key("reason"));
}

#[tokio::test]
async fn injected_identifiers_send_zero_bytes_to_a_non_local_card() {
    let bodies = [
        // plain string content
        json!({"model": "idoris/daily", "messages": [{"role": "user", "content": format!("我的身份证 {ID}")}]}),
        // array content parts, which the legacy message extractor skips
        json!({"model": "idoris/daily", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "回电 138-1234-5678"}
        ]}]}),
        // tool-call arguments
        json!({"model": "idoris/daily", "messages": [
            {"role": "user", "content": "查一下"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "lookup", "arguments": format!("{{\"card\":\"6222021234567894\"}}")}}]}
        ]}),
        // a JSON number, not a string
        json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "号码见 metadata"}],
               "metadata": {"phone": 13812345678u64}}),
    ];
    for body in bodies {
        let (upstream, state, event_log) = any_only_upstream().await;
        let (status, record_id, text) = chat(state, body.clone()).await;
        assert_eq!(
            status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "{body}"
        );
        assert!(text.contains("local_only_unavailable"), "{text}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "remote egress must be zero for {body}"
        );
        let events = event_log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
            .unwrap();
        let types: Vec<_> = events.iter().map(|e| e.event.event_type).collect();
        assert_eq!(
            types[..3],
            [
                EventType::RequestReceived,
                EventType::Inspected,
                EventType::Profiled
            ]
        );
        let inspected = &events[1].event;
        assert_eq!(inspected.metadata["privacy"], "local_only");
        assert_eq!(inspected.metadata["reason"], "privacy_tightened_by_content");
        assert_eq!(events[2].event.metadata["privacy"], "local_only");
        let rendered = serde_json::to_string(&inspected.metadata).unwrap();
        for secret in [ID, "13812345678", "138-1234-5678", "6222021234567894"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }
}

#[tokio::test]
async fn declared_local_only_is_never_loosened_and_carries_no_tightening_reason() {
    let (_upstream, state, event_log) = any_only_upstream().await;
    let response = build_app(state)
        .oneshot(post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#,
            &[("x-idoris-privacy", "local_only")],
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
    let inspected = &events[1].event;
    assert_eq!(inspected.event_type, EventType::Inspected);
    assert_eq!(inspected.metadata["privacy"], "local_only");
    assert!(!inspected.metadata.contains_key("reason"));
}

#[tokio::test]
async fn tightening_does_not_depend_on_an_event_log() {
    let (upstream, mut state, _) = any_only_upstream().await;
    state.event_log = None;
    let response = build_app(state)
        .oneshot(post_chat(
            &json!({"model": "idoris/daily", "messages": [{"role": "user", "content": format!("身份证 {ID}")}]})
                .to_string(),
            &[("x-idoris-privacy", "any")],
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn inspected_append_failure_is_503_before_profiling_or_egress() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("event-log.sqlite3");
    let event_log = Arc::new(EventLogStore::open(&db).unwrap());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_inspected BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='inspected' BEGIN SELECT RAISE(ABORT,'fail inspected'); END;",
        )
        .unwrap();
    let (upstream, mut state, _) = any_only_upstream().await;
    state.event_log = Some(event_log.clone());
    let response = build_app(state)
        .oneshot(post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello"}]}"#,
            &[("x-idoris-privacy", "any")],
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        error["error"]["reason_code"],
        "EVENT_LOG_APPEND_UNAVAILABLE"
    );
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| e.event.event_type)
            .collect::<Vec<_>>(),
        [EventType::RequestReceived, EventType::AuditFinalized]
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
