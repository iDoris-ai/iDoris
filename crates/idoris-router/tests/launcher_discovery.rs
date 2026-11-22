//! IDORIS-CORE 切片 S1（接口规范修订 1.6）：动态端口、`--launcher-stdin` 帧、发现文件、`/health` 新字段。
//! 新字段从 S1 起出现，`contract_version` 自 v0.6.0 起为 1.3.0（规范 §2.11）。
//!
//! 起真二进制验证：端口 0（数据面与 Admin 都是 0）、发现文件的内容与权限、校验失败的各情形、读者并发读不到半截文件、
//! `kill -9` 之后残留文件可识别且被下一个实例覆盖、第二实例退出码 3 且不碰文件、SIGTERM 删除自己的文件
//! （删不掉时文件原样保留、只留一行固定日志，退出码仍是 0）、
//! 帧错误退出 1 且 stderr / stdout 不含 token、`instance_nonce` 金丝雀不出现在 `/health` / Admin / 日志 / argv / 环境变量。
//! 旧的 `--admin-token-stdin` 路径由 `launcher_lifecycle.rs` 继续覆盖，本文件只补「互斥」与「不写发现文件」。
//!
//! 测试环境是密封的：`HOME`、`IDORIS_STATE_DIR` 指向临时目录（不读真实的 `~/.omlx/settings.json`，不碰
//! `~/.local/state/idoris`），组件卡的 endpoint 指向一个刚释放的本机端口或 wiremock，不连外网，不起任何推理进程。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

// 起的真二进制都用 `spawn_guarded`：断言失败或 panic 时守卫的 `Drop` 杀掉并收走子进程，不会留下占着端口的孤儿。
// 测试进程整个被 SIGKILL 时 `Drop` 不会跑；`register_uds.rs` 在帧里加了孤儿计时兜底，这个文件没有：这里的帧不带
// `register_socket`，要加 `orphan_timeout_s` 就得连 `register_socket` 一起加，每条测试都会带着登记客户端跑，
// 改变它们测的东西（日志、`/health`、退出路径）。
#[path = "support/child_guard.rs"]
mod child_guard;

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use child_guard::{ChildGuard, SpawnGuarded};
use idoris_router::{
    discovery::{DiscoveryError, DiscoveryRecord, DiscoveryTarget, RemoveOutcome},
    instance_nonce::InstanceNonce,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const FILE_NAME: &str = "idoris.json";

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// 一个刚释放的本机端口：此刻没有人监听。
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn canonical_private_dir(temp: &tempfile::TempDir) -> PathBuf {
    let canonical = fs::canonicalize(temp.path()).unwrap();
    fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700)).unwrap();
    canonical
}

fn resident_card(id: &str, endpoint: &str, version_pin: &str) -> String {
    format!(
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
version_pin: "{version_pin}"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
"#
    )
}

/// 与随包的 `config/components/omlx.yaml` 同形（on_demand、由 Supervisor 管理），endpoint 换成一个关着的端口，
/// 这样 Supervisor 的启动采样必然失败，`/health` 报 `unavailable` + `startup_latched`，且不依赖本机有没有 oMLX。
fn supervised_omlx_card(endpoint: &str) -> String {
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
endpoint: "{endpoint}"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: on_demand, keepalive: {{ idle_ttl_s: 300 }}, admission: requires_eviction }}
"#
    )
}

struct Env {
    repo: PathBuf,
    components: tempfile::TempDir,
    state: tempfile::TempDir,
    home: tempfile::TempDir,
    /// 持有发现文件所在的临时目录（drop 时删除）；路径用 `run_dir`。
    _run: tempfile::TempDir,
    run_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let components = tempfile::tempdir().unwrap();
        fs::write(
            components.path().join("omlx.yaml"),
            supervised_omlx_card(&format!("http://127.0.0.1:{}", closed_port())),
        )
        .unwrap();
        let run = tempfile::tempdir().unwrap();
        let run_dir = canonical_private_dir(&run);
        Self {
            repo,
            components,
            state: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            _run: run,
            run_dir,
        }
    }

    fn discovery_path(&self) -> PathBuf {
        self.run_dir.join(FILE_NAME)
    }

    fn command(&self, port: &str, admin_port: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
        {
            command.env_remove(key);
        }
        command
            .current_dir(&self.repo)
            .env("HOME", self.home.path())
            .env("IDORIS_STATE_DIR", self.state.path().join("engine-state"))
            .env("IDORIS_COMPONENTS_DIR", self.components.path())
            .env(
                "IDORIS_ROUTING_POLICY",
                self.repo.join("config/routing-policy.yaml"),
            )
            .env("IDORIS_CATALOG", self.repo.join("config/catalog.yaml"))
            .env("IDORIS_DB_PATH", self.state.path().join("state.sqlite3"))
            .env("IDORIS_PORT", port)
            .env("IDORIS_ADMIN_PORT", admin_port)
            .env("IDORIS_DEV_NO_KEY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn frame(&self, extra: Value) -> Value {
        let mut frame = json!({
            "v": 1,
            "admin_token": TOKEN,
            "discovery_path": self.discovery_path(),
        });
        for (key, value) in extra.as_object().unwrap() {
            frame[key] = value.clone();
        }
        frame
    }

    /// 以 `--launcher-stdin` 起一个实例（两个端口都是 0），写一帧并关闭 stdin（launcher 的预期行为）。
    fn spawn_launcher(&self, frame: &Value) -> ChildGuard {
        self.spawn_launcher_bytes("0", "0", format!("{frame}\n").as_bytes())
    }

    fn spawn_launcher_bytes(&self, port: &str, admin_port: &str, bytes: &[u8]) -> ChildGuard {
        let mut child = self
            .command(port, admin_port)
            .args(["serve", "--launcher-stdin"])
            .spawn_guarded()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // 子进程可能已经因为别的原因退出（例如第二实例），写失败不是这里要测的东西。
        let _ = stdin.write_all(bytes);
        drop(stdin);
        child
    }
}

fn send(signal: &str, child: &Child) {
    let status = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {signal} {}", child.id());
}

fn pid_alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
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

fn read_all(mut pipe: impl Read) -> String {
    let mut out = String::new();
    pipe.read_to_string(&mut out).unwrap();
    out
}

fn output_of(child: &mut Child) -> (String, String) {
    (
        read_all(child.stdout.take().unwrap()),
        read_all(child.stderr.take().unwrap()),
    )
}

/// 等发现文件出现并解析成功。文件一旦存在，每一次读到的内容都必须是完整的 JSON（原子写）。
fn wait_discovery(path: &Path, child: &mut Child) -> (Value, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match fs::read(path) {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                    panic!(
                        "the discovery file existed but was not complete JSON ({} bytes): {error}",
                        bytes.len()
                    )
                });
                return (value, bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("reading the discovery file: {error}"),
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the server exited before it wrote the discovery file"
        );
        assert!(Instant::now() < deadline, "discovery file never appeared");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

async fn get_json(client: &reqwest::Client, url: &str) -> Value {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{url}: {}",
        response.status()
    );
    response.json().await.unwrap()
}

async fn health(client: &reqwest::Client, port: u64) -> Value {
    get_json(client, &format!("http://127.0.0.1:{port}/health")).await
}

/// 等 `/health` 满足条件（探测器在后台跑，第一轮探测完成之前 `phase` 是 `starting`）。
async fn health_until(client: &reqwest::Client, port: u64, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let body = health(client, port).await;
        if done(&body) {
            return body;
        }
        assert!(Instant::now() < deadline, "health never converged: {body}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 固定端口的实例：等 `/health` 第一次可达（连接被拒就重试），返回那一次的响应体。
async fn wait_healthy(client: &reqwest::Client, port: u64, child: &mut Child) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && response.status().is_success()
        {
            return response.json().await.unwrap();
        }
        assert!(child.try_wait().unwrap().is_none(), "server exited early");
        assert!(Instant::now() < deadline, "never became healthy");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn dir_listing(dir: &Path) -> Vec<String> {
    let mut names = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn sigterm_and_collect(child: &mut Child) -> (std::process::ExitStatus, String, String) {
    send("-TERM", child);
    let status = wait_exit(child, Duration::from_secs(20));
    let (stdout, stderr) = output_of(child);
    (status, stdout, stderr)
}

/// 把目录权限恢复成 0700：断言失败时也要恢复，否则临时目录删不掉。
struct RestoreMode(PathBuf);

impl Drop for RestoreMode {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

/// 退出时「发现文件未删除」的那几行日志。
fn kept_file_log_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("发现文件未删除"))
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// 端口 0 与发现文件的内容
// ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn port_zero_binds_kernel_assigned_ports_and_the_discovery_file_carries_the_real_ones() {
    let env = Env::new();
    let before_ms = unix_ms();
    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);

    let data_port = file["data_port"].as_u64().unwrap();
    let admin_port = file["admin_port"].as_u64().unwrap();
    for port in [data_port, admin_port] {
        assert!((1..=65535).contains(&port), "{port}");
    }
    assert_ne!(data_port, admin_port);

    let client = http();
    let health = health(&client, data_port).await;
    assert_eq!(health["instance_id"], file["instance_id"]);
    assert_eq!(health["version"], file["version"]);
    assert_eq!(health["contract_version"], file["contract_version"]);
    assert_eq!(health["started_at_ms"], file["started_at_ms"]);
    assert_eq!(health["status"], "ok");

    // Admin 在它自己的（也是内核分配的）端口上，用帧里的 token。
    let admin: Value = client
        .get(format!("http://127.0.0.1:{admin_port}/admin/api/v1/status"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(admin["instance_id"], file["instance_id"]);
    // Admin 不在数据面端口上（同 bin 里 `bound_servers_serve_both_surfaces...` 的口径：数据面没有这条路由）。
    let wrong = client
        .get(format!("http://127.0.0.1:{data_port}/admin/api/v1/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::NOT_FOUND);

    let started = file["started_at_ms"].as_u64().unwrap();
    assert!(
        started >= before_ms.saturating_sub(1_000) && started <= unix_ms(),
        "{started}"
    );

    let (status, stdout, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    // stdout 不属于契约，但不能再打印请求值 `:0`；实际地址只打印一次。
    assert!(
        stdout.contains(&format!("idoris listening on http://127.0.0.1:{data_port}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "idoris admin listening on http://127.0.0.1:{admin_port}"
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("127.0.0.1:0\n"), "{stdout}");
    assert_eq!(stdout.matches("idoris listening on").count(), 1, "{stdout}");
}

#[tokio::test]
async fn discovery_file_has_the_specified_fields_permissions_and_launch_snapshot() {
    let env = Env::new();
    let mut command = env.command("0", "0");
    command.env("IDORIS_ALLOCATED_BUDGET_BYTES", "17179869184");
    let mut child = command
        .args(["serve", "--launcher-stdin"])
        .spawn_guarded()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", env.frame(json!({}))).as_bytes())
        .unwrap();
    let (file, bytes) = wait_discovery(&env.discovery_path(), &mut child);

    let mut keys = file
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        [
            "admin_port",
            "contract_version",
            "data_port",
            "instance_id",
            "instance_nonce",
            "launch",
            "pid",
            "schema",
            "service",
            "started_at_ms",
            "version"
        ]
    );
    assert_eq!(file["schema"], 1);
    assert_eq!(file["service"], "idoris");
    let instance_id = file["instance_id"].as_str().unwrap();
    assert_eq!(instance_id.len(), 36);
    assert_eq!(instance_id.as_bytes()[14], b'4', "UUID v4: {instance_id}");
    let nonce = file["instance_nonce"].as_str().unwrap();
    assert!(
        nonce.len() == 64
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{nonce}"
    );
    assert_eq!(file["pid"].as_u64().unwrap(), u64::from(child.id()));
    assert_eq!(file["version"], env!("CARGO_PKG_VERSION"));
    // 故意写死字面量：发现文件的 `contract_version` 等于 `/health.contract_version`，现为 1.3.0
    // （再升版本时，这里与 TS、conformance 的期望一起改）。
    assert_eq!(file["contract_version"], "1.3.0");
    assert!(file["started_at_ms"].as_u64().unwrap() > 1_735_689_600_000);

    // 紧凑的一行 JSON 加 LF，≤ 4096 字节，0600，属主是当前用户；不含 Admin token。
    assert!(bytes.len() <= 4096);
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert!(bytes.ends_with(b"\n"));
    let metadata = fs::metadata(env.discovery_path()).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
    assert!(!String::from_utf8_lossy(&bytes).contains(TOKEN));
    // 目录里只有发现文件和旁路锁，没有临时文件残留。
    assert_eq!(dir_listing(&env.run_dir), [FILE_NAME, "idoris.json.lock"]);

    // launch：启动输入快照。
    let launch = &file["launch"];
    assert_eq!(
        launch["db_path"],
        fs::canonicalize(env.state.path().join("state.sqlite3"))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );
    assert_eq!(
        launch["state_dir"],
        env.state
            .path()
            .join("engine-state")
            .to_string_lossy()
            .into_owned()
    );
    assert_eq!(
        launch["components_dir"],
        fs::canonicalize(env.components.path())
            .unwrap()
            .to_string_lossy()
            .into_owned()
    );
    assert_eq!(launch["allocated_budget_bytes"], 17_179_869_184_u64);
    assert_eq!(
        launch["engines"],
        json!([{"id": "omlx", "engine_kind": "omlx", "managed": false}])
    );
    assert!(launch["db_path"].as_str().unwrap().starts_with('/'));
    assert!(launch["components_dir"].as_str().unwrap().starts_with('/'));

    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[tokio::test]
async fn health_upstreams_and_launch_engines_list_every_loopback_engine_card_with_its_kind() {
    let env = Env::new();
    let lmstudio = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m"}]})))
        .mount(&lmstudio)
        .await;
    fs::write(
        env.components.path().join("lmstudio.yaml"),
        resident_card("lmstudio-local", &lmstudio.uri(), "lmstudio@0.3.1"),
    )
    .unwrap();
    // 版本钉没有产品名：engine_kind 是 unknown，不是错误。
    fs::write(
        env.components.path().join("mystery.yaml"),
        resident_card("mystery", &lmstudio.uri(), "mystery-engine"),
    )
    .unwrap();

    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let data_port = file["data_port"].as_u64().unwrap();
    let client = http();
    let body = health_until(&client, data_port, |body| body["phase"] != "starting").await;

    assert_eq!(body["status"], "ok");
    assert_eq!(body["phase"], "ready", "{body}");
    assert_eq!(body["ready"], true);
    assert_eq!(body["registered"], false);
    assert_eq!(body["components"], 3);
    let upstreams = body["upstreams"].as_array().unwrap();
    let by_id = |id: &str| {
        upstreams
            .iter()
            .find(|upstream| upstream["id"] == id)
            .unwrap()
    };
    // resident 卡：探测通过（200 + data 数组）就是 ready。
    assert_eq!(
        by_id("lmstudio-local"),
        &json!({"id": "lmstudio-local", "engine_kind": "lmstudio", "state": "ready", "managed": false, "owned": false})
    );
    assert_eq!(by_id("mystery")["engine_kind"], "unknown");
    // Supervisor 管理的卡：引擎起不来时报 unavailable + startup_latched，绝不是 ready。
    assert_eq!(
        by_id("omlx"),
        &json!({"id": "omlx", "engine_kind": "omlx", "state": "unavailable", "reason": "startup_latched", "managed": false, "owned": false})
    );
    assert_eq!(upstreams.len(), 3);

    // 发现文件里的 launch.engines 是同一批引擎型上游的静态清单（不含状态）。
    let mut engines = file["launch"]["engines"].as_array().unwrap().clone();
    engines.sort_by_key(|engine| engine["id"].as_str().unwrap().to_string());
    assert_eq!(
        engines,
        vec![
            json!({"id": "lmstudio-local", "engine_kind": "lmstudio", "managed": false}),
            json!({"id": "mystery", "engine_kind": "unknown", "managed": false}),
            json!({"id": "omlx", "engine_kind": "omlx", "managed": false}),
        ]
    );
    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[tokio::test]
async fn health_reports_auth_required_and_auth_failed_and_stays_degraded_without_a_ready_upstream()
{
    let env = Env::new();
    // 没有 key 的 401：auth_required。
    let locked = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&locked)
        .await;
    fs::write(
        env.components.path().join("lm.yaml"),
        resident_card("lm", &locked.uri(), "lmstudio@1"),
    )
    .unwrap();
    // oMLX 的卡带了 key（环境变量）却被拒：auth_failed。key 只发给 loopback 的 oMLX 卡。
    let omlx_auth = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer the-right-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .with_priority(1)
        .mount(&omlx_auth)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .with_priority(5)
        .mount(&omlx_auth)
        .await;
    fs::write(
        env.components.path().join("omlx-resident.yaml"),
        resident_card("omlx-embed", &omlx_auth.uri(), "omlx@0.6.4"),
    )
    .unwrap();

    let mut command = env.command("0", "0");
    command.env("IDORIS_OMLX_API_KEY", "a-wrong-key");
    let mut child = command
        .args(["serve", "--launcher-stdin"])
        .spawn_guarded()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", env.frame(json!({}))).as_bytes())
        .unwrap();
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let client = http();
    let body = health_until(&client, file["data_port"].as_u64().unwrap(), |body| {
        body["phase"] != "starting"
    })
    .await;

    assert_eq!(body["status"], "ok", "status stays ok in every phase");
    assert_eq!(body["phase"], "degraded", "{body}");
    assert_eq!(body["ready"], false);
    let upstreams = body["upstreams"].as_array().unwrap();
    let by_id = |id: &str| {
        upstreams
            .iter()
            .find(|upstream| upstream["id"] == id)
            .unwrap()
    };
    assert_eq!(by_id("lm")["state"], "unavailable");
    assert_eq!(by_id("lm")["reason"], "auth_required");
    assert_eq!(by_id("omlx-embed")["state"], "unavailable");
    assert_eq!(by_id("omlx-embed")["reason"], "auth_failed");
    assert_eq!(by_id("omlx")["reason"], "startup_latched");
    // 密钥本身不出现在任何地方。
    assert!(!body.to_string().contains("a-wrong-key"));
    let (status, stdout, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(!format!("{stdout}{stderr}").contains("a-wrong-key"));
}

fn managed_card(engine: &str, version_pin: &str, endpoint: &str) -> String {
    format!(
        r#"{}extensions:
  _degradation: "未托管时退回外部引擎：iDoris 只探测，不拉起"
  idoris_managed: {{ engine: {engine}, mode: auto }}
"#,
        resident_card("managed-engine", endpoint, version_pin)
    )
}

/// 规范 §2.3：`idoris_managed` 声明的引擎种类本版本不认识，或与 `version_pin` 的产品名不一致，是卡校验失败：
/// 启动失败（退出码 1），不静默忽略，也不会走到绑定端口。
#[test]
fn a_managed_declaration_this_version_does_not_know_or_that_contradicts_the_version_pin_fails_startup()
 {
    let env = Env::new();
    let endpoint = format!("http://127.0.0.1:{}", closed_port());
    for (card, expected) in [
        (
            managed_card("lmstudio", "lmstudio@1", &endpoint),
            "不是本版本认识的引擎种类",
        ),
        (
            managed_card("omlx", "lmstudio@1", &endpoint),
            "与 version_pin 的产品名",
        ),
    ] {
        fs::write(env.components.path().join("managed.yaml"), card).unwrap();
        let mut child = env.spawn_launcher(&env.frame(json!({})));
        let status = wait_exit(&mut child, Duration::from_secs(30));
        let (stdout, stderr) = output_of(&mut child);
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("[idoris] 启动失败：组件卡 \"managed-engine\""),
            "{stderr}"
        );
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!stdout.contains("listening"), "{stdout}");
        assert!(!env.discovery_path().exists());
    }
}

/// 声明了托管的卡：`managed` 是声明事实（`/health` 与 `launch.engines` 一致）。S5 起这张卡由引擎管理器接管：端点上
/// 已经有服务在答（这里是测试进程里的 mock，不是 oMLX，没有 oMLX 的签名团队），按 #619 校验端口属主不过，所以
/// 不接管、不发 key，报 `failed` + `port_in_use`；`owned` 是 false（不是 iDoris 拉起的）。接管真正的外部引擎与自己拉起的
/// 引擎由 `lifecycle_manager.rs`、`lifecycle_managed.rs` 覆盖。
#[tokio::test]
async fn a_managed_declaration_is_reported_as_managed_and_a_listener_that_is_not_the_engine_is_refused()
 {
    let env = Env::new();
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&upstream)
        .await;
    fs::write(
        env.components.path().join("managed.yaml"),
        managed_card("omlx", "omlx@0.6.4", &upstream.uri()),
    )
    .unwrap();
    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let body = health_until(&http(), file["data_port"].as_u64().unwrap(), |body| {
        body["upstreams"].as_array().is_some_and(|upstreams| {
            upstreams
                .iter()
                .any(|u| u["id"] == "managed-engine" && u["state"] == "failed")
        }) && body["phase"] == "degraded"
    })
    .await;
    let managed = body["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|upstream| upstream["id"] == "managed-engine")
        .unwrap();
    assert_eq!(
        managed,
        &json!({"id": "managed-engine", "engine_kind": "omlx", "state": "failed", "reason": "port_in_use", "managed": true, "owned": false})
    );
    // 托管上游处于终态：门控不被它拖住（降级放行，别的卡照常服务），`phase` 是 `degraded`（不是 `ready`）。放行之后默认卡
    // `omlx` 的探测器才起，它第一次观测之前是 `starting`，所以上面等到 `degraded` 为止。
    assert_eq!(body["phase"], "degraded");
    assert_eq!(body["ready"], false);
    assert!(
        file["launch"]["engines"]
            .as_array()
            .unwrap()
            .contains(&json!({"id": "managed-engine", "engine_kind": "omlx", "managed": true})),
        "{}",
        file["launch"]["engines"]
    );
    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

/// `register_socket` 与 `orphan_timeout_s` 从 S2 起由 `register` 使用（登记客户端、孤儿计时），它们的行为由
/// `register_uds.rs` 覆盖；本测试只钉住「没有任何 agent24d 在监听」这一种情形：照常服务、登记不上，
/// iDoris 是 UDS 客户端、不会自己创建这个 socket。`engine.*` 要等 S3/S5 才使用，这里只是被解析、被接受。
#[tokio::test]
async fn a_register_socket_nobody_listens_on_leaves_serving_unchanged_and_the_instance_unregistered()
 {
    let env = Env::new();
    let socket = env.run_dir.join("idoris-register.sock");
    let frame = env.frame(json!({
        "register_socket": socket,
        "orphan_timeout_s": 900,
        "engine": {
            "executables": {"omlx": "/Applications/oMLX.app/Contents/MacOS/omlx-cli"},
            "stop_on_exit": "never"
        }
    }));
    let mut child = env.spawn_launcher(&frame);
    let (file, _) = wait_discovery(&env.discovery_path(), &mut child);
    let body = health(&http(), file["data_port"].as_u64().unwrap()).await;
    // 没有 agent24d 在监听，登记不上：registered 为 false；iDoris 是 UDS 客户端，不会自己创建这个 socket。
    assert_eq!(body["registered"], false);
    assert!(!socket.exists());
    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[tokio::test]
async fn without_a_discovery_path_the_launcher_frame_serves_and_writes_no_file() {
    let env = Env::new();
    let frame = json!({"v": 1, "admin_token": TOKEN});
    let fixed = closed_port();
    let admin = closed_port();
    let mut child = env.spawn_launcher_bytes(
        &fixed.to_string(),
        &admin.to_string(),
        format!("{frame}\n").as_bytes(),
    );
    let client = http();
    let body = wait_healthy(&client, u64::from(fixed), &mut child).await;
    assert_eq!(body["registered"], false);
    // 固定端口的路径不变：请求值就是实际值。
    let admin_status = client
        .get(format!("http://127.0.0.1:{admin}/admin/api/v1/status"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert!(admin_status.status().is_success());
    assert_eq!(dir_listing(&env.run_dir), Vec::<String>::new());
    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[tokio::test]
async fn the_legacy_admin_token_flag_writes_no_discovery_file_and_has_the_new_health_fields() {
    let env = Env::new();
    let (port, admin) = (closed_port(), closed_port());
    let mut child = env
        .command(&port.to_string(), &admin.to_string())
        .args(["serve", "--admin-token-stdin"])
        .spawn_guarded()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{TOKEN}\n").as_bytes())
        .unwrap();
    let client = http();
    let body = wait_healthy(&client, u64::from(port), &mut child).await;
    assert_eq!(body["registered"], false);
    assert!(body["phase"].is_string() && body["upstreams"].is_array());
    assert_eq!(dir_listing(&env.run_dir), Vec::<String>::new());
    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[test]
fn the_two_stdin_flags_together_are_a_usage_error_with_exit_code_1() {
    let env = Env::new();
    for args in [
        ["serve", "--launcher-stdin", "--admin-token-stdin"],
        ["serve", "--admin-token-stdin", "--launcher-stdin"],
    ] {
        let mut child = env.command("0", "0").args(args).spawn_guarded().unwrap();
        drop(child.stdin.take());
        let status = wait_exit(&mut child, Duration::from_secs(10));
        let (stdout, stderr) = output_of(&mut child);
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("用法: idoris"), "{stderr}");
        assert!(!stdout.contains("listening"), "{stdout}");
    }
}

#[test]
fn the_admin_cli_refuses_to_connect_to_the_dynamic_port_request_value() {
    let env = Env::new();
    let mut child = env
        .command("0", "0")
        .args(["admin", "status", "--token-stdin"])
        .spawn_guarded()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{TOKEN}\n").as_bytes())
        .unwrap();
    let status = wait_exit(&mut child, Duration::from_secs(10));
    let (_, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("动态端口"), "{stderr}");
    assert!(!stderr.contains(TOKEN));
}

// ---------------------------------------------------------------------------------------------------------------
// 发现文件路径校验：失败即退出码 1，此时尚未对外服务，且什么都不写
// ---------------------------------------------------------------------------------------------------------------

fn assert_startup_refused(env: &Env, frame: &Value, expected: &str, listing_before: &[String]) {
    let mut child = env.spawn_launcher(frame);
    let status = wait_exit(&mut child, Duration::from_secs(30));
    let (stdout, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(1), "{expected}: {stderr}");
    assert!(
        stderr.contains(&format!("[idoris] 启动失败：{expected}")),
        "{expected}: {stderr}"
    );
    // 尚未对外服务，更没有打印监听地址。
    assert!(!stdout.contains("listening"), "{expected}: {stdout}");
    assert!(!format!("{stdout}{stderr}").contains(TOKEN));
    // 什么都没写：目录列表没变（没有发现文件、没有旁路锁、没有临时文件）。
    assert_eq!(dir_listing(&env.run_dir), listing_before, "{expected}");
}

#[test]
fn discovery_path_validation_failures_exit_1_before_serving_and_write_nothing() {
    let env = Env::new();
    let dir = env.run_dir.clone();

    // 父目录不存在：iDoris 不创建。
    let missing = dir.join("run-missing");
    assert_startup_refused(
        &env,
        &env.frame(json!({"discovery_path": missing.join(FILE_NAME)})),
        "发现文件不可用（parent_missing）",
        &[],
    );
    assert!(
        !missing.exists(),
        "iDoris must not create the parent directory"
    );

    // 父目录经过符号链接：launcher 必须传 canonical 路径。
    let real = dir.join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.join("link");
    symlink(&real, &link).unwrap();
    let listing = dir_listing(&dir);
    assert_startup_refused(
        &env,
        &env.frame(json!({"discovery_path": link.join(FILE_NAME)})),
        "发现文件不可用（parent_not_canonical）",
        &listing,
    );
    assert_eq!(dir_listing(&real), Vec::<String>::new());

    // 组或其他人可写。
    for (mode, label) in [
        (0o770, "group-writable"),
        (0o777, "world-writable"),
        (0o702, "other-writable"),
    ] {
        let open = dir.join(format!("open-{mode:o}"));
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(mode)).unwrap();
        let listing = dir_listing(&dir);
        assert_startup_refused(
            &env,
            &env.frame(json!({"discovery_path": open.join(FILE_NAME)})),
            "发现文件不可用（parent_writable_by_others）",
            &listing,
        );
        assert_eq!(dir_listing(&open), Vec::<String>::new(), "{label}");
    }

    // 目标已存在但不是普通文件：拒绝，且不删除、不跟随。
    let victim = dir.join("victim.txt");
    fs::write(&victim, b"precious").unwrap();
    let target_dir = dir.join("targets");
    fs::create_dir(&target_dir).unwrap();
    fs::set_permissions(&target_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let target = target_dir.join(FILE_NAME);

    symlink(&victim, &target).unwrap();
    let listing = dir_listing(&target_dir);
    assert_startup_refused(
        &env,
        &env.frame(json!({"discovery_path": &target})),
        "发现文件不可用（target_not_regular）",
        &dir_listing(&dir),
    );
    assert_eq!(dir_listing(&target_dir), listing);
    assert_eq!(fs::read(&victim).unwrap(), b"precious");
    assert!(
        fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&target).unwrap();

    fs::create_dir(&target).unwrap();
    assert_startup_refused(
        &env,
        &env.frame(json!({"discovery_path": &target})),
        "发现文件不可用（target_not_regular）",
        &dir_listing(&dir),
    );
    assert!(target.is_dir());
    fs::remove_dir(&target).unwrap();

    nix::unistd::mkfifo(&target, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    assert_startup_refused(
        &env,
        &env.frame(json!({"discovery_path": &target})),
        "发现文件不可用（target_not_regular）",
        &dir_listing(&dir),
    );
    assert!(target.exists());
}

#[test]
fn a_discovery_directory_owned_by_someone_else_is_refused() {
    // 根目录属于 root。以 root 运行时这个前提不成立，跳过；只在非 root 下，iDoris 会在校验处停下，什么都不写。
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    let frame = env.frame(json!({"discovery_path": "/idoris-s1-discovery-test.json"}));
    let mut child = env.spawn_launcher(&frame);
    let status = wait_exit(&mut child, Duration::from_secs(30));
    let (stdout, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("[idoris] 启动失败：发现文件不可用（parent_wrong_owner）"),
        "{stderr}"
    );
    assert!(!stdout.contains("listening"), "{stdout}");
    assert!(!Path::new("/idoris-s1-discovery-test.json").exists());
}

#[test]
fn a_relative_or_oversized_discovery_path_is_a_frame_error() {
    let env = Env::new();
    for (path, expected) in [
        (json!("run/idoris.json"), "field_invalid：discovery_path"),
        (
            json!(format!("/{}", "a".repeat(1100))),
            "field_invalid：discovery_path",
        ),
        (json!(7), "field_invalid：discovery_path"),
    ] {
        let mut child = env.spawn_launcher(&env.frame(json!({"discovery_path": path})));
        let status = wait_exit(&mut child, Duration::from_secs(30));
        let (stdout, stderr) = output_of(&mut child);
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains(&format!("[idoris] 启动失败：launcher 帧无效（{expected}）")),
            "{stderr}"
        );
        assert!(!stdout.contains("listening"), "{stdout}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 崩溃残留、第二实例、正常退出
// ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn kill_9_leaves_a_recognizable_stale_file_and_a_new_instance_overwrites_it_atomically() {
    let env = Env::new();
    let path = env.discovery_path();
    let mut first = env.spawn_launcher(&env.frame(json!({})));
    let (stale, stale_bytes) = wait_discovery(&path, &mut first);
    let (stale_pid, stale_id) = (
        stale["pid"].as_u64().unwrap(),
        stale["instance_id"].as_str().unwrap().to_string(),
    );
    let stale_data_port = stale["data_port"].as_u64().unwrap();
    health(&http(), stale_data_port).await;

    // 读者一直在读：任何时刻读到的都必须是完整的 JSON（崩溃、覆盖期间也是）。它读到过哪些 instance_id
    // 记在共享集合里，主线程据此等到它真的见过旧文件和新文件（机器忙时读线程可能晚一点才被调度到）。
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(std::sync::Mutex::new(
        std::collections::BTreeSet::<String>::new(),
    ));
    let reader = {
        let (path, stop, seen) = (path.clone(), stop.clone(), seen.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(bytes) = fs::read(&path) {
                    let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                        panic!("partial file ({} bytes): {error}", bytes.len())
                    });
                    assert!(bytes.ends_with(b"\n"));
                    seen.lock()
                        .unwrap()
                        .insert(value["instance_id"].as_str().unwrap().to_string());
                }
            }
        })
    };
    let wait_until_seen = |id: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !seen.lock().unwrap().contains(id) {
            assert!(
                Instant::now() < deadline,
                "the reader never observed instance {id}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_until_seen(&stale_id);

    send("-KILL", &first);
    let status = wait_exit(&mut first, Duration::from_secs(10));
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );

    // 崩溃残留：文件还在，内容原样；但 pid 已死、旧端口上没有人答话，所以消费者能认出它是陈旧的。
    assert_eq!(fs::read(&path).unwrap(), stale_bytes);
    assert!(!pid_alive(stale_pid), "pid {stale_pid} should be gone");
    // 旧端口上要么没有人答话，要么答话的是别的实例（端口一释放就可能被别的进程重新绑走）；
    // 绝不会再有人以残留文件里的 instance_id 答话。
    if let Ok(response) = http()
        .get(format!("http://127.0.0.1:{stale_data_port}/health"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        let answered: Value = response.json().await.unwrap_or_default();
        assert_ne!(
            answered["instance_id"],
            json!(stale_id),
            "nobody may answer on the stale port as the dead instance"
        );
    }

    // 第二种识别方式：旧端口上现在是另一个实例在答话（端口被别的进程占用的情形），它的
    // `/health.instance_id` 与残留文件里的不符。这个实例用的是另一个数据库和旧的 `--admin-token-stdin`，
    // 不会碰发现文件。
    let other_state = tempfile::tempdir().unwrap();
    let mut impostor = env.command(&stale_data_port.to_string(), "0");
    impostor.env("IDORIS_DB_PATH", other_state.path().join("other.sqlite3"));
    let mut impostor = impostor
        .args(["serve", "--admin-token-stdin"])
        .spawn_guarded()
        .unwrap();
    impostor
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{TOKEN}\n").as_bytes())
        .unwrap();
    let answered = wait_healthy(&http(), stale_data_port, &mut impostor).await;
    assert_ne!(
        answered["instance_id"],
        json!(stale_id),
        "a different instance answering on the stale port does not match the stale file"
    );
    assert_eq!(fs::read(&path).unwrap(), stale_bytes);
    let (status, _, stderr) = sigterm_and_collect(&mut impostor);
    assert_eq!(status.code(), Some(0), "{stderr}");

    // 再起一个实例（实例锁已经被内核释放）：拿到锁的新实例把陈旧文件视为陈旧并覆盖。
    let mut second = env.spawn_launcher(&env.frame(json!({})));
    let deadline = Instant::now() + Duration::from_secs(30);
    let fresh = loop {
        let (value, _) = wait_discovery(&path, &mut second);
        if value["instance_id"] != json!(stale_id) {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "the stale file was never overwritten"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_ne!(fresh["pid"].as_u64().unwrap(), stale_pid);
    assert_ne!(
        fresh["instance_nonce"], stale["instance_nonce"],
        "a new nonce for a new life"
    );
    assert_eq!(fresh["pid"].as_u64().unwrap(), u64::from(second.id()));
    assert_eq!(
        health(&http(), fresh["data_port"].as_u64().unwrap()).await["instance_id"],
        fresh["instance_id"]
    );

    // 读者见过旧文件，也见过新文件；而且从头到尾没有读到过半截内容（读线程里的断言）。
    wait_until_seen(fresh["instance_id"].as_str().unwrap());
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();

    let (status, _, stderr) = sigterm_and_collect(&mut second);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(
        !path.exists(),
        "the new instance removes its own file on a graceful stop"
    );
}

#[tokio::test]
async fn second_instance_exits_3_before_reading_the_frame_and_never_touches_the_discovery_file() {
    let env = Env::new();
    let path = env.discovery_path();
    let first_started = Instant::now();
    let mut first = env.spawn_launcher(&env.frame(json!({})));
    let (before, before_bytes) = wait_discovery(&path, &mut first);
    // 第一个实例从启动到写出发现文件用了多久：第二实例走到实例锁的那一段是它的真子集。
    let first_startup = first_started.elapsed();
    let before_meta = fs::metadata(&path).unwrap();
    let listing = dir_listing(&env.run_dir);

    // 第二实例：同一个数据库、同一个发现文件路径。stdin 一个字节都不写也不关：它在绑定端口、读帧之前就该退出 3。
    let second_started = Instant::now();
    let mut second = env
        .command("0", "0")
        .args(["serve", "--launcher-stdin"])
        .spawn_guarded()
        .unwrap();
    let held_open = second.stdin.take().unwrap();
    let status = wait_exit(&mut second, Duration::from_secs(30));
    let second_elapsed = second_started.elapsed();
    let (stdout, stderr) = output_of(&mut second);
    drop(held_open);
    assert_eq!(status.code(), Some(3), "{stderr}");
    // 它根本没有去读 stdin（实例锁排在读帧、绑定端口之前）：如果它等了读帧的 10 秒超时，耗时会比第一个实例
    // 从启动到写出发现文件还多出十秒；机器再忙，两者的差也远小于 8 秒。
    assert!(
        second_elapsed < first_startup + Duration::from_secs(8),
        "exit 3 took {second_elapsed:?} (the first instance needed {first_startup:?} to start): the second instance must not wait for the frame"
    );
    assert!(
        stderr.contains("[idoris] 启动失败：已有 iDoris 实例在使用同一数据库"),
        "{stderr}"
    );
    assert!(!stdout.contains("listening"), "{stdout}");

    // 文件原样：内容、inode、mtime 都没变，目录里没有多出任何东西。
    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    let after_meta = fs::metadata(&path).unwrap();
    assert_eq!(after_meta.ino(), before_meta.ino());
    assert_eq!(after_meta.mtime_nsec(), before_meta.mtime_nsec());
    assert_eq!(after_meta.mtime(), before_meta.mtime());
    assert_eq!(dir_listing(&env.run_dir), listing);
    // 第一个实例毫发无损。
    assert_eq!(
        health(&http(), before["data_port"].as_u64().unwrap()).await["instance_id"],
        before["instance_id"]
    );

    // 第二实例用另一个发现文件路径也一样：那个文件根本不会出现。
    let other = tempfile::tempdir().unwrap();
    let other_dir = canonical_private_dir(&other);
    let frame = env.frame(json!({"discovery_path": other_dir.join(FILE_NAME)}));
    let mut third = env.spawn_launcher(&frame);
    let status = wait_exit(&mut third, Duration::from_secs(30));
    assert_eq!(status.code(), Some(3));
    assert_eq!(dir_listing(&other_dir), Vec::<String>::new());

    let (status, _, stderr) = sigterm_and_collect(&mut first);
    assert_eq!(status.code(), Some(0), "{stderr}");
}

#[tokio::test]
async fn sigterm_and_sigint_remove_our_discovery_file_and_leave_the_sidecar_lock() {
    for signal in ["-TERM", "-INT"] {
        let env = Env::new();
        let path = env.discovery_path();
        let mut child = env.spawn_launcher(&env.frame(json!({})));
        let (file, _) = wait_discovery(&path, &mut child);
        health(&http(), file["data_port"].as_u64().unwrap()).await;
        send(signal, &child);
        let status = wait_exit(&mut child, Duration::from_secs(20));
        let (stdout, stderr) = output_of(&mut child);
        assert_eq!(status.code(), Some(0), "{signal}: {stderr}");
        assert!(stdout.contains("idoris: 已停止"), "{signal}: {stdout}");
        assert!(
            !path.exists(),
            "{signal}: the discovery file must be removed"
        );
        // 旁路锁文件本身从不删除（删除锁文件会让持锁的双方各持一把）。
        assert_eq!(dir_listing(&env.run_dir), ["idoris.json.lock"], "{signal}");
    }
}

#[tokio::test]
async fn a_file_that_belongs_to_another_instance_is_left_alone_on_shutdown() {
    let env = Env::new();
    let path = env.discovery_path();
    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (file, _) = wait_discovery(&path, &mut child);
    health(&http(), file["data_port"].as_u64().unwrap()).await;

    // 别的实例（比如被接管之后的新实例）已经把文件换成了它自己的。注意 S2 起运行中的实例每 10 秒自愈一次
    // （缺失或 `instance_id` 不一致就重写，接口规范 §2.7.3），所以「别人的文件原样保留」只对退出时的删除成立：
    // 这里换文件之后立刻 SIGTERM，远在第一轮自愈（发布之后 10 秒）之前。
    let mut foreign = file.clone();
    foreign["instance_id"] = json!("11111111-2222-4333-8444-555555555555");
    foreign["pid"] = json!(1);
    let replacement = env.run_dir.join("replacement.tmp");
    fs::write(&replacement, format!("{foreign}\n")).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let foreign_bytes = fs::read(&path).unwrap();

    let (status, _, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(
        fs::read(&path).unwrap(),
        foreign_bytes,
        "only a file that carries our instance_id may be removed"
    );
}

/// 退出时删除要等旁路锁（至多 2 秒）：消费者正持着它，等不到就放弃。文件保留、原样（成为崩溃残留，下一个实例会覆盖），
/// 日志是一行固定文案：不含路径、`instance_id`、nonce；退出码仍是 0。
#[test]
fn a_sidecar_lock_held_at_shutdown_keeps_the_file_and_logs_one_fixed_line() {
    let env = Env::new();
    let path = env.discovery_path();
    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (_, bytes) = wait_discovery(&path, &mut child);

    // 消费者（Agent24）正持着旁路锁清理陈旧文件。
    let consumer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(env.run_dir.join("idoris.json.lock"))
        .unwrap();
    consumer.lock().unwrap();

    let started = Instant::now();
    let (status, stdout, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stdout.contains("idoris: 已停止"), "{stdout}");
    // 它等过锁，不是一开始就放弃（等锁时限 2 秒；下界是安全的，上界不断言）。
    assert!(
        started.elapsed() >= Duration::from_millis(1_500),
        "{:?}",
        started.elapsed()
    );

    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the file must be kept as is"
    );
    assert_eq!(
        kept_file_log_lines(&stderr),
        ["[idoris] 发现文件未删除：旁路锁不可用或等待超时（文件保留，下一个实例会覆盖）"],
        "{stderr}"
    );
    consumer.unlock().unwrap();
}

/// 退出时 unlink 失败（目录对属主也不可写）：文件读得到、旁路锁文件早已存在也打得开，只有 unlink 得到 `EACCES`。
/// 文件保留、原样，日志是一行固定文案，退出码仍是 0。
/// 同上，但管道的读端没了（launcher 先走一步）：「发现文件未删除」那一行、`已停止` 那一行都写不出去，退出码仍是 0
/// （用 `eprintln!` 写的时候这里 panic，退出码是 101）。
#[test]
fn a_sidecar_lock_held_at_shutdown_still_exits_0_when_nobody_reads_its_stdout_and_stderr() {
    let env = Env::new();
    let path = env.discovery_path();
    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (_, bytes) = wait_discovery(&path, &mut child);
    let consumer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(env.run_dir.join("idoris.json.lock"))
        .unwrap();
    consumer.lock().unwrap();

    drop(child.stdout.take());
    drop(child.stderr.take());
    send("-TERM", &child);
    let status = wait_exit(&mut child, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the file must be kept as is"
    );
    consumer.unlock().unwrap();
}

#[test]
fn a_file_that_cannot_be_unlinked_at_shutdown_is_kept_and_one_fixed_line_is_logged() {
    // root 不受目录权限限制，没有这个前提：跳过。
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    let path = env.discovery_path();
    let mut child = env.spawn_launcher(&env.frame(json!({})));
    let (_, bytes) = wait_discovery(&path, &mut child);

    fs::set_permissions(&env.run_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let _restore = RestoreMode(env.run_dir.clone());

    let (status, stdout, stderr) = sigterm_and_collect(&mut child);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stdout.contains("idoris: 已停止"), "{stdout}");
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the file must be kept as is"
    );
    assert_eq!(
        kept_file_log_lines(&stderr),
        ["[idoris] 发现文件未删除：删除失败（文件保留，下一个实例会覆盖）"],
        "{stderr}"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// 帧错误：退出码 1，在服务任何请求之前，只有固定文案，绝不回显字段值
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn invalid_frames_exit_1_before_serving_and_never_echo_the_token_or_any_field_value() {
    let env = Env::new();
    let nonce = "feedc0de".repeat(8);
    let secret_path = "/Users/secret-user-canary/.agent24/run/idoris.json";
    let secret_exe = "/Users/secret-user-canary/omlx-cli";
    let long = format!(
        "{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"pad\":\"{}\"}}\n",
        "x".repeat(4100)
    );
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("frame_too_long", long.into_bytes(), "frame_too_long"),
        (
            "no_lf_then_eof",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\"}}").into_bytes(),
            "frame_not_json",
        ),
        ("empty_stdin", Vec::new(), "frame_not_json"),
        ("not_json", b"this is not json\n".to_vec(), "frame_not_json"),
        (
            "token_inside_broken_json",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",,}}\n").into_bytes(),
            "frame_not_json",
        ),
        ("json_array", format!("[\"{TOKEN}\"]\n").into_bytes(), "frame_not_json"),
        (
            "wrong_version",
            format!("{{\"v\":2,\"admin_token\":\"{TOKEN}\"}}\n").into_bytes(),
            "unsupported_frame_version",
        ),
        (
            "missing_v",
            format!("{{\"admin_token\":\"{TOKEN}\"}}\n").into_bytes(),
            "field_invalid：v",
        ),
        ("missing_token", b"{\"v\":1}\n".to_vec(), "field_invalid：admin_token"),
        (
            "uppercase_token",
            format!("{{\"v\":1,\"admin_token\":\"{}\"}}\n", TOKEN.to_uppercase()).into_bytes(),
            "field_invalid：admin_token",
        ),
        (
            "short_token",
            format!("{{\"v\":1,\"admin_token\":\"{}\"}}\n", &TOKEN[..63]).into_bytes(),
            "field_invalid：admin_token",
        ),
        (
            "unknown_field",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"bogus_field\":\"{nonce}\"}}\n").into_bytes(),
            "unknown_field：bogus_field",
        ),
        (
            "token_as_field_name",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"{TOKEN}\":1}}\n").into_bytes(),
            "unknown_field",
        ),
        (
            "register_socket_without_discovery_path",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"register_socket\":\"{secret_path}.sock\"}}\n")
                .into_bytes(),
            "field_invalid：register_socket",
        ),
        (
            "orphan_timeout_without_socket",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"discovery_path\":\"{secret_path}\",\"orphan_timeout_s\":900}}\n")
                .into_bytes(),
            "field_invalid：orphan_timeout_s",
        ),
        (
            "orphan_timeout_too_large",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"discovery_path\":\"{secret_path}\",\"register_socket\":\"/tmp/s.sock\",\"orphan_timeout_s\":86401}}\n")
                .into_bytes(),
            "field_invalid：orphan_timeout_s",
        ),
        (
            "bad_nonce",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"instance_nonce\":\"{}\"}}\n", nonce.to_uppercase())
                .into_bytes(),
            "field_invalid：instance_nonce",
        ),
        (
            "engine_unknown_subfield",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"engine\":{{\"bogus\":\"{secret_exe}\"}}}}\n").into_bytes(),
            "unknown_field：engine.bogus",
        ),
        (
            "engine_unknown_kind",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"engine\":{{\"executables\":{{\"lmstudio\":\"{secret_exe}\"}}}}}}\n")
                .into_bytes(),
            "field_invalid：engine.executables",
        ),
        (
            "engine_relative_executable",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"engine\":{{\"executables\":{{\"omlx\":\"relative/omlx-cli\"}}}}}}\n")
                .into_bytes(),
            "field_invalid：engine.executables",
        ),
        (
            "engine_bad_stop_on_exit",
            format!("{{\"v\":1,\"admin_token\":\"{TOKEN}\",\"engine\":{{\"stop_on_exit\":\"{nonce}\"}}}}\n").into_bytes(),
            "field_invalid：engine.stop_on_exit",
        ),
    ];
    for (label, bytes, expected) in cases {
        let mut child = env.spawn_launcher_bytes("0", "0", &bytes);
        let status = wait_exit(&mut child, Duration::from_secs(30));
        let (stdout, stderr) = output_of(&mut child);
        assert_eq!(status.code(), Some(1), "{label}: {stderr}");
        assert!(
            stderr.contains(&format!("[idoris] 启动失败：launcher 帧无效（{expected}）")),
            "{label}: {stderr}"
        );
        // 在服务任何请求之前：没有打印监听地址。
        assert!(!stdout.contains("listening"), "{label}: {stdout}");
        // 绝不回显字段值：token、nonce、路径、可执行文件都不出现，哪怕只是前缀。
        let combined = format!("{stdout}{stderr}");
        for secret in [
            TOKEN,
            nonce.as_str(),
            secret_path,
            secret_exe,
            "secret-user-canary",
        ] {
            assert!(
                !combined.contains(secret),
                "{label} echoed {secret}: {combined}"
            );
        }
        assert!(!combined.contains(&TOKEN[..16]), "{label}: {combined}");
        assert!(
            !combined.contains(&TOKEN.to_uppercase()[..16]),
            "{label}: {combined}"
        );
    }
}

#[tokio::test]
async fn nothing_is_served_while_waiting_for_the_frame_and_nothing_after_a_bad_one() {
    let env = Env::new();
    let (port, admin) = (closed_port(), closed_port());
    let mut child = env
        .command(&port.to_string(), &admin.to_string())
        .args(["serve", "--launcher-stdin"])
        .spawn_guarded()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let client = http();
    // 端口已经绑定（连接会进内核的 backlog），但数据面在读到合法的帧之前不会应答任何请求。
    let waited_until = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < waited_until {
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited while waiting for the frame"
        );
        let answered = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(150))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        assert!(
            !answered,
            "the data plane answered before the launcher frame was read"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 一帧无效：退出 1，之后端口关闭。
    stdin.write_all(b"{\"v\":9}\n").unwrap();
    drop(stdin);
    let status = wait_exit(&mut child, Duration::from_secs(30));
    let (stdout, stderr) = output_of(&mut child);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(!stdout.contains("listening"), "{stdout}");
    // 进程已经退出，内核随之关掉了它的全部 socket；不再断言「这个端口拒绝连接」：端口一释放，机器上别的进程
    // （比如并行的测试）随时可能把它重新绑走。
}

#[test]
fn a_launcher_that_never_writes_the_frame_times_out_after_10_seconds_with_exit_1() {
    let env = Env::new();
    let started = Instant::now();
    let mut child = env
        .command("0", "0")
        .args(["serve", "--launcher-stdin"])
        .spawn_guarded()
        .unwrap();
    // 保持 stdin 开着、一个字节也不写。
    let held_open = child.stdin.take().unwrap();
    let status = wait_exit(&mut child, Duration::from_secs(90));
    let (stdout, stderr) = output_of(&mut child);
    drop(held_open);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("[idoris] 启动失败：launcher 帧无效（timeout）"),
        "{stderr}"
    );
    assert!(!stdout.contains("listening"), "{stdout}");
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(9_500), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(80), "{elapsed:?}");
}

// ---------------------------------------------------------------------------------------------------------------
// instance_nonce 金丝雀
// ---------------------------------------------------------------------------------------------------------------

/// 响应头加响应体，渲染成一段文本。
async fn render(response: reqwest::Response) -> String {
    let headers = format!("{:?}", response.headers());
    let body = response.text().await.unwrap();
    format!("{headers}\n{body}")
}

/// 对一个正在运行的实例收集它能被读到的所有地方，返回拼在一起的文本。
async fn everything_observable(
    env: &Env,
    child: &Child,
    data_port: u64,
    admin_port: u64,
) -> String {
    let client = http();
    let mut seen = String::new();
    let mut take = |label: &str, text: String| {
        seen.push_str(&format!("\n--- {label} ---\n{text}"));
    };
    for url in [
        format!("http://127.0.0.1:{data_port}/health"),
        format!("http://127.0.0.1:{data_port}/v1/models"),
        format!("http://127.0.0.1:{data_port}/capabilities"),
        format!("http://127.0.0.1:{data_port}/does-not-exist"),
        format!("http://127.0.0.1:{admin_port}/admin/"),
    ] {
        let response = client.get(&url).send().await.unwrap();
        take(&url, render(response).await);
    }
    for resource in ["status", "backends", "models", "roles", "runtimes", "nope"] {
        let url = format!("http://127.0.0.1:{admin_port}/admin/api/v1/{resource}");
        let response = client
            .get(&url)
            .header("authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .unwrap();
        take(&url, render(response).await);
    }
    // 错误体：坏 JSON、没有 key、未知字段。
    for body in [
        "{not json",
        "{\"model\":\"idoris/daily\",\"messages\":[],\"bogus\":1}",
    ] {
        let response = client
            .post(format!("http://127.0.0.1:{data_port}/v1/chat/completions"))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        take("chat error", render(response).await);
    }
    // argv 与环境变量：nonce 只经 stdin 与发现文件，不进 argv / env。
    let pid = child.id().to_string();
    for args in [["-o", "args=", "-p"], ["eww", "-o", "command="]] {
        let output = Command::new("ps").args(args).arg(&pid).output().unwrap();
        take("ps", String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let _ = env;
    seen
}

#[tokio::test]
async fn the_instance_nonce_never_appears_outside_the_discovery_file() {
    // 两轮：一次用金丝雀 nonce（帧里给的；规范约定这个字段只给确定性测试用），一次用 iDoris 自己生成的随机 nonce。
    for supply_canary in [true, false] {
        let env = Env::new();
        let canary = "c0ffee00deadbeef".repeat(4);
        let frame = if supply_canary {
            env.frame(json!({"instance_nonce": canary}))
        } else {
            env.frame(json!({}))
        };
        let mut child = env.spawn_launcher(&frame);
        let (file, bytes) = wait_discovery(&env.discovery_path(), &mut child);
        let nonce = file["instance_nonce"].as_str().unwrap().to_string();
        if supply_canary {
            assert_eq!(nonce, canary, "the frame's nonce is used verbatim");
        }
        // 对照：发现文件里它恰好出现一次（说明下面的扫描确实能发现泄漏）。
        assert_eq!(String::from_utf8_lossy(&bytes).matches(&nonce).count(), 1);

        let observed = everything_observable(
            &env,
            &child,
            file["data_port"].as_u64().unwrap(),
            file["admin_port"].as_u64().unwrap(),
        )
        .await;
        let (status, stdout, stderr) = sigterm_and_collect(&mut child);
        assert_eq!(status.code(), Some(0), "{stderr}");
        let everything = format!("{observed}\n{stdout}\n{stderr}");
        for probe in [nonce.as_str(), &nonce[..16], &nonce[48..]] {
            assert!(
                !everything.contains(probe),
                "the nonce leaked (probe {probe}): {everything}"
            );
        }
        assert!(!everything.contains(&nonce.to_uppercase()));
        // Admin token 同样不出现在 stdout / stderr 里。
        assert!(!format!("{stdout}{stderr}").contains(TOKEN));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// 公开 API：parent 打开之后路径被替换、读者并发读
// ---------------------------------------------------------------------------------------------------------------

fn sample_record(pid: u32) -> DiscoveryRecord {
    DiscoveryRecord {
        instance_id: "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11".to_string(),
        instance_nonce: InstanceNonce::from_hex(&"ab".repeat(32)).unwrap(),
        pid,
        data_port: 51234,
        admin_port: 51235,
        version: "0.6.0".to_string(),
        contract_version: "1.3.0".to_string(),
        started_at_ms: 1_763_625_330_123,
        launch: None,
    }
}

#[test]
fn replacing_the_directory_path_after_the_parent_was_opened_still_writes_into_the_original_directory()
 {
    let temp = tempfile::tempdir().unwrap();
    let base = canonical_private_dir(&temp);
    let original = base.join("run");
    let moved = base.join("run-moved");
    let attacker = base.join("attacker");
    for directory in [&original, &attacker] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let target = DiscoveryTarget::open(&original.join(FILE_NAME)).unwrap();

    // 竞态：校验通过、parent 已经打开之后，路径被换成指向攻击者目录的符号链接。
    fs::rename(&original, &moved).unwrap();
    symlink(&attacker, &original).unwrap();

    target.publish(&sample_record(1)).unwrap();
    assert!(
        moved.join(FILE_NAME).is_file(),
        "the file lands in the directory that was validated"
    );
    assert_eq!(
        fs::read_dir(&attacker).unwrap().count(),
        0,
        "nothing may follow the replaced path"
    );
    assert_eq!(
        target.remove_if_owner("3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11"),
        RemoveOutcome::Removed
    );
    assert_eq!(fs::read_dir(&attacker).unwrap().count(), 0);
}

#[test]
fn the_public_api_refuses_unsafe_parents_with_stable_error_codes() {
    let temp = tempfile::tempdir().unwrap();
    let base = canonical_private_dir(&temp);
    let open = base.join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        DiscoveryTarget::open(&open.join(FILE_NAME)).unwrap_err(),
        DiscoveryError::ParentWritableByOthers
    );
    let link = base.join("link");
    symlink(&base, &link).unwrap();
    assert_eq!(
        DiscoveryTarget::open(&link.join(FILE_NAME)).unwrap_err(),
        DiscoveryError::ParentNotCanonical
    );
    assert_eq!(
        DiscoveryTarget::open(Path::new("relative.json")).unwrap_err(),
        DiscoveryError::PathInvalid
    );
    assert_eq!(
        DiscoveryError::ParentWritableByOthers.to_string(),
        "发现文件不可用（parent_writable_by_others）"
    );
}

#[test]
fn concurrent_readers_never_observe_a_partial_file_while_the_public_api_rewrites_it() {
    let temp = tempfile::tempdir().unwrap();
    let base = canonical_private_dir(&temp);
    let path = base.join(FILE_NAME);
    let target = DiscoveryTarget::open(&path).unwrap();
    target.publish(&sample_record(0)).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let readers = (0..4)
        .map(|_| {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut reads = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    let bytes = fs::read(&path).unwrap();
                    let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                        panic!("partial file ({} bytes): {error}", bytes.len())
                    });
                    assert!(bytes.ends_with(b"\n"));
                    assert_eq!(value["schema"], 1);
                    reads += 1;
                }
                reads
            })
        })
        .collect::<Vec<_>>();
    for round in 1..=200_u32 {
        target.publish(&sample_record(round)).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let total: u64 = readers
        .into_iter()
        .map(|reader| reader.join().unwrap())
        .sum();
    assert!(total > 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["pid"],
        200
    );
}
