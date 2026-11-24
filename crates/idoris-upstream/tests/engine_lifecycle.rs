//! IDORIS-CORE S3：引擎进程原语的端到端测试（设计稿 §2.7.4、§2.7.5）。
//!
//! **通用流程用一个在本文件里定义的假引擎（`FakeEngine`，种类 `fakeserve`）跑**：它与 oMLX 没有任何
//! 共同代码，只实现 [`EngineRecipe`]（另一套命令行、另一个环境变量、另一个就绪路径），拉起、记录、认回、
//! 停止、日志、状态目录布局一行不改照样工作，这就是「抽象真的通用」的证明。oMLX 自己的配方
//! （[`OmlxRecipe`]）另有几个测试：它拼的命令行与环境、它的就绪探测、整条生命周期；以及两种引擎并存、
//! 按种类分目录的记录与日志（规范 §2.7.6）、同一种类的两个实例、配置里的端口换了之后留下的记录。
//!
//! 全部用临时目录里的假 `omlx` / 假引擎 shell 脚本（睡觉、忽略 SIGTERM、再 fork 子进程、监听端口、立刻退出……）
//! 与 wiremock 假 HTTP 引擎；**不启动真实 oMLX 或任何推理进程，不连外网，不碰真实的 `~/.omlx`**。
//! 引擎的环境是清洗过的，所以脚本需要的路径都在生成脚本时写死。
//!
//! 两个说明：
//! - 假脚本在 `started` 标记文件出现之后才算「稳定」（`ps` 读到的命令行不再变）：拉起后立刻封印会读到
//!   `exec` 之前的命令行。真实 oMLX 的就绪由引擎自己的 HTTP 应答证明，那时 `exec` 链早已结束。
//! - 需要 `lsof` 的测试在没有 `lsof` 的机器上跳过并说明；macOS 有 `/usr/sbin/lsof`。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-router/tests/support/state_isolation.rs"]
mod state_isolation;
use state_isolation::IsolateState;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use idoris_upstream::engine::{
    DiscoveryPlan, EngineError, EngineInstance, EngineKey, EngineKind, EngineLaunch,
    EngineLaunchSpec, EngineProbe, EngineRecipe, ExecutableSource, IdentityVerdict, InspectError,
    LaunchError, LaunchInput, LaunchPlan, MismatchReason, OwnedEngine, OwnershipRecord, PidState,
    ProcessInspector, ReadyPolicy, Recovery, StopOnExit, SystemInspector, UnreadableReason,
    probe_http, recorded_ports, verify_identity,
};
use idoris_upstream::omlx::engine::{OmlxRecipe, base_path_in, prepare_base_dir};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LAUNCHED_BY: &str = "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11";
/// 单个动作的耐心：macOS 机器范围内串行化每个新写文件的首次 exec（约 0.2 秒一个），负载高时可达数秒。
const PATIENT: Duration = Duration::from_secs(60);
/// 假引擎的种类（合乎规范的 `^[a-z][a-z0-9_]{0,31}$`，不是任何保留名）。
const FAKE_KIND: &str = "fakeserve";

fn kind(name: &str) -> EngineKind {
    EngineKind::parse(name).unwrap()
}

// ---------------------------------------------------------------------------------------------
// 第二种引擎：与 oMLX 毫无关系，只实现 `EngineRecipe`
// ---------------------------------------------------------------------------------------------

/// 一个假的第二种引擎：自己的命令行（`--serve --bind … --listen-port N`）、自己的环境变量
/// （`FAKE_ENGINE_TOKEN`）、自己的就绪探测（`GET /health` 返回 `fake-ok`）。
#[derive(Debug, Clone)]
struct FakeEngine {
    kind: &'static str,
}

#[async_trait]
impl EngineRecipe for FakeEngine {
    fn kind(&self) -> EngineKind {
        kind(self.kind)
    }

    fn discovery_plan(&self) -> DiscoveryPlan {
        DiscoveryPlan::default()
    }

    fn launch_plan(&self, input: &LaunchInput<'_>) -> Result<LaunchPlan, LaunchError> {
        let mut env = BTreeMap::new();
        if let Some(key) = input.api_key {
            env.insert("FAKE_ENGINE_TOKEN".into(), key.expose().into());
        }
        Ok(LaunchPlan {
            args: [
                "--serve",
                "--bind",
                "127.0.0.1",
                "--listen-port",
                &input.port.to_string(),
            ]
            .map(OsString::from)
            .to_vec(),
            env,
        })
    }

    async fn probe_ready(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> EngineProbe {
        probe_http(
            client,
            &format!("{endpoint}/health"),
            bearer,
            timeout,
            |body| body == b"fake-ok",
        )
        .await
    }
}

fn fake_recipe() -> Arc<dyn EngineRecipe> {
    Arc::new(FakeEngine { kind: FAKE_KIND })
}

// ---------------------------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------------------------

struct Rig {
    state: tempfile::TempDir,
    work: tempfile::TempDir,
    /// 以 `stop_on_exit: never` 拉起的引擎按设计活得比句柄久；测试结束（含 panic）时，台架把它们剩下的清掉，
    /// 失败的测试不会留下孤儿。
    reapers: std::cell::RefCell<Vec<Reaper>>,
}

/// 假引擎进程（shell 脚本）的行为。
#[derive(Clone, Copy)]
enum Behavior {
    /// 睡觉；SIGTERM 即退出。主进程 + 一个 `sleep` 子进程，同一进程组。
    Obedient,
    /// 忽略 SIGTERM（子进程也继承忽略），主进程 `wait` 着它的 `sleep` 子进程（不再 fork：SIGKILL 扫过的那一刻
    /// 刚 fork 出来的子进程会漏过一轮）。只有 SIGKILL 杀得掉。
    IgnoresTerm,
    /// 起来 0.3 秒就以 3 退出（引擎在启动途中崩溃）。
    CrashesSoon,
}

struct Fake {
    exe: PathBuf,
    started: PathBuf,
    leader: PathBuf,
    child: PathBuf,
}

impl Fake {
    /// 等脚本稳定（主进程与子进程都已存在）。
    fn wait_started(&self) {
        wait_until("the fake engine to start", || self.started.exists());
    }
    /// (主进程 pid, 子进程 pid)。
    fn pids(&self) -> (i32, i32) {
        self.wait_started();
        let read = |p: &Path| std::fs::read_to_string(p).unwrap().trim().parse().unwrap();
        (read(&self.leader), read(&self.child))
    }
}

impl Rig {
    fn new() -> Self {
        Self {
            state: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
            reapers: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// 引擎一拉起就登记：之后哪怕测试在读它之前 panic，它的进程组也会被清掉。
    fn reap_on_drop(&self, engine: &OwnedEngine) {
        self.reapers
            .borrow_mut()
            .push(Reaper::new(engine.pid() as i32));
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.work.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn fake(&self, name: &str, behavior: Behavior) -> Fake {
        let w = self.work.path();
        let (started, leader, child) = (
            w.join(format!("{name}.started")),
            w.join(format!("{name}.leader")),
            w.join(format!("{name}.child")),
        );
        let (pre, tail) = match behavior {
            Behavior::Obedient => ("", "wait"),
            Behavior::IgnoresTerm => ("trap '' TERM", "wait"),
            Behavior::CrashesSoon => ("", "sleep 0.3; exit 3"),
        };
        let exe = self.script(
            name,
            &format!(
                "{pre}\necho $$ > '{leader}'\nsleep 600 &\necho $! > '{child}'\n: > '{started}'\n{tail}",
                leader = leader.display(),
                child = child.display(),
                started = started.display(),
            ),
        );
        Fake {
            exe,
            started,
            leader,
            child,
        }
    }

    /// 状态目录里、端口 `port` 上的一个实例，配方是 `recipe`。
    fn instance(&self, recipe: Arc<dyn EngineRecipe>, port: u16) -> EngineInstance {
        EngineInstance::new(recipe, self.state.path(), port).unwrap()
    }

    /// 假引擎（种类 `fakeserve`）在端口 `port` 上的实例。
    fn fake_instance(&self, port: u16) -> EngineInstance {
        self.instance(fake_recipe(), port)
    }

    /// iDoris 自己的 oMLX base：状态目录下的 `omlx-base`，准备好了（0700）。oMLX 的配方没有它不拉起。
    fn omlx_base(&self) -> PathBuf {
        let base = base_path_in(self.state.path());
        prepare_base_dir(&base).unwrap();
        base
    }

    /// 配好 iDoris 自己的 base 与模型目录的 oMLX 配方（拉起两者都要；假 `omlx` 脚本不看它们）。
    fn omlx_recipe(&self) -> OmlxRecipe {
        OmlxRecipe::new()
            .with_base_path(self.omlx_base())
            .with_model_dir(self.work.path().join("models"))
    }

    /// 规范 §2.7.6：`<状态目录>/engines-owned/<engine_kind>`。
    fn record_dir(&self, engine_kind: &str) -> PathBuf {
        self.state.path().join("engines-owned").join(engine_kind)
    }

    /// 规范 §2.7.6：`<状态目录>/logs`。
    fn logs_dir(&self) -> PathBuf {
        self.state.path().join("logs")
    }

    fn launch(
        &self,
        instance: &EngineInstance,
        exe: &Path,
        key: &EngineKey,
        stop: StopOnExit,
    ) -> EngineLaunch {
        self.launch_with(
            instance,
            exe,
            LaunchOpts {
                key: Some(key),
                stop,
                ..LaunchOpts::default()
            },
        )
    }

    fn launch_with(
        &self,
        instance: &EngineInstance,
        exe: &Path,
        opts: LaunchOpts<'_>,
    ) -> EngineLaunch {
        EngineLaunch::new(
            instance,
            EngineLaunchSpec {
                executable: exe.to_owned(),
                allocated_budget_bytes: opts.budget,
                api_key: opts.key.cloned(),
                stop_on_exit: opts.stop,
            },
            opts.parent_env,
        )
        .unwrap()
    }
}

/// `Rig::launch_with` 的可选项；缺省：没有 key、`Owned`、没有额度、父环境是这个测试进程真实的环境。
struct LaunchOpts<'a> {
    key: Option<&'a EngineKey>,
    stop: StopOnExit,
    budget: Option<u64>,
    parent_env: Vec<(OsString, OsString)>,
}

impl Default for LaunchOpts<'_> {
    fn default() -> Self {
        Self {
            key: None,
            stop: StopOnExit::Owned,
            budget: None,
            parent_env: std::env::vars_os().collect(),
        }
    }
}

fn endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// 假引擎的 HTTP 面：只认带这个引擎私有 key 的 `GET /health`，答 `fake-ok`。
async fn fake_http(key: &EngineKey) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .and(header(
            "authorization",
            format!("Bearer {}", key.expose()).as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("fake-ok"))
        .mount(&server)
        .await;
    server
}

/// oMLX 的 HTTP 面：只认带这个引擎私有 key 的 `GET /v1/models`。
async fn omlx_http(key: &EngineKey) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header(
            "authorization",
            format!("Bearer {}", key.expose()).as_str(),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": []})),
        )
        .mount(&server)
        .await;
    server
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn fast_ready() -> ReadyPolicy {
    ReadyPolicy {
        timeout: PATIENT,
        initial_interval: Duration::from_millis(20),
        max_interval: Duration::from_millis(100),
        request_timeout: Duration::from_secs(10),
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn lsof_available() -> bool {
    Path::new("/usr/sbin/lsof").is_file() || Path::new("/usr/bin/lsof").is_file()
}

/// 没有 `lsof` 的机器上跳过用到它的测试（说明原因，不静默）。
fn require_lsof(test: &str) -> bool {
    if lsof_available() {
        return true;
    }
    eprintln!("skipping {test}: lsof is not installed on this machine");
    false
}

// ---- 进程观察（独立于被测代码：直接读 ps / kill） ----

fn ps(args: &[&str]) -> String {
    let output = std::process::Command::new("/bin/ps")
        .args(args)
        .env_clear()
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn ps_command(pid: u32) -> String {
    ps(&["-o", "command=", "-p", &pid.to_string()])
}

fn ps_lstart(pid: u32) -> String {
    ps(&["-o", "lstart=", "-p", &pid.to_string()])
}

fn ps_pgid(pid: u32) -> String {
    ps(&["-o", "pgid=", "-p", &pid.to_string()])
}

/// 进程组里还没死（不是僵尸）的成员 pid。
fn live_group_members(pgid: i32) -> Vec<i32> {
    ps(&["-A", "-o", "pid=,pgid=,stat="])
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: i32 = fields.next()?.parse().ok()?;
            let group: i32 = fields.next()?.parse().ok()?;
            let stat = fields.next()?;
            (group == pgid && !stat.starts_with('Z')).then_some(pid)
        })
        .collect()
}

fn group_dead(pgid: i32) -> bool {
    live_group_members(pgid).is_empty()
}

fn pid_alive(pid: i32) -> bool {
    if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
        return false;
    }
    // 僵尸（已退出、还没被回收）不算活着。
    let stat = ps(&["-o", "stat=", "-p", &pid.to_string()]);
    !stat.is_empty() && !stat.starts_with('Z')
}

fn wait_group_dead(pgid: i32) {
    wait_until("the process group to disappear", || group_dead(pgid));
}

/// 测试自己拉起的进程组，在测试结束（含 panic）时一定清掉。
/// 只动自己的组：组长仍是创建时读到启动时间的那个进程；或者组长已经没了而组里还有活成员
/// （组里有成员时，组 id 不会被分给别的进程，所以这个组一定还是自己的）。pid 被别的进程复用了就不碰。
struct Reaper {
    pgid: i32,
    started: String,
}

impl Reaper {
    fn new(pgid: i32) -> Self {
        Self {
            pgid,
            started: ps_lstart(pgid as u32),
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let now = ps_lstart(self.pgid as u32);
        let leader_is_ours = !self.started.is_empty() && now == self.started;
        let leaderless_group = now.is_empty() && !group_dead(self.pgid);
        if leader_is_ours || leaderless_group {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// 一个测试自己的普通子进程，测试结束（含 panic）时杀掉并回收。
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---- 可注入的检查器：真 ps，外加「声称某个端口由引擎持有」与故障注入 ----

struct TestInspector {
    real: SystemInspector,
    /// 对这些端口声称「被问到的 pid 正在监听」（假引擎脚本不监听，监听的是测试进程里的 wiremock）。
    claim_ports: Vec<u16>,
    fail_command: AtomicBool,
    /// 假装 pid 被复用：启动时间读出另一个值。
    reuse_pid: AtomicBool,
}

impl TestInspector {
    fn new(claim_port: Option<u16>) -> Arc<Self> {
        Self::claiming(&claim_port.into_iter().collect::<Vec<_>>())
    }

    fn claiming(ports: &[u16]) -> Arc<Self> {
        Arc::new(Self {
            real: SystemInspector::new(),
            claim_ports: ports.to_vec(),
            fail_command: AtomicBool::new(false),
            reuse_pid: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl ProcessInspector for TestInspector {
    fn pid_state(&self, pid: u32) -> PidState {
        self.real.pid_state(pid)
    }
    async fn start_time(&self, pid: u32) -> Result<String, InspectError> {
        let real = self.real.start_time(pid).await?;
        Ok(if self.reuse_pid.load(Ordering::SeqCst) {
            format!("{real} (reused)")
        } else {
            real
        })
    }
    async fn command(&self, pid: u32) -> Result<String, InspectError> {
        if self.fail_command.load(Ordering::SeqCst) {
            return Err(InspectError::TimedOut);
        }
        self.real.command(pid).await
    }
    async fn process_group(&self, pid: u32) -> Result<u32, InspectError> {
        self.real.process_group(pid).await
    }
    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
        if self.claim_ports.contains(&port) {
            return Ok(true);
        }
        self.real.listens_on(pid, port).await
    }
}

fn dyn_inspector(inspector: &Arc<TestInspector>) -> Arc<dyn ProcessInspector> {
    inspector.clone()
}

/// 拉起、等就绪、封印：一个可以被下一个 iDoris 接管的引擎。
async fn sealed_engine(
    rig: &Rig,
    instance: &EngineInstance,
    fake: &Fake,
    key: &EngineKey,
    stop: StopOnExit,
    inspector: &Arc<TestInspector>,
) -> OwnedEngine {
    let mut engine = OwnedEngine::spawn(
        rig.launch(instance, &fake.exe, key, stop),
        dyn_inspector(inspector),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    rig.reap_on_drop(&engine);
    fake.wait_started();
    engine.wait_ready(&fast_ready()).await.unwrap();
    engine.seal().await.unwrap();
    engine
}

// ---------------------------------------------------------------------------------------------
// 拉起、就绪、封印、停止（假的第二种引擎）
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_spawned_engine_is_recorded_ready_sealed_and_stopped_with_its_whole_group() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let store = instance.store();

    let mut engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        dyn_inspector(&inspector),
        LAUNCHED_BY,
    )
    .await
    .unwrap();

    // 拉起后立刻就有一份未封印的记录，写明引擎种类（字段 `engine_kind`）。
    let record = store.read().unwrap().unwrap();
    assert_eq!(record.engine_kind.as_str(), FAKE_KIND);
    assert_eq!(record.pid, engine.pid());
    assert_eq!(
        ps_pgid(record.pid),
        record.pid.to_string(),
        "the engine leads its own process group, so the group id is the pid and is not stored"
    );
    assert_eq!(record.port, port);
    assert_eq!(record.command, None);
    assert_eq!(record.api_key.as_ref(), Some(&key));
    assert_eq!(record.executable, fake.exe);
    assert_eq!(record.launched_by_instance, LAUNCHED_BY);
    assert_eq!(record.started_at, ps_lstart(record.pid));
    assert!(!engine.is_sealed());
    // 规范 §2.7.6 的布局：记录 `engines-owned/<engine_kind>/<endpoint 编码>.json`，日志 `logs/<engine_kind>.log`。
    let dir = rig.record_dir(FAKE_KIND);
    let record_file = dir.join(format!("http%3A%2F%2F127%2E0%2E0%2E1%3A{port}.json"));
    assert_eq!(store.path(), record_file);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&record_file), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&rig.state.path().join("engines-owned")), 0o700);
    assert_eq!(mode(&rig.logs_dir().join("fakeserve.log")), 0o600);
    assert_eq!(mode(&rig.logs_dir()), 0o700);

    // 就绪探测走配方自己的路径（`/health`），带的是这个引擎的私有 key（假引擎只认它）。
    fake.wait_started();
    engine.wait_ready(&fast_ready()).await.unwrap();

    // 封印：命令行取自此刻的进程，其余不变。
    engine.seal().await.unwrap();
    let sealed = store.read().unwrap().unwrap();
    let expected = ps_command(engine.pid());
    assert!(expected.contains("fake-server"), "{expected}");
    assert_eq!(sealed.command.as_deref(), Some(expected.as_str()));
    assert_eq!(sealed.pid, record.pid);
    assert_eq!(sealed.started_at, record.started_at);
    assert_eq!(sealed.engine_kind.as_str(), FAKE_KIND);
    assert!(engine.is_sealed());
    engine.seal().await.unwrap(); // idempotent

    // SIGTERM 整组：主进程与它的 `sleep` 子进程都没了，记录删了。
    let (leader, child) = fake.pids();
    assert_eq!(leader as u32, engine.pid());
    assert!(pid_alive(child));
    engine.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
    assert!(!pid_alive(leader) && !pid_alive(child));
    assert_eq!(store.read().unwrap(), None);
}

#[tokio::test]
async fn a_spawned_engine_that_ignores_sigterm_is_killed_whole_with_its_forked_children() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let fake = rig.fake("fake-server", Behavior::IgnoresTerm);
    let instance = rig.fake_instance(port);
    let engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    let (leader, child) = fake.pids();
    // 它真的忽略了 SIGTERM：宽限期里整组都还在。
    let started = Instant::now();
    engine.stop(Duration::from_secs(1)).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "TERM was ignored, so the grace period had to run out first"
    );
    // SIGKILL 打的是整组：forked 出来的 `sleep 600`（同样忽略 TERM）也没了。
    wait_group_dead(leader);
    assert!(!pid_alive(leader), "leader");
    assert!(!pid_alive(child), "the forked child dies with the group");
    assert_eq!(instance.store().read().unwrap(), None);
}

#[tokio::test]
async fn dropping_an_owned_engine_kills_its_whole_process_group() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let instance = rig.fake_instance(port);
    let engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    let (leader, child) = fake.pids();
    drop(engine);
    wait_group_dead(leader);
    assert!(!pid_alive(leader) && !pid_alive(child));
}

#[tokio::test]
async fn with_stop_on_exit_never_dropping_the_engine_leaves_it_and_its_record_running() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &instance, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, child) = fake.pids();

    drop(engine);

    // Give a (wrong) Drop time to act; the engine and its record must be untouched.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(pid_alive(leader), "the engine outlives its handle");
    assert!(pid_alive(child));
    let record = instance.store().read().unwrap().unwrap();
    assert!(
        record.command.is_some(),
        "the sealed record stays for the next iDoris"
    );
}

// ---------------------------------------------------------------------------------------------
// 启动途中的失败
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_engine_that_dies_while_starting_is_reported_without_waiting_out_the_timeout() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let fake = rig.fake("fake-server", Behavior::CrashesSoon);
    let instance = rig.fake_instance(port);
    let mut engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    let started = Instant::now();
    let error = engine.wait_ready(&fast_ready()).await.unwrap_err();
    assert!(matches!(error, EngineError::ExitedEarly), "{error}");
    assert_eq!(error.reason_code(), "engine_exited");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the exit is noticed at once; the 60 s ready timeout is not waited out"
    );
    // Cleaning up after the crash removes the record.
    engine.stop(Duration::from_secs(5)).await.unwrap();
    assert_eq!(instance.store().read().unwrap(), None);
}

#[tokio::test]
async fn an_engine_that_never_answers_times_out_with_the_last_probe_outcome() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port(); // nothing listens here
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let instance = rig.fake_instance(port);
    let mut engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    fake.wait_started();
    let policy = ReadyPolicy {
        timeout: Duration::from_millis(600),
        ..fast_ready()
    };
    let error = engine.wait_ready(&policy).await.unwrap_err();
    assert!(
        matches!(
            error,
            EngineError::NotReady {
                last: Some(EngineProbe::ConnectionRefused)
            }
        ),
        "{error}"
    );
    assert_eq!(error.reason_code(), "startup_timeout");
}

#[tokio::test]
async fn a_missing_executable_fails_cleanly_and_leaves_no_record() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let instance = rig.fake_instance(port);
    let error = OwnedEngine::spawn(
        rig.launch(
            &instance,
            &rig.work.path().join("no-such-engine"),
            &key,
            StopOnExit::Owned,
        ),
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, EngineError::Spawn(std::io::ErrorKind::NotFound)),
        "{error}"
    );
    assert_eq!(instance.store().read().unwrap(), None);
}

#[tokio::test]
async fn another_server_answering_on_the_port_is_not_mistaken_for_the_engine() {
    if !require_lsof("another_server_answering_on_the_port_is_not_mistaken_for_the_engine") {
        return;
    }
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    // The mock stands in for some other server of this kind that holds the port and answers
    // the probe; the spawned fake never binds it (real `lsof`, no claim for the port).
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let instance = rig.fake_instance(port);
    let mut engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        Arc::new(SystemInspector::new()),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    fake.wait_started();
    engine.wait_ready(&fast_ready()).await.unwrap();
    let error = engine.seal().await.unwrap_err();
    assert!(matches!(error, EngineError::PortNotOwned), "{error}");
    assert_eq!(error.reason_code(), "port_in_use");
    assert!(!engine.is_sealed());
    assert_eq!(
        instance.store().read().unwrap().unwrap().command,
        None,
        "an engine that does not hold its port is never made adoptable"
    );
}

// ---------------------------------------------------------------------------------------------
// 孤儿接管
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_sealed_engine_survives_idoris_and_is_adopted_then_stopped_by_the_next_instance() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &instance, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, child) = fake.pids();
    drop(engine); // iDoris goes away; the engine stays

    let recovery = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap();
    let Recovery::Adopted(mut adopted) = recovery else {
        panic!("expected Adopted, got {recovery:?}");
    };
    assert_eq!(adopted.pid(), leader as u32);
    assert_eq!(adopted.port(), port, "the port comes back from the record");
    assert_eq!(adopted.endpoint(), endpoint(port));
    assert_eq!(
        adopted.api_key(),
        Some(&key),
        "the record's key comes back with the engine"
    );
    assert!(adopted.is_sealed());
    assert!(adopted.is_running().await.unwrap());
    adopted.wait_ready(&fast_ready()).await.unwrap();

    adopted.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
    assert!(!pid_alive(leader) && !pid_alive(child));
    assert_eq!(instance.store().read().unwrap(), None);
}

#[tokio::test]
async fn an_adopted_engine_that_ignores_sigterm_is_killed_whole_by_process_group() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::IgnoresTerm);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &instance, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, child) = fake.pids();
    drop(engine);

    let Recovery::Adopted(adopted) = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap()
    else {
        panic!("expected Adopted");
    };
    let started = Instant::now();
    adopted.stop(Duration::from_secs(1)).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "TERM was ignored"
    );
    wait_group_dead(leader);
    assert!(!pid_alive(leader), "leader");
    assert!(
        !pid_alive(child),
        "SIGKILL went to the whole group, not just the leader"
    );
    assert_eq!(instance.store().read().unwrap(), None);
}

#[tokio::test]
async fn an_engine_killed_while_starting_is_found_unsealed_and_can_only_be_stopped() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(None);
    let instance = rig.fake_instance(port);
    // The previous iDoris spawned the engine and died before it became ready: no seal.
    let engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Never),
        dyn_inspector(&inspector),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    rig.reap_on_drop(&engine);
    let (leader, child) = fake.pids();
    drop(engine);

    let recovery = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap();
    let Recovery::Starting(mut starting) = recovery else {
        panic!("expected Starting, got {recovery:?}");
    };
    assert!(!starting.is_sealed());
    let error = starting.seal().await.unwrap_err();
    assert!(matches!(error, EngineError::CannotSeal), "{error}");

    starting.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
    assert!(!pid_alive(leader) && !pid_alive(child));
    assert_eq!(instance.store().read().unwrap(), None);
}

#[tokio::test]
async fn an_identity_that_cannot_be_read_signals_nothing_and_keeps_the_record() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &instance, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, _) = fake.pids();
    drop(engine);

    inspector.fail_command.store(true, Ordering::SeqCst);
    let recovery = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap();
    assert!(
        matches!(recovery, Recovery::Unverifiable(UnreadableReason::Command)),
        "{recovery:?}"
    );
    assert!(
        pid_alive(leader),
        "an unreadable identity is not a reason to kill"
    );
    assert!(
        instance.store().read().unwrap().is_some(),
        "and not a reason to forget the engine either"
    );

    // The next look, with the tool back, adopts it.
    inspector.fail_command.store(false, Ordering::SeqCst);
    let recovery = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap();
    let Recovery::Adopted(adopted) = recovery else {
        panic!("expected Adopted, got {recovery:?}");
    };
    adopted.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
}

#[tokio::test]
async fn if_the_pid_is_reused_after_adoption_the_stop_sends_no_signal() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &instance, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, child) = fake.pids();
    drop(engine);
    let Recovery::Adopted(adopted) = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap()
    else {
        panic!("expected Adopted");
    };

    // The pid now reads as a different process (reused): the signal gate must hold even though
    // the process is, in this test, in fact still the engine.
    inspector.reuse_pid.store(true, Ordering::SeqCst);
    adopted.stop(Duration::from_millis(200)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        pid_alive(leader),
        "no signal went to a pid whose identity no longer matches"
    );
    assert!(pid_alive(child));
}

#[tokio::test]
async fn a_record_naming_an_unrelated_live_process_never_signals_it_and_is_dropped() {
    let rig = Rig::new();
    let port = free_port();
    let instance = rig.fake_instance(port);
    let store = instance.store();
    let inspector = TestInspector::new(None);

    // An unrelated process (pid reuse stand-in): its own group, so nothing below can reach this test.
    let mut unrelated = std::process::Command::new("/bin/sleep");
    unrelated.arg("600");
    std::os::unix::process::CommandExt::process_group(&mut unrelated, 0);
    let mut unrelated = KillOnDrop(unrelated.spawn().unwrap());
    let pid = unrelated.0.id();
    let real_start = ps_lstart(pid);
    let real_command = ps_command(pid);

    let forged = |tweak: &dyn Fn(&mut OwnershipRecord)| {
        let mut record = OwnershipRecord {
            engine_kind: kind(FAKE_KIND),
            pid,
            port,
            started_at: real_start.clone(),
            command: Some(real_command.clone()),
            executable: "/opt/engines/fake/bin/fake-server".into(),
            api_key: Some(EngineKey::generate().unwrap()),
            launched_by_instance: LAUNCHED_BY.into(),
        };
        tweak(&mut record);
        record
    };
    // Each forgery is wrong in exactly one way; the process survives every one of them.
    let cases: Vec<(&str, OwnershipRecord, MismatchReason)> = vec![
        (
            "other start time (the classic reused pid)",
            forged(&|r| r.started_at = "Mon Jan  1 00:00:00 2001".into()),
            MismatchReason::StartTime,
        ),
        (
            "other command",
            forged(&|r| r.command = Some("fake-server --serve".into())),
            MismatchReason::Command,
        ),
        (
            "not listening on the recorded port",
            forged(&|_| {}),
            MismatchReason::Port,
        ),
    ];
    for (name, record, reason) in cases {
        if reason == MismatchReason::Port && !lsof_available() {
            eprintln!("skipping the port forgery: lsof is not installed");
            continue;
        }
        store.write(&record).unwrap();
        let recovery = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
            .await
            .unwrap();
        assert!(
            matches!(recovery, Recovery::Stale(r) if r == reason),
            "{name}: {recovery:?}"
        );
        assert!(
            pid_alive(pid as i32),
            "{name}: the unrelated process was signalled"
        );
        assert_eq!(store.read().unwrap(), None, "{name}: record dropped");
    }

    // A pid with nothing behind it is Stale too, and nothing is signalled.
    unrelated.0.kill().unwrap();
    unrelated.0.wait().unwrap();
    store.write(&forged(&|_| {})).unwrap();
    let recovery = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap();
    assert!(
        matches!(recovery, Recovery::Stale(MismatchReason::ProcessGone)),
        "{recovery:?}"
    );
}

#[tokio::test]
async fn spawning_never_overwrites_the_record_of_an_engine_that_may_still_be_running() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &instance, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, _) = fake.pids();
    drop(engine);
    let before = instance.store().read().unwrap().unwrap();

    let second = rig.fake("fake-server-2", Behavior::Obedient);
    let error = OwnedEngine::spawn(
        rig.launch(
            &instance,
            &second.exe,
            &EngineKey::generate().unwrap(),
            StopOnExit::Owned,
        ),
        dyn_inspector(&inspector),
        LAUNCHED_BY,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, EngineError::AlreadyOwned), "{error}");
    assert_eq!(error.reason_code(), "engine_already_owned");
    assert!(pid_alive(leader), "the live engine is untouched");
    assert_eq!(instance.store().read().unwrap().unwrap(), before);
    assert!(
        !second.started.exists(),
        "the second engine was never started"
    );
}

#[tokio::test]
async fn a_stale_record_is_replaced_when_a_new_engine_is_spawned() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let instance = rig.fake_instance(port);
    let store = instance.store();
    let mut dead = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    store
        .write(&OwnershipRecord {
            engine_kind: kind(FAKE_KIND),
            pid: dead_pid,
            port,
            started_at: "Mon Jan  1 00:00:00 2001".into(),
            command: Some("fake-server".into()),
            executable: "/opt/engines/fake/bin/fake-server".into(),
            api_key: Some(EngineKey::generate().unwrap()),
            launched_by_instance: LAUNCHED_BY.into(),
        })
        .unwrap();

    let fake = rig.fake("fake-server", Behavior::Obedient);
    let engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Owned),
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    let record = store.read().unwrap().unwrap();
    assert_eq!(record.pid, engine.pid());
    assert_eq!(record.api_key.as_ref(), Some(&key));
}

// ---------------------------------------------------------------------------------------------
// 真实的四项身份（真 ps、真 lsof、一个真的监听进程）
// ---------------------------------------------------------------------------------------------

/// 一个真的监听 `port` 的假引擎脚本（perl，`exec` 之后进程还是同一个 pid）；监听成功后创建 `started`。
fn perl_listener(rig: &Rig, name: &str, port: u16, started: &Path) -> PathBuf {
    rig.script(
        name,
        &format!(
            "exec /usr/bin/perl -MIO::Socket::INET -e 'my $s = IO::Socket::INET->new(Listen => 5, LocalAddr => \"127.0.0.1\", LocalPort => $ARGV[0], ReuseAddr => 1) or die \"listen: $!\"; open(my $f, \">\", $ARGV[1]) or die; close $f; sleep 600' {port} '{}'",
            started.display()
        ),
    )
}

#[tokio::test]
async fn the_four_identity_items_are_each_required_against_real_ps_and_lsof() {
    let test = "the_four_identity_items_are_each_required_against_real_ps_and_lsof";
    if !require_lsof(test) {
        return;
    }
    if !Path::new("/usr/bin/perl").is_file() {
        eprintln!("skipping {test}: /usr/bin/perl is not installed");
        return;
    }
    let inspector = SystemInspector::new();
    let rig = Rig::new();
    let port = free_port();
    let started = rig.work.path().join("listening");
    // The subject is a real engine-shaped process: alive, leader of its own group, with a start
    // time and a command, and really listening on `port`. `verify_identity` only reads;
    // nothing here is signalled.
    let exe = perl_listener(&rig, "subject", port, &started);
    let mut leader = std::process::Command::new(&exe);
    leader.env_clear();
    std::os::unix::process::CommandExt::process_group(&mut leader, 0);
    let leader = KillOnDrop(leader.isolate_state().spawn().unwrap());
    let pid = leader.0.id();
    wait_until("the subject to listen", || started.exists());
    let record = OwnershipRecord {
        engine_kind: kind(FAKE_KIND),
        pid,
        port,
        started_at: inspector.start_time(pid).await.unwrap(),
        command: Some(inspector.command(pid).await.unwrap()),
        executable: "/opt/engines/fake/bin/fake-server".into(),
        api_key: Some(EngineKey::generate().unwrap()),
        launched_by_instance: LAUNCHED_BY.into(),
    };
    assert_eq!(
        verify_identity(&inspector, &record).await,
        IdentityVerdict::Match
    );

    // A live process that does not lead a group of its own (it shares this test's group):
    // everything else about it can match, and the engine's group would still not be its group.
    let member = KillOnDrop(
        std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .unwrap(),
    );
    let member_pid = member.0.id();
    assert_ne!(ps_pgid(member_pid), member_pid.to_string());

    let spare = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cases: Vec<(&str, OwnershipRecord, IdentityVerdict)> = vec![
        (
            "start time",
            OwnershipRecord {
                started_at: "Mon Jan  1 00:00:00 2001".into(),
                ..record.clone()
            },
            IdentityVerdict::Mismatch(MismatchReason::StartTime),
        ),
        (
            "command",
            OwnershipRecord {
                command: Some("fake-server --serve".into()),
                ..record.clone()
            },
            IdentityVerdict::Mismatch(MismatchReason::Command),
        ),
        (
            "port",
            OwnershipRecord {
                port: spare,
                ..record.clone()
            },
            IdentityVerdict::Mismatch(MismatchReason::Port),
        ),
        (
            "process group: not the leader of its own group",
            OwnershipRecord {
                pid: member_pid,
                started_at: ps_lstart(member_pid),
                command: Some(ps_command(member_pid)),
                ..record.clone()
            },
            IdentityVerdict::Mismatch(MismatchReason::ProcessGroup),
        ),
        (
            "unsealed: no command to compare, no port to check",
            OwnershipRecord {
                command: None,
                port: spare,
                ..record.clone()
            },
            IdentityVerdict::Unsealed,
        ),
    ];
    for (name, tweaked, expected) in cases {
        assert_eq!(
            verify_identity(&inspector, &tweaked).await,
            expected,
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_real_listening_engine_is_sealed_adopted_and_stopped_against_real_ps_and_lsof() {
    let test = "a_real_listening_engine_is_sealed_adopted_and_stopped_against_real_ps_and_lsof";
    if !require_lsof(test) {
        return;
    }
    if !Path::new("/usr/bin/perl").is_file() {
        eprintln!("skipping {test}: /usr/bin/perl is not installed");
        return;
    }
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let started = rig.work.path().join("listening");
    // The fake engine really binds the port (perl, which `exec` makes the process: same pid).
    let exe = perl_listener(&rig, "fake-server", port, &started);
    let real: Arc<dyn ProcessInspector> = Arc::new(SystemInspector::new());
    let instance = rig.fake_instance(port);
    let mut engine = OwnedEngine::spawn(
        rig.launch(&instance, &exe, &key, StopOnExit::Never),
        Arc::clone(&real),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    let pid = engine.pid() as i32;
    let _cleanup = Reaper::new(pid);
    wait_until("the fake engine to listen", || started.exists());

    engine.seal().await.unwrap(); // real lsof: the listener on `port` is this pid
    let sealed = instance.store().read().unwrap().unwrap();
    assert!(
        sealed
            .command
            .as_deref()
            .is_some_and(|c| c.contains("perl"))
    );
    drop(engine); // iDoris is killed; stop_on_exit never leaves the engine

    let recovery = OwnedEngine::recover(&instance, &real).await.unwrap();
    let Recovery::Adopted(adopted) = recovery else {
        panic!("expected Adopted, got {recovery:?}");
    };
    assert_eq!(adopted.pid() as i32, pid);
    adopted.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(pid);
    assert!(!pid_alive(pid));
    assert_eq!(instance.store().read().unwrap(), None);
}

// ---------------------------------------------------------------------------------------------
// 环境、argv、日志
// ---------------------------------------------------------------------------------------------

/// 拉起一个把自己看到的 argv、环境、工作目录写进文件的假引擎，读回来。
struct LaunchDump {
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    /// 保持引擎活着直到测试结束（`Owned`：Drop 时杀整组）。
    _engine: OwnedEngine,
}

async fn dump_launch(
    rig: &Rig,
    instance: &EngineInstance,
    key: Option<&EngineKey>,
    budget: Option<u64>,
    parent_env: EnvVars,
) -> LaunchDump {
    let w = rig.work.path();
    let (env_dump, args_dump, cwd_dump, started) = (
        w.join("env.txt"),
        w.join("args.txt"),
        w.join("cwd.txt"),
        w.join("started"),
    );
    let exe = rig.script(
        "engine-under-test",
        &format!(
            "env | sort > '{}'\nprintf '%s\\n' \"$@\" > '{}'\npwd -P > '{}'\n: > '{}'\nsleep 600 &\nwait",
            env_dump.display(),
            args_dump.display(),
            cwd_dump.display(),
            started.display()
        ),
    );
    let launch = rig.launch_with(
        instance,
        &exe,
        LaunchOpts {
            key,
            stop: StopOnExit::Owned,
            budget,
            parent_env,
        },
    );
    let engine = OwnedEngine::spawn(
        launch,
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    wait_until("the engine to start", || started.exists());
    let env = std::fs::read_to_string(&env_dump)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    LaunchDump {
        args: std::fs::read_to_string(&args_dump)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect(),
        env,
        cwd: std::fs::read_to_string(&cwd_dump)
            .unwrap()
            .trim()
            .to_owned(),
        _engine: engine,
    }
}

/// 父进程环境：这个测试进程真实的（任何机器上都有 CARGO_*、PATH……），加上绝不能进引擎的东西。
type EnvVars = Vec<(OsString, OsString)>;
type Planted = [(&'static str, &'static str); 11];

fn parent_env_with_planted() -> (EnvVars, Planted) {
    let planted = [
        ("IDORIS_OMLX_API_KEY", "planted-omlx-key"),
        ("IDORIS_ADMIN_TOKEN", "planted-admin-token"),
        ("IDORIS_DB_PATH", "/planted/idoris.sqlite3"),
        ("IDORIS_STATE_DIR", "/planted/state"),
        ("OMLX_API_KEY", "planted-user-key"),
        ("OMLX_BASE_PATH", "/planted/omlx-base"),
        ("FAKE_ENGINE_TOKEN", "planted-fake-token"),
        ("AWS_SECRET_ACCESS_KEY", "planted-aws"),
        ("DYLD_INSERT_LIBRARIES", "/planted/evil.dylib"),
        ("HOME", "/planted/home"),
        ("TZ", "Asia/Bangkok"),
    ];
    let env = std::env::vars_os()
        .filter(|(name, _)| !planted.iter().any(|(n, _)| name.to_str() == Some(*n)))
        .chain(planted.map(|(n, v)| (OsString::from(n), OsString::from(v))))
        .collect();
    (env, planted)
}

/// 引擎环境里允许出现的名字：通用白名单、各配方的专属变量，加上 `sh` 与 `env` 自己加的。
fn allowed_names(recipe_specific: &[&'static str]) -> BTreeSet<&'static str> {
    let mut allowed: BTreeSet<&'static str> = [
        "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
        // what `sh` and `env` add themselves
        "PWD", "OLDPWD", "SHLVL", "_",
    ]
    .into();
    allowed.extend(recipe_specific.iter().copied());
    allowed
}

#[tokio::test]
async fn the_engine_sees_only_the_allowlisted_environment_and_what_its_recipe_asked_for() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let instance = rig.fake_instance(port);
    let (parent_env, planted) = parent_env_with_planted();
    let dump = dump_launch(&rig, &instance, Some(&key), None, parent_env).await;

    // argv: whatever the *recipe* chose (this is not oMLX's command line), and the key is not on it.
    assert_eq!(
        dump.args,
        [
            "--serve",
            "--bind",
            "127.0.0.1",
            "--listen-port",
            port.to_string().as_str()
        ]
    );
    assert!(dump.args.iter().all(|a| !a.contains(key.expose())));

    // environment: the generic allowlist plus the variable the recipe asked for, nothing else.
    let seen: BTreeSet<&str> = dump.env.keys().map(String::as_str).collect();
    let allowed = allowed_names(&["FAKE_ENGINE_TOKEN"]);
    let leaked: Vec<_> = seen.difference(&allowed).collect();
    assert!(
        leaked.is_empty(),
        "variables that must not reach the engine: {leaked:?}"
    );
    let raw: String = dump.env.values().cloned().collect::<Vec<_>>().join("\n");
    for (name, secret) in planted {
        if matches!(name, "HOME" | "TZ" | "FAKE_ENGINE_TOKEN") {
            continue; // allowlisted or recipe-set: checked below
        }
        assert!(!raw.contains(secret), "the value of {name} leaked");
        assert!(!seen.contains(name), "{name} leaked");
    }
    assert_eq!(
        dump.env.get("FAKE_ENGINE_TOKEN").map(String::as_str),
        Some(key.expose()),
        "the engine's own key, not the planted one from the parent"
    );
    assert_eq!(
        dump.env.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin:/usr/sbin:/sbin")
    );
    assert_eq!(
        dump.env.get("HOME").map(String::as_str),
        Some("/planted/home"),
        "HOME is allowlisted"
    );
    assert_eq!(
        dump.env.get("TZ").map(String::as_str),
        Some("Asia/Bangkok"),
        "TZ is allowlisted"
    );

    // cwd: the state directory itself (spec 2.7.6).
    assert_eq!(
        dump.cwd,
        std::fs::canonicalize(rig.state.path())
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[tokio::test]
async fn the_omlx_engine_gets_the_designed_argv_and_environment() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let omlx = rig.omlx_recipe().with_model_dir("/Volumes/models");
    let instance = rig.instance(Arc::new(omlx), port);
    let (parent_env, planted) = parent_env_with_planted();
    let dump = dump_launch(
        &rig,
        &instance,
        Some(&key),
        Some(17_179_869_184), // 16 GiB
        parent_env,
    )
    .await;

    assert_eq!(
        dump.args,
        [
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            port.to_string().as_str(),
            "--memory-guard-gb",
            "16",
            "--model-dir",
            "/Volumes/models"
        ]
    );
    assert!(dump.args.iter().all(|a| !a.contains(key.expose())));
    assert!(!dump.args.iter().any(|a| a == "--api-key"));

    let seen: BTreeSet<&str> = dump.env.keys().map(String::as_str).collect();
    let allowed = allowed_names(&["OMLX_API_KEY", "OMLX_BASE_PATH"]);
    let leaked: Vec<_> = seen.difference(&allowed).collect();
    assert!(
        leaked.is_empty(),
        "variables that must not reach the engine: {leaked:?}"
    );
    let raw: String = dump.env.values().cloned().collect::<Vec<_>>().join("\n");
    for (name, secret) in planted {
        if matches!(name, "HOME" | "TZ" | "OMLX_API_KEY" | "OMLX_BASE_PATH") {
            continue;
        }
        assert!(!raw.contains(secret), "the value of {name} leaked");
    }
    assert_eq!(
        dump.env.get("OMLX_API_KEY").map(String::as_str),
        Some(key.expose()),
        "the private key, not the user's own OMLX_API_KEY"
    );
    // oMLX writes its command-line settings into its base: the engine gets iDoris's own, under the
    // state directory, not the parent's `OMLX_BASE_PATH` (`/planted/omlx-base`) and nothing derived
    // from the parent's `HOME` (`/planted/home`).
    assert_eq!(
        dump.env.get("OMLX_BASE_PATH").map(PathBuf::from),
        Some(rig.state.path().join("omlx-base")),
        "iDoris's own base, not the parent's"
    );
    assert_eq!(
        std::fs::metadata(rig.state.path().join("omlx-base"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        dump.cwd,
        std::fs::canonicalize(rig.state.path())
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[tokio::test]
async fn engine_output_lands_in_a_private_log_file_named_after_the_engine_kind() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let port = free_port();
    let started = rig.work.path().join("started");
    let exe = rig.script(
        "fake-server",
        &format!(
            "echo 'to stdout'\necho 'to stderr' >&2\n: > '{}'\nsleep 600 &\nwait",
            started.display()
        ),
    );
    let instance = rig.fake_instance(port);
    let launch = rig.launch(&instance, &exe, &key, StopOnExit::Owned);
    let log_path = launch.log().path().to_owned();
    assert_eq!(log_path, rig.logs_dir().join("fakeserve.log"));
    let engine = OwnedEngine::spawn(
        launch,
        dyn_inspector(&TestInspector::new(None)),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    wait_until("the fake engine to start", || started.exists());

    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("to stdout") && log.contains("to stderr"),
        "{log:?}"
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&log_path), 0o600);
    assert_eq!(mode(log_path.parent().unwrap()), 0o700);
    drop(engine);
}

// ---------------------------------------------------------------------------------------------
// oMLX 自己的配方：整条生命周期，经它的 `/v1/models` 就绪探测
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_omlx_engine_runs_the_whole_lifecycle_through_its_own_readiness_probe() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = omlx_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("omlx", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    let instance = rig.instance(Arc::new(rig.omlx_recipe()), port);

    let mut engine = OwnedEngine::spawn(
        rig.launch(&instance, &fake.exe, &key, StopOnExit::Never),
        dyn_inspector(&inspector),
        LAUNCHED_BY,
    )
    .await
    .unwrap();
    rig.reap_on_drop(&engine);
    let (leader, _) = fake.pids();
    // The record names the engine kind (the spec's reserved `omlx`) and sits where the spec puts it.
    let record = instance.store().read().unwrap().unwrap();
    assert_eq!(record.engine_kind, EngineKind::OMLX);
    assert_eq!(record.engine_kind.as_str(), "omlx");
    assert_eq!(
        instance.store().path(),
        rig.record_dir("omlx")
            .join(format!("http%3A%2F%2F127%2E0%2E0%2E1%3A{port}.json"))
    );
    assert!(rig.logs_dir().join("omlx.log").is_file());

    // Ready means `GET /v1/models` with the engine's key (the mock answers nothing else).
    engine.wait_ready(&fast_ready()).await.unwrap();
    engine.seal().await.unwrap();
    drop(engine);

    let Recovery::Adopted(adopted) = OwnedEngine::recover(&instance, &dyn_inspector(&inspector))
        .await
        .unwrap()
    else {
        panic!("expected Adopted");
    };
    assert_eq!(adopted.instance().kind(), &EngineKind::OMLX);
    adopted.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
    assert_eq!(instance.store().read().unwrap(), None);
}

// ---------------------------------------------------------------------------------------------
// 多个引擎并存：按种类分目录的记录、按种类命名的日志（规范 §2.7.6）
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn engines_of_different_kinds_and_two_of_one_kind_live_side_by_side() {
    let rig = Rig::new();
    let omlx_key = EngineKey::generate().unwrap();
    let fake_key = EngineKey::generate().unwrap();
    let fake_too_key = EngineKey::generate().unwrap();
    let omlx_mock = omlx_http(&omlx_key).await;
    let fake_mock = fake_http(&fake_key).await;
    let fake_too_mock = fake_http(&fake_too_key).await;
    let omlx_port = omlx_mock.address().port();
    let fake_port = fake_mock.address().port();
    let fake_too_port = fake_too_mock.address().port();
    let omlx_script = rig.fake("omlx", Behavior::Obedient);
    let fake_script = rig.fake("fake-server", Behavior::Obedient);
    let fake_too_script = rig.fake("fake-server-2", Behavior::Obedient);

    let omlx = rig.instance(Arc::new(rig.omlx_recipe()), omlx_port);
    let fake = rig.fake_instance(fake_port);
    let fake_too = rig.fake_instance(fake_too_port); // the same kind as `fake`, another endpoint

    // One inspector claims all three ports: the scripts do not listen, the mocks do.
    let inspector: Arc<dyn ProcessInspector> = dyn_inspector(&TestInspector::claiming(&[
        omlx_port,
        fake_port,
        fake_too_port,
    ]));

    let mut engines = Vec::new();
    for (instance, script, key) in [
        (&omlx, &omlx_script, &omlx_key),
        (&fake, &fake_script, &fake_key),
        (&fake_too, &fake_too_script, &fake_too_key),
    ] {
        let mut engine = OwnedEngine::spawn(
            rig.launch(instance, &script.exe, key, StopOnExit::Owned),
            Arc::clone(&inspector),
            LAUNCHED_BY,
        )
        .await
        .unwrap();
        script.wait_started();
        engine.wait_ready(&fast_ready()).await.unwrap();
        engine.seal().await.unwrap();
        engines.push(engine);
    }

    // Records: one directory per kind, one file per endpoint; nothing shared, nothing overwritten.
    let list = |dir: &Path| -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    let owned_dir = rig.state.path().join("engines-owned");
    assert_eq!(list(&owned_dir), ["fakeserve", "omlx"]);
    let file = |port: u16| format!("http%3A%2F%2F127%2E0%2E0%2E1%3A{port}.json");
    let mut fake_files = vec![file(fake_port), file(fake_too_port)];
    fake_files.sort();
    assert_eq!(list(&rig.record_dir("fakeserve")), fake_files);
    assert_eq!(list(&rig.record_dir("omlx")), [file(omlx_port)]);
    let kinds: Vec<String> = [&omlx, &fake, &fake_too]
        .iter()
        .map(|i| {
            i.store()
                .read()
                .unwrap()
                .unwrap()
                .engine_kind
                .as_str()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["omlx", "fakeserve", "fakeserve"]);
    let mut fake_ports = vec![fake_port, fake_too_port];
    fake_ports.sort_unstable();
    assert_eq!(
        recorded_ports(rig.state.path(), &kind("fakeserve")).unwrap(),
        fake_ports
    );
    assert_eq!(
        recorded_ports(rig.state.path(), &EngineKind::OMLX).unwrap(),
        [omlx_port]
    );

    // Logs: one file per kind (the two fake instances share theirs).
    assert_eq!(list(&rig.logs_dir()), ["fakeserve.log", "omlx.log"]);

    let pids: BTreeSet<u32> = engines.iter().map(OwnedEngine::pid).collect();
    assert_eq!(pids.len(), 3);
    let (omlx_leader, _) = omlx_script.pids();
    let (fake_leader, _) = fake_script.pids();
    let (fake_too_leader, _) = fake_too_script.pids();

    // Stopping one engine touches only that one, not even its own kind's other instance.
    let fake_too_engine = engines.pop().unwrap();
    fake_too_engine.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(fake_too_leader);
    assert_eq!(fake_too.store().read().unwrap(), None);
    assert!(pid_alive(omlx_leader) && pid_alive(fake_leader));
    assert!(omlx.store().read().unwrap().is_some());
    assert!(fake.store().read().unwrap().is_some());
    assert_eq!(
        recorded_ports(rig.state.path(), &kind("fakeserve")).unwrap(),
        [fake_port]
    );
    drop(engines); // `Owned`: the rest are killed with their handles
    wait_group_dead(omlx_leader);
    wait_group_dead(fake_leader);
}

#[tokio::test]
async fn another_kind_on_the_same_endpoint_has_its_own_record_and_never_sees_or_touches_this_one() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(port));
    // A live engine of kind `fakeserve` on `port`, left by a previous iDoris.
    let theirs = rig.fake_instance(port);
    let engine = sealed_engine(&rig, &theirs, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, child) = fake.pids();
    drop(engine);
    let before = theirs.store().read().unwrap().unwrap();

    // The same endpoint, now configured as an oMLX engine: it looks in `engines-owned/omlx` and
    // finds nothing. It does not adopt, signal or drop the other kind's record.
    let ours = rig.instance(Arc::new(OmlxRecipe::new()), port);
    let recovery = OwnedEngine::recover(&ours, &dyn_inspector(&inspector))
        .await
        .unwrap();
    assert!(matches!(recovery, Recovery::NoRecord), "{recovery:?}");
    assert!(
        pid_alive(leader) && pid_alive(child),
        "nothing was signalled"
    );
    assert_eq!(
        theirs.store().read().unwrap().unwrap(),
        before,
        "the other kind's record is exactly as it was"
    );
    assert_ne!(ours.store().path(), theirs.store().path());

    // Its own kind still adopts and stops it.
    let Recovery::Adopted(adopted) = OwnedEngine::recover(&theirs, &dyn_inspector(&inspector))
        .await
        .unwrap()
    else {
        panic!("the kind that wrote the record adopts it");
    };
    adopted.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
    assert_eq!(theirs.store().read().unwrap(), None);
}

#[tokio::test]
async fn an_engine_left_on_a_port_nobody_configures_any_more_is_found_by_its_record_and_stopped() {
    let rig = Rig::new();
    let key = EngineKey::generate().unwrap();
    let http = fake_http(&key).await;
    let old_port = http.address().port();
    let fake = rig.fake("fake-server", Behavior::Obedient);
    let inspector = TestInspector::new(Some(old_port));
    let old = rig.fake_instance(old_port);
    // The previous iDoris ran the engine on `old_port` and went away (stop_on_exit: never).
    let engine = sealed_engine(&rig, &old, &fake, &key, StopOnExit::Never, &inspector).await;
    let (leader, child) = fake.pids();
    drop(engine);

    // The configuration now says another port: looking there finds nothing.
    let new_port = free_port();
    let configured = rig.fake_instance(new_port);
    let recovery = OwnedEngine::recover(&configured, &dyn_inspector(&inspector))
        .await
        .unwrap();
    assert!(matches!(recovery, Recovery::NoRecord), "{recovery:?}");

    // The kind's record directory still names the old port, so a manager can find and stop it.
    assert_eq!(
        recorded_ports(rig.state.path(), &kind(FAKE_KIND)).unwrap(),
        [old_port]
    );
    let found = rig.fake_instance(old_port);
    let Recovery::Adopted(adopted) = OwnedEngine::recover(&found, &dyn_inspector(&inspector))
        .await
        .unwrap()
    else {
        panic!("expected Adopted");
    };
    assert_eq!(adopted.port(), old_port);
    adopted.stop(Duration::from_secs(10)).await.unwrap();
    wait_group_dead(leader);
    assert!(!pid_alive(leader) && !pid_alive(child));
    assert_eq!(
        recorded_ports(rig.state.path(), &kind(FAKE_KIND)).unwrap(),
        Vec::<u16>::new()
    );
}

// ---------------------------------------------------------------------------------------------
// 发现：显式路径（来自 launcher 帧 `engine.executables[<engine_kind>]`）作为输入
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_recipe_from_outside_the_crate_discovers_through_the_explicit_path_input() {
    let rig = Rig::new();
    let engine = FakeEngine { kind: FAKE_KIND };
    let exe = rig.script("fake-server", "sleep 600");

    // No explicit path and a plan that looks nowhere: nothing found, and it says so.
    let error = engine.discover(None).await.unwrap_err();
    assert_eq!(error.reason_code(), "engine_not_found");

    // `engine.executables.fakeserve` from the launcher frame: used as given (canonical path, no
    // codesign involved), and never replaced by anything else when it is no good.
    let found = engine.discover(Some(&exe)).await.unwrap();
    assert_eq!(found.source, ExecutableSource::Explicit);
    assert_eq!(found.path, std::fs::canonicalize(&exe).unwrap());
    let missing = rig.work.path().join("no-such-engine");
    let error = engine.discover(Some(&missing)).await.unwrap_err();
    assert_eq!(error.reason_code(), "engine_not_found");
    let not_executable = rig.work.path().join("plain.txt");
    std::fs::write(&not_executable, "x").unwrap();
    assert!(engine.discover(Some(&not_executable)).await.is_err());
}
