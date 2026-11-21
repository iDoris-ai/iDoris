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
use fake_omlx::{Behavior, FakeOmlx, Hold, spawn_retrying};
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
use idoris_upstream::omlx::key::{EndpointKey, endpoint_generation, governed_endpoint};
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
    /// `start_time(pid)`：编排过的 pid 报这个启动时间（造「pid 被回收给了另一个进程」），没编排的读真的 `ps`。
    start_times: Mutex<BTreeMap<u32, String>>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            real: SystemInspector::new(),
            claims: Mutex::new(Vec::new()),
            listeners: Mutex::new(BTreeMap::new()),
            teams: Mutex::new(BTreeMap::new()),
            start_times: Mutex::new(BTreeMap::new()),
        })
    }

    fn pretend_start_time(&self, pid: u32, start_time: &str) {
        self.start_times
            .lock()
            .unwrap()
            .insert(pid, start_time.to_owned());
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
        if let Some(start_time) = self.start_times.lock().unwrap().get(&pid) {
            return Ok(start_time.clone());
        }
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
        Self::start_on(work, "external", behavior, free_port())
    }

    /// 同 [`Self::start`]，目录名与端口由调用方给：同一个端口上先后起两个不同的外部引擎（A 退出、B 起来）。
    fn start_on(work: &std::path::Path, name: &str, behavior: &Behavior, port: u16) -> Self {
        let dir = work.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = FakeOmlx::create(&dir, behavior);
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

    /// 引擎退出（用户把它关了）：杀掉并收尸，端口随即空出来。
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        drain_timeout: Duration::from_secs(5),
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

    /// 停止做完了：`stopped` 而且 `owned: false`。受理时状态就已经是 `stopped`（不可服务），进程还在退出的那几秒 `owned` 还是
    /// true、`pid` 还在；进程没了、账本清空之后才是 `owned: false`。
    async fn until_gone(&self) -> Value {
        self.until("the engine to be gone", |element| {
            element["state"] == "stopped" && element["owned"] == false
        })
        .await
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

/// 托管上游的元素里已经累计的失败次数。
fn failures_of(element: &Value) -> u64 {
    element["failure_count"].as_u64().unwrap()
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
    // 精确的：一次顺利的首次启动没有任何失败（`failure_count` 为 0，没有 `last_failure_at_ms`）。启动前对端口的探测答不上来、
    // 或者被拒绝，都不是引擎失败，不能算进来（Codex 预审 6；`startup_probe_*` 钉住了每一种探测序列）。
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
    assert!(
        (started..=now).contains(&last),
        "{started} <= {last} <= {now}"
    );
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

    // 受理的那一刻状态已经是 `stopped`（不可服务）；进程是不是已经没了看 `owned` 与 `pid`。
    assert_eq!(body["state"], "stopped", "{body}");
    let stopped = world.until_gone().await;
    assert_eq!(stopped["owned"], false);
    assert!(stopped.get("pid").is_none());
    assert!(stopped.get("reason").is_none());
    assert_eq!(stopped["restart_required"], false, "stopped, not frozen");
    assert_eq!(stopped["failure_count"], 0, "a stop is not a failure");
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert_eq!(world.rig.record_count(), 0, "its ownership record is gone");
    // 端点被封上（什么凭证都不发），不是撤销登记（Codex 预审 3）。
    assert_eq!(
        governed_endpoint(&world.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    assert_eq!(
        world.health.view().upstreams[0].state,
        UpstreamState::Stopped
    );

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
    world.until_gone().await;

    let (status, body) = world.post("omlx", "start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // 202 的体是受理之后的元素，不能还说 `stopped`。
    assert_ne!(body["state"], "stopped", "{body}");
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
    // 202 的体是受理之后的元素：旧引擎还在被停掉，它已经不是「就绪的旧 pid」（要么是 `starting`，要么快到新引擎已经就绪）。
    assert!(
        !(body["state"] == "ready" && body["pid"] == json!(first)),
        "the 202 body still shows the engine that is being replaced: {body}"
    );
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
    assert_ne!(
        first_key,
        world.rig.key(1),
        "a new engine has a new private key"
    );
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
    let ((first, _), (second, _)) =
        tokio::join!(world.post("omlx", "stop"), world.post("omlx", "stop"));
    assert_eq!(first, StatusCode::ACCEPTED);
    assert_eq!(second, StatusCode::ACCEPTED);
    world.until_gone().await;
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert_eq!(
        world.logged("收到命令 stop，已受理"),
        2,
        "both were accepted"
    );
    assert_eq!(
        world.logged("：stopped"),
        1,
        "but the engine went down once"
    );
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
    let ((a, _), (b, _)) =
        tokio::join!(world.post("omlx", "restart"), world.post("omlx", "restart"));
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
        assert_eq!(
            error_of(&body),
            ("upstream_not_owned", "not_owned"),
            "{action}"
        );
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
        assert_eq!(
            error_of(&body),
            ("upstream_not_found", "upstream_not_found")
        );
    }
    assert!(pid_alive(world.rig.pid(0)));
    assert_eq!(
        world.logged("收到命令"),
        0,
        "the manager never heard of them"
    );
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
    assert_eq!(
        omlx["owned"], false,
        "an adopted external engine is not ours"
    );
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
    assert_eq!(
        world.rig.fake.launches(),
        0,
        "nothing was spawned beside it"
    );
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn the_manager_logs_every_accepted_command_and_never_a_key() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let key = world.rig.key(0);
    world.post("omlx", "stop").await;
    world.until_gone().await;
    assert_eq!(world.logged("引擎 omlx：收到命令 stop，已受理"), 1);
    for line in world.log.lock().unwrap().iter() {
        assert!(
            !line.contains(&key),
            "the private key is in a log line: {line}"
        );
        assert!(
            !line.contains(&world.token),
            "the admin token is in a log line"
        );
    }
    world.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// G2：私有 key 在进程确认消失之后才撤销；外部引擎的监听者没了，验证立刻撤销
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn g2_the_private_key_stays_registered_until_the_engine_has_exited_and_only_then_the_endpoint_is_blocked()
 {
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
                "replaced before the process was gone"
            );
            samples_while_alive += 1;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        samples_while_alive > 5,
        "the window was observed: {samples_while_alive}"
    );
    world.until_gone().await;
    assert_eq!(
        governed_endpoint(&world.rig.endpoint()),
        Some(EndpointKey::Blocked),
        "blocked (not released) once it was gone"
    );
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn g2_an_external_listener_that_disappears_loses_the_settings_key_at_once_and_gets_it_back_when_verified()
 {
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
    // 端口上不再有监听者：`unavailable`，同时什么凭证都立刻不再发（端口上之后出现的东西还没有通过校验；连环境变量的 key 也不发）。
    inspector.forget_listener(external.port);
    world.until_state("unavailable").await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "no credential goes out the moment the listener is gone"
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
async fn g3_start_on_an_alive_but_unresponsive_owned_engine_stops_it_gracefully_then_launches_a_new_one()
 {
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
    log: Arc<Mutex<Vec<String>>>,
}

impl Full {
    async fn start(rig: Rig, inspector: Arc<dyn ProcessInspector>) -> Self {
        Self::start_with(rig, inspector, fast()).await
    }

    async fn start_with(rig: Rig, inspector: Arc<dyn ProcessInspector>, timings: Timings) -> Self {
        let executable = rig.fake.executable.clone();
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: LogSink = {
            let log = log.clone();
            Arc::new(move |line| log.lock().unwrap().push(line.to_owned()))
        };
        let health = HealthState::with_upstreams(0, vec![upstream_status("omlx", "omlx", true)]);
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
        let cards = vec![managed_card(&rig.endpoint())];
        let state_dir = rig.state_dir();
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
                    // The adapter's durable load fence must be the very file the engine manager clears when it
                    // spawns a replacement (the production default resolves both from the same state root;
                    // this world has a state root of its own, so the adapter is told where it is).
                    let runtimes = RuntimeRegistry::spawn_with_factory_budget_and_kill_switch(
                        &runtime_cards,
                        Some(16 * GIB),
                        false,
                        |card| {
                            let fence = idoris_upstream::omlx::load_fence_path_in(
                                &state_dir,
                                &card.endpoint,
                            )?;
                            let adapter = idoris_upstream::OmlxAdapter::new(
                                idoris_upstream::OmlxAdapterConfig::for_base_url(
                                    card.endpoint.clone(),
                                ),
                            )?
                            .with_per_request_key()
                            .with_load_fence_path(fence);
                            Ok(Arc::new(adapter) as Arc<dyn idoris_backend::RuntimeAdapter>)
                        },
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
            log,
        }
    }

    fn logged(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }

    /// Admin 的 `GET upstreams` 里的一个元素。
    async fn element(&self, id: &str) -> Value {
        let (status, body) = self.admin("GET", "/admin/api/v1/upstreams").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|element| element["id"] == id)
            .unwrap_or_else(|| panic!("no upstream {id} in {body}"))
            .clone()
    }

    /// 轮询 `omlx` 的元素直到 `done`（返回那一次的元素）。
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

    /// 停止做完了：`stopped` 而且 `owned: false`。
    async fn until_gone(&self) -> Value {
        self.until("the engine to be gone", |element| {
            element["state"] == "stopped" && element["owned"] == false
        })
        .await
    }

    async fn until_unavailable(&self) -> Value {
        self.until("unavailable", |element| element["state"] == "unavailable")
            .await
    }

    async fn wait_open(&self) {
        wait_for("the gate to open", || self.health.gate() == GateState::Open).await;
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
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from the fake engine"
    );
    assert_eq!(full.rig.fake.model_loads(), 1);
    let before = full.runtime().await;
    assert_eq!(before["loaded"], json!(["fake-model"]));
    let first = full.rig.pid(0);

    // Admin 重启：新进程，没有任何模型。Supervisor 的账本若还以为 `fake-model` 已加载，聊天就会白白发给一个没有它的引擎（假引擎对
    // 没加载的模型回 404 → 503）。
    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
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
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from the fake engine"
    );
    assert_eq!(
        full.rig.fake.model_loads(),
        2,
        "the new process was told to load it"
    );
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
async fn b_a_stopped_engine_is_a_503_with_a_reason_and_nothing_reaches_it_and_start_brings_chat_back()
 {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    full.admin("POST", "/admin/api/v1/upstreams/omlx/stop")
        .await;
    wait_for("the engine to be gone", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Stopped && !upstream.owned
    })
    .await;
    // 数据面读实时状态：路由到这张卡的请求不碰引擎，直接 503，原因码沿用 `supervisor_unavailable`（被停下没有稳定原因码）。
    let requests_before = full.rig.fake.requests().len();
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable");
    assert_eq!(body["error"]["reason_code"], "supervisor_unavailable");
    assert_eq!(
        full.rig.fake.requests().len(),
        requests_before,
        "nothing reached the engine"
    );
    // 账本也跟着清空了（引擎已经没有了）：Admin 的 runtimes 不再说有模型加载着。
    let runtime = full.runtime().await;
    assert_eq!(runtime["loaded"], json!([]));
    assert_eq!(runtime["used_gb"], 0.0);

    // `start`：新引擎，账本复位，聊天回来。
    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/start")
        .await;
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
        upstream.state == UpstreamState::Failed
            && upstream.reason == Some(UpstreamReason::PortInUse)
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

// ---------------------------------------------------------------------------------------------
// 拉起时的一行日志：引擎改写自己的进程标题，`ps` 里看不到这些值
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_launch_is_logged_in_one_line_with_the_values_ps_cannot_show_and_no_key() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    let launch_lines = |world: &World| -> Vec<String> {
        world
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("拉起引擎："))
            .cloned()
            .collect()
    };
    let lines = launch_lines(&world);
    assert_eq!(lines.len(), 1, "one line per launch: {lines:?}");
    let line = &lines[0];
    assert!(
        line.starts_with("[idoris] 引擎 omlx：拉起引擎：program="),
        "{line}"
    );
    // 可执行文件是发现并校验之后的真实路径（`canonicalize`：macOS 的临时目录经符号链接）。
    let program = std::fs::canonicalize(&world.rig.fake.executable).unwrap();
    for expected in [
        format!("program={:?}", program.display().to_string()),
        format!(" port={} ", world.rig.port),
        // 额度 16 GiB：传给引擎的内存护栏。
        " memory_guard=16 GiB ".to_owned(),
        format!(
            "model_dir={:?}",
            world.rig.work.path().join("models").display().to_string()
        ),
        format!(
            "base={:?}",
            world
                .rig
                .state_dir()
                .join("omlx-base")
                .display()
                .to_string()
        ),
    ] {
        assert!(line.contains(&expected), "{expected} missing from {line}");
    }
    // 没有 key，也没有长得像 key 的东西（私有 key 是 64 位十六进制）。
    let key = world.rig.key(0);
    assert!(!line.contains(&key), "{line}");
    assert!(!line.contains("OMLX_API_KEY"), "{line}");
    assert!(
        !line
            .as_bytes()
            .windows(32)
            .any(|window| window.iter().all(u8::is_ascii_hexdigit)),
        "a key-shaped run in {line}"
    );

    // 每拉起一次写一行：重启之后是第二行（新的引擎，新的 key，同样不在里面）。
    let first = world.rig.pid(0);
    world.post("omlx", "restart").await;
    world
        .until("the replacement engine", |element| {
            element["state"] == "ready" && element["pid"] != json!(first)
        })
        .await;
    let lines = launch_lines(&world);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let second_key = world.rig.key(1);
    for line in &lines {
        assert!(
            !line.contains(&key) && !line.contains(&second_key),
            "{line}"
        );
    }
    world.manager.shutdown(true).await;
}

#[tokio::test]
async fn b_a_reset_that_cannot_sample_the_new_engine_keeps_it_out_of_service_until_it_can() {
    // 第二个起的引擎进程能答 `/v1/models`，但 `/api/status` 回 500：Supervisor 采样不到它，账本复位不了。
    let rig = Rig::new(&Behavior {
        stateful_models: true,
        api_status_fails_from_launch: Some(1),
        ..Behavior::default()
    });
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // 一个账本没有对上新进程的引擎不放行：停掉，按一次失败算，退避重来；一直复位不了就熔断（上限 2，第三次是 `failed`）。
    wait_for("the breaker", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Failed
            && upstream.reason == Some(UpstreamReason::CrashLoop)
    })
    .await;
    assert!(
        !full.health.view().upstreams[0].owned,
        "it never held an engine that was let through"
    );
    // 一直没放行：数据面 503，每个被拒绝的引擎都停掉了，Admin 读到的账本是粘滞着采样错误的（`error`）。
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["reason_code"], "crash_loop");
    for launch in full.rig.fake.all_launches().iter().skip(1) {
        assert!(
            !pid_alive(launch["pid"].as_u64().unwrap()),
            "an engine the Supervisor could not be synced with was left running"
        );
    }
    assert_eq!(full.runtime().await["state"], "error");
    let (status, body) = full.admin("GET", "/admin/api/v1/upstreams").await;
    assert_eq!(status, StatusCode::OK);
    let element = &body["upstreams"][0];
    assert_eq!(
        element["restart_required"], false,
        "not frozen: it can come back"
    );
    assert!(element["failure_count"].as_u64().unwrap() >= 3, "{element}");

    // 引擎「修好」了（之后拉起的进程 `/api/status` 正常）：下一次半开重试复位成功，放行，聊天回来，账本也恢复。
    full.rig.fake.set_behavior(&Behavior {
        stateful_models: true,
        ..Behavior::default()
    });
    wait_for("recovery through a half-open retry", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(full.runtime().await["state"], "observed");
    full.stop().await;
}

#[tokio::test]
async fn b_start_that_re_adopts_the_very_same_process_keeps_the_supervisors_ledger() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), &stateful());
    let mut rig = Rig::new(&Behavior::default());
    rig.port = external.port;
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let full = Full::start(rig, inspector).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(external.fake.model_loads(), 1);

    // 外部引擎活着却不答了：`unavailable`。操作员按 `start`：重新走启动决策，认回的是同一个进程。
    external.fake.force_status(503);
    wait_for("unavailable", || {
        full.health.view().upstreams[0].state == UpstreamState::Unavailable
    })
    .await;
    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/start")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    external.fake.answer_normally();
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 同一个进程：Supervisor 的账本没有过期，不复位。模型还在里面，聊天不再发 `load`；复位的话账本被忘掉，聊天会重新加载（`load` 两次）。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(
        external.fake.model_loads(),
        1,
        "the ledger was reset although the engine process never changed"
    );
    assert_eq!(full.runtime().await["loaded"], json!(["fake-model"]));
    full.stop().await;
}

#[tokio::test]
async fn after_the_manager_has_shut_down_a_command_is_a_503_idoris_stopping_not_a_hang_or_a_lie() {
    let world = World::start(Engine::Fake(Behavior::default()), fast()).await;
    world.manager.shutdown(true).await;
    // 状态快照还说 `ready`（实例任务收尾时不再发布），管理器已经不收命令了。
    for action in ["start", "stop"] {
        let (status, body) = world.post("omlx", action).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{action}: {body}");
        assert_eq!(error_of(&body), ("idoris_stopping", "idoris_stopping"));
    }
}

#[tokio::test]
async fn g2_shutdown_also_keeps_the_private_key_until_the_engine_has_exited() {
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
    let manager = world.manager.clone();
    let shutdown = tokio::spawn(async move { manager.shutdown(true).await });
    let mut samples_while_alive = 0;
    let deadline = Instant::now() + PATIENT;
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "the engine never exited");
        if world.rig.fake.got_sigterm(pid) {
            assert_eq!(
                governed_endpoint(&world.rig.endpoint()),
                Some(EndpointKey::Private(key.clone())),
                "released before the process was gone"
            );
            samples_while_alive += 1;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(samples_while_alive > 5, "{samples_while_alive}");
    shutdown.await.unwrap();
    assert_eq!(
        governed_endpoint(&world.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
}

/// 一个检查器，可以让「读进程的启动时间」失败：被认回的孤儿引擎的身份读不出来，停止就没有确认它消失（不发信号）。
struct Unreadable {
    inner: Arc<Scripted>,
    unreadable: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl ProcessInspector for Unreadable {
    fn pid_state(&self, pid: u32) -> PidState {
        self.inner.pid_state(pid)
    }
    async fn start_time(&self, pid: u32) -> Result<String, InspectError> {
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(InspectError::NoAnswer);
        }
        self.inner.start_time(pid).await
    }
    async fn command(&self, pid: u32) -> Result<String, InspectError> {
        self.inner.command(pid).await
    }
    async fn process_group(&self, pid: u32) -> Result<u32, InspectError> {
        self.inner.process_group(pid).await
    }
    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
        self.inner.listens_on(pid, port).await
    }
    async fn listeners_on(&self, port: u16) -> Result<Vec<u32>, InspectError> {
        self.inner.listeners_on(port).await
    }
    async fn uid_of(&self, pid: u32) -> Result<u32, InspectError> {
        self.inner.uid_of(pid).await
    }
    async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
        self.inner.signing_team(pid).await
    }
}

fn start_manager(
    rig: &Rig,
    inspector: Arc<dyn ProcessInspector>,
    stop_on_exit: StopOnExit,
) -> (Arc<EngineManager>, HealthState) {
    let health = HealthState::with_upstreams(0, vec![upstream_status("omlx", "omlx", true)]);
    let manager = Arc::new(EngineManager::start(
        ManagerConfig {
            state_dir: rig.state_dir(),
            allocated_budget_bytes: Some(16 * GIB),
            stop_on_exit,
            executables: BTreeMap::from([("omlx".to_owned(), rig.fake.executable.clone())]),
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
    (manager, health)
}

#[tokio::test]
async fn g2_a_stop_that_cannot_confirm_the_process_is_gone_never_hands_the_endpoint_back_to_the_default_key_order()
 {
    let rig = Rig::new(&Behavior::default());
    let scripted = Scripted::claiming(rig.port);
    // 先留一个孤儿引擎（`stop_on_exit:"never"` 的正常退出）：之后的管理器认回它，句柄是「认回的」，停它要先读它的身份。
    let (first, first_health) = start_manager(&rig, scripted.clone(), StopOnExit::Never);
    wait_for("ready", || {
        first_health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    first.shutdown(false).await;
    let pid = rig.pid(0);
    let key = rig.key(0);
    assert!(pid_alive(pid), "never: the engine is left running");

    let inspector = Arc::new(Unreadable {
        inner: scripted,
        unreadable: std::sync::atomic::AtomicBool::new(false),
    });
    let (manager, health) = start_manager(&rig, inspector.clone(), StopOnExit::Owned);
    wait_for("adopted", || {
        let upstream = &health.view().upstreams[0];
        upstream.state == UpstreamState::Ready && upstream.owned
    })
    .await;
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Private(key))
    );

    // 引擎的身份读不出来了：停止不发信号，没有确认它消失。受理时状态已经是 `stopped`（不可服务）；没能确认进程消失，就不能说它
    // 停了：收尾是 `unavailable` + `port_in_use`（进程可能还占着端口），围栏不动。
    inspector
        .unreadable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    manager.stop_upstream("omlx").await.unwrap();
    wait_for("the unconfirmed stop to be reported", || {
        let upstream = &health.view().upstreams[0];
        upstream.state == UpstreamState::Unavailable
            && upstream.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert!(
        pid_alive(pid),
        "an engine whose identity cannot be read is never signalled"
    );
    // 进程可能还在：登记不能撤销（那会按默认顺序把 `settings.json` 的 key 发给它）；私有 key 也不再发：端点封上，什么都不发。
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    inspector
        .unreadable
        .store(false, std::sync::atomic::Ordering::SeqCst);
    manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 预审（IDORIS-CORE S6）：关门、排空、停进程、封上、复位、开门
// ---------------------------------------------------------------------------------------------

/// 有状态的假引擎，收到 SIGTERM 立刻退出但会留下标记（证明走的是优雅停止、什么时候停的）。
fn stateful_with_term_marker() -> Behavior {
    Behavior {
        trap_sigterm_secs: Some(0.0),
        ..stateful()
    }
}

/// 此刻 Supervisor 的账本里有哪些模型（Admin `runtimes`）。
async fn loaded_models(full: &Full) -> Value {
    full.runtime().await["loaded"].clone()
}

#[tokio::test]
async fn c1_a_restart_closes_the_door_then_waits_for_the_chat_in_flight_and_only_then_stops_the_old_engine()
 {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let old = full.rig.pid(0);

    // 一个聊天已经到了引擎、还没答。
    full.rig.fake.hold(Hold::Chat);
    let chats = full.rig.fake.requests_of("POST:/v1/chat/completions");
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the chat to reach the engine", || {
        full.rig.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;

    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // 受理之后：不可服务（`restarting`），老引擎还握在手里（排空还没完）。
    assert_eq!(body["state"], "restarting", "{body}");
    assert_eq!(body["owned"], true, "{body}");
    assert_eq!(body["pid"], json!(old), "{body}");

    // 门关了：新的聊天立刻 503，没有一个字节到引擎。
    let requests = full.rig.fake.requests().len();
    let (status, _) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(full.rig.fake.requests().len(), requests);
    // 排空期间：老引擎活着、没收到 SIGTERM，也没有第二个引擎。
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(pid_alive(old));
    assert!(!full.rig.fake.got_sigterm(old), "stopped before it drained");
    assert_eq!(full.rig.fake.launches(), 1);

    // 放开：在途的聊天用它准入时的凭证在老引擎上做完，**成功**；老引擎这时才被停掉，新引擎换上。
    full.rig.fake.release(Hold::Chat);
    let (status, body) = in_flight.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for("the old engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_eq!(full.rig.fake.launches(), 2);
    assert_eq!(
        loaded_models(&full).await,
        json!([]),
        "the ledger was rebuilt"
    );
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(full.rig.fake.model_loads(), 2, "reloaded on the new engine");
    // 排空等的是真事：日志里没有「排空到点」。
    assert_eq!(full.logged("排空的上限到了"), 0);
}

#[tokio::test]
async fn c2_a_drain_that_runs_out_cuts_the_old_work_off_and_the_engine_is_still_replaced() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let mut timings = fast();
    timings.drain_timeout = Duration::from_millis(400);
    let full = Arc::new(Full::start_with(rig, Scripted::claiming(port), timings).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let old = full.rig.pid(0);

    // 一个永远不答的聊天。
    full.rig.fake.hold(Hold::Chat);
    let chats = full.rig.fake.requests_of("POST:/v1/chat/completions");
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the chat to reach the engine", || {
        full.rig.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;

    let generation = endpoint_generation(&endpoint);
    let started = Instant::now();
    let (status, _) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_for("the old engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "the drain was waited for: {:?}",
        started.elapsed()
    );
    // 到点了：旧一代的凭证被切断（代际前进）才停的进程。
    assert!(endpoint_generation(&endpoint) > generation);
    assert_eq!(full.logged("排空的上限到了"), 1);

    // 被切断的聊天失败（引擎被停了），不是挂着；新引擎照样换上、账本重建、聊天回来。
    let (status, _) = in_flight.await.unwrap();
    assert_ne!(status, StatusCode::OK);
    full.rig.fake.release(Hold::Chat);
    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_eq!(full.rig.fake.launches(), 2);
    assert_eq!(loaded_models(&full).await, json!([]));
    assert_eq!(full.chat().await.0, StatusCode::OK);
}

#[tokio::test]
async fn c3_a_load_in_flight_when_the_engine_is_replaced_never_reaches_the_new_engine() {
    // Codex 预审 1 的场景落到真引擎上：旧的加载已经准入（围栏已写）、已经发给旧引擎、还在加载；引擎被换掉。
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let mut timings = fast();
    timings.drain_timeout = Duration::from_millis(300);
    let full = Arc::new(Full::start_with(rig, Scripted::claiming(port), timings).await);
    full.wait_open().await;
    let old = full.rig.pid(0);

    full.rig.fake.hold(Hold::Load);
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the load to reach the old engine", || {
        full.rig.fake.model_loads() == 1
    })
    .await;

    let (status, _) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_for("the old engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    // 加载随旧引擎一起死了，聊天（等它的）失败。
    let (status, _) = in_flight.await.unwrap();
    assert_ne!(status, StatusCode::OK);
    full.rig.fake.release(Hold::Load);

    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 一个字节的加载也没有发给新引擎：整个引擎的历史里只有旧引擎收到的那一条。新进程里什么都没有，账本也是空的。
    assert_eq!(full.rig.fake.model_loads(), 1);
    assert_eq!(loaded_models(&full).await, json!([]));
    // 之后的请求才在新引擎上加载（带着新一代的凭证）。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(full.rig.fake.model_loads(), 2);
}

#[tokio::test]
async fn c4_an_accepted_stop_is_unservable_at_once_and_the_door_stays_shut_while_the_engine_is_still_exiting()
 {
    let rig = Rig::new(&Behavior {
        trap_sigterm_secs: Some(2.5),
        ..stateful()
    });
    let port = rig.port;
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let old = full.rig.pid(0);

    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/stop")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // 受理的那一刻状态已经不是 `ready`：不可服务。
    assert_eq!(body["state"], "stopped", "{body}");
    wait_for("the engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;

    // 引擎在退出的这几秒里：进程还在、还是我们的（`owned`、`pid`），状态是 `stopped`；数据面 503，没有请求到引擎。
    assert!(pid_alive(old));
    let element = full.element("omlx").await;
    assert_eq!(element["state"], "stopped", "{element}");
    assert_eq!(element["owned"], true, "{element}");
    assert_eq!(element["pid"], json!(old), "{element}");
    let requests = full.rig.fake.requests().len();
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(full.rig.fake.requests().len(), requests);

    // 进程没了才是 `owned: false`；账本也清空了，端点封上。
    let element = full.until_gone().await;
    assert!(element.get("pid").is_none());
    assert_eq!(loaded_models(&full).await, json!([]));
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
}

#[tokio::test]
async fn c5_a_stop_waits_for_the_chat_in_flight_before_it_signals_the_engine() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let old = full.rig.pid(0);
    full.rig.fake.hold(Hold::Chat);
    let chats = full.rig.fake.requests_of("POST:/v1/chat/completions");
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the chat to reach the engine", || {
        full.rig.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;

    let (status, body) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/stop")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body["state"], "stopped",
        "unservable from the acceptance on"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !full.rig.fake.got_sigterm(old),
        "signalled before it drained"
    );
    full.rig.fake.release(Hold::Chat);
    assert_eq!(in_flight.await.unwrap().0, StatusCode::OK);
    wait_for("the engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    full.until_gone().await;
}

#[tokio::test]
async fn c6_a_restart_enqueued_while_the_reset_is_waiting_is_merged_and_the_engine_is_replaced_once()
 {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 新引擎起来之后，账本复位的那次采样（`GET /api/status`）停在引擎里：管理器在等它，不去收命令。
    let samples = full.rig.fake.requests_of("/api/status");
    full.rig.fake.hold(Hold::Status);
    let first = {
        let full = full.clone();
        tokio::spawn(async move {
            full.admin("POST", "/admin/api/v1/upstreams/omlx/restart")
                .await
        })
    };
    wait_for("the reset sample to reach the new engine", || {
        full.rig.fake.launches() == 2 && full.rig.fake.requests_of("/api/status") > samples
    })
    .await;
    // 复位还没做完：状态是 `restarting`（Agent24 对它有界等待；操作员的重启从受理到就绪一直是它），不是 `starting`，更不是 `ready`：
    // 这时进来的请求会带着过期的账本。
    assert_eq!(
        full.element("omlx").await["state"],
        "restarting",
        "the engine is not ready until its ledger was rebuilt"
    );
    // 第二个 `restart` 此刻到达：管理器正卡在复位里，命令排在队里。
    let second = {
        let full = full.clone();
        tokio::spawn(async move {
            full.admin("POST", "/admin/api/v1/upstreams/omlx/restart")
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    full.rig.fake.release(Hold::Status);

    let (status, body) = first.await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = second.await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 并发的两个 `restart` 只换了一次引擎：之后也不会再换。
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(full.rig.fake.launches(), 2, "a second restart ran");
    assert_eq!(full.health.view().upstreams[0].state, UpstreamState::Ready);
    assert_eq!(full.logged("收到命令 restart，已受理"), 2);
    assert_eq!(full.chat().await.0, StatusCode::OK);
}

#[tokio::test]
async fn c7_an_operator_restart_is_not_a_failure_but_a_restart_whose_new_engines_keep_dying_is_counted_every_time()
 {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    let before = failures_of(&full.element("omlx").await);
    assert_eq!(before, 0, "a clean first start has no failure");

    // 一次顺利的 `restart` 不算失败，`restarting` 也不进熔断窗口。
    full.admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    full.until("the replacement", |element| {
        element["state"] == "ready" && element["pid"] != json!(full.rig.pid(0))
    })
    .await;
    assert_eq!(failures_of(&full.element("omlx").await), before);

    // 之后拉起的引擎都一起来就退出：每一次真的失败都按次计数，熔断照常（上限 2，第三次是 `failed`）。
    full.rig.fake.set_behavior(&Behavior {
        exit_immediately: Some(1),
        ..stateful()
    });
    full.admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    let failed = full
        .until("the breaker", |element| element["state"] == "failed")
        .await;
    assert_eq!(failed["reason"], "crash_loop", "{failed}");
    assert_eq!(
        failures_of(&failed),
        3,
        "each real failure of the restart counted once: {failed}"
    );
}

#[tokio::test]
async fn c8_an_external_engine_that_is_replaced_by_another_process_gets_a_rebuilt_ledger_before_it_is_ready_again()
 {
    let work = tempfile::tempdir().unwrap();
    let mut a = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = a.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, a.pid(), Some(TEAM));
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    // A 里加载了模型，账本也这么认为。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(a.fake.model_loads(), 1);
    assert_eq!(loaded_models(&full).await, json!(["fake-model"]));

    // 用户把 A 关了：没有监听者。`unavailable`，端点封上，请求立刻 503，没有东西发出去。
    a.kill();
    inspector.forget_listener(port);
    full.until_unavailable().await;
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    assert_eq!(full.chat().await.0, StatusCode::SERVICE_UNAVAILABLE);

    // B 在同一个端口上起来了（另一个进程）。它的 `/api/status` 被扣着：管理器认出它换了进程（pid 与启动时间都变了），封上端点、
    // 关了门，正对着它重建账本，采样停在 B 里。
    let b = External::start_on(work.path(), "external-b", &stateful(), port);
    b.fake.hold(Hold::Status);
    inspector.external_listener(port, b.pid(), Some(TEAM));
    wait_for("the rebuild's sample to reach B", || {
        b.fake.requests_of("/api/status") >= 1
    })
    .await;
    // 这时 B 还没就绪：请求仍然是 503，B 收不到聊天也收不到加载。
    assert_ne!(
        full.health.view().upstreams[0].state,
        UpstreamState::Ready,
        "ready before the ledger was rebuilt"
    );
    assert_eq!(full.chat().await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b.fake.model_loads(), 0);
    assert_eq!(b.fake.requests_of("POST:/v1/chat/completions"), 0);

    // 放开：账本对着 B 重建（空的），开门，`ready`；下一个聊天在 B 上重新加载。
    b.fake.release(Hold::Status);
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_eq!(
        loaded_models(&full).await,
        json!([]),
        "the old ledger is gone"
    );
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(b.fake.model_loads(), 1, "chat reloaded on B");
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::External {
            settings_key_verified: true
        })
    );
}

#[tokio::test]
async fn c9_the_same_external_process_coming_back_keeps_its_ledger_and_the_door_is_reopened() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, external.pid(), Some(TEAM));
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 一次「没有监听者」的误读：端点封上、门关上、请求 503。
    inspector.forget_listener(port);
    full.until_unavailable().await;
    assert_eq!(full.chat().await.0, StatusCode::SERVICE_UNAVAILABLE);
    // 同一个进程又被看到了：账本没有过期（模型还在里面），不复位，门开回来。
    inspector.external_listener(port, external.pid(), Some(TEAM));
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_eq!(loaded_models(&full).await, json!(["fake-model"]));
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(
        external.fake.model_loads(),
        1,
        "no reload: the ledger was right"
    );
}

#[tokio::test]
async fn c10_a_managed_endpoint_is_blocked_from_the_first_instant_and_stays_so_until_a_listener_is_verified()
 {
    // 引擎一直不监听：端点从管理器启动起就是封上的（没有任何凭证发往它），不是「没人管、回退链适用」。
    let rig = Rig::new(&Behavior {
        never_listen: true,
        ..Behavior::default()
    });
    let endpoint = rig.endpoint();
    assert_eq!(
        governed_endpoint(&endpoint),
        None,
        "nobody has governed it yet"
    );
    let (manager, _health) = start_manager(&rig, Scripted::claiming(rig.port), StopOnExit::Owned);
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "blocked while nothing verified listens"
    );
    manager.shutdown(true).await;
    assert_eq!(governed_endpoint(&endpoint), Some(EndpointKey::Blocked));
}

/// 一个请求已经过了数据面的状态检查、停在 Supervisor 的准入之前（它读模型目录的那一步被引擎扣着）；这时 `restart` 到来，旧引擎进入
/// 退出的宽限期（还在服务它已经接受的连接）。目录读取被放开之后，这个请求走到 Supervisor 的准入：门是关着的，所以被拒绝
/// （`supervisor_unavailable`），一个加载也没有发给旧引擎。不关门的话，它会被准入，把加载发给正在退出的引擎。状态检查挡不住它：它是
/// 在状态还是 `ready` 的时候过了那一关的。
async fn a_request_past_the_state_gate_is_refused_by_the_closed_admission(uri: &str, body: Value) {
    let rig = Rig::new(&Behavior {
        trap_sigterm_secs: Some(2.5),
        ..stateful()
    });
    let port = rig.port;
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    let old = full.rig.pid(0);

    full.rig.fake.hold(Hold::List);
    let listings = full.rig.fake.requests_of("/v1/models/status");
    let in_flight = {
        let (full, uri) = (full.clone(), uri.to_owned());
        tokio::spawn(async move { full.data("POST", &uri, Some(body)).await })
    };
    wait_for("the catalog read to reach the engine", || {
        full.rig.fake.requests_of("/v1/models/status") == listings + 1
    })
    .await;

    let (status, _) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    wait_for("the old engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    full.rig.fake.release(Hold::List);

    let (status, body) = in_flight.await.unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["error"]["reason_code"], "supervisor_unavailable",
        "refused by the closed admission, not by a dying engine: {body}"
    );
    assert_eq!(
        full.rig.fake.model_loads(),
        0,
        "no load was sent to the engine that was being replaced"
    );
}

#[tokio::test]
async fn c11_a_chat_past_the_state_gate_is_refused_by_the_closed_admission_when_the_engine_is_being_replaced()
 {
    a_request_past_the_state_gate_is_refused_by_the_closed_admission(
        "/v1/chat/completions",
        json!({"model": "idoris/fast", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
}

#[tokio::test]
async fn c11_a_warmup_goes_through_the_same_admission_and_is_refused_the_same_way() {
    a_request_past_the_state_gate_is_refused_by_the_closed_admission(
        "/v1/models/warmup",
        json!({"model": "idoris/fast"}),
    )
    .await;
}

#[tokio::test]
async fn c12_an_external_process_that_reuses_the_pid_of_the_one_that_left_is_a_different_process() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, external.pid(), Some(TEAM));
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(external.fake.model_loads(), 1);
    assert_eq!(loaded_models(&full).await, json!(["fake-model"]));

    // 监听者没了；之后端口上又出现了一个监听者，pid 一样、启动时间不一样（pid 被回收给了另一个进程）。pid 相同不等于同一个进程：
    // 按 pid 加启动时间对账，所以这是另一个进程，账本要对着它重建。
    inspector.forget_listener(port);
    full.until_unavailable().await;
    inspector.pretend_start_time(external.pid(), "Sat Nov 21 23:59:59 2026");
    inspector.external_listener(port, external.pid(), Some(TEAM));
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 账本是对着「新进程」重建的：它的采样里模型还在（其实还是同一个真进程，假引擎里 `LOADED` 没变），作为继承来的驻留进账本，
    // 要经加载流程确认之后才算就绪，所以下一个聊天会发一次加载。没有对账（pid 相同就当还是它）的话，账本里的模型是就绪的，
    // 聊天不会再发加载：这是 c9（同一个进程回来）与这里的区别。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(
        external.fake.model_loads(),
        2,
        "a recycled pid is a different process: the ledger was rebuilt, so the next chat confirmed the model with a load"
    );
}

#[tokio::test]
async fn c13_after_the_drain_runs_out_the_old_generation_sends_nothing_more_to_the_engine_it_is_stopping()
 {
    // 旧引擎收到 SIGTERM 之后还继续服务 2.5 秒（它已经接受的与新来的连接都答）：这一段里旧一代的任务要是还能发请求，就会发给一个
    // 正在退出的引擎。排空到点就切断旧一代的凭证，所以它们之后发的每个请求都在适配器就被拒绝，一个字节也没发出去。
    let rig = Rig::new(&Behavior {
        trap_sigterm_secs: Some(2.5),
        ..stateful()
    });
    let port = rig.port;
    let mut timings = fast();
    timings.drain_timeout = Duration::from_millis(300);
    let full = Arc::new(Full::start_with(rig, Scripted::claiming(port), timings).await);
    full.wait_open().await;
    let old = full.rig.pid(0);

    // 一个加载已经发给旧引擎、还没做完。
    full.rig.fake.hold(Hold::Load);
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the load to reach the old engine", || {
        full.rig.fake.model_loads() == 1
    })
    .await;
    let status_reads = full.rig.fake.requests_of("/v1/models/status");

    let (status, _) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // 排空到点（300 毫秒）：旧一代被切断，然后才是 SIGTERM；引擎接着还服务 2.5 秒。
    wait_for("the old engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    // 旧引擎这时还活着，放开它的加载：加载做完了，那个操作接着要发的下一个请求（确认模型的状态）被拒绝。
    assert!(pid_alive(old));
    full.rig.fake.release(Hold::Load);
    let (status, body) = in_flight.await.unwrap();
    assert_ne!(
        status,
        StatusCode::OK,
        "the old generation's operation went on talking to the engine: {body}"
    );
    assert_eq!(
        full.rig.fake.requests_of("/v1/models/status"),
        status_reads,
        "nothing more was sent for the old generation once it was cut off"
    );
}

#[tokio::test]
async fn c14_while_the_old_load_is_in_flight_its_fence_stays_and_no_second_engine_is_started() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let fence =
        idoris_upstream::omlx::load_fence_path_in(&rig.state_dir(), &rig.endpoint()).unwrap();
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    let old = full.rig.pid(0);

    full.rig.fake.hold(Hold::Load);
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the load to reach the old engine", || {
        full.rig.fake.model_loads() == 1
    })
    .await;
    assert!(fence.exists(), "an admitted load leaves its durable fence");

    let (status, _) = full
        .admin("POST", "/admin/api/v1/upstreams/omlx/restart")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // 排空在等这个加载（上限 5 秒）：这半秒里什么都不变。旧引擎活着、没收到信号；围栏还在（它说的是这个还没做完的加载，旧引擎
    // 没消失之前不能清）；没有第二个引擎。
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(pid_alive(old));
    assert!(
        !full.rig.fake.got_sigterm(old),
        "signalled before it drained"
    );
    assert!(
        fence.exists(),
        "the fence was cleared while its load was still running"
    );
    assert_eq!(
        full.rig.fake.launches(),
        1,
        "a second engine was started early"
    );

    // 加载做完，围栏随之清掉；这之后旧引擎才被停掉，新引擎换上。这个请求的加载是在关门之前准入的，做完了；它接着要来的聊天是
    // 关门之后才到的，被拒绝（503，`supervisor_unavailable`），不是发给一个正在被替换的引擎。
    full.rig.fake.release(Hold::Load);
    let (status, body) = in_flight.await.unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["error"]["reason_code"], "supervisor_unavailable",
        "{body}"
    );
    wait_for("the replacement to be ready", || {
        full.rig.fake.launches() == 2
            && full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert!(full.rig.fake.got_sigterm(old));
    assert!(!fence.exists());
}
