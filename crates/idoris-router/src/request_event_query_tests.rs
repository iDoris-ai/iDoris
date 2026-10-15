#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, sync::Arc};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use idoris_backend::{MockAdapter, ModelInfo};
use idoris_tenancy::event_log::{EventLogStore, EventType, NewEvent};
use idoris_tenancy::store::TenantStore;
use rusqlite::Connection;
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

use super::*;

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

fn event(tenant: &str, record_id: &str, event_type: EventType, ts: i64) -> NewEvent {
    NewEvent {
        event_id: Uuid::new_v4().to_string(),
        tenant_id: tenant.to_string(),
        record_id: record_id.to_string(),
        event_type,
        ts_utc_ms: ts,
        request_id: None,
        session_id: Some("session-query".into()),
        trace_id: Some("trace-query".into()),
        parent_id: None,
        origin_record_id: None,
        metadata: BTreeMap::from([("reason".into(), json!("query-safe"))]),
    }
}

fn get(path: &str, tenant: Option<&str>) -> Request<Body> {
    let mut request = Request::builder().method("GET").uri(path);
    if let Some(tenant) = tenant {
        request = request.header("x-idoris-tenant", tenant);
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn request_query_returns_ordered_tenant_scoped_event_chain() {
    let store = memory_event_log();
    for item in [
        event("acme", "record-1", EventType::RequestReceived, 10),
        event("acme", "record-1", EventType::Profiled, 20),
        event("other", "record-1", EventType::Decided, 30),
    ] {
        store.append(Some(&item.tenant_id), &item).unwrap();
    }
    let response = build_app(AppState {
        event_log: Some(store),
        ..AppState::default()
    })
    .oneshot(get("/idoris/tenants/acme/requests/record-1", Some("acme")))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["tenant_id"], "acme");
    assert_eq!(json["record_id"], "record-1");
    assert_eq!(json["events"].as_array().unwrap().len(), 2);
    assert_eq!(json["events"][0]["event_type"], "request.received");
    assert_eq!(json["events"][1]["event_type"], "profiled");
    assert!(
        json["events"][0]["sequence"].as_i64().unwrap()
            < json["events"][1]["sequence"].as_i64().unwrap()
    );
    assert_eq!(json["events"][0]["session_id"], "session-query");
    assert_eq!(json["events"][0]["trace_id"], "trace-query");
    assert_eq!(json["events"][0]["metadata"]["reason"], "query-safe");
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert!(!rendered.contains("prompt"));
    assert!(!rendered.contains("messages"));
}

#[tokio::test]
async fn request_query_returns_the_full_chain_of_a_real_paid_local_chat() {
    // Production-shaped stores (ledger + legacy records + Event Log): the
    // record id returned by a real chat must read back the whole decision
    // chain over HTTP, without any prompt content.
    const SENTINEL: &str = "query-chain-sentinel-5d1f0c";
    let event_log = memory_event_log();
    let records = Arc::new(std::sync::Mutex::new(
        TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let card = super::tests::paid_component_card("paid-local");
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: "paid-local".to_string(),
        memory_gb: 1.0,
    }]));
    let supervisor = idoris_backend::Supervisor::spawn(adapter, Default::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&card, supervisor).into(),
        budget_ledger: Some(Arc::new(ledger)),
        record_store: Some(records),
        event_log: Some(event_log),
        ..AppState::default()
    });

    let chat = app
        .clone()
        .oneshot(super::tests::post_chat(
            &format!(
                r#"{{"model":"idoris/daily","messages":[{{"role":"user","content":"{SENTINEL}"}}]}}"#
            ),
            &[("x-idoris-trace-id", "query-chain-trace")],
        ))
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    let record_id = chat
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // Positive control: the sentinel really travelled through the backend.
    let chat_body = chat.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8(chat_body.to_vec())
            .unwrap()
            .contains(SENTINEL)
    );

    let tenant = budget::PERSONAL_TENANT_ID;
    let uri = format!("/idoris/tenants/{tenant}/requests/{record_id}");
    let response = app.clone().oneshot(get(&uri, Some(tenant))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["tenant_id"], tenant);
    assert_eq!(json["record_id"], record_id.as_str());
    let events = json["events"].as_array().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event["event_type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            EventType::RequestReceived,
            EventType::Profiled,
            EventType::Decided,
            EventType::BudgetReserved,
            EventType::Dispatched,
            EventType::Completed,
            EventType::BudgetSettled,
            EventType::AuditFinalized,
        ]
        .map(EventType::as_str)
    );
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0]["sequence"].as_i64().unwrap()
                < pair[1]["sequence"].as_i64().unwrap())
    );
    // Every request-path fact carries the caller's correlation; the audit
    // terminal fact is written by the audit finalizer without a trace id.
    assert!(
        events[..events.len() - 1]
            .iter()
            .all(|event| event["trace_id"] == "query-chain-trace")
    );
    assert_eq!(events[2]["metadata"]["provider_id"], "paid-local");
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert!(!rendered.contains(SENTINEL));

    // The same real record is invisible from any other tenant scope.
    let mismatched = app.clone().oneshot(get(&uri, Some("other"))).await.unwrap();
    assert_eq!(mismatched.status(), StatusCode::BAD_REQUEST);
    let other = app
        .oneshot(get(
            &format!("/idoris/tenants/other/requests/{record_id}"),
            Some("other"),
        ))
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn request_query_rejects_missing_mismatched_and_duplicate_scope() {
    let store = memory_event_log();
    let item = event("acme", "record-1", EventType::RequestReceived, 10);
    store.append(Some("acme"), &item).unwrap();
    let app = build_app(AppState {
        event_log: Some(store),
        ..AppState::default()
    });
    for request in [
        get("/idoris/tenants/acme/requests/record-1", None),
        get("/idoris/tenants/acme/requests/record-1", Some("other")),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let duplicate = Request::builder()
        .method("GET")
        .uri("/idoris/tenants/acme/requests/record-1")
        .header("x-idoris-tenant", "acme")
        .header("x-idoris-tenant", "other")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(duplicate).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn request_query_distinguishes_invalid_missing_and_unconfigured_store() {
    let store = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(store),
        ..AppState::default()
    });
    let missing = app
        .clone()
        .oneshot(get("/idoris/tenants/acme/requests/missing", Some("acme")))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let invalid_id = "x".repeat(129);
    let invalid = app
        .oneshot(get(
            &format!("/idoris/tenants/acme/requests/{invalid_id}"),
            Some("acme"),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

    let unavailable = build_app(AppState::default())
        .oneshot(get("/idoris/tenants/acme/requests/record-1", Some("acme")))
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = unavailable.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_QUERY_UNAVAILABLE");

    let unauthorized_unavailable = build_app(AppState::default())
        .oneshot(get("/idoris/tenants/acme/requests/record-1", None))
        .await
        .unwrap();
    assert_eq!(unauthorized_unavailable.status(), StatusCode::BAD_REQUEST);
}
