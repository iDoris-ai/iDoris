#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
    Supervisor, SupervisorConfig,
};
use idoris_contracts::{
    ComponentCard,
    common::{Capability, FallbackPolicy, PrivacyClass, Tier},
    component_card::{Egress, Form},
    provider::{Cost, Family, Locality, ProviderDescriptor},
};
use idoris_router::{AppState, build_app};
use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Clock, Price, SpendGate};
use rusqlite::Connection;
use tokio::sync::Notify;
use tower::ServiceExt;

const TENANT: &str = "personal";
const PROVIDER: &str = "paid-card";
const PROVIDER_A: &str = "paid-a";
const PROVIDER_B: &str = "paid-b";
const COST: Cost = Cost {
    input_per_m: 1_000_000.0,
    output_per_m: 2_000_000.0,
};
const BODY: &str = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello from durable settlement"}]}"#;
const PROMPT: &str = "hello from durable settlement";

#[derive(Default)]
struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct GatedAdapter {
    mock: MockAdapter,
    entered: Arc<Notify>,
    continue_chat: Arc<Notify>,
    chat_calls: Option<Arc<AtomicUsize>>,
}

#[async_trait]
impl RuntimeAdapter for GatedAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }
    async fn load(
        &self,
        id: &str,
        policy: Option<&idoris_contracts::LoadPolicy>,
    ) -> Result<(), BackendError> {
        self.mock.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }
    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.mock.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.mock.probe_ready(id).await
    }
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        if let Some(calls) = &self.chat_calls {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        self.entered.notify_one();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(BackendError::cancelled()),
            _ = self.continue_chat.notified() => (),
        }
        self.mock.chat(req, cancel).await
    }
}

fn card() -> ComponentCard {
    ComponentCard {
        provider: ProviderDescriptor {
            id: PROVIDER.into(),
            family: Family::Local,
            tier: Tier::Local,
            capabilities: vec![Capability::Chat],
            privacy_class: PrivacyClass::LocalOnly,
            cost: COST,
            locality: Locality::Loopback,
            extensions: None,
        },
        form: Form::HttpService,
        endpoint: "mock://paid-card".into(),
        version_pin: "test".into(),
        privacy_class: PrivacyClass::LocalOnly,
        allowed_egress: vec![Egress::Loopback],
        fallback_policy: FallbackPolicy::FailClosed,
        fail_closed: true,
        load_policy: None,
        extensions: None,
    }
}

fn card_for(provider_id: &str, capability: Capability) -> ComponentCard {
    let mut component = card();
    component.provider.id = provider_id.into();
    component.provider.capabilities = vec![capability];
    component.endpoint = format!("mock://{provider_id}");
    component
}

struct LoadGatedAdapter {
    mock: MockAdapter,
    b_load_entered: Arc<Notify>,
    continue_b_load: Arc<Notify>,
    a_chat_entered: Arc<Notify>,
    continue_a_chat: Arc<Notify>,
    chat_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl RuntimeAdapter for LoadGatedAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }
    async fn load(
        &self,
        id: &str,
        policy: Option<&idoris_contracts::LoadPolicy>,
    ) -> Result<(), BackendError> {
        if id == PROVIDER_B {
            self.b_load_entered.notify_one();
            self.continue_b_load.notified().await;
        }
        self.mock.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }
    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.mock.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.mock.probe_ready(id).await
    }
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        if req.model == PROVIDER_A {
            self.a_chat_entered.notify_one();
            self.continue_a_chat.notified().await;
        }
        self.mock.chat(req, cancel).await
    }
}

fn scope() -> BudgetScope {
    BudgetScope::new(TENANT, "default", PROVIDER, PROVIDER)
}

fn build_ledger(path: &std::path::Path, clock: Arc<TestClock>) -> BudgetLedger {
    let ledger =
        BudgetLedger::open_with_busy_timeout(path, clock, 100, Duration::from_millis(80)).unwrap();
    ledger
        .configure_tenant(TENANT, 1_000_000, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger.configure(&scope(), 1_000_000, "UTC").unwrap();
    ledger
}

#[tokio::test]
async fn completed_busy_settlement_clears_live_exemption_before_waiting_dispatch() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock));
    for provider in [PROVIDER_A, PROVIDER_B] {
        ledger
            .configure(
                &BudgetScope::new(TENANT, "default", provider, provider),
                1_000_000,
                "UTC",
            )
            .unwrap();
    }

    let b_load_entered = Arc::new(Notify::new());
    let continue_b_load = Arc::new(Notify::new());
    let a_chat_entered = Arc::new(Notify::new());
    let continue_a_chat = Arc::new(Notify::new());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(LoadGatedAdapter {
        mock: MockAdapter::new(
            [PROVIDER_A, PROVIDER_B]
                .into_iter()
                .map(|id| ModelInfo {
                    id: id.into(),
                    memory_gb: 1.0,
                })
                .collect(),
        ),
        b_load_entered: b_load_entered.clone(),
        continue_b_load: continue_b_load.clone(),
        a_chat_entered: a_chat_entered.clone(),
        continue_a_chat: continue_a_chat.clone(),
        chat_calls: chat_calls.clone(),
    });
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![
            card_for(PROVIDER_A, Capability::Chat),
            card_for(PROVIDER_B, Capability::Coding),
        ],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });

    let a_request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(BODY))
        .unwrap();
    let a_task = tokio::spawn(app.clone().oneshot(a_request));
    tokio::time::timeout(Duration::from_secs(2), a_chat_entered.notified())
        .await
        .expect("A should enter upstream chat");

    let b_request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-idoris-capabilities", "coding")
        .body(Body::from(BODY))
        .unwrap();
    let b_task = tokio::spawn(app.clone().oneshot(b_request));
    tokio::time::timeout(Duration::from_secs(2), b_load_entered.notified())
        .await
        .expect("B should reserve and block in its provider load");
    let (b_reservation, b_status): (String, String) = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT id, status FROM reservations WHERE model_id=?1",
            [PROVIDER_B],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(b_status, "active");

    let mut sidecar =
        Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    sidecar.busy_timeout(Duration::from_millis(80)).unwrap();
    let tx = sidecar
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    continue_a_chat.notify_one();
    let a_response = tokio::time::timeout(Duration::from_secs(2), a_task)
        .await
        .expect("A should return after completed upstream chat")
        .unwrap()
        .unwrap();
    assert_eq!(a_response.status(), StatusCode::OK);
    assert!(!a_response.headers().contains_key("X-iDoris-Cost-Minor"));
    let a_bytes = a_response.into_body().collect().await.unwrap().to_bytes();
    let a_json: serde_json::Value = serde_json::from_slice(&a_bytes).unwrap();
    let completion = a_json["choices"][0]["message"]["content"].as_str().unwrap();
    let actual =
        idoris_router::budget::estimate_actual_cost_minor(&COST, PROMPT, completion, 1_000_000);
    assert!(actual > 0);

    let (a_reservation, a_actual): (String, Option<i64>) = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT id, actual_cost_minor FROM reservations WHERE model_id=?1",
            [PROVIDER_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        a_actual,
        Some(actual),
        "completed usage must fall back to a durable primary-ledger charge"
    );
    let intent_count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM settlement_intents WHERE reservation_id=?1",
            [&a_reservation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        intent_count, 1,
        "A's durable intent must remain for recovery"
    );
    drop(tx);
    drop(sidecar);

    // B's reservation predates A's settlement. Once the sidecar lock is gone,
    // recovery observes the primary fallback charge and admits B exactly once.
    continue_b_load.notify_one();
    let b_response = tokio::time::timeout(Duration::from_secs(2), b_task)
        .await
        .expect("B should complete after its load completes")
        .unwrap()
        .unwrap();
    assert_eq!(b_response.status(), StatusCode::OK);
    assert_eq!(
        chat_calls.load(Ordering::SeqCst),
        2,
        "both completed dispatches may reach adapter chat after primary fallback"
    );
    let b_status: String = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT status FROM reservations WHERE id=?1",
            [&b_reservation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(b_status, "settled");

    let a_scope = BudgetScope::new(TENANT, "default", PROVIDER_A, PROVIDER_A);
    let b_scope = BudgetScope::new(TENANT, "default", PROVIDER_B, PROVIDER_B);
    let b_actual: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT actual_cost_minor FROM reservations WHERE id=?1",
            [&b_reservation],
            |row| row.get(0),
        )
        .unwrap();
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.balance(&a_scope).unwrap(), 1_000_000 - actual);
    assert_eq!(ledger.balance(&b_scope).unwrap(), 1_000_000 - b_actual);
    ledger.retry_settlements().unwrap();
    assert_eq!(ledger.balance(&a_scope).unwrap(), 1_000_000 - actual);
}

#[tokio::test]
async fn successful_chat_leaves_durable_pending_charge_until_reopen_recovers_it_once() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock.clone()));

    let entered = Arc::new(Notify::new());
    let continue_chat = Arc::new(Notify::new());
    let adapter = Arc::new(GatedAdapter {
        mock: MockAdapter::new(vec![ModelInfo {
            id: PROVIDER.into(),
            memory_gb: 1.0,
        }]),
        entered: entered.clone(),
        continue_chat: continue_chat.clone(),
        chat_calls: None,
    });
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card()],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(BODY))
        .unwrap();
    let request_task = tokio::spawn(app.oneshot(request));

    // Reaching chat proves dispatch has committed its reservation first.
    if tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .is_err()
    {
        let response = tokio::time::timeout(Duration::from_secs(2), request_task)
            .await
            .expect("request should finish or reach adapter chat")
            .unwrap()
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        panic!(
            "request returned before entering adapter chat: {status} {}",
            String::from_utf8_lossy(&body)
        );
    }
    let mut primary = Connection::open(&db_path).unwrap();
    primary.busy_timeout(Duration::from_millis(80)).unwrap();
    let tx = primary
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let (reservation_id, tenant_id, reserved_minor, status): (String, String, i64, String) = tx.query_row(
        "SELECT id, tenant_id, reserved_minor, status FROM reservations WHERE tenant_id=?1 AND status='active'",
        [TENANT], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(tenant_id, TENANT);
    assert!(reserved_minor > 0);
    assert_eq!(status, "active");

    // Keep the primary writer lock while the adapter completes successfully.
    continue_chat.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(2), request_task)
        .await
        .expect("completed chat should return a response")
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("X-iDoris-Cost-Minor"));
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let completion = json["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(completion, format!("mock reply to: {PROMPT}"));

    let actual = idoris_router::budget::estimate_actual_cost_minor(
        &COST,
        PROMPT,
        completion,
        reserved_minor,
    );
    assert!(actual > 0);
    // Prove the held transaction excludes another primary writer with a
    // bounded, deterministic SQLite busy timeout.
    let mut competing_writer = Connection::open(&db_path).unwrap();
    competing_writer
        .busy_timeout(Duration::from_millis(20))
        .unwrap();
    let busy = competing_writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate);
    match &busy {
        Err(rusqlite::Error::SqliteFailure(error, _)) => {
            assert_eq!(error.code, rusqlite::ErrorCode::DatabaseBusy);
        }
        other => panic!("expected SQLITE_BUSY from competing writer, got {other:?}"),
    }
    drop(busy);
    drop(competing_writer);

    let sidecar = Connection::open_with_flags(
        db_path.with_added_extension("settlements.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let pending: (String, String, i64) = sidecar
        .query_row(
            "SELECT reservation_id, tenant_id, actual_cost_minor FROM pending_settlements",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(pending, (reservation_id.clone(), TENANT.into(), actual));
    let still_active: (String, i64) = tx
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&reservation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(still_active, ("active".into(), 0));
    drop(sidecar);
    drop(tx); // release the writer lock
    drop(primary);
    assert_eq!(
        Arc::strong_count(&ledger),
        1,
        "app must release its ledger handle"
    );
    drop(ledger);

    // Reopen well after reservation expiry: successful completed usage must
    // still be recovered and charged once from the durable sidecar.
    clock.0.store(10_000, Ordering::SeqCst);
    let recovered = build_ledger(&db_path, clock.clone());
    recovered.retry_settlements().unwrap();
    assert_eq!(
        recovered.tenant_balance(TENANT).unwrap(),
        1_000_000 - actual
    );
    assert_eq!(recovered.balance(&scope()).unwrap(), 1_000_000 - actual);
    let primary_check = Connection::open(&db_path).unwrap();
    let row: (String, i64) = primary_check
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations WHERE id=?1",
            [&reservation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("settled".into(), actual));
    let sidecar_check =
        Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    let pending_count: i64 = sidecar_check
        .query_row("SELECT COUNT(*) FROM pending_settlements", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pending_count, 0);
    drop(recovered);

    // Further retries/reopens are idempotent and do not charge again.
    let reopened = build_ledger(&db_path, clock);
    assert_eq!(reopened.tenant_balance(TENANT).unwrap(), 1_000_000 - actual);
    assert_eq!(reopened.balance(&scope()).unwrap(), 1_000_000 - actual);
    let sidecar_check =
        Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    let count: i64 = sidecar_check
        .query_row("SELECT COUNT(*) FROM pending_settlements", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn sidecar_insert_failure_falls_back_to_primary_charge_and_survives_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock.clone()));

    let sidecar = Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    sidecar
        .execute_batch(
            "CREATE TRIGGER fail_pending_insert BEFORE INSERT ON pending_settlements
         BEGIN SELECT RAISE(FAIL, 'injected sidecar failure'); END;",
        )
        .unwrap();
    drop(sidecar);

    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: PROVIDER.into(),
        memory_gb: 1.0,
    }]));
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card()],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(BODY))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let completion = json["choices"][0]["message"]["content"].as_str().unwrap();
    let primary = Connection::open(&db_path).unwrap();
    let reserved_minor: i64 = primary
        .query_row(
            "SELECT reserved_minor FROM reservations ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let actual = idoris_router::budget::estimate_actual_cost_minor(
        &COST,
        PROMPT,
        completion,
        reserved_minor,
    );
    assert!(actual > 0);
    assert_eq!(ledger.tenant_balance(TENANT).unwrap(), 1_000_000 - actual);
    assert_eq!(ledger.balance(&scope()).unwrap(), 1_000_000 - actual);

    let (status, recorded): (String, Option<i64>) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "settled");
    assert_eq!(recorded, Some(actual));
    drop(primary);
    drop(ledger);

    clock.0.store(10_000, Ordering::SeqCst);
    let reopened = build_ledger(&db_path, clock);
    assert_eq!(reopened.tenant_balance(TENANT).unwrap(), 1_000_000 - actual);
    assert_eq!(reopened.balance(&scope()).unwrap(), 1_000_000 - actual);
    let sidecar = Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    let pending: i64 = sidecar
        .query_row("SELECT COUNT(*) FROM pending_settlements", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn settlement_intent_persistence_failure_blocks_upstream_and_releases_reservation() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock));
    let sidecar = Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    sidecar
        .execute_batch(
            "CREATE TRIGGER fail_intent_insert BEFORE INSERT ON settlement_intents
             BEGIN SELECT RAISE(FAIL, 'injected intent persistence failure'); END;",
        )
        .unwrap();
    drop(sidecar);

    let entered = Arc::new(Notify::new());
    let continue_chat = Arc::new(Notify::new());
    let adapter = Arc::new(GatedAdapter {
        mock: MockAdapter::new(vec![ModelInfo {
            id: PROVIDER.into(),
            memory_gb: 1.0,
        }]),
        entered: entered.clone(),
        continue_chat,
        chat_calls: None,
    });
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card()],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(BODY))
        .unwrap();
    let request_task = tokio::spawn(app.oneshot(request));
    let response = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            _ = entered.notified() => panic!("upstream chat started without durable settlement intent"),
            response = request_task => response.expect("request task should complete").unwrap(),
        }
    })
    .await
    .expect("request should fail before upstream chat");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "internal_error");

    assert_eq!(ledger.tenant_balance(TENANT).unwrap(), 1_000_000);
    assert_eq!(ledger.balance(&scope()).unwrap(), 1_000_000);
    let primary = Connection::open(&db_path).unwrap();
    let (status, actual): (String, Option<i64>) = primary
        .query_row(
            "SELECT status, actual_cost_minor FROM reservations ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "released");
    assert_eq!(actual, None);
}

#[tokio::test]
async fn cancelled_dispatch_with_busy_sidecar_keeps_unknown_intent_fenced_after_ttl() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock.clone()));
    ledger
        .configure_tenant("tenant-a", 1_000_000, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger
        .configure(
            &BudgetScope::new("tenant-a", "default", PROVIDER, PROVIDER),
            1_000_000,
            "UTC",
        )
        .unwrap();
    ledger
        .configure_tenant("tenant-b", 1_000_000, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger
        .configure(
            &BudgetScope::new("tenant-b", "default", PROVIDER, PROVIDER),
            1_000_000,
            "UTC",
        )
        .unwrap();
    let entered = Arc::new(Notify::new());
    let continue_chat = Arc::new(Notify::new());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(GatedAdapter {
        mock: MockAdapter::new(vec![ModelInfo {
            id: PROVIDER.into(),
            memory_gb: 1.0,
        }]),
        entered: entered.clone(),
        continue_chat: continue_chat.clone(),
        chat_calls: Some(chat_calls.clone()),
    });
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        cards: vec![card()],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-idoris-tenant", "tenant-a")
        .body(Body::from(BODY))
        .unwrap();
    let task = tokio::spawn(app.clone().oneshot(request));
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("paid request should enter adapter");

    let mut sidecar =
        Connection::open(db_path.with_added_extension("settlements.sqlite3")).unwrap();
    sidecar.busy_timeout(Duration::from_millis(80)).unwrap();
    let tx = sidecar
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    task.abort();
    let _ = task.await;
    let intents: i64 = tx
        .query_row("SELECT COUNT(*) FROM settlement_intents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(intents, 1, "unknown execution intent must remain durable");
    let a_reservation: String = tx
        .query_row(
            "SELECT reservation_id FROM settlement_intents WHERE tenant_id='tenant-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(tx);
    drop(sidecar);

    // Advance past the reservation TTL, then probe admission until the
    // independent dispatch worker retires its live lease. While it is live,
    // same-tenant retries may pass, so release those probes immediately.
    clock.0.store(1_000_000, Ordering::SeqCst);
    let a_scope = BudgetScope::new("tenant-a", "default", PROVIDER, PROVIDER);
    let orphan_error = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ledger.reserve(&a_scope, Price::Known(1)) {
                Err(error) => break error.to_string(),
                Ok(probe) => ledger.release("tenant-a", &probe).unwrap(),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled dispatch worker should retire its live lease");
    assert!(
        !orphan_error.contains(&a_reservation),
        "public admission error must not expose the reservation id"
    );

    // The reservation is expired now, but an unresolved durable intent must
    // continue to block the next paid dispatch.
    let admission = ledger.reserve(&a_scope, Price::Known(1));
    assert!(admission.is_err(), "orphan intent must remain fenced");
    let request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-idoris-tenant", "tenant-a")
        .body(Body::from(BODY))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(2), app.clone().oneshot(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "internal_error");
    assert!(!String::from_utf8_lossy(&bytes).contains(&a_reservation));
    let intents: i64 = Connection::open(db_path.with_added_extension("settlements.sqlite3"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM settlement_intents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(intents, 1);

    let b_request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-idoris-tenant", "tenant-b")
        .body(Body::from(BODY))
        .unwrap();
    let b_task = tokio::spawn(app.clone().oneshot(b_request));
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("tenant B paid request should enter adapter despite A's intent");
    continue_chat.notify_waiters();
    let b_response = tokio::time::timeout(Duration::from_secs(2), b_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(b_response.status(), StatusCode::OK);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
    assert!(ledger.tenant_balance("tenant-b").unwrap() < 1_000_000);
    assert!(ledger.reserve(&a_scope, Price::Known(1)).is_err());
    let primary = Connection::open(&db_path).unwrap();
    let a_status: String = primary
        .query_row(
            "SELECT status FROM reservations WHERE id=?1",
            [&a_reservation],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(a_status, "active");
    let retained_intent: i64 = Connection::open(
        db_path.with_added_extension("settlements.sqlite3"),
    )
    .unwrap()
    .query_row(
        "SELECT COUNT(*) FROM settlement_intents WHERE reservation_id=?1 AND tenant_id='tenant-a'",
        [&a_reservation],
        |r| r.get(0),
    )
    .unwrap();
    assert_eq!(retained_intent, 1);
}

#[tokio::test]
async fn supervisor_adapter_limit_rejection_releases_confirmed_intent() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock));
    let entered = Arc::new(Notify::new());
    let continue_chat = Arc::new(Notify::new());
    let chat_calls = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(GatedAdapter {
        mock: MockAdapter::new(vec![ModelInfo {
            id: PROVIDER.into(),
            memory_gb: 1.0,
        }]),
        entered: entered.clone(),
        continue_chat: continue_chat.clone(),
        chat_calls: Some(chat_calls.clone()),
    });
    let config = SupervisorConfig {
        max_concurrent_adapter_calls: 1,
        ..SupervisorConfig::default()
    };
    let supervisor = Supervisor::spawn(adapter, config).unwrap();
    let app = build_app(AppState {
        cards: vec![card()],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });

    let first = tokio::spawn(
        app.clone().oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(BODY))
                .unwrap(),
        ),
    );
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("first paid request should occupy the adapter permit");

    let second = app.clone().oneshot(
        Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(BODY))
            .unwrap(),
    );
    let response = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["reason_code"], "supervisor_busy");
    assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
    let primary = Connection::open(&db_path).unwrap();
    let rejected_status: String = primary
        .query_row(
            "SELECT status FROM reservations ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rejected_status, "released");
    let intents: i64 = Connection::open(db_path.with_added_extension("settlements.sqlite3"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM settlement_intents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        intents, 1,
        "only the still-running first request stays fenced"
    );
    drop(primary);

    continue_chat.notify_one();
    let first_response = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);

    // Once the adapter permit is free, the previously rejected request's
    // reservation and intent must not prevent another request from running.
    let third = tokio::spawn(
        app.oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(BODY))
                .unwrap(),
        ),
    );
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("new paid request should enter the adapter after first completes");
    continue_chat.notify_one();
    let third_response = tokio::time::timeout(Duration::from_secs(2), third)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(third_response.status(), StatusCode::OK);
    assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
}

struct BusyAdapter(MockAdapter);

#[async_trait]
impl RuntimeAdapter for BusyAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.0.list().await
    }
    async fn load(
        &self,
        id: &str,
        policy: Option<&idoris_contracts::LoadPolicy>,
    ) -> Result<(), BackendError> {
        self.0.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.0.unload(id).await
    }
    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.0.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.0.probe_ready(id).await
    }
    async fn chat(
        &self,
        _req: ChatRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        Err(BackendError::Busy {
            reason: "adapter busy".into(),
            active_id: None,
            retry_after_ms: None,
        })
    }
}

#[tokio::test]
async fn adapter_busy_error_remains_unknown_and_fences_next_dispatch() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("budget.sqlite3");
    let clock = Arc::new(TestClock::default());
    let ledger = Arc::new(build_ledger(&db_path, clock));
    let adapter = Arc::new(BusyAdapter(MockAdapter::new(vec![ModelInfo {
        id: PROVIDER.into(),
        memory_gb: 1.0,
    }])));
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card()],
        supervisor: Some(supervisor),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let request = || {
        Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(BODY))
            .unwrap()
    };
    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let intents: i64 = Connection::open(db_path.with_added_extension("settlements.sqlite3"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM settlement_intents", [], |r| r.get(0))
        .unwrap();
    assert_eq!(intents, 1, "adapter Busy may follow an executed request");
    let next = app.oneshot(request()).await.unwrap();
    assert_eq!(next.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
