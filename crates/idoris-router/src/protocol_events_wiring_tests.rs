#![allow(clippy::unwrap_used)]

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use idoris_contracts::common::Capability;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn cases() -> Vec<(&'static str, Capability, serde_json::Value)> {
    vec![
        (
            "/v1/embeddings",
            Capability::Embedding,
            json!({"model":"m","input":"hello"}),
        ),
        (
            "/v1/rerank",
            Capability::Rerank,
            json!({"model":"m","query":"q","documents":["d"]}),
        ),
        (
            "/v1/messages",
            Capability::Chat,
            json!({"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hello"}]}),
        ),
    ]
}

fn post(path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_with_request_id(path: &str, body: &serde_json::Value) -> Request<Body> {
    let mut request = post(path, body);
    request
        .headers_mut()
        .insert(HEADER_REQUEST_ID, "protocol-cache".parse().unwrap());
    request
}

fn terminal_events(log: &EventLogStore, response: &Response) -> Vec<EventType> {
    let record = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    log.events_for_record(Some(budget::PERSONAL_TENANT_ID), record)
        .unwrap()
        .into_iter()
        .map(|row| row.event.event_type)
        .filter(|kind| matches!(kind, EventType::Dispatched | EventType::Completed))
        .collect()
}

#[tokio::test]
async fn protocol_send_attempts_record_terminal_failure_even_when_connect_fails() {
    for (path, capability, body) in cases() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut card = tests::resident_component_card("protocol", &endpoint);
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
        let response = app.oneshot(post(path, &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{path}");
        assert_eq!(
            terminal_events(&log, &response),
            [EventType::Dispatched, EventType::Completed],
            "{path}"
        );
        let record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap();
        let chain = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), record)
            .unwrap();
        let completed = chain
            .iter()
            .find(|row| row.event.event_type == EventType::Completed)
            .unwrap();
        assert_eq!(
            completed.event.metadata.get("status"),
            Some(&json!("failure")),
            "{path}"
        );
    }
}

#[tokio::test]
async fn protocol_preflight_refusal_has_no_fictitious_dispatch_or_completion() {
    for (path, capability, body) in cases() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "60"))
            .expect(1)
            .mount(&upstream)
            .await;
        let mut card = tests::resident_component_card("protocol", &upstream.uri());
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
        let first = app.clone().oneshot(post(path, &body)).await.unwrap();
        assert_eq!(
            terminal_events(&log, &first),
            [EventType::Dispatched, EventType::Completed],
            "{path}"
        );
        let refused = app.oneshot(post(path, &body)).await.unwrap();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert!(terminal_events(&log, &refused).is_empty(), "{path}");
        upstream.verify().await;
    }
}

#[tokio::test]
async fn buffered_protocol_success_is_completed_before_cache_replay() {
    for (path, capability, body) in cases() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .expect(1)
            .mount(&upstream)
            .await;
        let mut card = tests::resident_component_card("protocol", &upstream.uri());
        card.provider.capabilities = vec![capability];
        let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(log.clone()),
            proxy: Arc::new(proxy::ChatProxy::new(
                idoris_upstream::http_client().unwrap(),
            )),
            ..AppState::default()
        });
        let first = app
            .clone()
            .oneshot(post_with_request_id(path, &body))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK, "{path}");
        assert_eq!(
            terminal_events(&log, &first),
            [EventType::Dispatched, EventType::Completed],
            "{path}"
        );
        let replay = app
            .oneshot(post_with_request_id(path, &body))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK, "{path}");
        assert_eq!(terminal_events(&log, &replay), [], "{path}");
        upstream.verify().await;
    }
}

#[tokio::test]
async fn buffered_protocol_completed_append_failure_does_not_publish_success() {
    for (path, capability, body) in cases() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .expect(1)
            .mount(&upstream)
            .await;
        let mut card = tests::resident_component_card("protocol", &upstream.uri());
        card.provider.capabilities = vec![capability];
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
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(log),
            proxy: Arc::new(proxy::ChatProxy::new(
                idoris_upstream::http_client().unwrap(),
            )),
            ..AppState::default()
        });
        let response = app
            .oneshot(post_with_request_id(path, &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        upstream.verify().await;
    }
}
