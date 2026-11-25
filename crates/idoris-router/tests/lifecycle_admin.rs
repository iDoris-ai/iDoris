//! IDORIS-CORE S6（接口规范 §2.11.4、勘误十）：Admin 的 `upstreams` 读与 `start|stop|restart` 写，加上 S5 评审留下的三道门（G1、
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
// 引擎的端口是事先挑好、过一会儿才由假引擎去绑的：不能用 `bind(0)` 取号再放掉（见 `support/ports.rs`；`port_allocation.rs` 守着）。
#[path = "support/ports.rs"]
mod ports;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use child_guard::IsolateState;
use fake_omlx::{Behavior, FakeOmlx, Hold, OnSigterm, Release, spawn_retrying};
use http_body_util::BodyExt;
use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use idoris_router::admin::{
    AdminBindConfig, AdminSessionToken, SharedAdminToken, build_admin_app_with_engines,
};
use idoris_router::lifecycle::gate::{self, Finalized};
use idoris_router::lifecycle::{
    CommandAck, EngineBackend, EngineManager, ManagedPlan, ManagerConfig, OmlxBackend, Timings,
};
use idoris_router::readiness::{
    GateState, HealthState, Observation, ProberContext, ProberTimings, UpstreamReason,
    UpstreamState, UpstreamStatus, plan_upstreams,
};
use idoris_router::register::{JitterSource, LogSink};
use idoris_router::runtime::RuntimeRegistry;
use idoris_router::swap::SwapApp;
use idoris_router::{AppState, build_app};
use idoris_upstream::engine::{
    Bind, CodesignPolicy, FALLBACK_TTL, InspectError, ListenerPolicy, ListenerWatch,
    ListeningSocket, MANAGED_DIAL, PidState, ProcessInspector, StopOnExit, SystemInspector,
};
use idoris_upstream::omlx::key::{
    EndpointKey, endpoint_generation, forget_endpoint, governed_endpoint,
};
use ports::free_port;
use serde_json::{Value, json};
use tower::ServiceExt;

const GIB: u64 = 1 << 30;
const PATIENT: Duration = Duration::from_secs(60);
const TEAM: &str = "TESTTEAM01";

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
    /// `listens_on(_, port)` 一律是假的：引擎进程还活着，监听这个端口的却不再是它。
    lost: Mutex<Vec<u16>>,
    /// `listens_on` 一律读不出来（`lsof` 不答；Codex 终审 H1：读不出来也不发）。
    unreadable: AtomicBool,
    /// `watch_listener` 给一个永远回「进程还握着它的套接字」的原生监视（IDORIS-CORE S6，每个请求的确认不起子进程之后）：引擎自己的 LISTEN
    /// 套接字还在、另一个进程也在这个端口上监听——每个请求的确认看不见，管理器的监督节拍问的「唯一的监听者」看得见（`lose`）。要在管理器登记引擎
    /// **之前**打开（守卫建的时候问一次）。缺省 `false`：没有监视，守卫每个请求问一次 `listens_on`（H1 的那些用例就是这样）。
    native_always_holds: AtomicBool,
    /// `watch_listener` 给一个永远「读不了这个进程」的原生监视（Codex 专项评审 M10、M11：权限、读不出来）：守卫这一次问那个贵的问题，
    /// 不能就此拒绝。要在管理器登记引擎之前打开。
    native_unreadable: AtomicBool,
    /// 外部监听者的套接字（含绑的地址族与地址）：编排过的端口按它回答 `listening_sockets`；没编排的由 `listeners` 的 pid 推出（每个当作
    /// 绑在 IPv4 通配地址上）。
    sockets: Mutex<BTreeMap<u16, Vec<ListeningSocket>>>,
    /// 每次被问 `watch_listener` 的（pid，拨号地址）。
    watch_requests: Mutex<Vec<(u32, SocketAddr)>>,
    /// `listens_on` 被问了几次（退路上的贵的问题）。
    listens_asked: AtomicUsize,
    /// 守卫在没有原生监视时，贵的问题的**肯定**答案重用多久（毫秒）；缺省 0：每个请求都问。
    fallback_ttl_millis: AtomicUsize,
    /// 一个答案由测试编排的原生监视（[`Scripted::script_native`]）：所有守卫共用这一个。
    native_script: Mutex<Option<Arc<ScriptedWatch>>>,
    /// `signing_team` 停在这里（外部引擎的属主校验停住：实例任务忙在 `start` 接管外部引擎的那一步，不去收命令），见 [`Scripted::hold_team_checks`]。
    team_gate: Mutex<Option<Arc<TeamGate>>>,
}

/// 让属主校验里的 `signing_team` 停住的开关：先记下「已经到了」，再等测试放开。
#[derive(Debug, Default)]
struct TeamGate {
    entered: AtomicBool,
    released: AtomicBool,
}

impl TeamGate {
    fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

/// 永远说「还握着」的原生监视（[`Scripted::offer_a_native_watch_that_always_holds`]）。
#[derive(Debug)]
struct StillHolds;

#[async_trait]
impl ListenerWatch for StillHolds {
    async fn holds(&self) -> Result<bool, InspectError> {
        Ok(true)
    }
}

/// 答案由测试编排的原生监视（[`Scripted::script_native`]）：按顺序给，最后一个重复。
#[derive(Debug, Default)]
struct ScriptedWatch {
    answers: Mutex<std::collections::VecDeque<Result<bool, InspectError>>>,
}

#[async_trait]
impl ListenerWatch for ScriptedWatch {
    async fn holds(&self) -> Result<bool, InspectError> {
        let mut answers = self.answers.lock().unwrap();
        if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers
                .front()
                .copied()
                .unwrap_or(Err(InspectError::NoAnswer))
        }
    }
}

/// 永远「读不了这个进程」的原生监视（[`Scripted::offer_a_native_watch_that_cannot_read`]）：既不是「是」也不是「不是」。
#[derive(Debug)]
struct CannotRead;

#[async_trait]
impl ListenerWatch for CannotRead {
    async fn holds(&self) -> Result<bool, InspectError> {
        Err(InspectError::NoAnswer)
    }
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            real: SystemInspector::new(),
            claims: Mutex::new(Vec::new()),
            listeners: Mutex::new(BTreeMap::new()),
            teams: Mutex::new(BTreeMap::new()),
            start_times: Mutex::new(BTreeMap::new()),
            lost: Mutex::new(Vec::new()),
            unreadable: AtomicBool::new(false),
            native_always_holds: AtomicBool::new(false),
            native_unreadable: AtomicBool::new(false),
            sockets: Mutex::new(BTreeMap::new()),
            watch_requests: Mutex::new(Vec::new()),
            listens_asked: AtomicUsize::new(0),
            fallback_ttl_millis: AtomicUsize::new(0),
            native_script: Mutex::new(None),
            team_gate: Mutex::new(None),
        })
    }

    /// 从现在起，`signing_team` 停住，直到返回的开关被放开。
    fn hold_team_checks(&self) -> Arc<TeamGate> {
        let gate = Arc::new(TeamGate::default());
        *self.team_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// 从现在起建的守卫带一个答案由测试编排的原生监视（按顺序，最后一个重复）；之后再调一次只是换掉它的答案，已经建好的守卫也跟着变。要在管理器登记引擎
    /// **之前**第一次调用（守卫建的时候取到这个监视）。
    fn script_native(&self, answers: Vec<Result<bool, InspectError>>) {
        let watch = self
            .native_script
            .lock()
            .unwrap()
            .get_or_insert_with(|| Arc::new(ScriptedWatch::default()))
            .clone();
        *watch.answers.lock().unwrap() = answers.into();
    }

    /// 从现在起建的守卫带一个永远「读不了这个进程」的原生监视（守卫退到 `listens_on`）。
    fn offer_a_native_watch_that_cannot_read(&self) {
        self.native_unreadable.store(true, Ordering::SeqCst);
    }

    /// 编排这个端口上的监听套接字（含地址族与地址）；校验看这些，不看 `external_listener` 给的 pid 列表。
    fn sockets_on(&self, port: u16, sockets: Vec<ListeningSocket>) {
        self.sockets.lock().unwrap().insert(port, sockets);
    }

    /// 给一个 pid 编排签名团队（它的套接字另行用 [`Self::sockets_on`] 编排）。
    fn sign(&self, pid: u32, team: &str) {
        self.teams.lock().unwrap().insert(pid, team.to_owned());
    }

    /// 守卫在没有原生监视时，贵的问题的肯定答案重用这么久。
    fn reuse_fallback_for(&self, window: Duration) {
        self.fallback_ttl_millis
            .store(window.as_millis() as usize, Ordering::SeqCst);
    }

    /// `listens_on` 被问了几次。
    fn listens_asked(&self) -> usize {
        self.listens_asked.load(Ordering::SeqCst)
    }

    /// 每次被问 `watch_listener` 的（pid，拨号地址）。
    fn watched(&self) -> Vec<(u32, SocketAddr)> {
        self.watch_requests.lock().unwrap().clone()
    }

    /// 从现在起建的守卫带一个永远说「还握着」的原生监视。
    fn offer_a_native_watch_that_always_holds(&self) {
        self.native_always_holds
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 从现在起（或不再）`listens_on` 读不出来。
    fn set_unreadable(&self, unreadable: bool) {
        self.unreadable
            .store(unreadable, std::sync::atomic::Ordering::SeqCst);
    }

    /// 从现在起，`listens_on` 对这个端口回 `Ok(false)`（进程还活着，监听这个端口的不是它了）。
    fn lose(&self, port: u16) {
        self.claims
            .lock()
            .unwrap()
            .retain(|claimed| *claimed != port);
        self.lost.lock().unwrap().push(port);
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
        self.listens_asked.fetch_add(1, Ordering::SeqCst);
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(InspectError::NoAnswer);
        }
        if self.lost.lock().unwrap().contains(&port) {
            return Ok(false);
        }
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
    async fn listening_sockets(&self, port: u16) -> Result<Vec<ListeningSocket>, InspectError> {
        if let Some(sockets) = self.sockets.lock().unwrap().get(&port) {
            return Ok(sockets.clone());
        }
        Ok(self
            .listeners_on(port)
            .await?
            .into_iter()
            .map(ListeningSocket::any)
            .collect())
    }
    async fn uid_of(&self, pid: u32) -> Result<u32, InspectError> {
        self.real.uid_of(pid).await
    }
    async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
        let gate = self.team_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.entered.store(true, Ordering::SeqCst);
            while !gate.released.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        Ok(self.teams.lock().unwrap().get(&pid).cloned())
    }
    fn watch_listener(&self, pid: u32, dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
        self.watch_requests.lock().unwrap().push((pid, dial));
        if let Some(watch) = self.native_script.lock().unwrap().clone() {
            return Some(watch);
        }
        if self.native_unreadable.load(Ordering::SeqCst) {
            return Some(Arc::new(CannotRead));
        }
        self.native_always_holds
            .load(Ordering::SeqCst)
            .then(|| Arc::new(StillHolds) as Arc<dyn ListenerWatch>)
    }
    fn fallback_ttl(&self) -> Duration {
        Duration::from_millis(self.fallback_ttl_millis.load(Ordering::SeqCst) as u64)
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
        command_timeout: Duration::from_secs(10),
        unverifiable_retries: 2,
        unverifiable_pause: Duration::from_millis(20),
    }
}

/// 命令的截止时间（[`parked_actor_timings`]）：只有「实例任务认领之前」才会碰到它。
const PARKED_COMMAND_TIMEOUT: Duration = Duration::from_millis(1000);

/// 让实例任务在引擎就绪之后停在「等邮箱」上：存活检查与周期探测的间隔都拉到一小时，监督循环里不会再有一次 `lsof` 或 Supervisor 状态读
///（`supervisor_latched` 会被扣着的采样拖住）占住实例任务。这样迟到 202 的测试里，命令一入队就立刻被认领（邮箱空、实例任务闲），认领不会落在 `command_timeout`
/// 之后而让命令合法地过期成 503；之后截止时间只由被扣着的 `close_admission` 越过。没有任何靠睡眠定先后的地方。
fn parked_actor_timings() -> Timings {
    let mut timings = fast();
    timings.probe_interval = Duration::from_secs(3600);
    timings.liveness_interval = Duration::from_secs(3600);
    timings.command_timeout = PARKED_COMMAND_TIMEOUT;
    timings
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
        // The key registry is process-global and the OS hands a freed port to another test of this process: leave nothing of
        // this endpoint's registration (not even its generation) behind.
        forget_endpoint(&self.endpoint());
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
        Self::start_with_sink(rig, executable, inspector, timings, wait_settled, None).await
    }

    /// 同 [`Self::start`]，日志的出口多带一个钩子（[`LogGate::sink`]）：实例任务写某一行日志时可以被测试扣住。
    async fn start_gated(engine: Engine, timings: Timings, gate: &Arc<LogGate>) -> Self {
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
        Self::start_with_sink(rig, executable, inspector, timings, true, Some(gate.sink())).await
    }

    async fn start_with_sink(
        rig: Rig,
        executable: PathBuf,
        inspector: Arc<dyn ProcessInspector>,
        timings: Timings,
        wait_settled: bool,
        extra_sink: Option<LineHook>,
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
            Arc::new(move |line| {
                log.lock().unwrap().push(line.to_owned());
                if let Some(extra) = &extra_sink {
                    extra(line);
                }
            })
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
    // 202 的体是实例任务受理那一刻自己的快照（Codex 预审 L4 的 M1）：`starting`，还没有引擎。不会是 `stopped`，也不会因为新引擎来得快而是 `ready`。
    assert_eq!(body["state"], "starting", "{body}");
    assert_eq!(body["owned"], false, "{body}");
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
    // 202 的体是实例任务受理那一刻自己的快照（Codex 预审 L4 的 M1），与新引擎来得多快无关：`restarting`，旧引擎还握在手里（`owned`、旧 pid）。
    // 之前它在回答之后再读一遍共享状态，快到新引擎已经就绪时读到的是 `ready` 加新 pid。
    assert_eq!(body["state"], "restarting", "{body}");
    assert_eq!(body["owned"], true, "{body}");
    assert_eq!(body["pid"], json!(first), "{body}");
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
        Self::start_with_cards(rig, inspector, timings, Vec::new()).await
    }

    /// 同 [`Self::start_with`]，Router 里除了托管的 `omlx` 卡还有这些卡（伴生卡、独立的上游……）。
    async fn start_with_cards(
        rig: Rig,
        inspector: Arc<dyn ProcessInspector>,
        timings: Timings,
        extra_cards: Vec<ComponentCard>,
    ) -> Self {
        let mut cards = vec![managed_card(&rig.endpoint())];
        cards.extend(extra_cards);
        Self::start_with_card_order(rig, inspector, timings, cards).await
    }

    /// 同 [`Self::start_with_cards`]，Router 里的全部卡（含托管的 `omlx`）由调用方按顺序给：模型列表按卡的顺序逐张请求，先排的慢卡会
    /// 让后面的卡在列表期间被停掉（Codex 终审 M6）。
    async fn start_with_card_order(
        rig: Rig,
        inspector: Arc<dyn ProcessInspector>,
        timings: Timings,
        cards: Vec<ComponentCard>,
    ) -> Self {
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
                            let adapter = idoris_upstream::factory::create_omlx_adapter(card)?
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
    // 等到它真的入了邮箱（第 1 条是第一个 `restart`），不靠睡眠。
    wait_for("the second restart to be in the mailbox", || {
        full.manager.commands_enqueued("omlx") >= 2
    })
    .await;
    full.rig.fake.release(Hold::Status);

    let (status, body) = first.await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = second.await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // 第二个并进了第一个：它是在复位做完、`ready` 发布之后才被收下的（并入的判据是入队时盖的章不超过 `ready` 那一步记的水位），回答里的状态是 `ready`；
    // 当成一次新的重启收下的话回答是 `restarting`，引擎要再换一次。这一条取代了原来的「睡 800 毫秒再数拉起次数」。
    assert_eq!(
        body["state"], "ready",
        "the second restart was merged: {body}"
    );
    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 并发的两个 `restart` 只换了一次引擎。
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

    // 用户把 A 关了：端口上没有监听者了。`unavailable`，端点封上，请求立刻 503，没有东西发出去。先让监听者消失、等管理器看到它
    // （此刻状态与封上是同一个周期里一起发生的），再让进程退出：反过来的话，进程先退出，探测被拒的那个周期可能赶在「没有监听者」的
    // 那个周期之前发布 `unavailable`，端点这时还没封上。
    inspector.forget_listener(port);
    full.until_unavailable().await;
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    a.kill();
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

// ---------------------------------------------------------------------------------------------
// 密钥治理审计（IDORIS-CORE S6，Codex 审计 H1、H2、H3）：真的 Router、真的假引擎、一个占着端口的冒名者
// ---------------------------------------------------------------------------------------------
//
// 每个用例都在**子进程**里再跑一遍，子进程的环境里有金丝雀 key（环境变量 `IDORIS_OMLX_API_KEY` 与 `~/.omlx/settings.json` 的
// `auth.api_key`）：key 取自进程环境，进程内改不了（`unsafe_code = "forbid"`），而「冒名者从不收到 `Authorization`」只有在环境里
// 真有 key 可发的时候才证明了什么（环境里恰好没有 key 的机器上，一个会把 key 发出去的实现也能通过）。冒名者记下它收到的每个
// 请求的头；断言是它**一个** `Authorization` 头都没收到，不只是金丝雀的值。

const CANARY_ENV_KEY: &str = "canary-env-key-that-must-never-reach-an-impostor-41c7";
const CANARY_SETTINGS_KEY: &str = "canary-settings-key-that-must-never-reach-an-impostor-8e2b";

/// 当前这个用例在子进程里再跑一遍（环境里有金丝雀 key）。`true` = 这就是子进程，调用方接着做场景；`false` = 这是父进程，子进程
/// 已经跑完并且通过了，调用方直接返回。
fn in_a_child_with_canary_keys(test: &str) -> bool {
    if std::env::var_os("IDORIS_KEY_AUDIT_CHILD").is_some() {
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".omlx")).unwrap();
    std::fs::write(
        home.path().join(".omlx/settings.json"),
        format!(r#"{{"auth":{{"api_key":"{CANARY_SETTINGS_KEY}"}}}}"#),
    )
    .unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--test-threads=1", "--nocapture"])
        .env("IDORIS_KEY_AUDIT_CHILD", "1")
        .env("HOME", home.path())
        .env("IDORIS_OMLX_API_KEY", CANARY_ENV_KEY)
        .isolate_state()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the child run of {test} failed:\n--- stdout\n{}\n--- stderr\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // 名字对不上的 `--exact` 一个测试也不跑、照样成功退出：子进程必须真的跑了这一个测试。
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"),
        "the child did not run {test}:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    false
}

/// 占着一个端口的冒名者：什么都答（`GET /v1/models` 答一个不像 oMLX 的体），记下每个请求的头。
struct Impostor {
    server: wiremock::MockServer,
}

impl Impostor {
    async fn on(listener: std::net::TcpListener) -> Self {
        let server = wiremock::MockServer::builder()
            .listener(listener)
            .start()
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({"hello": "world"})),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(
                    json!({"object": "list", "data": [], "models": [], "choices": []}),
                ),
            )
            .mount(&server)
            .await;
        Self { server }
    }

    /// 绑这个端口（引擎刚放掉它时要等一会儿）。
    async fn on_port(port: u16) -> Self {
        let deadline = Instant::now() + PATIENT;
        loop {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => return Self::on(listener).await,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "the port never came free: {error}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    async fn requests(&self) -> Vec<wiremock::Request> {
        self.server.received_requests().await.unwrap()
    }

    async fn count(&self) -> usize {
        self.requests().await.len()
    }

    /// 带着 `Authorization` 头的请求（方法加路径）。
    async fn authorized(&self) -> Vec<String> {
        self.requests()
            .await
            .iter()
            .filter(|request| request.headers.contains_key("authorization"))
            .map(|request| format!("{} {}", request.method, request.url.path()))
            .collect()
    }

    async fn assert_never_authorized(&self) {
        let authorized = self.authorized().await;
        assert!(
            authorized.is_empty(),
            "the impostor received credentials: {authorized:?}"
        );
    }
}

/// 伴生 embedding 卡（`setup omlx --bind embed=…` 生成的那种）：与主卡同一个 endpoint，resident，只声明 `embedding`，不托管。
fn companion_card(endpoint: &str) -> ComponentCard {
    use idoris_contracts::common::Capability;
    let mut card = template();
    card.provider.id = "omlx-embed".to_owned();
    card.provider.capabilities = vec![Capability::Embedding];
    card.endpoint = endpoint.to_owned();
    let policy = card.load_policy.as_mut().unwrap();
    policy.mode = LoadMode::Resident;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"embed": "embed-model"}
        }))
        .unwrap(),
    );
    card
}

/// 手写的、伪装成 oMLX 的卡：`provider.id` 是 `omlx`、`version_pin` 是 `omlx@…`，但它带 `idoris_upstream`（自定义上游）。不托管。
fn disguised_card(endpoint: &str) -> ComponentCard {
    let mut card = template();
    card.endpoint = endpoint.to_owned();
    card.provider.id = "omlx".to_owned();
    card.load_policy.as_mut().unwrap().mode = LoadMode::OnDemand;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_upstream": {"kind": "openai_compat", "flavor": "omlx"},
            "idoris_role_models": {"fast": "fake-model"}
        }))
        .unwrap(),
    );
    card
}

/// 真的（可以带 key 的）oMLX 卡，不托管：v0.5.0 的口径，环境变量与 `settings.json` 的 key 照旧发给它。对照用。
fn unmanaged_omlx_card(endpoint: &str) -> ComponentCard {
    let mut card = disguised_card(endpoint);
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"fast": "fake-model"}
        }))
        .unwrap(),
    );
    card
}

/// 一个什么都答 200 的 oMLX 形状的假上游（目录、状态、聊天），记下每个请求。
async fn recording_upstream() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"id": "fake-model", "estimated_size": 1_000_000_000_u64, "loaded": false, "pinned": false}],
            "data": [{"id": "up-model", "object": "model"}],
            "choices": [{"message": {"content": "x"}}],
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 17_179_869_184_u64,
            "pressure": "ok"
        })))
        .mount(&server)
        .await;
    server
}

async fn request_headers(server: &wiremock::MockServer) -> Vec<(String, Option<String>)> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            (
                format!("{} {}", request.method, request.url.path()),
                request
                    .headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_owned()),
            )
        })
        .collect()
}

#[tokio::test]
async fn h1_a_companion_card_inherits_the_failure_of_the_engine_it_shares_and_the_impostor_never_sees_a_credential()
 {
    if !in_a_child_with_canary_keys(
        "h1_a_companion_card_inherits_the_failure_of_the_engine_it_shares_and_the_impostor_never_sees_a_credential",
    ) {
        return;
    }
    // 端口上是冒名者（先于一切占着端口）。主卡的匿名探测得到 200 加一个不像 oMLX 的体：`NotThisEngine`，终局 `failed` + `port_in_use`。
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let impostor = Impostor::on(held).await;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let endpoint = rig.endpoint();
    // 独立的上游（不共用这个引擎）：照常服务。
    let independent = recording_upstream().await;
    let cards = vec![companion_card(&endpoint), upstream_card(&independent.uri())];
    let full = Full::start_with_cards(rig, Scripted::new(), fast(), cards.clone()).await;
    full.wait_open().await;

    let main = full.element("omlx").await;
    assert_eq!(main["state"], "failed", "{main}");
    assert_eq!(main["reason"], "port_in_use", "{main}");
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "the terminal failure left the endpoint closed to every credential"
    );
    // 主卡自己做过的匿名探测之外，冒名者此后不再收到任何东西。
    let baseline = impostor.count().await;
    assert!(baseline >= 1, "the main card's own anonymous probe");
    impostor.assert_never_authorized().await;

    // 数据面：伴生卡继承主卡的失败，503 带同一个原因码，一个请求都不碰那个端口。
    let (status, body) = full
        .data(
            "POST",
            "/v1/embeddings",
            Some(json!({"model": "idoris/embed", "input": "hello"})),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable", "{body}");
    assert_eq!(body["error"]["reason_code"], "port_in_use", "{body}");
    // 预热同样。
    let (status, body) = full
        .data(
            "POST",
            "/v1/models/warmup",
            Some(json!({"model": "idoris/embed"})),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["reason_code"], "port_in_use", "{body}");
    // 模型列表：伴生卡与主卡都不被探测，只列出独立上游的模型。
    let (status, body) = full.data("GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["up-model"], "{body}");
    // 独立的上游照常服务。
    let (status, body) = full
        .data(
            "POST",
            "/v1/chat/completions",
            Some(json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Admin 的模型来源：伴生卡与主卡一样如实报「不可用」，不探测。
    let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let companion_source = body["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["provider_id"] == "omlx-embed")
        .unwrap_or_else(|| panic!("no source for the companion card: {body}"));
    assert_eq!(companion_source["state"], "error", "{companion_source}");
    assert_eq!(
        companion_source["error"], "unavailable",
        "{companion_source}"
    );

    // 后台探测：伴生卡继承失败、不发请求；独立的上游照常就绪。
    let context = ProberContext {
        health: full.health.clone(),
        plans: plan_upstreams(&cards).unwrap(),
        runtimes: RuntimeRegistry::default(),
        client: reqwest::Client::new(),
        remote_kill_switch: Arc::new(AtomicBool::new(false)),
        timings: ProberTimings::default(),
    };
    for plan in &context.plans {
        match plan.id.as_str() {
            "omlx-embed" => assert_eq!(
                context.observe(plan).await,
                Observation::Unavailable(Some(UpstreamReason::PortInUse)),
                "the companion card inherits the engine's failure"
            ),
            "lmstudio" => assert_eq!(context.observe(plan).await, Observation::Ready),
            other => panic!("unexpected plan {other}"),
        }
    }

    assert_eq!(
        impostor.count().await,
        baseline,
        "nothing but the main card's own anonymous probe ever reached the impostor"
    );
    impostor.assert_never_authorized().await;
    // 独立上游收到的东西里，没有 oMLX 的 key（它不是 oMLX 卡）。
    for (what, authorization) in request_headers(&independent).await {
        assert_eq!(authorization, None, "{what}");
    }
}

#[tokio::test]
async fn h1_the_managed_endpoint_is_closed_to_every_credential_before_anything_has_probed_it() {
    if !in_a_child_with_canary_keys(
        "h1_the_managed_endpoint_is_closed_to_every_credential_before_anything_has_probed_it",
    ) {
        return;
    }
    // 引擎管理器一启动（`start` 同步返回）托管的端点就已经封上，先于它自己的第一次探测、也先于别的卡（伴生卡的后台探测）对同一个
    // 端点的第一次探测：这个窗口里端点没有登记，会落进环境变量与 `settings.json` 的回退链，金丝雀 key 就发给了端口上的冒名者。
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let impostor = Impostor::on(held).await;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let endpoint = rig.endpoint();
    let health = HealthState::with_upstreams(0, vec![upstream_status("omlx", "omlx", true)]);
    let manager = EngineManager::start(
        ManagerConfig {
            state_dir: rig.state_dir(),
            allocated_budget_bytes: Some(16 * GIB),
            stop_on_exit: StopOnExit::Owned,
            executables: BTreeMap::from([("omlx".to_owned(), rig.fake.executable.clone())]),
            launched_by: "test-instance".to_owned(),
            timings: fast(),
            inspector: Scripted::new(),
            log: Arc::new(|_| {}),
        },
        vec![ManagedPlan {
            id: "omlx".to_owned(),
            endpoint: endpoint.clone(),
            port,
            backend: omlx_backend(&rig.work.path().join("models")),
        }],
        health.clone(),
    );
    assert_eq!(governed_endpoint(&endpoint), Some(EndpointKey::Blocked));
    // 马上对伴生卡做一次后台探测，不等管理器做完任何事。
    let companion = companion_card(&endpoint);
    let plans = plan_upstreams(std::slice::from_ref(&companion)).unwrap();
    let context = ProberContext {
        health: health.clone(),
        plans,
        runtimes: RuntimeRegistry::default(),
        client: reqwest::Client::new(),
        remote_kill_switch: Arc::new(AtomicBool::new(false)),
        timings: ProberTimings::default(),
    };
    let _ = context.observe(&context.plans[0]).await;
    // 之后管理器自己的探测与终局的失败也不带任何凭证。
    wait_for("the managed engine to fail", || {
        health.view().upstreams[0].state == UpstreamState::Failed
    })
    .await;
    assert!(impostor.count().await >= 1);
    impostor.assert_never_authorized().await;
    manager.shutdown(true).await;
}

#[tokio::test]
async fn h2_a_handwritten_card_that_claims_to_be_omlx_never_gets_an_omlx_key_while_a_real_one_does()
{
    if !in_a_child_with_canary_keys(
        "h2_a_handwritten_card_that_claims_to_be_omlx_never_gets_an_omlx_key_while_a_real_one_does",
    ) {
        return;
    }
    // 两张卡各有各的 Router（各自走生产的适配器构造路径：`RuntimeRegistry` 的缺省工厂），各对着各的记录上游：手写的、带
    // `idoris_upstream` 的伪装卡，和一张真的（不托管的）oMLX 卡。
    let disguised_upstream = recording_upstream().await;
    let real_upstream = recording_upstream().await;
    let router_for = |card: ComponentCard| {
        let cards = vec![card];
        let runtimes = RuntimeRegistry::spawn_with_local_configs_budget_and_kill_switch(
            &cards,
            &BTreeMap::new(),
            Some(16 * GIB),
            false,
        )
        .unwrap();
        build_app(AppState {
            cards,
            runtimes,
            ..AppState::default()
        })
    };
    for app in [
        router_for(disguised_card(&disguised_upstream.uri())),
        router_for(unmanaged_omlx_card(&real_upstream.uri())),
    ] {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("X-iDoris-Privacy", "local_only")
                    .body(Body::from(
                        json!({"model": "idoris/fast", "messages": [{"role": "user", "content": "hi"}]})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let _ = response.into_body().collect().await;
    }

    // 对照：真的卡收到了金丝雀 key（所以这个环境里的 key 确实会被发出去，下面的断言不是空的）。
    let real = request_headers(&real_upstream).await;
    assert!(!real.is_empty());
    assert!(
        real.iter()
            .any(|(_, authorization)| authorization.as_deref()
                == Some(&format!("Bearer {CANARY_ENV_KEY}"))),
        "the control card should carry the environment key: {real:?}"
    );
    // 伪装卡：生命周期的流程确实去问了它（启动采样、目录），但没有一个请求带 `Authorization`。
    let disguised = request_headers(&disguised_upstream).await;
    assert!(!disguised.is_empty(), "the Supervisor did talk to it");
    for (what, authorization) in &disguised {
        assert_eq!(*authorization, None, "{what}");
    }
}

#[tokio::test]
async fn h2_after_the_port_changed_hands_the_cached_adapter_sends_nothing_to_whoever_took_it() {
    if !in_a_child_with_canary_keys(
        "h2_after_the_port_changed_hands_the_cached_adapter_sends_nothing_to_whoever_took_it",
    ) {
        return;
    }
    // 接管的外部引擎（有状态的假引擎）：聊天经 Supervisor 与它早就建好的适配器正常工作。
    let work = tempfile::tempdir().unwrap();
    let mut external = External::start(work.path(), &stateful());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, external.pid(), Some(TEAM));
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 外部引擎退出：管理器看到监听者没了，封上端点、`unavailable`。冒名者在这之后才绑上端口（先有「校验通过、然后发 key」这一步
    // 与端口换主人之间的检查到使用的间隙，是任何校验都消除不了的；这里要证明的是：封上之后，端口上无论换成谁，都没有凭证发过去）。
    // 检查器这时说监听者是另一个进程（测试进程自己，同一个用户、没有签名团队）。
    external.kill();
    inspector.forget_listener(port);
    wait_for("unavailable, endpoint blocked", || {
        full.health.view().upstreams[0].state == UpstreamState::Unavailable
            && governed_endpoint(&full.rig.endpoint()) == Some(EndpointKey::Blocked)
    })
    .await;
    let impostor = Impostor::on_port(port).await;
    inspector.external_listener(port, std::process::id(), None);
    wait_for("failed, port_in_use", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Failed
            && upstream.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    // 适配器、Supervisor、Router 都早就建好了：请求在运行期被挡住，冒名者什么都没收到。
    for _ in 0..3 {
        let (status, body) = full.chat().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["reason_code"], "port_in_use", "{body}");
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(impostor.count().await, 0, "not one request reached it");
    impostor.assert_never_authorized().await;
}

#[tokio::test]
async fn h3_an_engine_that_stays_alive_but_gives_the_port_away_never_sends_its_private_key_to_the_new_listener()
 {
    if !in_a_child_with_canary_keys(
        "h3_an_engine_that_stays_alive_but_gives_the_port_away_never_sends_its_private_key_to_the_new_listener",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let inspector = Scripted::claiming(port);
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let engine = full.rig.pid(0);
    let private_key = full.rig.key(0);

    // 引擎放掉端口、但不退出（进程活着）；检查器也不再说它在监听。
    full.rig.fake.release_port();
    inspector.lose(port);

    // 管理器的下一个节拍：先收回私有 key（端点封上）、发布 `unavailable` 加 `port_in_use`（数据面 503），不发探测。收不回来（没有每个
    // 节拍重新确认），状态永远不会变，这一步就超时。
    wait_for("unavailable, port_in_use", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Unavailable
            && upstream.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert!(pid_alive(engine), "the engine process is still alive");
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    // 冒名者在这之后绑上同一个端口（先有「确认、然后发 key」与端口换主人之间的检查到使用的间隙，是任何确认都消除不了的）：
    // 之后好几个探测间隔里，没有凭证发过去。
    let impostor = Impostor::on_port(port).await;
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    // 好几个探测间隔（`fast()` 是 100 毫秒一次）：冒名者没收到任何带 key 的请求，更没有引擎的私有 key。
    tokio::time::sleep(Duration::from_millis(700)).await;
    impostor.assert_never_authorized().await;
    for request in impostor.requests().await {
        assert!(
            !format!("{:?}", request.headers).contains(&private_key),
            "the engine's private key reached the new listener"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Codex 终审（17ac795a）：H1、H2、H5、H6、M1、M2、M5、M6。真的 Router、真的假引擎，时序用同步点（引擎里被扣着的请求、可编排的检查器、
// 管理器监督节拍的暂停）到达，不靠墙钟。
// ---------------------------------------------------------------------------------------------

/// 监督节拍拉到小时级（Codex 终审 H1）：管理器自己的节拍不会在用例期间收回登记，登记表里的口径一直是「私有 key」；用例要让监听者
/// 在「撤销登记**之前**」换掉。
fn supervision_paused() -> Timings {
    let mut timings = fast();
    timings.liveness_interval = Duration::from_secs(3600);
    timings.probe_interval = Duration::from_secs(3600);
    timings
}

/// 一张 resident 的聊天卡，与托管的 `omlx` 同一个引擎（同一个 endpoint）：`idoris/daily` 走它，经代理转发（流式也在这里）。
fn resident_chat_card(endpoint: &str) -> ComponentCard {
    let mut card = template();
    card.provider.id = "omlx-chat".to_owned();
    card.endpoint = endpoint.to_owned();
    card.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"daily": "fake-model"}
        }))
        .unwrap(),
    );
    card
}

impl Full {
    /// 数据面的请求，响应头一到就返回（响应体不读）：流式响应在这个时刻还开着。
    async fn data_raw(&self, method: &str, path: &str, body: Value) -> axum::http::Response<Body> {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("X-iDoris-Privacy", "local_only")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.data.router().oneshot(request).await.unwrap()
    }

    /// Admin 对一条路径的原始调用（`restart`、`stop`……）：状态码与体。
    async fn post_command(&self, action: &str) -> (StatusCode, Value) {
        self.admin("POST", &format!("/admin/api/v1/upstreams/omlx/{action}"))
            .await
    }
}

// ---------------------------------------------------------------------------------------------
// H1（owner 2026-11-22 的拍板）：发送前重新确认监听者；在「撤销登记之前」切换监听者
// ---------------------------------------------------------------------------------------------

/// 引擎放掉了端口（进程还活着），一个冒名者绑上来，**而管理器自己的监督还没有走到收回登记那一步**（节拍被暂停）：登记表里还是引擎的私有
/// key，状态还是 `ready`，数据面的状态门放行。发出去的每一个请求（聊天、公共模型列表、Admin 的模型观察）都在发送前经授权，守卫重新
/// 确认监听者：不是引擎了，拒绝，冒名者**一个请求**也没有收到，更没有 `Authorization`。检查器读不出来（`lsof` 不答）同样不发。
///
/// 没有消除的窗口：重新确认与那一次发送之间的毫秒（本机另一个用户的进程恰好抢到端口）——见 `omlx_authorize.rs` 的
/// `known_limit_…`，规范勘误十把它写成已知限制。那一段这里不断言。
#[tokio::test]
async fn h1_a_listener_switched_before_the_registration_is_revoked_gets_nothing_from_chat_the_model_list_or_the_admin_observation()
 {
    if !in_a_child_with_canary_keys(
        "h1_a_listener_switched_before_the_registration_is_revoked_gets_nothing_from_chat_the_model_list_or_the_admin_observation",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let inspector = Scripted::claiming(port);
    let full = Full::start_with(rig, inspector.clone(), supervision_paused()).await;
    full.wait_open().await;
    assert_eq!(
        full.chat().await.0,
        StatusCode::OK,
        "it serves before the switch"
    );
    let private_key = full.rig.key(0);

    // 引擎放掉端口，冒名者绑上，检查器也不再说端口上的是引擎。管理器没有注意到（节拍被暂停）。
    full.rig.fake.release_port();
    inspector.lose(port);
    let impostor = Impostor::on_port(port).await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Private(private_key.clone())),
        "the switch landed before the registration was revoked"
    );
    assert_eq!(
        full.health.view().upstreams[0].state,
        UpstreamState::Ready,
        "the state gate still lets requests through"
    );

    for scenario in [
        "the listener is someone else",
        "the listener cannot be read",
    ] {
        if scenario == "the listener cannot be read" {
            inspector.set_unreadable(true);
        }
        // 聊天：Supervisor 的门开着，适配器在发送前被拒。
        let (status, _) = full.chat().await;
        assert_ne!(status, StatusCode::OK, "{scenario}");
        // 公共模型列表：这张卡被跳过（不可用），不是一个匿名的、或带 key 的 GET。
        let (status, body) = full.data("GET", "/v1/models", None).await;
        assert_eq!(status, StatusCode::OK, "{scenario}: {body}");
        assert_eq!(body["data"], json!([]), "{scenario}: {body}");
        // Admin 的模型观察：如实报「不可用」。
        let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
        assert_eq!(status, StatusCode::OK, "{scenario}: {body}");
        let source = body["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["provider_id"] == "omlx")
            .unwrap_or_else(|| panic!("{scenario}: no source for omlx: {body}"));
        assert_eq!(source["state"], "error", "{scenario}: {source}");
        assert_eq!(source["error"], "unavailable", "{scenario}: {source}");
    }
    assert_eq!(
        impostor.count().await,
        0,
        "not one request reached the new listener"
    );
    impostor.assert_never_authorized().await;
    for request in impostor.requests().await {
        assert!(!format!("{:?}", request.headers).contains(&private_key));
    }
}

// ---------------------------------------------------------------------------------------------
// H1 之后（主会话的要求）：每个请求的确认 p99 < 5 毫秒、请求路径上不起子进程；贵的「唯一的监听者」留给监督节拍
// ---------------------------------------------------------------------------------------------

/// 一个替身 `ps` / `lsof`：每被运行一次往 `log` 里追加一行，然后运行真的那个。请求路径上起了子进程，就会在 `log` 里留下一行。
fn counting_tool(
    dir: &std::path::Path,
    name: &str,
    real: &[&str],
    log: &std::path::Path,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut body = format!("#!/bin/sh\necho {name} >> '{}'\n", log.display());
    for candidate in real {
        body.push_str(&format!("[ -x {candidate} ] && exec {candidate} \"$@\"\n"));
    }
    body.push_str("exit 127\n");
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn tool_calls(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

/// 等到工具调用数 300 毫秒内不再变（引擎落定、门控放行之后管理器还在做的那几次 `ps`）：之后的增量只能是后来的东西起的。
async fn settled_tool_calls(log: &std::path::Path) -> usize {
    let mut last = tool_calls(log);
    loop {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let now = tool_calls(log);
        if now == last {
            return now;
        }
        last = now;
    }
}

/// **每个请求的确认不起子进程，并且在下一个请求就看见引擎放掉了端口**（真的 `SystemInspector`：原生监视，不是编排的检查器）。
///
/// 检查器是真的，`ps` 与 `lsof` 换成会数自己被运行了几次的替身（运行完还是真的那个）：引擎落定、门控放行之后，一串聊天请求不让它们多运行一次；
/// 管理器的监督被拉到小时级，所以挡在引擎的私有 key 与端口之间的只有每个请求的确认。引擎放掉端口（进程还活着），冒名者绑上来：聊天、公共模型
/// 列表、Admin 的模型观察都在发送前被拒，冒名者**一个请求**也没有收到，更没有 `Authorization`；拒绝同样没有起子进程。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn h1_the_per_request_check_starts_no_child_process_and_the_very_next_request_sees_the_engine_let_go_of_its_port()
 {
    if !in_a_child_with_canary_keys(
        "h1_the_per_request_check_starts_no_child_process_and_the_very_next_request_sees_the_engine_let_go_of_its_port",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("calls.log");
    let ps = counting_tool(tools.path(), "ps", &["/bin/ps"], &log);
    let lsof = counting_tool(
        tools.path(),
        "lsof",
        &["/usr/sbin/lsof", "/usr/bin/lsof"],
        &log,
    );
    let inspector = Arc::new(
        SystemInspector::with_tools(ps, lsof)
            .with_timeouts(PATIENT, PATIENT)
            .with_native_probe(true),
    );
    let full = Full::start_with(rig, inspector, supervision_paused()).await;
    full.wait_open().await;
    assert_eq!(
        full.chat().await.0,
        StatusCode::OK,
        "it serves before the engine lets go"
    );
    let private_key = full.rig.key(0);
    let settled = settled_tool_calls(&log).await;
    assert!(
        settled >= 1,
        "the manager asked the tools at the state changes"
    );

    // 一串请求：每个都在发送前经守卫重新确认，一个子进程也没有起。
    for _ in 0..30 {
        assert_eq!(full.chat().await.0, StatusCode::OK);
    }
    assert_eq!(
        tool_calls(&log),
        settled,
        "a child process (ps or lsof) was started on the request path"
    );

    // 引擎放掉端口、不退出；冒名者绑上。管理器没有注意到（节拍被暂停）：登记表里还是引擎的私有 key，状态还是 `ready`。
    full.rig.fake.release_port();
    let impostor = Impostor::on_port(port).await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Private(private_key.clone())),
        "the manager has not noticed: only the per-request check stands between the key and the new listener"
    );
    assert_eq!(full.health.view().upstreams[0].state, UpstreamState::Ready);

    let (status, _) = full.chat().await;
    assert_ne!(status, StatusCode::OK, "the very next chat request");
    let (status, body) = full.data("GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"], json!([]), "{body}");
    let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = body["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["provider_id"] == "omlx")
        .unwrap_or_else(|| panic!("no source for omlx: {body}"));
    assert_eq!(source["error"], "unavailable", "{source}");

    assert_eq!(
        impostor.count().await,
        0,
        "not one request reached the new listener"
    );
    impostor.assert_never_authorized().await;
    for request in impostor.requests().await {
        assert!(!format!("{:?}", request.headers).contains(&private_key));
    }
    assert_eq!(
        tool_calls(&log),
        settled,
        "refusing the requests started no child process either"
    );
}

/// **一个并存的监听者，由监督节拍的「唯一的监听者」问题抓到，之后没有一个请求发给那个端口。** 引擎自己的 LISTEN 套接字还在（每个请求的原生确认
/// 永远说「还握着」），另一个进程也在这个端口上监听（`lose`：`listens_on` 回 `false`）。每个请求的确认看不见它，那是它的窗口（规范勘误十（8）：
/// 「自上一个节拍起」）；管理器的下一个节拍问「唯一的监听者」，收回私有 key、端点封上、状态 `unavailable`（`port_in_use`）。之后聊天、公共模型列表、
/// Admin 的模型观察都不再发往那个端口（引擎的请求记录不再增加），好几个节拍里也没有探测带着 key 过去。
#[tokio::test]
async fn h1_a_second_listener_next_to_the_engines_own_socket_is_caught_within_a_tick_and_nothing_is_sent_to_the_port_after_it()
 {
    if !in_a_child_with_canary_keys(
        "h1_a_second_listener_next_to_the_engines_own_socket_is_caught_within_a_tick_and_nothing_is_sent_to_the_port_after_it",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let inspector = Scripted::claiming(port);
    inspector.offer_a_native_watch_that_always_holds();
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Private(full.rig.key(0))),
        "the engine is registered with its private key"
    );

    // 另一个进程也监听了这个端口；引擎自己的套接字没动。
    inspector.lose(port);

    // 下一个节拍的「唯一的监听者」问题抓到它：收回私有 key、`unavailable` 加 `port_in_use`。
    wait_for("unavailable, port_in_use", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Unavailable
            && upstream.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "the tick revoked the private key"
    );
    assert!(
        full.logged("收回私有 key") >= 1,
        "it was the supervision tick that noticed"
    );

    // 之后：不再有任何请求发往那个端口。
    let before = full.rig.fake.requests().len();
    let (status, body) = full.chat().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, body) = full.data("GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"], json!([]), "{body}");
    let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 好几个节拍（`fast()` 是 50 毫秒一次）：管理器自己也不带 key 探测它。
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        full.rig.fake.requests().len(),
        before,
        "a request reached the port after the tick revoked the key: {:?}",
        full.rig.fake.requests()
    );
}

// ---------------------------------------------------------------------------------------------
// H2：外部授权在每一条没有落到「持续监督」上的出口都撤销
// ---------------------------------------------------------------------------------------------

/// 接管外部引擎之后，Supervisor 的复位失败（新进程的 `/api/status` 答不上）：这个实例不再监督它，`self.external` 被清掉；之前已验证的登记留着，
/// 端口上之后出现的东西，只要守卫恰好通过，仍能拿到 `settings.json` 的 key。现在端点封上。退避期间（20 秒）断言，不会被下一次接管覆盖。
#[tokio::test]
async fn h2_a_failed_ledger_reset_after_an_external_takeover_leaves_the_endpoint_blocked() {
    let work = tempfile::tempdir().unwrap();
    let mut a = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = a.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let endpoint = rig.endpoint();
    let inspector = Scripted::new();
    inspector.external_listener(port, a.pid(), Some(TEAM));
    let mut timings = fast();
    // 失败之后退避很久：用例断言的那一刻，实例正在退避，不会又去接管一次。
    timings.restart_backoff_base = Duration::from_secs(20);
    timings.restart_backoff_cap = Duration::from_secs(30);
    timings.crash_limit = 10;
    let full = Full::start_with(rig, inspector.clone(), timings).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // A 关了：端口上没有监听者，`unavailable`，端点封上。
    inspector.forget_listener(port);
    full.until_unavailable().await;
    a.kill();
    // B 在同一个端口上起来了，它的 `/api/status` 一直答 500（对着它重建账本会失败）。操作员 `start`：新一轮启动决策接管 B，门控早已放行，
    // 所以接管之后要对着 B 复位账本。
    let behavior = Behavior {
        api_status_fails_from_launch: Some(0),
        ..stateful()
    };
    let b = External::start_on(work.path(), "external-b", &behavior, port);
    inspector.external_listener(port, b.pid(), Some(TEAM));
    let (status, body) = full.post_command("start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let latched = full
        .until("the reset to fail", |element| {
            element["state"] == "restarting" && element["reason"] == "startup_latched"
        })
        .await;
    assert!(latched["failure_count"].as_u64().unwrap() >= 1, "{latched}");
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "the verified external registration was left behind after the takeover failed"
    );
    // 管理器不再监督 B；数据面 503，B 收不到聊天。
    assert_eq!(full.chat().await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b.fake.requests_of("POST:/v1/chat/completions"), 0);
}

// ---------------------------------------------------------------------------------------------
// H5：伴生卡按明确的托管实例关联，不按主机名的写法
// ---------------------------------------------------------------------------------------------

/// 主卡 `127.0.0.1:<端口>`，伴生卡写成 `localhost:<端口>`（IPv4 的冒名者）或 `[::1]:<端口>`（IPv6 的冒名者）：`stop` 做完、端点封上之后，
/// 别名上的转发、预热、模型列表、Admin 的模型观察、后台探测都被挡在状态门外（伴生卡继承主卡的停止），冒名者收不到一个请求，更没有
/// `Authorization`：之前登记与继承都按 URL 的 host 字符串匹配，别名落进了没人管的端点、走 §2.9 的回退链，金丝雀 key 发了出去。
#[tokio::test]
async fn h5_a_companion_card_spelled_with_a_loopback_alias_is_gated_and_never_handed_a_credential_after_a_stop()
 {
    if !in_a_child_with_canary_keys(
        "h5_a_companion_card_spelled_with_a_loopback_alias_is_gated_and_never_handed_a_credential_after_a_stop",
    ) {
        return;
    }
    // (别名的写法, 冒名者绑的地址)
    for (alias, impostor_address) in [("localhost", "127.0.0.1"), ("[::1]", "::1")] {
        let rig = Rig::new(&stateful());
        let port = rig.port;
        let alias_endpoint = format!("http://{alias}:{port}");
        let companion = companion_card(&alias_endpoint);
        let full = Arc::new(
            Full::start_with_cards(
                rig,
                Scripted::claiming(port),
                fast(),
                vec![companion.clone()],
            )
            .await,
        );
        full.wait_open().await;
        assert_eq!(
            full.chat().await.0,
            StatusCode::OK,
            "{alias}: it serves while the engine is up"
        );

        let (status, body) = full.post_command("stop").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{alias}: {body}");
        full.until_gone().await;
        assert_eq!(
            governed_endpoint(&full.rig.endpoint()),
            Some(EndpointKey::Blocked),
            "{alias}"
        );
        assert_eq!(
            governed_endpoint(&alias_endpoint),
            Some(EndpointKey::Blocked),
            "{alias}: the alias lands on the same registration"
        );
        // 冒名者绑上这个端口（IPv6 的别名对应 IPv6 的地址；没有 IPv6 loopback 的机器跳过这一个）。
        let Ok(listener) = std::net::TcpListener::bind((impostor_address, port)) else {
            eprintln!("skipping {alias}: cannot bind {impostor_address}:{port} here");
            continue;
        };
        let impostor = Impostor::on(listener).await;

        // 转发：伴生卡继承主卡的停止，状态门 503，一个请求也不碰那个端口。
        let (status, body) = full
            .data(
                "POST",
                "/v1/embeddings",
                Some(json!({"model": "idoris/embed", "input": "hello"})),
            )
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{alias}: {body}");
        assert_eq!(
            body["error"]["type"], "local_only_unavailable",
            "{alias}: {body}"
        );
        // 预热。
        let (status, body) = full
            .data(
                "POST",
                "/v1/models/warmup",
                Some(json!({"model": "idoris/embed"})),
            )
            .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{alias}: {body}");
        // 公共模型列表：两张卡都不探测。
        let (status, body) = full.data("GET", "/v1/models", None).await;
        assert_eq!(status, StatusCode::OK, "{alias}: {body}");
        assert_eq!(body["data"], json!([]), "{alias}: {body}");
        // Admin 的模型观察：伴生卡如实报「不可用」。
        let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
        assert_eq!(status, StatusCode::OK, "{alias}: {body}");
        let source = body["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["provider_id"] == "omlx-embed")
            .unwrap_or_else(|| panic!("{alias}: no source for the companion: {body}"));
        assert_eq!(source["state"], "error", "{alias}: {source}");
        // 后台探测。
        let context = ProberContext {
            health: full.health.clone(),
            plans: plan_upstreams(std::slice::from_ref(&companion)).unwrap(),
            runtimes: RuntimeRegistry::default(),
            client: reqwest::Client::new(),
            remote_kill_switch: Arc::new(AtomicBool::new(false)),
            timings: ProberTimings::default(),
        };
        assert!(
            matches!(
                context.observe(&context.plans[0]).await,
                Observation::Unavailable(_)
            ),
            "{alias}"
        );
        assert_eq!(
            impostor.count().await,
            0,
            "{alias}: nothing reached the impostor"
        );
        impostor.assert_never_authorized().await;
        drop(impostor);
        full.manager.shutdown(true).await;
    }
}

// ---------------------------------------------------------------------------------------------
// H6：外部引擎换进程：明确的不可服务状态，排空之后重新确认身份
// ---------------------------------------------------------------------------------------------

/// A 换成 B：管理器关门、发布 `restarting`（不是数据面当成「只是慢」的 `unavailable`），伴生卡也被状态门挡住；B 在排空期间离开了：排空
/// 之后重新核对监听者，不是预期的那一个，不登记、不复位、不放行；之前排空前的结论直接恢复凭证、对着已经走了的 B 复位采样。
#[tokio::test]
async fn h6_an_external_changeover_is_unservable_at_once_and_is_re_verified_after_the_drain() {
    let work = tempfile::tempdir().unwrap();
    let a = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = a.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let endpoint = rig.endpoint();
    let inspector = Scripted::new();
    inspector.external_listener(port, a.pid(), Some(TEAM));
    let full = Arc::new(
        Full::start_with_cards(
            rig,
            inspector.clone(),
            fast(),
            vec![companion_card(&endpoint)],
        )
        .await,
    );
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 一个聊天停在 A 里：排空要等它。
    a.fake.hold(Hold::Chat);
    let chats = a.fake.requests_of("POST:/v1/chat/completions");
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the chat to reach A", || {
        a.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;

    // 检查器说监听这个端口的换成了另一个（通过校验的）进程 B（pid、启动时间都不同）。
    let b_pid = std::process::id();
    inspector.external_listener(port, b_pid, Some(TEAM));
    wait_for("the changeover to be published as unservable", || {
        full.health.view().upstreams[0].state == UpstreamState::Restarting
    })
    .await;
    let element = full.element("omlx").await;
    assert_eq!(
        element["failure_count"], 0,
        "a changeover of the external process is not an engine failure: {element}"
    );
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "nothing is sent while the ledger is rebuilt"
    );
    // 伴生卡此刻被状态门挡住（继承了主卡的 `restarting`）：它不能在账本复位完成之前通过健康门。
    let (status, body) = full
        .data(
            "POST",
            "/v1/embeddings",
            Some(json!({"model": "idoris/embed", "input": "hello"})),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable", "{body}");

    // B 在排空期间离开了；放开 A 里的聊天，排空结束。
    inspector.forget_listener(port);
    a.fake.release(Hold::Chat);
    assert_eq!(in_flight.await.unwrap().0, StatusCode::OK);
    full.until_unavailable().await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "the process verified before the drain was gone: no credential was restored for it"
    );
    assert_eq!(full.chat().await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_ne!(full.health.view().upstreams[0].state, UpstreamState::Ready);
}

/// H6 的另一条：排空期间监听者没有离开，而是又换成了**第三个**（同样通过校验的）进程：排空之前验过的 B 已经不是此刻的监听者，不能凭那一次的结论放行
/// （登记它的凭证与新的守卫、复位、开门）。重新核对的那一刻日志里有「又换了一个进程」；下一个周期按第三个进程重新来，之后才回 `ready`。
/// 之前排空之后直接恢复凭证、用排空前的结论复位。
#[tokio::test]
async fn h6_a_third_process_after_the_drain_is_not_let_through_on_the_strength_of_the_one_verified_before()
 {
    let work = tempfile::tempdir().unwrap();
    let a = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = a.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let endpoint = rig.endpoint();
    let inspector = Scripted::new();
    inspector.external_listener(port, a.pid(), Some(TEAM));
    let full = Arc::new(Full::start(rig, inspector.clone()).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 一个聊天停在 A 里：排空要等它。
    a.fake.hold(Hold::Chat);
    let chats = a.fake.requests_of("POST:/v1/chat/completions");
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the chat to reach A", || {
        a.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;

    // A 换成了 B（通过校验）；排空开始，状态是 `restarting`。
    let b_pid = std::process::id();
    let c_pid = std::os::unix::process::parent_id();
    assert_ne!(b_pid, c_pid);
    inspector.external_listener(port, b_pid, Some(TEAM));
    wait_for("the changeover to be published as unservable", || {
        full.health.view().upstreams[0].state == UpstreamState::Restarting
    })
    .await;
    // 排空还在等 A 里的聊天时，B 又被第三个进程 C 取代。
    inspector.external_listener(port, c_pid, Some(TEAM));
    a.fake.release(Hold::Chat);
    assert_eq!(in_flight.await.unwrap().0, StatusCode::OK);

    wait_for(
        "the re-verification after the drain to refuse the process verified before it",
        || full.logged("外部引擎在排空期间又换了一个进程") >= 1,
    )
    .await;
    // 下一个周期按第三个进程重新来：最终放行它（引擎还是同一个假引擎在应答），而且是以 C 的身份。
    full.until("the third process to be let through", |element| {
        element["state"] == "ready"
    })
    .await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::External {
            settings_key_verified: true
        })
    );
    assert_eq!(full.chat().await.0, StatusCode::OK);
}

/// H4（Router 层）：Supervisor 经适配器的 Admin API 读引擎的活动（`status()`）；放行时的第一次采样登录并缓存了那个进程签发的会话 cookie。引擎被
/// `restart` 换成新进程之后，复位采样又读一次：旧进程签发的 cookie 属于上一代引擎，**不能发给新进程**；适配器丢弃它、对着新进程重新登录。
/// 之前缓存命中时只看有没有 cookie、不看它是哪一代签发的，新进程先收到旧会话的 cookie（假引擎把别的进程签发的 cookie 记成 `bad`）。
#[tokio::test]
async fn h4_the_admin_session_of_the_old_engine_is_never_sent_to_the_replacement() {
    let rig = Rig::new(&Behavior {
        admin_api: true,
        ..stateful()
    });
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    // 放行时的采样经 Admin API 读了第一个引擎的活动：登录一次，之后带着它自己签发的 cookie 读。
    let first = full.rig.pid(0);
    wait_for("the first engine's admin session to be used", || {
        full.rig
            .fake
            .admin_requests()
            .iter()
            .any(|r| r.pid == first && r.path == "/admin/api/activity" && r.cookie == "ok")
    })
    .await;

    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the replacement to be ready", || {
        full.rig.fake.launches() == 2
            && full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    let second = full.rig.pid(1);
    let seen: Vec<_> = full
        .rig
        .fake
        .admin_requests()
        .into_iter()
        .filter(|r| r.pid == second)
        .collect();
    assert!(
        !seen.is_empty(),
        "the replacement's reset sample read the engine's activity through the admin API"
    );
    assert!(
        seen.iter().all(|r| r.cookie != "bad"),
        "the old engine's admin session was sent to the replacement: {seen:?}"
    );
    assert_eq!(
        seen[0].path, "/admin/api/login",
        "the replacement is logged in to first: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|r| r.path == "/admin/api/activity" && r.cookie == "ok"),
        "{seen:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// M1：排空要看见列表、resident 转发、后台预热、仍在输出的流式响应体
// ---------------------------------------------------------------------------------------------

/// `held` 的那一类请求已经在引擎里、还没做完（调用方起好并等它到达）；这时 `restart` 到来：状态马上是 `restarting`，旧引擎在这些请求做完之前
/// **不被停止**（SIGTERM 的标记不出现、没有第二个引擎）。返回旧引擎的 pid。
async fn restart_and_expect_the_drain_to_wait(full: &Full, held: Hold) -> u64 {
    let old = full.rig.pid(0);
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "restarting", "{body}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(pid_alive(old), "{held:?}");
    assert!(
        !full.rig.fake.got_sigterm(old),
        "{held:?}: the engine was stopped before the request in flight was done"
    );
    assert_eq!(full.rig.fake.launches(), 1, "{held:?}");
    old
}

/// 放开之后：旧引擎才被停、新引擎换上；排空是等出来的，不是到点切断的。
async fn release_and_expect_the_replacement(full: &Full, held: Hold, old: u64) {
    full.rig.fake.release(held);
    wait_for("the old engine to get SIGTERM", || {
        full.rig.fake.got_sigterm(old)
    })
    .await;
    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    assert_eq!(full.rig.fake.launches(), 2, "{held:?}");
    assert_eq!(
        full.logged("排空的上限到了"),
        0,
        "{held:?}: the drain was waited for, not cut off at its limit"
    );
}

/// 一个已准入的目录读取（`list`，预热在加载之前读它）只持有 Supervisor 的调用许可：不在任何模型的 `inflight` 里。排空看不见它，就去停进程。
#[tokio::test]
async fn m1_a_catalog_read_in_flight_keeps_the_drain_open() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let full = Arc::new(Full::start(rig, Scripted::claiming(port)).await);
    full.wait_open().await;
    full.rig.fake.hold(Hold::List);
    let before = full.rig.fake.requests_of("/v1/models/status");
    let warmup = {
        let full = full.clone();
        tokio::spawn(async move {
            full.data(
                "POST",
                "/v1/models/warmup",
                Some(json!({"model": "idoris/fast"})),
            )
            .await
        })
    };
    wait_for("the catalog read to reach the engine", || {
        full.rig.fake.requests_of("/v1/models/status") > before
    })
    .await;
    let old = restart_and_expect_the_drain_to_wait(&full, Hold::List).await;
    full.rig.fake.release(Hold::List);
    // 目录读取放开之后，预热接着去加载，被关着的门拒绝：不是 200。
    let (status, _) = warmup.await.unwrap();
    assert_ne!(status, StatusCode::OK);
    release_and_expect_the_replacement(&full, Hold::List, old).await;
}

/// 公共模型列表对托管引擎的读取（`GET /v1/models`，`models.rs` 里自己的授权，不经 Supervisor）：它是端点上登记的一个在途操作，排空要等它做完。
/// 监督的节拍拉到小时级，管理器自己不再探测 `/v1/models`，所以引擎里这个路径上被扣着的请求只可能来自这次列表。
#[tokio::test]
async fn m1_a_model_list_request_in_flight_keeps_the_drain_open() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let full =
        Arc::new(Full::start_with(rig, Scripted::claiming(port), supervision_paused()).await);
    full.wait_open().await;
    full.rig.fake.hold(Hold::Models);
    let before = full.rig.fake.requests_of("/v1/models");
    let listing = {
        let full = full.clone();
        tokio::spawn(async move { full.data("GET", "/v1/models", None).await })
    };
    wait_for("the model list request to reach the engine", || {
        full.rig.fake.requests_of("/v1/models") > before
    })
    .await;
    let old = restart_and_expect_the_drain_to_wait(&full, Hold::Models).await;
    full.rig.fake.release(Hold::Models);
    assert_eq!(listing.await.unwrap().0, StatusCode::OK);
    release_and_expect_the_replacement(&full, Hold::Models, old).await;
}

/// resident 的伴生卡经代理转发的请求（不经 Supervisor）：没有 Supervisor 可问，排空要等的是这个端点上登记的在途操作。
#[tokio::test]
async fn m1_a_forwarded_request_in_flight_keeps_the_drain_open() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let full = Arc::new(
        Full::start_with_cards(
            rig,
            Scripted::claiming(port),
            fast(),
            vec![companion_card(&endpoint)],
        )
        .await,
    );
    full.wait_open().await;
    full.rig.fake.hold(Hold::Embeddings);
    let forwarded = {
        let full = full.clone();
        tokio::spawn(async move {
            full.data(
                "POST",
                "/v1/embeddings",
                Some(json!({"model": "idoris/embed", "input": "hello"})),
            )
            .await
        })
    };
    wait_for("the forwarded request to reach the engine", || {
        full.rig.fake.requests_of("POST:/v1/embeddings") == 1
    })
    .await;
    let old = restart_and_expect_the_drain_to_wait(&full, Hold::Embeddings).await;
    full.rig.fake.release(Hold::Embeddings);
    assert_eq!(forwarded.await.unwrap().0, StatusCode::OK);
    release_and_expect_the_replacement(&full, Hold::Embeddings, old).await;
}

/// 后台预热（resident 的最小请求在自己的任务里跑，调用方放弃之后照常跑完）：它活到引擎被停止、替换之后也不该被排空漏掉。
#[tokio::test]
async fn m1_a_background_warmup_in_flight_keeps_the_drain_open() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let full = Arc::new(
        Full::start_with_cards(
            rig,
            Scripted::claiming(port),
            fast(),
            vec![companion_card(&endpoint)],
        )
        .await,
    );
    full.wait_open().await;
    full.rig.fake.hold(Hold::Embeddings);
    let warmup = {
        let full = full.clone();
        tokio::spawn(async move {
            full.data(
                "POST",
                "/v1/models/warmup",
                Some(json!({"model": "idoris/embed", "timeout_ms": 1000})),
            )
            .await
        })
    };
    wait_for("the warmup request to reach the engine", || {
        full.rig.fake.requests_of("POST:/v1/embeddings") == 1
    })
    .await;
    let old = restart_and_expect_the_drain_to_wait(&full, Hold::Embeddings).await;
    full.rig.fake.release(Hold::Embeddings);
    let _ = warmup.await.unwrap();
    release_and_expect_the_replacement(&full, Hold::Embeddings, old).await;
}

/// 已经收到响应头与第一块、引擎还在往外输出的流式响应体：它是端点上的一个在途操作，直到响应体结束或被丢弃。之前没有谁数它，排空以为没有在途的，
/// 停进程把流打断了。
#[tokio::test]
async fn m1_a_streaming_body_that_is_still_coming_keeps_the_drain_open() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let full = Arc::new(
        Full::start_with_cards(
            rig,
            Scripted::claiming(port),
            fast(),
            vec![resident_chat_card(&endpoint)],
        )
        .await,
    );
    full.wait_open().await;
    full.rig.fake.hold(Hold::Stream);
    let chats = full.rig.fake.requests_of("POST:/v1/chat/completions");
    let response = full
        .data_raw(
            "POST",
            "/v1/chat/completions",
            json!({"model": "idoris/daily", "stream": true,
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    // 响应头已经到了（引擎发出了第一块），响应体还没读完：流是开着的。
    assert_eq!(response.status(), StatusCode::OK);
    wait_for("the streaming request to reach the engine", || {
        full.rig.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;
    let old = restart_and_expect_the_drain_to_wait(&full, Hold::Stream).await;
    // 放开：引擎把流写完；读完响应体，流结束，在途登记归零，排空才结束。
    full.rig.fake.release(Hold::Stream);
    let text = body_text(response).await;
    assert!(text.contains("[DONE]"), "the stream ran to its end: {text}");
    release_and_expect_the_replacement(&full, Hold::Stream, old).await;
}

async fn body_text(response: axum::http::Response<Body>) -> String {
    String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes()).into_owned()
}

// ---------------------------------------------------------------------------------------------
// M2：重启之后新引擎起不来，旧引擎的容量占用也要放掉
// ---------------------------------------------------------------------------------------------

/// `stop` 确认旧进程消失之后清算它的账本（`forget_engine`），`restart` 之前丢弃退役结果、等新引擎就绪才复位：旧进程确已停止、替代的可执行文件却
/// 找不到（实例进入 `absent`）时，旧模型的容量占用一直留在账本与共享的容量账本里，挡着别的上游。现在旧进程一确认消失就清算，门保持关着。
#[tokio::test]
async fn m2_when_the_replacement_cannot_start_the_old_engines_models_stop_counting() {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let before = full.runtime().await;
    assert_eq!(before["loaded"], json!(["fake-model"]), "{before}");
    assert!(before["used_gb"].as_f64().unwrap() > 0.0, "{before}");

    // 可执行文件没了：替代的引擎起不来。
    std::fs::remove_file(&full.rig.fake.executable).unwrap();
    let old = full.rig.pid(0);
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    full.until("the replacement to be impossible", |element| {
        element["state"] == "absent"
    })
    .await;
    wait_for("the old engine to be gone", || !pid_alive(old)).await;
    let after = full.runtime().await;
    assert_eq!(
        after["loaded"],
        json!([]),
        "the old model still counts: {after}"
    );
    assert_eq!(after["used_gb"].as_f64().unwrap(), 0.0, "{after}");
}

/// 同上，`start` 的那一条路：一个活着、监听着、却答不上来的自有引擎（`unavailable`）被 `start`：走完整的退役路径（`attempt()` 开头），旧进程一确认
/// 消失就清算它的账本，不等新引擎就绪；替代的可执行文件找不到（`absent`）时，旧模型也不再占着容量。
#[tokio::test]
async fn m2_start_on_an_alive_but_unresponsive_engine_settles_its_models_when_the_replacement_cannot_start()
 {
    let rig = Rig::new(&stateful_with_term_marker());
    let port = rig.port;
    let full = Full::start(rig, Scripted::claiming(port)).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let before = full.runtime().await;
    assert_eq!(before["loaded"], json!(["fake-model"]), "{before}");
    assert!(before["used_gb"].as_f64().unwrap() > 0.0, "{before}");

    // 活着、监听着、但「不答」：探测拿到 503，状态 `unavailable`，进程与句柄都还在。
    full.rig.fake.force_status(503);
    full.until("the engine to stop answering", |element| {
        element["state"] == "unavailable"
    })
    .await;
    let old = full.rig.pid(0);
    assert!(pid_alive(old));

    // 可执行文件没了：替代的引擎起不来。
    std::fs::remove_file(&full.rig.fake.executable).unwrap();
    let (status, body) = full.post_command("start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    full.until("the replacement to be impossible", |element| {
        element["state"] == "absent"
    })
    .await;
    wait_for("the old engine to be gone", || !pid_alive(old)).await;
    let after = full.runtime().await;
    assert_eq!(
        after["loaded"],
        json!([]),
        "the old model still counts: {after}"
    );
    assert_eq!(after["used_gb"].as_f64().unwrap(), 0.0, "{after}");
}

// ---------------------------------------------------------------------------------------------
// M5：命令的绝对截止时间，超时的命令不会事后执行
// ---------------------------------------------------------------------------------------------

/// 实例任务正忙：一个聊天停在引擎里，`restart` 的排空在等它。老引擎还握在手里（元素的 `owned` 仍是 true），所以一个 `stop` 过得了 Admin 的预检、
/// 排进邮箱，而实例任务不去收它。它在 `command_timeout`（这里 400 毫秒）的绝对截止时间放弃，Admin 回 503 `idoris_stopping`；放开之后 `restart`
/// 做完，引擎仍是 `ready`，日志里没有「已受理」的 stop。**这一条只证明 Admin 一层的映射**：这个窗口里被放弃的 `stop` 之后被实例任务收到时，
/// 新引擎还没登记（`engine` 是空），它本来就只会得到 `NotOwned`；「事后不会执行」要在下一个用例的窗口里才测得出来。
#[tokio::test]
async fn m5_a_stop_the_busy_instance_cannot_take_in_time_is_abandoned_and_never_runs_later() {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let mut timings = fast();
    timings.command_timeout = Duration::from_millis(400);
    let full = Arc::new(Full::start_with(rig, Scripted::claiming(port), timings).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 一个聊天已经到了引擎、还没答；`restart` 受理之后排空在等它，实例任务正忙。
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
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["owned"], true, "the old engine is still held: {body}");

    // 这时的 `stop` 过了预检、排在队里，实例任务不去收它：到截止时间放弃。
    let started = Instant::now();
    let (status, body) = full.post_command("stop").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["type"], "idoris_stopping", "{body}");
    assert!(
        started.elapsed() >= Duration::from_millis(350)
            && started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    // 放开：在途的聊天做完，`restart` 做完，引擎回到 `ready`；被放弃的 `stop` 没有被执行。
    full.rig.fake.release(Hold::Chat);
    assert_eq!(in_flight.await.unwrap().0, StatusCode::OK);
    wait_for("the replacement to be ready", || {
        full.rig.fake.launches() == 2
            && full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 邮箱是先进先出：这个无害的 `start`（就绪的引擎上是幂等的 202）要等排在它前面的每条命令（含那个被放弃的 `stop`）都处理过才有回答，
    // 回答之后再看，被放弃的 `stop` 已经有过执行的机会；取代了原来的「睡 700 毫秒」。
    let (status, body) = full.post_command("start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(full.health.view().upstreams[0].state, UpstreamState::Ready);
    assert_eq!(full.rig.fake.launches(), 2);
    assert_eq!(
        full.logged("收到命令 stop，已受理"),
        0,
        "the abandoned stop must not run later"
    );
    assert_eq!(full.chat().await.0, StatusCode::OK);
}

/// M5 的「事后不执行」：新引擎已经起来、已经登记（`engine` 不空），实例任务正忙在账本复位的采样里（`GET /api/status` 被引擎扣着，发布的状态还是
/// `restarting`、`owned` 还是 false，所以 Admin 的预检挡着 `stop`，这一步直接走管理器的接口：Admin 调的就是它）。一个 `stop` 排进邮箱，实例任务不去
/// 收它，到 `command_timeout` 放弃（`Closed`）。放开之后复位做完、发布 `ready`，实例任务这才收到那个已经过期的 `stop`：不执行（回答 `Closed`），
/// 刚换上的引擎还在、仍是 `ready`。之前命令不带截止时间，它在这时被执行，把刚换上的引擎停掉。
#[tokio::test]
async fn m5_a_command_abandoned_while_the_instance_was_busy_in_the_reset_is_never_executed_afterwards()
 {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let mut timings = fast();
    timings.command_timeout = Duration::from_millis(400);
    let full = Arc::new(Full::start_with(rig, Scripted::claiming(port), timings).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 新引擎起来之后，复位的那次采样（`GET /api/status`）停在引擎里：实例任务在等它，不去收命令。
    let samples = full.rig.fake.requests_of("/api/status");
    full.rig.fake.hold(Hold::Status);
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the reset sample to reach the replacement", || {
        full.rig.fake.launches() == 2 && full.rig.fake.requests_of("/api/status") > samples
    })
    .await;

    let started = Instant::now();
    let result = full.manager.stop_upstream("omlx").await;
    assert_eq!(result, Err(idoris_router::lifecycle::CommandError::Closed));
    assert!(
        started.elapsed() >= Duration::from_millis(350)
            && started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    // 放开：复位做完，引擎回到 `ready`；那个被放弃的 `stop` 没有被执行。
    full.rig.fake.release(Hold::Status);
    wait_for("the replacement to be ready", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    // 先进先出的哨兵：就绪的引擎上 `start` 是幂等的 202，要等排在它前面的每条命令（含那个被放弃的 `stop`）都处理过才有回答；取代了原来的「睡 700 毫秒」。
    let (status, body) = full.post_command("start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        full.health.view().upstreams[0].state,
        UpstreamState::Ready,
        "the abandoned stop was executed after its caller had given up"
    );
    assert_eq!(full.rig.fake.launches(), 2);
    assert!(pid_alive(full.rig.pid(1)), "the replacement was stopped");
    assert_eq!(full.chat().await.0, StatusCode::OK);
}

// ---------------------------------------------------------------------------------------------
// M6：模型列表与 Admin 的模型观察带着准入时的代际，被封上或换代的端点跳过
// ---------------------------------------------------------------------------------------------

/// 一个对 `GET /v1/models` 不答、直到被放行的上游（慢卡）：到达与放行都是同步点。
struct SlowUpstream {
    port: u16,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl SlowUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (arrive, go) = (reached.clone(), release.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (arrive, go) = (arrive.clone(), go.clone());
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buffer = [0_u8; 4096];
                    let _ = socket.read(&mut buffer).await;
                    arrive.notify_one();
                    go.notified().await;
                    let body =
                        br#"{"object":"list","data":[{"id":"slow-model","object":"model"}]}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        });
        Self {
            port,
            reached,
            release,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// 列表（公共的或 Admin 的）正卡在前一张慢卡上；托管的引擎这时被停掉、端口被别的东西占了；放行之后列表走到托管的那张卡：它的端点被封上了，
/// 逐卡请求发出前被拒绝，**一个请求也没有发给占着那个端口的东西**。之前先筛整批卡、再逐卡请求，`Blocked` 被当成「没有 key」，照样发一个匿名的
/// GET，未验证的监听者的回答还会被采纳为这张卡的模型列表。
async fn listing_while_the_managed_card_is_stopped(uri: &str, admin: bool) {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let slow = SlowUpstream::start().await;
    let cards = vec![
        upstream_card(&slow.endpoint()),
        managed_card(&rig.endpoint()),
    ];
    let full =
        Arc::new(Full::start_with_card_order(rig, Scripted::claiming(port), fast(), cards).await);
    full.wait_open().await;
    let listing = {
        let full = full.clone();
        let uri = uri.to_owned();
        tokio::spawn(async move {
            if admin {
                full.admin("GET", &uri).await
            } else {
                full.data("GET", &uri, None).await
            }
        })
    };
    // 列表已经在第一张卡（慢卡）的请求里：状态检查与准入时的代际早已读过，托管的那张卡还没轮到。
    slow.reached.notified().await;
    let (status, body) = full.post_command("stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    full.until_gone().await;
    let impostor = Impostor::on_port(port).await;
    slow.release.notify_one();
    let (status, body) = listing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        impostor.count().await,
        0,
        "the stopped engine's endpoint was asked for its models: {:?}",
        impostor.requests().await
    );
    impostor.assert_never_authorized().await;
    if !admin {
        // 慢卡的模型列出来了，托管的那张卡不可用，没有冒名者的东西。
        let listed: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed, vec!["slow-model"], "{body}");
    }
}

/// 同上，换代而不是停止：公共列表卡在前一张慢卡上时，托管的引擎被 `restart` 到做完（新进程、新的私有 key、更新的代际）。放行之后列表走到托管的
/// 那张卡：它是为旧引擎准入的（代际先于挑选这批卡的状态检查读），代际对不上，逐卡请求发出前被拒绝，**新引擎没有收到这一次列表的任何请求**
/// （更没有带着它的新 key）。之前 `fetch_models_body` 不带准入代际，换了代的端点会改用新一代的凭证。监督的节拍拉到小时级，所以新引擎收到的
/// `GET /v1/models` 只可能来自这次列表。
#[tokio::test]
async fn m6_the_public_model_list_never_asks_the_replacement_of_a_card_replaced_while_a_slower_card_was_being_listed()
 {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let slow = SlowUpstream::start().await;
    let cards = vec![
        upstream_card(&slow.endpoint()),
        managed_card(&rig.endpoint()),
    ];
    let full = Arc::new(
        Full::start_with_card_order(rig, Scripted::claiming(port), supervision_paused(), cards)
            .await,
    );
    full.wait_open().await;
    let listing = {
        let full = full.clone();
        tokio::spawn(async move { full.data("GET", "/v1/models", None).await })
    };
    // 列表已经在第一张卡（慢卡）的请求里：准入时的代际早已读过，托管的那张卡还没轮到。
    slow.reached.notified().await;
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the replacement to be ready", || {
        full.rig.fake.launches() == 2
            && full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    let asked = full.rig.fake.requests_of("/v1/models");
    slow.release.notify_one();
    let (status, body) = listing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        full.rig.fake.requests_of("/v1/models"),
        asked,
        "the listing admitted for the old engine asked the new one (with the new engine's key)"
    );
    let listed: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["slow-model"], "{body}");
}

#[tokio::test]
async fn m6_the_public_model_list_never_asks_a_card_stopped_while_a_slower_card_was_being_listed() {
    listing_while_the_managed_card_is_stopped("/v1/models", false).await;
}

#[tokio::test]
async fn m6_the_admin_model_observation_never_asks_a_card_stopped_while_a_slower_card_was_being_observed()
 {
    listing_while_the_managed_card_is_stopped("/admin/api/v1/models", true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 专项评审（d900486d）的必修项：H1、H2、H3、H6、H7、M9、M10、M11、M13、L14。真的 Router、真的假引擎；每个用例都在带金丝雀 key 的子进程里再跑一遍
// （冒名者收到任何 `Authorization` 都是失败，不只是金丝雀的值）。时序用同步点到达，不靠墙钟。
// ---------------------------------------------------------------------------------------------

/// 一个在 IPv4 回环上空着、而且 `[::1]` 上已经有监听套接字的端口；这台机器没有 IPv6 回环时是 `None`。
fn ipv6_listener_on_a_port_free_on_ipv4() -> Option<(u16, std::net::TcpListener)> {
    for _ in 0..50 {
        let port = free_port();
        if let Ok(listener) = std::net::TcpListener::bind(("::1", port)) {
            return Some((port, listener));
        }
    }
    None
}

fn has_lsof() -> bool {
    ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .iter()
        .any(|path| std::path::Path::new(path).is_file())
}

/// 等到没有任何东西在监听这个端口（引擎放掉它是异步的）。
async fn wait_until_nothing_listens(port: u16) {
    wait_for("the port to be free", || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
    })
    .await;
}

// ----- H1：核验过的地址就是拨号的地址 ------------------------------------------------------------------------------------------

/// 引擎在 `127.0.0.1:<端口>`，一个冒名者在**同一个端口**的 `[::1]` 上。伴生卡写成 `http://[::1]:<端口>`（登记表把它与主卡并成同一个端口身份）：它的
/// 转发、预热、公共模型列表、Admin 的模型观察、后台探测都必须拨守卫验证过的 `127.0.0.1`，不能拨卡里写的 `[::1]`。冒名者**一个请求**也没收到，更没有
/// `Authorization`。之前授权只核对「端口上的是引擎」，发送端照卡里的写法去拨，请求（连同引擎的私有 key）落在 `[::1]` 的冒名者手里。
#[tokio::test]
async fn fr_h1_a_companion_card_spelled_as_ipv6_loopback_is_dialed_at_the_verified_ipv4_address() {
    if !in_a_child_with_canary_keys(
        "fr_h1_a_companion_card_spelled_as_ipv6_loopback_is_dialed_at_the_verified_ipv4_address",
    ) {
        return;
    }
    let Some((port, listener)) = ipv6_listener_on_a_port_free_on_ipv4() else {
        eprintln!("no IPv6 loopback on this machine: nothing to show");
        return;
    };
    let impostor = Impostor::on(listener).await;
    let mut rig = Rig::new(&stateful());
    rig.port = port;
    let companion = resident_chat_card(&format!("http://[::1]:{port}"));
    let inspector = Scripted::claiming(port);
    let full = Full::start_with_cards(
        rig,
        inspector.clone(),
        supervision_paused(),
        vec![companion.clone()],
    )
    .await;
    full.wait_open().await;
    // 管理器给守卫的拨号地址是 127.0.0.1（托管的引擎只被拨它）。
    assert!(
        inspector
            .watched()
            .iter()
            .all(|(_, dial)| *dial == SocketAddr::new(MANAGED_DIAL, port)),
        "{:?}",
        inspector.watched()
    );

    // 经托管的卡把模型加载上；之后伴生卡（`[::1]` 写法）的聊天经代理发出去。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let chats = full.rig.fake.requests_of("POST:/v1/chat/completions");
    let (status, body) = full
        .data(
            "POST",
            "/v1/chat/completions",
            Some(json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        full.rig.fake.requests_of("POST:/v1/chat/completions"),
        chats + 1,
        "the companion card's chat reached the engine"
    );
    assert!(
        full.rig
            .fake
            .requests()
            .iter()
            .filter(|(path, _)| path == "POST:/v1/chat/completions")
            .all(|(_, auth)| auth == "ok"),
        "{:?}",
        full.rig.fake.requests()
    );

    // 预热（resident 的最小请求）。
    let (status, body) = full
        .data(
            "POST",
            "/v1/models/warmup",
            Some(json!({"model": "idoris/daily"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 公共模型列表：伴生卡与主卡都读到引擎。
    let models = full.rig.fake.requests_of("/v1/models");
    let (status, body) = full.data("GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        full.rig.fake.requests_of("/v1/models") > models,
        "the model list reached the engine"
    );
    assert!(
        !body["data"].as_array().unwrap().is_empty(),
        "the engine's model is listed: {body}"
    );

    // Admin 的模型观察：伴生卡如实读到了引擎。
    let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = body["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["provider_id"] == "omlx-chat")
        .unwrap_or_else(|| panic!("no source for the companion card: {body}"));
    assert_eq!(source["state"], "observed", "{source}");
    assert_eq!(source["models"], json!(["fake-model"]), "{source}");

    // 后台探测：伴生卡经 `[::1]` 写法的探测读到的是引擎，不是冒名者。
    let context = ProberContext {
        health: full.health.clone(),
        plans: plan_upstreams(std::slice::from_ref(&companion)).unwrap(),
        runtimes: RuntimeRegistry::default(),
        client: reqwest::Client::new(),
        remote_kill_switch: Arc::new(AtomicBool::new(false)),
        timings: ProberTimings::default(),
    };
    assert_eq!(
        context.observe(&context.plans[0]).await,
        Observation::Ready,
        "the probe through the [::1] spelling read the engine"
    );

    assert_eq!(
        impostor.count().await,
        0,
        "the IPv6 listener was sent something: {:?}",
        impostor.requests().await
    );
    impostor.assert_never_authorized().await;
}

// ----- H2：IPv6 通配没有「也接 IPv4」的证据，不算 IPv4 拨号地址的监听者 ------------------------------------------------------------

/// 引擎放掉 IPv4 的监听套接字，只在同一个端口上留一个 `IPV6_V6ONLY` 的 `::` 套接字（连到 `127.0.0.1` 的连接到不了它）；冒名者绑上 `127.0.0.1:<端口>`。
/// 引擎在这个端口上**还握着一个 LISTEN 套接字**，但不是 `127.0.0.1` 连得到的那一个：每个请求的确认说不是，聊天、公共模型列表、Admin 的模型观察都在发送前被
/// 拒，冒名者一个请求也没收到。（管理器的监督被暂停：挡在 key 与冒名者之间的只有每个请求的确认。）
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn fr_h2_an_engine_that_keeps_only_an_ipv6_only_socket_is_refused_on_the_very_next_request() {
    if !in_a_child_with_canary_keys(
        "fr_h2_an_engine_that_keeps_only_an_ipv6_only_socket_is_refused_on_the_very_next_request",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let inspector = Arc::new(SystemInspector::new().with_native_probe(true));
    let full = Full::start_with(rig, inspector, supervision_paused()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let private_key = full.rig.key(0);

    let outcome = full.rig.fake.release_port_as(Release::Ipv6Only);
    if outcome != "ok" {
        eprintln!("this machine cannot bind an IPv6-only socket: {outcome}");
        return;
    }
    let impostor = Impostor::on_port(port).await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Private(private_key.clone())),
        "the manager has not noticed: only the per-request check stands between the key and the new listener"
    );

    let (status, _) = full.chat().await;
    assert_ne!(status, StatusCode::OK, "the very next chat request");
    let (status, body) = full.data("GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"], json!([]), "{body}");
    let (status, body) = full.admin("GET", "/admin/api/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        impostor.count().await,
        0,
        "not one request reached the new listener"
    );
    impostor.assert_never_authorized().await;
    for request in impostor.requests().await {
        assert!(!format!("{:?}", request.headers).contains(&private_key));
    }
}

/// 同一件事走 `lsof` 的退路（没有原生监视的平台，或原生读不了那个进程）：`lsof -Fpnt` 的 `t` 行保留地址族，IPv6 通配不当作 IPv4 的监听者。引擎只留一个
/// `IPV6_V6ONLY` 的 `::` 套接字：管理器的下一个节拍问「唯一的监听者」，答案是「不是」（它在这个端口上没有 `127.0.0.1` 连得到的套接字）：收回私有 key、
/// `unavailable` 加 `port_in_use`。之前 `*` 一律当作「任何地址」，这个引擎一直被当成端口的主人。
#[tokio::test]
async fn fr_h2_the_lsof_fallback_keeps_the_address_family_and_the_tick_notices_an_ipv6_only_engine()
{
    if !has_lsof() {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let inspector = Arc::new(SystemInspector::new().with_native_probe(false));
    let full = Full::start(rig, inspector).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Private(full.rig.key(0)))
    );

    let outcome = full.rig.fake.release_port_as(Release::Ipv6Only);
    if outcome != "ok" {
        eprintln!("this machine cannot bind an IPv6-only socket: {outcome}");
        return;
    }
    wait_for("unavailable, port_in_use", || {
        let upstream = &full.health.view().upstreams[0];
        upstream.state == UpstreamState::Unavailable
            && upstream.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Blocked),
        "the tick revoked the private key of an engine whose only socket cannot take connections to 127.0.0.1"
    );
    let _ = port;
}

// ----- H3（Linux）：描述符还在，套接字已经不是 LISTEN ------------------------------------------------------------------------------

/// 引擎不关套接字，对它 `shutdown(SHUT_RD)`：Linux 上监听套接字离开 LISTEN 状态、描述符还开着；冒名者绑上 `127.0.0.1:<端口>`（旧套接字不是 LISTEN，
/// `SO_REUSEADDR` 的绑定不冲突）。之前快路径只看「这个描述符是端口上的一个套接字」，一直说「还握着」，key 发给了冒名者。现在每个请求的确认读到那个套接字
/// 已经不是 LISTEN：下一个请求就被拒。
#[cfg(target_os = "linux")]
#[tokio::test]
async fn fr_h3_a_socket_that_left_listen_with_its_descriptor_kept_is_refused_on_the_very_next_request()
 {
    if !in_a_child_with_canary_keys(
        "fr_h3_a_socket_that_left_listen_with_its_descriptor_kept_is_refused_on_the_very_next_request",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let endpoint = rig.endpoint();
    let inspector = Arc::new(SystemInspector::new().with_native_probe(true));
    let full = Full::start_with(rig, inspector, supervision_paused()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let private_key = full.rig.key(0);

    assert_eq!(full.rig.fake.release_port_as(Release::ShutdownRead), "ok");
    let impostor = Impostor::on_port(port).await;
    assert_eq!(
        governed_endpoint(&endpoint),
        Some(EndpointKey::Private(private_key.clone())),
        "the manager has not noticed"
    );
    let (status, _) = full.chat().await;
    assert_ne!(status, StatusCode::OK, "the very next chat request");
    let (status, body) = full.data("GET", "/v1/models", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"], json!([]), "{body}");
    assert_eq!(
        impostor.count().await,
        0,
        "not one request reached the new listener"
    );
    impostor.assert_never_authorized().await;
}

// ----- H6：引擎换代之后，同一个适配器不复用通往旧进程的连接 ---------------------------------------------------------------------------

/// 外部引擎 A 服务过请求（每个发送的地方都留下了一个通往 A 的连接：Supervisor 的适配器、转发的代理、公共模型列表、后台探测），然后 A 放掉端口**但进程活着、
/// 连接开着**；B 在同一个端口上起来，管理器接管它（新一代）。之后每个发送的地方的下一个请求（账本复位的采样、加载、聊天、转发、模型列表、探测）都必须落在 B，
/// 走新开的连接：之前它们走池里的旧连接，落在 A 身上（A 的请求记录里多出带新引擎 key 的请求），B 收到的反而是空的。
#[tokio::test]
async fn fr_h6_after_an_external_changeover_no_sender_reuses_a_connection_to_the_old_process() {
    if !in_a_child_with_canary_keys(
        "fr_h6_after_an_external_changeover_no_sender_reuses_a_connection_to_the_old_process",
    ) {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let a = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = a.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let endpoint = rig.endpoint();
    let companion = resident_chat_card(&endpoint);
    let inspector = Scripted::new();
    inspector.external_listener(port, a.pid(), Some(TEAM));
    let full =
        Full::start_with_cards(rig, inspector.clone(), fast(), vec![companion.clone()]).await;
    full.wait_open().await;
    // 后台探测用它自己的客户端，前后用同一个（真的探测器也是）。
    let prober = ProberContext {
        health: full.health.clone(),
        plans: plan_upstreams(std::slice::from_ref(&companion)).unwrap(),
        runtimes: RuntimeRegistry::default(),
        client: reqwest::Client::new(),
        remote_kill_switch: Arc::new(AtomicBool::new(false)),
        timings: ProberTimings::default(),
    };
    let daily = || {
        full.data(
            "POST",
            "/v1/chat/completions",
            Some(json!({"model": "idoris/daily", "messages": [{"role": "user", "content": "hi"}]})),
        )
    };

    // 每个发送的地方都用一遍：适配器（加载、聊天）、转发的代理、模型列表、探测。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(a.fake.model_loads(), 1);
    assert_eq!(daily().await.0, StatusCode::OK);
    assert_eq!(full.data("GET", "/v1/models", None).await.0, StatusCode::OK);
    assert_eq!(
        prober.observe(&prober.plans[0]).await,
        Observation::Ready,
        "the probe read A"
    );

    // A 放掉端口，进程活着、它已经接受的连接还开着；管理器看到没有监听者。
    a.fake.release_port();
    wait_until_nothing_listens(port).await;
    inspector.forget_listener(port);
    full.until_unavailable().await;
    let a_saw = a.fake.requests().len();

    // B 在同一个端口上起来，管理器接管它（新一代，账本对着 B 重建）。
    let b = External::start_on(work.path(), "external-b", &stateful(), port);
    inspector.external_listener(port, b.pid(), Some(TEAM));
    wait_for("ready again", || {
        full.health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;

    // 适配器：加载与聊天落在 B。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(b.fake.model_loads(), 1, "the chat loaded the model on B");
    // 转发的代理、模型列表、探测：落在 B。
    let b_chats = b.fake.requests_of("POST:/v1/chat/completions");
    assert_eq!(daily().await.0, StatusCode::OK);
    assert_eq!(
        b.fake.requests_of("POST:/v1/chat/completions"),
        b_chats + 1,
        "the forwarded chat reached B"
    );
    assert_eq!(full.data("GET", "/v1/models", None).await.0, StatusCode::OK);
    assert_eq!(
        prober.observe(&prober.plans[0]).await,
        Observation::Ready,
        "the probe read B"
    );

    assert_eq!(
        a.fake.requests().len(),
        a_saw,
        "the old process was sent something over a connection that outlived it: {:?}",
        a.fake.requests()
    );
    assert!(
        pid_alive(u64::from(a.pid())),
        "A is still alive, holding its connections"
    );
}

// ----- H7：退路缓存：一个突发的并发请求只起一个 `lsof` ---------------------------------------------------------------------------

/// 没有原生监视（退路）：窗口关着的时候同时到达的一批请求，只有一个贵的检查（`lsof`）在途，别的请求排队等它的结论；之前每个请求各起一个 `lsof`，
/// 一批请求一起把 `lsof` 拉起来几十次。
#[tokio::test]
async fn fr_h7_a_burst_of_concurrent_requests_costs_a_handful_of_lsof_in_the_fallback() {
    if !has_lsof() {
        return;
    }
    let rig = Rig::new(&stateful());
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("calls.log");
    let ps = counting_tool(tools.path(), "ps", &["/bin/ps", "/usr/bin/ps"], &log);
    let lsof = counting_tool(
        tools.path(),
        "lsof",
        &["/usr/sbin/lsof", "/usr/bin/lsof"],
        &log,
    );
    let inspector = Arc::new(
        SystemInspector::with_tools(ps, lsof)
            .with_timeouts(PATIENT, PATIENT)
            .with_native_probe(false),
    );
    let full = Arc::new(Full::start_with(rig, inspector, supervision_paused()).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let settled = settled_tool_calls(&log).await;
    // 上一次「是」的窗口过期之后，突发的请求找到的是关着的窗口。
    tokio::time::sleep(FALLBACK_TTL + Duration::from_millis(150)).await;

    let tasks: Vec<_> = (0..24)
        .map(|_| {
            let full = full.clone();
            tokio::spawn(async move { full.chat().await.0 })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
    let started = tool_calls(&log) - settled;
    assert!(started >= 1, "the fallback asked lsof at all");
    assert!(
        started <= 12,
        "24 concurrent requests started {started} lsof (one check at a time, the rest wait for its answer; every request on its own would be 24 or more)"
    );
}

/// 退路的缓存与原生监视并存：原生读不出来的时候，贵的问题（`listens_on`）的「是」重用一个窗口；原生一旦说「不握了」，窗口里的「是」作废，之后原生再读不出
/// 来，就得重新问，不能拿那个旧的「是」放行（旧的「是」比这次「不握了」早）。窗口设成一分钟，所以任何放行都只可能来自缓存。
#[tokio::test]
async fn fr_h7_a_native_no_ends_the_window_of_the_fallback_so_a_stale_yes_is_never_served() {
    if !in_a_child_with_canary_keys(
        "fr_h7_a_native_no_ends_the_window_of_the_fallback_so_a_stale_yes_is_never_served",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let inspector = Scripted::claiming(port);
    inspector.reuse_fallback_for(Duration::from_secs(60));
    inspector.script_native(vec![Err(InspectError::NoAnswer)]);
    let full = Full::start_with(rig, inspector.clone(), supervision_paused()).await;
    full.wait_open().await;

    // 原生读不出来：贵的问题答「是」，开一个一分钟的窗口；窗口里的请求不再问。
    assert_eq!(full.chat().await.0, StatusCode::OK);
    let asked = inspector.listens_asked();
    assert!(asked >= 1);
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert_eq!(
        inspector.listens_asked(),
        asked,
        "the second request was served from the window"
    );

    // 原生说「不握了」：下一个请求被拒，即使窗口还开着。
    inspector.script_native(vec![Ok(false)]);
    let (status, _) = full.chat().await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a native no wins over the open window"
    );

    // 原生又读不出来了：窗口已经作废，要重新问（不能拿旧的「是」放行）。
    inspector.script_native(vec![Err(InspectError::NoAnswer)]);
    assert_eq!(full.chat().await.0, StatusCode::OK);
    assert!(
        inspector.listens_asked() > asked,
        "the stale yes was served after a native no instead of asking again"
    );
}

// ----- M10、M11：原生监视读不了这个进程：问那个贵的问题，不能就此拒绝 --------------------------------------------------------------

/// 原生监视对这个引擎永远「读不出来」（权限、短读、没查成的候选）：守卫每个请求退到 `listens_on`，引擎照常被服务（不被一口回绝）；引擎放掉端口之后，
/// 退路仍然抓得到它（冒名者一个请求也收不到）。守卫建监视时问的是 `127.0.0.1:<端口>` 这个地址，对着引擎的 pid。
#[tokio::test]
async fn fr_m10_m11_a_watch_that_cannot_read_the_process_falls_back_and_never_refuses_outright() {
    if !in_a_child_with_canary_keys(
        "fr_m10_m11_a_watch_that_cannot_read_the_process_falls_back_and_never_refuses_outright",
    ) {
        return;
    }
    let rig = Rig::new(&stateful());
    let port = rig.port;
    let inspector = Scripted::claiming(port);
    inspector.offer_a_native_watch_that_cannot_read();
    let full = Full::start_with(rig, inspector.clone(), supervision_paused()).await;
    full.wait_open().await;
    let engine = u32::try_from(full.rig.pid(0)).unwrap();
    assert!(
        inspector
            .watched()
            .contains(&(engine, SocketAddr::new(MANAGED_DIAL, port))),
        "{:?}",
        inspector.watched()
    );
    let asked = inspector.listens_asked();
    for _ in 0..5 {
        assert_eq!(
            full.chat().await.0,
            StatusCode::OK,
            "an unreadable watch is not a refusal"
        );
    }
    assert!(
        inspector.listens_asked() >= asked + 5,
        "every request fell back to the expensive question"
    );

    // 引擎放掉端口：退路抓得到。
    full.rig.fake.release_port();
    inspector.lose(port);
    let impostor = Impostor::on_port(port).await;
    let (status, _) = full.chat().await;
    assert_ne!(status, StatusCode::OK);
    assert_eq!(impostor.count().await, 0);
    impostor.assert_never_authorized().await;
}

// ----- M9：进程自己重建了监听套接字（新的描述符）：重新定位，仍被服务 --------------------------------------------------------------

/// 引擎在同一个进程、同一个端口上关掉监听套接字再重新监听（新的描述符）。原生监视记着的描述符已经关了：它在阻塞线程池里重新定位新的套接字，之后的每个
/// 请求照常被服务（不被误判成「不握了」，也不拖住异步执行器）。
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn fr_m9_an_engine_that_listens_again_on_a_new_descriptor_is_still_served() {
    let rig = Rig::new(&stateful());
    let inspector = Arc::new(SystemInspector::new().with_native_probe(true));
    let full = Full::start_with(rig, inspector, supervision_paused()).await;
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    assert_eq!(full.rig.fake.release_port_as(Release::Relisten), "ok");
    for _ in 0..5 {
        assert_eq!(
            full.chat().await.0,
            StatusCode::OK,
            "the engine listens on a new descriptor: the watch relocates it"
        );
    }
}

// ----- M13：外部引擎：只数能接这个连接的监听者 ------------------------------------------------------------------------------------

/// 外部引擎的端口上还有第二个监听者，但它绑在 `[::1]` 上（不合格：没有签名团队），连到 `127.0.0.1` 的连接到不了它：它不影响校验，引擎被接管并照常
/// 服务；守卫的期望集合里只有那个能接连接的引擎。之前端口上的每个 pid 都得合格，这个旁观者让引擎永远接管不了。
#[tokio::test]
async fn fr_m13_a_listener_that_cannot_take_the_connection_does_not_keep_an_external_engine_from_being_adopted()
 {
    if !in_a_child_with_canary_keys(
        "fr_m13_a_listener_that_cannot_take_the_connection_does_not_keep_an_external_engine_from_being_adopted",
    ) {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let external = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.sign(external.pid(), TEAM);
    let bystander = std::os::unix::process::parent_id();
    inspector.sockets_on(
        port,
        vec![
            ListeningSocket::v4(external.pid(), std::net::Ipv4Addr::LOCALHOST),
            ListeningSocket {
                pid: bystander,
                bind: Bind::v6(std::net::Ipv6Addr::LOCALHOST, false),
            },
        ],
    );
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::External {
            settings_key_verified: true
        }),
        "the engine was adopted"
    );
    assert_eq!(full.chat().await.0, StatusCode::OK);
    // 守卫只为那个能接连接的引擎建监视（旁观者不在期望集合里）。
    assert!(
        inspector
            .watched()
            .iter()
            .all(|(pid, dial)| *pid == external.pid()
                && *dial == SocketAddr::new(MANAGED_DIAL, port)),
        "{:?}",
        inspector.watched()
    );
}

/// 反过来：第二个监听者绑在 IPv4 通配上（能接 `127.0.0.1` 的连接）而且不合格：任何能接这个连接的监听者都得是被授权的，所以不接管，凭证一个也不发；
/// 引擎收到的只有匿名的探测。
#[tokio::test]
async fn fr_m13_a_second_listener_that_can_take_the_connection_and_is_not_authorized_prevents_the_adoption()
 {
    if !in_a_child_with_canary_keys(
        "fr_m13_a_second_listener_that_can_take_the_connection_and_is_not_authorized_prevents_the_adoption",
    ) {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let external = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.sign(external.pid(), TEAM);
    let stranger = std::os::unix::process::parent_id();
    inspector.sockets_on(
        port,
        vec![
            ListeningSocket::v4(external.pid(), std::net::Ipv4Addr::LOCALHOST),
            ListeningSocket::any(stranger),
        ],
    );
    let full = Full::start(rig, inspector.clone()).await;
    full.wait_open().await;
    assert_eq!(
        governed_endpoint(&full.rig.endpoint()),
        Some(EndpointKey::Blocked),
        "an unauthorized listener that can take the connection: nothing is adopted"
    );
    assert_eq!(full.chat().await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        external
            .fake
            .requests()
            .iter()
            .all(|(_, auth)| auth == "none"),
        "no credential reached the engine: {:?}",
        external.fake.requests()
    );
}

// ---------------------------------------------------------------------------------------------
// Codex 预审（第 4 层）：M1 回答带受理那一刻的快照、M2 归属与并入一起决定、M4 停止什么时候算做完、M5 排空之后封上、L4 重传
// ---------------------------------------------------------------------------------------------

/// M1：管理器的回答是实例任务**自己**在受理那一刻的快照，一个值：之后新引擎就绪、又有别的命令被受理，它都不变。之前调用方在回答之后再读共享状态，
/// 受理之后实例任务还在往前走（也还会收下排在后面的命令），`restart` 的 202 可能说 `stopped`，或者快到新引擎已经就绪、`restarting` 被盖掉了。
#[tokio::test]
async fn m1_the_answer_to_a_command_is_the_snapshot_of_its_acceptance_not_of_whatever_came_after() {
    let world = World::start(
        Engine::Fake(Behavior {
            trap_sigterm_secs: Some(0.0),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let first = world.rig.pid(0) as u32;
    let CommandAck::Snapshot(restarted) = world.manager.restart_upstream("omlx").await.unwrap()
    else {
        panic!("the instance answered in time: a snapshot");
    };
    // 受理那一刻：旧引擎还握在手里。
    assert_eq!(restarted.state, UpstreamState::Restarting);
    assert!(restarted.owned);
    assert_eq!(restarted.pid, Some(first));
    assert_eq!(restarted.reason, None);
    assert_eq!(
        restarted.failure_count, 0,
        "an operator restart is no failure"
    );
    assert!(!restarted.restart_required);

    // 新引擎就绪之后，又一个命令被受理：它的回答是它自己的。
    let ready = world
        .until("the replacement", |element| {
            element["state"] == "ready" && element["pid"] != json!(first)
        })
        .await;
    let second = ready["pid"].as_u64().unwrap() as u32;
    let CommandAck::Snapshot(stopping) = world.manager.stop_upstream("omlx").await.unwrap() else {
        panic!("a snapshot");
    };
    assert_eq!(stopping.state, UpstreamState::Stopped);
    assert!(
        stopping.owned,
        "the engine is still held at the moment of acceptance"
    );
    assert_eq!(stopping.pid, Some(second));
    // 第一个回答没有变成 `stopped`、也没有变成 `ready`：它是一个值。
    assert_eq!(restarted.state, UpstreamState::Restarting);
    assert_eq!(restarted.pid, Some(first));

    // 幂等的回答带当前状态：已经停下的引擎再 `stop`。
    world.until_gone().await;
    let CommandAck::Snapshot(again) = world.manager.stop_upstream("omlx").await.unwrap() else {
        panic!("a snapshot");
    };
    assert_eq!(again.state, UpstreamState::Stopped);
    assert!(!again.owned);
    assert_eq!(again.pid, None);
    world.manager.shutdown(true).await;
}

/// M2：外部引擎换了进程，管理器自动重新同步（对外是 `restarting`，`owned: false`）；排空期间监听者又变了，重新同步带着 `restarting` 返回，下一个监管节拍
/// 收到排在邮箱里的 `restart`。它过得了 Admin 的预检（`restarting` 也可能是一次并得进去的操作员重启），但这里没有任何一次可并入的、iDoris 拥有的重启：
/// 要回 409 `upstream_not_owned`（规范 §2.11.4：外部引擎一律如此），而不是一个没有任何东西跟在后面的 202。之前实例任务看见发布的状态是 `restarting` 就回受理。
#[tokio::test]
async fn m2_a_restart_that_arrives_while_an_external_engine_is_resynchronising_is_not_owned_and_never_a_false_202()
 {
    let work = tempfile::tempdir().unwrap();
    let a = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = a.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, a.pid(), Some(TEAM));
    let full = Arc::new(Full::start(rig, inspector.clone()).await);
    full.wait_open().await;
    assert_eq!(full.chat().await.0, StatusCode::OK);

    // 一个聊天停在 A 里：自动重新同步的排空要等它。
    a.fake.hold(Hold::Chat);
    let chats = a.fake.requests_of("POST:/v1/chat/completions");
    let in_flight = {
        let full = full.clone();
        tokio::spawn(async move { full.chat().await })
    };
    wait_for("the chat to reach A", || {
        a.fake.requests_of("POST:/v1/chat/completions") == chats + 1
    })
    .await;
    // A 换成了 B（通过校验）：不是操作员的重启，状态是 `restarting`、`owned: false`。
    let b_pid = std::process::id();
    let c_pid = std::os::unix::process::parent_id();
    assert_ne!(b_pid, c_pid);
    inspector.external_listener(port, b_pid, Some(TEAM));
    wait_for(
        "the resynchronisation to be published as restarting",
        || full.health.view().upstreams[0].state == UpstreamState::Restarting,
    )
    .await;
    assert_eq!(full.element("omlx").await["owned"], false);

    // 排空期间一个 `restart` 过了预检、排进实例任务的邮箱（它正忙在排空里，不去收）。
    let restart = {
        let full = full.clone();
        tokio::spawn(async move { full.post_command("restart").await })
    };
    // 等到它真的入了邮箱（盖了章、排在实例任务的邮箱里），不靠睡眠。
    wait_for("the restart to be in the mailbox", || {
        full.manager.commands_enqueued("omlx") >= 1
    })
    .await;
    // 排空期间监听者又变了（第三个进程）：重新同步失败、带着 `restarting` 返回，实例任务回到监管，第一件事收下排着的 `restart`。
    inspector.external_listener(port, c_pid, Some(TEAM));
    a.fake.release(Hold::Chat);
    assert_eq!(in_flight.await.unwrap().0, StatusCode::OK);

    let (status, body) = restart.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"));
    // 什么都没发生：没有拉起任何引擎，没有给 A 发任何信号，这不是一次失败；自动的重新同步照常做完。
    assert_eq!(full.rig.fake.launches(), 0);
    assert!(
        pid_alive(u64::from(a.pid())),
        "an external engine is never signalled"
    );
    full.until("the third process to be let through", |element| {
        element["state"] == "ready"
    })
    .await;
    let element = full.element("omlx").await;
    assert_eq!(element["failure_count"], 0, "{element}");
    assert_eq!(element["owned"], false, "{element}");
    assert_eq!(full.chat().await.0, StatusCode::OK);
    full.manager.shutdown(true).await;
}

/// 在一个裸的管理器（不经 `World`）上建 Admin Router：`GET`、`POST` 经真的 HTTP 层。
fn admin_over(
    manager: &Arc<EngineManager>,
    health: &HealthState,
    endpoint: &str,
) -> (Router, String) {
    let token = AdminSessionToken::mint();
    let secret = token.expose_secret().to_owned();
    let admin = build_admin_app_with_engines(
        AppState {
            cards: vec![managed_card(endpoint)],
            health_state: health.clone(),
            ..AppState::default()
        },
        SharedAdminToken::new(token),
        Some(manager.clone()),
    );
    (admin, secret)
}

async fn admin_call(admin: &Router, secret: &str, method: &str, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .unwrap();
    let response = admin.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 轮询 Admin 的 `GET`（这个管理器只有 `omlx` 一个上游）直到 `done`，返回那一次的元素。
async fn admin_until(
    admin: &Router,
    secret: &str,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + PATIENT;
    loop {
        let (status, body) = admin_call(admin, secret, "GET", "/admin/api/v1/upstreams").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let current = body["upstreams"][0].clone();
        if done(&current) {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {current}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 文档里的完成判据：`stop` 做完了 = `state` 是 `stopped` **并且** `owned` 是 false。
fn stop_is_done(element: &Value) -> bool {
    element["state"] == "stopped" && element["owned"] == false
}

/// M4：身份读不出来的引擎，停止不发信号、没能确认它消失：句柄交出去收尾，`owned` 是 false、没有 `pid`，状态是 `unavailable` + `port_in_use`——进程还活着。
/// 只看 `owned: false` 的调用方会把它当成停好了；文档的判据（`stopped` 并且 `owned: false`）不会。`stop` 此时没有句柄可停（409 `not_owned`）；`start` 重新
/// 走启动决策、先重读所有权记录，身份此刻读得出就认回同一个进程，之后真的 `stop` 才满足判据。全部经 Admin 的 `GET` 与 `POST` 断言。
#[tokio::test]
async fn m4_an_unconfirmed_stop_reads_unavailable_and_port_in_use_with_owned_false_and_is_not_a_finished_stop()
 {
    let rig = Rig::new(&Behavior::default());
    let scripted = Scripted::claiming(rig.port);
    // 先留一个孤儿引擎（`stop_on_exit:"never"` 的正常退出）：之后的管理器认回它，停它要先读它的身份。
    let (first, first_health) = start_manager(&rig, scripted.clone(), StopOnExit::Never);
    wait_for("ready", || {
        first_health.view().upstreams[0].state == UpstreamState::Ready
    })
    .await;
    first.shutdown(false).await;
    let pid = rig.pid(0);
    assert!(pid_alive(pid), "never: the engine is left running");

    let inspector = Arc::new(Unreadable {
        inner: scripted,
        unreadable: AtomicBool::new(false),
    });
    let (manager, health) = start_manager(&rig, inspector.clone(), StopOnExit::Owned);
    wait_for("adopted", || {
        let upstream = &health.view().upstreams[0];
        upstream.state == UpstreamState::Ready && upstream.owned
    })
    .await;
    let (admin, secret) = admin_over(&manager, &health, &rig.endpoint());

    // 身份读不出来了：`stop` 受理（202，`stopped`、还握着）。
    inspector.unreadable.store(true, Ordering::SeqCst);
    let (status, body) =
        admin_call(&admin, &secret, "POST", "/admin/api/v1/upstreams/omlx/stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "stopped", "{body}");
    assert_eq!(body["owned"], true, "{body}");
    // 收尾：没能确认进程消失。`owned` 是 false，没有 `pid`——但这不是停好了。
    let unconfirmed = admin_until(
        &admin,
        &secret,
        "the unconfirmed stop to be reported",
        |element| element["state"] == "unavailable",
    )
    .await;
    assert_eq!(unconfirmed["reason"], "port_in_use", "{unconfirmed}");
    assert_eq!(unconfirmed["owned"], false, "{unconfirmed}");
    assert!(unconfirmed.get("pid").is_none(), "{unconfirmed}");
    assert_eq!(unconfirmed["restart_required"], false, "{unconfirmed}");
    assert!(
        pid_alive(pid),
        "an engine whose identity cannot be read is never signalled"
    );
    assert!(
        !stop_is_done(&unconfirmed),
        "the documented completion rule must not call an unconfirmed stop done: {unconfirmed}"
    );
    assert_eq!(
        unconfirmed["owned"], false,
        "(the trap: owned alone says false here too, while the process is alive)"
    );
    // 没有句柄可停：再 `stop` 是 409 `not_owned`；`start` 是出路。
    let (status, body) =
        admin_call(&admin, &secret, "POST", "/admin/api/v1/upstreams/omlx/stop").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"));

    // 身份读得出了：`start` 先重读所有权记录。读到的是之前没能确认停掉的那个进程（它被判了「停」，只是没能确认），所以**不认回、不放行**：先把它停掉、确认消失，
    // 再拉起一个新的（第 3 层的 `condemned`）；一个操作员要它停的进程不能因为 `start` 就又成了服务中的引擎。
    inspector.unreadable.store(false, Ordering::SeqCst);
    let (status, body) = admin_call(
        &admin,
        &secret,
        "POST",
        "/admin/api/v1/upstreams/omlx/start",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let replacement = admin_until(&admin, &secret, "a fresh engine to be serving", |element| {
        element["state"] == "ready" && element["owned"] == true
    })
    .await;
    assert_ne!(
        replacement["pid"],
        json!(pid),
        "the process whose stop was never confirmed is not put back into service: {replacement}"
    );
    wait_for("the condemned process to be gone", || !pid_alive(pid)).await;
    assert_eq!(rig.fake.launches(), 2, "a fresh engine was launched");

    // 这一次停得成：`stopped` 并且 `owned: false`——判据满足，进程确实没了。
    let (status, body) =
        admin_call(&admin, &secret, "POST", "/admin/api/v1/upstreams/omlx/stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let done = admin_until(&admin, &secret, "the stop to be done", stop_is_done).await;
    assert!(done.get("pid").is_none(), "{done}");
    wait_for("the replacement to be gone", || {
        !pid_alive(replacement["pid"].as_u64().unwrap())
    })
    .await;
    manager.shutdown(true).await;
}

/// L4：`restart` 的幂等只在第一次操作进行期间成立。第一次做完（新引擎就绪）之后到达的重传（丢了第一个 202 的调用方重发）是一次**新的**重启，再换一次引擎。
/// 文档这样写（README、规范 §2.11.4）：要确认第一次是否成功，读 `GET`，不要重发。并发的重复请求仍并入进行中的那一次
/// （`two_concurrent_restarts_replace_the_engine_once_and_both_are_accepted`）。
#[tokio::test]
async fn l4_a_restart_that_arrives_after_the_first_one_finished_is_a_new_restart() {
    let world = World::start(
        Engine::Fake(Behavior {
            trap_sigterm_secs: Some(0.0),
            ..Behavior::default()
        }),
        fast(),
    )
    .await;
    let first = world.rig.pid(0);
    let (status, body) = world.post("omlx", "restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let second = world
        .until("the replacement", |element| {
            element["state"] == "ready" && element["pid"] != json!(first)
        })
        .await["pid"]
        .clone();
    assert_eq!(world.rig.fake.launches(), 2);
    // 第一个 202 丢了，调用方又发一次：第一次早就做完了，这是新的重启。
    let (status, body) = world.post("omlx", "restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "restarting", "{body}");
    world
        .until("the second replacement", |element| {
            element["state"] == "ready" && element["pid"] != second
        })
        .await;
    assert_eq!(
        world.rig.fake.launches(),
        3,
        "a retransmission after completion is a new restart"
    );
    world.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 复审（第 4 层的修复，044da0c8..da0d38fa）：M1'、L2'、L4'
// ---------------------------------------------------------------------------------------------

/// M1'：操作员的 `start` 在接管一个外部引擎，不是在拉起或替换 iDoris 自己的引擎：它做完是 `ready`、`owned: false`，没有任何自有引擎会被重启。这时到达的 `restart`
/// 不能回 202（一个没有任何东西跟在后面的受理），要回 409 `upstream_not_owned`（规范 §2.11.4：外部引擎一律如此）。这一条：`start` 正在等外部引擎答话
/// （它的 `GET /v1/models` 被扣着）的时候到达的 `restart`。
#[tokio::test]
async fn m1b_a_restart_during_a_start_that_is_waiting_on_an_external_engine_is_not_owned_and_never_a_false_202()
 {
    let work = tempfile::tempdir().unwrap();
    let external = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, external.pid(), Some(TEAM));
    let full = Full::start(rig, inspector).await;
    full.wait_open().await;
    let element = full.element("omlx").await;
    assert_eq!(element["state"], "ready", "{element}");
    assert_eq!(element["owned"], false, "{element}");

    // 外部引擎不再答话：监督说 `unavailable`；操作员的 `start` 重新走启动决策、等它答话。
    external.fake.hold(Hold::Models);
    full.until("the external engine to be unavailable", |element| {
        element["state"] == "unavailable"
    })
    .await;
    let (status, body) = full.post_command("start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "starting", "{body}");

    // `start` 在等外部引擎答话；这时没有任何自有引擎可重启。
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"));

    // 外部引擎答话了：`start` 接管它，做完是 `ready`、`owned: false`；没有拉起任何自有引擎，没有给外部引擎发任何信号。
    external.fake.release(Hold::Models);
    let element = full
        .until("the start to adopt the external engine", |element| {
            element["state"] == "ready"
        })
        .await;
    assert_eq!(element["owned"], false, "{element}");
    assert_eq!(full.rig.fake.launches(), 0);
    assert!(
        pid_alive(u64::from(external.pid())),
        "an external engine is never signalled"
    );
    full.manager.shutdown(true).await;
}

/// M1'：同上，但 `restart` 是在 `start` 的实例任务忙着、不去收命令的时候入队的：接管外部引擎的属主校验停在那里（`signing_team` 被扣着）。`start` 做完（`ready`、
/// `owned: false`）之后实例任务才收下它：接管外部引擎的操作不记 `restart` 的水位，所以它不并入，按各自到达时的状态回答（没有自有引擎：409）。
/// 之前水位记在每一次 `start`、`restart` 做完时，这个 `restart` 被当成并入了「刚做完的操作」，回了 202，没有任何引擎被重启。
#[tokio::test]
async fn m1b_a_restart_queued_behind_a_start_that_adopts_an_external_engine_is_not_owned_and_never_a_false_202()
 {
    let work = tempfile::tempdir().unwrap();
    let external = External::start_on(work.path(), "external-a", &stateful(), free_port());
    let port = external.port;
    let mut rig = Rig::new(&Behavior::default());
    rig.port = port;
    let inspector = Scripted::new();
    inspector.external_listener(port, external.pid(), Some(TEAM));
    let full = Arc::new(Full::start(rig, inspector.clone()).await);
    full.wait_open().await;

    external.fake.hold(Hold::Models);
    full.until("the external engine to be unavailable", |element| {
        element["state"] == "unavailable"
    })
    .await;
    let (status, body) = full.post_command("start").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    // 外部引擎一答话，`start` 就去接管它，属主校验停在 `signing_team`：实例任务忙着，不收命令。
    let gate = inspector.hold_team_checks();
    external.fake.release(Hold::Models);
    wait_for("the ownership check of the adoption", || gate.entered()).await;
    let restart = {
        let full = full.clone();
        tokio::spawn(async move { full.post_command("restart").await })
    };
    // `start` 是第 1 条命令，`restart` 是第 2 条：等到它真的入了邮箱，不靠睡眠。
    wait_for("the restart to be in the mailbox", || {
        full.manager.commands_enqueued("omlx") >= 2
    })
    .await;
    gate.release();

    let (status, body) = restart.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"));
    let element = full
        .until("the start to have adopted the external engine", |element| {
            element["state"] == "ready"
        })
        .await;
    assert_eq!(element["owned"], false, "{element}");
    assert_eq!(full.rig.fake.launches(), 0);
    assert!(
        pid_alive(u64::from(external.pid())),
        "an external engine is never signalled"
    );
    full.manager.shutdown(true).await;
}

/// L2'（`stop`）：认领在第一个副作用（关 Supervisor 的门）**之前**，真的经 HTTP 钉住。Supervisor 起来之后先做启动采样（`GET /api/status`，这里被引擎扣着），
/// 采样完成之前它不收任何消息，`close_admission` 的回答就一直不来：实例任务已经认领了 `stop`（受理了、会执行），第一步却等得比 `command_timeout`（这里 400 毫秒）
/// 还久。Admin 回 **202**，不是 503「没有受理」，体是回答那一刻读到的元素（还是命令之前的状态）；采样放开之后 `stop` 照常做完。如果认领被挪到 `close_admission`
/// 之后，到点时调用方先放弃、拿到 503，实例任务随后才发现命令已经作废：一个没有受理、门却已经关了的 `stop`。
#[tokio::test]
async fn l2b_a_stop_whose_first_step_outlasts_the_deadline_is_accepted_late_over_tcp_and_still_runs()
 {
    let listener = AdminBindConfig::parse(Some("0"))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let rig = Rig::new(&stateful());
    let port = rig.port;
    // 引擎一起来，Supervisor 的启动采样就被扣着（门控放行之后 Supervisor 才起）。
    rig.fake.hold(Hold::Status);
    let timings = parked_actor_timings();
    let full = Full::start_with(rig, Scripted::claiming(port), timings).await;
    full.wait_open().await;
    let old = full.rig.pid(0);
    assert_eq!(full.element("omlx").await["state"], "ready");

    let app = full.admin.router();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        }
    });
    let _shutdown_on_drop = shutdown.clone().drop_guard();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(PATIENT)
        .build()
        .unwrap();
    let started = Instant::now();
    let response = client
        .post(format!("http://{addr}/admin/api/v1/upstreams/omlx/stop"))
        .bearer_auth(&full.token)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(
        started.elapsed() >= PARKED_COMMAND_TIMEOUT - Duration::from_millis(50),
        "the answer came after the deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(
        (body["state"].as_str(), body["owned"].as_bool()),
        (Some("ready"), Some(true)),
        "the late answer reads the element as it is then, not as of the acceptance: {body}"
    );

    // 采样放开：Supervisor 开始收消息，`close_admission` 有了回答，`stop` 照常做完。
    full.rig.fake.release(Hold::Status);
    full.until("the stop to be done", stop_is_done).await;
    wait_for("the engine process to be gone", || !pid_alive(old)).await;
    assert_eq!(
        full.logged("收到命令 stop，已受理（实例任务在期限内没有回答"),
        1,
        "the late acceptance is audited as such"
    );
    full.manager.shutdown(true).await;
    shutdown.cancel();
    server.await.unwrap();
}

/// L2'（`restart`）：同上，`restart` 的第一步也是关门。迟到的 202 之后，放开采样，旧引擎退役、新引擎拉起、回到 `ready`（pid 换了）。
#[tokio::test]
async fn l2b_a_restart_whose_first_step_outlasts_the_deadline_is_accepted_late_and_still_runs() {
    let rig = Rig::new(&stateful());
    let port = rig.port;
    rig.fake.hold(Hold::Status);
    let timings = parked_actor_timings();
    let full = Full::start_with(rig, Scripted::claiming(port), timings).await;
    full.wait_open().await;
    let old = full.rig.pid(0);

    let started = Instant::now();
    let (status, body) = full.post_command("restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(
        started.elapsed() >= PARKED_COMMAND_TIMEOUT - Duration::from_millis(50),
        "the answer came after the deadline: {:?}",
        started.elapsed()
    );

    full.rig.fake.release(Hold::Status);
    full.until("the replacement", |element| {
        element["state"] == "ready" && element["pid"] != json!(old)
    })
    .await;
    assert_eq!(full.rig.fake.launches(), 2);
    assert_eq!(
        full.logged("收到命令 restart，已受理（实例任务在期限内没有回答"),
        1,
        "the late acceptance is audited as such"
    );
    full.manager.shutdown(true).await;
}

/// 日志出口上多挂的钩子：每一行日志写进去之后调用一次。
type LineHook = Arc<dyn Fn(&str) + Send + Sync>;

/// 让实例任务在写某一行日志的时候停住：发布状态之后、`ready` 的那一行日志是同步写的（日志的出口是测试的），这个窗口里状态已经对外可见，实例任务还没有回到命令循环。
struct LogGate {
    needle: &'static str,
    armed: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
}

impl LogGate {
    fn new(needle: &'static str) -> Arc<Self> {
        Arc::new(Self {
            needle,
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
        })
    }

    /// 从现在起，第一条含 `needle` 的日志停在这里，直到 [`Self::release`]。
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    fn sink(self: &Arc<Self>) -> LineHook {
        let gate = self.clone();
        Arc::new(move |line| {
            if line.contains(gate.needle) && gate.armed.swap(false, Ordering::SeqCst) {
                gate.entered.store(true, Ordering::SeqCst);
                // 扣住的是实例任务所在的运行时线程：`block_in_place` 让运行时把别的任务（含测试自己的定时器）交给另一个线程，不然这个线程一扣，
                // 测试这一头也跟着停了。有界：测试出了岔子也不会把它永远扣在这里。
                tokio::task::block_in_place(|| {
                    let give_up = Instant::now() + Duration::from_secs(30);
                    while !gate.released.load(Ordering::SeqCst) && Instant::now() < give_up {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                });
            }
        })
    }
}

/// L4'：客户端一看到 `ready` 就发下一个 `restart`，那是一次**新的**重启。实例任务发布 `ready` 之后还要写一行日志，才去记 `restart` 的水位：这个窗口里一个已经看到
/// `ready` 的客户端发来的 `restart` 盖的章比水位小，被当成并入了「刚做完」的那一次，回了 202，没有换引擎。水位在发布 `ready` **之前**记，窗口就没有了：日志的出口在这里
/// 被扣住，状态已经对外可见，第二个 `restart` 照样是新的重启（引擎换第三次）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l4b_a_restart_sent_the_moment_ready_is_visible_is_a_new_restart_even_while_the_log_line_is_still_being_written()
 {
    let gate = LogGate::new("引擎 omlx：ready");
    let world = World::start_gated(Engine::Fake(Behavior::default()), fast(), &gate).await;
    let first = world.rig.pid(0);
    gate.arm();
    let (status, body) = world.post("omlx", "restart").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // 新引擎就绪、`ready` 已经发布；实例任务停在紧跟着的那一行日志里。
    wait_for(
        "the instance task to be inside the log line of `ready`",
        || gate.entered(),
    )
    .await;
    let element = world.element("omlx").await;
    assert_eq!(element["state"], "ready", "{element}");
    let second = element["pid"].clone();
    assert_ne!(second, json!(first), "{element}");
    assert_eq!(world.rig.fake.launches(), 2);

    // 看到 `ready` 的客户端马上又发一个 `restart`：实例任务还停在日志里，它排在邮箱里。
    // 放开之前先等第二个 `restart` 真的入了邮箱（第 1 条是第一个 `restart`），不靠睡眠。
    let release = async {
        wait_for("the second restart to be in the mailbox", || {
            world.manager.commands_enqueued("omlx") >= 2
        })
        .await;
        gate.release();
    };
    let ((status, body), ()) = tokio::join!(world.post("omlx", "restart"), release);
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    world
        .until("the second replacement", |element| {
            element["state"] == "ready" && element["pid"] != second
        })
        .await;
    assert_eq!(
        world.rig.fake.launches(),
        3,
        "a restart sent after `ready` was visible is a new restart"
    );
    world.manager.shutdown(true).await;
}

/// M5（第 4 层的 HTTP 视角）：公共模型列表逐卡走，慢卡让它停在托管卡前面；`stop` 到来时托管端点上没有任何在途的操作，排空立刻成功、SIGTERM 发给引擎，
/// 引擎无视它、整个宽限期里照常监听。慢卡放开之后，列表走到托管的那张卡：在途登记已经封上，授权处拒绝，**引擎在 SIGTERM 之后没有收到这次列表的任何请求**。
/// 之前排空读到零就去停进程，两者之间没有封：这个请求登记新的在途、发往还在监听的引擎，随后被 SIGTERM 打断。监督的节拍拉到小时级，所以引擎收到的
/// `GET /v1/models` 只可能来自这次列表。
#[tokio::test]
async fn m5_a_model_list_that_reaches_the_managed_card_during_the_stop_grace_sends_nothing_to_the_stopping_engine()
 {
    let rig = Rig::new(&Behavior {
        on_sigterm: OnSigterm::Ignore,
        ..stateful()
    });
    let port = rig.port;
    let slow = SlowUpstream::start().await;
    let cards = vec![
        upstream_card(&slow.endpoint()),
        managed_card(&rig.endpoint()),
    ];
    let full = Arc::new(
        Full::start_with_card_order(rig, Scripted::claiming(port), supervision_paused(), cards)
            .await,
    );
    full.wait_open().await;
    let listing = {
        let full = full.clone();
        tokio::spawn(async move { full.data("GET", "/v1/models", None).await })
    };
    // 列表已经在第一张卡（慢卡）的请求里：托管的那张卡还没轮到，端点上没有在途的操作。
    slow.reached.notified().await;
    let old = full.rig.pid(0);
    let (status, body) = full.post_command("stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the engine to receive its SIGTERM", || {
        !full.rig.fake.sigterm_times().is_empty()
    })
    .await;
    assert!(
        pid_alive(old),
        "the engine ignores SIGTERM: it is still listening in its grace"
    );
    let asked = full.rig.fake.requests_of("/v1/models");

    // 慢卡放开：列表走到托管的那张卡。
    slow.release.notify_one();
    let (status, body) = listing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        full.rig.fake.requests_of("/v1/models"),
        asked,
        "the late model list reached the engine while it was being stopped"
    );
    let listed: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["slow-model"], "{body}");
    // 宽限到点、SIGKILL：停止照常做完。
    full.until_gone().await;
    wait_for("the process to be gone", || !pid_alive(old)).await;
}

/// 复审（第二轮）：一次因为找不到可执行文件、以 `absent` 收尾的 `start`，排在它后面的 `restart` 不能被说成并进了一个什么引擎都没换的操作。之前水位记在每一次不走外部路径的
/// 操作做完时，这个 `restart` 回了 202、没有任何引擎被换；现在水位只给拉起了自有引擎并且就绪了的操作记，这个 `restart` 按它到达时的状态（没有自有引擎）回 409 `upstream_not_owned`。
/// 引擎先起来（放行时就绪，不冻结）、被 `stop` 停下，可执行文件随后不见了；日志出口（`LogGate`）把实例任务扣在 `start` 发布 `starting` 的那行日志里（已经认领、还没回答），
/// `restart` 在这时入队（序号 3：`stop` 是 1，`start` 是 2）；放开之后 `start` 走完（`absent`），`restart` 才被收下。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m1c_a_restart_queued_behind_a_start_that_ends_absent_is_decided_against_the_current_state()
{
    let gate = LogGate::new("引擎 omlx：starting");
    let world = World::start_gated(Engine::Fake(Behavior::default()), fast(), &gate).await;
    assert_eq!(world.element("omlx").await["state"], "ready");
    let (status, body) = world.post("omlx", "stop").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    world
        .until("the engine to be stopped", |element| {
            element["state"] == "stopped" && element["owned"] == false
        })
        .await;
    // 可执行文件不见了：下一次 `start` 拉不起任何东西。
    std::fs::rename(
        &world.rig.fake.executable,
        world.rig.work.path().join("moved-away"),
    )
    .unwrap();
    assert_eq!(world.rig.fake.launches(), 1);

    gate.arm();
    let start = world.post("omlx", "start");
    let restart = async {
        wait_for(
            "the instance task to be inside the log line of `starting`",
            || gate.entered(),
        )
        .await;
        world.post("omlx", "restart").await
    };
    let release = async {
        wait_for("the restart to be in the mailbox", || {
            world.manager.commands_enqueued("omlx") >= 3
        })
        .await;
        gate.release();
    };
    let ((start_status, start_body), (status, body), ()) = tokio::join!(start, restart, release);
    assert_eq!(start_status, StatusCode::ACCEPTED, "{start_body}");
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_of(&body), ("upstream_not_owned", "not_owned"));
    // 什么都没发生：没有再拉起任何引擎，状态是 `absent`。
    let element = world
        .until("the start to have ended", |element| {
            element["state"] == "absent"
        })
        .await;
    assert_eq!(element["owned"], false, "{element}");
    assert_eq!(
        world.rig.fake.launches(),
        1,
        "no second engine was launched"
    );
    world.manager.shutdown(true).await;
}
