//! IDORIS-CORE S6（接口规范 §2.11.4、勘误七）：Admin 的 `upstreams` 读与 `start|stop|restart` 写，加上 S5 评审留下的三道门（G1、
//! G2、G3）。进程内：真的引擎管理器、真的 Admin Router（经 `tower` 的 `oneshot`）、假 oMLX（`tests/support/fake_omlx.rs`）；
//! 起真二进制的那一份在 `lifecycle_admin_process.rs`。
//!
//! **全部用假引擎**：不加载任何模型、不启动任何推理进程，不连外网，不读真实的 `~/.omlx`。引擎的可执行文件都是显式给的，所以这台机器上装没装
//! 真的 oMLX.app 都不会被拉起；检查器是可编排的（身份读真的 `ps`，「谁在监听」「谁签的名」按测试编排），所以不依赖 `lsof` 与 `codesign`。
//! 带 Supervisor 的几条（`B` 组）用 `BoundSupervisor::spawn_omlx`，和别的 Router 集成测试一样，它的围栏锁文件按端口落在默认状态目录下。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;
#[path = "support/fake_omlx.rs"]
mod fake_omlx;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use fake_omlx::{Behavior, FakeOmlx, spawn_retrying};
use http_body_util::BodyExt;
use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use idoris_router::admin::{AdminSessionToken, SharedAdminToken, build_admin_app_with_engines};
use idoris_router::lifecycle::gate::{self, Finalized};
use idoris_router::lifecycle::{
    EngineBackend, EngineManager, ManagedPlan, ManagerConfig, OmlxBackend, Timings,
};
use idoris_router::readiness::{
    GateState, HealthState, Observation, UpstreamReason, UpstreamState, UpstreamStatus,
};
use idoris_router::register::{JitterSource, LogSink};
use idoris_router::runtime::RuntimeRegistry;
use idoris_router::swap::SwapApp;
use idoris_router::{AppState, build_app};
use idoris_upstream::engine::{
    CodesignPolicy, InspectError, ListenerPolicy, PidState, ProcessInspector, StopOnExit,
    SystemInspector,
};
use idoris_upstream::omlx::key::{EndpointKey, governed_endpoint};
use serde_json::{Value, json};
use tower::ServiceExt;

const GIB: u64 = 1 << 30;
const PATIENT: Duration = Duration::from_secs(60);
const TEAM: &str = "TESTTEAM01";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn pid_alive(pid: u64) -> bool {
    let signalled = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    let stat = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&stat.stdout).trim().to_owned();
    signalled && !stat.is_empty() && !stat.starts_with('Z')
}

fn command_of(pid: u64) -> String {
    let output = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// 可编排的检查器（同 `lifecycle_manager.rs`）
// ---------------------------------------------------------------------------------------------

struct Scripted {
    real: SystemInspector,
    claims: Mutex<Vec<u16>>,
    listeners: Mutex<BTreeMap<u16, Vec<u32>>>,
    teams: Mutex<BTreeMap<u32, String>>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            real: SystemInspector::new(),
            claims: Mutex::new(Vec::new()),
            listeners: Mutex::new(BTreeMap::new()),
            teams: Mutex::new(BTreeMap::new()),
        })
    }

    /// 自有引擎的端口：`listens_on` 一律是真。
    fn claiming(port: u16) -> Arc<Self> {
        let scripted = Self::new();
        scripted.claims.lock().unwrap().push(port);
        scripted
    }

    fn external_listener(&self, port: u16, pid: u32, team: Option<&str>) {
        self.listeners.lock().unwrap().insert(port, vec![pid]);
        if let Some(team) = team {
            self.teams.lock().unwrap().insert(pid, team.to_owned());
        }
    }

    fn forget_listener(&self, port: u16) {
        self.listeners.lock().unwrap().remove(&port);
    }
}

#[async_trait]
impl ProcessInspector for Scripted {
    fn pid_state(&self, pid: u32) -> PidState {
        self.real.pid_state(pid)
    }
    async fn start_time(&self, pid: u32) -> Result<String, InspectError> {
        self.real.start_time(pid).await
    }
    async fn command(&self, pid: u32) -> Result<String, InspectError> {
        self.real.command(pid).await
    }
    async fn process_group(&self, pid: u32) -> Result<u32, InspectError> {
        self.real.process_group(pid).await
    }
    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
        if self.claims.lock().unwrap().contains(&port) {
            return Ok(true);
        }
        self.real.listens_on(pid, port).await
    }
    async fn listeners_on(&self, port: u16) -> Result<Vec<u32>, InspectError> {
        Ok(self
            .listeners
            .lock()
            .unwrap()
            .get(&port)
            .cloned()
            .unwrap_or_default())
    }
    async fn uid_of(&self, pid: u32) -> Result<u32, InspectError> {
        self.real.uid_of(pid).await
    }
    async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
        Ok(self.teams.lock().unwrap().get(&pid).cloned())
    }
}

/// 测试自己起的「外部引擎」：假引擎，不经 iDoris。
struct External {
    child: Option<child_guard::ChildGuard>,
    fake: FakeOmlx,
    port: u16,
}

impl External {
    fn start(work: &std::path::Path, behavior: &Behavior) -> Self {
        let dir = work.join("external");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = FakeOmlx::create(&dir, behavior);
        let port = free_port();
        let mut command = Command::new(&fake.executable);
        command
            .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", work)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = spawn_retrying(&mut command).unwrap();
        let deadline = Instant::now() + PATIENT;
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "the external engine never listened"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            child: Some(child),
            fake,
            port,
        }
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
}

impl Drop for External {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------------------------

/// 缩短到毫秒级的时间参数。
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

fn template() -> ComponentCard {
    serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap()
}

/// 托管的 oMLX 卡（on_demand，`idoris/fast` 绑 `fake-model`）。
fn managed_card(endpoint: &str) -> ComponentCard {
    let mut card = template();
    card.endpoint = endpoint.to_owned();
    card.provider.id = "omlx".to_owned();
    card.load_policy.as_mut().unwrap().mode = LoadMode::OnDemand;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_managed": {"engine": "omlx", "mode": "auto"},
            "idoris_role_models": {"fast": "fake-model"}
        }))
        .unwrap(),
    );
    card
}

/// `setup upstream` 生成的那种卡（resident、不托管）。
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

fn omlx_backend(models_dir: &std::path::Path) -> Arc<dyn EngineBackend> {
    let models = models_dir.to_owned();
    Arc::new(
        OmlxBackend::from_env()
            .with_models_dir(move || Ok(models.clone()))
            .with_codesign(CodesignPolicy::Off)
            .with_listener_policy(ListenerPolicy::new(TEAM)),
    )
}

fn upstream_status(id: &str, engine_kind: &str, managed: bool) -> UpstreamStatus {
    UpstreamStatus {
        id: id.to_owned(),
        engine_kind: engine_kind.to_owned(),
        state: UpstreamState::Starting,
        reason: None,
        managed,
        owned: false,
    }
}

/// 一个测试的工作目录、状态目录、假引擎、端口。结束（含 panic）时清掉它拉起过的引擎进程。
struct Rig {
    work: tempfile::TempDir,
    state: tempfile::TempDir,
    fake: FakeOmlx,
    port: u16,
}

impl Rig {
    fn new(behavior: &Behavior) -> Self {
        let work = tempfile::tempdir().unwrap();
        let fake = FakeOmlx::create(work.path(), behavior);
        Self {
            work,
            state: tempfile::tempdir().unwrap(),
            fake,
            port: free_port(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.state.path().join("state")
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn pid(&self, launch: usize) -> u64 {
        self.fake.launch(launch)["pid"].as_u64().unwrap()
    }

    /// 第 `launch` 次拉起的引擎的私有 key（它的环境变量 `OMLX_API_KEY`）。
    fn key(&self, launch: usize) -> String {
        self.fake.launch(launch)["api_key"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn record_count(&self) -> usize {
        std::fs::read_dir(self.state_dir().join("engines-owned/omlx"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        fake_omlx::kill_fake_engines_under(self.work.path());
        for launch in self.fake.all_launches() {
            let pid = launch["pid"].as_u64().unwrap();
            if command_of(pid).contains("fake_omlx.py") {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// 引擎管理器加 Admin Router，门控已经放行（`open_gate`，同 `bin` 在引擎落定之后做的）。没有数据面、没有 Supervisor。
struct World {
    manager: Arc<EngineManager>,
    health: HealthState,
    admin: Router,
    token: String,
    log: Arc<Mutex<Vec<String>>>,
    rig: Rig,
}

enum Engine {
    /// 假引擎，按这个行为。
    Fake(Behavior),
    /// 没装 oMLX：可执行文件的路径不存在。
    Missing,
}

impl World {
    /// 起管理器，等引擎落定，放行门控，建 Admin Router。`inspector` 为 `None` 时自有引擎的端口一律算「监听者是引擎」。
    async fn start(engine: Engine, timings: Timings) -> Self {
        let (rig, executable) = match &engine {
            Engine::Fake(behavior) => {
                let rig = Rig::new(behavior);
                let executable = rig.fake.executable.clone();
                (rig, executable)
            }
            Engine::Missing => {
                let rig = Rig::new(&Behavior::default());
                let executable = rig.work.path().join("no-such-omlx-cli");
                (rig, executable)
            }
        };
        let inspector = Scripted::claiming(rig.port);
        Self::start_with(rig, executable, inspector, timings, true).await
    }

    async fn start_with(
        rig: Rig,
        executable: PathBuf,
        inspector: Arc<dyn ProcessInspector>,
        timings: Timings,
        wait_settled: bool,
    ) -> Self {
        let health = HealthState::with_upstreams(
            0,
            vec![
                upstream_status("omlx", "omlx", true),
                upstream_status("lmstudio", "openai_compat", false),
            ],
        );
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: LogSink = {
            let log = log.clone();
            Arc::new(move |line| log.lock().unwrap().push(line.to_owned()))
        };
        let manager = Arc::new(EngineManager::start(
            ManagerConfig {
                state_dir: rig.state_dir(),
                allocated_budget_bytes: Some(16 * GIB),
                stop_on_exit: StopOnExit::Owned,
                executables: BTreeMap::from([("omlx".to_owned(), executable)]),
                launched_by: "test-instance".to_owned(),
                timings,
                inspector,
                log: sink,
            },
            vec![ManagedPlan {
                id: "omlx".to_owned(),
                endpoint: rig.endpoint(),
                port: rig.port,
                backend: omlx_backend(&rig.work.path().join("models")),
            }],
            health.clone(),
        ));
        if wait_settled {
            tokio::time::timeout(PATIENT, manager.wait_all_settled())
                .await
                .expect("the engine settles");
        }
        let outcome = manager.open_gate().expect("settled: the gate opens");
        health.set_gate(GateState::Open);
        health.apply("lmstudio", Observation::Ready);
        let token = AdminSessionToken::mint();
        let secret = token.expose_secret().to_owned();
        let admin = build_admin_app_with_engines(
            AppState {
                cards: vec![
                    managed_card(&rig.endpoint()),
                    upstream_card("http://127.0.0.1:1234"),
                ],
                health_state: health.clone(),
                ..AppState::default()
            },
            SharedAdminToken::new(token),
            Some(manager.clone()),
        );
        let _ = outcome;
        Self {
            manager,
            health,
            admin,
            token: secret,
            log,
            rig,
        }
    }

    async fn call(&self, method: &str, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = self
            .admin
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get_upstreams(&self) -> Value {
        let (status, body) = self
            .call("GET", "/admin/api/v1/upstreams", Some(&self.token))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            idoris_contracts::parse::<idoris_contracts::admin_v0::AdminUpstreamsResponse>(&body)
                .is_ok(),
            "{body}"
        );
        body
    }

    async fn element(&self, id: &str) -> Value {
        let body = self.get_upstreams().await;
        body["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["id"] == id)
            .unwrap_or_else(|| panic!("no upstream {id} in {body}"))
            .clone()
    }

    async fn post(&self, id: &str, action: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/admin/api/v1/upstreams/{id}/{action}"),
            Some(&self.token),
        )
        .await
    }

    /// 轮询 Admin 的 GET 直到 `done`（返回那一次的元素）。
    async fn until(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + PATIENT;
        loop {
            let element = self.element("omlx").await;
            if done(&element) {
                return element;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {element}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn until_state(&self, state: &str) -> Value {
        self.until(state, |element| element["state"] == state).await
    }

    fn logged(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }
}

fn error_of(body: &Value) -> (&str, &str) {
    (
        body["error"]["type"].as_str().unwrap(),
        body["error"]["reason_code"].as_str().unwrap(),
    )
}

// ---------------------------------------------------------------------------------------------
// 读
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_owned_ready_engine_reads_with_its_pid_and_port_and_the_same_facts_as_health() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let omlx = world.element("omlx").await;
    assert_eq!(
        omlx,
        json!({
            "id": "omlx", "engine_kind": "omlx", "state": "ready", "managed": true, "owned": true,
            "port": world.rig.port, "pid": world.rig.pid(0), "restart_required": false,
            "failure_count": 0
        })
    );
    // 同源同口径：`/health.upstreams` 里同一个元素的六个字段与它一字不差。
    let health = serde_json::to_value(world.health.view().upstreams).unwrap();
    let health_omlx = health
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "omlx")
        .unwrap();
    for key in ["id", "engine_kind", "state", "reason", "managed", "owned"] {
        assert_eq!(omlx.get(key), health_omlx.get(key), "{key}");
    }
    // 别的卡：resident，不托管，端口来自卡，没有 pid 与失败计数。
    assert_eq!(
        world.element("lmstudio").await,
        json!({
            "id": "lmstudio", "engine_kind": "openai_compat", "state": "ready", "managed": false,
            "owned": false, "port": 1234, "restart_required": false
        })
    );
    // 没有任何 key、token、nonce：引擎的私有 key、Admin token 都不在响应里。
    let text = world.get_upstreams().await.to_string();
    assert!(!text.contains(&world.rig.key(0)), "{text}");
    assert!(!text.contains(&world.token));
    for needle in ["key", "token", "nonce", "secret"] {
        assert!(!text.contains(needle), "{needle} in {text}");
    }
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn failures_are_counted_and_dated_and_a_failed_engine_reads_as_such() {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // 引擎一起来就退出：门控放行之前是 `restarting`（退避）、第三次失败（超过上限 2）是 `failed` + `crash_loop`。
    let world = World::start(
        Engine::Fake(Behavior {
            exit_immediately: Some(5),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let failed = world.element("omlx").await;
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["reason"], "crash_loop");
    assert_eq!(failed["owned"], false);
    assert!(failed.get("pid").is_none(), "no process: no pid");
    // 状态进入 `restarting`、`failed` 的次数：两次 `restarting`，之后一次 `failed`。
    assert_eq!(failed["failure_count"], 3, "{failed}");
    let last = failed["last_failure_at_ms"].as_u64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!((started..=now).contains(&last), "{started} <= {last} <= {now}");
    // 放行时它没就绪：冻结，要重启 iDoris。
    assert_eq!(failed["restart_required"], true);
    world.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 写：stop、start、restart（门控放行之后）
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_is_accepted_stops_the_engine_and_is_idempotent() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let pid = world.rig.pid(0);
    assert!(pid_alive(pid));

    let (status, body) = world.post("omlx", "stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["id"], "omlx");
    assert!(
        idoris_contracts::parse::<idoris_contracts::admin_v0::AdminUpstream>(&body).is_ok(),
        "the 202 body is one upstream element: {body}"
    );

    let stopped = world.until_state("stopped").await;
    assert_eq!(stopped["owned"], false);
    assert!(stopped.get("pid").is_none());
    assert!(stopped.get("reason").is_none());
    assert_eq!(stopped["restart_required"], false, "stopped, not frozen");
    assert_eq!(stopped["failure_count"], 0, "a stop is not a failure");
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert_eq!(world.rig.record_count(), 0, "its ownership record is gone");
    assert!(governed_endpoint(&world.rig.endpoint()).is_none());
    assert_eq!(world.health.view().upstreams[0].state, UpstreamState::Stopped);

    // 幂等：已经停下，再 `stop` 仍是 202 加当前状态；没有第二个引擎，没有错误。
    let (status, body) = world.post("omlx", "stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "stopped");
    assert_eq!(world.rig.fake.launches(), 1);
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn after_a_stop_start_brings_a_fresh_engine_back_with_a_new_private_key() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let first = world.rig.pid(0);
    let first_key = world.rig.key(0);
    world.post("omlx", "stop").await;
    world.until_state("stopped").await;

    let (status, body) = world.post("omlx", "start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let ready = world
        .until("a new engine", |element| {
            element["state"] == "ready" && element["pid"] != json!(first)
        })
        .await;
    assert_eq!(ready["owned"], true);
    assert_eq!(world.rig.fake.launches(), 2);
    let second = world.rig.pid(1);
    assert_eq!(ready["pid"], json!(second));
    assert_ne!(first, second);
    // 新引擎换了私有 key，登记的是新的那把；旧的不再留在进程里。
    let second_key = world.rig.key(1);
    assert_ne!(first_key, second_key);
    assert_eq!(
        governed_endpoint(&world.rig.endpoint()),
        Some(EndpointKey::Private(second_key))
    );
    // 一次 stop 加一次 start 不是失败。
    assert_eq!(ready["failure_count"], 0, "{:?}", world.log.lock().unwrap());
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn restart_replaces_the_engine_process_even_though_the_gate_is_open() {
    let world = World::start(
        Engine::Fake(Behavior {
            trap_sigterm_secs: Some(0.0),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let first = world.rig.pid(0);
    let first_key = world.rig.key(0);

    let (status, body) = world.post("omlx", "restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let ready = world
        .until("the replacement engine", |element| {
            element["state"] == "ready" && element["pid"] != json!(first)
        })
        .await;
    assert_eq!(ready["owned"], true);
    assert_eq!(world.rig.fake.launches(), 2);
    wait_for("the old engine to die", || !pid_alive(first)).await;
    assert!(
        world.rig.fake.got_sigterm(first),
        "the old engine was stopped with SIGTERM (the full stop path), not killed"
    );
    assert_ne!(first_key, world.rig.key(1), "a new engine has a new private key");
    assert_eq!(
        governed_endpoint(&world.rig.endpoint()),
        Some(EndpointKey::Private(world.rig.key(1)))
    );
    assert_eq!(world.rig.record_count(), 1, "one record: the new engine's");
    assert!(world.logged("收到命令 restart，已受理") == 1);
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn start_on_a_ready_engine_is_an_idempotent_202_and_launches_nothing() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let (status, body) = world.post("omlx", "start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "ready");
    assert_eq!(body["pid"], json!(world.rig.pid(0)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(world.rig.fake.launches(), 1);
    assert!(pid_alive(world.rig.pid(0)));
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn two_concurrent_stops_are_both_accepted_and_the_engine_is_stopped_once() {
    let world = World::start(
        Engine::Fake(Behavior {
            trap_sigterm_secs: Some(0.5),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let pid = world.rig.pid(0);
    let ((first, _), (second, _)) = tokio::join!(
        world.post("omlx", "stop"),
        world.post("omlx", "stop")
    );
    assert_eq!(first, StatusCode::ACCEPTED);
    assert_eq!(second, StatusCode::ACCEPTED);
    world.until_state("stopped").await;
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert_eq!(world.logged("收到命令 stop，已受理"), 2, "both were accepted");
    assert_eq!(world.logged("：stopped"), 1, "but the engine went down once");
    assert_eq!(world.rig.fake.launches(), 1);
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn two_concurrent_restarts_replace_the_engine_once_and_both_are_accepted() {
    let world = World::start(
        Engine::Fake(Behavior {
            trap_sigterm_secs: Some(0.0),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let first = world.rig.pid(0);
    let ((a, _), (b, _)) = tokio::join!(
        world.post("omlx", "restart"),
        world.post("omlx", "restart")
    );
    assert_eq!((a, b), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    world
        .until("the replacement engine", |element| {
            element["state"] == "ready" && element["pid"] != json!(first)
        })
        .await;
    // 稳住之后再看：只重启了一次（第二个请求发现已经在重启，并入第一个）。
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(world.rig.fake.launches(), 2, "exactly one replacement");
    assert_eq!(world.element("omlx").await["state"], "ready");
    world.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 写：不归 iDoris 的、冻结的、没有鉴权的
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_frozen_upstream_reads_restart_required_and_refuses_start_while_stop_and_restart_are_not_owned()
 {
    let world = World::start(Engine::Missing, fast()).await;
    let omlx = world.element("omlx").await;
    assert_eq!(
        omlx,
        json!({
            "id": "omlx", "engine_kind": "omlx", "state": "absent", "reason": "engine_not_found",
            "managed": true, "owned": false, "port": world.rig.port, "restart_required": true,
            "failure_count": 0
        })
    );
    // `start`：放行时没就绪，没有它的 Supervisor：要重启 iDoris。
    let (status, body) = world.post("omlx", "start").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_of(&body), ("restart_required", "restart_required"));
    // `stop`、`restart`：没有自己拥有的引擎（规范 §2.11.4：`owned` 为 false 一律 `upstream_not_owned`）。
    for action in ["stop", "restart"] {
        let (status, body) = world.post("omlx", action).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}");
        assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"), "{action}");
    }
    assert_eq!(world.rig.fake.launches(), 0, "nothing was launched");
    assert_eq!(world.element("omlx").await["state"], "absent");
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_card_that_is_not_managed_and_an_unknown_id_are_refused_before_anything_happens() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    for action in ["start", "stop", "restart"] {
        let (status, body) = world.post("lmstudio", action).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}");
        assert_eq!(error_of(&body), ("upstream_not_owned", "not_managed"));
        let (status, body) = world.post("nope", action).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{action}");
        assert_eq!(error_of(&body), ("upstream_not_found", "upstream_not_found"));
    }
    assert!(pid_alive(world.rig.pid(0)));
    assert_eq!(world.logged("收到命令"), 0, "the manager never heard of them");
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_unauthenticated_write_does_nothing() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let wrong = AdminSessionToken::mint().expose_secret().to_owned();
    for token in [None, Some(wrong.as_str())] {
        for action in ["start", "stop", "restart"] {
            let (status, _) = world
                .call(
                    "POST",
                    &format!("/admin/api/v1/upstreams/omlx/{action}"),
                    token,
                )
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{action} {token:?}");
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(pid_alive(world.rig.pid(0)));
    assert_eq!(world.element("omlx").await["state"], "ready");
    assert_eq!(world.logged("收到命令"), 0);
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_engine_is_never_signalled_stop_and_restart_are_not_owned_and_start_is_idempotent()
 {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), &Behavior::default());
    let mut rig = Rig::new(&Behavior::default());
    rig.port = external.port;
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let executable = rig.fake.executable.clone();
    let world = World::start_with(rig, executable, inspector, fast(), true).await;

    let omlx = world.element("omlx").await;
    assert_eq!(omlx["state"], "ready");
    assert_eq!(omlx["managed"], true);
    assert_eq!(omlx["owned"], false, "an adopted external engine is not ours");
    assert!(omlx.get("pid").is_none());
    assert_eq!(omlx["restart_required"], false);
    for action in ["stop", "restart"] {
        let (status, body) = world.post("omlx", action).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}");
        assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"));
    }
    // `start` 不要求归属，而且已经就绪：幂等。
    let (status, body) = world.post("omlx", "start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    assert_eq!(world.rig.fake.launches(), 0, "nothing was spawned beside it");
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn the_manager_logs_every_accepted_command_and_never_a_key() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let key = world.rig.key(0);
    world.post("omlx", "stop").await;
    world.until_state("stopped").await;
    assert_eq!(world.logged("引擎 omlx：收到命令 stop，已受理"), 1);
    for line in world.log.lock().unwrap().iter() {
        assert!(!line.contains(&key), "the private key is in a log line: {line}");
        assert!(!line.contains(&world.token), "the admin token is in a log line");
    }
    world.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// G2：私有 key 在进程确认消失之后才撤销；外部引擎的监听者没了，验证立刻撤销
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn g2_the_private_key_stays_registered_until_the_engine_has_exited_and_only_then_is_released() {
    // 引擎收到 SIGTERM 之后再服务一秒才退出：停止的这一秒里它还活着、还在监听。
    let world = World::start(
        Engine::Fake(Behavior {
            trap_sigterm_secs: Some(1.0),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let pid = world.rig.pid(0);
    let key = world.rig.key(0);
    world.manager.stop_upstream("omlx").await.unwrap();
    let mut samples_while_alive = 0;
    let deadline = Instant::now() + PATIENT;
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "the engine never exited");
        if world.rig.fake.got_sigterm(pid) {
            // 引擎已经收到 SIGTERM、还没退出：发往它的请求仍然只用它的私有 key。
            assert_eq!(
                governed_endpoint(&world.rig.endpoint()),
                Some(EndpointKey::Private(key.clone())),
                "released before the process was gone"
            );
            samples_while_alive += 1;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(samples_while_alive > 5, "the window was observed: {samples_while_alive}");
    world.until_state("stopped").await;
    assert!(
        governed_endpoint(&world.rig.endpoint()).is_none(),
        "released once it was gone"
    );
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn g2_an_external_listener_that_disappears_loses_the_settings_key_at_once_and_gets_it_back_when_verified() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), &Behavior::default());
    let mut rig = Rig::new(&Behavior::default());
    rig.port = external.port;
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let executable = rig.fake.executable.clone();
    let world = World::start_with(rig, executable, inspector.clone(), fast(), true).await;
    let endpoint = world.rig.endpoint();
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::External {
            settings_key_verified: true
        })
    );
    // 端口上不再有监听者：`unavailable`，同时 `settings.json` 的 key 立刻不再允许（端口上之后出现的东西还没有通过校验）。
    inspector.forget_listener(external.port);
    world.until_state("unavailable").await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::External {
            settings_key_verified: false
        }),
        "the verification is withdrawn the moment the listener is gone"
    );
    // 监听者回来并通过校验：重新允许。
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    world.until_state("ready").await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::External {
            settings_key_verified: true
        })
    );
    world.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// G3：对还活着的自有引擎 `start`，走完整的停止路径，不丢弃句柄
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn g3_start_on_an_alive_but_unresponsive_owned_engine_stops_it_gracefully_then_launches_a_new_one() {
    // 引擎收到 SIGTERM 就写标记并退出（优雅停止的证据）；SIGKILL（丢弃句柄）不会留下标记。
    let rig = Rig::new(&Behavior {
        trap_sigterm_secs: Some(0.0),
        ..Behavior::default()
    });
    let health = HealthState::with_upstreams(0, vec![upstream_status("omlx", "omlx", true)]);
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let manager = EngineManager::start(
        ManagerConfig {
            state_dir: rig.state_dir(),
            allocated_budget_bytes: Some(16 * GIB),
            stop_on_exit: StopOnExit::Owned,
            executables: BTreeMap::from([("omlx".to_owned(), rig.fake.executable.clone())]),
            launched_by: "test-instance".to_owned(),
            timings: fast(),
            inspector: Scripted::claiming(rig.port),
            log: {
                let log = log.clone();
                Arc::new(move |line| log.lock().unwrap().push(line.to_owned()))
            },
        },
        vec![ManagedPlan {
            id: "omlx".to_owned(),
            endpoint: rig.endpoint(),
            port: rig.port,
            backend: omlx_backend(&rig.work.path().join("models")),
        }],
        health.clone(),
    );
    let until = |state: UpstreamState| {
        let health = health.clone();
        async move {
            wait_for("the state", || health.view().upstreams[0].state == state).await;
        }
    };
    until(UpstreamState::Ready).await;
    let first = rig.pid(0);
    let first_key = rig.key(0);
    // 活着、监听着、但「不答」：探测拿到 503，状态 `unavailable`，进程还在，句柄还在。
    rig.fake.force_status(503);
    until(UpstreamState::Unavailable).await;
    assert!(health.view().upstreams[0].owned, "alive: still ours");
    assert!(pid_alive(first));
    // 引擎修好之后的新进程要正常答：先取消强制状态（对之后拉起的新进程也生效）。
    manager.start_upstream("omlx").await.unwrap();
    rig.fake.answer_normally();
    wait_for("the old engine to die", || !pid_alive(first)).await;
    assert!(
        rig.fake.got_sigterm(first),
        "stopped with SIGTERM (the full stop path), not killed by dropping the handle"
    );
    wait_for("a second launch", || rig.fake.launches() == 2).await;
    until(UpstreamState::Ready).await;
    assert!(health.view().upstreams[0].owned);
    // 旧的私有 key 撤销了，登记的是新引擎的。
    assert_ne!(first_key, rig.key(1));
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Private(rig.key(1)))
    );
    assert_eq!(rig.record_count(), 1);
    manager.shutdown(true).await;
    drop(log);
}

// ---------------------------------------------------------------------------------------------
// B：带真 Router 与真 Supervisor 的（`BoundSupervisor::spawn_omlx`）：账本复位、数据面的实时门
// ---------------------------------------------------------------------------------------------

/// 管理器加门控任务加真正的 Router 与 Admin（和 `bin` 同一套接线）：门控放行时建 Supervisor 表，数据面与 Admin 换成完整的 Router。
struct Full {
    manager: Arc<EngineManager>,
    health: HealthState,
    data: SwapApp,
    admin: SwapApp,
    token: String,
    _task: gate::GateTask,
    rig: Rig,
}

impl Full {
    async fn start(rig: Rig, inspector: Arc<dyn ProcessInspector>) -> Self {
        let executable = rig.fake.executable.clone();
        let health = HealthState::with_upstreams(0, vec![upstream_status("omlx", "omlx", true)]);
        let manager = Arc::new(EngineManager::start(
            ManagerConfig {
                state_dir: rig.state_dir(),
                allocated_budget_bytes: Some(16 * GIB),
                stop_on_exit: StopOnExit::Owned,
                executables: BTreeMap::from([("omlx".to_owned(), executable)]),
                launched_by: "test-instance".to_owned(),
                timings: fast(),
                inspector,
                log: Arc::new(|_| {}),
            },
            vec![ManagedPlan {
                id: "omlx".to_owned(),
                endpoint: rig.endpoint(),
                port: rig.port,
                backend: omlx_backend(&rig.work.path().join("models")),
            }],
            health.clone(),
        ));
        let cards = vec![managed_card(&rig.endpoint())];
        let token = AdminSessionToken::mint();
        let secret = token.expose_secret().to_owned();
        let shared = SharedAdminToken::new(token);
        let data = SwapApp::new(idoris_router::build_starting_app(AppState {
            health_state: health.clone(),
            ..AppState::default()
        }));
        let admin = SwapApp::new(Router::new());
        let task = gate::spawn(
            manager.clone(),
            health.clone(),
            data.clone(),
            admin.clone(),
            {
                let (health, cards, manager, shared) =
                    (health.clone(), cards, manager.clone(), shared);
                Box::new(move |outcome| {
                    let runtime_cards: Vec<_> = cards
                        .iter()
                        .filter(|card| !outcome.degraded.contains_key(&card.provider.id))
                        .cloned()
                        .collect();
                    let runtimes = RuntimeRegistry::spawn_with_local_configs_budget_and_kill_switch(
                        &runtime_cards,
                        &BTreeMap::new(),
                        Some(16 * GIB),
                        false,
                    )?;
                    let state = AppState {
                        cards,
                        runtimes: runtimes.clone(),
                        health_state: health,
                        ..AppState::default()
                    };
                    Ok(Finalized {
                        data: build_app(state.clone()),
                        admin: build_admin_app_with_engines(state, shared, Some(manager)),
                        runtimes,
                    })
                })
            },
            tokio_util::sync::CancellationToken::new(),
        );
        Self {
            manager,
            health,
            data,
            admin,
            token: secret,
            _task: task,
            rig,
        }
    }

    async fn wait_open(&self) {
        wait_for("the gate to open", || {
            self.health.gate() == GateState::Open
        })
        .await;
    }

    async fn data(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("X-iDoris-Privacy", "local_only");
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn chat(&self) -> (StatusCode, Value) {
        self.data(
            "POST",
            "/v1/chat/completions",
            Some(json!({"model": "idoris/fast", "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await
    }

    async fn admin(&self, method: &str, path: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {}", self.token))
            .body(Body::empty())
            .unwrap();
        let response = self.admin.router().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn runtime(&self) -> Value {
        let (status, body) = self.admin("GET", "/admin/api/v1/runtimes").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["runtimes"][0].clone()
    }

    async fn stop(self) {
        self.manager.shutdown(true).await;
    }
}

fn stateful() -> Behavior {
    Behavior {
        stateful_models: true,
        ..Behavior::default()
    }
}

#[tokio::test]
async fn b_after_a_restart_the_supervisors_ledger_is_rebuilt_and_the_next_chat_loads_the_model_again()
 {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;

    // 第一个聊天：Supervisor 对（空的）新引擎加载模型，聊天 200。
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "hello from the fake engine");
    assert_eq!(full.rig.fake.model_loads(), 1);
    let before = full.runtime().await;
    assert_eq!(before["loaded"], json!(["fake-model"]));
    let first = full.rig.pid(0);

    // Admin 重启：新进程，没有任何模型。Supervisor 的账本若还以为 `fake-model` 已加载，聊天就会白白发给一个没有它的引擎（假引擎对
    // 没加载的模型回 404 → 503）。
    let (status, body) = full.admin("POST", "/admin/api/v1/upstreams/omlx/restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the replacement engine", || {
        let view = full.health.view();
        full.rig.fake.launches() == 2 && view.upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_ne!(full.rig.pid(1), first);

    // 复位之后下一次请求重新加载：新引擎真的收到了 `load`，聊天 200，账本与新进程一致。
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "hello from the fake engine");
    assert_eq!(full.rig.fake.model_loads(), 2, "the new process was told to load it");
    let after = full.runtime().await;
    assert_eq!(after["state"], "observed");
    assert_eq!(after["loaded"], json!(["fake-model"]));
    assert_eq!(
        after["used_gb"], before["used_gb"],
        "the same one model: counted once, not twice"
    );
    full.stop().await;
}

#[tokio::test]
async fn b_a_stopped_engine_is_a_503_with_a_reason_and_nothing_reaches_it_and_start_brings_chat_back() {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    full.admin("POST", "/admin/api/v1/upstreams/omlx/stop").await;
    wait_for("stopped", || {
        full.health.view().upstreams[0].state == UpstreamState::Stopped
    })
    .await;
    // 数据面读实时状态：路由到这张卡的请求不碰引擎，直接 503，原因码沿用 `supervisor_unavailable`（被停下没有稳定原因码）。
    let requests_before = full.rig.fake.requests().len();
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable");
    assert_eq!(body["error"]["reason_code"], "supervisor_unavailable");
    assert_eq!(full.rig.fake.requests().len(), requests_before, "nothing reached the engine");
    // 账本也跟着清空了（引擎已经没有了）：Admin 的 runtimes 不再说有模型加载着。
    let runtime = full.runtime().await;
    assert_eq!(runtime["loaded"], json!([]));
    assert_eq!(runtime["used_gb"], 0.0);

    // `start`：新引擎，账本复位，聊天回来。
    let (status, body) = full.admin("POST", "/admin/api/v1/upstreams/omlx/start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(full.rig.fake.model_loads(), 2);
    full.stop().await;
}

#[tokio::test]
async fn b_g1_a_card_whose_port_changed_owner_is_a_503_at_run_time_and_nothing_is_sent_to_it() {
    // 接管的外部引擎（有状态的假引擎）：聊天通过 Supervisor 与它的适配器正常工作。
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), &stateful());
    let mut rig = Rig::new(&Behavior::default());
    rig.port = external.port;
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(full.rig.fake.launches(), 0, "adopted, not spawned");

    // 端口换了主人：另一个进程（借测试进程自己：同一个用户，没有签名团队）。管理器标 `failed` + `port_in_use`……
    inspector.external_listener(external.port, std::process::id(), None);
    wait_for("failed/port_in_use", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Failed && upstream.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    // ……而 Router 已经创建了、它的 Supervisor 与适配器早就存在：请求在运行期被挡住，503 带原因，没有一个字节发给那个端口。
    let before = external.fake.requests().len();
    for _ in 0..3 {
        let (status, body) = full.chat().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["type"], "local_only_unavailable");
        assert_eq!(body["error"]["reason_code"], "port_in_use");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        external.fake.requests().len(),
        before,
        "no request, with or without a key, reached the port that changed hands"
    );
    // 它又通过校验：恢复服务（同一个引擎进程，账本没有过期）。
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    full.stop().await;
}
