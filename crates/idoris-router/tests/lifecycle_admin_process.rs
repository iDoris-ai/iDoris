//! IDORIS-CORE S6（接口规范 §2.11.4、勘误七）：Admin 的 `upstreams`，起**真二进制**加有状态的假 oMLX：Admin 的接线（引擎管理器交给完整的
//! Admin Router）、HTTP 上的 202 与状态码、重启之后聊天恢复、停下之后数据面的 503、冻结的上游要重启 iDoris。
//!
//! 进程内的细节（逐条状态码、并发、G2、G3、Router 里的 Supervisor 与实时门）在 `lifecycle_admin.rs`。**全部用假引擎**
//! （`tests/support/fake_omlx.rs`）：不加载任何模型、不启动任何推理进程，不连外网，不读真实的 `~/.omlx`；每个测试的 `HOME`、
//! `IDORIS_STATE_DIR` 都在临时目录里；帧里给显式的 `engine.executables.omlx`。iDoris 用 `ChildGuard`、假引擎在测试结束（含 panic）时被清掉。
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
const PATIENT: Duration = Duration::from_secs(60);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 托管卡（on_demand），`idoris/fast` 绑 `fake-model`。
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
  idoris_role_models: {{ fast: "fake-model" }}
"#
    )
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn command_of(pid: u64) -> String {
    let output = Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn pid_alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 一个测试的全部临时目录与端口；结束（含 panic）时清掉它拉起过的假引擎。
struct Env {
    repo: PathBuf,
    components: tempfile::TempDir,
    state: tempfile::TempDir,
    home: tempfile::TempDir,
    work: tempfile::TempDir,
    run_dir: PathBuf,
    _run: tempfile::TempDir,
    port: u16,
}

impl Drop for Env {
    fn drop(&mut self) {
        fake_omlx::kill_fake_engines_under(self.work.path());
    }
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
        fs::write(
            env.components.path().join("omlx.yaml"),
            managed_card(env.port),
        )
        .unwrap();
        env
    }

    fn state_dir(&self) -> PathBuf {
        self.state.path().join("engine-state")
    }

    fn discovery_path(&self) -> PathBuf {
        self.run_dir.join(FILE_NAME)
    }

    fn fake(&self, behavior: &Behavior) -> FakeOmlx {
        let dir = self.work.path().join("engine");
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

    /// 以 `--launcher-stdin` 起一个实例，写一帧并关闭 stdin；等到发现文件，返回 (进程, 数据面端口, Admin 端口)。
    fn start(&self, executable: &Path) -> (ChildGuard, u64, u64) {
        let frame = json!({
            "v": 1,
            "admin_token": TOKEN,
            "discovery_path": self.discovery_path(),
            "engine": {"executables": {"omlx": executable}},
        });
        let _ = fs::remove_file(self.discovery_path());
        let mut command = self.command();
        command.args(["serve", "--launcher-stdin"]);
        let mut child = spawn_retrying(&mut command).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(format!("{frame}\n").as_bytes());
        drop(stdin);
        let deadline = Instant::now() + PATIENT;
        loop {
            if let Ok(bytes) = fs::read(self.discovery_path()) {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                return (
                    child,
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
}

fn read_all(mut pipe: impl Read) -> String {
    let mut out = String::new();
    pipe.read_to_string(&mut out).unwrap();
    out
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

async fn health(client: &reqwest::Client, port: u64) -> Value {
    client
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

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

fn omlx_of(body: &Value) -> &Value {
    body["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|upstream| upstream["id"] == "omlx")
        .unwrap()
}

struct Api {
    client: reqwest::Client,
    data_port: u64,
    admin_port: u64,
}

impl Api {
    async fn admin(&self, method: reqwest::Method, path: &str, token: bool) -> (u16, Value) {
        let mut request = self.client.request(
            method,
            format!("http://127.0.0.1:{}/admin/api/v1/{path}", self.admin_port),
        );
        if token {
            request = request.bearer_auth(TOKEN);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn upstream(&self) -> Value {
        let (status, body) = self.admin(reqwest::Method::GET, "upstreams", true).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            idoris_contracts::parse::<idoris_contracts::admin_v0::AdminUpstreamsResponse>(&body)
                .is_ok(),
            "{body}"
        );
        body["upstreams"][0].clone()
    }

    async fn chat(&self) -> (u16, Value) {
        let response = self
            .client
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                self.data_port
            ))
            .header("X-iDoris-Privacy", "local_only")
            .json(&json!({
                "model": "idoris/fast",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn the_real_binary_stops_starts_and_restarts_an_engine_and_chat_survives_each_replacement() {
    let env = Env::new();
    let fake = env.fake(&Behavior {
        stateful_models: true,
        ..Behavior::default()
    });
    let (mut child, data_port, admin_port) = env.start(&fake.executable);
    let api = Api {
        client: http(),
        data_port,
        admin_port,
    };
    health_until(&api.client, data_port, |body| body["phase"] == "ready").await;

    // 读：元素带引擎的 pid 与端口，同 `/health` 的六个字段一字不差；没有任何 key、token。
    let first = fake.launch(0)["pid"].as_u64().unwrap();
    let element = api.upstream().await;
    assert_eq!(
        element,
        json!({
            "id": "omlx", "engine_kind": "omlx", "state": "ready", "managed": true, "owned": true,
            "port": env.port, "pid": first, "restart_required": false, "failure_count": 0
        })
    );
    let health_element = omlx_of(&health(&api.client, data_port).await).clone();
    for key in ["id", "engine_kind", "state", "managed", "owned"] {
        assert_eq!(element[key], health_element[key], "{key}");
    }
    let private_key = fake.launch(0)["api_key"].as_str().unwrap().to_owned();
    assert!(!element.to_string().contains(&private_key));

    // 没有 token：401，什么也没发生。
    let (status, _) = api
        .admin(reqwest::Method::POST, "upstreams/omlx/stop", false)
        .await;
    assert_eq!(status, 401);
    assert!(pid_alive(first));

    // 聊天：Supervisor 对（空的）引擎加载模型，聊天 200。
    let (status, body) = api.chat().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(fake.model_loads(), 1);

    // restart：202；新进程；聊天 200，而且新进程真的收到了 `load`（账本复位了，不是还以为模型在里面）。
    let (status, body) = api
        .admin(reqwest::Method::POST, "upstreams/omlx/restart", true)
        .await;
    assert_eq!(status, 202, "{body}");
    wait_until("the replacement engine", || {
        fake.launches() == 2 && !pid_alive(first)
    })
    .await;
    health_until(&api.client, data_port, |body| {
        omlx_of(body)["state"] == "ready" && body["phase"] == "ready"
    })
    .await;
    let second = fake.launch(1)["pid"].as_u64().unwrap();
    assert_eq!(api.upstream().await["pid"], json!(second));
    let (status, body) = api.chat().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(fake.model_loads(), 2);

    // stop：202；引擎停了，`/health` 是 `stopped` 加 `degraded`；数据面 503 带原因，没有请求发给引擎。
    let (status, body) = api
        .admin(reqwest::Method::POST, "upstreams/omlx/stop", true)
        .await;
    assert_eq!(status, 202, "{body}");
    let degraded = health_until(&api.client, data_port, |body| {
        omlx_of(body)["state"] == "stopped"
    })
    .await;
    assert_eq!(degraded["phase"], "degraded");
    wait_until("the second engine to die", || !pid_alive(second)).await;
    let requests = fake.requests().len();
    let (status, body) = api.chat().await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["type"], "local_only_unavailable");
    assert_eq!(body["error"]["reason_code"], "supervisor_unavailable");
    assert_eq!(
        fake.requests().len(),
        requests,
        "nothing reached the engine"
    );
    let stopped = api.upstream().await;
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["restart_required"], false);
    // 幂等：再 stop 仍是 202。
    let (status, _) = api
        .admin(reqwest::Method::POST, "upstreams/omlx/stop", true)
        .await;
    assert_eq!(status, 202);

    // start：202；第三个引擎；聊天回来。
    let (status, body) = api
        .admin(reqwest::Method::POST, "upstreams/omlx/start", true)
        .await;
    assert_eq!(status, 202, "{body}");
    health_until(&api.client, data_port, |body| {
        omlx_of(body)["state"] == "ready" && body["phase"] == "ready"
    })
    .await;
    assert_eq!(fake.launches(), 3);
    let (status, body) = api.chat().await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(fake.model_loads(), 3);

    // 干净退出：iDoris 与它拉起的引擎一起停，退出码 0。
    let third = fake.launch(2)["pid"].as_u64().unwrap();
    assert!(pid_alive(third));
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let status = wait_exit(&mut child, Duration::from_secs(30));
    assert_eq!(status.code(), Some(0));
    wait_until("the last engine to die", || !pid_alive(third)).await;
    for launch in fake.all_launches() {
        let pid = launch["pid"].as_u64().unwrap();
        assert!(
            !command_of(pid).contains("fake_omlx.py"),
            "an engine outlived iDoris"
        );
    }
}

#[tokio::test]
async fn the_real_binary_freezes_an_engine_that_was_not_there_when_it_started() {
    let env = Env::new();
    // 没装 oMLX：帧里给了显式路径而它不存在（所以这台机器上有没有真的 oMLX.app 都不会被拉起）。
    let missing = env.work.path().join("no-such-omlx-cli");
    let (mut child, data_port, admin_port) = env.start(&missing);
    let api = Api {
        client: http(),
        data_port,
        admin_port,
    };
    health_until(&api.client, data_port, |body| {
        body["phase"] == "degraded" && omlx_of(body)["state"] == "absent"
    })
    .await;
    let element = api.upstream().await;
    assert_eq!(
        element,
        json!({
            "id": "omlx", "engine_kind": "omlx", "state": "absent", "reason": "engine_not_found",
            "managed": true, "owned": false, "port": env.port, "restart_required": true,
            "failure_count": 0
        })
    );
    let (status, body) = api
        .admin(reqwest::Method::POST, "upstreams/omlx/start", true)
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["type"], "restart_required");
    assert_eq!(body["error"]["reason_code"], "restart_required");
    for action in ["stop", "restart"] {
        let (status, body) = api
            .admin(
                reqwest::Method::POST,
                &format!("upstreams/omlx/{action}"),
                true,
            )
            .await;
        assert_eq!(status, 409, "{action}: {body}");
        assert_eq!(body["error"]["type"], "upstream_not_owned");
        assert_eq!(body["error"]["reason_code"], "not_owned");
    }
    // 数据面：路由到它的请求 503 带原因（降级放行，S5）。
    let (status, body) = api.chat().await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["reason_code"], "engine_not_found");
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert_eq!(
        wait_exit(&mut child, Duration::from_secs(30)).code(),
        Some(0)
    );
}
