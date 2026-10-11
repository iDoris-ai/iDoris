#![allow(clippy::unwrap_used)]

use std::{sync::Arc, time::Duration};

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

fn dispatched_count(events: &[EventLogEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.event.event_type == EventType::Dispatched)
        .count()
}

#[tokio::test]
async fn buffered_proxy_records_one_content_free_dispatch_attempt() {
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
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });

    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"dispatch secret"}]}"#,
            &[
                ("x-idoris-session", "session-dispatch"),
                ("x-idoris-trace-id", "trace-dispatch"),
                ("x-idoris-parent-id", "parent-dispatch"),
            ],
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
            EventType::Dispatched,
        ]
    );
    let dispatched = &chain[3].event;
    assert_eq!(dispatched.metadata.get("status"), Some(&json!("attempted")));
    assert_eq!(
        dispatched.metadata.get("provider_id"),
        Some(&json!("proxy"))
    );
    assert_eq!(
        dispatched.metadata.get("served_locality"),
        Some(&json!("loopback"))
    );
    assert_eq!(dispatched.metadata.len(), 3);
    assert_eq!(dispatched.session_id.as_deref(), Some("session-dispatch"));
    assert_eq!(dispatched.trace_id.as_deref(), Some("trace-dispatch"));
    assert_eq!(dispatched.parent_id.as_deref(), Some("parent-dispatch"));
    assert!(
        !serde_json::to_string(&dispatched.metadata)
            .unwrap()
            .contains("dispatch secret")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_proxy_records_one_dispatch_attempt() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "stream",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"stream":true,"messages":[{"role":"user","content":"stream"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let chain = events(&event_log, &response);
    assert_eq!(dispatched_count(&chain), 1);
    assert_eq!(
        chain.last().unwrap().event.event_type,
        EventType::Dispatched
    );
    let _ = response.into_body().collect().await.unwrap();
    upstream.verify().await;
}

#[tokio::test]
async fn dispatched_append_failure_is_503_with_zero_post_for_buffered_and_stream() {
    for stream in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&upstream)
            .await;
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("events.sqlite3");
        let event_log = Arc::new(EventLogStore::open(&db).unwrap());
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_dispatched BEFORE INSERT ON event_log_events \
                 WHEN NEW.event_type='dispatched' BEGIN SELECT RAISE(ABORT,'fail dispatched'); END;",
            )
            .unwrap();
        let app = build_app(AppState {
            cards: vec![super::tests::resident_component_card(
                "blocked",
                &upstream.uri(),
            )],
            event_log: Some(event_log.clone()),
            ..AppState::default()
        });
        let body = if stream {
            r#"{"stream":true,"messages":[{"role":"user","content":"blocked"}]}"#
        } else {
            r#"{"messages":[{"role":"user","content":"blocked"}]}"#
        };
        let response = app
            .oneshot(super::tests::post_chat(body, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let chain = events(&event_log, &response);
        assert_eq!(dispatched_count(&chain), 0);
        assert_eq!(chain.len(), 3);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
        upstream.verify().await;
    }
}

#[tokio::test]
async fn cache_and_request_id_conflict_do_not_record_dispatch_attempt() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "cache",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let headers = [(HEADER_REQUEST_ID, "dispatch-id")];
    let first = app
        .clone()
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(dispatched_count(&events(&event_log, &first)), 1);
    let cached = app
        .clone()
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::OK);
    assert_eq!(dispatched_count(&events(&event_log, &cached)), 0);
    let conflict = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"changed"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(dispatched_count(&events(&event_log, &conflict)), 0);
    upstream.verify().await;
}

#[tokio::test]
async fn singleflight_waiter_replay_does_not_record_dispatch_attempt() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(120))
                .set_body_json(json!({"leader": true})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let event_log = memory_event_log();
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "flight",
            &upstream.uri(),
        )],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let request = || {
        super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same flight"}]}"#,
            &[(HEADER_REQUEST_ID, "flight-id")],
        )
    };
    let leader = tokio::spawn(app.clone().oneshot(request()));
    for _ in 0..50 {
        if upstream.received_requests().await.unwrap().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let waiter = tokio::spawn(app.oneshot(request()));
    let first = leader.await.unwrap().unwrap();
    let second = waiter.await.unwrap().unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let total = dispatched_count(&events(&event_log, &first))
        + dispatched_count(&events(&event_log, &second));
    assert_eq!(total, 1);
    upstream.verify().await;
}

#[tokio::test]
async fn exhausted_connect_failure_still_records_router_dispatch_attempt() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let event_log = memory_event_log();
    let proxy = proxy::ChatProxy::with_config(
        idoris_upstream::http_client().unwrap(),
        Duration::from_secs(60),
        vec![Duration::ZERO, Duration::ZERO],
    );
    let app = build_app(AppState {
        cards: vec![super::tests::resident_component_card(
            "connect-fail",
            &format!("http://{addr}"),
        )],
        event_log: Some(event_log.clone()),
        proxy: Arc::new(proxy),
        ..AppState::default()
    });
    let response = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"attempt only"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let chain = events(&event_log, &response);
    assert_eq!(dispatched_count(&chain), 1);
    let event = chain.last().unwrap();
    assert_eq!(event.event.event_type, EventType::Dispatched);
    assert_eq!(
        event.event.metadata.get("status"),
        Some(&json!("attempted"))
    );
}

fn local_state(
    event_log: Arc<EventLogStore>,
    adapter: Arc<idoris_backend::MockAdapter>,
) -> AppState {
    let card = super::tests::sample_component_card("local-1");
    let supervisor =
        idoris_backend::Supervisor::spawn(adapter, idoris_backend::SupervisorConfig::default())
            .unwrap();
    AppState {
        cards: vec![card.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(&card, supervisor)).into(),
        event_log: Some(event_log),
        ..AppState::default()
    }
}

fn local_adapter() -> Arc<idoris_backend::MockAdapter> {
    Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "local-1".to_string(),
            memory_gb: 1.0,
        },
    ]))
}

#[tokio::test]
async fn local_supervisor_records_dispatch_after_successful_load() {
    let event_log = memory_event_log();
    let adapter = local_adapter();
    let response = build_app(local_state(event_log.clone(), adapter.clone()))
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"local secret"}]}"#,
            &[
                ("x-idoris-session", "local-session"),
                ("x-idoris-trace-id", "local-trace"),
                ("x-idoris-parent-id", "local-parent"),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(adapter.load_call_count("local-1"), 1);
    let chain = events(&event_log, &response);
    assert_eq!(dispatched_count(&chain), 1);
    let dispatched = chain
        .iter()
        .find(|event| event.event.event_type == EventType::Dispatched)
        .unwrap();
    assert_eq!(
        dispatched.event.metadata.get("status"),
        Some(&json!("attempted"))
    );
    assert_eq!(
        dispatched.event.metadata.get("provider_id"),
        Some(&json!("local-1"))
    );
    assert_eq!(
        dispatched.event.metadata.get("served_locality"),
        Some(&json!("loopback"))
    );
    assert_eq!(dispatched.event.metadata.len(), 3);
    assert_eq!(
        dispatched.event.session_id.as_deref(),
        Some("local-session")
    );
    assert_eq!(dispatched.event.trace_id.as_deref(), Some("local-trace"));
    assert_eq!(dispatched.event.parent_id.as_deref(), Some("local-parent"));
    assert!(
        !serde_json::to_string(&dispatched.event.metadata)
            .unwrap()
            .contains("local secret")
    );
}

#[tokio::test]
async fn local_load_failure_never_records_dispatch() {
    let event_log = memory_event_log();
    let adapter = local_adapter();
    adapter.set_load_script("local-1", vec![idoris_backend::mock::LoadOutcome::Fail]);
    let response = build_app(local_state(event_log.clone(), adapter))
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"load fail"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(dispatched_count(&events(&event_log, &response)), 0);
}

#[tokio::test]
async fn local_dispatched_append_failure_is_generic_503_after_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("events.sqlite3");
    let event_log = Arc::new(EventLogStore::open(&db).unwrap());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_local_dispatched BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='dispatched' BEGIN SELECT RAISE(ABORT,'fail dispatched'); END;",
        )
        .unwrap();
    let adapter = local_adapter();
    let response = build_app(local_state(event_log.clone(), adapter.clone()))
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"blocked local"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(adapter.load_call_count("local-1"), 1);
    let chain = events(&event_log, &response);
    assert_eq!(dispatched_count(&chain), 0);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
}
