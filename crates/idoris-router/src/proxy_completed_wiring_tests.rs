#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
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
        .into_iter()
        .filter(|row| row.event.event_type != EventType::AuditFinalized)
        .collect()
}

fn completed(events: &[EventLogEvent]) -> Vec<&EventLogEvent> {
    events
        .iter()
        .filter(|event| event.event.event_type == EventType::Completed)
        .collect()
}

#[tokio::test]
async fn buffered_proxy_records_completed_success_after_dispatch() {
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
            r#"{"messages":[{"role":"user","content":"proxy secret"}]}"#,
            &[("x-idoris-trace-id", "proxy-completed-trace")],
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
            EventType::Inspected,
            EventType::Profiled,
            EventType::Decided,
            EventType::Dispatched,
            EventType::Completed,
        ]
    );
    let event = &completed(&chain)[0].event;
    for key in ["tokens_in", "tokens_out", "token_estimator"] {
        assert!(!event.metadata.contains_key(key), "unexpected {key}");
    }
    assert_eq!(event.metadata.get("status"), Some(&json!("success")));
    assert_eq!(event.metadata.get("provider_id"), Some(&json!("proxy")));
    assert_eq!(event.trace_id.as_deref(), Some("proxy-completed-trace"));
    assert!(
        !serde_json::to_string(&event.metadata)
            .unwrap()
            .contains("proxy secret")
    );
}

#[tokio::test]
async fn buffered_proxy_failure_records_completed_failure() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
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
            r#"{"messages":[{"role":"user","content":"fail"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let chain = events(&event_log, &response);
    let completed = completed(&chain);
    assert_eq!(completed.len(), 1);
    for key in ["tokens_in", "tokens_out", "token_estimator"] {
        assert!(
            !completed[0].event.metadata.contains_key(key),
            "unexpected {key}"
        );
    }
    assert_eq!(
        completed[0].event.metadata.get("status"),
        Some(&json!("failure"))
    );
}

#[tokio::test]
async fn buffered_proxy_cache_replay_does_not_record_completed() {
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
    let headers = [(HEADER_REQUEST_ID, "proxy-completed-cache")];
    let first = app
        .clone()
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(completed(&events(&event_log, &first)).len(), 1);
    let cached = app
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &headers,
        ))
        .await
        .unwrap();
    assert_eq!(cached.status(), StatusCode::OK);
    assert_eq!(completed(&events(&event_log, &cached)).len(), 0);
}

#[tokio::test]
async fn paid_completed_append_failure_preserves_settled_response_with_and_without_request_id() {
    for request_id in [None, Some("paid-completed-failure")] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": "charged reply"}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 11, "total_tokens": 18}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("events.sqlite3");
        let event_log = Arc::new(EventLogStore::open(&db).unwrap());
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_completed BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='completed' BEGIN SELECT RAISE(ABORT,'failure'); END;",
            )
            .unwrap();
        let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
        let ledger = Arc::new(ledger);
        let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        let mut card = super::tests::resident_component_card("paid", &upstream.uri());
        card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(event_log.clone()),
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let headers = request_id
            .map(|id| vec![("X-iDoris-Request-Id", id)])
            .unwrap_or_default();
        let response = app
            .oneshot(super::tests::post_chat(
                r#"{"messages":[{"role":"user","content":"bill this"}]}"#,
                &headers,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(HEADER_COST_MINOR));
        let chain = events(&event_log, &response);
        assert_eq!(completed(&chain).len(), 0);
        assert_eq!(
            chain.last().unwrap().event.event_type,
            EventType::Dispatched
        );
        let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert!(after.spent_minor > before.spent_minor);
        assert_eq!(after.reserved_minor, before.reserved_minor);
        upstream.verify().await;
    }
}

#[tokio::test]
async fn chat_connect_failure_completes_the_attempt_without_charging() {
    for (paid, streaming) in [(false, false), (false, true), (true, false)] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mut card = super::tests::resident_component_card("proxy", &endpoint);
        if paid {
            card.provider.cost = super::tests::paid_component_card("proxy").provider.cost;
        }
        let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
        let ledger = Arc::new(ledger);
        let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        let log = memory_event_log();
        let app = build_app(AppState {
            cards: vec![card],
            event_log: Some(log.clone()),
            budget_ledger: Some(ledger.clone()),
            proxy: Arc::new(proxy::ChatProxy::with_config(
                idoris_upstream::http_client().unwrap(),
                std::time::Duration::from_secs(60),
                vec![],
            )),
            ..AppState::default()
        });
        let body = format!(
            r#"{{"stream":{streaming},"messages":[{{"role":"user","content":"attempt"}}]}}"#
        );
        drop(listener);
        let response = app
            .oneshot(super::tests::post_chat(&body, &[]))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{paid} {streaming}"
        );
        let chain = events(&log, &response);
        assert_eq!(
            chain.last().unwrap().event.event_type,
            EventType::Completed,
            "{paid} {streaming}: {chain:?}"
        );
        assert_eq!(completed(&chain).len(), 1);
        assert_eq!(completed(&chain)[0].event.metadata["status"], "failure");
        assert!(!response.headers().contains_key(HEADER_COST_MINOR));
        let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(after.spent_minor, before.spent_minor);
        assert_eq!(after.reserved_minor, before.reserved_minor);
    }
}

#[tokio::test]
async fn chat_open_breaker_without_dispatch_never_records_completed() {
    for (paid, streaming) in [(false, false), (false, true), (true, false)] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "60"))
            .expect(1)
            .mount(&upstream)
            .await;
        let proxy = proxy::ChatProxy::new(idoris_upstream::http_client().unwrap());
        let body = serde_json::json!({"stream": streaming, "messages": [{"role":"user", "content":"attempt"}]});
        let warmup = proxy.forward_stream(&upstream.uri(), &body).await;
        drop(warmup);
        let mut card = super::tests::resident_component_card("proxy", &upstream.uri());
        if paid {
            card.provider.cost = super::tests::paid_component_card("proxy").provider.cost;
        }
        let parsed = profile::parse_profile(
            &HeaderMap::new(),
            None,
            idoris_contracts::DeployMode::Personal,
        )
        .unwrap();
        let selected = dispatch::select(&[card], &parsed, "attempt").unwrap();
        let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
        let state = AppState {
            proxy: Arc::new(proxy),
            budget_ledger: Some(Arc::new(ledger)),
            ..AppState::default()
        };
        let log = memory_event_log();
        let dispatched = DispatchedEventContext {
            event_log: log.clone(),
            tenant_id: budget::PERSONAL_TENANT_ID.into(),
            record_id: "preflight".into(),
            correlation: correlation::RequestCorrelation::default(),
        };
        let completed = CompletedEventContext {
            event_log: log.clone(),
            tenant_id: budget::PERSONAL_TENANT_ID.into(),
            record_id: "preflight".into(),
            correlation: correlation::RequestCorrelation::default(),
        };
        let contexts = ProxyEventContexts {
            dispatched: Some(&dispatched),
            completed: Some(&completed),
        };
        let response = if streaming {
            chat_via_proxy_stream(&state, &selected, &body, contexts).await
        } else {
            let mut reservation = dispatch::ReservationGuard::reserve(
                state.budget_ledger.as_deref(),
                None,
                &selected.card.provider.id,
                selected.estimated_cost_minor,
            )
            .unwrap();
            chat_via_proxy_buffered(
                &state,
                &selected,
                &HeaderMap::new(),
                &parsed,
                &body,
                "preflight",
                contexts,
                &mut reservation,
            )
            .await
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response.headers().contains_key(HEADER_COST_MINOR));
        assert!(
            log.events_for_record(Some(budget::PERSONAL_TENANT_ID), "preflight")
                .unwrap()
                .is_empty(),
            "paid={paid} streaming={streaming}"
        );
        upstream.verify().await;
    }
}
