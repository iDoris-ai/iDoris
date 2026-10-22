#![allow(clippy::unwrap_used)]

use std::{sync::Arc, time::Duration};

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
    terminal_events_for_tenant(log, budget::PERSONAL_TENANT_ID, response)
}

fn terminal_events_for_tenant(
    log: &EventLogStore,
    tenant: &str,
    response: &Response,
) -> Vec<EventType> {
    let record = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    log.events_for_record(Some(tenant), record)
        .unwrap()
        .into_iter()
        .map(|row| row.event.event_type)
        .filter(|kind| matches!(kind, EventType::Dispatched | EventType::Completed))
        .collect()
}

#[tokio::test]
async fn messages_stream_records_one_success_after_message_stop_in_tenant_scope() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let (app, key) = tenant_test_app(
        AppState {
            deploy_mode: idoris_contracts::DeployMode::Tenant,
            cards: vec![tests::resident_component_card("messages", &upstream.uri())],
            event_log: Some(log.clone()),
            ..AppState::default()
        },
        "tenant-a",
    );
    let mut request = post(
        "/v1/messages",
        &json!({"model":"m","max_tokens":8,"stream":true,"messages":[]}),
    );
    request
        .headers_mut()
        .insert("x-idoris-tenant", "tenant-a".parse().unwrap());
    let response = app
        .oneshot(tenant_test_request(request, &key))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let record = response.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        terminal_events_for_tenant(&log, "tenant-a", &response),
        [EventType::Dispatched]
    );
    let _ = response.into_body().collect().await.unwrap();
    let chain = log.events_for_record(Some("tenant-a"), &record).unwrap();
    assert_eq!(
        chain
            .iter()
            .filter(|row| row.event.event_type == EventType::Completed)
            .count(),
        1
    );
    assert!(chain.iter().all(|row| row.event.tenant_id == "tenant-a"));
    let completed = chain
        .iter()
        .find(|row| row.event.event_type == EventType::Completed)
        .unwrap();
    assert_eq!(
        completed.event.metadata.get("status"),
        Some(&json!("success"))
    );
    upstream.verify().await;
}

#[tokio::test]
async fn messages_stream_without_message_stop_records_one_failure() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "event: message_delta\ndata: {\"type\":\"message_delta\"}\n\n",
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![tests::resident_component_card("messages", &upstream.uri())],
        event_log: Some(log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(post(
            "/v1/messages",
            &json!({"model":"m","max_tokens":8,"stream":true,"messages":[]}),
        ))
        .await
        .unwrap();
    let record = response.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();
    assert!(response.into_body().collect().await.is_err());
    let chain = log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record)
        .unwrap();
    let completed = chain
        .iter()
        .filter(|row| row.event.event_type == EventType::Completed)
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].event.metadata.get("status"),
        Some(&json!("failure"))
    );
    upstream.verify().await;
}

#[tokio::test]
async fn messages_stream_buffered_and_connect_failures_complete_failure() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .expect(1)
        .mount(&upstream)
        .await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    for endpoint in [upstream.uri(), unavailable_endpoint] {
        let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
        let app = build_app(AppState {
            cards: vec![tests::resident_component_card("messages", &endpoint)],
            event_log: Some(log.clone()),
            ..AppState::default()
        });
        let response = app
            .oneshot(post(
                "/v1/messages",
                &json!({"model":"m","max_tokens":8,"stream":true,"messages":[]}),
            ))
            .await
            .unwrap();
        assert_eq!(
            terminal_events(&log, &response),
            [EventType::Dispatched, EventType::Completed]
        );
        let record = response.headers()[HEADER_RECORD_ID].to_str().unwrap();
        let chain = log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), record)
            .unwrap();
        let completed = chain
            .iter()
            .filter(|row| row.event.event_type == EventType::Completed)
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].event.metadata.get("status"),
            Some(&json!("failure"))
        );
    }
    upstream.verify().await;
}

#[tokio::test]
async fn messages_stream_breaker_preflight_has_no_dispatch_or_completion() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "60"))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut card = tests::resident_component_card("messages", &upstream.uri());
    card.provider.capabilities = vec![Capability::Chat];
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
    proxy.set_breakers_enabled(true);
    let app = build_app(AppState {
        cards: vec![card],
        event_log: Some(log.clone()),
        proxy: Arc::new(proxy),
        ..AppState::default()
    });
    let body = json!({"model":"m","max_tokens":8,"stream":true,"messages":[]});
    let dispatched = app
        .clone()
        .oneshot(post("/v1/messages", &body))
        .await
        .unwrap();
    assert_eq!(
        terminal_events(&log, &dispatched),
        [EventType::Dispatched, EventType::Completed]
    );
    let refused = app.oneshot(post("/v1/messages", &body)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(terminal_events(&log, &refused).is_empty());
    upstream.verify().await;
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
