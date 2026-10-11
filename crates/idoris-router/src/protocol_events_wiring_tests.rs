//! The Event Log chain on /v1/embeddings, /v1/rerank and /v1/messages:
//! any request's record id finds its decision chain, as for chat.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::{HeaderValue, Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

const ID: &str = "11010519491231002X";

fn endpoints() -> [(&'static str, Capability, serde_json::Value); 3] {
    [
        (
            "/v1/embeddings",
            Capability::Embedding,
            json!({"model": "m", "input": ["hello"]}),
        ),
        (
            "/v1/rerank",
            Capability::Rerank,
            json!({"model": "m", "query": "q", "documents": ["a"]}),
        ),
        (
            "/v1/messages",
            Capability::Chat,
            json!({"model": "m", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}),
        ),
    ]
}

async fn app_with(
    capability: Capability,
    privacy: PrivacyClass,
) -> (MockServer, Router, Arc<EventLogStore>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    let mut card = super::tests::resident_component_card("proto-card", &server.uri());
    card.provider.capabilities = vec![capability];
    card.provider.privacy_class = privacy;
    card.privacy_class = privacy;
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![card],
        event_log: Some(log.clone()),
        ..AppState::default()
    });
    (server, app, log)
}

fn post(path: &str, body: &serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn record_id(response: &Response) -> String {
    response.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn served_protocol_requests_leave_a_queryable_decision_chain() {
    for (path, capability, body) in endpoints() {
        let (_server, app, log) = app_with(capability, PrivacyClass::LocalOnly).await;
        let response = app
            .clone()
            .oneshot(post(path, &body, &[("x-idoris-trace-id", "proto-trace")]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let record = record_id(&response);
        let events = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record)
            .unwrap();
        let types: Vec<_> = events.iter().map(|e| e.event.event_type).collect();
        assert_eq!(
            types,
            [
                EventType::RequestReceived,
                EventType::Inspected,
                EventType::Profiled,
                EventType::Decided
            ],
            "{path}"
        );
        assert!(
            events
                .iter()
                .all(|e| e.event.trace_id.as_deref() == Some("proto-trace"))
        );
        assert_eq!(
            events[3].event.metadata["provider_id"], "proto-card",
            "{path}"
        );
        // The same chain over HTTP, by record id.
        let query = Request::builder()
            .method("GET")
            .uri(format!(
                "/idoris/tenants/{}/requests/{record}",
                budget::PERSONAL_TENANT_ID
            ))
            .header("x-idoris-tenant", budget::PERSONAL_TENANT_ID)
            .body(Body::empty())
            .unwrap();
        let queried = app.oneshot(query).await.unwrap();
        assert_eq!(queried.status(), StatusCode::OK);
        let bytes = queried.into_body().collect().await.unwrap().to_bytes();
        let chain: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(chain["events"].as_array().unwrap().len(), 4, "{path}");
    }
}

#[tokio::test]
async fn a_tightened_protocol_request_records_the_rejection() {
    for (path, capability, mut body) in endpoints() {
        let (server, app, log) = app_with(capability, PrivacyClass::Any).await;
        body["metadata"] = json!({"note": format!("身份证 {ID}")});
        let response = app
            .oneshot(post(path, &body, &[("x-idoris-privacy", "any")]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert!(server.received_requests().await.unwrap().is_empty());
        let events = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&response))
            .unwrap();
        assert_eq!(
            events[1].event.metadata["reason"], "privacy_tightened_by_content",
            "{path}"
        );
        let decided = &events[3].event;
        assert_eq!(decided.event_type, EventType::Decided);
        assert_eq!(decided.metadata["status"], "rejected", "{path}");
        assert_eq!(
            decided.metadata["reason"], "local_only_unavailable",
            "{path}"
        );
        let rendered =
            serde_json::to_string(&events.iter().map(|e| &e.event.metadata).collect::<Vec<_>>())
                .unwrap();
        assert!(!rendered.contains(ID), "{rendered}");
    }
}

#[tokio::test]
async fn malformed_correlation_is_rejected_before_anything_is_recorded() {
    for (path, capability, body) in endpoints() {
        let (server, app, log) = app_with(capability, PrivacyClass::LocalOnly).await;
        let mut request = post(path, &body, &[]);
        request.headers_mut().insert(
            "x-idoris-trace-id",
            HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        assert!(server.received_requests().await.unwrap().is_empty());
        let events = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&response))
            .unwrap();
        assert!(events.is_empty(), "{path}");
    }
}
