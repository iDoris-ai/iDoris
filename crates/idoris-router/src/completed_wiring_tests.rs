#![allow(clippy::unwrap_used)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
};
use idoris_contracts::LoadPolicy;
use idoris_tenancy::event_log::{EventLogEvent, EventLogStore, EventType};
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::*;

struct ObservedAdapter {
    inner: MockAdapter,
    chat_calls: AtomicUsize,
    fail_chat: bool,
}

impl ObservedAdapter {
    fn new(fail_chat: bool) -> Self {
        Self {
            inner: MockAdapter::new(vec![ModelInfo {
                id: "local-1".to_string(),
                memory_gb: 1.0,
            }]),
            chat_calls: AtomicUsize::new(0),
            fail_chat,
        }
    }

    fn fail_load(&self) {
        self.inner
            .set_load_script("local-1", vec![idoris_backend::mock::LoadOutcome::Fail]);
    }
}

#[async_trait::async_trait]
impl RuntimeAdapter for ObservedAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.inner.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.inner.list().await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
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
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_chat {
            return Err(BackendError::Upstream {
                message: "synthetic chat failure".to_string(),
            });
        }
        self.inner.chat(req, cancel).await
    }
}

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

fn local_state(event_log: Arc<EventLogStore>, adapter: Arc<ObservedAdapter>) -> AppState {
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

fn events(event_log: &EventLogStore, response: &Response) -> Vec<EventLogEvent> {
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    stage_events(
        event_log
            .events_for_record(Some(budget::PERSONAL_TENANT_ID), record_id)
            .unwrap(),
    )
}

fn completed(events: &[EventLogEvent]) -> Vec<&EventLogEvent> {
    events
        .iter()
        .filter(|event| event.event.event_type == EventType::Completed)
        .collect()
}

#[tokio::test]
async fn local_supervisor_records_one_content_free_completed_success() {
    let event_log = memory_event_log();
    let adapter = Arc::new(ObservedAdapter::new(false));
    let response = build_app(local_state(event_log.clone(), adapter.clone()))
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"completion secret"}]}"#,
            &[
                ("x-idoris-session", "completed-session"),
                ("x-idoris-trace-id", "completed-trace"),
                ("x-idoris-parent-id", "completed-parent"),
            ],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(adapter.chat_calls.load(Ordering::SeqCst), 1);
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
            EventType::Completed,
        ]
    );
    let completed = completed(&chain);
    assert_eq!(completed.len(), 1);
    let event = &completed[0].event;
    assert_eq!(event.metadata.get("status"), Some(&json!("success")));
    assert_eq!(event.metadata.get("provider_id"), Some(&json!("local-1")));
    assert_eq!(
        event.metadata.get("served_locality"),
        Some(&json!("loopback"))
    );
    assert_eq!(
        event.metadata.get("tokens_in"),
        Some(&json!(idoris_tenancy::budget::estimate_tokens(
            "completion secret",
            "unknown"
        )))
    );
    assert_eq!(
        event.metadata.get("tokens_out"),
        Some(&json!(idoris_tenancy::budget::estimate_tokens(
            "mock reply to: completion secret",
            "unknown"
        )))
    );
    assert_eq!(
        event.metadata.get("token_estimator"),
        Some(&json!("conservative"))
    );
    assert_eq!(event.metadata.len(), 6);
    assert_eq!(event.session_id.as_deref(), Some("completed-session"));
    assert_eq!(event.trace_id.as_deref(), Some("completed-trace"));
    assert_eq!(event.parent_id.as_deref(), Some("completed-parent"));
    assert!(
        !serde_json::to_string(&event.metadata)
            .unwrap()
            .contains("completion secret")
    );
}

#[tokio::test]
async fn backend_chat_error_records_completed_failure() {
    let event_log = memory_event_log();
    let adapter = Arc::new(ObservedAdapter::new(true));
    let response = build_app(local_state(event_log.clone(), adapter.clone()))
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"fail after dispatch"}]}"#,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(adapter.chat_calls.load(Ordering::SeqCst), 1);
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
async fn pre_chat_load_failure_never_records_completed() {
    let event_log = memory_event_log();
    let adapter = Arc::new(ObservedAdapter::new(false));
    adapter.fail_load();
    let response = build_app(local_state(event_log.clone(), adapter.clone()))
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"never chatted"}]}"#,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(adapter.chat_calls.load(Ordering::SeqCst), 0);
    assert_eq!(completed(&events(&event_log, &response)).len(), 0);
}

#[tokio::test]
async fn completed_append_failure_is_generic_503_after_chat() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("events.sqlite3");
    let event_log = Arc::new(EventLogStore::open(&db).unwrap());
    Connection::open(&db)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_completed BEFORE INSERT ON event_log_events \
             WHEN NEW.event_type='completed' BEGIN SELECT RAISE(ABORT,'fail completed'); END;",
        )
        .unwrap();
    let adapter = Arc::new(ObservedAdapter::new(false));
    let (_budget_dir, ledger) = super::tests::configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let before = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    let paid_card = super::tests::paid_component_card("local-1");
    let supervisor = idoris_backend::Supervisor::spawn(
        adapter.clone(),
        idoris_backend::SupervisorConfig::default(),
    )
    .unwrap();
    let state = AppState {
        cards: vec![paid_card.clone()],
        runtimes: dispatch::BoundSupervisor::new(&paid_card, supervisor).into(),
        event_log: Some(event_log.clone()),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    };
    let response = build_app(state)
        .oneshot(super::tests::post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"already executed"}]}"#,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(adapter.chat_calls.load(Ordering::SeqCst), 1);
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let after = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert!(after.spent_minor > before.spent_minor);
    let chain = events(&event_log, &response);
    assert_eq!(completed(&chain).len(), 0);
    assert_eq!(
        chain.last().unwrap().event.event_type,
        EventType::Dispatched
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["reason_code"], "EVENT_LOG_APPEND_UNAVAILABLE");
}

#[tokio::test]
async fn request_history_exposes_conservative_source_for_ascii_and_cjk() {
    for (prompt, tokens_in, tokens_out, rough_in, rough_out) in [
        ("completion secret", 6, 11, 5, 8),
        ("你好世界，这是一个测试", 33, 38, 3, 7),
    ] {
        let event_log = memory_event_log();
        let app = build_app(local_state(
            event_log,
            Arc::new(ObservedAdapter::new(false)),
        ));
        let response = app
            .clone()
            .oneshot(super::tests::post_chat(
                &json!({"model":"idoris/daily", "messages":[{"role":"user", "content":prompt}]})
                    .to_string(),
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response_json["usage"]["prompt_tokens"], rough_in);
        assert_eq!(response_json["usage"]["completion_tokens"], rough_out);

        let history = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!(
                        "/idoris/tenants/{}/requests/{record_id}",
                        budget::PERSONAL_TENANT_ID
                    ))
                    .header("x-idoris-tenant", budget::PERSONAL_TENANT_ID)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(history.status(), StatusCode::OK);
        let body = history.into_body().collect().await.unwrap().to_bytes();
        let history: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let completed = history["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event_type"] == "completed")
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        let metadata = &completed[0]["metadata"];
        assert_eq!(metadata["tokens_in"], tokens_in);
        assert_eq!(metadata["tokens_out"], tokens_out);
        assert_eq!(metadata["token_estimator"], "conservative");
        assert!(!metadata.to_string().contains(prompt));
    }
}
