#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::common::Capability;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

const SECRET: &str = "11010519491231002X";
const PROVIDER: &str = "selected-local";
const MODEL: &str = "bound-local-model";

fn cases() -> [(&'static str, Capability, &'static str, serde_json::Value); 3] {
    [
        (
            "/v1/embeddings",
            Capability::Embedding,
            "embed",
            json!({"model":"idoris/embed", "input":format!("身份 {SECRET}")}),
        ),
        (
            "/v1/rerank",
            Capability::Rerank,
            "rerank",
            json!({"model":"idoris/rerank", "query":"match", "documents":[format!("电话 13812345678 {SECRET}")]}),
        ),
        (
            "/v1/messages",
            Capability::Chat,
            "daily",
            json!({"model":"idoris/daily", "max_tokens":8, "messages":[{"role":"user","content":format!("身份 {SECRET}")}]}),
        ),
    ]
}

async fn selected(
    path: &str,
    capability: Capability,
    role: &str,
    body: &serde_json::Value,
    privacy: &str,
) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut card = tests::resident_component_card(PROVIDER, &upstream.uri());
    card.provider.capabilities = vec![capability];
    card.extensions =
        Some(serde_json::from_value(json!({"idoris_role_models":{(role):MODEL}})).unwrap());
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-idoris-privacy", privacy)
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = build_app(AppState {
        cards: vec![card],
        event_log: Some(log.clone()),
        ..AppState::default()
    })
    .oneshot(request)
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path} ({privacy})");
    let record = response.headers()[HEADER_RECORD_ID].to_str().unwrap();
    let all_rows = log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), record)
        .unwrap();
    let rows = all_rows
        .iter()
        .filter(|row| row.event.event_type != EventType::AuditFinalized)
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter()
            .map(|row| row.event.event_type)
            .collect::<Vec<_>>(),
        [
            EventType::RequestReceived,
            EventType::Inspected,
            EventType::Profiled,
            EventType::Decided,
            EventType::Dispatched,
            EventType::Completed,
        ],
        "{path} ({privacy})"
    );
    assert_eq!(rows[1].event.event_type, EventType::Inspected, "{path}");
    assert_eq!(rows[1].event.metadata["privacy"], "local_only", "{path}");
    assert_eq!(rows[3].event.event_type, EventType::Decided, "{path}");
    assert_eq!(rows[3].event.metadata["status"], "selected", "{path}");
    assert_eq!(rows[3].event.metadata["provider_id"], PROVIDER, "{path}");
    let reason_codes = rows[3].event.metadata.get("reason_codes");
    let tightening_count = reason_codes.map(|codes| {
        codes
            .as_array()
            .expect("reason_codes metadata must be an array")
            .iter()
            .filter(|code| {
                code.as_str().expect("reason_codes entries must be strings")
                    == "privacy_tightened_by_content"
            })
            .count()
    });
    if privacy == "any" {
        assert_eq!(
            tightening_count.expect("selected Any request must include reason_codes"),
            1,
            "{path}"
        );
    } else {
        assert!(
            tightening_count.unwrap_or(0) == 0,
            "{path}: {reason_codes:?}"
        );
    }
    assert_eq!(rows[4].event.event_type, EventType::Dispatched, "{path}");
    assert_eq!(rows[4].event.metadata["provider_id"], PROVIDER, "{path}");
    assert_eq!(rows[5].event.event_type, EventType::Completed, "{path}");
    assert_eq!(rows[5].event.metadata["status"], "success", "{path}");
    assert!(
        !serde_json::to_string(
            &all_rows
                .iter()
                .map(|row| &row.event.metadata)
                .collect::<Vec<_>>()
        )
        .unwrap()
        .contains(SECRET),
        "{path}: raw input in event metadata"
    );
    let _ = response.into_body().collect().await.unwrap();
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{path}");
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        sent["model"], MODEL,
        "{path}: selected provider model binding"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn sensitive_any_protocol_requests_record_selected_local_tightening_once() {
    for (path, capability, role, body) in cases() {
        selected(path, capability, role, &body, "any").await;
    }
}

#[tokio::test]
async fn declared_local_only_protocol_requests_do_not_record_content_tightening() {
    for (path, capability, role, body) in cases() {
        selected(path, capability, role, &body, "local_only").await;
    }
}
