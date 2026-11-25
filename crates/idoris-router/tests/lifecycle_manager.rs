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
// 引擎的端口是事先挑好、过一会儿才由假引擎去绑的：不能用 `bind(0)` 取号再放掉（见 `support/ports.rs`）。
#[path = "support/ports.rs"]
mod ports;

use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fake_omlx::{Behavior, FakeOmlx, OnSigterm, spawn_retrying};
use idoris_router::lifecycle::{
    CommandAck, CommandError, EngineBackend, EngineManager, GateOutcome, ManagedPlan,
    ManagerConfig, OmlxBackend, PlanError, PrepareFailure, Timings, plan_managed_with,
};
use idoris_router::readiness::{
    GateState, HealthState, ManagedFailure, Phase, UpstreamReason, UpstreamState, UpstreamStatus,
};
use idoris_router::register::{JitterSource, LogSink};
use idoris_upstream::engine::{
    CodesignPolicy, DiscoveryError, DiscoveryPlan, EngineExecutable, EngineInstance, EngineKey,
    EngineKind, EngineLaunch, EngineLaunchSpec, EngineProbe, EngineRecipe, InspectError,
    LaunchError, LaunchInput, LaunchPlan, ListenerPolicy, ListenerWatch, OwnedEngine, PidState,
    ProcessInspector, StopOnExit, SystemInspector, VersionError,
};
use idoris_upstream::omlx::key::{
    EndpointGuard, EndpointKey, KeyRefusal, authorize_omlx_request, endpoint_flights,
    forget_endpoint, governed_endpoint,
};
use ports::free_port;

const GIB: u64 = 1 << 30;
const PATIENT: Duration = Duration::from_secs(60);
const TEAM: &str = "TESTTEAM01";

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
    synthetic_listener_identity: bool,
    /// 对这些端口声称「被问到的 pid 正在监听」（假引擎真的在监听，但不依赖 `lsof` 来证明）。
    claims: Mutex<Vec<u16>>,
    /// `listeners_on(port)`：端口上的监听者。没编排的端口没有监听者。
    listeners: Mutex<BTreeMap<u16, Vec<u32>>>,
    /// `signing_team(pid)`：没编排的 pid 没有团队。
    teams: Mutex<BTreeMap<u32, String>>,
    team_checks: AtomicUsize,
    /// 端口 → 还剩几次「`listens_on` 为真」，用完之后一律为假：造「等就绪时监听它的是引擎，封印时端口已被别人占了」。
    claim_budget: Mutex<BTreeMap<u16, usize>>,
    /// `listens_on` 一律读不出来（`lsof` 不答）。
    unreadable: std::sync::atomic::AtomicBool,
    /// 读启动时间、进程组之前先睡这么久（慢 `ps`：认回的孤儿的身份闸因此很慢）。
    slow: Mutex<Duration>,
    /// 读启动时间一律读不出来（`ps` 不答）：认回的孤儿的身份闸因此「确认不了」，不发信号（造「停止没能确认」）。
    identity_unreadable: std::sync::atomic::AtomicBool,
    /// 第一次签名团队检查（外部监听者的属主校验）做完之后，把这个端口上的监听者换成这个 pid（没有团队：不合格）：造「校验期间端口换了主人」。
    flip_after_first_team_check: Mutex<Option<(u16, u32)>>,
    /// 读命令行（封印的第一步）之前先睡这么久：造「封印正在进行」。
    seal_delay: Mutex<Duration>,
    /// 守卫的原生监视的开关（[`Scripted::offer_a_native_watch_that_can_say_gone`]）：为真时监视说「进程不再握着那个套接字」，端口上的监听者名单与签名照旧。
    /// 没有编排（`None`）时没有原生监视，守卫每个请求问一次 `listeners_on`（退路）。
    watch_gone: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            real: SystemInspector::new(),
            synthetic_listener_identity: false,
            claims: Mutex::new(Vec::new()),
            listeners: Mutex::new(BTreeMap::new()),
            teams: Mutex::new(BTreeMap::new()),
            team_checks: AtomicUsize::new(0),
            claim_budget: Mutex::new(BTreeMap::new()),
            unreadable: std::sync::atomic::AtomicBool::new(false),
            slow: Mutex::new(Duration::ZERO),
            identity_unreadable: std::sync::atomic::AtomicBool::new(false),
            flip_after_first_team_check: Mutex::new(None),
            seal_delay: Mutex::new(Duration::ZERO),
            watch_gone: Mutex::new(None),
        })
    }

    fn set_identity_unreadable(&self, unreadable: bool) {
        self.identity_unreadable.store(unreadable, Ordering::SeqCst);
    }

    /// 从现在起建的守卫带一个原生监视，它的答案由返回的开关决定：`true` = 进程不再握着那个套接字（守卫的确认不过），`false` = 还握着。
    /// 端口上的监听者名单（`listeners_on`）与签名不受影响，所以周期性的属主校验仍然通过：造「校验通过、紧接着的授权里守卫的重新确认却不过」。
    /// 要在管理器登记引擎之前调用（守卫建的时候问一次）。
    fn offer_a_native_watch_that_can_say_gone(&self) -> Arc<std::sync::atomic::AtomicBool> {
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *self.watch_gone.lock().unwrap() = Some(gone.clone());
        gone
    }

    /// 从现在起每次读启动时间、进程组都先睡 `delay`。
    fn set_slow(&self, delay: Duration) {
        *self.slow.lock().unwrap() = delay;
    }

    async fn dawdle(&self) {
        let delay = *self.slow.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    /// 自有引擎的端口，但只有第一次问到 `listens_on` 时为真，之后一律为假。等就绪的循环在每一次带 key 的探测之前都重新问，所以这「第一次」
    /// 是等就绪的第一轮，不管引擎那时有没有开始监听：之后没有哪个带 key 的探测会发出去（H2 的用例靠它）。
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
        if self.synthetic_listener_identity {
            return Ok(format!("test process {pid}"));
        }
        self.dawdle().await;
        if self.identity_unreadable.load(Ordering::SeqCst) {
            return Err(InspectError::NoAnswer);
        }
        self.real.start_time(pid).await
    }
    async fn generation(&self, pid: u32) -> Result<String, InspectError> {
        self.real.generation(pid).await
    }
    async fn command(&self, pid: u32) -> Result<String, InspectError> {
        let delay = *self.seal_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.real.command(pid).await
    }
    async fn process_group(&self, pid: u32) -> Result<u32, InspectError> {
        self.dawdle().await;
        self.real.process_group(pid).await
    }
    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err(InspectError::NoAnswer);
        }
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
        if self.synthetic_listener_identity {
            return Ok(nix::unistd::geteuid().as_raw());
        }
        self.real.uid_of(pid).await
    }
    async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
        self.team_checks.fetch_add(1, Ordering::SeqCst);
        let team = self.teams.lock().unwrap().get(&pid).cloned();
        if let Some((port, other)) = self.flip_after_first_team_check.lock().unwrap().take() {
            self.listeners.lock().unwrap().insert(port, vec![other]);
        }
        Ok(team)
    }
    fn watch_listener(&self, _pid: u32, _dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
        let gone = self.watch_gone.lock().unwrap().clone()?;
        Some(Arc::new(SwitchableWatch(gone)))
    }
}

/// 答案由一个开关决定的原生监视（[`Scripted::offer_a_native_watch_that_can_say_gone`]）。
#[derive(Debug)]
struct SwitchableWatch(Arc<std::sync::atomic::AtomicBool>);

#[async_trait]
impl ListenerWatch for SwitchableWatch {
    async fn holds(&self) -> Result<bool, InspectError> {
        Ok(!self.0.load(Ordering::SeqCst))
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
        Self::with_port(behavior, free_port())
    }

    fn with_port(behavior: &Behavior, port: u16) -> Self {
        let work = tempfile::tempdir().unwrap();
        let fake = FakeOmlx::create(work.path(), behavior);
        Self {
            state: tempfile::tempdir().unwrap(),
            work,
            fake,
            port,
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

    /// 第 `launch` 次拉起的引擎的私有 key（它的环境变量 `OMLX_API_KEY`）。
    fn key(&self, launch: usize) -> String {
        self.fake.launch(launch)["api_key"]
            .as_str()
            .unwrap()
            .to_owned()
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
        drain_timeout: Duration::from_secs(5),
        command_timeout: Duration::from_secs(10),
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
    fn govern_private(&self, endpoint: &str, key: &EngineKey, guard: Arc<dyn EndpointGuard>) {
        self.inner.govern_private(endpoint, key, guard);
    }
    fn govern_external(&self, endpoint: &str, guard: Arc<dyn EndpointGuard>) {
        self.inner.govern_external(endpoint, guard);
    }
    fn block(&self, endpoint: &str) {
        self.inner.block(endpoint);
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
        backend: Arc<dyn EngineBackend>,
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
        backend: Arc<dyn EngineBackend>,
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
    // 停止之后：引擎没了，记录删了，私有 key 不再留在进程里；端点被封上（什么凭证都不发），不是撤销登记：撤销会让环境变量与
    // `settings.json` 的回退链重新适用。
    assert!(!pid_alive(rig.pid(0)));
    assert_eq!(rig.records(), 0);
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
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
    // 已经发布的失败在第一次被 poll 时就就绪，不用等下一次变化：停止流程（`stop::StopSources`，规范勘误十二）靠这一点让它排在
    // 同一轮到达的信号前面（`EngineControl::failed` 就是它）。
    assert_eq!(
        futures_util::FutureExt::now_or_never(h.manager.engine_failed())
            .map(|failure| failure.upstream_id),
        Some("omlx".to_owned())
    );
    // 规范勘误十二（1）（H4）：失败已经发布、同一轮里信号也就绪：停止原因是引擎失效（退出码 5），不是信号（0）。真的管理器，
    // 真的发布出来的失败；每次都是同一个结果（`select!` 随机挑的话，64 次里总有几次挑中信号）。
    for round in 0..64 {
        let verdict = idoris_router::stop::StopSources {
            engine_failed: idoris_router::stop::EngineControl::failed(&h.manager),
            registration: std::future::pending::<idoris_router::register::StopCause>(),
            gate_failed: std::future::pending::<String>(),
            signal: std::future::ready(()),
        }
        .first()
        .await;
        assert_eq!(
            verdict,
            idoris_router::stop::StopVerdict::Stop(
                idoris_router::register::StopCause::EngineFailed
            ),
            "round {round}"
        );
    }
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

    // stop：自有引擎停掉（整个进程组），状态 `stopped`，记录与私有 key 都没了。受理时状态就已经是 `stopped`（不可服务），进程
    // 还在退出的这几秒 `owned` 还是 true；进程没了才是 `owned: false`。端点被封上，不是撤销登记。
    h.manager.stop_upstream("omlx").await.unwrap();
    let stopped = h
        .until("the engine to be gone", |s| {
            s.state == UpstreamState::Stopped && !s.owned
        })
        .await;
    assert_eq!(stopped.reason, None);
    wait_for("the engine to die", || !pid_alive(first_pid)).await;
    assert_eq!(rig.records(), 0);
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
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
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked),
        "blocked at shutdown (not released: the fallback chain must not apply to a managed endpoint)"
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
// 总期限：停止必须在 iDoris 被 launcher SIGKILL 之前完成（M1）
// ---------------------------------------------------------------------------------------------

/// `shutdown_by` 带期限：对 SIGTERM 没有反应的引擎只得到期限里剩下的宽限，不是整整 `stop_grace`（这里 10 秒）。就绪的引擎，和启动
/// 途中被打断的引擎（管理器正在等它就绪，停止命令打断了等待）都一样：后者的引擎放回实例、交给那条带期限的 `Shutdown` 命令去停（H4），
/// 宽限同样只拿剩下的。引擎被 SIGKILL、确认消失之后所有权记录才删。
#[tokio::test]
async fn a_stop_against_a_deadline_gives_a_sigterm_ignoring_engine_only_the_time_left() {
    for starting in [false, true] {
        let case = if starting {
            "interrupted while starting"
        } else {
            "ready"
        };
        let rig = Rig::new(&Behavior {
            on_sigterm: OnSigterm::Ignore,
            never_listen: starting,
            ..Behavior::default()
        });
        let mut timings = fast();
        timings.stop_grace = Duration::from_secs(10);
        let h = Harness::start(
            &rig,
            default_backend(&rig),
            Scripted::claiming(rig.port),
            timings,
            StopOnExit::Owned,
        );
        if starting {
            // 启动记录写下来的时候 SIGTERM 的处理已经就位（假引擎先装处理器、后写记录）。
            wait_for("the engine to be launched", || rig.fake.launches() >= 1).await;
        } else {
            h.until_state(UpstreamState::Ready).await;
        }
        let pid = rig.pid(0);
        let begun = Instant::now();
        h.manager
            .shutdown_by(
                true,
                Some(tokio::time::Instant::now() + Duration::from_secs(3)),
            )
            .await;
        let took = begun.elapsed();
        assert!(!pid_alive(pid), "{case}: the engine is gone");
        assert_eq!(
            rig.fake.sigterm_times().len(),
            1,
            "{case}: asked once, ignored it"
        );
        assert!(
            took >= Duration::from_millis(2500),
            "{case}: SIGTERM was ignored, so only the SIGKILL at the deadline ended it: {took:?}"
        );
        assert!(
            took < Duration::from_secs(7),
            "{case}: the grace is what is left of the deadline, not the full 10 s: {took:?}"
        );
        assert_eq!(rig.records(), 0, "{case}: no record left behind");
    }
}

/// H3（管理器这一层）：认回的孤儿要先过身份闸（读启动时间、进程组），慢的 `ps` 让闸花掉 2.4 秒。期限是**绝对**的：闸花掉的时间从同一个期限里扣，
/// 宽限在真正发 SIGTERM 的那一刻按剩下的时间算（不到 2 秒就 SIGTERM 之后立刻 SIGKILL）。宽限若是在闸之前就换算成固定时长（3.5 秒），
/// 闸之后还要再等 3.5 秒、总共六秒多，超过这 3.5 秒的期限，对 SIGTERM 没有反应的引擎也等不到期限内的 SIGKILL。
#[tokio::test]
async fn a_slow_identity_gate_comes_out_of_the_same_deadline_for_an_adopted_engine_that_ignores_sigterm()
 {
    let rig = Rig::new(&Behavior {
        on_sigterm: OnSigterm::Ignore,
        ..Behavior::default()
    });
    let inspector = Scripted::claiming(rig.port);
    let pid = leave_an_orphan(&rig, &inspector).await;
    let mut timings = fast();
    timings.stop_grace = Duration::from_secs(10);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        timings,
        StopOnExit::Owned,
    );
    let status = h.until_state(UpstreamState::Ready).await;
    assert!(status.owned, "adopted: the engine is ours");
    inspector.set_slow(Duration::from_millis(1200));

    let begun = Instant::now();
    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_millis(3500)),
        )
        .await;
    let took = begun.elapsed();
    assert!(
        !pid_alive(pid),
        "the engine that ignores SIGTERM received its SIGKILL"
    );
    // （剩下的时间不到 2 秒：SIGTERM 之后立刻 SIGKILL，假引擎来不及把这次 SIGTERM 记下来，所以不数它收到过几次。）
    assert!(
        took >= Duration::from_millis(2300),
        "the identity gate really took 2.4 s: {took:?}"
    );
    assert!(
        took < Duration::from_millis(4600),
        "the gate and the stop share one 3.5 s deadline: {took:?}"
    );
    assert_eq!(rig.records(), 0);
}

/// H4：一个 `Shutdown` 打断了认回的孤儿的就绪等待（它活着、在听，只是回 503，管理器还在等它就绪）。引擎的去留由这条命令说了算：
/// `stop_engine:false`（`stop_on_exit:"never"` 的信号停止、以退出码 0 退出）就让它活着、所有权记录原样留给下一个 iDoris；
/// `stop_engine:true` 才停。此前等就绪被打断时一概先停掉半拉起的引擎，认回来的引擎就被无条件杀掉、记录也删了。
#[tokio::test]
async fn a_shutdown_that_interrupts_the_readiness_wait_of_an_adopted_engine_leaves_its_fate_to_that_command()
 {
    for stop in [false, true] {
        let case = if stop {
            "stop_engine true"
        } else {
            "stop_engine false"
        };
        let rig = Rig::new(&Behavior::default());
        let inspector = Scripted::claiming(rig.port);
        let pid = leave_an_orphan(&rig, &inspector).await;
        // 孤儿忙着（503）：下一个管理器认回它之后一直在等它就绪。
        rig.fake.force_status(503);
        let probed = rig.fake.requests().len();
        let h = Harness::start(
            &rig,
            default_backend(&rig),
            inspector,
            fast(),
            StopOnExit::Never,
        );
        wait_for("the adopted engine to be probed for readiness", || {
            rig.fake.requests().len() > probed
        })
        .await;
        assert_ne!(
            h.view().state,
            UpstreamState::Ready,
            "{case}: still waiting for it to be ready"
        );

        h.manager
            .shutdown_by(
                stop,
                Some(tokio::time::Instant::now() + Duration::from_secs(5)),
            )
            .await;
        if stop {
            wait_for("the engine to be stopped", || !pid_alive(pid)).await;
            assert_eq!(rig.records(), 0, "{case}: stopped and its record reclaimed");
        } else {
            assert!(
                pid_alive(pid),
                "{case}: handed over, not stopped: the original process survives"
            );
            assert_eq!(rig.records(), 1, "{case}: its ownership record is kept");
            assert_eq!(rig.fake.launches(), 1, "{case}: nothing was relaunched");
            assert!(
                rig.fake.sigterm_times().is_empty(),
                "{case}: it was never even signalled"
            );
        }
    }
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
    // 等就绪的时候监听这个端口的是引擎（`listens_on` 为真，只有一次），封印所有权记录时再看，端口已经是别人的了。等就绪的循环在每一次
    // 带 key 的探测之前都重新问（H3），所以「一次为真」要花在第一次带 key 的探测上：那一次探测是编排好的 `Ready`，不管真引擎启动
    // 要走几轮。
    let script =
        ProbeScript::new(|call| (call.keyed && call.index == 0).then_some(EngineProbe::Ready));
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(default_backend(&rig), script),
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
    // 探测是编排的（引擎可能连启动记录都还没来得及写，就被停掉了）：看所有权记录，不看进程号。
    wait_for("the ownership record to be removed", || rig.records() == 0).await;
    if rig.fake.launches() > 0 {
        wait_for("the engine to be stopped", || !pid_alive(rig.pid(0))).await;
    }
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked),
        "its private key was never registered: the endpoint stayed blocked"
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
async fn external_liveness_detects_a_second_listener_before_the_http_probe_is_due() {
    let rig = Rig::with_port(&Behavior::default(), 64600);
    let mut inspector = Scripted::new();
    Arc::get_mut(&mut inspector)
        .unwrap()
        .synthetic_listener_identity = true;
    let owner = std::process::id();
    inspector.external_listener(rig.port, owner, Some(TEAM));
    let script = ProbeScript::new(|_| Some(EngineProbe::Ready));
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(default_backend(&rig), script.clone()),
        inspector.clone(),
        Timings {
            liveness_interval: Duration::from_millis(50),
            probe_interval: Duration::from_secs(30),
            ..fast()
        },
        StopOnExit::Owned,
    );
    tokio::time::timeout(Duration::from_secs(3), h.until_state(UpstreamState::Ready))
        .await
        .unwrap();
    let second = std::os::unix::process::parent_id();
    inspector
        .listeners
        .lock()
        .unwrap()
        .insert(rig.port, vec![owner, second]);
    let failed = tokio::time::timeout(Duration::from_secs(3), h.until_state(UpstreamState::Failed))
        .await
        .unwrap();
    assert_eq!(failed.reason, Some(UpstreamReason::PortInUse));
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    assert_eq!(
        script.keyless.load(Ordering::SeqCst) + script.keyed.load(Ordering::SeqCst),
        1,
        "only the startup HTTP probe was due"
    );
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
        Some(EndpointKey::Blocked),
        "no credential of any kind goes to a listener that failed the check (the env key included)"
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

// ---------------------------------------------------------------------------------------------
// 启动决策里的探测不是引擎失败（IDORIS-CORE S6，Codex 预审 6）
// ---------------------------------------------------------------------------------------------
//
// 第一次启动之前对端口的探测，答案可能是「有东西在监听、但答不上来」（`Unavailable`：超时，或者连接被接受之后被重置——一个正在被
// 拆掉的监听者，别的进程刚用过这个端口），紧接着端口就空了（`ConnectionRefused`）。这时**还没有哪个我们拉起的进程出过问题**：
// 管理器回到决策的开头重来、拉起引擎；不能记一次失败（`failure_count`、熔断窗口、退避、`restarting`）。S5 的实现把它当成了引擎
// 失败（`Attempt::Failure`），于是首次启动偶尔就带着一次失败、退避一轮，机器忙的时候测试里的 `failure_count == 0` 断言因此偶发地红。
//
// 下面分两类钉住：**编排**的探测序列（确定的，每一个分支各一个测试）与**真的套接字**（一个占着端口的监听者：接受连接之后重置，或者
// 一直不答，然后走掉）。

/// 一次探测的编号：`keyed` = 这次探测带了 key；`index` = 同一类（带 key / 不带 key）里的第几次，从 0 起。
#[derive(Debug, Clone, Copy)]
struct ProbeCall {
    keyed: bool,
    index: usize,
}

/// 编排探测的回答：`answer` 返回 `Some` 就用它，返回 `None` 就去问真的端口。
struct ProbeScript {
    answer: Box<dyn Fn(ProbeCall) -> Option<EngineProbe> + Send + Sync>,
    keyless: AtomicUsize,
    keyed: AtomicUsize,
}

impl std::fmt::Debug for ProbeScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeScript").finish_non_exhaustive()
    }
}

impl ProbeScript {
    fn new(answer: impl Fn(ProbeCall) -> Option<EngineProbe> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            answer: Box::new(answer),
            keyless: AtomicUsize::new(0),
            keyed: AtomicUsize::new(0),
        })
    }

    fn next(&self, bearer: Option<&str>) -> Option<EngineProbe> {
        let keyed = bearer.is_some();
        let counter = if keyed { &self.keyed } else { &self.keyless };
        let index = counter.fetch_add(1, Ordering::SeqCst);
        (self.answer)(ProbeCall { keyed, index })
    }
}

/// 别的方法都是真配方的，`probe_ready` 先问编排。
#[derive(Debug)]
struct ScriptedRecipe {
    inner: Arc<dyn EngineRecipe>,
    script: Arc<ProbeScript>,
}

#[async_trait]
impl EngineRecipe for ScriptedRecipe {
    fn kind(&self) -> EngineKind {
        self.inner.kind()
    }
    fn discovery_plan(&self) -> DiscoveryPlan {
        self.inner.discovery_plan()
    }
    async fn discover(&self, explicit: Option<&Path>) -> Result<EngineExecutable, DiscoveryError> {
        self.inner.discover(explicit).await
    }
    async fn version(&self, executable: &Path, timeout: Duration) -> Result<String, VersionError> {
        self.inner.version(executable, timeout).await
    }
    fn launch_plan(&self, input: &LaunchInput<'_>) -> Result<LaunchPlan, LaunchError> {
        self.inner.launch_plan(input)
    }
    async fn probe_ready(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> EngineProbe {
        if let Some(answer) = self.script.next(bearer) {
            return answer;
        }
        self.inner
            .probe_ready(client, endpoint, bearer, timeout)
            .await
    }
}

/// [`TestBackend`]，但启动决策用的探测配方按 [`ProbeScript`] 回答。
#[derive(Debug)]
struct ScriptedProbeBackend {
    inner: Arc<TestBackend>,
    script: Arc<ProbeScript>,
}

impl ScriptedProbeBackend {
    fn new(inner: Arc<TestBackend>, script: Arc<ProbeScript>) -> Arc<Self> {
        Arc::new(Self { inner, script })
    }
}

impl EngineBackend for ScriptedProbeBackend {
    fn kind(&self) -> EngineKind {
        self.inner.kind()
    }
    fn probe_recipe(&self) -> Arc<dyn EngineRecipe> {
        Arc::new(ScriptedRecipe {
            inner: self.inner.probe_recipe(),
            script: self.script.clone(),
        })
    }
    /// 拉起之后等就绪用的探测（带自己的私有 key）走的是拉起用的配方：它同样按编排回答。
    fn launch_recipe(&self, state_dir: &Path) -> Result<Arc<dyn EngineRecipe>, PrepareFailure> {
        let inner = self.inner.launch_recipe(state_dir)?;
        Ok(Arc::new(ScriptedRecipe {
            inner,
            script: self.script.clone(),
        }))
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
    fn govern_private(&self, endpoint: &str, key: &EngineKey, guard: Arc<dyn EndpointGuard>) {
        self.inner.govern_private(endpoint, key, guard);
    }
    fn govern_external(&self, endpoint: &str, guard: Arc<dyn EndpointGuard>) {
        self.inner.govern_external(endpoint, guard);
    }
    fn block(&self, endpoint: &str) {
        self.inner.block(endpoint);
    }
    fn current_key(&self, endpoint: &str) -> Option<String> {
        self.inner.current_key(endpoint)
    }
}

impl Harness {
    /// 这个实例此刻的失败事实（Admin 读到的同一份）：`(failure_count, last_failure_at_ms)`。
    fn failures(&self) -> (u32, Option<u64>) {
        let view = self.manager.views().remove("omlx").unwrap();
        (view.failure_count, view.last_failure_at_ms)
    }

    /// 日志里（按顺序）包含 `needle` 的行数。
    fn logged_count(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }
}

/// 一次没有失败的首次启动：引擎只拉起了一次，状态从来没有进过 `restarting`，没有失败事实，日志里有「重新判断（不算失败）」。
fn assert_started_without_a_failure(rig: &Rig, h: &Harness, redecided: usize) {
    assert_eq!(rig.fake.launches(), 1, "{:?}", h.log.lock().unwrap());
    assert_eq!(
        h.failures(),
        (0, None),
        "not an engine failure: {:?}",
        h.log.lock().unwrap()
    );
    assert_eq!(
        h.logged_count("：restarting"),
        0,
        "never entered restarting: {:?}",
        h.log.lock().unwrap()
    );
    assert_eq!(
        h.logged_count("重新判断（不算失败）"),
        redecided,
        "{:?}",
        h.log.lock().unwrap()
    );
}

#[tokio::test]
async fn a_startup_probe_that_cannot_answer_and_then_finds_the_port_empty_is_not_an_engine_failure()
{
    let rig = Rig::new(&Behavior::default());
    // 第一次探测：有东西在监听但答不上来；等它的时候再探一次：端口已经空了；再重新判断：问真的端口，空的，拉起。
    let script = ProbeScript::new(|call| match (call.keyed, call.index) {
        (false, 0) => Some(EngineProbe::Unavailable),
        (false, 1) => Some(EngineProbe::ConnectionRefused),
        _ => None,
    });
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(default_backend(&rig), script),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let ready = h.until_state(UpstreamState::Ready).await;
    assert!(ready.owned);
    assert_started_without_a_failure(&rig, &h, 1);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_listener_that_is_gone_before_the_owner_check_is_decided_again_and_is_not_an_engine_failure()
 {
    let rig = Rig::new(&Behavior::default());
    // 第一次探测说「就绪」（有一个外部引擎）；校验端口属主的时候那里已经没有监听者了（检查器没有编排任何监听者）。
    let script =
        ProbeScript::new(|call| (!call.keyed && call.index == 0).then_some(EngineProbe::Ready));
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(default_backend(&rig), script),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    let ready = h.until_state(UpstreamState::Ready).await;
    assert!(ready.owned, "the engine was spawned, not adopted");
    assert_started_without_a_failure(&rig, &h, 1);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_engine_that_is_gone_when_asked_with_its_key_is_decided_again_and_is_not_an_engine_failure()
 {
    let rig = Rig::new(&Behavior::default());
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("sk-external".to_owned());
    // 第一次探测：401（有外部引擎，要 key）。属主校验通过（监听者是一个同用户、团队对的进程）。带 key 再问的时候它已经走了：连接被拒。
    let inspector = Scripted::claiming(rig.port);
    inspector.external_listener(rig.port, std::process::id(), Some(TEAM));
    let script = {
        let (inspector, port) = (inspector.clone(), rig.port);
        ProbeScript::new(move |call| match (call.keyed, call.index) {
            (false, 0) => Some(EngineProbe::AuthRejected(401)),
            (true, 0) => {
                inspector.forget_listener(port);
                Some(EngineProbe::ConnectionRefused)
            }
            _ => None,
        })
    };
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(backend, script),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    let ready = h.until_state(UpstreamState::Ready).await;
    assert!(ready.owned, "the engine was spawned, not adopted");
    assert_started_without_a_failure(&rig, &h, 1);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_port_that_keeps_flip_flopping_between_unanswered_and_empty_gives_up_at_the_startup_timeout_and_is_never_counted()
 {
    let rig = Rig::new(&Behavior::default());
    // 每一轮：答不上来，然后空了；永远如此。重新判断受启动超时约束，到点按「有东西占着端口答不上来」收尾；没有拉起任何东西，
    // 没有失败。
    let script = ProbeScript::new(|call| match (call.keyed, call.index % 2) {
        (false, 0) => Some(EngineProbe::Unavailable),
        (false, _) => Some(EngineProbe::ConnectionRefused),
        _ => None,
    });
    let mut timings = fast();
    timings.startup_timeout = Duration::from_millis(700);
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(default_backend(&rig), script),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    let gave_up = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(gave_up.reason, Some(UpstreamReason::StartupTimeout));
    assert!(!gave_up.owned);
    assert_eq!(rig.fake.launches(), 0, "nothing was spawned");
    assert_eq!(h.failures(), (0, None), "{:?}", h.log.lock().unwrap());
    assert_eq!(h.logged_count("：restarting"), 0);
    assert!(
        h.logged_count("重新判断（不算失败）") >= 2,
        "it did decide again, more than once: {:?}",
        h.log.lock().unwrap()
    );
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_real_crash_loop_still_trips_the_breaker_even_when_every_start_is_preceded_by_an_unanswered_probe()
 {
    let rig = Rig::new(&Behavior {
        exit_immediately: Some(5),
        ..Behavior::default()
    });
    // 每一轮启动决策：答不上来、空了、问真的端口（空的：拉起）。引擎一起来就退出：这才是引擎失败，每一轮恰好一次。
    let script = ProbeScript::new(|call| match (call.keyed, call.index % 3) {
        (false, 0) => Some(EngineProbe::Unavailable),
        (false, 1) => Some(EngineProbe::ConnectionRefused),
        _ => None,
    });
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(default_backend(&rig), script),
        Scripted::claiming(rig.port),
        observable_restarts(),
        StopOnExit::Owned,
    );
    let failed = h.until_state(UpstreamState::Failed).await;
    assert_eq!(failed.reason, Some(UpstreamReason::CrashLoop));
    // 熔断的那一刻之前：恰好两次 `restarting`（上限 2，第三次失败就是 `failed`）、三次拉起、三次「不算失败」的重新判断。
    let log = h.log.lock().unwrap().clone();
    let tripped = log
        .iter()
        .position(|line| line.contains("failed（crash_loop）"))
        .unwrap();
    let before = &log[..tripped];
    let count = |needle: &str| before.iter().filter(|line| line.contains(needle)).count();
    assert_eq!(count("：restarting"), 2, "{log:?}");
    assert_eq!(count("拉起引擎"), 3, "{log:?}");
    assert_eq!(count("重新判断（不算失败）"), 3, "{log:?}");
    assert_eq!(rig.fake.launches(), 3);
    h.manager.shutdown(true).await;
}

/// 占着端口的监听者怎么处置它接受的那一个连接。
#[derive(Debug, Clone, Copy)]
enum Squat {
    /// 等请求到了，不读就关：对端收到的是 RST（`ECONNRESET`），不是正常的 FIN。
    Reset,
    /// 一个字节也不答，拿着连接，直到对端放弃（探测超时）。
    Silent,
}

/// 在 `listener` 上只接受第一个连接，然后立刻关掉监听（端口空了，之后的连接被拒），那个连接按 `mode` 处置。
fn squat(listener: std::net::TcpListener, mode: Squat) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        drop(listener);
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        match mode {
            Squat::Reset => {
                // 等请求到了（peek 不消耗它），然后带着没读的数据关掉连接。
                let mut byte = [0_u8; 1];
                let _ = stream.peek(&mut byte);
                drop(stream);
            }
            Squat::Silent => {
                let mut buffer = [0_u8; 1024];
                let mut stream = stream;
                while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
            }
        }
    })
}

/// 端口上先有一个真的监听者（接受第一个探测的连接，然后重置它或者不答），之后它走了：管理器的探测先答不上来、再被拒，之后拉起
/// 引擎，**没有任何失败**。这是 S5 那次偶发的失败的真实形状（`SendRequest … ECONNRESET`）。
async fn a_squatter_that_leaves_does_not_cost_the_engine_a_failure(mode: Squat) {
    let mut rig = Rig::new(&Behavior::default());
    let listener = loop {
        let port = free_port();
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            rig.port = port;
            break listener;
        }
    };
    let squatter = squat(listener, mode);
    let mut timings = fast();
    // 不答的那一个靠探测超时放弃。
    timings.probe_timeout = Duration::from_millis(300);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Owned,
    );
    let ready = h.until_state(UpstreamState::Ready).await;
    assert!(ready.owned);
    squatter.join().unwrap();
    assert_started_without_a_failure(&rig, &h, 1);
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn a_listener_that_resets_the_first_probe_connection_and_leaves_does_not_cost_the_engine_a_failure()
 {
    a_squatter_that_leaves_does_not_cost_the_engine_a_failure(Squat::Reset).await;
}

#[tokio::test]
async fn a_listener_that_never_answers_the_first_probe_and_leaves_does_not_cost_the_engine_a_failure()
 {
    a_squatter_that_leaves_does_not_cost_the_engine_a_failure(Squat::Silent).await;
}

// ---------------------------------------------------------------------------------------------
// 自有引擎的私有 key 只发给确认过的监听者，每一次（IDORIS-CORE S6，Codex 审计 H3）
// ---------------------------------------------------------------------------------------------

/// `Ok(true)` 的次数不限，直到 [`Scripted::release`]：造「引擎进程活着、端口却不再是它的」。
impl Scripted {
    /// 从现在起，`listens_on` 对这个端口回 `Ok(false)`（进程还活着，监听这个端口的不是它了）。
    fn lose(&self, port: u16) {
        self.claims
            .lock()
            .unwrap()
            .retain(|claimed| *claimed != port);
        self.claim_budget.lock().unwrap().insert(port, 0);
    }

    /// 又是它在监听了。
    fn regain(&self, port: u16) {
        self.claim_budget.lock().unwrap().remove(&port);
        self.claims.lock().unwrap().push(port);
    }
}

/// 在这个引擎的记录里，带着一个 key 的请求有几个（`ok` = 引擎自己的私有 key，`bad` = 别的 key；没带 key 的是 `none`）。
fn keyed_requests(rig: &Rig) -> usize {
    rig.fake
        .requests()
        .iter()
        .filter(|(_, auth)| auth != "none")
        .count()
}

#[tokio::test]
async fn an_engine_that_listens_late_gets_no_key_once_the_port_stopped_being_confirmed_during_startup()
 {
    // 引擎 0.8 秒之后才开始监听。第一次问「监听这个端口的是不是这个进程」是真的（那时引擎还没起来，带 key 的探测被拒，端口上没有任何东西
    // 收到它），之后一直是假的。S5 的实现把第一次的「是」缓存下来，之后每一次探测都带着 key 出去：引擎一开始监听就收到了私有 key。
    // 现在每一次带 key 的探测之前都重新问，所以之后没有一个带 key 的请求到过这个端口。
    let rig = Rig::new(&Behavior {
        listen_delay_secs: 0.8,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming_once(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    // 起点是引擎的启动记录（python 一起来就写）：它在这之后 0.8 秒才开始监听。机器忙时进程起得慢，所以不按固定的时间算，
    // 也不给启动一个紧的上限（`fast()` 是 30 秒）：引擎开始监听之后再多等两秒（探测每 20 到 100 毫秒一次），这段时间里任何
    // 一个带 key 的探测都会被记下来。
    wait_for("the engine process to start", || rig.fake.launches() >= 1).await;
    tokio::time::sleep(Duration::from_millis(2800)).await;
    assert_eq!(
        keyed_requests(&rig),
        0,
        "a key went to a port nobody had confirmed: {:?}",
        rig.fake.requests()
    );
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_engine_that_stops_being_the_listener_loses_its_key_at_once_and_gets_it_back_when_it_is_again()
 {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    let key = rig.key(0);
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Private(key.clone()))
    );
    wait_for("keyed probes to be flowing", || keyed_requests(&rig) >= 2).await;

    // 进程活着，端口却不再是它的（放掉了，或别的进程绑上了）：先收回私有 key（端点封上）、发布 `unavailable` 加 `port_in_use`
    // （数据面 503），之后没有一个带 key 的请求发往那个端口。
    inspector.lose(rig.port);
    let lost = h
        .until("unavailable, port_in_use", |s| {
            s.state == UpstreamState::Unavailable && s.reason == Some(UpstreamReason::PortInUse)
        })
        .await;
    assert!(lost.owned, "the process is still ours and alive");
    assert!(pid_alive(rig.pid(0)));
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked),
        "no credential of any kind goes to a port the engine no longer owns"
    );
    let keyed = keyed_requests(&rig);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        keyed_requests(&rig),
        keyed,
        "a keyed probe went out after the engine lost its port"
    );

    // 又是它在监听了：私有 key 登记回去，探测恢复，回到 `ready`；同一个进程，没有重启。
    inspector.regain(rig.port);
    h.until_state(UpstreamState::Ready).await;
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Private(key))
    );
    assert_eq!(rig.fake.launches(), 1, "the same process, never restarted");
    wait_for("keyed probes to flow again", || {
        keyed_requests(&rig) > keyed
    })
    .await;
    h.manager.shutdown(true).await;
}

#[tokio::test]
async fn an_external_engine_that_cannot_be_used_leaves_its_endpoint_blocked_after_the_terminal_failure()
 {
    // 外部引擎要 key。接管它的时候校验通过的监听者已经被登记成「外部」（为了带 key 探测它）；key 没有（`auth_required`）或被拒
    // （`auth_failed`）是终局，之后没有监督再去重新校验端口，所以那个口径不能留着：端点回到封上，端口上之后出现的任何东西都拿不到
    // `settings.json` 的 key（Codex 审计 H1：终局的失败之后保持默认拒绝）。
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), Some("ext-secret"));
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    for (key, reason) in [
        (None, UpstreamReason::AuthRequired),
        (Some("wrong"), UpstreamReason::AuthFailed),
    ] {
        let backend = default_backend(&rig);
        *backend.external_key.lock().unwrap() = key.map(str::to_owned);
        let h = Harness::start(&rig, backend, inspector.clone(), fast(), StopOnExit::Owned);
        let status = h.until_state(UpstreamState::Unavailable).await;
        assert_eq!(status.reason, Some(reason));
        assert_eq!(
            governed_endpoint(&rig.endpoint()),
            Some(EndpointKey::Blocked),
            "{reason:?}: after a terminal failure the endpoint stays closed to every credential"
        );
        h.manager.shutdown(true).await;
    }
}

#[tokio::test]
async fn an_engine_whose_listener_cannot_be_read_is_not_a_confirmed_listener_and_gets_no_key() {
    // 检查器读不出来（`lsof` 不答）不是「确认了」：重读几次（`unverifiable_retries`）之后仍读不出来就当作没有确认，先收回私有 key。
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    wait_for("keyed probes to be flowing", || keyed_requests(&rig) >= 2).await;
    inspector
        .unreadable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    h.until("unavailable, port_in_use", |s| {
        s.state == UpstreamState::Unavailable && s.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    let keyed = keyed_requests(&rig);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(keyed_requests(&rig), keyed);
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 终审（17ac795a）H2、M3
// ---------------------------------------------------------------------------------------------

/// H2：接管外部引擎时，校验通过的监听者已经被登记成「外部」（为了带 key 探测它）；带 key 的探测一次次得不到答案（监听者「没了」），整个启动决策
/// 一遍遍重来，直到启动超时才按终态收尾。重判耗尽直接发布终态，不经 `Attempt::Terminal`：之前这条出口留着已验证的外部登记，没有监督去重新校验
/// 端口，端口上之后出现的东西（只要校验恰好通过）仍拿得到 `settings.json` 的 key。现在每一条没有落到「持续监督」上的出口都封上端点。
#[tokio::test]
async fn h2_an_external_takeover_decided_again_until_the_startup_timeout_leaves_the_endpoint_blocked()
 {
    let rig = Rig::new(&Behavior::default());
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("sk-external".to_owned());
    let inspector = Scripted::claiming(rig.port);
    inspector.external_listener(rig.port, std::process::id(), Some(TEAM));
    // 不带 key：401（有外部引擎，要 key）；带 key：连接被拒（监听者走了）。每一轮都是这样。
    let script = ProbeScript::new(|call| {
        Some(if call.keyed {
            EngineProbe::ConnectionRefused
        } else {
            EngineProbe::AuthRejected(401)
        })
    });
    let mut timings = fast();
    timings.startup_timeout = Duration::from_millis(700);
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(backend, script),
        inspector,
        timings,
        StopOnExit::Owned,
    );
    let gave_up = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(gave_up.reason, Some(UpstreamReason::StartupTimeout));
    assert_eq!(rig.fake.launches(), 0, "nothing was spawned");
    assert_eq!(h.failures(), (0, None), "{:?}", h.log.lock().unwrap());
    assert!(
        h.logged_count("重新判断（不算失败）") >= 2,
        "it did decide again, more than once: {:?}",
        h.log.lock().unwrap()
    );
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked),
        "the verified external registration was left behind when the decision gave up"
    );
    h.manager.shutdown(true).await;
}

/// M3：带 key 的探测没有结论（有东西在监听、一直答不上来），而监听者**还是校验通过的那个进程**：这是它的毛病，不是「前提变了」。按一次失败算：
/// `restarting`、`failure_count`、退避、熔断照旧（S5 的语义）。之前一律当「前提变了」重判，耗尽启动超时之后按 `unavailable` + `startup_timeout`
/// 收尾，跳过失败计数、指数退避与熔断的半开重试。
#[tokio::test]
async fn m3_a_stable_listener_that_keeps_failing_the_keyed_probe_is_counted_backed_off_and_trips_the_breaker()
 {
    let rig = Rig::new(&Behavior::default());
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("sk-external".to_owned());
    let inspector = Scripted::claiming(rig.port);
    inspector.external_listener(rig.port, std::process::id(), Some(TEAM));
    let script = ProbeScript::new(|call| {
        Some(if call.keyed {
            EngineProbe::Unavailable
        } else {
            EngineProbe::AuthRejected(401)
        })
    });
    let mut timings = observable_restarts();
    // 短的启动超时：之前的实现在这里收尾成 `unavailable`，用例立刻看到，不必等 60 秒。
    timings.startup_timeout = Duration::from_millis(700);
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(backend, script),
        inspector,
        timings,
        StopOnExit::Owned,
    );
    let first = h
        .until("a decision", |status| {
            matches!(
                status.state,
                UpstreamState::Restarting | UpstreamState::Unavailable | UpstreamState::Failed
            )
        })
        .await;
    assert_eq!(
        first.state,
        UpstreamState::Restarting,
        "a listener that is still the verified one and keeps failing is an engine failure: {:?}",
        h.log.lock().unwrap()
    );
    assert!(h.failures().0 >= 1, "{:?}", h.log.lock().unwrap());
    let tripped = h.until_state(UpstreamState::Failed).await;
    assert_eq!(tripped.reason, Some(UpstreamReason::CrashLoop));
    assert!(
        h.failures().0 >= 3,
        "every failure counted: {:?}",
        h.failures()
    );
    assert_eq!(
        rig.fake.launches(),
        0,
        "an external engine is never restarted"
    );
    assert_eq!(
        governed_endpoint(&rig.endpoint()),
        Some(EndpointKey::Blocked)
    );
    h.manager.shutdown(true).await;
}

/// M3 的另一边：监听者换成了**另一个**通过校验的进程（身份变了）：不是第一个进程的毛病，重新判断，不算失败；之后接管新的那一个。
#[tokio::test]
async fn m3_a_listener_that_changed_identity_is_decided_again_and_is_not_an_engine_failure() {
    let rig = Rig::new(&Behavior::default());
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("sk-external".to_owned());
    let first = std::process::id();
    let second = std::os::unix::process::parent_id();
    assert_ne!(first, second);
    let inspector = Scripted::claiming(rig.port);
    inspector.external_listener(rig.port, first, Some(TEAM));
    let script = {
        let (inspector, port) = (inspector.clone(), rig.port);
        ProbeScript::new(move |call| match (call.keyed, call.index) {
            (false, 0) => Some(EngineProbe::AuthRejected(401)),
            // 带 key 再问的时候，端口上换成了另一个（通过校验的）进程，它也答不上来。
            (true, 0) => {
                inspector.external_listener(port, second, Some(TEAM));
                Some(EngineProbe::Unavailable)
            }
            // 新一轮决策：匿名的探测就答 `ready`，接管它。
            (false, _) => Some(EngineProbe::Ready),
            _ => None,
        })
    };
    let h = Harness::start(
        &rig,
        ScriptedProbeBackend::new(backend, script),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    let ready = h.until_state(UpstreamState::Ready).await;
    assert!(
        !ready.owned,
        "the engine is external: it was adopted, not spawned"
    );
    assert_eq!(rig.fake.launches(), 0);
    assert_eq!(h.failures(), (0, None), "{:?}", h.log.lock().unwrap());
    assert_eq!(h.logged_count("：restarting"), 0);
    assert!(h.logged_count("重新判断（不算失败）") >= 1);
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 预审 L3 的 M5：排空成功之后、进程确认消失之前，端点不再放新的操作进来
// ---------------------------------------------------------------------------------------------

/// 一个早先抓到这张托管卡的请求（公共模型列表逐卡走，前面的慢卡让它晚到）在排空**之后**才走到授权处。之前排空读到「在途数为零」就去停进程，
/// 两者之间没有封：这个请求登记新的在途、发往还在宽限期里监听的引擎，随后被 SIGTERM 打断。现在排空成功的那一刻在途登记同时封上，宽限期里
/// 到达的请求在授权处被拒绝（`Draining`），引擎在 SIGTERM 之后没有收到任何请求；进程确认消失之后端点封上（`Blocked`），`restart` 的新引擎
/// 登记它的私有 key 时封上解开，新引擎照常被服务。
///
/// 假引擎对 SIGTERM 记一行、然后照常服务（宽限期里它还在监听，正是那个窗口），`stop_grace` 之后才被 SIGKILL。
#[tokio::test]
async fn after_the_drain_has_succeeded_a_request_that_arrives_late_is_refused_for_the_whole_stop_grace()
 {
    for restart in [false, true] {
        let case = if restart { "restart" } else { "stop" };
        let rig = Rig::new(&Behavior {
            on_sigterm: OnSigterm::Ignore,
            ..Behavior::default()
        });
        let mut timings = fast();
        timings.stop_grace = Duration::from_secs(3);
        let h = Harness::start(
            &rig,
            default_backend(&rig),
            Scripted::claiming(rig.port),
            timings,
            StopOnExit::Owned,
        );
        h.until_state(UpstreamState::Ready).await;
        let (url, old, old_key) = (rig.endpoint(), rig.pid(0), rig.key(0));
        // 起点：端点照常授权，在途的操作数是它持有的那一个。
        let held = authorize_omlx_request(&url, None).await.unwrap();
        assert_eq!(held.key(), Some(old_key.as_str()), "{case}");
        assert_eq!(endpoint_flights(&url), 1, "{case}");
        drop(held);

        // 命令到来时没有任何在途的操作：排空立刻成功，随后 SIGTERM 发给引擎，它无视、继续监听（宽限期）。
        if restart {
            h.manager.restart_upstream("omlx").await.unwrap();
        } else {
            h.manager.stop_upstream("omlx").await.unwrap();
        }
        wait_for("the engine to receive its SIGTERM", || {
            !rig.fake.sigterm_times().is_empty()
        })
        .await;
        assert!(
            pid_alive(old),
            "{case}: the engine is alive in its grace (it ignores SIGTERM)"
        );
        let requests = rig.fake.requests().len();

        // 宽限期里一个晚到的请求：被拒绝，什么也没登记，引擎什么也没收到。
        let refused = authorize_omlx_request(&url, None).await.unwrap_err();
        assert_eq!(refused, KeyRefusal::Draining, "{case}: {refused}");
        assert_eq!(endpoint_flights(&url), 0, "{case}: nothing registered");
        assert_eq!(
            rig.fake.requests().len(),
            requests,
            "{case}: the engine heard nothing after its SIGTERM"
        );
        // 登记的口径仍是旧引擎的私有 key（进程还活着，G2）：封上的只是「新的操作」。
        assert_eq!(
            governed_endpoint(&url),
            Some(EndpointKey::Private(old_key.clone())),
            "{case}"
        );

        // 宽限到点、SIGKILL、确认消失：端点封上；`restart` 的新引擎登记之后封上解开，被服务。
        wait_for("the old engine to be gone", || !pid_alive(old)).await;
        if restart {
            h.until("the replacement", |status| {
                status.state == UpstreamState::Ready && status.owned
            })
            .await;
            assert_eq!(rig.fake.launches(), 2, "{case}");
            let served = authorize_omlx_request(&url, None).await.unwrap();
            assert_eq!(served.key(), Some(rig.key(1).as_str()), "{case}");
            assert_ne!(rig.key(1), old_key, "{case}: a new private key");
        } else {
            h.until("the stop to be complete", |status| {
                status.state == UpstreamState::Stopped && !status.owned
            })
            .await;
            assert_eq!(
                governed_endpoint(&url),
                Some(EndpointKey::Blocked),
                "{case}"
            );
            assert_eq!(
                authorize_omlx_request(&url, None).await.unwrap_err(),
                KeyRefusal::Blocked,
                "{case}"
            );
        }
        h.manager.shutdown(true).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Codex 预审（S6c）：H1、H3、H5、M2、M3、M4
// ---------------------------------------------------------------------------------------------

/// 秒数很长的排空（`fast()` 的 5 秒放宽到 30 秒），停止宽限 3 秒。
fn long_drain() -> Timings {
    let mut timings = fast();
    timings.drain_timeout = Duration::from_secs(30);
    timings.stop_grace = Duration::from_secs(3);
    timings
}

/// H3：一个 `restart` 正在排空一个在途的请求（要等 30 秒）时 iDoris 停止。此前停止命令排在退役后面，管理器等 11 秒（这里是期限加一点余量）
/// 之后中止实例任务；`stop_on_exit:"never"` 拉起的引擎在句柄丢弃时不杀，成了没人管的孤儿。现在停止指令一到，排空立刻放手，引擎按指令在
/// 期限里被停掉并回收记录。
#[tokio::test]
async fn a_shutdown_behind_a_restart_drain_interrupts_it_and_stops_the_never_engine_within_the_deadline()
 {
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        long_drain(),
        StopOnExit::Never,
    );
    h.until_state(UpstreamState::Ready).await;
    let pid = rig.pid(0);
    // 一个在途的请求：排空要等它（它一直不放手）。
    let held = authorize_omlx_request(&rig.endpoint(), None).await.unwrap();
    h.manager.restart_upstream("omlx").await.unwrap();
    h.until_state(UpstreamState::Restarting).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        pid_alive(pid),
        "still draining: the old engine is untouched"
    );

    let begun = Instant::now();
    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_secs(4)),
        )
        .await;
    let took = begun.elapsed();
    assert!(
        !pid_alive(pid),
        "the engine was stopped, not orphaned (stop_on_exit never survives a dropped handle)"
    );
    assert_eq!(rig.records(), 0, "and its ownership record reclaimed");
    assert!(
        took < Duration::from_secs(6),
        "the drain did not hold the shutdown for its 30 s: {took:?}"
    );
    assert_eq!(rig.fake.launches(), 1, "no replacement was started");
    drop(held);
}

/// M2：`restart` 正在排空时，队里依次排着 `stop` 与另一个 `restart`。第二个 `restart` 是对「`stop` 之后」的请求：此前它被并进第一次
/// 操作的结果，回 `Ok` 而状态是 `stopped`；现在按当时的状态回答（没有自有引擎：`NotOwned`）。
#[tokio::test]
async fn a_restart_queued_behind_a_stop_is_not_folded_into_the_restart_before_it() {
    let rig = Rig::new(&Behavior::default());
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        long_drain(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    let held = authorize_omlx_request(&rig.endpoint(), None).await.unwrap();
    h.manager.restart_upstream("omlx").await.unwrap();
    h.until_state(UpstreamState::Restarting).await;

    let stop = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.manager.stop_upstream("omlx").await
    };
    let second_restart = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.manager.restart_upstream("omlx").await
    };
    let release = async {
        tokio::time::sleep(Duration::from_millis(700)).await;
        drop(held);
    };
    let (stopped, restarted, ()) = tokio::join!(stop, second_restart, release);
    assert!(
        matches!(stopped, Ok(CommandAck::Snapshot(_))),
        "the stop was accepted: {stopped:?}"
    );
    assert_eq!(
        restarted,
        Err(CommandError::NotOwned),
        "the stop came first: there is no engine to restart"
    );
    let status = h
        .until("stopped", |s| s.state == UpstreamState::Stopped && !s.owned)
        .await;
    assert_eq!(status.state, UpstreamState::Stopped);
    h.manager.shutdown(true).await;
}

/// M3：`stop` 到的时候一个自有的子进程还在启动（拉起了、不听）。此前它还在 `wait_owned_ready` 的局部变量里，`stop` 回 `NotOwned`，
/// 子进程接着启动；现在 `stop` 看得见它：打断等待，完整的退役，状态 `stopped`，进程没了、记录没了。
#[tokio::test]
async fn a_stop_during_startup_sees_and_stops_the_child_that_is_still_starting() {
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
    wait_for("the engine to be launched", || rig.fake.launches() >= 1).await;
    let pid = rig.pid(0);
    assert_eq!(h.view().state, UpstreamState::Starting);
    h.manager
        .stop_upstream("omlx")
        .await
        .expect("a child that is starting is ours to stop");
    h.until("stopped", |s| s.state == UpstreamState::Stopped && !s.owned)
        .await;
    wait_for("the child to die", || !pid_alive(pid)).await;
    assert_eq!(rig.records(), 0);
    assert_eq!(rig.fake.launches(), 1, "and it was not started again");
    h.manager.shutdown(true).await;
}

/// M3：`restart` 到的时候启动只是在等一个外部监听者答话（没有自有引擎）。此前「正在启动」的捷径回 `Ok`，什么也没发生；现在没有自有的
/// 操作可并入，回 `NotOwned`。
#[tokio::test]
async fn a_restart_while_startup_only_waits_on_an_external_listener_is_not_owned() {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), None);
    let rig = rig_for(&external);
    // 外部引擎在听，但一直回 503：启动在等它答话。
    external.fake.force_status(503);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    wait_for("the first probe of the external engine", || {
        !external.fake.requests().is_empty()
    })
    .await;
    assert_eq!(h.view().state, UpstreamState::Starting);
    assert_eq!(
        h.manager.restart_upstream("omlx").await,
        Err(CommandError::NotOwned)
    );
    assert_eq!(
        h.manager.stop_upstream("omlx").await,
        Err(CommandError::NotOwned)
    );
    h.manager.shutdown(true).await;
}

/// M4：认回的引擎，`restart` 的退役没能确认它消失（身份读不出来：不发信号，进程还活着）。此前之后的启动决策又认回同一个健康的、已封印的
/// 进程，同一个身份，直接 `ready`：没有换。现在记着这个没能确认的进程：认回它时先把它停掉，之后拉起一个新的。
#[tokio::test]
async fn an_unconfirmed_retirement_never_ends_in_ready_on_the_original_process() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let old = leave_an_orphan(&rig, &inspector).await;
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    let adopted = h.until_state(UpstreamState::Ready).await;
    assert!(adopted.owned);

    inspector.set_identity_unreadable(true);
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the stop to be reported unconfirmed", || {
        h.logged("停止引擎失败")
    })
    .await;
    assert!(pid_alive(old), "nothing was signalled: it is still alive");
    inspector.set_identity_unreadable(false);

    let status = h
        .until("a replacement", |s| {
            s.state == UpstreamState::Ready && s.owned
        })
        .await;
    assert_eq!(status.reason, None);
    wait_for("the unconfirmed process to be stopped", || !pid_alive(old)).await;
    assert_eq!(
        rig.fake.launches(),
        2,
        "a new engine was launched: the original was not put back into service"
    );
    assert!(pid_alive(rig.pid(1)));
    h.manager.shutdown(true).await;
}

/// 一个 `status()` 在第一次（Supervisor 启动采样）睡很久的适配器：Supervisor 的 actor 在采样结束前不收任何命令，关门的 RPC 因此要等。
struct SlowFirstStatus {
    mock: idoris_backend::MockAdapter,
    /// 到目前为止 `status()` 被问了几次（测试据此知道复位的采样什么时候开始）。
    calls: Arc<AtomicUsize>,
    /// 第几次（从 0 起）`status()` 睡 `first` 那么久。
    slow_call: usize,
    first: Duration,
}

#[async_trait]
impl idoris_backend::RuntimeAdapter for SlowFirstStatus {
    fn load_fence_path(&self) -> Result<PathBuf, idoris_backend::BackendError> {
        self.mock.load_fence_path()
    }
    async fn list(&self) -> Result<Vec<idoris_backend::ModelInfo>, idoris_backend::BackendError> {
        self.mock.list().await
    }
    async fn load(
        &self,
        id: &str,
        policy: Option<&idoris_contracts::LoadPolicy>,
    ) -> Result<(), idoris_backend::BackendError> {
        self.mock.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), idoris_backend::BackendError> {
        self.mock.unload(id).await
    }
    async fn status(&self) -> Result<idoris_backend::BackendStatus, idoris_backend::BackendError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.slow_call {
            tokio::time::sleep(self.first).await;
        }
        self.mock.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, idoris_backend::BackendError> {
        self.mock.probe_ready(id).await
    }
    async fn chat(
        &self,
        req: idoris_backend::ChatRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<idoris_backend::ChatResponse, idoris_backend::BackendError> {
        self.mock.chat(req, cancel).await
    }
}

/// H1：门控放行之后自有引擎的进程退出，而 Supervisor 的邮箱此刻正被占着（启动采样 4 秒）。此前先收拾（关 Supervisor 的门要等邮箱）、后报
/// 失效：引擎失效（退出码 5）晚了 4 秒才发布，这段时间里到的 SIGTERM 能把退出码盖成 0。现在先报、后收拾。
#[tokio::test]
async fn an_engine_failure_is_published_before_the_slow_cleanup_not_after_it() {
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
    let registry =
        RuntimeRegistry::spawn_with_factory(&[card("omlx", &rig.endpoint(), true)], |_| {
            Ok(Arc::new(SlowFirstStatus {
                mock: idoris_backend::MockAdapter::new(Vec::new()),
                calls: Arc::new(AtomicUsize::new(0)),
                slow_call: 0,
                first: Duration::from_secs(4),
            }))
        })
        .unwrap();
    h.manager.attach_runtimes(registry);

    let _ = Command::new("kill")
        .args(["-KILL", &rig.pid(0).to_string()])
        .status();
    let begun = Instant::now();
    let failure = tokio::time::timeout(Duration::from_millis(2500), h.manager.engine_failed())
        .await
        .expect("the failure is published without waiting for the Supervisor's mailbox");
    assert_eq!(failure.upstream_id, "omlx");
    assert!(begun.elapsed() < Duration::from_millis(2500));
    h.manager.shutdown(true).await;
}

/// H5：外部引擎换了进程（A 到 B），复位 B 的账本失败（B 的采样 503）。此前 `self.external`（守卫）在复位之前就换成了 B 的，身份还是 A：
/// 之后 A 回来，同一个身份的捷径把 B 的守卫登记给 A、直接开门，账本从没对着 A 重建，状态卡在 `startup_latched`。现在复位失败清掉身份、旧
/// 守卫原样留着：A 回来重走一遍换代，复位成功，回到 `ready`。
#[tokio::test]
async fn an_external_reset_that_failed_does_not_let_the_old_process_back_on_the_stale_ledger() {
    use idoris_router::runtime::RuntimeRegistry;

    let work = tempfile::tempdir().unwrap();
    let a = External::start(work.path(), None);
    let other = tempfile::tempdir().unwrap();
    let b = External::start(other.path(), None);
    let rig = rig_for(&a);
    let inspector = Scripted::new();
    inspector.external_listener(a.port, a.pid(), Some(TEAM));
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    assert!(h.manager.open_gate().is_some());
    h.manager
        .attach_runtimes(RuntimeRegistry::spawn(&[card("omlx", &rig.endpoint(), true)]).unwrap());
    wait_for("the ledger to be sampled", || !a.fake.requests().is_empty()).await;
    h.until_state(UpstreamState::Ready).await;

    // A 变成 B，而 B 的采样失败（它答 503）。
    a.fake.force_status(503);
    inspector.external_listener(a.port, b.pid(), Some(TEAM));
    h.until("the reset to fail", |s| {
        s.reason == Some(UpstreamReason::StartupLatched)
    })
    .await;

    // A 回来，并且好了。
    a.fake.answer_normally();
    inspector.external_listener(a.port, a.pid(), Some(TEAM));
    let status = h
        .until("ready again on a rebuilt ledger", |s| {
            s.state == UpstreamState::Ready
        })
        .await;
    assert_eq!(status.reason, None);
    // 并且不是一时的：之后的几个周期仍是 `ready`（账本粘滞着的话会回到 `startup_latched`）。
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status = h.view();
    assert_eq!(
        (status.state, status.reason),
        (UpstreamState::Ready, None),
        "the ledger was rebuilt for A, not reopened stale"
    );
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 复审（S6c 修复）：H1、H2、M5、M6
// ---------------------------------------------------------------------------------------------

/// 复审 H1：退役里的停进程已经开始（SIGTERM 发出，引擎无视，宽限 10 秒），这时 iDoris 停止、带 3 秒的期限。停进程的任务是按没有期限算的，
/// 只缩短外面的等待就会在 SIGKILL 之前中止它：`stop_on_exit:"never"` 的句柄丢弃时不杀，引擎活下来。现在到期限之前留出升级的时间，
/// 中止任务之后认回它、SIGTERM 之后立刻 SIGKILL。
#[tokio::test]
async fn a_shutdown_in_the_middle_of_a_retirement_stop_still_gets_the_never_engine_killed_in_time()
{
    let rig = Rig::new(&Behavior {
        on_sigterm: OnSigterm::Ignore,
        ..Behavior::default()
    });
    let mut timings = fast();
    timings.stop_grace = Duration::from_secs(10);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        timings,
        StopOnExit::Never,
    );
    h.until_state(UpstreamState::Ready).await;
    let pid = rig.pid(0);
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the retirement's SIGTERM", || {
        !rig.fake.sigterm_times().is_empty()
    })
    .await;
    assert!(
        pid_alive(pid),
        "it ignores SIGTERM: the stop is in its grace"
    );

    let begun = Instant::now();
    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_secs(3)),
        )
        .await;
    let took = begun.elapsed();
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert!(
        took < Duration::from_secs(5),
        "the 10 s grace did not hold the 3 s deadline: {took:?}"
    );
    assert_eq!(
        rig.records(),
        0,
        "its record was reclaimed once it was gone"
    );
}

/// 复审 H2：退役没能确认之后，认回同一个进程时的重试停止也走有期限、可被停止指令打断的路。此前它是一个内联的、拿满宽限的停止：iDoris 在停的时候
/// 打断不了，管理器中止任务，认回的引擎（没有子进程句柄、丢弃不杀）活下来。
#[tokio::test]
async fn a_shutdown_during_the_retry_of_an_unconfirmed_retirement_still_gets_the_engine_stopped() {
    let rig = Rig::new(&Behavior {
        on_sigterm: OnSigterm::Ignore,
        ..Behavior::default()
    });
    let inspector = Scripted::claiming(rig.port);
    let old = leave_an_orphan(&rig, &inspector).await;
    let mut timings = fast();
    timings.stop_grace = Duration::from_secs(10);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        timings,
        StopOnExit::Never,
    );
    h.until_state(UpstreamState::Ready).await;

    inspector.set_identity_unreadable(true);
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the unconfirmed stop", || h.logged("停止引擎失败")).await;
    assert!(rig.fake.sigterm_times().is_empty(), "nothing was signalled");
    inspector.set_identity_unreadable(false);
    // 下一轮启动决策认回同一个进程，重试停止：SIGTERM 发出，它无视。
    wait_for("the retry's SIGTERM", || {
        !rig.fake.sigterm_times().is_empty()
    })
    .await;
    assert!(pid_alive(old));

    let begun = Instant::now();
    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_secs(3)),
        )
        .await;
    wait_for("the engine to die", || !pid_alive(old)).await;
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(rig.records(), 0);
}

/// 复审 M5：`restart` 的新引擎正在启动（拉起了、还没听）时来第二个 `restart`：并进它，不去杀刚拉起的替身再拉一个。
#[tokio::test]
async fn a_second_restart_while_the_replacement_is_starting_joins_it_and_does_not_kill_it() {
    let rig = Rig::new(&Behavior {
        listen_delay_secs: 1.5,
        ..Behavior::default()
    });
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        Scripted::claiming(rig.port),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the replacement to be launched", || {
        rig.fake.launches() >= 2
    })
    .await;
    let replacement = rig.pid(1);
    h.manager.restart_upstream("omlx").await.unwrap();
    h.until("ready", |s| s.state == UpstreamState::Ready && s.owned)
        .await;
    assert_eq!(
        rig.fake.launches(),
        2,
        "the second restart did not replace the replacement"
    );
    assert!(pid_alive(replacement));
    h.manager.shutdown(true).await;
}

/// 复审 M6、复审四 H：退役没能确认的进程，所有权记录文件被删（认回的路走不通）、进程还在。之前只记 pid 与启动时间，记录没了就没有东西能证明它的身份，
/// 既不能停它、也不能放它回来；现在留着完整的记录，过身份闸照样停它（停掉才拉替身，它不会作为外部引擎或别的什么回来）。
#[tokio::test]
async fn a_condemned_process_whose_record_file_is_gone_is_stopped_from_the_kept_record() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let old = leave_an_orphan(&rig, &inspector).await;
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    inspector.set_identity_unreadable(true);
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the unconfirmed stop", || h.logged("停止引擎失败")).await;
    for entry in std::fs::read_dir(rig.record_dir()).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    inspector.set_identity_unreadable(false);
    wait_for("the condemned process to be stopped", || !pid_alive(old)).await;
    let status = h
        .until("a replacement", |s| {
            s.state == UpstreamState::Ready && s.owned
        })
        .await;
    assert_eq!(status.reason, None);
    assert_eq!(
        rig.fake.launches(),
        2,
        "the replacement came after it was gone"
    );
    h.manager.shutdown(true).await;
}

/// 复审四 H（上一轮的 H2 还没修）：`stop` 没能确认（身份读不出来）后落在终态不再重试；记录文件被删；这时 iDoris 停止。此前 `shutdown` 的回收路径用
/// 磁盘上的记录认回引擎，记录没了就当它已经消失，引擎活下来（测试以前是自己在 `shutdown` 之后补一个 SIGKILL）。现在 `shutdown` 自己用留着的完整记录把
/// 它停掉。
#[tokio::test]
async fn a_shutdown_stops_a_condemned_engine_even_when_its_record_file_is_gone() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let old = leave_an_orphan(&rig, &inspector).await;
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Owned,
    );
    h.until_state(UpstreamState::Ready).await;
    inspector.set_identity_unreadable(true);
    h.manager.stop_upstream("omlx").await.unwrap();
    wait_for("the unconfirmed stop", || h.logged("停止引擎失败")).await;
    h.until("terminal and unconfirmed", |s| {
        s.state == UpstreamState::Unavailable && s.reason == Some(UpstreamReason::PortInUse)
    })
    .await;
    assert!(pid_alive(old));
    for entry in std::fs::read_dir(rig.record_dir()).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    inspector.set_identity_unreadable(false);

    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_secs(4)),
        )
        .await;
    assert!(
        !pid_alive(old),
        "the shutdown itself stopped the condemned engine"
    );
}

/// 复审四 H：外部引擎换进程（A 到 B），复位的采样在进行时端口又换了主人（C，不合格）。此前复位一成功就开门、紧接着对端口发带 key 的探测：key 发给了
/// 一个没有通过校验的进程。现在开门之前重新核对监听者，不对就不开门、端点保持封上；带 key 的周期探测也走授权，不是裸的 bearer。
#[tokio::test]
async fn the_key_is_not_sent_to_a_process_that_took_the_port_during_an_external_reset() {
    use idoris_router::runtime::RuntimeRegistry;

    let work = tempfile::tempdir().unwrap();
    let a = External::start(work.path(), Some("secret"));
    let other_b = tempfile::tempdir().unwrap();
    let b = External::start(other_b.path(), None);
    let other_c = tempfile::tempdir().unwrap();
    let c = External::start(other_c.path(), None);
    let rig = rig_for(&a);
    let inspector = Scripted::new();
    inspector.external_listener(a.port, a.pid(), Some(TEAM));
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("secret".to_owned());
    let h = Harness::start(&rig, backend, inspector.clone(), fast(), StopOnExit::Owned);
    h.until_state(UpstreamState::Ready).await;
    assert!(h.manager.open_gate().is_some());
    let calls = Arc::new(AtomicUsize::new(0));
    let sampled = calls.clone();
    let registry =
        RuntimeRegistry::spawn_with_factory(&[card("omlx", &rig.endpoint(), true)], move |_| {
            Ok(Arc::new(SlowFirstStatus {
                mock: idoris_backend::MockAdapter::new(Vec::new()),
                calls: sampled.clone(),
                // 第 0 次是 Supervisor 的启动采样，第 1 次是外部换进程之后的复位：它睡 2 秒。
                slow_call: 1,
                first: Duration::from_secs(2),
            }))
        })
        .unwrap();
    h.manager.attach_runtimes(registry);
    h.until_state(UpstreamState::Ready).await;
    wait_for("the startup sample", || calls.load(Ordering::SeqCst) >= 1).await;

    // A 换成 B：复位开始，采样睡着。
    inspector.external_listener(a.port, b.pid(), Some(TEAM));
    wait_for("the reset's sample to start", || {
        calls.load(Ordering::SeqCst) >= 2
    })
    .await;
    // 采样睡着的时候端口被 C 占了：C 没有通过校验（没有团队）。
    inspector.external_listener(a.port, c.pid(), None);
    let keyed = || {
        a.fake
            .requests()
            .iter()
            .filter(|(_, auth)| auth == "ok")
            .count()
    };
    let before = keyed();
    // 采样结束、复位成功之后的几秒里，没有一个带 key 的请求发出去。
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(
        keyed(),
        before,
        "a key went to a process that never passed the owner check: {:?}",
        a.fake.requests()
    );
    assert_ne!(h.view().state, UpstreamState::Ready);
    h.manager.shutdown(true).await;
}

/// 复审四 H：监听者检查（`lsof` 读不出来，按重试次数重读、每次隔一段）占着实例任务时 iDoris 停止。此前实例任务不看停止指令，管理器按期限中止它，
/// `stop_on_exit:"never"` 的子进程成了孤儿。现在检查与重试的等待都在停止指令到来时放手，引擎握在手里带期限收尾。
#[tokio::test]
async fn a_shutdown_during_a_slow_listener_inspection_still_stops_the_never_engine() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let mut timings = fast();
    timings.unverifiable_retries = 4;
    timings.unverifiable_pause = Duration::from_secs(3);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        timings,
        StopOnExit::Never,
    );
    h.until_state(UpstreamState::Ready).await;
    let pid = rig.pid(0);
    // 从现在起每次检查都读不出来：监督的下一个节拍进入 4 次、每次隔 3 秒的重读。
    inspector.unreadable.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;

    let begun = Instant::now();
    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_secs(3)),
        )
        .await;
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert!(
        begun.elapsed() < Duration::from_secs(6),
        "the 12 s of inspection did not hold the 3 s deadline: {:?}",
        begun.elapsed()
    );
    assert_eq!(rig.records(), 0);
}

/// 复审四 M：受理的 `restart` 退役没能确认，之后的重试确认了（旧进程停掉）、替身起来。此前重试成功只清了 `condemned`，没清「已受理的退役」：之后 `never` 的
/// 正常退出把无辜的替身也停掉了。现在退役的意图绑在那个引擎上，确认就清，替身被留下。
#[tokio::test]
async fn a_confirmed_retirement_does_not_make_a_later_never_shutdown_kill_the_replacement() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    let old = leave_an_orphan(&rig, &inspector).await;
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Never,
    );
    h.until_state(UpstreamState::Ready).await;
    inspector.set_identity_unreadable(true);
    h.manager.restart_upstream("omlx").await.unwrap();
    wait_for("the unconfirmed stop", || h.logged("停止引擎失败")).await;
    inspector.set_identity_unreadable(false);
    h.until("the replacement", |s| {
        s.state == UpstreamState::Ready && s.owned
    })
    .await;
    wait_for("the old engine to be gone", || !pid_alive(old)).await;
    let replacement = rig.pid(1);

    // 正常退出、`never`：替身留给下一个 iDoris。
    h.manager.shutdown(false).await;
    assert!(
        pid_alive(replacement),
        "the replacement was handed over, not killed by a stale retirement intent"
    );
    assert_eq!(rig.records(), 1, "its record is kept");
}

// ---------------------------------------------------------------------------------------------
// 外部引擎换了监听者：用新校验出来的监听者重新登记，监听者没了就封上（Codex 预审 L2 的 M1）
// ---------------------------------------------------------------------------------------------

/// 预审 L2 的 M1：合法的外部重启——监听者 A 退出、B 起来。监督原来校验了 B、却拿着 A 的守卫继续对着 B 登记：数据面的每个请求都对着 A 的集合验、一直被拒绝
/// （登记表也没有新的代际）。现在新校验出来的集合与登记时的不一样，就用它建一个新的守卫登记进去（代际前进），请求对着 B 验、通过。监听者没了（`NoListener`）
/// 原来只发布 `unavailable`、登记还留着：现在端点立刻封上，什么凭证都不发；同一个监听者回来，再登记回去。
#[tokio::test]
async fn m1_a_replaced_external_listener_is_registered_afresh_and_a_vanished_one_blocks_the_endpoint()
 {
    use idoris_upstream::omlx::key::{KeyRefusal, authorize_omlx_request, endpoint_generation};

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
    let endpoint = rig.endpoint();
    let verified = EndpointKey::External {
        settings_key_verified: true,
    };
    assert_eq!(governed_endpoint(&endpoint), Some(verified.clone()));
    let first_generation = endpoint_generation(&endpoint);
    assert!(
        authorize_omlx_request(&endpoint, None).await.is_ok(),
        "the verified listener is authorized"
    );

    // A 被 B 取代（B 通过同样的校验，pid 与启动时间都不同）：新登记，代际前进，请求对着 B 验、通过。
    let b_pid = std::process::id();
    inspector.external_listener(external.port, b_pid, Some(TEAM));
    wait_for("the listener to be registered afresh", || {
        endpoint_generation(&endpoint) > first_generation
    })
    .await;
    h.until_state(UpstreamState::Ready).await;
    let second_generation = endpoint_generation(&endpoint);
    assert_eq!(governed_endpoint(&endpoint), Some(verified.clone()));
    let authorized = authorize_omlx_request(&endpoint, None).await;
    assert!(
        authorized.is_ok(),
        "the guard of the listener that left would refuse the one that came: {authorized:?}"
    );
    drop(authorized);

    // 监听者没了：端点立刻封上（之前只发布 `unavailable`，登记留着），授权被拒绝。
    inspector.forget_listener(external.port);
    wait_for("the endpoint to be blocked", || {
        governed_endpoint(&endpoint) == Some(EndpointKey::Blocked)
    })
    .await;
    assert_eq!(
        authorize_omlx_request(&endpoint, None).await.unwrap_err(),
        KeyRefusal::Blocked
    );
    assert!(endpoint_generation(&endpoint) > second_generation);

    // 同一个监听者回来：校验通过，再登记，回到 `ready`。
    inspector.external_listener(external.port, b_pid, Some(TEAM));
    h.until_state(UpstreamState::Ready).await;
    assert_eq!(governed_endpoint(&endpoint), Some(verified));
    assert!(authorize_omlx_request(&endpoint, None).await.is_ok());
    assert_eq!(
        rig.fake.launches(),
        0,
        "an external engine is never replaced by ours"
    );
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// 外部引擎的带凭证探测经授权路径；监听者没了的独立覆盖（Codex 复审 L2 的 M2' 与 M1 的 NoListener 分支）
// ---------------------------------------------------------------------------------------------

/// 复审 L2 的 M2'（接管）：外部引擎要 key，属主校验通过（名单与签名都对），但紧接着的授权里守卫的重新确认不过（原生监视说进程不再握着那个套接字）：带 key 的探测**一个
/// 字节都没发出去**。之前先校验、再用裸的探测把 key 发给 endpoint：校验与发送是两件事，守卫的确认根本没参与。守卫又说握着了，才接管、才发 key。
#[tokio::test]
async fn m2b_adopting_an_external_engine_sends_its_key_only_when_the_guard_still_vouches_for_the_listener()
 {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), Some("ext-secret"));
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let gone = inspector.offer_a_native_watch_that_can_say_gone();
    gone.store(true, Ordering::SeqCst);
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("ext-secret".to_owned());
    let h = Harness::start(&rig, backend, inspector, fast(), StopOnExit::Owned);

    wait_for("the keyed probe to be refused", || {
        h.logged("外部引擎的带凭证探测没有发出")
    })
    .await;
    // 发给引擎的只有那一次不带 key 的探测（它答 401），key 没有出过门；之后的几次重试也一样。
    tokio::time::sleep(Duration::from_millis(500)).await;
    let requests = external.fake.requests();
    assert!(!requests.is_empty(), "the keyless first probe was sent");
    assert!(
        requests.iter().all(|(_, auth)| auth == "none"),
        "the key went out although the guard did not vouch for the listener: {requests:?}"
    );

    // 守卫又说握着：接管，带上 key。
    gone.store(false, Ordering::SeqCst);
    let status = h.until_state(UpstreamState::Ready).await;
    assert!(!status.owned);
    assert!(
        external
            .fake
            .requests()
            .iter()
            .any(|(_, auth)| auth == "ok"),
        "the key was sent once the listener was vouched for"
    );
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    h.manager.shutdown(true).await;
}

/// 复审 L2 的 M2'（监督）：接管之后每个周期的探测同样经授权路径。周期性的属主校验仍通过，守卫的重新确认不过：探测被拒绝（记一行日志、`unavailable`），之后没有一个带 key
/// 的请求到达引擎；守卫又说握着，回到 `ready`。
#[tokio::test]
async fn m2b_supervising_an_external_engine_sends_its_key_only_when_the_guard_still_vouches_for_the_listener()
 {
    let work = tempfile::tempdir().unwrap();
    let external = External::start(work.path(), Some("ext-secret"));
    let rig = rig_for(&external);
    let inspector = Scripted::new();
    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    let gone = inspector.offer_a_native_watch_that_can_say_gone();
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("ext-secret".to_owned());
    let h = Harness::start(&rig, backend, inspector, fast(), StopOnExit::Owned);
    h.until_state(UpstreamState::Ready).await;

    gone.store(true, Ordering::SeqCst);
    wait_for("the supervision probe to be refused", || {
        h.logged("外部引擎的带凭证探测没有发出")
    })
    .await;
    h.until_state(UpstreamState::Unavailable).await;
    // 日志里有了拒绝，此后每个周期都被拒绝：引擎收到的带 key 的请求不再增加。
    let keyed = |external: &External| {
        external
            .fake
            .requests()
            .iter()
            .filter(|(_, auth)| auth == "ok")
            .count()
    };
    let before = keyed(&external);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        keyed(&external),
        before,
        "a keyed probe went out although the guard did not vouch for the listener"
    );

    gone.store(false, Ordering::SeqCst);
    h.until_state(UpstreamState::Ready).await;
    assert!(keyed(&external) > before, "probing resumed with the key");
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    h.manager.shutdown(true).await;
}

/// 复审 L2 的 M1 的 `NoListener` 分支，**单独**覆盖（`m1_a_replaced_…` 只在换监听者之后的末尾顺带走到它）：刚接管的外部引擎，端口上的监听者没了：端点**立刻**封上
/// （不等下一个周期的校验，什么凭证都不再发）、状态是 `unavailable`（没有原因码）、授权被拒绝；同一个监听者回来，校验通过，再登记，回到 `ready`。
#[tokio::test]
async fn m1b_a_vanished_external_listener_blocks_the_endpoint_at_once_and_the_one_that_comes_back_is_registered_again()
 {
    use idoris_upstream::omlx::key::{KeyRefusal, authorize_omlx_request, endpoint_generation};

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
    let endpoint = rig.endpoint();
    let verified = EndpointKey::External {
        settings_key_verified: true,
    };
    assert_eq!(governed_endpoint(&endpoint), Some(verified.clone()));
    let registered = endpoint_generation(&endpoint);

    inspector.forget_listener(external.port);
    wait_for("the endpoint to be blocked", || {
        governed_endpoint(&endpoint) == Some(EndpointKey::Blocked)
    })
    .await;
    let status = h.until_state(UpstreamState::Unavailable).await;
    assert_eq!(status.reason, None, "{status:?}");
    assert!(!status.owned);
    assert_eq!(
        authorize_omlx_request(&endpoint, None).await.unwrap_err(),
        KeyRefusal::Blocked
    );
    assert!(endpoint_generation(&endpoint) > registered);

    inspector.external_listener(external.port, external.pid(), Some(TEAM));
    h.until_state(UpstreamState::Ready).await;
    assert_eq!(governed_endpoint(&endpoint), Some(verified));
    assert!(authorize_omlx_request(&endpoint, None).await.is_ok());
    assert_eq!(rig.fake.launches(), 0, "nothing is spawned beside it");
    assert!(pid_alive(u64::from(external.pid())), "never signalled");
    h.manager.shutdown(true).await;
}

/// 复审五 H：初次接管外部引擎时，带 key 的重试原来是对着端口发裸的 bearer。属主校验期间端口换了主人（C，没有团队）：校验通过的是 A，key 却发给了
/// 校验之后才到的 C。现在重试走授权，发出之前重新确认监听者，不对就一个字节也不发。
#[tokio::test]
async fn the_key_of_an_initial_external_adoption_is_not_sent_to_a_listener_that_replaced_the_verified_one()
 {
    let work = tempfile::tempdir().unwrap();
    let a = External::start(work.path(), Some("secret"));
    let other = tempfile::tempdir().unwrap();
    let c = External::start(other.path(), None);
    let rig = rig_for(&a);
    let inspector = Scripted::new();
    inspector.external_listener(a.port, a.pid(), Some(TEAM));
    *inspector.flip_after_first_team_check.lock().unwrap() = Some((a.port, c.pid()));
    let backend = default_backend(&rig);
    *backend.external_key.lock().unwrap() = Some("secret".to_owned());
    let h = Harness::start(&rig, backend, inspector.clone(), fast(), StopOnExit::Owned);
    wait_for("the owner check to have run", || {
        inspector.team_checks.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let keyed = a
        .fake
        .requests()
        .iter()
        .filter(|(_, auth)| auth == "ok")
        .count();
    assert_eq!(
        keyed,
        0,
        "a key went to a listener the owner check never saw: {:?}",
        a.fake.requests()
    );
    assert_ne!(h.view().state, UpstreamState::Ready);
    h.manager.shutdown(true).await;
}

// ---------------------------------------------------------------------------------------------
// Codex 复审六：封印与停止指令
// ---------------------------------------------------------------------------------------------

/// 复审五 M 的回归（此前没有）：封印正在进行（读命令行睡 6 秒）时 iDoris 要**停**引擎、期限 3 秒。封印不放手的话实例任务卡在里面，管理器按期限中止它，
/// `stop_on_exit:"never"` 的子进程成了孤儿。
#[tokio::test]
async fn a_shutdown_that_must_stop_the_engine_interrupts_a_pending_seal() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    *inspector.seal_delay.lock().unwrap() = Duration::from_secs(6);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Never,
    );
    wait_for("the engine to be ready and the seal to be pending", || {
        rig.fake.launches() >= 1 && rig.fake.requests().iter().any(|(_, auth)| auth == "ok")
    })
    .await;
    let pid = rig.pid(0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(
        h.view().state,
        UpstreamState::Ready,
        "the seal is still pending"
    );

    let begun = Instant::now();
    h.manager
        .shutdown_by(
            true,
            Some(tokio::time::Instant::now() + Duration::from_secs(3)),
        )
        .await;
    wait_for("the engine to die", || !pid_alive(pid)).await;
    assert!(
        begun.elapsed() < Duration::from_secs(6),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(rig.records(), 0);
}

/// 复审六 M：`stop_on_exit:"never"` 的正常退出（引擎要留）到来时封印还在进行。此前封印被取消，留下一条没封印的记录，下一个 iDoris 把它当作「启动途中被杀」
/// 停掉重拉，暖着的引擎丢了。现在封印做完（在期限里），记录是封印的，下一个 iDoris 认回同一个引擎。
#[tokio::test]
async fn a_clean_never_shutdown_lets_a_pending_seal_finish_so_the_next_idoris_adopts_the_engine() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    *inspector.seal_delay.lock().unwrap() = Duration::from_millis(1500);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector.clone(),
        fast(),
        StopOnExit::Never,
    );
    wait_for("the engine to be ready and the seal to be pending", || {
        rig.fake.launches() >= 1 && rig.fake.requests().iter().any(|(_, auth)| auth == "ok")
    })
    .await;
    let pid = rig.pid(0);
    assert_ne!(
        h.view().state,
        UpstreamState::Ready,
        "the seal is still pending"
    );
    h.manager
        .shutdown_by(
            false,
            Some(tokio::time::Instant::now() + Duration::from_secs(8)),
        )
        .await;
    assert!(pid_alive(pid), "never + clean exit: the engine is kept");

    // 下一个 iDoris：认回同一个引擎，不是「没有封印」停掉重拉。
    *inspector.seal_delay.lock().unwrap() = Duration::ZERO;
    let next = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Owned,
    );
    next.until_state(UpstreamState::Ready).await;
    assert!(!next.logged("没有封印"), "the record was sealed");
    assert_eq!(
        rig.fake.launches(),
        1,
        "the warm engine was adopted, not replaced"
    );
    assert!(pid_alive(pid));
    next.manager.shutdown(true).await;
}

/// 复审七 M：要留引擎的封印撞上停止期限（封印要 3 秒，期限 1 秒）：到期限就放弃封印，收尾在期限里结束，引擎仍在（`never`），不是等封印做完把整个收尾拖过期限。
#[tokio::test]
async fn a_keep_engine_seal_that_reaches_the_cutoff_is_given_up_at_the_cutoff() {
    let rig = Rig::new(&Behavior::default());
    let inspector = Scripted::claiming(rig.port);
    *inspector.seal_delay.lock().unwrap() = Duration::from_secs(3);
    let h = Harness::start(
        &rig,
        default_backend(&rig),
        inspector,
        fast(),
        StopOnExit::Never,
    );
    wait_for("the engine to be ready and the seal to be pending", || {
        rig.fake.launches() >= 1 && rig.fake.requests().iter().any(|(_, auth)| auth == "ok")
    })
    .await;
    let pid = rig.pid(0);
    let begun = Instant::now();
    h.manager
        .shutdown_by(
            false,
            Some(tokio::time::Instant::now() + Duration::from_secs(1)),
        )
        .await;
    assert!(
        begun.elapsed() < Duration::from_millis(2500),
        "the shutdown did not wait for the 3 s seal: {:?}",
        begun.elapsed()
    );
    assert!(pid_alive(pid), "never + clean exit: the engine is kept");
}
