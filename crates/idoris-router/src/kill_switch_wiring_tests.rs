//! M5-08 remote kill switch: while engaged, no request reaches a card that
//! is not trusted local, whatever the caller declared, and flipping the
//! shared switch takes effect on the next request.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

async fn any_only(capability: Capability) -> (MockServer, AppState) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    let mut card = super::tests::resident_component_card("any-only", &server.uri());
    card.provider.capabilities = vec![capability];
    card.provider.privacy_class = PrivacyClass::Any;
    card.privacy_class = PrivacyClass::Any;
    let state = AppState {
        cards: vec![card],
        ..AppState::default()
    };
    (server, state)
}

fn post(path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-idoris-privacy", "any")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn clean_chat() -> serde_json::Value {
    json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hello"}]})
}

#[tokio::test]
async fn flipping_the_shared_switch_stops_the_next_chat_egress() {
    let (server, mut state) = any_only(Capability::Chat).await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    state.event_log = Some(event_log.clone());
    let switch = state.remote_kill_switch.clone();
    let app = build_app(state);

    let before = app
        .clone()
        .oneshot(post("/v1/chat/completions", clean_chat()))
        .await
        .unwrap();
    assert_eq!(before.status(), StatusCode::OK);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    switch.store(true, Ordering::SeqCst);
    let after = app
        .oneshot(post("/v1/chat/completions", clean_chat()))
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::SERVICE_UNAVAILABLE);
    let record_id = after
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let body = after.into_body().collect().await.unwrap().to_bytes();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "local_only_unavailable");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "no new egress"
    );

    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let inspected = &events[1].event;
    assert_eq!(inspected.event_type, EventType::Inspected);
    assert_eq!(inspected.metadata["privacy"], "local_only");
    assert_eq!(inspected.metadata["sensitivity"], "S0");
    assert_eq!(inspected.metadata["reason"], "remote_kill_switch");
}

#[tokio::test]
async fn engaged_switch_stops_protocol_endpoints() {
    for (path, capability, body) in [
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
    ] {
        let (server, state) = any_only(capability).await;
        state.remote_kill_switch.store(true, Ordering::SeqCst);
        let response = build_app(state).oneshot(post(path, body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn engaged_switch_turns_inspect_allow_into_block() {
    let state = AppState {
        remote_kill_switch: Arc::new(AtomicBool::new(true)),
        ..AppState::default()
    };
    let response = build_app(state)
        .oneshot(post("/v1/inspect", json!({"input": "今天天气不错"})))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let verdict: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(verdict["action"], "BLOCK");
    assert_eq!(verdict["findings"], json!([]));
}
