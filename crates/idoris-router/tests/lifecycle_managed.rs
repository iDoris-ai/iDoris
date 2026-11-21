//! IDORIS-CORE S5（接口规范 §2.7.4–§2.7.7、§2.11.1）：引擎管理器与启动门控，起真二进制加假 oMLX。
//!
//! **全部用假引擎**（`tests/support/fake_omlx.rs`：一个只回答 oMLX 三个状态接口的 python HTTP 服务，外面套 shell 包装当作
//! `omlx-cli`），**绝不启动真实的 oMLX 或任何推理进程**，不连外网，不读真实的 `~/.omlx`。每个测试的 `HOME`、
//! `IDORIS_STATE_DIR` 都在临时目录里；每个测试都在帧里给**显式**的 `engine.executables.omlx`（给了就只看它，不回退），
//! 所以这台机器上装没装真的 `/Applications/oMLX.app` 都不会被拉起。iDoris 用 `ChildGuard`、假引擎在测试结束（含 panic）时
//! 被清掉，不留进程。
//!
//! 本文件是进程级测试：验证接线（`/health` 的阶段与上游、starting 体与完整 Router 的替换、退出码、日志、启动命令里的参数与
//! 环境、孤儿接管、围栏、端口属主）。状态机的细节（退避、熔断、命令、外部引擎）在 `lifecycle_manager.rs`，
//! 用进程内的管理器加缩短的时间参数测。需要 `lsof`（封印所有权记录要确认监听端口的是引擎进程）的测试在没有它的机器上
//! 跳过并说明；macOS 有 `/usr/sbin/lsof`。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;
#[path = "support/fake_omlx.rs"]
mod fake_omlx;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use child_guard::ChildGuard;
use fake_omlx::{Behavior, FakeOmlx, spawn_retrying};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path as url_path},
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const FILE_NAME: &str = "idoris.json";
/// 单个动作的耐心：macOS 机器范围内串行化每个新写文件的首次 exec（约 0.2 秒一个），负载高时可达数秒。
const PATIENT: Duration = Duration::from_secs(60);
/// oMLX 配方的启动超时是 90 秒；这里的测试都在它之内有结论。
const GIB: u64 = 1 << 30;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

fn managed_card(port: u16) -> String {
    format!(
        r#"provider:
  id: omlx
  family: local
  tier: local
  capabilities: [chat, embedding]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:{port}"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: on_demand, keepalive: {{ idle_ttl_s: 300 }}, admission: requires_eviction }}
extensions:
  _degradation: "未托管时退回外部 oMLX：iDoris 只探测，不拉起、不停止"
  idoris_managed: {{ engine: omlx, mode: auto }}
"#
    )
}

/// 托管卡再绑一个角色（`fast`）：`idoris/fast` 路由到它。
fn managed_card_with_a_role(port: u16) -> String {
    format!(
        "{}  idoris_role_models: {{ fast: \"managed-model\" }}\n",
        managed_card(port)
    )
}

/// `setup upstream` 生成的那种卡（LM Studio 之类）：resident、带 `idoris_upstream`，绑 `daily`。
fn upstream_card(endpoint: &str) -> String {
    format!(
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
  _degradation: "未配置时 daily 角色没有候选"
  idoris_role_models: {{ daily: "up-model" }}
  idoris_upstream: {{ kind: openai_compat, flavor: openai_compat }}
"#
    )
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// 测试结束（含 panic）时清掉它登记的引擎进程：只杀命令行里带假引擎脚本名的（pid 被复用了就不碰）；再按这个测试自己的
/// 工作目录扫一遍，登记不及的残留也清掉。
#[derive(Default)]
struct EngineReaper {
    pids: Mutex<Vec<u64>>,
    work_dir: Mutex<Option<PathBuf>>,
}

impl EngineReaper {
    fn track(&self, pid: u64) {
        self.pids.lock().unwrap().push(pid);
    }

    fn sweep_under(&self, dir: &Path) {
        *self.work_dir.lock().unwrap() = Some(dir.to_owned());
    }
}

fn command_of(pid: u64) -> String {
    let output = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

impl Drop for EngineReaper {
    fn drop(&mut self) {
        for pid in self.pids.lock().unwrap().iter() {
            if command_of(*pid).contains("fake_omlx.py") {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
        if let Some(dir) = self.work_dir.lock().unwrap().as_deref() {
            fake_omlx::kill_fake_engines_under(dir);
        }
    }
}

/// 一个测试的全部临时目录与端口。
struct Env {
    repo: PathBuf,
    components: tempfile::TempDir,
    state: tempfile::TempDir,
    home: tempfile::TempDir,
    work: tempfile::TempDir,
    run_dir: PathBuf,
    _run: tempfile::TempDir,
    /// 托管卡的端口（一个刚释放的本机端口）。
    port: u16,
    reaper: EngineReaper,
    /// 额外的环境变量（预算等）。
    extra_env: Vec<(String, String)>,
}

impl Env {
    fn new() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let run = tempfile::tempdir().unwrap();
        let run_dir = fs::canonicalize(run.path()).unwrap();
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let env = Self {
            repo,
            components: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
            run_dir,
            _run: run,
            port: free_port(),
            reaper: EngineReaper::default(),
            extra_env: Vec::new(),
        };
        env.write_card(&managed_card(env.port));
        env.reaper.sweep_under(env.work.path());
        env
    }

    fn with_env(mut self, name: &str, value: &str) -> Self {
        self.extra_env.push((name.to_owned(), value.to_owned()));
        self
    }

    fn write_card(&self, yaml: &str) {
        fs::write(self.components.path().join("omlx.yaml"), yaml).unwrap();
    }

    fn discovery_path(&self) -> PathBuf {
        self.run_dir.join(FILE_NAME)
    }

    /// iDoris 的状态目录（所有权记录、引擎日志、oMLX 自己的 base、加载围栏都在它下面）。
    fn state_dir(&self) -> PathBuf {
        self.state.path().join("engine-state")
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn fence_path(&self) -> PathBuf {
        idoris_upstream::omlx::load_fence_path_in(&self.state_dir(), &self.endpoint()).unwrap()
    }

    fn fake(&self, name: &str, behavior: &Behavior) -> FakeOmlx {
        let dir = self.work.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        FakeOmlx::create(&dir, behavior)
    }

    /// 一个不存在的可执行文件：帧里给了显式路径就只看它，所以这台机器上有没有真的 oMLX.app 都不会被拉起。
    fn missing_executable(&self) -> PathBuf {
        self.work.path().join("no-such-omlx-cli")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
        {
            command.env_remove(key);
        }
        command
            .current_dir(&self.repo)
            .env("HOME", self.home.path())
            .env("IDORIS_STATE_DIR", self.state_dir())
            .env("IDORIS_COMPONENTS_DIR", self.components.path())
            .env(
                "IDORIS_ROUTING_POLICY",
                self.repo.join("config/routing-policy.yaml"),
            )
            .env("IDORIS_CATALOG", self.repo.join("config/catalog.yaml"))
            .env("IDORIS_DB_PATH", self.state.path().join("state.sqlite3"))
            .env("IDORIS_PORT", "0")
            .env("IDORIS_ADMIN_PORT", "0")
            .env("IDORIS_DEV_NO_KEY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.extra_env {
            command.env(name, value);
        }
        command
    }

    /// 帧：Admin token、发现文件、引擎可执行文件（`engine.executables.omlx`，必给），加调用方要加的字段。
    /// `engine_extra` 并进 `engine` 对象（`stop_on_exit` 等）。
    fn frame(&self, executable: &Path, engine_extra: Value, extra: Value) -> Value {
        let mut engine = json!({"executables": {"omlx": executable}});
        for (key, value) in engine_extra.as_object().unwrap() {
            engine[key] = value.clone();
        }
        let mut frame = json!({
            "v": 1,
            "admin_token": TOKEN,
            "discovery_path": self.discovery_path(),
            "engine": engine,
        });
        for (key, value) in extra.as_object().unwrap() {
            frame[key] = value.clone();
        }
        frame
    }

    /// 以 `--launcher-stdin` 起一个实例，写一帧并关闭 stdin（launcher 的预期行为）。
    fn spawn(&self, frame: &Value) -> ChildGuard {
        let mut command = self.command();
        command.args(["serve", "--launcher-stdin"]);
        let mut child = spawn_retrying(&mut command).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(format!("{frame}\n").as_bytes());
        drop(stdin);
        child
    }

    /// 起一个实例并等到它的发现文件（端口）。
    fn start(&self, frame: &Value) -> (ChildGuard, u64, u64) {
        let _ = fs::remove_file(self.discovery_path());
        let mut child = self.spawn(frame);
        let (data_port, admin_port) = wait_ports(&self.discovery_path(), &mut child);
        (child, data_port, admin_port)
    }
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "process did not exit in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send(signal: &str, pid: u32) {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {signal} {pid}");
}

fn pid_alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn wait_dead(pid: u64) {
    let deadline = Instant::now() + PATIENT;
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} did not die");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_all(mut pipe: impl Read) -> String {
    let mut out = String::new();
    pipe.read_to_string(&mut out).unwrap();
    out
}

/// 等发现文件出现并解析成功，返回 (数据面端口, Admin 端口)。
fn wait_ports(path: &Path, child: &mut Child) -> (u64, u64) {
    let deadline = Instant::now() + PATIENT;
    loop {
        if let Ok(bytes) = fs::read(path) {
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            return (
                value["data_port"].as_u64().unwrap(),
                value["admin_port"].as_u64().unwrap(),
            );
        }
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = child.stderr.take().map(read_all).unwrap_or_default();
            panic!("the server exited ({status}) before it wrote the discovery file: {stderr}");
        }
        assert!(Instant::now() < deadline, "discovery file never appeared");
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn health(client: &reqwest::Client, port: u64) -> Value {
    let response = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

/// 等 `/health` 满足条件。
async fn health_until(client: &reqwest::Client, port: u64, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + PATIENT;
    loop {
        let body = health(client, port).await;
        if done(&body) {
            return body;
        }
        assert!(Instant::now() < deadline, "health never converged: {body}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn upstream<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|upstream| upstream["id"] == id)
        .unwrap_or_else(|| panic!("no upstream {id} in {body}"))
}

/// 等托管上游 `omlx` 到达某个状态，返回那一次的 `/health`。
async fn omlx_until(client: &reqwest::Client, port: u64, state: &str) -> Value {
    health_until(client, port, |body| {
        upstream(body, "omlx")["state"] == state
    })
    .await
}

fn stderr_of(child: &mut Child) -> String {
    read_all(child.stderr.take().unwrap())
}

// ---------------------------------------------------------------------------------------------
// 启动：引擎不在时拉起并就绪；先答 /health，之后才切换到完整的 Router
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_engine_is_spawned_and_the_health_answers_before_the_full_router_takes_over() {
    let env = Env::new();
    // 冷启动慢：监听前先睡 2 秒，这段时间里门控一定还没有完成。
    let fake = env.fake(
        "engine",
        &Behavior {
            listen_delay_secs: 2.0,
            ..Behavior::default()
        },
    );
    let (mut child, data_port, admin_port) =
        env.start(&env.frame(&fake.executable, json!({}), json!({})));
    let client = http();

    // 先答 `/health`：门控还没有完成，`phase:"starting"`，托管上游 `starting`，`ready:false`。
    let early = health(&client, data_port).await;
    assert_eq!(early["status"], "ok", "{early}");
    assert_eq!(early["phase"], "starting", "{early}");
    assert_eq!(early["ready"], false);
    assert_eq!(
        upstream(&early, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "starting", "managed": true, "owned": false}),
    );

    // 数据面除 `/health` 外一律 503 `idoris_starting`，带 `Retry-After`。
    let response = client
        .get(format!("http://127.0.0.1:{data_port}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers().get("retry-after").unwrap(), "2");
    assert!(response.headers().contains_key("x-idoris-record-id"));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "idoris_starting");
    assert_eq!(body["error"]["reason_code"], "idoris_starting");
    // Admin：`status` 照常（Agent24 的「iDoris 可用」要它），其余 503；没有 token 仍是 401。
    let admin = |path: &str, token: bool| {
        let mut request = client.get(format!("http://127.0.0.1:{admin_port}/admin/api/v1/{path}"));
        if token {
            request = request.bearer_auth(TOKEN);
        }
        request.send()
    };
    assert_eq!(admin("status", true).await.unwrap().status(), 200);
    let blocked = admin("backends", true).await.unwrap();
    assert_eq!(blocked.status(), 503);
    assert_eq!(blocked.headers().get("retry-after").unwrap(), "2");
    let blocked: Value = blocked.json().await.unwrap();
    assert_eq!(blocked["error"]["type"], "idoris_starting");
    assert_eq!(blocked["error"]["reason_code"], "idoris_starting");
    assert_eq!(admin("backends", false).await.unwrap().status(), 401);

    // 引擎起来之后：切换。`phase:"ready"`，托管上游 `ready` 且 `owned:true`。
    let ready = health_until(&client, data_port, |body| body["phase"] == "ready").await;
    assert_eq!(ready["ready"], true);
    assert_eq!(
        upstream(&ready, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "ready", "managed": true, "owned": true}),
    );
    assert_eq!(
        ready["instance_id"], early["instance_id"],
        "the same instance throughout"
    );
    // 完整的 Router 在服务：`/v1/models` 经私有 key 问到了引擎。
    let models: Value = client
        .get(format!("http://127.0.0.1:{data_port}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "fake-model", "{models}");
    assert_eq!(admin("backends", true).await.unwrap().status(), 200);

    // 引擎只被拉起了一次，所有问它的请求都带着它的私有 key（没有一个被拒）。
    assert_eq!(fake.launches(), 1);
    let requests = fake.requests();
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|(_, auth)| auth == "ok"),
        "every request carried the engine's private key: {requests:?}"
    );

    // SIGTERM：iDoris 与它拉起的引擎一起停，退出码 0；所有权记录删了。
    let engine_pid = fake.launch(0)["pid"].as_u64().unwrap();
    env.reaper.track(engine_pid);
    assert!(pid_alive(engine_pid));
    send("-TERM", child.id());
    let status = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(status.code(), Some(0), "stderr: {}", stderr_of(&mut child));
    assert!(
        !pid_alive(engine_pid),
        "the engine iDoris spawned is stopped with it"
    );
    let records = env.state_dir().join("engines-owned/omlx");
    assert!(
        fs::read_dir(&records)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "the ownership record is removed on a clean stop"
    );
}

// ---------------------------------------------------------------------------------------------
// 启动命令：落盘的设置每次都传，环境清洗，iDoris 自己的 base，模型目录取自 S4 的 managed_models_dir()
// ---------------------------------------------------------------------------------------------

/// 一个已就绪的实例，干净停掉它（SIGTERM，退出码 0）。
async fn ready_then_stop(env: &Env, fake: &FakeOmlx, frame: &Value) -> usize {
    let (mut child, data_port, _) = env.start(frame);
    let client = http();
    health_until(&client, data_port, |body| body["phase"] == "ready").await;
    let launches = fake.launches();
    let engine_pid = fake.launch(launches - 1)["pid"].as_u64().unwrap();
    env.reaper.track(engine_pid);
    send("-TERM", child.id());
    let status = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(status.code(), Some(0), "stderr: {}", stderr_of(&mut child));
    wait_dead(engine_pid);
    launches
}

#[tokio::test]
async fn every_launch_states_the_persisted_settings_with_the_model_dir_taken_from_s4_each_time() {
    // 一份状态目录，先后拉起两次：第一次有额度，第二次没有额度。两次的命令行里都有内存护栏与模型目录：
    // oMLX 把它们落盘到 iDoris 自己的 base，不传就沿用上一次的值。
    let env = Env::new()
        .with_env("IDORIS_ALLOCATED_BUDGET_BYTES", &(16 * GIB).to_string())
        .with_env("IDORIS_CANARY", "canary-iDoris-secret")
        .with_env("AWS_SECRET_ACCESS_KEY", "canary-aws-secret")
        .with_env("HTTP_PROXY", "http://canary-proxy.invalid")
        .with_env("HF_TOKEN", "canary-hf-token")
        .with_env("OMLX_API_KEY", "canary-user-omlx-key")
        .with_env("IDORIS_OMLX_API_KEY", "canary-user-omlx-key");
    let fake = env.fake("engine", &Behavior::default());
    let frame = env.frame(&fake.executable, json!({}), json!({}));
    assert_eq!(ready_then_stop(&env, &fake, &frame).await, 1);

    // 第二次：同一个状态目录、同一个假引擎，没有额度。
    let mut env = env;
    env.extra_env
        .retain(|(name, _)| name != "IDORIS_ALLOCATED_BUDGET_BYTES");
    assert_eq!(ready_then_stop(&env, &fake, &frame).await, 2);

    let models_dir = idoris_router::setup_omlx::OmlxSetup::new(Some(env.home.path().to_path_buf()));
    let models_dir = idoris_router::setup_engine::SetupEngine::managed_models_dir(&models_dir)
        .expect("a plain home directory has a usable model dir");
    assert_eq!(models_dir, env.home.path().join(".omlx/models"));
    let port = env.port.to_string();
    let first = fake.launch(0);
    let second = fake.launch(1);
    assert_eq!(
        first["argv"],
        json!([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            port,
            "--memory-guard-gb",
            "16",
            "--model-dir",
            models_dir
        ]),
    );
    assert_eq!(
        second["argv"],
        json!([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            port,
            "--memory-guard",
            "balanced",
            "--model-dir",
            models_dir
        ]),
        "no budget: the default tier, spelled out, so the previous 16 GiB ceiling does not survive in the base"
    );

    for launch in [&first, &second] {
        // 环境清洗：iDoris 自己的环境（`IDORIS_*`、代理、云凭据、用户自己的 oMLX key……）一个都没有进入引擎。
        // 不用「名字白名单」来断言：引擎的包装脚本与 python 自己（macOS 的 `/usr/bin/python3` 是 xcrun 的垫片）会加变量。
        let names: Vec<&str> = launch["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap())
            .collect();
        for name in &names {
            assert!(
                !name.starts_with("IDORIS_"),
                "{name} must not reach the engine: {names:?}"
            );
        }
        for canary in ["AWS_SECRET_ACCESS_KEY", "HTTP_PROXY", "HF_TOKEN"] {
            assert!(
                !names.contains(&canary),
                "{canary} must not reach the engine: {names:?}"
            );
        }
        let everything = launch["env_all"].to_string();
        for secret in [
            "canary-iDoris-secret",
            "canary-aws-secret",
            "canary-proxy.invalid",
            "canary-hf-token",
            "canary-user-omlx-key",
        ] {
            assert!(
                !everything.contains(secret),
                "the value of a parent variable leaked: {secret}"
            );
        }
        // 私有 key：每次拉起各生成一个 64 位 hex，不是用户自己的 `OMLX_API_KEY`。
        let key = launch["api_key"].as_str().unwrap();
        assert_eq!(key.len(), 64, "a fresh 32-byte hex key");
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        // iDoris 自己的 base：`<状态目录>/omlx-base`，0700；不是 HOME 下的 `~/.omlx`。
        let base = PathBuf::from(launch["env_values"]["OMLX_BASE_PATH"].as_str().unwrap());
        assert_eq!(base, env.state_dir().join("omlx-base"));
        assert_eq!(
            fs::metadata(&base).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            launch["env_values"]["HOME"],
            env.home.path().to_str().unwrap()
        );
        // 工作目录是状态目录。
        assert_eq!(
            fs::canonicalize(launch["cwd"].as_str().unwrap()).unwrap(),
            fs::canonicalize(env.state_dir()).unwrap()
        );
        // key 不在命令行里。
        assert!(!launch["argv"].to_string().contains(key), "{launch}");
    }
    assert_ne!(
        first["api_key"], second["api_key"],
        "a new private key for every launch"
    );
    // 两次启动的私有 key 不同：每次拉起各生成一个（这里只能间接看：key 只在环境变量里，值不记录）；
    // 两次的进程不同，是各自拉起的。
    assert_ne!(first["pid"], second["pid"]);
}

#[tokio::test]
async fn a_home_that_has_a_comma_in_it_means_no_usable_model_dir_and_no_launch() {
    // oMLX 把 `--model-dir` 按逗号拆成多个目录：`~/.omlx/models` 的路径里有逗号，托管引擎就不能用它。
    // S4 的 `managed_models_dir()` 拒绝它，管理器每次拉起前都问它，所以不拉起，也没有稳定的原因码（只记日志）。
    let env = Env::new();
    let comma_home = env.work.path().join("a,b");
    fs::create_dir_all(&comma_home).unwrap();
    let env = env.with_env("HOME", comma_home.to_str().unwrap());
    let fake = env.fake("engine", &Behavior::default());
    let (mut child, data_port, _) = env.start(&env.frame(&fake.executable, json!({}), json!({})));
    let client = http();
    let body = omlx_until(&client, data_port, "failed").await;
    let omlx = upstream(&body, "omlx");
    assert!(
        omlx.get("reason").is_none(),
        "no stable reason code for this one: {body}"
    );
    assert_eq!(body["phase"], "degraded");
    assert_eq!(
        fake.launches(),
        0,
        "the engine is not started with a model dir oMLX would split"
    );
    send("-TERM", child.id());
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
    let stderr = stderr_of(&mut child);
    assert!(stderr.contains("模型目录"), "{stderr}");
}

#[tokio::test]
async fn an_unusable_base_directory_refuses_the_launch_with_base_unavailable() {
    // iDoris 自己的 base 必须是当前用户自己的真目录（0700，不是符号链接）；核对不过就不拉起，不回退到 `~/.omlx`。
    let env = Env::new();
    fs::create_dir_all(env.state_dir()).unwrap();
    let elsewhere = env.work.path().join("somewhere-else");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, env.state_dir().join("omlx-base")).unwrap();
    let fake = env.fake("engine", &Behavior::default());
    let (mut child, data_port, _) = env.start(&env.frame(&fake.executable, json!({}), json!({})));
    let client = http();
    let body = omlx_until(&client, data_port, "failed").await;
    assert_eq!(
        upstream(&body, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "failed", "reason": "base_unavailable", "managed": true, "owned": false}),
    );
    assert_eq!(body["phase"], "degraded");
    assert_eq!(fake.launches(), 0);
    // 链接原样留着，链接指向的目录没被碰。
    assert!(
        fs::symlink_metadata(env.state_dir().join("omlx-base"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    send("-TERM", child.id());
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
}

#[tokio::test]
async fn a_budget_too_small_to_hold_anything_never_launches_and_says_budget_zero() {
    for budget in ["0", "500000000"] {
        let env = Env::new().with_env("IDORIS_ALLOCATED_BUDGET_BYTES", budget);
        let fake = env.fake("engine", &Behavior::default());
        let (mut child, data_port, _) =
            env.start(&env.frame(&fake.executable, json!({}), json!({})));
        let client = http();
        let body = omlx_until(&client, data_port, "failed").await;
        assert_eq!(
            upstream(&body, "omlx"),
            &json!({"id": "omlx", "engine_kind": "omlx", "state": "failed", "reason": "budget_zero", "managed": true, "owned": false}),
            "budget {budget}"
        );
        assert_eq!(body["phase"], "degraded");
        assert_eq!(
            fake.launches(),
            0,
            "budget {budget}: nothing would fit, nothing is started"
        );
        if budget == "0" {
            // 额度为 0 在碰任何东西之前就拒绝：连 iDoris 自己的 base 目录都没有建。
            assert!(
                !env.state_dir().join("omlx-base").exists(),
                "a zero budget is refused before anything is prepared"
            );
        }
        // 终局的失败不拖住门控：iDoris 本身继续服务，数据面是完整的 Router（`/v1/models` 不再是启动期的 503；托管引擎没起来，
        // 列表是空的）。
        let response = client
            .get(format!("http://127.0.0.1:{data_port}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let models: Value = response.json().await.unwrap();
        assert_eq!(models["data"], json!([]), "{models}");
        send("-TERM", child.id());
        assert_eq!(
            wait_exit(&mut child, Duration::from_secs(30)).code(),
            Some(0)
        );
    }
}

#[tokio::test]
async fn an_engine_that_is_not_installed_is_absent_and_idoris_keeps_serving() {
    let env = Env::new();
    let (mut child, data_port, _) =
        env.start(&env.frame(&env.missing_executable(), json!({}), json!({})));
    let client = http();
    let body = omlx_until(&client, data_port, "absent").await;
    // `engine_not_found` 与同一个元素里的 `engine_kind` 一起出现：界面据此引导安装哪一种。
    assert_eq!(
        upstream(&body, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "absent", "reason": "engine_not_found", "managed": true, "owned": false}),
    );
    assert_eq!(body["status"], "ok");
    assert_eq!(body["phase"], "degraded");
    assert_eq!(body["ready"], false);
    send("-TERM", child.id());
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
}

#[tokio::test]
async fn an_engine_that_cannot_start_is_restarted_with_backoff_and_the_phase_stays_starting() {
    let env = Env::new();
    let fake = env.fake(
        "engine",
        &Behavior {
            exit_immediately: Some(7),
            ..Behavior::default()
        },
    );
    let (mut child, data_port, _) = env.start(&env.frame(&fake.executable, json!({}), json!({})));
    let client = http();
    let body = omlx_until(&client, data_port, "restarting").await;
    assert_eq!(
        body["phase"], "starting",
        "a restart in progress is not a terminal state: {body}"
    );
    // 退避之后再试：不止一次拉起（第一次失败等约 1 秒，第二次约 2 秒）。
    let deadline = Instant::now() + PATIENT;
    while fake.launches() < 2 {
        assert!(Instant::now() < deadline, "never retried");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    send("-TERM", child.id());
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
}

// ---------------------------------------------------------------------------------------------
// 门控不被一个起不来的托管引擎无限期拖住（接口规范 §2.7.6，勘误六）
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_engine_does_not_hold_a_healthy_upstream_card_hostage_the_gate_opens_degraded() {
    // 没装 oMLX（`engine_not_found`），另有一张健康的上游卡（`setup upstream` 生成的那种）：门控不被托管引擎拖住，
    // 上游卡照常服务；路由到托管卡的请求直接 503 带 `engine_not_found`；`/health.phase` 是 `degraded`。
    let lm_studio = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"object": "list", "data": [{"id": "up-model", "object": "model"}]}),
        ))
        .mount(&lm_studio)
        .await;
    Mock::given(method("POST"))
        .and(url_path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "up-model",
            "choices": [{"message": {"content": "hello from the upstream"}}]
        })))
        .mount(&lm_studio)
        .await;
    let env = Env::new();
    env.write_card(&managed_card_with_a_role(env.port));
    fs::write(
        env.components.path().join("lmstudio.yaml"),
        upstream_card(&lm_studio.uri()),
    )
    .unwrap();
    let (mut child, data_port, _) =
        env.start(&env.frame(&env.missing_executable(), json!({}), json!({})));
    let client = http();
    // 门控放行之后非托管卡的探测器才起：上游卡 `ready` 说明数据面已经是完整的 Router。
    let body = health_until(&client, data_port, |body| {
        upstream(body, "lmstudio")["state"] == "ready"
    })
    .await;
    assert_eq!(body["phase"], "degraded", "{body}");
    assert_eq!(body["ready"], false);
    assert_eq!(
        upstream(&body, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "absent", "reason": "engine_not_found", "managed": true, "owned": false}),
    );
    let chat = |model: &str| {
        client
            .post(format!("http://127.0.0.1:{data_port}/v1/chat/completions"))
            .header("X-iDoris-Privacy", "local_only")
            .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
    };
    // 上游卡照常服务。
    let response = chat("idoris/daily").await.unwrap();
    assert_eq!(response.status(), 200);
    let answer: Value = response.json().await.unwrap();
    assert!(
        answer.to_string().contains("hello from the upstream"),
        "{answer}"
    );
    // 托管卡：503 带原因，请求没有发给任何人。
    let response = chat("idoris/fast").await.unwrap();
    assert_eq!(response.status(), 503);
    let refused: Value = response.json().await.unwrap();
    assert_eq!(
        refused["error"]["type"], "local_only_unavailable",
        "{refused}"
    );
    assert_eq!(
        refused["error"]["reason_code"], "engine_not_found",
        "{refused}"
    );
    let chats = lm_studio
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/v1/chat/completions")
        .count();
    assert_eq!(chats, 1, "only the daily request reached the upstream");
    send("-TERM", child.id());
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
}

// ---------------------------------------------------------------------------------------------
// #619：端口属主。占着端口的、不是我们认的那种引擎的东西，不会收到 key，也不会被接管
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_unsigned_listener_on_the_managed_port_gets_no_key_is_not_adopted_and_no_second_engine_starts()
 {
    // 冒充者：占着托管端口、答得像 oMLX（`/v1/models` 有 `data` 数组）的普通进程（这个测试进程里的 wiremock）。
    // 它不是 oMLX.app 签名的代码（测试进程没有团队签名；没有 lsof 或 codesign 的机器读不出身份，同样不通过）。
    let impostor = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": []})),
        )
        .mount(&impostor)
        .await;
    let mut env = Env::new();
    env.port = impostor.address().port();
    env.write_card(&managed_card(env.port));
    let fake = env.fake("engine", &Behavior::default());
    let (mut child, data_port, admin_port) =
        env.start(&env.frame(&fake.executable, json!({}), json!({})));
    let client = http();
    let body = omlx_until(&client, data_port, "failed").await;
    assert_eq!(
        upstream(&body, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "failed", "reason": "port_in_use", "managed": true, "owned": false}),
    );
    assert_eq!(body["phase"], "degraded");
    // 不拉起第二个：端口上已经有东西在答。
    assert_eq!(fake.launches(), 0);
    // 管理器自己的（匿名）探测已经发生过了。终局的失败降级放行：数据面是完整的 Router，但它**不再碰**这个没通过属主校验的端口：
    // 没有为它建 Supervisor（它会去采样这个端口），`/v1/models` 与 Admin 的模型观察都不探测它。
    let before = impostor.received_requests().await.unwrap().len();
    assert!(before > 0, "the anonymous probe did reach it");
    let models = client
        .get(format!("http://127.0.0.1:{data_port}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let admin_models = client
        .get(format!("http://127.0.0.1:{admin_port}/admin/api/v1/models"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_models.status(), 200);
    let admin_models: Value = admin_models.json().await.unwrap();
    assert_eq!(
        admin_models["sources"][0]["state"], "error",
        "reported as unavailable without a probe: {admin_models}"
    );
    // 冒充者收到的每个请求都没有 `Authorization`：先校验属主，才可能发 key。给 Supervisor（如果错建了）几个周期去采样，
    // 也不会有新的请求。
    tokio::time::sleep(Duration::from_millis(600)).await;
    let received = impostor.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        before,
        "nothing contacted the listener after the degraded open: {received:?}"
    );
    for request in &received {
        assert!(
            request.headers.get("authorization").is_none(),
            "no credential may reach an unverified listener: {request:?}"
        );
    }
    // iDoris 不给它发信号，退出时也不动它（这里的「它」是测试进程自己，没被杀就是证明）。
    send("-TERM", child.id());
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
}

// ---------------------------------------------------------------------------------------------
// 孤儿接管：kill -9 iDoris 之后，重启接管仍在跑的自有引擎；加载中被杀则清围栏重拉
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn after_kill_9_a_restarted_idoris_adopts_the_engine_it_left_running() {
    if !require_lsof("after_kill_9_a_restarted_idoris_adopts_the_engine_it_left_running") {
        return;
    }
    let env = Env::new();
    let fake = env.fake("engine", &Behavior::default());
    let frame = env.frame(&fake.executable, json!({}), json!({}));
    let client = http();

    let (mut first, data_port, _) = env.start(&frame);
    health_until(&client, data_port, |body| body["phase"] == "ready").await;
    let engine_pid = fake.launch(0)["pid"].as_u64().unwrap();
    env.reaper.track(engine_pid);
    // SIGKILL：`Drop` 不会跑，来不及停引擎，它成了孤儿，继续占着端口。
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(
        pid_alive(engine_pid),
        "the engine outlives a kill -9 of iDoris"
    );
    // 引擎在，但它的私有 key 只有所有权记录里有：没有 key 的请求被拒。
    let record_dir = env.state_dir().join("engines-owned/omlx");
    assert_eq!(
        fs::read_dir(&record_dir).unwrap().count(),
        1,
        "one ownership record"
    );

    let (mut second, data_port, _) = env.start(&frame);
    let body = health_until(&client, data_port, |body| body["phase"] == "ready").await;
    assert_eq!(
        upstream(&body, "omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "ready", "managed": true, "owned": true}),
        "adopted: the engine is ours (owned), not an external one"
    );
    assert_eq!(
        fake.launches(),
        1,
        "no second engine: the running one was adopted"
    );
    assert!(
        pid_alive(engine_pid),
        "adopting does not restart it, the loaded models stay"
    );
    // 接管之后经私有 key 照常服务（key 沿用记录里的）。
    let models: Value = client
        .get(format!("http://127.0.0.1:{data_port}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "fake-model", "{models}");
    assert!(fake.requests().iter().all(|(_, auth)| auth == "ok"));

    // 默认 `stop_on_exit:"owned"`：干净停止时停掉自己的引擎，记录删了。
    send("-TERM", second.id());
    assert_eq!(
        wait_exit(&mut second, Duration::from_secs(30)).code(),
        Some(0)
    );
    wait_dead(engine_pid);
    assert_eq!(fs::read_dir(&record_dir).unwrap().count(), 0);
}

#[tokio::test]
async fn a_fence_left_by_a_killed_idoris_makes_the_owned_engine_killed_cleared_and_respawned() {
    if !require_lsof(
        "a_fence_left_by_a_killed_idoris_makes_the_owned_engine_killed_cleared_and_respawned",
    ) {
        return;
    }
    let env = Env::new();
    let fake = env.fake("engine", &Behavior::default());
    let frame = env.frame(&fake.executable, json!({}), json!({}));
    let client = http();

    let (mut first, data_port, _) = env.start(&frame);
    health_until(&client, data_port, |body| body["phase"] == "ready").await;
    let engine_pid = fake.launch(0)["pid"].as_u64().unwrap();
    env.reaper.track(engine_pid);
    first.kill().unwrap();
    first.wait().unwrap();
    // 加载进行中被杀：`load.pending` 围栏留着（Supervisor 在发出 load 之前持久化它，从不隐式删除）。
    let fence = env.fence_path();
    fs::create_dir_all(fence.parent().unwrap()).unwrap();
    fs::write(&fence, b"idoris load in progress\n").unwrap();

    let (mut second, data_port, _) = env.start(&frame);
    let body = health_until(&client, data_port, |body| body["phase"] == "ready").await;
    assert_eq!(upstream(&body, "omlx")["owned"], true);
    // 它是我们的：没有证据证明加载已停止，所以杀掉整组、确认消失、清围栏、重拉。
    assert!(!pid_alive(engine_pid), "the old engine was killed");
    assert_eq!(fake.launches(), 2, "a new engine was spawned");
    let new_pid = fake.launch(1)["pid"].as_u64().unwrap();
    env.reaper.track(new_pid);
    assert_ne!(new_pid, engine_pid);
    assert!(pid_alive(new_pid));
    assert!(
        !fence.exists(),
        "the fence is cleared, so loads are not refused fail-closed"
    );
    // 完整的 Router 的 Supervisor 是在清围栏之后才创建的：`/v1/models` 经新引擎的私有 key 照常。
    let models = client
        .get(format!("http://127.0.0.1:{data_port}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    send("-TERM", second.id());
    assert_eq!(
        wait_exit(&mut second, Duration::from_secs(30)).code(),
        Some(0)
    );
    let stderr = stderr_of(&mut second);
    assert!(stderr.contains("加载围栏"), "{stderr}");
}

#[tokio::test]
async fn stop_on_exit_never_keeps_the_engine_on_a_clean_exit_for_the_next_idoris() {
    if !require_lsof("stop_on_exit_never_keeps_the_engine_on_a_clean_exit_for_the_next_idoris") {
        return;
    }
    let env = Env::new();
    let fake = env.fake("engine", &Behavior::default());
    let frame = env.frame(
        &fake.executable,
        json!({"stop_on_exit": "never"}),
        json!({}),
    );
    let client = http();
    let (mut first, data_port, _) = env.start(&frame);
    health_until(&client, data_port, |body| body["phase"] == "ready").await;
    let engine_pid = fake.launch(0)["pid"].as_u64().unwrap();
    env.reaper.track(engine_pid);
    send("-TERM", first.id());
    assert_eq!(
        wait_exit(&mut first, Duration::from_secs(30)).code(),
        Some(0)
    );
    // 正常退出（退出码 0）保留引擎和所有权记录（升级重启时不想重新加载大模型）。
    assert!(
        pid_alive(engine_pid),
        "stop_on_exit never: a clean exit leaves the engine running"
    );
    let record_dir = env.state_dir().join("engines-owned/omlx");
    assert_eq!(fs::read_dir(&record_dir).unwrap().count(), 1);

    // 下一个 iDoris 接管它。
    let (mut second, data_port, _) = env.start(&frame);
    let body = health_until(&client, data_port, |body| body["phase"] == "ready").await;
    assert_eq!(upstream(&body, "omlx")["owned"], true);
    assert_eq!(fake.launches(), 1);
    send("-TERM", second.id());
    assert_eq!(
        wait_exit(&mut second, Duration::from_secs(30)).code(),
        Some(0)
    );
    // 还是 never：引擎仍在跑，由 `EngineReaper` 在测试结束时清掉。
    assert!(pid_alive(engine_pid));
}

// ---------------------------------------------------------------------------------------------
// 退出码 5：门控完成之后自有引擎失效，进程内无法恢复（热恢复是 v0.6.1），iDoris 优雅停止并通知 agent24d
// ---------------------------------------------------------------------------------------------

/// 一个最小的假 agent24d：在 UDS 上先发 `challenge`，收 `register`，回带服务端证明的 `ack`，之后回应心跳，
/// 并记下收到的每个 `op`（尤其是 `bye`）。证明用发现文件里的 `instance_nonce` 计算（只有读得到发现文件的才算得出来）。
struct FakeAgent24d {
    ops: Arc<Mutex<Vec<String>>>,
}

impl FakeAgent24d {
    fn start(socket: &Path, discovery: PathBuf) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let ops: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = ops.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (recorded, discovery) = (recorded.clone(), discovery.clone());
                std::thread::spawn(move || {
                    let conn_id = "0123456789abcdef";
                    let challenge = "00112233445566778899aabbccddeeff";
                    let _ = writeln!(
                        stream,
                        "{}",
                        json!({"v": 1, "op": "challenge", "conn_id": conn_id, "nonce": challenge})
                    );
                    let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
                    while let Some(Ok(line)) = lines.next() {
                        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let op = frame["op"].as_str().unwrap_or("").to_owned();
                        recorded.lock().unwrap().push(op.clone());
                        match op.as_str() {
                            "register" => {
                                let record: Value =
                                    serde_json::from_slice(&fs::read(&discovery).unwrap()).unwrap();
                                let nonce = record["instance_nonce"].as_str().unwrap();
                                let key_bytes =
                                    idoris_router::instance_nonce::decode_lower_hex::<32>(nonce)
                                        .unwrap();
                                let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key_bytes);
                                let session = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
                                let proof = idoris_router::register::ack_proof(
                                    &key, conn_id, challenge, session,
                                );
                                let _ = writeln!(
                                    stream,
                                    "{}",
                                    json!({"v": 1, "op": "ack", "session_id": session, "heartbeat_ms": 500, "proof": proof})
                                );
                            }
                            "heartbeat" => {
                                let _ = writeln!(
                                    stream,
                                    "{}",
                                    json!({"v": 1, "op": "heartbeat_ack", "seq": frame["seq"]})
                                );
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        Self { ops }
    }

    fn ops(&self) -> Vec<String> {
        self.ops.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn an_engine_that_dies_after_the_gate_makes_idoris_stop_with_exit_code_5_and_say_bye() {
    if !require_lsof(
        "an_engine_that_dies_after_the_gate_makes_idoris_stop_with_exit_code_5_and_say_bye",
    ) {
        return;
    }
    let env = Env::new();
    // 引擎起来 5 秒后自己退出（退出码 3）：门控早已完成，Supervisor 的账本之后会过期，进程内恢复不了。
    let fake = env.fake(
        "engine",
        &Behavior {
            exit_after_secs: 5.0,
            ..Behavior::default()
        },
    );
    let socket = env.run_dir.join("register.sock");
    let agent = FakeAgent24d::start(&socket, env.discovery_path());
    let frame = env.frame(
        &fake.executable,
        json!({}),
        json!({"register_socket": socket}),
    );
    let (mut child, data_port, _) = env.start(&frame);
    let client = http();
    health_until(&client, data_port, |body| {
        body["phase"] == "ready" && body["registered"] == true
    })
    .await;
    let engine_pid = fake.launch(0)["pid"].as_u64().unwrap();
    env.reaper.track(engine_pid);

    // 引擎退出 → iDoris 优雅停止，退出码 5（计入 launcher 的崩溃熔断，界面显示「模型引擎异常」）。
    let status = wait_exit(&mut child, Duration::from_secs(60));
    assert_eq!(status.code(), Some(5), "stderr: {}", stderr_of(&mut child));
    let stderr = stderr_of(&mut child);
    assert!(stderr.contains("退出码 5"), "{stderr}");
    // 已 ack 的会话上发了 `bye`（优雅停止的信号，让 agent24d 区分「预期停止」与「崩溃」）。
    assert!(
        agent.ops().iter().any(|op| op == "bye"),
        "bye on the acked session: {:?}",
        agent.ops()
    );
    // 一律停掉自己拥有的引擎（这里它已经死了）：没有残留的记录，发现文件也删了。
    assert!(!pid_alive(engine_pid));
    let record_dir = env.state_dir().join("engines-owned/omlx");
    assert_eq!(fs::read_dir(&record_dir).map(|d| d.count()).unwrap_or(0), 0);
    assert!(!env.discovery_path().exists());
}
