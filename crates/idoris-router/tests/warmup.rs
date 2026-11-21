//! IDORIS-CORE S8（接口规范 §2.11.2，修订 1.6 勘误八）：`POST /v1/models/warmup`。
//!
//! 全部进程内、不启动任何真实模型进程：生命周期卡用 `MockAdapter`（外面套一个数 `chat` 调用的壳，证明预热不生成 token），
//! resident 卡与假 oMLX 用 `wiremock`。状态码矩阵、余量确认、key 范围、并发合并、超时不留围栏、门控与降级冻结都在这里；
//! 需要 crate 内部旋钮的（代理的响应头超时）在 `src/warmup_tests.rs`。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use child_guard::SpawnGuarded;
use http_body_util::BodyExt;
use idoris_backend::mock::LoadOutcome;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
};
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_contracts::{ComponentCard, LoadPolicy};
use idoris_router::readiness::{ManagedFailure, UpstreamReason, UpstreamState};
use idoris_router::runtime::RuntimeRegistry;
use idoris_router::{AppState, auth, build_app, build_starting_app};
use idoris_tenancy::event_log::{EventLogStore, EventType};
use idoris_tenancy::store::{RecordKind, TenantStore};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WARMUP: &str = "/v1/models/warmup";
const BUDGET: u64 = 5_000_000_000;
const CONFIRM: (&str, &str) = ("X-iDoris-Headroom-Breach", "confirmed");

// ---------------------------------------------------------------------------------------------
// 请求与应答
// ---------------------------------------------------------------------------------------------

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
    raw: String,
}

impl Reply {
    fn error_type(&self) -> &str {
        self.body["error"]["type"].as_str().unwrap_or("<none>")
    }

    fn reason(&self) -> &str {
        self.body["error"]["reason_code"]
            .as_str()
            .unwrap_or("<none>")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

async fn send_raw(
    app: &Router,
    uri: &str,
    raw: String,
    headers: &[(&str, &str)],
    bearer: Option<&str>,
) -> Reply {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(secret) = bearer {
        request = request.header("authorization", format!("Bearer {secret}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(raw)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    Reply {
        status,
        headers,
        body: serde_json::from_str(&raw).unwrap_or(Value::Null),
        raw,
    }
}

async fn warm(app: &Router, body: Value) -> Reply {
    send_raw(app, WARMUP, body.to_string(), &[], None).await
}

async fn warm_with(app: &Router, body: Value, headers: &[(&str, &str)]) -> Reply {
    send_raw(app, WARMUP, body.to_string(), headers, None).await
}

async fn warm_as(app: &Router, body: Value, bearer: &str, headers: &[(&str, &str)]) -> Reply {
    send_raw(app, WARMUP, body.to_string(), headers, Some(bearer)).await
}

async fn chat(app: &Router, model: &str, headers: &[(&str, &str)]) -> Reply {
    let body = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
    send_raw(app, "/v1/chat/completions", body.to_string(), headers, None).await
}

async fn wait_until(what: &str, patience: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + patience;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// 生命周期卡：MockAdapter（数 chat 的壳）
// ---------------------------------------------------------------------------------------------

/// `MockAdapter` 加一个 `chat` 计数：预热「不生成任何 token」靠它证明。
struct Spy {
    inner: MockAdapter,
    chats: AtomicU32,
}

impl Spy {
    fn new(models: &[(&str, f64)]) -> Self {
        Self {
            inner: MockAdapter::new(
                models
                    .iter()
                    .map(|(id, memory_gb)| ModelInfo {
                        id: (*id).to_string(),
                        memory_gb: *memory_gb,
                    })
                    .collect(),
            ),
            chats: AtomicU32::new(0),
        }
    }

    fn chats(&self) -> u32 {
        self.chats.load(Ordering::SeqCst)
    }

    fn fence(&self) -> PathBuf {
        self.inner.load_fence_path().unwrap()
    }
}

#[async_trait::async_trait]
impl RuntimeAdapter for Spy {
    fn load_fence_path(&self) -> Result<PathBuf, BackendError> {
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
        self.chats.fetch_add(1, Ordering::SeqCst);
        self.inner.chat(req, cancel).await
    }
}

fn omlx_template() -> ComponentCard {
    serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap()
}

/// 默认的 oMLX 卡（on_demand）加角色绑定。没有绑定时角色别名无候选，只能用具体模型 id。
fn lifecycle_card(roles: &[(&str, &str)]) -> ComponentCard {
    let mut card = omlx_template();
    if !roles.is_empty() {
        let bindings: serde_json::Map<String, Value> = roles
            .iter()
            .map(|(role, model)| ((*role).to_string(), json!(model)))
            .collect();
        card.extensions = Some(
            serde_json::from_value(json!({
                "_degradation": "Removing these bindings disables aliases.",
                "idoris_role_models": bindings,
            }))
            .unwrap(),
        );
    }
    card
}

struct Rig {
    app: Router,
    adapter: Arc<Spy>,
}

fn rig_with(
    cards: Vec<ComponentCard>,
    models: &[(&str, f64)],
    budget: Option<u64>,
    customize: impl FnOnce(&mut AppState),
) -> Rig {
    let adapter = Arc::new(Spy::new(models));
    let factory_adapter = adapter.clone();
    let runtimes = RuntimeRegistry::spawn_with_factory_and_budget(&cards, budget, move |_| {
        Ok(factory_adapter.clone() as Arc<dyn RuntimeAdapter>)
    })
    .unwrap();
    let mut state = AppState {
        cards,
        runtimes,
        ..AppState::default()
    };
    customize(&mut state);
    Rig {
        app: build_app(state),
        adapter,
    }
}

/// 一张绑了 `daily`/`deep`/`fast` 的 oMLX 卡，三个模型；缺省 24 GB 额度。
fn standard() -> Rig {
    rig_with(
        vec![lifecycle_card(&[
            ("daily", "qwen-daily"),
            ("deep", "qwen-deep"),
            ("fast", "qwen-fast"),
        ])],
        &[("qwen-daily", 2.0), ("qwen-deep", 4.0), ("qwen-fast", 1.0)],
        None,
        |_| {},
    )
}

/// 与 `headroom_breach.rs` 同一套：5 GB 额度（留 1 GB 余量），`big` 4.5、`small` 1.0、`huge` 6.0。
fn budgeted() -> Rig {
    rig_with(
        vec![omlx_template()],
        &[("big", 4.5), ("small", 1.0), ("huge", 6.0)],
        Some(BUDGET),
        |_| {},
    )
}

// ---------------------------------------------------------------------------------------------
// 成功路径与应答形状
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn warming_a_role_loads_its_bound_model_and_generates_nothing() {
    let rig = standard();
    let reply = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "qwen-daily", "解析后的具体模型 id");
    assert_eq!(reply.body["provider_id"], "omlx");
    assert_eq!(reply.body["state"], "ready");
    assert_eq!(reply.body["already_loaded"], false);
    assert!(reply.body["load_ms"].is_u64(), "{}", reply.raw);
    let mut keys: Vec<_> = reply.body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["already_loaded", "load_ms", "model", "provider_id", "state"],
        "应答只有规范里的五个字段"
    );
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.header("x-idoris-served-locality"), Some("loopback"));
    let record_id = reply.header("x-idoris-record-id").expect("record id");
    assert!(uuid::Uuid::parse_str(record_id).is_ok(), "{record_id}");
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 1);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-deep"), 0);
    assert_eq!(rig.adapter.chats(), 0, "预热不生成任何 token");
    assert!(!rig.adapter.fence().exists(), "加载完成就清掉围栏");
}

#[tokio::test]
async fn a_second_warmup_is_a_no_op_that_reports_already_loaded() {
    let rig = standard();
    assert_eq!(
        warm(&rig.app, json!({"model": "idoris/daily"}))
            .await
            .status,
        StatusCode::OK
    );
    let again = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.raw);
    assert_eq!(again.body["already_loaded"], true);
    assert_eq!(again.body["state"], "ready");
    assert_eq!(
        rig.adapter.inner.load_call_count("qwen-daily"),
        1,
        "幂等：不重复加载"
    );
    assert_eq!(rig.adapter.chats(), 0);
}

#[tokio::test]
async fn a_warmed_model_serves_the_next_chat_without_loading_again() {
    let rig = standard();
    assert_eq!(
        warm(&rig.app, json!({"model": "idoris/daily"}))
            .await
            .status,
        StatusCode::OK
    );
    let reply = chat(&rig.app, "idoris/daily", &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 1);
    assert_eq!(rig.adapter.chats(), 1, "只有真正的聊天生成了 token");
}

#[tokio::test]
async fn a_concrete_model_id_is_warmed_as_given() {
    let rig = rig_with(vec![omlx_template()], &[("some-model", 1.0)], None, |_| {});
    let reply = warm(&rig.app, json!({"model": "some-model"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "some-model");
    assert_eq!(rig.adapter.inner.load_call_count("some-model"), 1);
}

#[tokio::test]
async fn the_valid_timeout_range_is_inclusive_at_both_ends() {
    for timeout in [1_000, 300_000] {
        let rig = standard();
        let reply = warm(
            &rig.app,
            json!({"model": "idoris/fast", "timeout_ms": timeout}),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "timeout_ms={timeout}: {}",
            reply.raw
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 请求校验（400）
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_body_that_is_not_json_is_400_invalid_json() {
    let rig = standard();
    for raw in ["", "{", "not json", "{\"model\":"] {
        let reply = send_raw(&rig.app, WARMUP, raw.to_string(), &[], None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{raw:?}");
        assert_eq!(reply.error_type(), "invalid_json", "{raw:?}");
        assert_eq!(reply.reason(), "invalid_json");
    }
}

#[tokio::test]
async fn a_json_body_that_is_not_an_object_is_400_invalid_body() {
    let rig = standard();
    for raw in ["[]", "\"idoris/daily\"", "5", "null", "true"] {
        let reply = send_raw(&rig.app, WARMUP, raw.to_string(), &[], None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(reply.error_type(), "invalid_body", "{raw}");
    }
}

#[tokio::test]
async fn an_unknown_field_is_400_unsupported_field_and_loads_nothing() {
    let rig = standard();
    for extra in [
        json!({"stream": true}),
        json!({"max_tokens": 1}),
        json!({"messages": []}),
        json!({"Model": "idoris/daily"}),
        json!({"timeout": 5000}),
    ] {
        let mut body = json!({"model": "idoris/daily"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let reply = warm(&rig.app, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.error_type(), "unsupported_field", "{body}");
        assert_eq!(reply.reason(), "unsupported_parameter", "{body}");
    }
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 0);
}

#[tokio::test]
async fn a_long_or_control_laden_field_name_is_not_echoed_raw() {
    let rig = standard();
    let name = format!("bad\nfield{}", "x".repeat(200));
    let mut body = json!({"model": "idoris/daily"});
    body[&name] = json!(1);
    let reply = warm(&rig.app, body).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let remediation = reply.body["error"]["remediation"].as_str().unwrap();
    assert!(!remediation.contains('\n'), "{remediation:?}");
    assert!(remediation.len() < 220, "{remediation:?}");
}

#[tokio::test]
async fn model_must_be_a_non_empty_string() {
    let rig = standard();
    for body in [
        json!({}),
        json!({"model": null}),
        json!({"model": 5}),
        json!({"model": ["idoris/daily"]}),
        json!({"model": ""}),
        json!({"model": "   "}),
        json!({"timeout_ms": 5000}),
    ] {
        let reply = warm(&rig.app, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.error_type(), "unsupported_field", "{body}");
        assert_eq!(reply.reason(), "unsupported_model", "{body}");
    }
}

#[tokio::test]
async fn timeout_ms_must_be_an_integer_between_1000_and_300000() {
    let rig = standard();
    for bad in [
        json!(999),
        json!(0),
        json!(-5000),
        json!(300_001),
        json!(1000.5),
        json!(5000.0),
        json!("5000"),
        json!(null),
        json!(true),
        json!([5000]),
    ] {
        let body = json!({"model": "idoris/daily", "timeout_ms": bad});
        let reply = warm(&rig.app, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.error_type(), "unsupported_field", "{body}");
        assert_eq!(reply.reason(), "invalid_timeout_ms", "{body}");
    }
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 0);
}

#[tokio::test]
async fn header_and_role_errors_are_the_chat_ones() {
    let rig = standard();
    let reply = warm_with(
        &rig.app,
        json!({"model": "idoris/daily"}),
        &[("X-iDoris-Privacy", "bogus")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_type(), "invalid_privacy");
    let reply = warm(&rig.app, json!({"model": "idoris/no-such-role"})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_type(), "unknown_role");
    // 请求体错误先于头部错误（同聊天的顺序）。
    let reply = send_raw(
        &rig.app,
        WARMUP,
        "{".into(),
        &[("X-iDoris-Privacy", "bogus")],
        None,
    )
    .await;
    assert_eq!(reply.error_type(), "invalid_json");
}

#[tokio::test]
async fn header_errors_come_before_the_key_scope_check() {
    let ring = keyring(&[("daily", &["daily"], LOCAL)]);
    let rig = authed_standard(&ring);
    // The key may not warm `deep`, but a malformed privacy header is a 400 first (same order as chat).
    let reply = warm_as(
        &rig.app,
        json!({"model": "idoris/deep"}),
        &ring.secrets["daily"],
        &[("X-iDoris-Privacy", "bogus")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.raw);
    assert_eq!(reply.error_type(), "invalid_privacy");
    let reply = warm_as(
        &rig.app,
        json!({"model": "idoris/deep"}),
        &ring.secrets["daily"],
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.raw);
}

#[tokio::test]
async fn idoris_auto_is_warmed_as_an_ordinary_role_binding() {
    let rig = rig_with(
        vec![lifecycle_card(&[("auto", "qwen-auto")])],
        &[("qwen-auto", 1.0)],
        None,
        |_| {},
    );
    let reply = warm(&rig.app, json!({"model": "idoris/auto"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "qwen-auto");
    assert_eq!(rig.adapter.inner.load_call_count("qwen-auto"), 1);
}

/// 一张只会 rerank 的 oMLX 卡（on_demand）：模板卡的能力是 chat 与 embedding，不含 rerank。
fn lifecycle_rerank_card(model: &str) -> ComponentCard {
    let mut card = lifecycle_card(&[("rerank", model)]);
    card.provider.capabilities = vec![Capability::Rerank];
    card
}

#[tokio::test]
async fn a_lifecycle_rerank_card_is_warmed_by_loading_it_like_any_other_role() {
    let rig = rig_with(
        vec![lifecycle_rerank_card("qwen-rerank")],
        &[("qwen-rerank", 1.0)],
        None,
        |_| {},
    );
    let reply = warm(&rig.app, json!({"model": "idoris/rerank"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "qwen-rerank");
    assert_eq!(reply.body["provider_id"], "omlx");
    assert_eq!(reply.body["state"], "ready");
    assert_eq!(reply.body["already_loaded"], false);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-rerank"), 1);
    assert_eq!(rig.adapter.chats(), 0, "就绪就是已加载：不发任何请求");
    assert!(!rig.adapter.fence().exists());
    let again = warm(&rig.app, json!({"model": "idoris/rerank"})).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.raw);
    assert_eq!(again.body["already_loaded"], true);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-rerank"), 1);
}

#[tokio::test]
async fn the_rerank_role_is_warmed_only_on_a_card_that_does_rerank() {
    // 模板卡能 chat 与 embedding，不能 rerank：即使角色绑定把 rerank 指给它，也不是预热 rerank 的候选，什么都不加载。
    let rig = rig_with(
        vec![lifecycle_card(&[("rerank", "qwen-rerank")])],
        &[("qwen-rerank", 1.0)],
        None,
        |_| {},
    );
    let reply = warm(&rig.app, json!({"model": "idoris/rerank"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "no_eligible_candidate");
    assert_eq!(rig.adapter.inner.load_call_count("qwen-rerank"), 0);
    assert_eq!(rig.adapter.chats(), 0);
}

#[tokio::test]
async fn only_post_is_routed() {
    let rig = standard();
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let request = Request::builder()
            .method(method)
            .uri(WARMUP)
            .body(Body::empty())
            .unwrap();
        let response = rig.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
    }
}

// ---------------------------------------------------------------------------------------------
// 选卡：同聊天
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_model_is_404_model_not_found_for_a_concrete_id_and_for_a_bound_role() {
    let rig = rig_with(
        vec![lifecycle_card(&[("daily", "not-installed")])],
        &[("installed", 1.0)],
        None,
        |_| {},
    );
    for model in ["no-such-model", "idoris/daily"] {
        let reply = warm(&rig.app, json!({"model": model})).await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{model}: {}",
            reply.raw
        );
        assert_eq!(reply.error_type(), "model_not_found", "{model}");
        assert_eq!(reply.reason(), "model_not_found", "{model}");
        assert_eq!(
            reply.header("x-idoris-served-locality"),
            Some("loopback"),
            "候选已选定，失败也带 served-locality"
        );
    }
    assert_eq!(rig.adapter.inner.load_call_count("installed"), 0);
    assert_eq!(rig.adapter.inner.load_call_count("not-installed"), 0);
}

#[tokio::test]
async fn an_unbound_role_is_rejected_exactly_like_chat_rejects_it() {
    let rig = standard();
    let warmed = warm(&rig.app, json!({"model": "idoris/vision"})).await;
    let chatted = chat(&rig.app, "idoris/vision", &[]).await;
    assert_eq!(
        warmed.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        warmed.raw
    );
    assert_eq!(warmed.status, chatted.status);
    assert_eq!(warmed.error_type(), chatted.error_type());
    assert_eq!(warmed.reason(), chatted.reason());
}

#[tokio::test]
async fn no_candidate_at_all_is_503_local_only_unavailable() {
    let rig = rig_with(vec![], &[], None, |_| {});
    let reply = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "local_only_unavailable");
}

#[tokio::test]
async fn a_lifecycle_card_without_a_supervisor_fails_closed() {
    let cards = vec![lifecycle_card(&[("daily", "qwen-daily")])];
    let app = build_app(AppState {
        cards,
        ..AppState::default()
    });
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "local_only_unavailable");
    assert_eq!(reply.reason(), "supervisor_unavailable");
}

#[tokio::test]
async fn a_supervisor_bound_to_a_different_endpoint_is_not_used() {
    let adapter = Arc::new(Spy::new(&[("qwen-daily", 1.0)]));
    let bound = lifecycle_card(&[("daily", "qwen-daily")]);
    let factory_adapter = adapter.clone();
    let runtimes = RuntimeRegistry::spawn_with_factory_and_budget(
        std::slice::from_ref(&bound),
        None,
        move |_| Ok(factory_adapter.clone() as Arc<dyn RuntimeAdapter>),
    )
    .unwrap();
    // The card that routes is not the card the Supervisor was built for (another endpoint): like chat,
    // fail closed instead of driving somebody else's engine.
    let mut served = bound.clone();
    served.endpoint = "http://127.0.0.1:9".into();
    let app = build_app(AppState {
        cards: vec![served],
        runtimes,
        ..AppState::default()
    });
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.reason(), "supervisor_unavailable");
    assert_eq!(adapter.inner.load_call_count("qwen-daily"), 0);
}

#[tokio::test]
async fn a_paid_card_is_refused_because_warmup_is_local_and_free() {
    let mut card = lifecycle_card(&[("daily", "qwen-daily")]);
    card.provider.cost = idoris_contracts::provider::Cost {
        input_per_m: 1.0,
        output_per_m: 2.0,
    };
    let rig = rig_with(vec![card], &[("qwen-daily", 1.0)], None, |_| {});
    let reply = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "paid_proxy_unavailable");
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 0);
}

// ---------------------------------------------------------------------------------------------
// 1 GB 余量确认（规范 §2.8 的约定原样适用）
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unconfirmed_warmup_into_the_headroom_asks_for_confirmation_and_loads_nothing() {
    let rig = budgeted();
    let reply = warm(&rig.app, json!({"model": "big"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "headroom_confirmation_required");
    assert_eq!(reply.reason(), "headroom_confirmation_required");
    let remediation = reply.body["error"]["remediation"].as_str().unwrap();
    assert!(remediation.contains("X-iDoris-Headroom-Breach: confirmed"));
    assert_eq!(reply.header("x-idoris-served-locality"), Some("loopback"));
    assert_eq!(rig.adapter.inner.load_call_count("big"), 0);
}

#[tokio::test]
async fn the_confirmation_header_spends_the_headroom_for_that_warmup_only() {
    let rig = budgeted();
    let reply = warm_with(&rig.app, json!({"model": "big"}), &[CONFIRM]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(rig.adapter.inner.load_call_count("big"), 1);
    // Nothing is remembered: once `small` evicts `big`, warming `big` asks again.
    assert_eq!(
        warm(&rig.app, json!({"model": "small"})).await.status,
        StatusCode::OK
    );
    let again = warm(&rig.app, json!({"model": "big"})).await;
    assert_eq!(
        again.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        again.raw
    );
    assert_eq!(again.error_type(), "headroom_confirmation_required");
    assert_eq!(rig.adapter.inner.load_call_count("big"), 1);
}

#[tokio::test]
async fn any_value_but_confirmed_counts_as_absent_and_a_repeated_header_is_ambiguous() {
    let rig = budgeted();
    for value in ["true", "1", "confirm", "confirmed2", "yes", ""] {
        let reply = warm_with(
            &rig.app,
            json!({"model": "big"}),
            &[("x-idoris-headroom-breach", value)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{value:?}");
        assert_eq!(
            reply.error_type(),
            "headroom_confirmation_required",
            "{value:?}"
        );
    }
    // ASCII case-insensitive, as on the chat path.
    let upper = warm_with(
        &rig.app,
        json!({"model": "big"}),
        &[("x-idoris-headroom-breach", "CONFIRMED")],
    )
    .await;
    assert_eq!(upper.status, StatusCode::OK, "{}", upper.raw);
    let rig = budgeted();
    let request = Request::builder()
        .method("POST")
        .uri(WARMUP)
        .header("content-type", "application/json")
        .header("x-idoris-headroom-breach", "confirmed")
        .header("x-idoris-headroom-breach", "confirmed")
        .body(Body::from(json!({"model": "big"}).to_string()))
        .unwrap();
    let response = rig.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rig.adapter.inner.load_call_count("big"), 0);
}

#[tokio::test]
async fn a_model_that_does_not_fit_the_whole_allocation_is_terminal_even_when_confirmed() {
    let rig = budgeted();
    for headers in [&[][..], &[CONFIRM][..]] {
        let reply = warm_with(&rig.app, json!({"model": "huge"}), headers).await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            reply.raw
        );
        assert_eq!(reply.error_type(), "local_only_unavailable");
        assert_ne!(
            reply.reason(),
            "headroom_confirmation_required",
            "终态，不该再弹确认"
        );
    }
    assert_eq!(rig.adapter.inner.load_call_count("huge"), 0);
}

#[tokio::test]
async fn a_model_that_fits_under_the_reduced_cap_needs_no_confirmation() {
    let rig = budgeted();
    let reply = warm(&rig.app, json!({"model": "small"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
}

#[tokio::test]
async fn the_header_on_a_chat_request_does_not_confirm_a_later_warmup() {
    let rig = budgeted();
    assert_eq!(
        chat(&rig.app, "big", &[CONFIRM]).await.status,
        StatusCode::OK
    );
    assert_eq!(chat(&rig.app, "small", &[]).await.status, StatusCode::OK);
    let reply = warm(&rig.app, json!({"model": "big"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "headroom_confirmation_required");
}

// ---------------------------------------------------------------------------------------------
// 生命周期繁忙、加载失败
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn warming_another_model_while_one_loads_is_503_supervisor_busy() {
    let rig = standard();
    // The second request arrives 150 ms in, with 1.35 s to spare before the load ends: a slow machine
    // cannot make the first load finish before the second one is admitted.
    rig.adapter
        .inner
        .set_load_delay("qwen-daily", Duration::from_millis(1_500));
    let late = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        warm(&rig.app, json!({"model": "idoris/deep"})).await
    };
    let (first, second) = tokio::join!(warm(&rig.app, json!({"model": "idoris/daily"})), late);
    assert_eq!(first.status, StatusCode::OK, "{}", first.raw);
    assert_eq!(
        second.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        second.raw
    );
    assert_eq!(second.error_type(), "local_only_unavailable");
    assert_eq!(second.reason(), "supervisor_busy");
    assert_eq!(rig.adapter.inner.load_call_count("qwen-deep"), 0);
}

#[tokio::test]
async fn a_warmup_cannot_join_a_load_admitted_with_a_different_confirmation() {
    let rig = budgeted();
    rig.adapter
        .inner
        .set_load_delay("small", Duration::from_millis(1_500));
    let late = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        warm_with(&rig.app, json!({"model": "small"}), &[CONFIRM]).await
    };
    let (plain, confirmed) = tokio::join!(warm(&rig.app, json!({"model": "small"})), late);
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.raw);
    assert_eq!(
        confirmed.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        confirmed.raw
    );
    assert_eq!(confirmed.reason(), "supervisor_busy");
    assert!(
        confirmed.body["error"]["remediation"]
            .as_str()
            .unwrap()
            .contains("different headroom confirmation"),
        "{}",
        confirmed.raw
    );
    assert_eq!(rig.adapter.inner.load_call_count("small"), 1);
}

#[tokio::test]
async fn an_engine_load_failure_is_503_local_only_unavailable_and_a_retry_can_succeed() {
    let rig = standard();
    rig.adapter
        .inner
        .set_load_script("qwen-daily", vec![LoadOutcome::Fail]);
    let failed = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        failed.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        failed.raw
    );
    assert_eq!(failed.error_type(), "local_only_unavailable");
    assert_eq!(failed.reason(), "upstream_error");
    // 引擎明确拒绝：什么都没分配，围栏已清，下一次预热能成功。
    assert!(!rig.adapter.fence().exists());
    let retried = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(retried.status, StatusCode::OK, "{}", retried.raw);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 2);
}

// ---------------------------------------------------------------------------------------------
// 并发合并：同一模型的并发预热与并发首个请求只加载一次
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_warmups_and_the_first_chat_of_one_model_load_it_once() {
    let rig = standard();
    rig.adapter
        .inner
        .set_load_delay("qwen-daily", Duration::from_millis(1_000));
    let body = || json!({"model": "idoris/daily"});
    let (a, b, c, first_chat) = tokio::join!(
        warm(&rig.app, body()),
        warm(&rig.app, body()),
        warm(&rig.app, body()),
        chat(&rig.app, "idoris/daily", &[]),
    );
    for reply in [&a, &b, &c] {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
        assert_eq!(reply.body["state"], "ready");
    }
    assert_eq!(first_chat.status, StatusCode::OK, "{}", first_chat.raw);
    assert_eq!(
        rig.adapter.inner.load_call_count("qwen-daily"),
        1,
        "三个预热加一个聊天合并成一次引擎加载"
    );
    assert_eq!(rig.adapter.chats(), 1, "只有那个聊天生成了 token");
}

// ---------------------------------------------------------------------------------------------
// 超时：504 `warmup_timeout`，不取消加载、不留围栏
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn timeout_ms_expiring_is_504_warmup_timeout_and_the_load_finishes_without_leaving_a_fence() {
    let rig = standard();
    rig.adapter
        .inner
        .set_load_delay("qwen-daily", Duration::from_millis(3_000));
    let started = Instant::now();
    let reply = warm(
        &rig.app,
        json!({"model": "idoris/daily", "timeout_ms": 1000}),
    )
    .await;
    let waited = started.elapsed();
    assert_eq!(reply.status, StatusCode::GATEWAY_TIMEOUT, "{}", reply.raw);
    assert_eq!(reply.error_type(), "warmup_timeout");
    assert_eq!(reply.reason(), "warmup_timeout");
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_millis(2_500),
        "等了 timeout_ms，没有等到加载结束: {waited:?}"
    );
    assert_eq!(reply.header("x-idoris-served-locality"), Some("loopback"));
    // 放弃等待不是取消：加载还在引擎里进行，围栏正好说明它还没结束……
    assert!(rig.adapter.fence().exists(), "加载仍在进行，围栏在");
    // ……它自己跑完并清掉围栏，没有人留下 load.pending。
    let fence = rig.adapter.fence();
    wait_until(
        "the load to finish and clear its fence",
        Duration::from_secs(15),
        || !fence.exists(),
    )
    .await;
    assert_eq!(
        rig.adapter.inner.load_call_count("qwen-daily"),
        1,
        "没有重新加载"
    );
    assert!(
        rig.adapter
            .inner
            .event_log()
            .contains(&"load:qwen-daily:end".to_string()),
        "引擎里的加载完成了"
    );
    assert_eq!(
        rig.adapter.inner.unload_call_count("qwen-daily"),
        0,
        "超时不卸载"
    );
    // 之后的预热直接看到「已就绪」，聊天也不再加载。
    let after = warm(
        &rig.app,
        json!({"model": "idoris/daily", "timeout_ms": 1000}),
    )
    .await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.raw);
    assert_eq!(after.body["already_loaded"], true);
    assert_eq!(
        chat(&rig.app, "idoris/daily", &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 1);
}

#[tokio::test]
async fn a_chat_that_arrives_after_a_warmup_timeout_joins_the_load_that_is_still_running() {
    let rig = standard();
    rig.adapter
        .inner
        .set_load_delay("qwen-daily", Duration::from_millis(3_000));
    let reply = warm(
        &rig.app,
        json!({"model": "idoris/daily", "timeout_ms": 1000}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::GATEWAY_TIMEOUT, "{}", reply.raw);
    let joined = chat(&rig.app, "idoris/daily", &[]).await;
    assert_eq!(joined.status, StatusCode::OK, "{}", joined.raw);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 1);
    assert!(!rig.adapter.fence().exists());
}

#[tokio::test]
async fn a_client_that_hangs_up_does_not_cancel_the_load_either() {
    let rig = standard();
    rig.adapter
        .inner
        .set_load_delay("qwen-daily", Duration::from_millis(1_500));
    let abandoned = tokio::time::timeout(
        Duration::from_millis(200),
        warm(&rig.app, json!({"model": "idoris/daily"})),
    )
    .await;
    assert!(
        abandoned.is_err(),
        "the request future was dropped mid-load"
    );
    let fence = rig.adapter.fence();
    wait_until(
        "the abandoned load to finish",
        Duration::from_secs(15),
        || !fence.exists(),
    )
    .await;
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 1);
    let after = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(after.body["already_loaded"], true, "{}", after.raw);
}

// ---------------------------------------------------------------------------------------------
// 鉴权与 key 范围（同聊天）
// ---------------------------------------------------------------------------------------------

struct Keyring {
    authenticator: auth::VirtualKeyAuthenticator,
    secrets: BTreeMap<&'static str, String>,
}

/// 每把 key 一个名字：`(名字, allowed_roles, allowed_privacy)`。
fn keyring(specs: &[(&'static str, &[&str], &[PrivacyClass])]) -> Keyring {
    let store = Arc::new(Mutex::new(
        VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    let mut secrets = BTreeMap::new();
    for (name, roles, privacy) in specs {
        let key = MintedVirtualKey::mint();
        store
            .lock()
            .unwrap()
            .insert_active(
                &key.key_id,
                key.hash,
                &VirtualKeyScope {
                    owner: (*name).into(),
                    tenant_id: None,
                    allowed_privacy: privacy.to_vec(),
                    allowed_roles: roles.iter().map(|role| (*role).to_string()).collect(),
                    budget_ref: None,
                    expires_at_ms: None,
                    admin_scopes: Vec::new(),
                },
            )
            .unwrap();
        secrets.insert(*name, key.secret.expose_secret().to_string());
    }
    Keyring {
        authenticator: auth::VirtualKeyAuthenticator::new(store),
        secrets,
    }
}

const ALL_ROLES: &[&str] = &[
    "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
];
const LOCAL: &[PrivacyClass] = &[PrivacyClass::LocalOnly];

fn authed_standard(ring: &Keyring) -> Rig {
    let authenticator = ring.authenticator.clone();
    rig_with(
        vec![lifecycle_card(&[
            ("daily", "qwen-daily"),
            ("deep", "qwen-deep"),
        ])],
        &[("qwen-daily", 2.0), ("qwen-deep", 4.0), ("some-model", 1.0)],
        None,
        move |state| {
            state.virtual_key_authenticator = Some(authenticator);
            state.remote_bind_requires_auth = true;
        },
    )
}

#[tokio::test]
async fn without_a_valid_key_warmup_is_401_before_anything_loads() {
    let ring = keyring(&[("daily", &["daily"], LOCAL)]);
    let rig = authed_standard(&ring);
    let reply = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.raw);
    let reply = warm_as(
        &rig.app,
        json!({"model": "idoris/daily"}),
        "not-a-real-key",
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.raw);
    // 鉴权先于校验：没有 key 的人看到 401，不是 400。
    let reply = send_raw(&rig.app, WARMUP, "{".into(), &[], None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 0);
}

#[tokio::test]
async fn a_role_scoped_key_may_warm_only_its_own_roles() {
    let ring = keyring(&[
        ("daily", &["daily"], LOCAL),
        ("embed", &["embed"], LOCAL),
        ("all", ALL_ROLES, LOCAL),
    ]);
    let rig = authed_standard(&ring);
    let daily = ring.secrets["daily"].as_str();
    let ok = warm_as(&rig.app, json!({"model": "idoris/daily"}), daily, &[]).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.raw);
    // 聊天会被拒的 key，预热也被拒：别的角色、无角色的具体模型 id。
    for model in ["idoris/deep", "some-model"] {
        let denied = warm_as(&rig.app, json!({"model": model}), daily, &[]).await;
        assert_eq!(
            denied.status,
            StatusCode::FORBIDDEN,
            "{model}: {}",
            denied.raw
        );
        assert_eq!(denied.error_type(), "policy_violation", "{model}");
        assert_eq!(denied.reason(), "VIRTUAL_KEY_ROLE_FORBIDDEN", "{model}");
    }
    assert_eq!(rig.adapter.inner.load_call_count("qwen-deep"), 0);
    assert_eq!(rig.adapter.inner.load_call_count("some-model"), 0);
    // 与聊天同一个判定：同一把 key、同一个角色，聊天也被拒。
    let chat_denied = {
        let body = json!({"model": "idoris/deep", "messages": [{"role": "user", "content": "hi"}]});
        send_raw(
            &rig.app,
            "/v1/chat/completions",
            body.to_string(),
            &[],
            Some(daily),
        )
        .await
    };
    assert_eq!(chat_denied.status, StatusCode::FORBIDDEN);
    assert_eq!(chat_denied.reason(), "VIRTUAL_KEY_ROLE_FORBIDDEN");
    // 只有 embed 角色的 key 预热不了 daily。
    let embed_key = ring.secrets["embed"].as_str();
    let denied = warm_as(&rig.app, json!({"model": "idoris/daily"}), embed_key, &[]).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.raw);
    // 全角色 key：具体模型 id 也行。
    let all = ring.secrets["all"].as_str();
    let concrete = warm_as(&rig.app, json!({"model": "some-model"}), all, &[]).await;
    assert_eq!(concrete.status, StatusCode::OK, "{}", concrete.raw);
}

#[tokio::test]
async fn a_local_only_key_cannot_warm_with_privacy_any() {
    let ring = keyring(&[
        ("local", ALL_ROLES, LOCAL),
        (
            "both",
            ALL_ROLES,
            &[PrivacyClass::LocalOnly, PrivacyClass::Any],
        ),
    ]);
    let rig = authed_standard(&ring);
    let denied = warm_as(
        &rig.app,
        json!({"model": "idoris/daily"}),
        &ring.secrets["local"],
        &[("X-iDoris-Privacy", "any")],
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.raw);
    assert_eq!(denied.reason(), "VIRTUAL_KEY_PRIVACY_FORBIDDEN");
    let allowed = warm_as(
        &rig.app,
        json!({"model": "idoris/daily"}),
        &ring.secrets["both"],
        &[("X-iDoris-Privacy", "any")],
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.raw);
}

#[tokio::test]
async fn fallback_next_in_chain_needs_full_role_authority() {
    let ring = keyring(&[("daily", &["daily"], LOCAL), ("all", ALL_ROLES, LOCAL)]);
    let rig = authed_standard(&ring);
    let denied = warm_as(
        &rig.app,
        json!({"model": "idoris/daily"}),
        &ring.secrets["daily"],
        &[("X-iDoris-Fallback", "next_in_chain")],
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.raw);
    assert_eq!(denied.reason(), "VIRTUAL_KEY_FALLBACK_FORBIDDEN");
}

#[tokio::test]
async fn developer_no_key_mode_allows_only_local_only_warmups() {
    let rig = rig_with(
        vec![lifecycle_card(&[("daily", "qwen-daily")])],
        &[("qwen-daily", 1.0)],
        None,
        |state| state.dev_no_key_enabled = true,
    );
    let ok = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.raw);
    let denied = warm_with(
        &rig.app,
        json!({"model": "idoris/daily"}),
        &[("X-iDoris-Privacy", "any")],
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.raw);
    assert_eq!(denied.reason(), "DEV_NO_KEY_SCOPE_FORBIDDEN");
}

#[tokio::test]
async fn tenant_mode_requires_the_tenant_header() {
    let rig = rig_with(
        vec![lifecycle_card(&[("daily", "qwen-daily")])],
        &[("qwen-daily", 1.0)],
        None,
        |state| state.deploy_mode = idoris_contracts::DeployMode::Tenant,
    );
    let denied = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    assert_eq!(denied.status, StatusCode::BAD_REQUEST, "{}", denied.raw);
    assert_eq!(denied.error_type(), "tenant_missing");
}

#[tokio::test]
async fn tenant_mode_checks_the_tenant_header_before_authenticating_like_every_inference_path() {
    let ring = keyring(&[("all", ALL_ROLES, LOCAL)]);
    let authenticator = ring.authenticator.clone();
    let rig = rig_with(
        vec![lifecycle_card(&[("daily", "qwen-daily")])],
        &[("qwen-daily", 1.0)],
        None,
        move |state| {
            state.deploy_mode = idoris_contracts::DeployMode::Tenant;
            state.virtual_key_authenticator = Some(authenticator);
        },
    );
    // 没有租户头也没有 key：和聊天一样先是 400 `tenant_missing`，不是 401。
    let warmed = warm(&rig.app, json!({"model": "idoris/daily"})).await;
    let chatted = chat(&rig.app, "idoris/daily", &[]).await;
    assert_eq!(warmed.status, StatusCode::BAD_REQUEST, "{}", warmed.raw);
    assert_eq!(warmed.error_type(), "tenant_missing");
    assert_eq!(
        (warmed.status, warmed.error_type()),
        (chatted.status, chatted.error_type())
    );
    assert_eq!(rig.adapter.inner.load_call_count("qwen-daily"), 0);
}

// ---------------------------------------------------------------------------------------------
// resident 卡：一个最小的真实请求
// ---------------------------------------------------------------------------------------------

/// 一张 resident 卡。`omlx` 为真时是 `omlx@` 的 loopback 卡（伴生卡的形状，会带 oMLX key），否则是自定义 OpenAI 兼容上游
/// （带 `idoris_upstream`，永远不带 key）。
fn resident_card(
    id: &str,
    endpoint: &str,
    capabilities: &str,
    roles: &str,
    omlx: bool,
) -> ComponentCard {
    let (pin, upstream) = if omlx {
        ("omlx@0.6.4", "")
    } else {
        (
            "openai_compat@unknown",
            "  idoris_upstream: { kind: openai_compat, flavor: openai_compat }\n",
        )
    };
    serde_yaml::from_str(&format!(
        r#"provider:
  id: {id}
  family: local
  tier: local
  capabilities: [{capabilities}]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "{pin}"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
extensions:
  _degradation: "Removing these bindings disables aliases."
  idoris_role_models: {{ {roles} }}
{upstream}"#
    ))
    .unwrap()
}

/// 对 chat 与 embeddings 都答 200 的假上游。
async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    for route in ["/v1/chat/completions", "/v1/embeddings"] {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "x"}}],
                "data": [{"object": "embedding", "index": 0, "embedding": [0.1]}]
            })))
            .mount(&server)
            .await;
    }
    server
}

fn resident_app(cards: Vec<ComponentCard>) -> Router {
    build_app(AppState {
        cards,
        ..AppState::default()
    })
}

async fn requests_of(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap()
}

#[tokio::test]
async fn a_resident_chat_card_gets_one_minimal_chat_request() {
    let server = upstream().await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "up-model");
    assert_eq!(reply.body["provider_id"], "lmstudio");
    assert_eq!(reply.body["state"], "ready");
    assert_eq!(
        reply.body["already_loaded"], false,
        "resident 卡没有账本可问"
    );
    assert!(reply.body["load_ms"].is_u64());
    assert_eq!(reply.header("x-idoris-served-locality"), Some("loopback"));
    assert!(reply.header("x-idoris-record-id").is_some());
    let seen = requests_of(&server).await;
    assert_eq!(seen.len(), 1, "恰好一个请求");
    assert_eq!(seen[0].url.path(), "/v1/chat/completions");
    let sent: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(
        sent,
        json!({
            "model": "up-model",
            "messages": [{"role": "user", "content": "warmup"}],
            "max_tokens": 1,
            "stream": false,
        })
    );
}

#[tokio::test]
async fn a_resident_embedding_card_gets_one_minimal_embedding_request() {
    let server = upstream().await;
    let app = resident_app(vec![resident_card(
        "omlx-embed",
        &server.uri(),
        "embedding",
        "embed: \"bge-m3-mlx-fp16\"",
        true,
    )]);
    let reply = warm(&app, json!({"model": "idoris/embed"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "bge-m3-mlx-fp16");
    assert_eq!(reply.body["provider_id"], "omlx-embed");
    let seen = requests_of(&server).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].url.path(), "/v1/embeddings");
    let sent: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent, json!({"model": "bge-m3-mlx-fp16", "input": "warmup"}));
}

#[tokio::test]
async fn the_role_decides_which_card_and_which_minimal_request() {
    let chat_server = upstream().await;
    let embed_server = upstream().await;
    let app = resident_app(vec![
        resident_card(
            "lmstudio",
            &chat_server.uri(),
            "chat",
            "daily: \"up-model\"",
            false,
        ),
        resident_card(
            "omlx-embed",
            &embed_server.uri(),
            "embedding",
            "embed: \"bge-m3-mlx-fp16\"",
            true,
        ),
    ]);
    assert_eq!(
        warm(&app, json!({"model": "idoris/embed"})).await.status,
        StatusCode::OK
    );
    assert_eq!(requests_of(&chat_server).await.len(), 0);
    let embed = requests_of(&embed_server).await;
    assert_eq!(embed.len(), 1);
    assert_eq!(embed[0].url.path(), "/v1/embeddings");
    assert_eq!(
        warm(&app, json!({"model": "idoris/daily"})).await.status,
        StatusCode::OK
    );
    assert_eq!(
        requests_of(&embed_server).await.len(),
        1,
        "daily 不碰 embedding 卡"
    );
    let chats = requests_of(&chat_server).await;
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].url.path(), "/v1/chat/completions");
    // 要预热什么由端点和角色定，不由调用方的能力头定。
    let overridden = warm_with(
        &app,
        json!({"model": "idoris/embed"}),
        &[("X-iDoris-Capabilities", "chat")],
    )
    .await;
    assert_eq!(overridden.status, StatusCode::OK, "{}", overridden.raw);
    assert_eq!(requests_of(&embed_server).await.len(), 2);
    assert_eq!(requests_of(&chat_server).await.len(), 1);
}

#[tokio::test]
async fn a_concrete_model_id_on_a_resident_card_is_forwarded_as_given() {
    let server = upstream().await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    let reply = warm(&app, json!({"model": "whatever-the-engine-has"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "whatever-the-engine-has");
    let sent: Value = serde_json::from_slice(&requests_of(&server).await[0].body).unwrap();
    assert_eq!(sent["model"], "whatever-the-engine-has");
}

#[tokio::test]
async fn a_resident_warmup_is_never_replayed_from_the_idempotency_cache() {
    let server = upstream().await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    for _ in 0..2 {
        let reply = warm_with(
            &app,
            json!({"model": "idoris/daily"}),
            &[("X-iDoris-Request-Id", "same-id")],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
        assert!(reply.header("x-idoris-cached").is_none());
    }
    assert_eq!(requests_of(&server).await.len(), 2, "每次预热都是真请求");
}

#[tokio::test]
async fn resident_upstream_answers_map_onto_the_warmup_error_matrix() {
    // 404：上游不认这个模型。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({"error": {"message": "gone"}})),
        )
        .mount(&server)
        .await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.raw);
    assert_eq!(reply.error_type(), "model_not_found");
    assert_eq!(reply.reason(), "model_not_found");
    assert!(
        !reply.raw.contains("gone"),
        "上游的文字不外传: {}",
        reply.raw
    );

    // 401：代理自己的 502 `upstream_auth_failed`，与聊天一样。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("API key required"))
        .mount(&server)
        .await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.raw);
    assert_eq!(reply.error_type(), "upstream_auth_failed");
    assert!(!reply.raw.contains("API key required"), "{}", reply.raw);

    // 其他非 2xx：和聊天的 resident 路径一样原样透传。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"engine": "broke"})))
        .mount(&server)
        .await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.raw
    );
    assert_eq!(reply.body, json!({"engine": "broke"}));
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.header("x-idoris-served-locality"), Some("loopback"));
}

#[tokio::test]
async fn a_resident_upstream_slower_than_timeout_ms_is_504_warmup_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(4_000)))
        .mount(&server)
        .await;
    let app = resident_app(vec![resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    )]);
    let started = Instant::now();
    let reply = warm(&app, json!({"model": "idoris/daily", "timeout_ms": 1000})).await;
    assert_eq!(reply.status, StatusCode::GATEWAY_TIMEOUT, "{}", reply.raw);
    assert_eq!(reply.error_type(), "warmup_timeout");
    assert_eq!(reply.reason(), "warmup_timeout");
    assert!(
        started.elapsed() < Duration::from_millis(3_500),
        "gave up at timeout_ms, did not wait for the upstream: {:?}",
        started.elapsed()
    );
    assert_eq!(reply.header("x-idoris-served-locality"), Some("loopback"));
}

#[tokio::test]
async fn a_paid_resident_card_is_refused_before_anything_is_sent() {
    let server = upstream().await;
    let mut card = resident_card(
        "lmstudio",
        &server.uri(),
        "chat",
        "daily: \"up-model\"",
        false,
    );
    card.provider.cost = idoris_contracts::provider::Cost {
        input_per_m: 1.0,
        output_per_m: 1.0,
    };
    let app = resident_app(vec![card]);
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "paid_proxy_unavailable");
    assert_eq!(requests_of(&server).await.len(), 0);
}

#[tokio::test]
async fn a_resident_rerank_card_is_refused_because_the_spec_has_no_minimal_rerank_request() {
    let server = upstream().await;
    let app = resident_app(vec![resident_card(
        "rerank-local",
        &server.uri(),
        "rerank",
        "rerank: \"up-rerank\"",
        false,
    )]);
    let reply = warm(&app, json!({"model": "idoris/rerank"})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.raw);
    assert_eq!(reply.error_type(), "unsupported_field");
    assert_eq!(reply.reason(), "unsupported_model");
    assert_eq!(
        reply.header("x-idoris-served-locality"),
        Some("loopback"),
        "候选已选定，拒绝也带 served-locality"
    );
    assert_eq!(
        requests_of(&server).await.len(),
        0,
        "不发一个种类不对的请求（既不是 chat 也不是 embeddings）"
    );
}

#[tokio::test]
async fn the_callers_virtual_key_never_reaches_the_resident_upstream() {
    let ring = keyring(&[("daily", &["daily"], LOCAL)]);
    let server = upstream().await;
    let authenticator = ring.authenticator.clone();
    let app = build_app(AppState {
        cards: vec![resident_card(
            "lmstudio",
            &server.uri(),
            "chat",
            "daily: \"up-model\"",
            false,
        )],
        virtual_key_authenticator: Some(authenticator),
        remote_bind_requires_auth: true,
        ..AppState::default()
    });
    let reply = warm_as(
        &app,
        json!({"model": "idoris/daily"}),
        &ring.secrets["daily"],
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    let seen = requests_of(&server).await;
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].headers.get("authorization").is_none(),
        "自定义上游不带任何 Authorization: {:?}",
        seen[0].headers
    );
}

#[tokio::test]
async fn a_cooling_resident_card_is_skipped_when_another_card_can_warm() {
    // Card `a-bad` answers 500: two consecutive faults open its (loopback) provider breaker, after which
    // the readiness snapshot excludes it from selection, as for chat.
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bad)
        .await;
    let good = upstream().await;
    let app = resident_app(vec![
        resident_card("a-bad", &bad.uri(), "chat", "daily: \"m\"", false),
        resident_card("b-good", &good.uri(), "chat", "daily: \"m\"", false),
    ]);
    for attempt in 0..2 {
        let reply = warm(&app, json!({"model": "idoris/daily"})).await;
        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "attempt {attempt}: {}",
            reply.raw
        );
    }
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["provider_id"], "b-good");
}

#[tokio::test]
async fn a_subscription_card_is_never_a_warmup_candidate() {
    let subscription: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap();
    // Remote-tier cards are eligible for `any` here, so only the warmup's own exclusion keeps it out.
    let routing_policy = idoris_contracts::RoutingPolicy {
        routing_policy: idoris_contracts::routing_policy::RoutingPolicyInner {
            version: 1,
            rules: Vec::new(),
            default: idoris_contracts::routing_policy::Action {
                tiers: Some(vec![
                    idoris_contracts::common::Tier::Local,
                    idoris_contracts::common::Tier::Remote,
                ]),
                fail_closed: Some(false),
                ..Default::default()
            },
        },
    };
    let app = build_app(AppState {
        cards: vec![subscription],
        routing_policy,
        ..AppState::default()
    });
    let reply = warm_with(
        &app,
        json!({"model": "some-model"}),
        &[("X-iDoris-Privacy", "any")],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    // With the subscription card excluded there is no candidate at all. (Were it a candidate, it would
    // be selected and fail as `supervisor_unavailable`: it has no lifecycle runtime.)
    assert_eq!(reply.error_type(), "no_eligible_candidate", "{}", reply.raw);
}

#[tokio::test]
async fn content_that_tightens_privacy_is_reported_in_the_reason_header() {
    let rig = rig_with(
        vec![omlx_template()],
        &[("carol@example.com", 1.0), ("plain-model", 1.0)],
        None,
        |_| {},
    );
    let any = [("X-iDoris-Privacy", "any")];
    // The model id is inspected like any other string of the body: an e-mail address pins `any` to local.
    let tightened = warm_with(&rig.app, json!({"model": "carol@example.com"}), &any).await;
    assert_eq!(tightened.status, StatusCode::OK, "{}", tightened.raw);
    let reason = tightened.header("x-idoris-reason").unwrap_or("");
    assert!(reason.contains("PrivacyTightenedByContent"), "{reason:?}");
    // Control: without such content the same `any` request carries no tightening reason.
    let plain = warm_with(&rig.app, json!({"model": "plain-model"}), &any).await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.raw);
    let reason = plain.header("x-idoris-reason").unwrap_or("");
    assert!(!reason.contains("PrivacyTightenedByContent"), "{reason:?}");
}

// ---------------------------------------------------------------------------------------------
// 隐私：远程熔断开关与 `X-iDoris-Privacy`
// ---------------------------------------------------------------------------------------------

/// 一张「本机之外」的免费卡（局域网，隐私等级 any）：只有 `X-iDoris-Privacy: any` 才有资格被选。
fn lan_card(endpoint: &str) -> ComponentCard {
    let mut card = resident_card("lan-box", endpoint, "chat", "daily: \"lan-model\"", false);
    card.provider.locality = idoris_contracts::provider::Locality::Lan;
    card.provider.privacy_class = PrivacyClass::Any;
    card.privacy_class = PrivacyClass::Any;
    card.allowed_egress = vec![idoris_contracts::component_card::Egress::Lan];
    card
}

#[tokio::test]
async fn privacy_defaults_to_local_only_and_the_kill_switch_pins_any_back_to_local() {
    let server = upstream().await;
    let app = resident_app(vec![lan_card(&server.uri())]);
    // 缺省 local_only：局域网卡没有资格。
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(requests_of(&server).await.len(), 0);
    // 声明 any：有资格，served-locality 如实报 lan。
    let reply = warm_with(
        &app,
        json!({"model": "idoris/daily"}),
        &[("X-iDoris-Privacy", "any")],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.header("x-idoris-served-locality"), Some("lan"));
    assert_eq!(requests_of(&server).await.len(), 1);
    // 远程熔断开关合上：any 被钉回 local_only，同一个请求不再能到局域网卡。
    let state = AppState {
        cards: vec![lan_card(&server.uri())],
        ..AppState::default()
    };
    state
        .remote_kill_switch
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let app = build_app(state);
    let reply = warm_with(
        &app,
        json!({"model": "idoris/daily"}),
        &[("X-iDoris-Privacy", "any")],
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(
        requests_of(&server).await.len(),
        1,
        "开关合上之后一个字节也没出去"
    );
}

// ---------------------------------------------------------------------------------------------
// 启动门控（S5）与降级冻结的托管卡
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn while_the_startup_gate_is_closed_warmup_is_503_idoris_starting() {
    let app = build_starting_app(AppState {
        cards: vec![lifecycle_card(&[("daily", "qwen-daily")])],
        ..AppState::default()
    });
    for raw in [
        json!({"model": "idoris/daily"}).to_string(),
        "{".to_string(),
    ] {
        let reply = send_raw(&app, WARMUP, raw, &[], None).await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            reply.raw
        );
        assert_eq!(reply.error_type(), "idoris_starting");
        assert_eq!(reply.reason(), "idoris_starting");
        assert!(reply.header("retry-after").is_some());
    }
}

fn frozen(reason: Option<UpstreamReason>) -> Arc<BTreeMap<String, ManagedFailure>> {
    Arc::new(BTreeMap::from([(
        "omlx".to_string(),
        ManagedFailure {
            engine_kind: "omlx".into(),
            state: UpstreamState::Failed,
            reason,
        },
    )]))
}

fn managed_lifecycle(roles: &[(&str, &str)]) -> ComponentCard {
    let mut card = lifecycle_card(roles);
    let extensions = card.extensions.get_or_insert_with(Default::default);
    extensions.insert(
        "idoris_managed".into(),
        json!({"engine": "omlx", "mode": "auto"}),
    );
    card
}

#[tokio::test]
async fn a_frozen_managed_lifecycle_card_answers_exactly_like_chat_without_touching_the_engine() {
    for reason in [
        Some(UpstreamReason::CrashLoop),
        Some(UpstreamReason::EngineNotFound),
        None,
    ] {
        // 降级放行：没就绪的托管卡不建 Supervisor（`runtimes` 里没有它）。
        let app = build_app(AppState {
            cards: vec![managed_lifecycle(&[("fast", "qwen-fast")])],
            managed_failures: frozen(reason),
            ..AppState::default()
        });
        let warmed = warm(&app, json!({"model": "idoris/fast"})).await;
        let chatted = chat(&app, "idoris/fast", &[]).await;
        let expected = reason.map_or("supervisor_unavailable", UpstreamReason::as_str);
        assert_eq!(
            warmed.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{reason:?}: {}",
            warmed.raw
        );
        assert_eq!(warmed.error_type(), "local_only_unavailable", "{reason:?}");
        assert_eq!(warmed.reason(), expected, "{reason:?}");
        assert_eq!(warmed.status, chatted.status, "{reason:?}");
        assert_eq!(warmed.error_type(), chatted.error_type(), "{reason:?}");
        assert_eq!(warmed.reason(), chatted.reason(), "{reason:?}");
        assert_eq!(warmed.header("x-idoris-served-locality"), Some("loopback"));
    }
}

#[tokio::test]
async fn a_frozen_managed_resident_card_never_gets_the_minimal_request() {
    let server = upstream().await;
    let mut card = resident_card("omlx", &server.uri(), "chat", "daily: \"qwen\"", true);
    card.extensions.as_mut().unwrap().insert(
        "idoris_managed".into(),
        json!({"engine": "omlx", "mode": "auto"}),
    );
    let app = build_app(AppState {
        cards: vec![card],
        managed_failures: frozen(Some(UpstreamReason::StartupTimeout)),
        ..AppState::default()
    });
    let reply = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "local_only_unavailable");
    assert_eq!(reply.reason(), "startup_timeout");
    assert_eq!(
        requests_of(&server).await.len(),
        0,
        "没发给一个没起来的端口"
    );
}

#[tokio::test]
async fn a_healthy_sibling_card_still_warms_while_the_managed_one_is_frozen() {
    let server = upstream().await;
    let app = build_app(AppState {
        cards: vec![
            managed_lifecycle(&[("fast", "qwen-fast")]),
            resident_card(
                "lmstudio",
                &server.uri(),
                "chat",
                "daily: \"up-model\"",
                false,
            ),
        ],
        managed_failures: frozen(Some(UpstreamReason::CrashLoop)),
        ..AppState::default()
    });
    let healthy = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(healthy.status, StatusCode::OK, "{}", healthy.raw);
    let frozen_one = warm(&app, json!({"model": "idoris/fast"})).await;
    assert_eq!(
        frozen_one.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        frozen_one.raw
    );
    assert_eq!(frozen_one.reason(), "crash_loop");
}

// ---------------------------------------------------------------------------------------------
// 审计：一条 `intent: warmup` 的事件，不进用量与预算记录
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_warmup_writes_one_audit_event_with_intent_warmup_and_no_budget_or_usage_trace() {
    let tenant = Some(idoris_router::budget::PERSONAL_TENANT_ID);
    let log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let records = Arc::new(Mutex::new(
        TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    let (sink, record_sink) = (log.clone(), records.clone());
    let rig = rig_with(
        vec![lifecycle_card(&[("daily", "qwen-daily")])],
        &[("qwen-daily", 1.0)],
        None,
        move |state| {
            state.event_log = Some(sink);
            state.record_store = Some(record_sink);
        },
    );
    // 调用方声明的 intent 不能把预热记成别的。
    let reply = warm_with(
        &rig.app,
        json!({"model": "idoris/daily"}),
        &[
            ("X-iDoris-Intent", "coding"),
            ("X-iDoris-Request-Id", "warm-req-1"),
        ],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    let record_id = reply.header("x-idoris-record-id").unwrap().to_string();
    let events = log.events_for_record(tenant, &record_id).unwrap();
    let kinds: Vec<_> = events.iter().map(|e| e.event.event_type).collect();
    assert_eq!(
        kinds,
        [EventType::AuditFinalized],
        "只有一条审计事件: {events:?}"
    );
    let metadata = &events[0].event.metadata;
    assert_eq!(metadata["intent"], "warmup");
    assert_eq!(metadata["http_status"], 200);
    assert_eq!(metadata["component"], "router");
    assert_eq!(events[0].event.request_id.as_deref(), Some("warm-req-1"));
    // 不进用量记录；审计记录有一条，intent 同样是 warmup。
    let stored = |kind| records.lock().unwrap().list(tenant, Some(kind)).unwrap();
    assert!(
        stored(RecordKind::Usage).is_empty(),
        "预热不进用量记录（本地免费）"
    );
    let audit = stored(RecordKind::Audit);
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].payload["intent"], "warmup");
    // 失败的预热也记一条。
    let failed = warm(&rig.app, json!({"model": "no-such-model"})).await;
    assert_eq!(failed.status, StatusCode::NOT_FOUND);
    let failed_id = failed.header("x-idoris-record-id").unwrap().to_string();
    let events = log.events_for_record(tenant, &failed_id).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].event.metadata["http_status"], 404);
    assert_eq!(events[0].event.metadata["intent"], "warmup");
    assert!(stored(RecordKind::Usage).is_empty());
    // 对照：真正的聊天会留下用量记录，所以上面的「空」说明的是预热没有，而不是夹具没记。
    assert_eq!(
        chat(&rig.app, "idoris/daily", &[]).await.status,
        StatusCode::OK
    );
    assert_eq!(stored(RecordKind::Usage).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// oMLX 适配器端到端：独立的 `load_timeout`
// ---------------------------------------------------------------------------------------------

const GIB: u64 = 1 << 30;

/// 假 oMLX：`GET /api/status`（启动采样）、`GET /v1/models/status`（目录、固定状态、就绪探测一份回答）与带延迟的
/// `POST /v1/models/{id}/load`。
async fn fake_omlx(model: &str, load_delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"id": model, "loaded": true, "pinned": false, "estimated_size": 2 * GIB}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/models/{model}/load")))
        .respond_with(ResponseTemplate::new(200).set_delay(load_delay))
        .mount(&server)
        .await;
    server
}

/// 一台真的 `OmlxAdapter`（指向假 oMLX，管理调用超时 300 毫秒）经 Supervisor 接进路由器。`load_timeout` 为 `None` 时
/// 适配器不单独给加载超时（v0.5.0 的行为）。返回围栏路径。
fn omlx_rig(server: &MockServer, load_timeout: Option<Duration>) -> (Router, PathBuf) {
    let fence = idoris_backend::mock::temporary_load_fence_path();
    let mut card = lifecycle_card(&[("daily", "m")]);
    card.endpoint = server.uri();
    let factory_fence = fence.clone();
    let runtimes = RuntimeRegistry::spawn_with_factory_and_budget(
        std::slice::from_ref(&card),
        None,
        move |card| {
            let adapter = idoris_upstream::OmlxAdapter::new(idoris_upstream::OmlxAdapterConfig {
                base_url: card.endpoint.clone(),
                api_key: None,
                call_timeout: Duration::from_millis(300),
            })?
            .with_load_fence_path(factory_fence.clone());
            let adapter = match load_timeout {
                Some(timeout) => adapter.with_load_timeout(timeout),
                None => adapter,
            };
            Ok(Arc::new(adapter) as Arc<dyn RuntimeAdapter>)
        },
    )
    .unwrap();
    let app = build_app(AppState {
        cards: vec![card],
        runtimes,
        ..AppState::default()
    });
    (app, fence)
}

#[tokio::test]
async fn a_cold_load_slower_than_the_management_timeout_warms_up_without_leaving_a_fence() {
    let server = fake_omlx("m", Duration::from_millis(900)).await;
    let (app, fence) = omlx_rig(&server, Some(Duration::from_secs(10)));
    let reply = warm(&app, json!({"model": "idoris/daily", "timeout_ms": 10_000})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
    assert_eq!(reply.body["model"], "m");
    assert_eq!(reply.body["already_loaded"], false);
    assert!(
        reply.body["load_ms"].as_u64().unwrap() >= 800,
        "{}",
        reply.raw
    );
    assert!(
        !fence.exists(),
        "load_timeout 够长，加载不是「结果未知」，没有围栏"
    );
    assert_eq!(
        warm(&app, json!({"model": "idoris/daily"})).await.body["already_loaded"],
        true
    );
}

#[tokio::test]
async fn without_a_separate_load_timeout_the_same_cold_load_is_an_unknown_outcome_that_leaves_the_fence()
 {
    // 对照：v0.5.0 的行为（load 与别的管理调用共用 call_timeout）。这就是设计稿 R2 要避免的事。
    let server = fake_omlx("m", Duration::from_millis(1_500)).await;
    let (app, fence) = omlx_rig(&server, None);
    let reply = warm(&app, json!({"model": "idoris/daily", "timeout_ms": 10_000})).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.raw
    );
    assert_eq!(reply.error_type(), "local_only_unavailable");
    assert_eq!(reply.reason(), "load_unconfirmed");
    assert!(fence.exists(), "结果未知：围栏保留，直到人工确认");
}

#[tokio::test]
async fn a_warmup_timeout_over_a_real_adapter_neither_cancels_the_load_nor_leaves_a_fence() {
    let server = fake_omlx("m", Duration::from_millis(3_000)).await;
    let (app, fence) = omlx_rig(&server, Some(Duration::from_secs(10)));
    let reply = warm(&app, json!({"model": "idoris/daily", "timeout_ms": 1000})).await;
    assert_eq!(reply.status, StatusCode::GATEWAY_TIMEOUT, "{}", reply.raw);
    assert_eq!(reply.error_type(), "warmup_timeout");
    assert!(fence.exists(), "加载仍在进行");
    wait_until("the engine load to finish", Duration::from_secs(15), || {
        !fence.exists()
    })
    .await;
    let after = warm(&app, json!({"model": "idoris/daily"})).await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.raw);
    assert_eq!(after.body["already_loaded"], true);
    let loads = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/v1/models/m/load")
        .count();
    assert_eq!(loads, 1, "引擎只收到一次加载请求");
}

// ---------------------------------------------------------------------------------------------
// resident 的 oMLX key：只给 oMLX 的 loopback 卡（环境变量是进程全局的，所以每个用例跑在子进程里）
// ---------------------------------------------------------------------------------------------

const CHILD: &str = "IDORIS_WARMUP_KEY_CASE";
const KEY_ENV: &str = "IDORIS_OMLX_API_KEY";
const KEY: &str = "omlx-secret-key-s8";

async fn run_key_case(case: &str) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("API key required"))
        .with_priority(5)
        .mount(&server)
        .await;
    match case {
        "omlx_card_carries_the_key" => {
            let app = resident_app(vec![resident_card(
                "omlx-embed",
                &server.uri(),
                "embedding",
                "embed: \"bge\"",
                true,
            )]);
            let reply = warm(&app, json!({"model": "idoris/embed"})).await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.raw);
            let seen = requests_of(&server).await;
            assert_eq!(seen.len(), 1);
            assert_eq!(
                seen[0]
                    .headers
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                format!("Bearer {KEY}")
            );
        }
        "custom_upstream_never_carries_it" => {
            // 自定义上游即使端口、id 都像 oMLX 也不带 key：上游回 401，映射成 502 `upstream_auth_failed`。
            let app = resident_app(vec![resident_card(
                "omlx",
                &server.uri(),
                "chat",
                "daily: \"m\"",
                false,
            )]);
            let reply = warm(&app, json!({"model": "idoris/daily"})).await;
            assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.raw);
            assert_eq!(reply.error_type(), "upstream_auth_failed");
            let seen = requests_of(&server).await;
            assert_eq!(seen.len(), 1);
            assert!(seen[0].headers.get("authorization").is_none());
        }
        other => panic!("unknown case {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// `IDORIS_LOAD_TIMEOUT_S` 一路到生产适配器与启动（同样在子进程里：环境变量是进程全局的）
// ---------------------------------------------------------------------------------------------

const LOAD_CHILD: &str = "IDORIS_WARMUP_LOAD_TIMEOUT_CASE";
const LOAD_ENV: &str = "IDORIS_LOAD_TIMEOUT_S";

fn run_load_timeout_case(case: &str) {
    let card = lifecycle_card(&[]);
    let adapter = idoris_upstream::factory::create_adapter(&card);
    let seconds = |secs| Some(Duration::from_secs(secs));
    match case {
        "unset" | "blank" => assert_eq!(adapter.ok().unwrap().load_timeout(), seconds(180)),
        "seven" => assert_eq!(adapter.ok().unwrap().load_timeout(), seconds(7)),
        "padded" => assert_eq!(adapter.ok().unwrap().load_timeout(), seconds(42)),
        "bad_text" | "zero" | "too_big" => {
            let Err(error) = adapter else {
                panic!("an invalid {LOAD_ENV} must not construct an adapter");
            };
            assert!(error.to_string().contains(LOAD_ENV), "{error}");
            // 启动（建运行时登记）也失败，错误点名这个变量：不会悄悄用缺省值起来。
            let Err(startup) = RuntimeRegistry::spawn(std::slice::from_ref(&card)) else {
                panic!("an invalid {LOAD_ENV} must fail start-up");
            };
            assert!(startup.contains(LOAD_ENV), "{startup}");
        }
        other => panic!("unknown case {other}"),
    }
}

#[tokio::test]
async fn the_load_timeout_override_reaches_the_production_adapter_and_a_bad_value_fails_startup() {
    if let Ok(case) = std::env::var(LOAD_CHILD) {
        run_load_timeout_case(&case);
        return;
    }
    for (case, value) in [
        ("unset", None),
        ("blank", Some("")),
        ("seven", Some("7")),
        ("padded", Some(" 42 ")),
        ("bad_text", Some("abc")),
        ("zero", Some("0")),
        ("too_big", Some("3601")),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "the_load_timeout_override_reaches_the_production_adapter_and_a_bad_value_fails_startup",
                "--nocapture",
            ])
            .env(LOAD_CHILD, case)
            .env_remove(LOAD_ENV)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(value) = value {
            command.env(LOAD_ENV, value);
        }
        let output = command.spawn_guarded().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "case {case}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn the_omlx_key_is_attached_to_omlx_cards_only() {
    if let Ok(case) = std::env::var(CHILD) {
        run_key_case(&case).await;
        return;
    }
    for case in [
        "omlx_card_carries_the_key",
        "custom_upstream_never_carries_it",
    ] {
        let home = std::env::temp_dir().join(format!(
            "idoris-warmup-key-home-{}-{case}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "the_omlx_key_is_attached_to_omlx_cards_only",
                "--nocapture",
            ])
            .env(CHILD, case)
            // The key may also come from ~/.omlx/settings.json: the child gets an empty home.
            .env("HOME", &home)
            .env(KEY_ENV, KEY)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = command.spawn_guarded().unwrap().wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "case {case}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            !stdout.contains(KEY) && !stderr.contains(KEY),
            "the key is never logged (case {case})"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
