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
                EventType::Decided,
                EventType::Dispatched,
                EventType::Completed,
            ],
            "{path}"
        );
        assert_eq!(events[5].event.metadata["status"], "success", "{path}");
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
        assert_eq!(chain["events"].as_array().unwrap().len(), 6, "{path}");
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

#[tokio::test]
async fn an_idempotent_replay_records_no_second_dispatch() {
    for (path, capability, body) in endpoints() {
        let (server, app, log) = app_with(capability, PrivacyClass::LocalOnly).await;
        let headers = [("x-idoris-request-id", "proto-replay-1")];
        let first = app
            .clone()
            .oneshot(post(path, &body, &headers))
            .await
            .unwrap();
        let second = app.oneshot(post(path, &body, &headers)).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "{path}");
        let replayed = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&second))
            .unwrap();
        assert!(
            replayed.iter().all(|e| !matches!(
                e.event.event_type,
                EventType::Dispatched | EventType::Completed
            )),
            "{path}: {:?}",
            replayed
                .iter()
                .map(|e| e.event.event_type)
                .collect::<Vec<_>>()
        );
        let original = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&first))
            .unwrap();
        assert_eq!(
            original.last().unwrap().event.event_type,
            EventType::Completed,
            "{path}"
        );
    }
}

async fn streamed_messages_chain(sse: &'static str) -> Vec<(EventType, Option<serde_json::Value>)> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .mount(&server)
        .await;
    let mut card = super::tests::resident_component_card("proto-card", &server.uri());
    card.provider.capabilities = vec![Capability::Chat];
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![card],
        event_log: Some(log.clone()),
        ..AppState::default()
    });
    let body = json!({"model": "m", "max_tokens": 8, "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]});
    let response = app.oneshot(post("/v1/messages", &body, &[])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record = record_id(&response);
    // Drain the stream; completion is recorded when it ends.
    let _ = response.into_body().collect().await;
    log.events_for_record(Some(budget::PERSONAL_TENANT_ID), &record)
        .unwrap()
        .into_iter()
        .map(|e| (e.event.event_type, e.event.metadata.get("status").cloned()))
        .collect()
}

#[tokio::test]
async fn streamed_messages_complete_when_the_stream_ends() {
    let chain = streamed_messages_chain(
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    )
    .await;
    let types: Vec<_> = chain.iter().map(|(t, _)| *t).collect();
    assert_eq!(
        types,
        [
            EventType::RequestReceived,
            EventType::Inspected,
            EventType::Profiled,
            EventType::Decided,
            EventType::Dispatched,
            EventType::Completed,
        ]
    );
    assert_eq!(chain[5].1, Some(json!("success")));
}

#[tokio::test]
async fn a_stream_without_message_stop_completes_as_failure() {
    let chain =
        streamed_messages_chain("event: message_start\ndata: {\"type\":\"message_start\"}\n\n")
            .await;
    let completed = chain
        .iter()
        .find(|(t, _)| *t == EventType::Completed)
        .unwrap();
    assert_eq!(completed.1, Some(json!("failure")));
}

#[tokio::test]
async fn failed_protocol_completion_never_becomes_a_cached_success() {
    for (path, capability, body) in endpoints() {
        for request_id in [None, Some("failed-protocol-completion")] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
                .expect(1)
                .mount(&server)
                .await;
            let dir = tempfile::TempDir::new().unwrap();
            let db = dir.path().join("events.sqlite3");
            let log = Arc::new(EventLogStore::open(&db).unwrap());
            Connection::open(&db)
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER reject_completed BEFORE INSERT ON event_log_events \
                 WHEN NEW.event_type='completed' BEGIN SELECT RAISE(ABORT,'failure'); END;",
                )
                .unwrap();
            let mut card = super::tests::resident_component_card("proto-card", &server.uri());
            card.provider.capabilities = vec![capability];
            let app = build_app(AppState {
                cards: vec![card],
                event_log: Some(log.clone()),
                ..AppState::default()
            });
            let headers = request_id
                .map(|id| vec![("x-idoris-request-id", id)])
                .unwrap_or_default();
            let first = app
                .clone()
                .oneshot(post(path, &body, &headers))
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            let chain = log
                .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&first))
                .unwrap();
            assert_eq!(
                chain.last().unwrap().event.event_type,
                EventType::Dispatched
            );
            assert_eq!(
                chain
                    .iter()
                    .filter(|e| e.event.event_type == EventType::Dispatched)
                    .count(),
                1
            );
            assert!(!first.headers().contains_key(HEADER_CACHED));
            if request_id.is_some() {
                let second = app.oneshot(post(path, &body, &headers)).await.unwrap();
                assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
                assert!(!second.headers().contains_key(HEADER_CACHED));
                let replay = log
                    .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&second))
                    .unwrap();
                assert!(replay.iter().all(|e| !matches!(
                    e.event.event_type,
                    EventType::Dispatched | EventType::Completed
                )));
            }
            server.verify().await;
            assert_eq!(server.received_requests().await.unwrap().len(), 1, "{path}");
        }
    }
}

fn attempted_protocol_cases() -> Vec<(&'static str, Capability, serde_json::Value)> {
    let mut cases = endpoints().to_vec();
    let (_, capability, mut body) = endpoints()[2].clone();
    body["stream"] = json!(true);
    cases.push(("/v1/messages", capability, body));
    cases
}

fn terminal_types(log: &EventLogStore, response: &Response) -> Vec<EventType> {
    log.events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(response))
        .unwrap()
        .into_iter()
        .map(|e| e.event.event_type)
        .filter(|t| matches!(t, EventType::Dispatched | EventType::Completed))
        .collect()
}

#[tokio::test]
async fn protocol_connect_failure_terminates_its_recorded_attempt() {
    for (path, capability, body) in attempted_protocol_cases() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut card = super::tests::resident_component_card("proto-card", &endpoint);
        card.provider.capabilities = vec![capability];
        let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(log.clone()),
            proxy: Arc::new(proxy::ChatProxy::with_config(
                idoris_upstream::http_client().unwrap(),
                Duration::from_secs(60),
                vec![],
            )),
            ..AppState::default()
        });
        drop(listener);
        let response = app.oneshot(post(path, &body, &[])).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{path}");
        assert_eq!(
            terminal_types(&log, &response),
            [EventType::Dispatched, EventType::Completed]
        );
        let chain = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id(&response))
            .unwrap();
        assert_eq!(chain.last().unwrap().event.metadata["status"], "failure");
    }
}

#[tokio::test]
async fn protocol_breaker_refusal_creates_no_fictitious_attempt_or_completion() {
    for (path, capability, body) in attempted_protocol_cases() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "60"))
            .expect(1)
            .mount(&server)
            .await;
        let mut card = super::tests::resident_component_card("proto-card", &server.uri());
        card.provider.capabilities = vec![capability];
        let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
        let proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
        proxy.set_breakers_enabled(true);
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(log.clone()),
            proxy: Arc::new(proxy),
            ..AppState::default()
        });
        let attempted = app.clone().oneshot(post(path, &body, &[])).await.unwrap();
        assert_eq!(attempted.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            terminal_types(&log, &attempted),
            [EventType::Dispatched, EventType::Completed]
        );
        let refused = app.oneshot(post(path, &body, &[])).await.unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(terminal_types(&log, &refused).is_empty(), "{path}");
        server.verify().await;
    }
}
