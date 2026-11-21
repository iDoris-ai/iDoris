//! IDORIS-CORE S5（接口规范 §2.7.6，勘误六）：启动门控不被一个起不来的托管引擎无限期拖住。
//!
//! 引擎管理器、门控任务与真正的 Router 接在一起（进程内，不起 iDoris 二进制）：托管的 oMLX 没装（`engine_not_found`）、熔断、
//! 一直等不到就绪（到门控的截止时间放弃）或有没有稳定原因码的配置性失败时，**别的卡照常服务**（这里是一张 resident 的自定义
//! 上游卡），路由到托管卡的请求**直接 503 并带上原因**（`local_only_unavailable` + 该上游在 `/health` 里的 `reason`，不发给引擎），
//! `/health.phase` 是 `degraded`、`upstreams[]` 是真实的状态与原因。引擎还在 `starting` 时门控不放行，所有卡都是 503
//! `idoris_starting`。
//!
//! 假引擎是 `tests/support/fake_omlx.rs` 的 python 脚本：不加载任何模型，不启动任何推理进程。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;
#[path = "support/fake_omlx.rs"]
mod fake_omlx;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode},
};
use fake_omlx::{Behavior, FakeOmlx};
use http_body_util::BodyExt;
use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use idoris_router::lifecycle::gate::{self, Finalized, GateTask};
use idoris_router::lifecycle::{
    EngineBackend, EngineManager, ManagedPlan, ManagerConfig, OmlxBackend, Timings,
};
use idoris_router::readiness::{GateState, HealthState, Observation, UpstreamStatus};
use idoris_router::register::JitterSource;
use idoris_router::runtime::RuntimeRegistry;
use idoris_router::setup_engine::SetupError;
use idoris_router::swap::SwapApp;
use idoris_router::{AppState, build_app, build_starting_app};
use idoris_upstream::engine::{CodesignPolicy, ListenerPolicy, StopOnExit, SystemInspector};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const GIB: u64 = 1 << 30;
const PATIENT: Duration = Duration::from_secs(60);

/// 缩短到毫秒级的时间参数：退避 60 毫秒起翻倍，熔断上限 2（第三次失败就熔断），半开 400 毫秒。
fn fast() -> Timings {
    Timings {
        startup_timeout: Duration::from_secs(30),
        ready_initial_interval: Duration::from_millis(20),
        ready_max_interval: Duration::from_millis(100),
        probe_timeout: Duration::from_secs(2),
        probe_interval: Duration::from_millis(100),
        liveness_interval: Duration::from_millis(50),
        restart_backoff_base: Duration::from_millis(60),
        restart_backoff_cap: Duration::from_millis(500),
        restart_jitter: 0.0,
        jitter: JitterSource::None,
        crash_window: Duration::from_secs(60),
        crash_limit: 2,
        half_open_interval: Duration::from_millis(400),
        stop_grace: Duration::from_secs(3),
        unverifiable_retries: 2,
        unverifiable_pause: Duration::from_millis(20),
    }
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---------------------------------------------------------------------------------------------
// 卡：托管的 oMLX（绑 `fast`）与一张健康的 resident 自定义上游卡（绑 `daily`）
// ---------------------------------------------------------------------------------------------

fn template() -> ComponentCard {
    serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap()
}

fn managed_card(endpoint: &str) -> ComponentCard {
    let mut card = template();
    card.endpoint = endpoint.to_owned();
    card.provider.id = "omlx".to_owned();
    card.load_policy.as_mut().unwrap().mode = LoadMode::OnDemand;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_managed": {"engine": "omlx", "mode": "auto"},
            "idoris_role_models": {"fast": "managed-model"}
        }))
        .unwrap(),
    );
    card
}

/// `setup upstream` 生成的那种卡（LM Studio 之类）：resident、pinned、coexist，带 `idoris_upstream`，绑 `daily`。
fn upstream_card(endpoint: &str) -> ComponentCard {
    serde_yaml::from_str(&format!(
        r#"provider:
  id: lmstudio
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "openai_compat@unknown"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
extensions:
  _degradation: "Removing these bindings disables aliases."
  idoris_role_models: {{ daily: "up-model" }}
  idoris_upstream: {{ kind: openai_compat, flavor: openai_compat }}
"#
    ))
    .unwrap()
}

fn status(id: &str, engine_kind: &str, managed: bool) -> UpstreamStatus {
    UpstreamStatus {
        id: id.to_owned(),
        engine_kind: engine_kind.to_owned(),
        state: idoris_router::readiness::UpstreamState::Starting,
        reason: None,
        managed,
        owned: false,
    }
}

// ---------------------------------------------------------------------------------------------
// 夹具：管理器 + 门控任务 + 真正的 Router
// ---------------------------------------------------------------------------------------------

enum Engine {
    /// 没装 oMLX：可执行文件的路径不存在。
    Missing,
    /// 假引擎，按这个行为。
    Fake(Behavior),
}

struct World {
    manager: Arc<EngineManager>,
    health: HealthState,
    /// 数据面：启动期是 starting 体，门控放行之后是完整的 Router。
    data: SwapApp,
    upstream: MockServer,
    task: GateTask,
    fake: Option<FakeOmlx>,
    _work: tempfile::TempDir,
}

impl World {
    async fn start(engine: Engine, timings: Timings) -> Self {
        Self::start_with_models_dir(engine, timings, None, None).await
    }

    /// `models_dir_error`：模型目录取不到（没有稳定原因码的配置性失败）。`holder`：占着托管端口的服务（不是 oMLX）。
    async fn start_with_models_dir(
        engine: Engine,
        timings: Timings,
        models_dir_error: Option<&'static str>,
        holder: Option<&MockServer>,
    ) -> Self {
        let work = tempfile::tempdir().unwrap();
        let fake = match &engine {
            Engine::Fake(behavior) => {
                let dir = work.path().join("engine");
                std::fs::create_dir_all(&dir).unwrap();
                Some(FakeOmlx::create(&dir, behavior))
            }
            Engine::Missing => None,
        };
        let executable: PathBuf = match &fake {
            Some(fake) => fake.executable.clone(),
            None => work.path().join("no-such-omlx-cli"),
        };
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "up-model",
                "choices": [{"message": {"content": "hello from the upstream"}}]
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"object": "list", "data": [{"id": "up-model", "object": "model"}]}),
            ))
            .mount(&upstream)
            .await;
        let port = match holder {
            Some(holder) => holder.address().port(),
            None => free_port(),
        };
        let managed_endpoint = format!("http://127.0.0.1:{port}");
        let cards = vec![
            managed_card(&managed_endpoint),
            upstream_card(&upstream.uri()),
        ];
        let health = HealthState::with_upstreams(
            0,
            vec![
                status("omlx", "omlx", true),
                status("lmstudio", "openai_compat", false),
            ],
        );
        let models = work.path().join("models");
        let backend = {
            let backend = OmlxBackend::from_env()
                .with_codesign(CodesignPolicy::Off)
                .with_listener_policy(ListenerPolicy::new("TESTTEAM01"));
            match models_dir_error {
                Some(message) => backend
                    .with_models_dir(move || Err(SetupError::new("validation_failed", message))),
                None => backend.with_models_dir(move || Ok(models.clone())),
            }
        };
        let backend: Arc<dyn EngineBackend> = Arc::new(backend);
        let manager = Arc::new(EngineManager::start(
            ManagerConfig {
                state_dir: work.path().join("state"),
                allocated_budget_bytes: Some(16 * GIB),
                stop_on_exit: StopOnExit::Owned,
                executables: BTreeMap::from([("omlx".to_owned(), executable)]),
                launched_by: "test-instance".to_owned(),
                timings,
                inspector: Arc::new(SystemInspector::new()),
                log: Arc::new(|_| {}),
            },
            vec![ManagedPlan {
                id: "omlx".to_owned(),
                endpoint: managed_endpoint,
                port,
                backend,
            }],
            health.clone(),
        ));
        // 启动期：starting 体。放行之后换成完整的 Router，它拿到门控放行的结果（哪些托管上游没就绪）。
        let data = SwapApp::new(build_starting_app(AppState {
            health_state: health.clone(),
            ..AppState::default()
        }));
        let state_health = health.clone();
        let task = gate::spawn(
            manager.clone(),
            health.clone(),
            data.clone(),
            SwapApp::new(Router::new()),
            Box::new(move |_outcome| {
                // 没就绪的托管卡由 `health_state` 的实时快照挡住（IDORIS-CORE S6：不再有门控放行时拍下的名单）。
                let state = AppState {
                    cards,
                    health_state: state_health,
                    ..AppState::default()
                };
                Ok(Finalized {
                    data: build_app(state),
                    admin: Router::new(),
                    runtimes: RuntimeRegistry::default(),
                })
            }),
            tokio_util::sync::CancellationToken::new(),
        );
        Self {
            manager,
            health,
            data,
            upstream,
            task,
            fake,
            _work: work,
        }
    }

    async fn wait_open(&self) {
        wait_for("the gate to open", || self.health.gate() == GateState::Open).await;
    }

    /// 探测器（门控放行之后才起）第一次看到健康的上游卡。
    fn upstream_is_ready(&self) {
        self.health.apply("lmstudio", Observation::Ready);
    }

    async fn call(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("X-iDoris-Privacy", "local_only")
            .header("authorization", "Bearer test-caller-key");
        let body = match body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        };
        let response = self
            .data
            .router()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, headers, value)
    }

    async fn health(&self) -> Value {
        let (status, _, body) = self.call(Method::GET, "/health", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn chat(&self, model: &str) -> (StatusCode, HeaderMap, Value) {
        self.call(
            Method::POST,
            "/v1/chat/completions",
            Some(json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await
    }

    async fn stop(self) {
        self.manager.shutdown(true).await;
        drop(self.task);
        drop(self.fake);
    }
}

fn upstream_entry<'a>(health: &'a Value, id: &str) -> &'a Value {
    health["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("no upstream {id} in {health}"))
}

/// 门控降级放行之后的全部断言：别的卡照常服务，托管卡直接 503 带原因，`/health` 是真实的状态。
async fn assert_degraded_open(world: &World, state: &str, reason: &str) {
    world.upstream_is_ready();
    // `/health`：`phase` 是 `degraded`（有托管引擎没好），`upstreams[]` 是真实的状态与原因。
    let health = world.health().await;
    assert_eq!(health["phase"], "degraded", "{health}");
    assert_eq!(health["ready"], false);
    assert_eq!(
        upstream_entry(&health, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": state, "reason": reason, "managed": true, "owned": false}),
    );
    assert_eq!(upstream_entry(&health, "lmstudio")["state"], "ready");
    // 别的卡照常服务：不再是启动期的 503，请求真的到了上游。
    let (status, _, body) = world.chat("idoris/daily").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.to_string().contains("hello from the upstream"),
        "{body}"
    );
    assert_eq!(
        world
            .upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/chat/completions")
            .count(),
        1
    );
    // 路由到托管卡的请求：503，`local_only_unavailable`（这条路径上已有的 type），原因码是该上游的 `reason`；候选已经选定，
    // 所以带 served-locality。引擎一个字节也没收到（托管卡根本没有 Supervisor，也没有转发）。
    let (status, headers, body) = world.chat("idoris/fast").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable", "{body}");
    assert_eq!(body["error"]["reason_code"], reason, "{body}");
    assert_eq!(headers["x-idoris-served-locality"], "loopback");
    assert_eq!(
        world
            .upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/chat/completions")
            .count(),
        1,
        "the managed card's request went nowhere"
    );
    // `/v1/models`：不探测没就绪的托管卡，别的卡照常列出（一个 401 不会把整个列表变成 502）。
    let (status, _, models) = world.call(Method::GET, "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{models}");
    assert_eq!(
        models["data"],
        json!([{"id": "up-model", "object": "model", "owned_by": "lmstudio"}])
    );
}

// ---------------------------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_engine_does_not_hold_the_other_cards_hostage() {
    let world = World::start(Engine::Missing, fast()).await;
    world.wait_open().await;
    assert_degraded_open(&world, "absent", "engine_not_found").await;
    // `engine_not_found` 与同一个元素里的 `engine_kind` 一起出现（上面整个元素逐字比过）；门控放行了，托管引擎冻结。
    assert_eq!(
        world.manager.start_upstream("omlx").await,
        Err(idoris_router::lifecycle::CommandError::RestartRequired)
    );
    world.stop().await;
}

#[tokio::test]
async fn a_crash_looping_engine_does_not_hold_the_other_cards_hostage_either() {
    // 引擎一起来就退出：失败三次（上限 2）熔断，`failed` + `crash_loop` 落定，门控放行。
    let world = World::start(
        Engine::Fake(Behavior {
            exit_immediately: Some(5),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    world.wait_open().await;
    assert_degraded_open(&world, "failed", "crash_loop").await;
    // 放行之后不再半开重试：半开间隔是 400 毫秒，等三个。
    let launches = world.fake.as_ref().unwrap().launches();
    assert_eq!(launches, 3, "three failed starts, then the breaker");
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(world.fake.as_ref().unwrap().launches(), launches);
    world.stop().await;
}

#[tokio::test]
async fn an_engine_that_never_becomes_ready_is_given_up_at_the_deadline_and_the_other_cards_serve()
{
    // 引擎活着、一直不监听；熔断远在天边：只有门控的截止时间（一个启动超时）能让它落定。
    let mut timings = fast();
    timings.startup_timeout = Duration::from_millis(1500);
    timings.crash_limit = 50;
    let world = World::start(
        Engine::Fake(Behavior {
            never_listen: true,
            ..Behavior::default()
        }),
        timings,
    )
    .await;
    let started = Instant::now();
    // 在截止时间之前，数据面是启动期的 503，所有卡一视同仁。
    let (status, _, body) = world.chat("idoris/daily").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["reason_code"], "idoris_starting", "{body}");
    world.wait_open().await;
    assert!(started.elapsed() < Duration::from_secs(20), "bounded");
    assert_degraded_open(&world, "failed", "startup_timeout").await;
    world.stop().await;
}

#[tokio::test]
async fn a_failure_without_a_stable_reason_code_answers_with_the_existing_supervisor_unavailable() {
    // 模型目录取不到：配置性的终局失败，`/health` 里没有 `reason`（规范没有给它原因码）：请求仍然直接 503，用这条路径上已有的
    // `supervisor_unavailable`，不新增错误码。
    let world = World::start_with_models_dir(
        Engine::Fake(Behavior::default()),
        fast(),
        Some("no usable model dir"),
        None,
    )
    .await;
    world.wait_open().await;
    world.upstream_is_ready();
    let health = world.health().await;
    assert_eq!(health["phase"], "degraded");
    let omlx = upstream_entry(&health, "omlx");
    assert_eq!(omlx["state"], "failed");
    assert!(omlx.get("reason").is_none(), "{omlx}");
    let (status, _, body) = world.chat("idoris/fast").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable");
    assert_eq!(
        body["error"]["reason_code"], "supervisor_unavailable",
        "{body}"
    );
    assert_eq!(world.fake.as_ref().unwrap().launches(), 0);
    let (status, _, body) = world.chat("idoris/daily").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    world.stop().await;
}

#[tokio::test]
async fn while_the_engine_is_still_starting_the_gate_stays_closed_and_every_card_is_503() {
    // 冷启动：监听前先睡 2 秒。`starting` 不算落定，门控等着：这段时间里连健康的上游卡也是启动期的 503。
    let world = World::start(
        Engine::Fake(Behavior {
            listen_delay_secs: 2.0,
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    assert_eq!(world.health.gate(), GateState::Pending);
    let (status, headers, body) = world.chat("idoris/daily").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["reason_code"], "idoris_starting", "{body}");
    assert!(headers.contains_key("retry-after"));
    assert!(world.upstream.received_requests().await.unwrap().is_empty());
    let health = world.health().await;
    assert_eq!(health["phase"], "starting", "{health}");
    // 引擎好了：门控放行，没有降级；上游卡这时才服务。
    world.wait_open().await;
    world.upstream_is_ready();
    let health = world.health().await;
    assert_eq!(health["phase"], "ready", "{health}");
    assert_eq!(upstream_entry(&health, "omlx")["state"], "ready");
    let (status, _, body) = world.chat("idoris/daily").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    world.stop().await;
}

#[tokio::test]
async fn an_unverified_listener_on_the_managed_port_is_never_contacted_by_the_data_plane_after_a_degraded_open()
 {
    // 托管端口上占着一个不是 oMLX 的东西（这里是个对所有请求回 401 的服务，也是这个测试进程里的 wiremock）：管理器不接管它
    // （`port_in_use`），门控降级放行。放行之后数据面不能再给它发任何东西：`/v1/models` 不探测它（否则它的 401 会把整个列表变成
    // 502，env 里的 key 也会被带过去），路由到托管卡的请求直接 503。
    let holder = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(401))
        .mount(&holder)
        .await;
    let world = World::start_with_models_dir(Engine::Missing, fast(), None, Some(&holder)).await;
    world.wait_open().await;
    world.upstream_is_ready();
    let health = world.health().await;
    assert_eq!(
        upstream_entry(&health, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "failed", "reason": "port_in_use", "managed": true, "owned": false}),
    );
    // 管理器自己的（匿名）探测已经发生过了；放行之后的数据面不再碰它。
    let before = holder.received_requests().await.unwrap().len();
    let (status, _, models) = world.call(Method::GET, "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{models}");
    let (status, _, body) = world.chat("idoris/fast").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["reason_code"], "port_in_use", "{body}");
    let after = holder.received_requests().await.unwrap();
    assert_eq!(after.len(), before, "the data plane never contacted it");
    for request in &after {
        assert!(
            request.headers.get("authorization").is_none(),
            "no credential may reach an unverified listener: {request:?}"
        );
    }
    world.stop().await;
}
