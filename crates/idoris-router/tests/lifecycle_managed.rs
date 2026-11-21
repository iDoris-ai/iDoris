//! IDORIS-CORE S5（接口规范 §2.7.4–§2.7.7、§2.11.1）：引擎管理器与启动门控，起真二进制加假 oMLX。
//!
//! **全部用假引擎**（`tests/support/fake_omlx.rs`：一个只回答 oMLX 三个状态接口的 python HTTP 服务，外面套 shell 包装当作
//! `omlx-cli`），**绝不启动真实的 oMLX 或任何推理进程**，不连外网，不读真实的 `~/.omlx`。每个测试的 `HOME`、
//! `IDORIS_STATE_DIR` 都在临时目录里；iDoris 与假引擎都用 `spawn_guarded`（或在测试结束时清理），不留进程。
//!
//! 进程级测试验证接线（`/health` 的阶段与上游、starting 体与完整 Router 的替换、退出码、日志、启动命令里的参数与环境）；
//! 状态机的细节（退避、熔断、命令）在文件后半部分用进程内的管理器加缩短的时间参数测。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;
#[path = "support/fake_omlx.rs"]
mod fake_omlx;

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use child_guard::ChildGuard;
use fake_omlx::{Behavior, FakeOmlx, spawn_retrying};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const FILE_NAME: &str = "idoris.json";
/// 单个动作的耐心：macOS 机器范围内串行化每个新写文件的首次 exec（约 0.2 秒一个），负载高时可达数秒。
const PATIENT: Duration = Duration::from_secs(60);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
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
        };
        env.write_card(&managed_card(env.port));
        env
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

    fn fake(&self, name: &str, behavior: &Behavior) -> FakeOmlx {
        let dir = self.work.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        FakeOmlx::create(&dir, behavior)
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
        command
    }

    /// 帧：Admin token、发现文件、引擎可执行文件（`engine.executables.omlx`），加调用方要加的字段。
    fn frame(&self, executable: Option<&Path>, extra: Value) -> Value {
        let mut frame = json!({
            "v": 1,
            "admin_token": TOKEN,
            "discovery_path": self.discovery_path(),
        });
        if let Some(executable) = executable {
            frame["engine"] = json!({"executables": {"omlx": executable}});
        }
        for (key, value) in extra.as_object().unwrap() {
            frame[key] = value.clone();
        }
        frame
    }

    /// 以 `--launcher-stdin` 起一个实例，写一帧并关闭 stdin（launcher 的预期行为）。
    fn spawn(&self, frame: &Value) -> ChildGuard {
        let mut command = self.command();
        command.args(["serve", "--launcher-stdin"]);
        self.spawn_command(command, frame)
    }

    fn spawn_command(&self, mut command: Command, frame: &Value) -> ChildGuard {
        let child = spawn_retrying(&mut command).unwrap();
        let mut child = ChildGuard::new(child);
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(format!("{frame}\n").as_bytes());
        drop(stdin);
        child
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
        assert!(
            child.try_wait().unwrap().is_none(),
            "the server exited before it wrote the discovery file"
        );
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
    let mut child = env.spawn(&env.frame(Some(&fake.executable), json!({})));
    let (data_port, admin_port) = wait_ports(&env.discovery_path(), &mut child);
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
    assert!(response.headers().contains_key("retry-after"));
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
    assert_eq!(admin("backends", true).await.unwrap().status(), 503);
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
    assert!(pid_alive(engine_pid));
    send("-TERM", child.id());
    let status = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(0),
        "stderr: {}",
        read_all(child.stderr.take().unwrap())
    );
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
