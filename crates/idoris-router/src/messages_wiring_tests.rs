#![allow(clippy::unwrap_used)]

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::Capability;
use idoris_contracts::component_card::Egress;
use idoris_tenancy::budget::{BudgetLedger, SpendGate};
use idoris_tenancy::store::{RecordKind, TenantStore};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn messages_card(id: &str, endpoint: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::resident_component_card(id, endpoint);
    card.provider.capabilities = vec![Capability::Chat];
    card
}

fn request(body: serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn messages_preserves_native_path_payload_and_response() {
    let server = MockServer::start().await;
    let payload = json!({
        "model": "claude-local",
        "max_tokens": 128,
        "system": [{"type":"text","text":"be concise"}],
        "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}]
    });
    let upstream_body = json!({
        "id":"msg_1",
        "type":"message",
        "role":"assistant",
        "content":[{"type":"text","text":"hi"}],
        "usage":{"input_tokens":4,"output_tokens":1}
    });
    let mut forwarded_payload = payload.clone();
    forwarded_payload
        .as_object_mut()
        .unwrap()
        .insert("stream".to_string(), json!(false));
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .and(wiremock::matchers::body_json(forwarded_payload))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        upstream_body
    );
    server.verify().await;
}

#[tokio::test]
async fn messages_streaming_preserves_native_sse_and_requires_message_stop() {
    let server = MockServer::start().await;
    let body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"stream":true,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[axum::http::header::CONTENT_TYPE],
        HeaderValue::from_static("text/event-stream")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), body.as_bytes());
    server.verify().await;
}

#[tokio::test]
async fn messages_streaming_clean_eof_without_message_stop_is_body_error() {
    let server = MockServer::start().await;
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
        .mount(&server)
        .await;
    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"stream":true,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
    server.verify().await;
}

#[tokio::test]
async fn messages_non_boolean_stream_values_fail_before_upstream() {
    for stream in [json!("true"), json!(1)] {
        let server = MockServer::start().await;
        let response = build_app(AppState {
            cards: vec![messages_card("messages-local", &server.uri())],
            ..AppState::default()
        })
        .oneshot(request(
            json!({"model":"claude-local","max_tokens":32,"stream":stream,"messages":[]}),
            &[],
        ))
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["reason_code"], "unsupported_stream");
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn free_messages_respect_exhausted_spend_gate_all_before_upstream() {
    let server = MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
        .unwrap();
    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        budget_ledger: Some(Arc::new(ledger)),
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn free_messages_stream_respects_exhausted_spend_gate_all_before_upstream() {
    let server = MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
        .unwrap();
    let response = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        budget_ledger: Some(Arc::new(ledger)),
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"stream":true,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn messages_audit_usage_and_cache_origin_follow_record_id_contract() {
    let server = MockServer::start().await;
    let store = Arc::new(std::sync::Mutex::new(
        TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_1","type":"message","role":"assistant","content":[]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let app = build_app(AppState {
        cards: vec![messages_card("messages-local", &server.uri())],
        record_store: Some(store.clone()),
        ..AppState::default()
    });
    let payload = json!({"model":"claude-local","max_tokens":32,"messages":[]});
    let headers = [(HEADER_REQUEST_ID, "messages-cache-1")];

    let first = app
        .clone()
        .oneshot(request(payload.clone(), &headers))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_record_id = first.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();

    let second = app.oneshot(request(payload, &headers)).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_record_id = second.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(second.headers()[HEADER_CACHED], "true");
    assert_eq!(
        second.headers()[HEADER_ORIGIN_RECORD_ID].to_str().unwrap(),
        first_record_id
    );

    let guard = store.lock().unwrap();
    let audit = guard
        .list(Some(budget::PERSONAL_TENANT_ID), Some(RecordKind::Audit))
        .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(
        audit
            .iter()
            .find(|row| row.record_id == second_record_id)
            .unwrap()
            .origin_record_id
            .as_deref(),
        Some(first_record_id.as_str())
    );
    let usage = guard
        .list(Some(budget::PERSONAL_TENANT_ID), Some(RecordKind::Usage))
        .unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].record_id, first_record_id);
}

#[tokio::test]
async fn messages_fails_closed_for_wrong_capability_and_untrusted_locality() {
    let server = MockServer::start().await;
    let payload = json!({"model":"claude-local","max_tokens":32,"messages":[]});

    let mut embed_only = super::tests::resident_component_card("embed", &server.uri());
    embed_only.provider.capabilities = vec![Capability::Embedding];
    let response = build_app(AppState {
        cards: vec![embed_only],
        ..AppState::default()
    })
    .oneshot(request(payload.clone(), &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut untrusted = messages_card("untrusted", &server.uri());
    untrusted.allowed_egress = vec![Egress::Internet];
    let response = build_app(AppState {
        cards: vec![untrusted],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn paid_messages_fail_closed_before_upstream() {
    let server = MockServer::start().await;
    let mut card = messages_card("messages-paid", &server.uri());
    card.provider.cost.output_per_m = 1.0;
    let response = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"claude-local","max_tokens":32,"messages":[]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(server.received_requests().await.unwrap().is_empty());
}
