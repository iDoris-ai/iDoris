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

fn rerank_card(id: &str, endpoint: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::resident_component_card(id, endpoint);
    card.provider.capabilities = vec![Capability::Rerank];
    card
}

fn request(body: serde_json::Value, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/rerank")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn rerank_preserves_upstream_path_payload_and_response() {
    let server = MockServer::start().await;
    let payload = json!({
        "model": "rerank-model",
        "query": "best result",
        "documents": ["alpha", "beta"],
        "top_n": 1
    });
    let upstream_body = json!({"results":[{"index":1,"relevance_score":0.9}]});
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/rerank"))
        .and(wiremock::matchers::body_json(payload.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let response = build_app(AppState {
        cards: vec![rerank_card("rerank-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(payload, &[]))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[HEADER_SERVED_LOCALITY],
        HeaderValue::from_static("loopback")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        upstream_body
    );
    server.verify().await;
}

#[tokio::test]
async fn rerank_fails_closed_before_upstream_for_wrong_capability_and_untrusted_locality() {
    let server = MockServer::start().await;
    let payload = json!({"model":"rerank-model","query":"secret","documents":["a"]});

    let chat_only = super::tests::resident_component_card("chat-only", &server.uri());
    let response = build_app(AppState {
        cards: vec![chat_only],
        ..AppState::default()
    })
    .oneshot(request(payload.clone(), &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let mut untrusted = rerank_card("untrusted", &server.uri());
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
async fn tenant_rerank_requires_tenant_before_upstream() {
    let server = MockServer::start().await;
    let response = build_app(AppState {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        cards: vec![rerank_card("rerank-local", &server.uri())],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"rerank-model","query":"q","documents":["tenant data"]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "tenant_missing");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn paid_rerank_fails_closed_before_upstream() {
    let server = MockServer::start().await;
    let mut card = rerank_card("rerank-paid", &server.uri());
    card.provider.cost.input_per_m = 1.0;
    let response = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"rerank-model","query":"q","documents":["paid data"]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "paid_proxy_unavailable");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn free_rerank_respects_exhausted_spend_gate_all_before_upstream() {
    let server = MockServer::start().await;
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap();
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
        .unwrap();
    let response = build_app(AppState {
        cards: vec![rerank_card("rerank-local", &server.uri())],
        budget_ledger: Some(Arc::new(ledger)),
        ..AppState::default()
    })
    .oneshot(request(
        json!({"model":"rerank-model","query":"q","documents":["free but gated"]}),
        &[],
    ))
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "budget_exceeded");
    assert_eq!(json["error"]["reason_code"], "budget_exceeded");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cached_rerank_still_respects_exhausted_spend_gate_all() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::TempDir::new().unwrap();
    let ledger = Arc::new(BudgetLedger::open(dir.path().join("budget.sqlite3")).unwrap());
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 1, "UTC", SpendGate::All)
        .unwrap();
    let app = build_app(AppState {
        cards: vec![rerank_card("rerank-local", &server.uri())],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let payload = json!({"model":"rerank-model","query":"q","documents":["cache then gate"]});
    let headers = [(HEADER_REQUEST_ID, "rerank-gated-cache")];

    let first = app
        .clone()
        .oneshot(request(payload.clone(), &headers))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    ledger
        .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
        .unwrap();

    let second = app.oneshot(request(payload, &headers)).await.unwrap();
    assert_eq!(second.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = second.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["reason_code"], "budget_exceeded");
    server.verify().await;
}

#[tokio::test]
async fn rerank_audit_usage_and_cache_origin_follow_record_id_contract() {
    let server = MockServer::start().await;
    let store = Arc::new(std::sync::Mutex::new(
        TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/rerank"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{"index":0,"relevance_score":0.9}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let app = build_app(AppState {
        cards: vec![rerank_card("rerank-local", &server.uri())],
        record_store: Some(store.clone()),
        ..AppState::default()
    });
    let payload = json!({"model":"rerank-model","query":"q","documents":["cache me"]});
    let headers = [(HEADER_REQUEST_ID, "rerank-cache-1")];

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
    assert!(first.headers().get(HEADER_CACHED).is_none());

    let second = app.oneshot(request(payload, &headers)).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_record_id = second.headers()[HEADER_RECORD_ID]
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(second_record_id, first_record_id);
    assert_eq!(second.headers()[HEADER_CACHED], "true");
    assert_eq!(
        second.headers()[HEADER_ORIGIN_RECORD_ID].to_str().unwrap(),
        first_record_id
    );

    {
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
        assert_eq!(usage.len(), 1, "cache replay must not duplicate usage");
        assert_eq!(usage[0].record_id, first_record_id);
        assert_eq!(usage[0].payload["cost_minor"], json!(0));
    }
    server.verify().await;
}
