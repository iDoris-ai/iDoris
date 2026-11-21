//! IDORIS-CORE S5（接口规范 §2.7.6）：引擎管理器的状态机，进程内测试，时间参数缩短到毫秒级。
//!
//! 和 `lifecycle_managed.rs`（起真二进制）互补：那里验证接线，这里逐条验证状态机：退避的节奏、熔断与半开重试、命令、外部引擎只探测
//! 不发信号、端口属主校验、孤儿接管与围栏、门控的放行与退出码 5 的触发条件、`managed_models_dir()` 每次拉起都重新取。
//!
//! **全部用假引擎**（`tests/support/fake_omlx.rs` 的 python HTTP 服务），不启动真实的 oMLX 或任何推理进程。引擎的可执行文件
//! 都是显式给的（`ManagerConfig::executables`），所以这台机器上装没装真的 oMLX.app 都不会被拉起。检查器是可编排的
//! （[`Scripted`]）：身份信息（pid、启动时间）读真的 `ps`，「谁在监听」「谁签的名」按测试的需要编排，所以不依赖 `lsof` 与
//! `codesign`。唯一用到真 `lsof` 的测试（端口被别的进程占了）在没有它的机器上也成立（读不出来按「没有确认」处理）。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;
#[path = "support/fake_omlx.rs"]
mod fake_omlx;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fake_omlx::{Behavior, FakeOmlx, spawn_retrying};
use idoris_router::lifecycle::{
    CommandError, EngineBackend, EngineManager, GateOutcome, ManagedPlan, ManagerConfig,
    OmlxBackend, PlanError, PrepareFailure, Timings, plan_managed_with,
};
use idoris_router::readiness::{
    GateState, HealthState, ManagedFailure, Phase, UpstreamReason, UpstreamState, UpstreamStatus,
};
use idoris_router::register::{JitterSource, LogSink};
use idoris_upstream::engine::{
    CodesignPolicy, EngineInstance, EngineKey, EngineKind, EngineLaunch, EngineLaunchSpec,
    EngineRecipe, InspectError, ListenerPolicy, OwnedEngine, PidState, ProcessInspector,
    StopOnExit, SystemInspector,
};
use idoris_upstream::omlx::key::{EndpointKey, governed_endpoint};

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

fn ps_stat(pid: u64) -> String {
    let output = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// 活着：`kill -0` 成功，且不是僵尸（已退出、还没被回收的不算）。
fn pid_alive(pid: u64) -> bool {
    let signalled = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    let stat = ps_stat(pid);
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
// 可编排的检查器
// ---------------------------------------------------------------------------------------------

/// 身份信息（启动时间、命令行、进程组、uid）读真的 `ps`；「谁在监听」「谁签的名」按测试编排。
struct Scripted {
    real: SystemInspector,
    /// 对这些端口声称「被问到的 pid 正在监听」（假引擎真的在监听，但不依赖 `lsof` 来证明）。
    claims: Mutex<Vec<u16>>,
    /// `listeners_on(port)`：端口上的监听者。没编排的端口没有监听者。
    listeners: Mutex<BTreeMap<u16, Vec<u32>>>,
    /// `signing_team(pid)`：没编排的 pid 没有团队。
    teams: Mutex<BTreeMap<u32, String>>,
    team_checks: AtomicUsize,
    /// 端口 → 还剩几次「`listens_on` 为真」，用完之后一律为假：造「等就绪时监听它的是引擎，封印时端口已被别人占了」。
    claim_budget: Mutex<BTreeMap<u16, usize>>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            real: SystemInspector::new(),
            claims: Mutex::new(Vec::new()),
            listeners: Mutex::new(BTreeMap::new()),
            teams: Mutex::new(BTreeMap::new()),
            team_checks: AtomicUsize::new(0),
            claim_budget: Mutex::new(BTreeMap::new()),
        })
    }

    /// 自有引擎的端口，但只有第一次问到 `listens_on` 时为真（等就绪那一次），之后为假（封印那一次）。
    fn claiming_once(port: u16) -> Arc<Self> {
        let scripted = Self::new();
        scripted.claim_budget.lock().unwrap().insert(port, 1);
        scripted
    }

    /// 自有引擎的端口：`listens_on` 一律是真。
    fn claiming(port: u16) -> Arc<Self> {
        let scripted = Self::new();
        scripted.claims.lock().unwrap().push(port);
        scripted
    }

    /// 端口上不再有监听者（`lsof` 什么也看不到）。
    fn forget_listener(&self, port: u16) {
        self.listeners.lock().unwrap().remove(&port);
    }

    fn external_listener(&self, port: u16, pid: u32, team: Option<&str>) {
        self.listeners.lock().unwrap().insert(port, vec![pid]);
        if let Some(team) = team {
            self.teams.lock().unwrap().insert(pid, team.to_owned());
        }
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
        if let Some(left) = self.claim_budget.lock().unwrap().get_mut(&port) {
            if *left > 0 {
                *left -= 1;
                return Ok(true);
            }
            return Ok(false);
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
        self.team_checks.fetch_add(1, Ordering::SeqCst);
        Ok(self.teams.lock().unwrap().get(&pid).cloned())
    }
}

// ---------------------------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------------------------

/// 一个测试的状态目录、假引擎与端口。结束（含 panic）时清掉它拉起过的引擎进程。
struct Rig {
    state: tempfile::TempDir,
    work: tempfile::TempDir,
    fake: FakeOmlx,
    port: u16,
}

impl Rig {
    fn new(behavior: &Behavior) -> Self {
        let work = tempfile::tempdir().unwrap();
        let fake = FakeOmlx::create(work.path(), behavior);
        Self {
            state: tempfile::tempdir().unwrap(),
            work,
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

    fn fence_path(&self) -> PathBuf {
        idoris_upstream::omlx::load_fence_path_in(&self.state_dir(), &self.endpoint()).unwrap()
    }

    fn record_dir(&self) -> PathBuf {
        self.state_dir().join("engines-owned/omlx")
    }

    fn records(&self) -> usize {
        std::fs::read_dir(self.record_dir())
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    fn pid(&self, launch: usize) -> u64 {
        self.fake.launch(launch)["pid"].as_u64().unwrap()
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

/// 退避拉长到 600 毫秒（封顶 1.2 秒）：测试要观察 `restarting` 这个短暂的状态，慢机器上轮询任务可能一时得不到 CPU，
/// 窗口太短就漏掉了。
fn observable_restarts() -> Timings {
    let mut timings = fast();
    timings.restart_backoff_base = Duration::from_millis(600);
    timings.restart_backoff_cap = Duration::from_millis(1200);
    timings
}

/// `OmlxBackend`，加上可编排的外部引擎 key 与对 `govern_*` 的观察。
#[derive(Debug)]
struct TestBackend {
    inner: OmlxBackend,
    /// 外部引擎的 key（`None` = 没有）。
    external_key: Mutex<Option<String>>,
    /// `launch_recipe` 是否失败（造一个起不来的实例）。
    refuse_launch: Mutex<bool>,
    /// `launch_recipe` 是否 panic（造一个实例任务里的 bug）。
    panic_on_launch: Mutex<bool>,
}

impl TestBackend {
    fn new(inner: OmlxBackend) -> Arc<Self> {
        Arc::new(Self {
            inner,
            external_key: Mutex::new(None),
            refuse_launch: Mutex::new(false),
            panic_on_launch: Mutex::new(false),
        })
    }
}

impl EngineBackend for TestBackend {
    fn kind(&self) -> EngineKind {
        self.inner.kind()
    }
    fn probe_recipe(&self) -> Arc<dyn EngineRecipe> {
        self.inner.probe_recipe()
    }
    fn launch_recipe(&self, state_dir: &Path) -> Result<Arc<dyn EngineRecipe>, PrepareFailure> {
        if *self.panic_on_launch.lock().unwrap() {
            panic!("test: the instance task panics");
        }
        if *self.refuse_launch.lock().unwrap() {
            return Err(PrepareFailure {
                reason: Some(UpstreamReason::BaseUnavailable),
                message: "test: refuse".to_owned(),
            });
        }
        self.inner.launch_recipe(state_dir)
    }
    fn supports_private_key(&self) -> bool {
        self.inner.supports_private_key()
    }
    fn listener_policy(&self) -> Option<ListenerPolicy> {
        self.inner.listener_policy()
    }
    fn fence_path(&self, state_dir: &Path, endpoint: &str) -> Option<PathBuf> {
        self.inner.fence_path(state_dir, endpoint)
    }
    fn govern_private(&self, endpoint: &str, key: &EngineKey) {
        self.inner.govern_private(endpoint, key);
    }
    fn govern_external(&self, endpoint: &str, settings_key_verified: bool) {
        self.inner.govern_external(endpoint, settings_key_verified);
    }
    fn release(&self, endpoint: &str) {
        self.inner.release(endpoint);
    }
    fn current_key(&self, _endpoint: &str) -> Option<String> {
        self.external_key.lock().unwrap().clone()
    }
}

/// 一份 oMLX 后端：模型目录是 `<工作目录>/models`（可换），不做签名检查，外部引擎的监听者要求团队 [`TEAM`]。
fn omlx_backend(models_dir: &Path) -> OmlxBackend {
    let models = models_dir.to_owned();
    OmlxBackend::from_env()
        .with_models_dir(move || Ok(models.clone()))
        .with_codesign(CodesignPolicy::Off)
        .with_listener_policy(ListenerPolicy::new(TEAM))
}

struct Harness {
    manager: EngineManager,
    health: HealthState,
    log: Arc<Mutex<Vec<String>>>,
}

fn upstream_status(id: &str) -> UpstreamStatus {
    UpstreamStatus {
        id: id.to_owned(),
        engine_kind: "omlx".to_owned(),
        state: UpstreamState::Starting,
        reason: None,
        managed: true,
        owned: false,
    }
}

impl Harness {
    /// 起一个管理器（一个托管上游 `omlx`）。`budget` 是额度；`stop_on_exit` 是 launcher 的设置。
    fn start(
        rig: &Rig,
        backend: Arc<TestBackend>,
        inspector: Arc<dyn ProcessInspector>,
        timings: Timings,
        stop_on_exit: StopOnExit,
    ) -> Self {
        Self::start_with(
            rig,
            backend,
            inspector,
            timings,
            stop_on_exit,
            rig.fake.executable.clone(),
        )
    }

    /// 同 [`Self::start`]，引擎的可执行文件由调用方给（给一个不存在的路径就是「没装 oMLX」）。
    fn start_with(
        rig: &Rig,
        backend: Arc<TestBackend>,
        inspector: Arc<dyn ProcessInspector>,
        timings: Timings,
        stop_on_exit: StopOnExit,
        executable: PathBuf,
    ) -> Self {
        let health = HealthState::with_upstreams(0, vec![upstream_status("omlx")]);
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink: LogSink = {
            let log = log.clone();
            Arc::new(move |line| log.lock().unwrap().push(line.to_owned()))
        };
        let manager = EngineManager::start(
            ManagerConfig {
                state_dir: rig.state_dir(),
                allocated_budget_bytes: Some(16 * GIB),
                stop_on_exit,
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
                backend,
            }],
            health.clone(),
        );
        Self {
            manager,
            health,
            log,
        }
    }

    fn view(&self) -> UpstreamStatus {
        self.health.view().upstreams[0].clone()
    }

    async fn until(&self, what: &str, done: impl Fn(&UpstreamStatus) -> bool) -> UpstreamStatus {
        let deadline = Instant::now() + PATIENT;
        loop {
            let status = self.view();
            if done(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn until_state(&self, state: UpstreamState) -> UpstreamStatus {
        self.until(&format!("{state:?}"), |status| status.state == state)
            .await
    }

    fn logged(&self, needle: &str) -> bool {
        self.log
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle))
    }
}

fn default_backend(rig: &Rig) -> Arc<TestBackend> {
    TestBackend::new(omlx_backend(&rig.work.path().join("models")))
}

// ---------------------------------------------------------------------------------------------
// 就绪、私有 key、状态与门控
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_engine_that_is_not_there_is_spawned_and_becomes_ready_and_owned_with_its_private_key() {
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Ready).await;
    assert_eq!(status.reason, None);
    assert!(status.owned, "an engine iDoris spawned is owned");
    assert!(status.managed);
    assert_eq!(rig.fake.launches(), 1);
    // 私有 key 登记给了这个 endpoint：发往它的请求（适配器、代理、模型列表）只用它。
    let governed = governed_endpoint(&rig.endpoint());
    assert!(
        matches!(governed, Some(EndpointKey::Private(ref key)) if key.len() == 64),
        "{governed:?}"
    );
    // 引擎只认这个 key：就绪探测带着它，每个请求都是 ok。
    assert!(rig.fake.requests().iter().all(|(_, auth)| auth == "ok"));
    h.manager.shutdown(true).await;
    // 停止之后：引擎没了，记录删了，私有 key 不再留在进程里。
    assert!(!pid_alive(rig.pid(0)));
    assert_eq!(rig.records(), 0);
    assert!(governed_endpoint(&rig.endpoint()).is_none());
}

#[tokio::test]
async fn before_the_gate_opens_an_engine_exit_is_a_restart_and_never_an_engine_failure() {
    // 门控一直没放行（调用方没有调用 `open_gate`）：引擎自己退出（`exit_after_secs`）是门控之前的失败，走重启与熔断，
    // 不是退出码 5。
    let rig = Rig::new(&Behavior {
        exit_after_secs: 0.8,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    assert_eq!(
        h.health.gate(),
        GateState::Pending,
        "starting the manager closes the gate"
    );
    wait_for("a restart after the engine exited", || {
        rig.fake.launches() >= 2
    })
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), h.manager.engine_failed())
            .await
            .is_err(),
        "an exit before the gate opened is not an engine failure"
    );
    assert!(!h.manager.gate_is_open());
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_that_exits_after_the_gate_opened_is_an_engine_failure_not_a_restart() {
    let rig = Rig::new(&Behavior {
        exit_after_secs: 1.5,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.manager.wait_all_settled().await;
    assert!(
        h.manager.open_gate().is_some(),
        "everything is ready, so the gate opens"
    );
    assert!(h.manager.gate_is_open());
    h.health.set_gate(GateState::Open);
    let failure = tokio::time::timeout(PATIENT, h.manager.engine_failed())
        .await
        .expect("the exit after the gate is reported");
    assert_eq!(failure.upstream_id, "omlx");
    // 进程内无法恢复（Supervisor 的账本已经过期，热恢复是 v0.6.1）：不重启，状态是 failed，等 iDoris 以退出码 5 停止。
    assert_eq!(h.view().state, UpstreamState::Failed);
    assert!(!h.view().owned);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(rig.fake.launches(), 1, "no restart after the gate opened");
    assert_eq!(h.health.view().phase, Phase::Degraded);
    assert!(h.logged("退出码 5"));
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_process_exit_is_noticed_on_the_liveness_tick_not_on_the_probe_interval() {
    // 探测很稀（20 秒一次），存活检查很密（50 毫秒）：进程退出要在存活检查那一拍里发现，不能等下一次探测。
    let rig = Rig::new(&Behavior {
        exit_after_secs: 1.0,
        ..Behavior::default()
    });
    let mut timings = fast();
    timings.probe_interval = Duration::from_secs(20);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    h.manager.wait_all_settled().await;
    assert!(h.manager.open_gate().is_some());
    let failure = tokio::time::timeout(Duration::from_secs(10), h.manager.engine_failed())
        .await
        .expect("the exit is noticed long before the next probe is due");
    assert_eq!(failure.upstream_id, "omlx");
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn the_gate_waits_while_an_engine_is_still_starting_and_opens_once_it_is_ready() {
    // 还在 `starting` 的引擎不算落定：门控等它，不放行；好了之后放行，没有降级。
    let rig = Rig::new(&Behavior {
        listen_delay_secs: 1.0,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    assert_eq!(h.view().state, UpstreamState::Starting);
    assert!(
        h.manager.open_gate().is_none(),
        "a starting engine is not settled"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(400), h.manager.wait_all_settled())
            .await
            .is_err()
    );
    assert!(!h.manager.gate_is_open());
    tokio::time::timeout(PATIENT, h.manager.wait_all_settled())
        .await
        .expect("ready is settled");
    let outcome = h.manager.open_gate().expect("settled: the gate opens");
    assert!(outcome.degraded.is_empty(), "everything is ready");
    assert!(h.manager.gate_is_open());
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_terminally_failed_engine_does_not_hold_the_gate_it_opens_degraded_and_the_engine_is_frozen()
 {
    // 第二个实例起不来（`launch_recipe` 拒绝，配置性的终局失败）：它「落定」了，不拖住门控；第一个就绪。
    let rig_a = Rig::new(&Behavior::default());
    let backend_a = default_backend(&rig_a);
    let backend_b = default_backend(&rig_a);
    *backend_b.refuse_launch.lock().unwrap() = true;
    let health =
        HealthState::with_upstreams(0, vec![upstream_status("omlx"), upstream_status("omlx-b")]);
    let port_b = free_port();
    let manager = EngineManager::start(
        ManagerConfig {
            state_dir: rig_a.state_dir(),
            allocated_budget_bytes: Some(16 * GIB),
            stop_on_exit: StopOnExit::Owned,
            executables: BTreeMap::from([("omlx".to_owned(), rig_a.fake.executable.clone())]),
            launched_by: "test".to_owned(),
            timings: fast(),
            inspector: {
                let scripted = Scripted::claiming(rig_a.port);
                scripted.claims.lock().unwrap().push(port_b);
                scripted
            },
            log: Arc::new(|_| {}),
        },
        vec![
            ManagedPlan {
                id: "omlx".to_owned(),
                endpoint: rig_a.endpoint(),
                port: rig_a.port,
                backend: backend_a,
            },
            ManagedPlan {
                id: "omlx-b".to_owned(),
                endpoint: format!("http://127.0.0.1:{port_b}"),
                port: port_b,
                backend: backend_b.clone(),
            },
        ],
        health.clone(),
    );
    wait_for("omlx ready and omlx-b failed", || {
        let view = health.view();
        view.upstreams[0].state == UpstreamState::Ready
            && view.upstreams[1].state == UpstreamState::Failed
    })
    .await;
    assert_eq!(
        health.view().upstreams[1].reason,
        Some(UpstreamReason::BaseUnavailable)
    );
    assert_eq!(health.view().phase, Phase::Degraded);
    // 终局的失败不拖住门控：两个都落定了，放行，降级：失败的那个带着当时的状态与原因。
    tokio::time::timeout(PATIENT, manager.wait_all_settled())
        .await
        .expect("both are settled");
    let outcome = manager.open_gate().expect("settled: the gate opens");
    assert_eq!(
        outcome.degraded.len(),
        1,
        "only the failed one: {outcome:?}"
    );
    let failure = &outcome.degraded["omlx-b"];
    assert_eq!(failure.engine_kind, "omlx");
    assert_eq!(failure.state, UpstreamState::Failed);
    assert_eq!(failure.reason, Some(UpstreamReason::BaseUnavailable));
    health.set_gate(GateState::Open);
    assert_eq!(
        health.view().phase,
        Phase::Degraded,
        "a managed engine that is down: degraded, even though the other one serves"
    );
    // 冻结：降级放行之后它不再启动，哪怕「修好了」；要重启 iDoris（它的 Supervisor 是放行时创建的）。
    *backend_b.refuse_launch.lock().unwrap() = false;
    assert_eq!(
        manager.start_upstream("omlx-b").await,
        Err(CommandError::RestartRequired)
    );
    assert_eq!(
        manager.restart_upstream("omlx-b").await,
        Err(CommandError::RestartRequired)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(health.view().upstreams[1].state, UpstreamState::Failed);
    assert_eq!(rig_a.fake.launches(), 1, "nothing was launched for omlx-b");
    // 就绪的那个不受影响：`start` 幂等。
    manager.start_upstream("omlx").await.unwrap();
    manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 失败、退避、熔断与半开重试
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_loop_trips_after_more_than_the_limit_then_retries_half_open_and_recovers() {
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(5),
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        observable_restarts(),
        StopOnExit::Owned,
    );
    // 上限 2：前两次失败是 `restarting`（退避），第三次失败（超过上限）就是 `failed` + `crash_loop`。
    let restarting = h.until_state(UpstreamState::Restarting).await;
    assert_eq!(
        restarting.reason, None,
        "an engine that exits has no stable reason code"
    );
    let failed = h.until_state(UpstreamState::Failed).await;
    assert_eq!(failed.reason, Some(UpstreamReason::CrashLoop));
    assert!(!failed.owned);
    assert_eq!(
        rig.fake.launches(),
        3,
        "three failed starts, then the breaker"
    );
    assert_eq!(h.health.view().phase, Phase::Degraded);
    // 半开：之后每 400 毫秒试一次，不永久放弃（也不更快：这期间不再有退避重试）。
    h.until("a half-open retry", |_| rig.fake.launches() >= 4)
        .await;
    h.until_state(UpstreamState::Failed).await;
    assert_eq!(
        h.view().reason,
        Some(UpstreamReason::CrashLoop),
        "the half-open attempt failed again"
    );
    // 引擎修好了（换脚本，已经起过的不受影响）：下一次半开重试成功，回到 ready。
    rig.fake.set_behavior(&Behavior::default());
    let ready = h
        .until("recovery through a half-open retry", |s| {
            s.state == UpstreamState::Ready
        })
        .await;
    assert_eq!(ready.reason, None);
    assert!(ready.owned);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_that_exits_at_once_while_something_else_holds_its_port_says_port_in_use() {
    // 探测的时候端口还是空的（连接被拒），引擎拉起来绑不上端口就退出了；这时 `lsof` 看到别人在监听：不是引擎自己的毛病。
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(98),
        ..Behavior::default()
    });
    let inspector = Scripted::claiming(rig.port);
    inspector.external_listener(rig.port, std::process::id(), None);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        observable_restarts(),
        StopOnExit::Owned,
    );
    let status = h
        .until("a restart that names the port", |s| {
            s.state == UpstreamState::Restarting && s.reason == Some(UpstreamReason::PortInUse)
        })
        .await;
    assert!(!status.owned);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn restarts_back_off_by_doubling_from_the_base() {
    // 熔断上限放宽到 5，看前四次重启的间隔：60、120、240、480 毫秒（封顶 500）。间隔是两次启动之间的时间，
    // 含 python 起动与退出的开销，所以只断言下界，并要求后面的间隔明显更长（不是每次都等一样久）。
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(5),
        ..Behavior::default()
    });
    let mut timings = fast();
    timings.crash_limit = 5;
    timings.restart_backoff_base = Duration::from_millis(250);
    timings.restart_backoff_cap = Duration::from_millis(2000);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    wait_for("five launches", || rig.fake.launches() >= 5).await;
    h.manager.shutdown(true).await;
    let times: Vec<f64> = rig
        .fake
        .all_launches()
        .iter()
        .map(|launch| launch["time"].as_f64().unwrap())
        .collect();
    let gaps: Vec<f64> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let expected = [0.25, 0.5, 1.0, 2.0];
    for (index, (gap, delay)) in gaps.iter().zip(expected).enumerate() {
        assert!(
            *gap >= delay * 0.95,
            "restart {}: waited {gap:.3} s, the backoff is {delay} s: {gaps:?}",
            index + 1
        );
    }
}

#[tokio::test]
async fn an_explicit_start_clears_the_failure_count_so_the_breaker_does_not_trip_at_once() {
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(5),
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        observable_restarts(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Failed).await;
    let launches = rig.fake.launches();
    h.manager.start_upstream("omlx").await.unwrap();
    // 之前的失败不再计数：第一次失败是 `restarting`，不是立刻又 `failed`。
    let next = h
        .until("the state to leave failed", |s| {
            s.state != UpstreamState::Failed
        })
        .await;
    assert!(
        matches!(
            next.state,
            UpstreamState::Starting | UpstreamState::Restarting
        ),
        "{next:?}"
    );
    h.until("another launch", |_| rig.fake.launches() > launches)
        .await;
    // 那次启动也失败了：窗口被清过，所以它是新窗口里的第一次失败，`restarting`；没清的话（之前的三次还在）会立刻又 `failed`。
    let after = h
        .until("the first failure after the start", |s| {
            matches!(s.state, UpstreamState::Restarting | UpstreamState::Failed)
        })
        .await;
    assert_eq!(after.state, UpstreamState::Restarting, "{after:?}");
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_start_command_while_ready_or_starting_changes_nothing() {
    let rig = Rig::new(&Behavior {
        listen_delay_secs: 2.0,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    // 引擎已经真的起来了（记录写下了）、但还没开始监听：这时的 `start` 不能把进行到一半的启动废掉重来。
    wait_for("the engine to be launched", || rig.fake.launches() >= 1).await;
    let first = rig.pid(0);
    assert_eq!(h.view().state, UpstreamState::Starting);
    h.manager.start_upstream("omlx").await.unwrap(); // while starting
    h.until_state(UpstreamState::Ready).await;
    assert!(
        pid_alive(first),
        "the engine that was being started is the one that became ready"
    );
    h.manager.start_upstream("omlx").await.unwrap(); // idempotent when ready
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.view().state, UpstreamState::Ready);
    assert_eq!(rig.fake.launches(), 1, "no second engine");
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 命令：stop、restart、start；对非自有引擎一律拒绝
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_restart_and_start_act_only_on_an_engine_idoris_owns() {
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    let first_pid = rig.pid(0);

    // stop：自有引擎停掉（整个进程组），状态 `stopped`，记录与私有 key 都没了。
    h.manager.stop_upstream("omlx").await.unwrap();
    let stopped = h.until_state(UpstreamState::Stopped).await;
    assert!(!stopped.owned);
    assert_eq!(stopped.reason, None);
    wait_for("the engine to die", || !pid_alive(first_pid)).await;
    assert_eq!(rig.records(), 0);
    assert!(governed_endpoint(&rig.endpoint()).is_none());
    assert_eq!(
        h.health.view().phase,
        Phase::Degraded,
        "a stopped managed engine keeps the gate shut"
    );
    // 已经 stopped：再 stop 幂等；restart 需要一个自有引擎，现在没有，拒绝。
    h.manager.stop_upstream("omlx").await.unwrap();
    assert_eq!(
        h.manager.restart_upstream("omlx").await,
        Err(CommandError::NotOwned)
    );
    assert_eq!(rig.fake.launches(), 1, "stopping does not respawn");

    // start：从 stopped 回到启动决策。
    h.manager.start_upstream("omlx").await.unwrap();
    h.until("a new engine", |s| {
        s.state == UpstreamState::Ready && s.owned
    })
    .await;
    assert_eq!(rig.fake.launches(), 2);
    let second_pid = rig.pid(1);
    assert_ne!(first_pid, second_pid);

    // restart：停掉再重新走启动决策，换了一个新的引擎进程。
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the third launch", || rig.fake.launches() == 3).await;
    h.until("ready again", |s| {
        s.state == UpstreamState::Ready && s.owned
    })
    .await;
    wait_for("the second engine to die", || !pid_alive(second_pid)).await;
    assert!(pid_alive(rig.pid(2)));

    // 不存在的上游：NotFound。
    assert_eq!(
        h.manager.stop_upstream("nope").await,
        Err(CommandError::NotFound)
    );
    assert_eq!(
        h.manager.start_upstream("nope").await,
        Err(CommandError::NotFound)
    );
    assert_eq!(
        h.manager.restart_upstream("nope").await,
        Err(CommandError::NotFound)
    );
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 外部引擎：只探测、绝不发信号；端口属主校验先于 key
// ---------------------------------------------------------------------------------------------

/// 测试自己起的「外部引擎」：假引擎，不经 iDoris。`key` 有值就要求这个 key。
struct External {
    child: Option<child_guard::ChildGuard>,
    fake: FakeOmlx,
    port: u16,
}

impl External {
    fn start(work: &Path, key: Option<&str>) -> Self {
        let dir = work.join("external");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = FakeOmlx::create(&dir, &Behavior::default());
        let port = free_port();
        let mut command = Command::new(&fake.executable);
        command
            .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", work)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(key) = key {
            command.env("OMLX_API_KEY", key);
        }
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

/// 一个把托管端口指到外部引擎上的 rig（管理器自己不会拉起任何东西：端口上已经有引擎）。
fn rig_for(external: &External) -> Rig {
    let mut rig = Rig::new(&Behavior::default());
    rig.port = external.port;
    rig
}

#[tokio::test]
async fn an_external_engine_is_adopted_without_a_signal_and_survives_idoris_even_when_engines_are_stopped()
 {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), None);
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );

    let status = h.until_state(UpstreamState::Ready).await;
    assert!(!status.owned, "an external engine is never owned");
    assert_eq!(status.reason, None);
    assert_eq!(
        rig.fake.launches(),
        0,
        "nothing is spawned beside an engine that already answers"
    );
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::External {
            settings_key_verified: true
        }),
        "the owner check passed, so the settings key may go to it"
    );
    // 命令：对不是自己的引擎，stop、restart 一律拒绝；start 在已经就绪时幂等。
    assert_eq!(
        h.manager.stop_upstream("omlx").await,
        Err(CommandError::NotOwned)
    );
    assert_eq!(
        h.manager.restart_upstream("omlx").await,
        Err(CommandError::NotOwned)
    );
    h.manager.start_upstream("omlx").await.unwrap();
    assert!(h.manager.open_gate().is_some());
    // 停止时「停掉自己拥有的引擎」：外部引擎不在其中。它仍在运行，没有所有权记录，围栏原样。
    h.manager.shutdown(true).await;
    assert!(
        pid_alive(u64::from(external.pid())),
        "iDoris never signals an engine it does not own"
    );
    assert_eq!(rig.records(), 0);
    assert!(
        governed_endpoint(&rig.endpoint()).is_none(),
        "released at shutdown"
    );
    // 引擎看到的只有匿名请求（它没要求 key）：没有带 key 的请求。
    assert!(
        external
            .fake
            .requests()
            .iter()
            .all(|(_, auth)| auth == "none")
    );
}

#[tokio::test]
async fn an_external_engine_behind_a_leftover_fence_is_not_let_through_and_the_fence_is_left_alone()
{
    // 外部引擎的围栏不是我们的（没有证据证明它的加载已经停止）：原样留着，要人工处理。Supervisor 在围栏存在时会把启动错误
    // 粘滞住，所以不把它当成就绪：状态是 `unavailable` + `fence_pending`（不是放行之后才报 `startup_latched`）；它是终局的失败，
    // 门控不被它拖住，降级放行。
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), None);
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let fence = rig.fence_path();
    std::fs::create_dir_all(fence.parent().unwrap()).unwrap();
    std::fs::write(&fence, b"idoris load in progress\n").unwrap();
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(status.reason, Some(UpstreamReason::FencePending));
    assert!(!status.owned);
    assert_eq!(h.health.view().phase, Phase::Degraded);
    // 终局的失败不拖住门控：降级放行（别的卡照常服务），围栏与引擎都不动。
    let outcome = h
        .manager
        .open_gate()
        .expect("a terminal failure is settled: the gate opens, degraded");
    assert_eq!(
        outcome.degraded["omlx"].reason,
        Some(UpstreamReason::FencePending)
    );
    assert_eq!(rig.fake.launches(), 0, "nothing is spawned beside it");
    h.manager.shutdown(true).await;
    assert!(fence.exists(), "an external engine's fence is left alone");
    assert!(
        pid_alive(u64::from(external.pid())),
        "and the engine is not signalled"
    );
    assert!(
        external
            .fake
            .requests()
            .iter()
            .all(|(_, auth)| auth == "none")
    );
}

#[tokio::test]
async fn an_external_engine_that_wants_a_key_is_asked_only_after_its_owner_was_verified() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), Some("ext-secret"));
    let rig = rig_for(&external);

    // 属主校验通过，但调用方没有 key：`unavailable` + `auth_required`，不拉起第二个。
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let backend = default_backend(&rig);
    let h = Harness::start(
        &rig,
        backend.clone(),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(status.reason, Some(UpstreamReason::AuthRequired));
    assert!(!status.owned);
    assert_eq!(h.health.view().phase, Phase::Degraded);
    h.manager.shutdown(true).await;

    // 错的 key：`auth_failed`。
    *backend.external_key.lock().unwrap() = Some("wrong".to_owned());
    let h = Harness::start(
        &rig,
        backend.clone(),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(status.reason, Some(UpstreamReason::AuthFailed));
    h.manager.shutdown(true).await;

    // 对的 key：接管，ready，`owned:false`。
    *backend.external_key.lock().unwrap() = Some("ext-secret".to_owned());
    let h = Harness::start(
        &rig,
        backend.clone(),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Ready).await;
    assert!(!status.owned);
    h.manager.shutdown(true).await;
    assert!(pid_alive(u64::from(external.pid())));
    assert!(inspector.team_checks.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn a_listener_that_fails_the_owner_check_is_not_adopted_and_never_sees_a_credential() {
    // 监听者的团队不对、没有团队（未签名）、别的用户、读不出身份：四种都不接管，`failed` + `port_in_use`。
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), Some("ext-secret"));
    for (case, team, listed) in [
        ("another team", Some("EVILTEAM99"), true),
        ("no team", None, true),
        // 没编排监听者：校验读到「没有人监听」，按探测与校验之间监听者没了处理：重试，不接管，也不发 key。
        ("the listener is not even visible", Some(TEAM), false),
    ] {
        let rig = rig_for(&external);
        let inspector = Scripted::new();
        if listed {
            inspector.external_listener(external.port, external.pid(), team);
        }
        let backend = default_backend(&rig);
        // 即使调用方有 key，也不能发给没通过校验的监听者。
        *backend.external_key.lock().unwrap() = Some("ext-secret".to_owned());
        let h = Harness::start(&rig, backend, inspector, fast(), StopOnExit::Owned);
        if listed {
            let status = h.until_state(UpstreamState::Failed).await;
            assert_eq!(status.reason, Some(UpstreamReason::PortInUse), "{case}");
            assert!(!status.owned);
            assert_eq!(h.health.view().phase, Phase::Degraded);
        } else {
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_ne!(
                h.view().state,
                UpstreamState::Ready,
                "{case}: never adopted"
            );
        }
        h.manager.shutdown(true).await;
        assert_eq!(rig.fake.launches(), 0, "{case}: no second engine beside it");
    }
    // 外部引擎要 key：它收到的每个请求都没有带 key（匿名探测被拒，之后不再有请求带凭证）。
    let seen = external.fake.requests();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|(_, auth)| auth == "none"),
        "no credential reached the unverified listener: {seen:?}"
    );
    assert!(pid_alive(u64::from(external.pid())));
}

#[tokio::test]
async fn another_process_that_takes_the_port_after_the_spawn_never_receives_the_private_key() {
    // 引擎进程活着但没有监听端口（`never_listen`）；这时别的进程占住了托管端口。在确认监听端口的是引擎进程之前，
    // 私有 key 不发出去，探测也不发：冒充者一个请求都收不到。
    let rig = Rig::new(&Behavior {
        never_listen: true,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        // 真的检查器：`listens_on(引擎 pid, 端口)` 问 lsof，冒充者是测试进程，不是引擎 pid；没有 lsof 时读不出来，同样不算确认。
        Arc::new(SystemInspector::new()),
        fast(),
        StopOnExit::Owned,
    );
    wait_for("the engine to be spawned", || rig.fake.launches() == 1).await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", rig.port))
        .await
        .unwrap();
    let impostor = wiremock::MockServer::builder()
        .listener(listener.into_std().unwrap())
        .start()
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object": "list", "data": []})),
        )
        .mount(&impostor)
        .await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        h.view().state,
        UpstreamState::Starting,
        "not ready: the engine does not hold the port"
    );
    assert!(
        impostor.received_requests().await.unwrap().is_empty(),
        "nothing, not even a probe, is sent before the listener is confirmed to be the engine"
    );
    h.manager.shutdown(true).await;
    assert!(
        impostor.received_requests().await.unwrap().is_empty(),
        "nor while the engine is being stopped"
    );
}

#[tokio::test]
async fn an_engine_that_never_listens_is_stopped_after_the_startup_timeout_and_retried() {
    let rig = Rig::new(&Behavior {
        never_listen: true,
        ..Behavior::default()
    });
    let mut timings = observable_restarts();
    timings.startup_timeout = Duration::from_secs(2);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    // 等不到就绪：启动超时，`restarting` + `startup_timeout`；引擎被停掉，退避之后重试。
    let status = h
        .until("the startup timeout", |s| {
            s.reason == Some(UpstreamReason::StartupTimeout)
        })
        .await;
    assert_eq!(status.state, UpstreamState::Restarting);
    assert!(!status.owned);
    // 退避之后重试：第二个引擎拉起来之前，第一个已经被停掉了（不管 python 起得多慢，先记下来的那个都是更早的）。
    wait_for("the retry", || rig.fake.launches() >= 2).await;
    assert!(
        !pid_alive(rig.pid(0)),
        "the engine that never listened is stopped before the next one is launched"
    );
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 孤儿接管、围栏
// ---------------------------------------------------------------------------------------------

/// 先起一个管理器，把引擎带到就绪并封印，再以 `stop_on_exit:"never"` 的正常退出结束它：引擎与所有权记录都留着，
/// 等价于「iDoris 被 kill -9 时引擎成了孤儿」（进程内没法 kill -9 自己，但留下的东西一样）。
async fn leave_an_orphan(rig: &Rig, inspector: &Arc<Scripted>) -> u64 {
    let h = Harness::start(
        rig,
        default_backend(rig),
        inspector.clone(),
        fast(),
        StopOnExit::Never,
    );
    h.until_state(UpstreamState::Ready).await;
    h.manager.shutdown(false).await;
    let pid = rig.pid(0);
    assert!(pid_alive(pid), "never: the engine is left running");
    assert_eq!(rig.records(), 1, "and its ownership record");
    pid
}

#[tokio::test]
async fn a_new_manager_adopts_the_engine_a_previous_one_left_and_goes_on_using_its_key() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let pid = leave_an_orphan(&rig, &inspector).await;
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Ready).await;
    assert!(
        status.owned,
        "adopted: the engine is ours, not an external one"
    );
    assert_eq!(
        rig.fake.launches(),
        1,
        "not restarted: the loaded models stay in memory"
    );
    assert!(pid_alive(pid));
    // 沿用记录里的私有 key：每个请求都是 ok（引擎只认它自己的 key）。
    assert!(rig.fake.requests().iter().all(|(_, auth)| auth == "ok"));
    assert!(matches!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Private(_))
    ));
    // 接管来的引擎也是自己的：停止时停掉它，记录删了。
    h.manager.shutdown(true).await;
    wait_for("the adopted engine to die", || !pid_alive(pid)).await;
    assert_eq!(rig.records(), 0);
}

#[tokio::test]
async fn a_fence_makes_an_adopted_own_engine_killed_cleared_and_respawned() {
    // （外部引擎的围栏原样留着、不放行：见 `an_external_engine_behind_a_leftover_fence_…`。）
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let pid = leave_an_orphan(&rig, &inspector).await;
    // 加载进行中被杀：围栏留着。
    let fence = rig.fence_path();
    std::fs::create_dir_all(fence.parent().unwrap()).unwrap();
    std::fs::write(&fence, b"idoris load in progress\n").unwrap();
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    h.until("a respawned engine", |s| {
        s.state == UpstreamState::Ready && s.owned
    })
    .await;
    wait_for("the old engine to die", || !pid_alive(pid)).await;
    assert_eq!(rig.fake.launches(), 2);
    assert!(pid_alive(rig.pid(1)));
    assert!(
        !fence.exists(),
        "cleared: loads are no longer refused fail-closed"
    );
    assert!(h.logged("围栏"));
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_killed_while_starting_is_found_unsealed_stopped_and_respawned() {
    // 上一个 iDoris 在引擎启动途中被杀：记录没封印。它没有就绪过、什么都没加载，只能停掉重拉。
    let rig = Rig::new(&Behavior {
        listen_delay_secs: 30.0,
        ..Behavior::default()
    });
    let backend = default_backend(&rig);
    let recipe = backend.launch_recipe(&rig.state_dir()).unwrap();
    let instance = EngineInstance::new(recipe, &rig.state_dir(), rig.port).unwrap();
    let key = EngineKey::generate().unwrap();
    let launch = EngineLaunch::new(
        &instance,
        EngineLaunchSpec {
            executable: rig.fake.executable.clone(),
            allocated_budget_bytes: Some(16 * GIB),
            api_key: Some(key),
            stop_on_exit: StopOnExit::Never,
        },
        std::env::vars_os(),
    )
    .unwrap();
    let inspector = Scripted::claiming(rig.port);
    let left = OwnedEngine::spawn(launch, inspector.clone(), "previous-instance")
        .await
        .unwrap();
    let old_pid = u64::from(left.pid());
    assert!(!left.is_sealed());
    drop(left); // never: the engine and its (unsealed) record stay
    assert!(pid_alive(old_pid));
    // Let it get as far as running its script (and so reading the slow behaviour) before the script is swapped.
    wait_for("the first engine to run its script", || {
        rig.fake.launches() == 1
    })
    .await;
    rig.fake.set_behavior(&Behavior::default());
    let h = Harness::start(&rig, backend, inspector, fast(), StopOnExit::Owned);
    h.until("a fresh engine", |s| {
        s.state == UpstreamState::Ready && s.owned
    })
    .await;
    wait_for("the unsealed engine to die", || !pid_alive(old_pid)).await;
    assert_eq!(rig.fake.launches(), 2);
    assert!(h.logged("没有封印"));
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_freshly_spawned_engine_clears_a_fence_nobody_owns_any_more_and_an_uncleanable_fence_blocks_the_launch()
 {
    // 端口上没有任何引擎，围栏却在（上一代被杀时留下的）：新拉起的引擎什么都没加载，清掉围栏再拉起。
    let rig = Rig::new(&Behavior::default());
    let fence = rig.fence_path();
    std::fs::create_dir_all(fence.parent().unwrap()).unwrap();
    std::fs::write(&fence, b"idoris load in progress\n").unwrap();
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    assert!(!fence.exists());
    h.manager.shutdown(true).await;

    // 清不掉的围栏（这里是一个非空目录占着它的位置）：不拉起，`failed` + `fence_pending`。
    let rig = Rig::new(&Behavior::default());
    let fence = rig.fence_path();
    std::fs::create_dir_all(fence.join("inner")).unwrap();
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Failed).await;
    assert_eq!(status.reason, Some(UpstreamReason::FencePending));
    assert_eq!(
        rig.fake.launches(),
        0,
        "an engine started behind a fence it cannot clear would refuse every load"
    );
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 每次拉起都重新取模型目录；模型目录取不到就不拉起
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_model_dir_is_asked_for_again_on_every_launch_and_a_dir_omlx_would_split_stops_the_launches()
 {
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(5),
        ..Behavior::default()
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let backend = TestBackend::new(
        OmlxBackend::from_env()
            .with_models_dir(move || {
                match counter.fetch_add(1, Ordering::SeqCst) + 1 {
                    n @ (1 | 2) => Ok(PathBuf::from(format!("/models/{n}"))),
                    // 第三次：HOME 变成了带逗号的路径（S4 的 `managed_models_dir()` 拒绝它）。
                    _ => Err(idoris_router::setup_engine::SetupError::new(
                        "validation_failed",
                        "模型目录路径含逗号",
                    )),
                }
            })
            .with_codesign(CodesignPolicy::Off),
    );
    let mut timings = fast();
    timings.crash_limit = 10;
    let h = Harness::start(
        &rig,
        backend,
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Failed).await;
    assert_eq!(
        status.reason, None,
        "the model dir has no stable reason code"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "asked once per launch attempt"
    );
    assert_eq!(rig.fake.launches(), 2, "the third attempt did not launch");
    // 每次拉起的 `--model-dir` 是那一次问到的值（不是缓存的第一次）。
    let model_dir = |launch: usize| {
        let argv = rig.fake.launch(launch)["argv"].clone();
        let argv: Vec<String> = argv
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect();
        let at = argv.iter().position(|a| a == "--model-dir").unwrap();
        argv[at + 1].clone()
    };
    assert_eq!(model_dir(0), "/models/1");
    assert_eq!(model_dir(1), "/models/2");
    assert!(h.logged("模型目录"));
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 监管：进程活着但引擎「不好用」只改状态，不杀、不退出、不重拉；好了就回来
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_own_engine_that_is_alive_but_stops_answering_is_unavailable_never_killed_and_comes_back()
 {
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    assert!(
        h.manager.open_gate().is_some(),
        "from here an engine exit is fatal; an unresponsive engine must not be"
    );
    let pid = rig.pid(0);
    // oMLX 加载大模型时 `/v1/models` 可能很慢或回 503：那不是失效。
    rig.fake.force_status(503);
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert!(status.owned, "it is still ours");
    assert_eq!(status.reason, None);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(h.view().state, UpstreamState::Unavailable);
    assert!(pid_alive(pid), "never killed");
    assert_eq!(rig.fake.launches(), 1, "never respawned");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), h.manager.engine_failed())
            .await
            .is_err(),
        "an unresponsive engine is not an engine failure: no exit code 5"
    );
    rig.fake.answer_normally();
    let status = h.until_state(UpstreamState::Ready).await;
    assert!(status.owned);
    assert_eq!(rig.fake.launches(), 1);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_that_rejects_the_key_twice_while_starting_still_becomes_ready() {
    let rig = Rig::new(&Behavior::default());
    // 引擎一起来就对所有请求回 401（比如它的 key 还没生效）；只有前两次探测被拒，第三次之前就好了。
    rig.fake.force_status(401);
    let mut timings = fast();
    timings.ready_initial_interval = Duration::from_millis(400);
    timings.ready_max_interval = Duration::from_millis(800);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    wait_for("two rejected probes", || rig.fake.requests().len() >= 2).await;
    rig.fake.answer_normally();
    let status = h.until_state(UpstreamState::Ready).await;
    assert!(status.owned);
    assert_eq!(
        rig.fake.launches(),
        1,
        "two rejections are not enough to give the engine up"
    );
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_that_keeps_rejecting_its_own_key_while_starting_is_given_up_with_auth_failed() {
    let rig = Rig::new(&Behavior::default());
    rig.fake.force_status(401);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        observable_restarts(),
        StopOnExit::Owned,
    );
    // 连续三次被拒才算：这个引擎不认自己的 key，停掉它，退避之后重来。
    let status = h
        .until("a restart that names the rejected key", |s| {
            s.state == UpstreamState::Restarting && s.reason == Some(UpstreamReason::AuthFailed)
        })
        .await;
    assert!(!status.owned);
    wait_for("the engine to be stopped", || !pid_alive(rig.pid(0))).await;
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_whose_port_turns_out_to_be_held_by_another_process_when_sealed_is_stopped() {
    let rig = Rig::new(&Behavior::default());
    // 等就绪的时候监听这个端口的是引擎（`listens_on` 为真），封印所有权记录时再看，端口已经是别人的了。
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming_once(rig.port),
        observable_restarts(),
        StopOnExit::Owned,
    );
    let status = h
        .until("a restart that names the port", |s| {
            s.state == UpstreamState::Restarting && s.reason == Some(UpstreamReason::PortInUse)
        })
        .await;
    assert!(!status.owned);
    wait_for("the engine to be stopped", || !pid_alive(rig.pid(0))).await;
    assert_eq!(
        rig.records(),
        0,
        "no ownership record is left for an engine that was refused"
    );
    assert!(
        governed_endpoint(&rig.endpoint()).is_none(),
        "its private key was never registered"
    );
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_own_engine_that_rejects_its_own_key_is_unavailable_with_auth_failed_and_left_alone() {
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    let pid = rig.pid(0);
    rig.fake.force_status(401);
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(status.reason, Some(UpstreamReason::AuthFailed));
    assert!(status.owned);
    assert!(pid_alive(pid));
    rig.fake.answer_normally();
    let status = h.until_state(UpstreamState::Ready).await;
    assert_eq!(status.reason, None);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_engine_that_stops_answering_is_unavailable_never_signalled_and_comes_back() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), None);
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    external.fake.force_status(503);
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert!(!status.owned);
    assert_eq!(status.reason, None);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        pid_alive(u64::from(external.pid())),
        "an engine iDoris does not own is never signalled"
    );
    assert_eq!(rig.fake.launches(), 0, "and nothing is started beside it");
    assert_eq!(rig.records(), 0);
    external.fake.answer_normally();
    h.until_state(UpstreamState::Ready).await;
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_listener_that_turns_into_another_service_is_failed_with_port_in_use() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), None);
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    // 200，但答的不是模型列表：这个端口上现在是别的服务。
    external.fake.force_status(200);
    let status = h.until_state(UpstreamState::Failed).await;
    assert_eq!(status.reason, Some(UpstreamReason::PortInUse));
    assert!(!status.owned);
    assert!(pid_alive(u64::from(external.pid())));
    external.fake.answer_normally();
    h.until_state(UpstreamState::Ready).await;
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_listener_that_stops_passing_the_owner_check_is_failed_and_the_settings_key_goes_off()
 {
    // 接管之后，端口上换了一个没通过属主校验的监听者（另一个进程，没有签名团队）：`failed` + `port_in_use`；`settings.json` 的 key
    // 立刻不再允许（登记表里是「未验证」）。**这里只钉管理器这一层**：key 登记表的口径与管理器自己的探测次数。数据面的请求怎么
    // 办是另一层的事，在 `lifecycle_admin.rs` 的 `b_g1_…` 里用真的 Router 钉：Router 创建之后才失败的卡，请求在运行期被挡住，
    // 503 带 `port_in_use`，没有一个字节发给那个端口；已经存在的适配器每个请求都按登记表此刻的口径取 key（`OmlxAdapter::
    // with_per_request_key`，在 `idoris-upstream` 的单元测试里钉）。
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), Some("ext-secret"));
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("ext-secret".to_owned());
    let h = Harness::start(&rig, backend, inspector.clone(), fast(), StopOnExit::Owned);
    h.until_state(UpstreamState::Ready).await;
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::External {
            settings_key_verified: true
        })
    );
    // 换监听者：另一个 pid（借测试进程自己：同一个用户，没有团队）。校验按 pid 与启动时间缓存，所以会重新校验。
    inspector.external_listener(external.port, std::process::id(), None);
    let status = h.until_state(UpstreamState::Failed).await;
    assert_eq!(status.reason, Some(UpstreamReason::PortInUse));
    assert!(!status.owned);
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::External {
            settings_key_verified: false
        }),
        "the settings.json key is off again for a listener that failed the check"
    );
    let before = external.fake.requests().len();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        external.fake.requests().len(),
        before,
        "the manager sends no more probes to a listener that failed the check"
    );
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_engine_whose_listener_is_gone_is_unavailable_and_nothing_is_spawned() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), None);
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    // 端口上不再有监听者（引擎被它自己的主人停了）：只报 `unavailable`，iDoris 不会在它旁边再拉起一个。
    inspector.forget_listener(external.port);
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(status.reason, None);
    assert!(!status.owned);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.fake.launches(), 0);
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    h.until_state(UpstreamState::Ready).await;
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_ready_engine_whose_supervisor_latched_its_startup_error_is_unavailable_not_ready() {
    use idoris_router::runtime::RuntimeRegistry;

    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    assert!(h.manager.open_gate().is_some());
    // Supervisor 在创建时对引擎采样一次：此刻引擎回 503，启动错误就粘滞住，之后所有调用都失败。
    rig.fake.force_status(503);
    let registry = RuntimeRegistry::spawn(&[card("omlx", &rig.endpoint(), true)]).unwrap();
    wait_for(
        "the Supervisor's startup sample to reach the engine",
        || {
            rig.fake
                .requests()
                .iter()
                .any(|(path, _)| path != "/v1/models")
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.manager.attach_runtimes(registry);
    rig.fake.answer_normally();
    // 引擎本身答得上来了，但聊天实际是 503：不能报 `ready`。
    let status = h
        .until("the latched Supervisor to show", |s| {
            s.reason == Some(UpstreamReason::StartupLatched)
        })
        .await;
    assert_eq!(status.state, UpstreamState::Unavailable);
    assert!(status.owned);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_instance_task_that_panics_is_an_engine_failure_and_never_a_silent_hang() {
    let rig = Rig::new(&Behavior::default());
    let backend = default_backend(&rig);
    *backend.panic_on_launch.lock().unwrap() = true;
    let h = Harness::start(
        &rig,
        backend,
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    // 管理器把它当成引擎失效报出去（`bin` 以退出码 5 停止，由启动器重启），不是无声地冻结在 starting。
    let failure = tokio::time::timeout(PATIENT, h.manager.engine_failed())
        .await
        .expect("the panic is reported");
    assert_eq!(failure.upstream_id, "omlx");
    let status = h.view();
    assert_eq!(status.state, UpstreamState::Failed);
    assert!(!status.owned);
    assert!(h.logged("panic"));
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 门控不被一个起不来的托管引擎无限期拖住：终局的失败「落定」，降级放行；等待有上限；放行之后它冻结
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_engine_that_is_not_installed_settles_the_gate_which_opens_degraded_and_the_engine_is_frozen()
 {
    let rig = Rig::new(&Behavior::default());
    let missing = rig.work.path().join("no-such-omlx-cli");
    let h = Harness::start_with(
        &rig,
        default_backend(&rig),
        Scripted::new(),
        fast(),
        StopOnExit::Owned,
        missing,
    );
    let status = h.until_state(UpstreamState::Absent).await;
    assert_eq!(status.reason, Some(UpstreamReason::EngineNotFound));
    // `absent` + `engine_not_found` 是终局：落定，门控放行，降级（别的卡照常服务）。
    tokio::time::timeout(PATIENT, h.manager.wait_all_settled())
        .await
        .expect("an engine that is not installed is settled");
    let outcome = h.manager.open_gate().expect("settled: the gate opens");
    assert_eq!(
        outcome.degraded["omlx"],
        ManagedFailure {
            engine_kind: "omlx".to_owned(),
            state: UpstreamState::Absent,
            reason: Some(UpstreamReason::EngineNotFound),
        }
    );
    assert!(h.manager.gate_is_open());
    assert_eq!(rig.fake.launches(), 0);
    // 冻结：用户装好引擎之后也要重启 iDoris；`start` 不接受。
    assert_eq!(
        h.manager.start_upstream("omlx").await,
        Err(CommandError::RestartRequired)
    );
    assert_eq!(h.view().state, UpstreamState::Absent);
    // 退出码 5 的语义不变：没有进程的失败不触发它。
    assert!(
        tokio::time::timeout(Duration::from_millis(200), h.manager.engine_failed())
            .await
            .is_err()
    );
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_tripped_breaker_settles_the_gate_and_no_half_open_retry_follows_a_degraded_open() {
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(5),
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        observable_restarts(),
        StopOnExit::Owned,
    );
    // 还在 `restarting`（熔断之前的失败）不算落定：门控等着。
    h.until_state(UpstreamState::Restarting).await;
    assert!(
        h.manager.open_gate().is_none(),
        "a restarting engine is not settled"
    );
    // 熔断：`failed` + `crash_loop` 落定，门控放行，降级。
    tokio::time::timeout(PATIENT, h.manager.wait_all_settled())
        .await
        .expect("a tripped breaker is settled");
    let outcome = h.manager.open_gate().expect("settled: the gate opens");
    assert_eq!(
        outcome.degraded["omlx"].reason,
        Some(UpstreamReason::CrashLoop)
    );
    assert_eq!(h.view().state, UpstreamState::Failed);
    let launches = rig.fake.launches();
    assert_eq!(launches, 3, "three failed starts, then the breaker");
    // 降级放行之后不再半开重试（门控放行之前会，见 `a_crash_loop_trips_…`）：半开间隔是 400 毫秒，等四个。
    tokio::time::sleep(Duration::from_millis(1700)).await;
    assert_eq!(
        rig.fake.launches(),
        launches,
        "no half-open retry after a degraded open"
    );
    assert_eq!(h.view().state, UpstreamState::Failed);
    assert_eq!(h.view().reason, Some(UpstreamReason::CrashLoop));
    assert_eq!(
        h.manager.start_upstream("omlx").await,
        Err(CommandError::RestartRequired)
    );
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn the_gate_task_gives_up_on_an_engine_that_never_becomes_ready_at_the_deadline_and_opens_degraded()
 {
    use axum::Router;
    use idoris_router::lifecycle::gate::{self, Finalized};
    use idoris_router::runtime::RuntimeRegistry;
    use idoris_router::swap::SwapApp;

    // 引擎活着、一直不监听；熔断远在天边（上限 50）：只有门控自己的截止时间能让它落定。
    let rig = Rig::new(&Behavior {
        never_listen: true,
        ..Behavior::default()
    });
    let mut timings = fast();
    timings.startup_timeout = Duration::from_millis(1500);
    timings.crash_limit = 50;
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    let manager = Arc::new(h.manager);
    let seen: Arc<Mutex<Option<GateOutcome>>> = Arc::default();
    let record = seen.clone();
    let started = Instant::now();
    let task = gate::spawn(
        manager.clone(),
        h.health.clone(),
        SwapApp::new(Router::new()),
        SwapApp::new(Router::new()),
        Box::new(move |outcome| {
            *record.lock().unwrap() = Some(outcome.clone());
            Ok(Finalized {
                data: Router::new(),
                admin: Router::new(),
                runtimes: RuntimeRegistry::default(),
            })
        }),
        tokio_util::sync::CancellationToken::new(),
    );
    wait_for("the gate to open at the deadline", || {
        h.health.gate() == GateState::Open
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the wait for the gate is bounded by the startup timeout, not by the breaker"
    );
    task.handle.await.unwrap();
    // 到点放弃：`failed` + `startup_timeout`，降级放行；finalize 拿到的就是这个结果。
    let outcome = seen.lock().unwrap().clone().expect("finalize ran");
    assert_eq!(
        outcome.degraded["omlx"].reason,
        Some(UpstreamReason::StartupTimeout)
    );
    assert_eq!(outcome.degraded["omlx"].state, UpstreamState::Failed);
    assert_eq!(
        h.health.view().phase,
        Phase::Degraded,
        "a managed engine that was given up on: degraded"
    );
    // 半拉起的引擎停掉了，放弃之后不再重试（冻结）。
    let launches = rig.fake.launches();
    if launches >= 1 {
        wait_for("the abandoned engine to be stopped", || {
            !pid_alive(rig.pid(0))
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.fake.launches(), launches, "no retry after giving up");
    assert_eq!(
        manager.start_upstream("omlx").await,
        Err(CommandError::RestartRequired)
    );
    manager.shutdown(true).await;
}

#[tokio::test]
async fn giving_up_on_an_owned_engine_that_stopped_answering_stops_it_and_reports_the_startup_timeout()
 {
    // 就绪过、之后活着却不答的自有引擎（`unavailable`，不是终局）：门控一直等它，到截止时间放弃（这里直接调放弃的那一步）。
    // 放弃就是停掉它：不能留一个没人管的引擎进程在后台，状态却说 `failed`。
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    let pid = rig.pid(0);
    rig.fake.force_status(503);
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert!(status.owned);
    assert!(
        h.manager.open_gate().is_none(),
        "unavailable is not settled: it can still come back"
    );
    h.manager.give_up_unsettled().await;
    let status = h.until_state(UpstreamState::Failed).await;
    assert_eq!(status.reason, Some(UpstreamReason::StartupTimeout));
    assert!(!status.owned, "the engine is gone");
    wait_for("the given-up engine to be stopped", || !pid_alive(pid)).await;
    assert_eq!(rig.records(), 0, "its ownership record went with it");
    let outcome = h.manager.open_gate().expect("settled now");
    assert_eq!(
        outcome.degraded["omlx"].reason,
        Some(UpstreamReason::StartupTimeout)
    );
    // 冻结的实例没有进程：不是引擎失效，没有退出码 5；也不再重试。
    assert!(
        tokio::time::timeout(Duration::from_millis(300), h.manager.engine_failed())
            .await
            .is_err()
    );
    assert_eq!(rig.fake.launches(), 1, "no retry");
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn the_gate_task_hands_the_degraded_outcome_to_finalize_and_the_health_says_degraded() {
    use axum::Router;
    use idoris_router::lifecycle::gate::{self, Finalized};
    use idoris_router::runtime::RuntimeRegistry;
    use idoris_router::swap::SwapApp;

    let rig = Rig::new(&Behavior::default());
    let missing = rig.work.path().join("no-such-omlx-cli");
    let h = Harness::start_with(
        &rig,
        default_backend(&rig),
        Scripted::new(),
        fast(),
        StopOnExit::Owned,
        missing,
    );
    let manager = Arc::new(h.manager);
    let seen: Arc<Mutex<Option<GateOutcome>>> = Arc::default();
    let record = seen.clone();
    let task = gate::spawn(
        manager.clone(),
        h.health.clone(),
        SwapApp::new(Router::new()),
        SwapApp::new(Router::new()),
        Box::new(move |outcome| {
            *record.lock().unwrap() = Some(outcome.clone());
            Ok(Finalized {
                data: Router::new(),
                admin: Router::new(),
                runtimes: RuntimeRegistry::default(),
            })
        }),
        tokio_util::sync::CancellationToken::new(),
    );
    wait_for("the gate to open", || h.health.gate() == GateState::Open).await;
    task.handle.await.unwrap();
    let outcome = seen.lock().unwrap().clone().expect("finalize ran");
    assert_eq!(
        outcome.degraded["omlx"].reason,
        Some(UpstreamReason::EngineNotFound)
    );
    let view = h.health.view();
    assert_eq!(view.phase, Phase::Degraded);
    assert!(!view.ready);
    assert_eq!(view.upstreams[0].state, UpstreamState::Absent);
    assert_eq!(
        view.upstreams[0].reason,
        Some(UpstreamReason::EngineNotFound)
    );
    manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 计划：卡 → 实例
// ---------------------------------------------------------------------------------------------

fn card(id: &str, endpoint: &str, managed: bool) -> idoris_contracts::ComponentCard {
    let extensions = if managed {
        "extensions:\n  _degradation: \"x\"\n  idoris_managed: { engine: omlx, mode: auto }\n"
    } else {
        ""
    };
    serde_yaml::from_str(&format!(
        r#"provider:
  id: {id}
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: on_demand, keepalive: {{ idle_ttl_s: 300 }}, admission: requires_eviction }}
{extensions}"#
    ))
    .unwrap()
}

fn plans_for(
    cards: &[idoris_contracts::ComponentCard],
) -> Vec<idoris_router::readiness::UpstreamPlan> {
    idoris_router::readiness::plan_upstreams(cards).unwrap()
}

#[test]
fn only_managed_cards_become_instances_with_the_port_from_their_endpoint() {
    let cards = [
        card("omlx", "http://127.0.0.1:19731", true),
        card("other", "http://127.0.0.1:19999", false),
    ];
    let backend: Arc<dyn EngineBackend> = Arc::new(OmlxBackend::from_env());
    let plans = plan_managed_with(&plans_for(&cards), |kind| {
        (kind == "omlx").then(|| backend.clone())
    })
    .unwrap();
    assert_eq!(
        plans.len(),
        1,
        "a card without `idoris_managed` is not managed"
    );
    assert_eq!(plans[0].id, "omlx");
    assert_eq!(plans[0].port, 19731);
    assert_eq!(plans[0].endpoint, "http://127.0.0.1:19731");
    assert!(
        plan_managed_with(
            &plans_for(&[card("other", "http://127.0.0.1:19999", false)]),
            |_| None
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn a_managed_card_that_cannot_be_managed_is_a_card_validation_error() {
    let backend: Arc<dyn EngineBackend> = Arc::new(OmlxBackend::from_env());
    let with_backend = |kind: &str| (kind == "omlx").then(|| backend.clone());
    for endpoint in [
        "http://localhost:19731",
        "http://127.0.0.1",
        "http://127.0.0.1:19731/v1",
        "https://127.0.0.1:19731",
    ] {
        let error = plan_managed_with(&plans_for(&[card("omlx", endpoint, true)]), with_backend)
            .unwrap_err();
        assert_eq!(
            error,
            PlanError::EndpointNotManageable {
                id: "omlx".to_owned()
            },
            "{endpoint}"
        );
        assert!(error.to_string().contains("http://127.0.0.1:<端口>"));
    }
    // 本版本没有这种引擎的管理器。
    let error = plan_managed_with(
        &plans_for(&[card("omlx", "http://127.0.0.1:19731", true)]),
        |_| None,
    )
    .unwrap_err();
    assert_eq!(
        error,
        PlanError::NoBackend {
            id: "omlx".to_owned(),
            engine_kind: "omlx".to_owned()
        }
    );
    // 两张托管卡用同一个端口；同一种引擎两个实例（会共用同一个私有 base）。
    let error = plan_managed_with(
        &plans_for(&[
            card("a", "http://127.0.0.1:19731", true),
            card("b", "http://127.0.0.1:19731", true),
        ]),
        with_backend,
    )
    .unwrap_err();
    assert!(matches!(error, PlanError::PortShared { .. }), "{error:?}");
    let error = plan_managed_with(
        &plans_for(&[
            card("a", "http://127.0.0.1:19731", true),
            card("b", "http://127.0.0.1:19732", true),
        ]),
        with_backend,
    )
    .unwrap_err();
    assert!(matches!(error, PlanError::SecondOfKind { .. }), "{error:?}");
    assert!(error.to_string().contains("每种引擎只托管一个实例"));
}

// ---------------------------------------------------------------------------------------------
// 门控任务：就绪之后才替换，替换之后才是 Open
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_gate_task_swaps_the_full_router_in_only_once_the_engines_are_ready() {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use idoris_router::lifecycle::gate::{self, Finalized};
    use idoris_router::runtime::RuntimeRegistry;
    use idoris_router::swap::SwapApp;
    use tower::ServiceExt;

    let rig = Rig::new(&Behavior {
        listen_delay_secs: 1.0,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let manager = Arc::new(h.manager);
    let data = SwapApp::new(Router::new().route("/x", get(|| async { "starting" })));
    let admin = SwapApp::new(Router::new().route("/x", get(|| async { "admin starting" })));
    let finalizations = Arc::new(AtomicUsize::new(0));
    let counted = finalizations.clone();
    let task = gate::spawn(
        manager.clone(),
        h.health.clone(),
        data.clone(),
        admin.clone(),
        Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(Finalized {
                data: Router::new().route("/x", get(|| async { "ready" })),
                admin: Router::new().route("/x", get(|| async { "admin ready" })),
                runtimes: RuntimeRegistry::default(),
            })
        }),
        tokio_util::sync::CancellationToken::new(),
    );
    let fetch = |app: &SwapApp| {
        let app = app.router();
        async move {
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/x")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let bytes = http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes();
            String::from_utf8(bytes.to_vec()).unwrap()
        }
    };
    // 引擎还在冷启动：starting 体，门控 Pending，没有创建完整的服务体。
    assert_eq!(fetch(&data).await, "starting");
    assert_eq!(h.health.gate(), GateState::Pending);
    assert_eq!(finalizations.load(Ordering::SeqCst), 0);
    // 就绪之后：完整的服务体替换上去，门控 Open，只创建一次。
    wait_for("the gate to open", || h.health.gate() == GateState::Open).await;
    assert_eq!(fetch(&data).await, "ready");
    assert_eq!(fetch(&admin).await, "admin ready");
    assert_eq!(finalizations.load(Ordering::SeqCst), 1);
    assert!(manager.gate_is_open());
    assert_eq!(h.health.view().phase, Phase::Ready);
    assert!(task.handle.await.is_ok());
    manager.shutdown(true).await;
}

#[tokio::test]
async fn a_failing_finalize_is_reported_as_a_startup_failure_and_nothing_is_swapped() {
    use axum::Router;
    use axum::routing::get;
    use idoris_router::lifecycle::gate;
    use idoris_router::swap::SwapApp;

    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let manager = Arc::new(h.manager);
    let data = SwapApp::new(Router::new().route("/x", get(|| async { "starting" })));
    let admin = SwapApp::new(Router::new());
    let task = gate::spawn(
        manager.clone(),
        h.health.clone(),
        data,
        admin,
        Box::new(|_| Err("runtime construction failed".to_owned())),
        tokio_util::sync::CancellationToken::new(),
    );
    let message = tokio::time::timeout(PATIENT, task.failed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message, "runtime construction failed");
    assert_eq!(h.health.gate(), GateState::Pending, "never opened");
    manager.shutdown(true).await;
}

#[tokio::test]
async fn a_panicking_finalize_is_reported_as_a_startup_failure_and_nothing_is_swapped() {
    use axum::Router;
    use axum::routing::get;
    use idoris_router::lifecycle::gate::{self, Finalized};
    use idoris_router::swap::SwapApp;

    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let manager = Arc::new(h.manager);
    let data = SwapApp::new(Router::new().route("/x", get(|| async { "starting" })));
    let task = gate::spawn(
        manager.clone(),
        h.health.clone(),
        data,
        SwapApp::new(Router::new()),
        Box::new(|_| -> Result<Finalized, String> {
            panic!("test: building the full router panicked")
        }),
        tokio_util::sync::CancellationToken::new(),
    );
    // 任务 panic 也要当成启动失败报出去，否则数据面会一直 503 而没人知道为什么。
    let message = tokio::time::timeout(PATIENT, task.failed)
        .await
        .unwrap()
        .unwrap();
    assert!(message.contains("panic"), "{message}");
    assert_eq!(h.health.gate(), GateState::Pending, "never opened");
    assert!(task.handle.await.unwrap_err().is_panic());
    manager.shutdown(true).await;
}

#[tokio::test]
async fn a_cancelled_gate_task_never_swaps() {
    use axum::Router;
    use idoris_router::lifecycle::gate;
    use idoris_router::swap::SwapApp;

    let rig = Rig::new(&Behavior {
        never_listen: true,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let manager = Arc::new(h.manager);
    let cancel = tokio_util::sync::CancellationToken::new();
    let swapped = Arc::new(AtomicUsize::new(0));
    let counted = swapped.clone();
    let task = gate::spawn(
        manager.clone(),
        h.health.clone(),
        SwapApp::new(Router::new()),
        SwapApp::new(Router::new()),
        Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Err("must not be called".to_owned())
        }),
        cancel.clone(),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    tokio::time::timeout(PATIENT, task.handle)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(swapped.load(Ordering::SeqCst), 0);
    manager.shutdown(true).await;
}
