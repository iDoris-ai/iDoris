//! M5-14: the provider breaker on the resident proxy path, seen from a
//! chat client — an open provider receives nothing and no dispatch is
//! recorded for the refused request.
#![allow(clippy::unwrap_used)]

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, ModelInfo, RuntimeAdapter,
};
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::tests::{post_chat, resident_component_card};
use super::*;

#[tokio::test]
async fn failing_local_upstream_is_not_contacted_once_its_breaker_opens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .mount(&server)
        .await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![resident_component_card("flaky-local", &server.uri())],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

    let mut last_record = String::new();
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        statuses.push(response.status());
        last_record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        if statuses.last() == Some(&StatusCode::SERVICE_UNAVAILABLE) {
            let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(envelope["error"]["type"], "provider_circuit_open");
        }
    }
    assert_eq!(
        statuses,
        [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE
        ]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    let refused = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &last_record)
        .unwrap();
    assert!(
        refused
            .iter()
            .all(|e| e.event.event_type != EventType::Dispatched),
        "a breaker refusal must not be recorded as a dispatch"
    );
}

struct NativeWitnessAdapter {
    model_id: String,
    fail_chat: bool,
    inner: idoris_backend::MockAdapter,
    loads: AtomicUsize,
    chats: AtomicUsize,
}

impl NativeWitnessAdapter {
    fn new(model_id: &str, fail_chat: bool) -> Self {
        Self {
            model_id: model_id.to_string(),
            fail_chat,
            inner: idoris_backend::MockAdapter::new(vec![ModelInfo {
                id: model_id.to_string(),
                memory_gb: 1.0,
            }]),
            loads: AtomicUsize::new(0),
            chats: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl RuntimeAdapter for NativeWitnessAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.inner.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.inner.list().await
    }

    async fn load(
        &self,
        id: &str,
        policy: Option<&idoris_contracts::load_policy::LoadPolicy>,
    ) -> Result<(), BackendError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(id, policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.inner.unload(id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.inner.status().await
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.inner.probe_ready(id).await
    }

    async fn chat(
        &self,
        request: ChatRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.chats.fetch_add(1, Ordering::SeqCst);
        if self.fail_chat {
            return Err(BackendError::ModelUnavailable {
                model_id: self.model_id.clone(),
            });
        }
        self.inner.chat(request, cancel).await
    }
}

fn native_witness_card(provider_id: &str, model_id: &str) -> idoris_contracts::ComponentCard {
    let mut card = super::tests::sample_component_card(provider_id);
    card.endpoint = format!("https://decorative.invalid/{provider_id}");
    if let Some(extensions) = card.extensions.as_mut() {
        extensions["idoris_role_models"]["daily"] = serde_json::json!(model_id);
    }
    card
}

fn native_witness_binding(
    card: &idoris_contracts::ComponentCard,
    adapter: Arc<NativeWitnessAdapter>,
) -> dispatch::BoundSupervisor {
    let handle = idoris_backend::Supervisor::spawn(
        adapter,
        idoris_backend::SupervisorConfig {
            oom_retry_backoff: std::time::Duration::ZERO,
            ..Default::default()
        },
    )
    .unwrap();
    dispatch::BoundSupervisor::new(card, handle)
}

fn decided_event(
    event_log: &EventLogStore,
    response: &axum::response::Response,
) -> serde_json::Value {
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), record_id)
        .unwrap();
    let decided = events
        .iter()
        .find(|event| event.event.event_type == EventType::Decided)
        .unwrap();
    serde_json::Value::Object(decided.event.metadata.clone().into_iter().collect())
}

#[tokio::test]
async fn organic_resident_faults_fail_over_and_decided_names_the_selected_provider() {
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error":"boom"})))
        .mount(&failing)
        .await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices":[]})))
        .mount(&healthy)
        .await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![
            resident_component_card("a-flaky", &failing.uri()),
            resident_component_card("z-healthy", &healthy.uri()),
        ],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    for _ in 0..2 {
        assert_eq!(
            app.clone()
                .oneshot(post_chat(body, &[]))
                .await
                .unwrap()
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let response = app.oneshot(post_chat(body, &[])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(failing.received_requests().await.unwrap().len(), 2);
    assert_eq!(healthy.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        decided_event(&event_log, &response)["provider_id"],
        "z-healthy"
    );
}

#[tokio::test]
async fn organic_native_faults_fail_over_to_a_healthy_native_and_record_decided_provider() {
    let failing_card = native_witness_card("a-native-fault", "model-fault");
    let healthy_card = native_witness_card("z-native-healthy", "model-healthy");
    let failing_adapter = Arc::new(NativeWitnessAdapter::new("model-fault", true));
    let healthy_adapter = Arc::new(NativeWitnessAdapter::new("model-healthy", false));
    let runtimes = runtime::RuntimeRegistry::from_supervisors([
        native_witness_binding(&failing_card, failing_adapter.clone()),
        native_witness_binding(&healthy_card, healthy_adapter.clone()),
    ]);
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![failing_card, healthy_card],
        runtimes,
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    for _ in 0..2 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        assert_ne!(response.status(), StatusCode::OK);
    }
    let response = app.oneshot(post_chat(body, &[])).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(failing_adapter.chats.load(Ordering::SeqCst), 2);
    assert_eq!(healthy_adapter.chats.load(Ordering::SeqCst), 1);
    assert_eq!(
        decided_event(&event_log, &response)["provider_id"],
        "z-native-healthy"
    );
}

#[tokio::test]
async fn native_only_lockout_returns_the_decided_rejection_without_a_third_load() {
    let card = native_witness_card("native-only", "model-only");
    let adapter = Arc::new(NativeWitnessAdapter::new("model-only", true));
    let runtimes = runtime::RuntimeRegistry::from_supervisors([native_witness_binding(
        &card,
        adapter.clone(),
    )]);
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![card],
        runtimes,
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    for _ in 0..2 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        assert_ne!(response.status(), StatusCode::OK);
    }
    assert_eq!(adapter.chats.load(Ordering::SeqCst), 2);
    let calls_before_refusal = (
        adapter.loads.load(Ordering::SeqCst),
        adapter.chats.load(Ordering::SeqCst),
    );
    let response = app.oneshot(post_chat(body, &[])).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let decided = decided_event(&event_log, &response);
    assert_eq!(decided["status"], "rejected");
    assert_eq!(decided["reason"], "no_eligible_candidate");
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), record_id)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type == EventType::Decided)
            .count(),
        1
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["error"]["type"], "no_eligible_candidate");
    assert_eq!(envelope["error"]["type"], decided["reason"]);
    assert_eq!(
        (
            adapter.loads.load(Ordering::SeqCst),
            adapter.chats.load(Ordering::SeqCst)
        ),
        calls_before_refusal,
        "the rejected request must not reach a third native load or chat"
    );
}

#[tokio::test]
async fn embedding_only_native_does_not_run_during_cooling_resident_chat_fallback() {
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error":"boom"})))
        .mount(&failing)
        .await;
    let mut embedding = native_witness_card("embedding-native", "embedding-model");
    embedding.provider.capabilities = vec![idoris_contracts::common::Capability::Embedding];
    let adapter = Arc::new(NativeWitnessAdapter::new("embedding-model", false));
    let runtimes = runtime::RuntimeRegistry::from_supervisors([native_witness_binding(
        &embedding,
        adapter.clone(),
    )]);
    let app = build_app(AppState {
        cards: vec![
            resident_component_card("chat-resident", &failing.uri()),
            embedding,
        ],
        runtimes,
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    for _ in 0..2 {
        assert_eq!(
            app.clone()
                .oneshot(post_chat(body, &[]))
                .await
                .unwrap()
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let response = app.oneshot(post_chat(body, &[])).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["error"]["type"], "provider_circuit_open");
    assert_eq!(failing.received_requests().await.unwrap().len(), 2);
    assert_eq!(adapter.loads.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.chats.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn paid_open_refusal_releases_only_its_reservation_and_has_no_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error":"uncertain"})))
        .mount(&server)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let mut card = resident_component_card("paid-flaky", &server.uri());
    card.provider.cost = super::tests::paid_component_card("paid-flaky")
        .provider
        .cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    for _ in 0..2 {
        assert_eq!(
            app.clone()
                .oneshot(post_chat(r#"{"messages":[]}"#, &[]))
                .await
                .unwrap()
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert!(before.reserved_minor > 0);
    let response = app
        .oneshot(post_chat(r#"{"messages":[]}"#, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let record = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), record)
        .unwrap();
    assert!(
        events
            .iter()
            .all(|e| e.event.event_type != EventType::Dispatched)
    );
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(
        (after.spent_minor, after.reserved_minor),
        (before.spent_minor, before.reserved_minor)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
