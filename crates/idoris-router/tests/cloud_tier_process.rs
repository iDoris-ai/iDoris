//! 云端档（计算服务 S7）的进程级测试：真实的 `idoris` 二进制、真实的环境变量、真实的 TCP。
//!
//! 库里的测试证明规则；这里证明规则被**接上了**：
//!
//! - `idoris cloud pair | status | unpair` 的真实行为（对 S5 的云形态应用，挂在 `127.0.0.1` 的 TCP 上）：成功、重复配对、
//!   错误码、过期码、已用码、云端不可达、本机写不进去（兑换之前预检，配对码不被烧掉）、解绑时云端可达 / 不可达；
//! - 真实进程里：启动时**零出网**；`local_only` 不出网；`any` 没同意不出网；`any` + 同意 + 本机服务不了才走云端；
//! - **云凭证不出现在任何日志、Admin v0 输出、数据库、事件日志里**（只在 0600 的设备凭证文件里）；
//! - 三类密钥互不通用；启动时该拒绝的形态和凭证都被拒绝。
//!
//! 本机的 oMLX 指向一个关着的端口（运行时「不在线」）：不连接任何真实的外部服务，不启动任何推理进程。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/cloud_tier_harness.rs"]
mod h;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use h::*;
use serde_json::{Value, json};
use tempfile::TempDir;

/// 进程级测试彼此串行：同一台机器上同时起很多 `idoris` 会互相拖慢首次 exec。
static PROCESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Fixture {
    work: TempDir,
    components: TempDir,
    policy: PathBuf,
}

fn fixture() -> Fixture {
    let work = TempDir::new().unwrap();
    let components = TempDir::new().unwrap();
    // 本机 oMLX 指向一个关着的端口：运行时「不在线」
    let closed = free_port();
    let raw = std::fs::read_to_string(repo().join("config/components/omlx.yaml"))
        .unwrap()
        .replace(":8000", &format!(":{closed}"));
    std::fs::write(components.path().join("omlx.yaml"), raw).unwrap();
    let policy = work.path().join("routing-policy.yaml");
    std::fs::write(&policy, POLICY_ALLOWING_REMOTE).unwrap();
    Fixture {
        work,
        components,
        policy,
    }
}

impl Fixture {
    fn db(&self) -> PathBuf {
        self.work.path().join("idoris.sqlite3")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.work.path().join(name)
    }

    fn state_dir(&self) -> PathBuf {
        self.path("cloud-tier")
    }

    /// 个人形态的云端档配置文件（设备凭证放在 `state_dir`）。
    fn tier_config(&self, cloud: &TcpCloud) -> PathBuf {
        let path = self.path("cloud-tier.yaml");
        std::fs::write(&path, tier_config_yaml(cloud, Some(&self.state_dir()), "")).unwrap();
        path
    }

    fn command(&self, env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
        {
            command.env_remove(key);
        }
        command
            .current_dir(repo())
            .env("IDORIS_DB_PATH", self.db())
            .envs(env.iter().copied());
        command
    }
}

/// 在阻塞线程里跑一个一次性命令（云在测试进程里，不能把运行时卡住）。
async fn run(mut command: Command, stdin: Option<String>) -> Output {
    tokio::task::spawn_blocking(move || {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(input) = stdin {
            pipe.write_all(input.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn json_of(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("stdout is not JSON: {}", text(output).0))
}

impl Fixture {
    async fn cloud_cmd(
        &self,
        cloud_config: Option<&Path>,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Output {
        let mut env = Vec::new();
        if let Some(path) = cloud_config {
            env.push(("IDORIS_CLOUD_TIER_CONFIG", path.to_str().unwrap()));
        }
        let mut command = self.command(&env);
        command.arg("cloud").args(args);
        run(command, stdin.map(|code| format!("{code}\n"))).await
    }
}

/// 一次 CLI 调用的输出里不许出现任何密钥形状的东西（`idk_` 加十六进制，或 64 位十六进制）。
fn assert_no_secret(output: &Output, secrets: &[&str]) {
    let (out, err) = text(output);
    for stream in [&out, &err] {
        for secret in secrets {
            assert!(!stream.contains(secret), "a secret was printed: {stream}");
        }
        assert!(
            !stream.contains("idk_"),
            "something key-shaped was printed: {stream}"
        );
    }
}

fn device_secret(state_dir: &Path) -> String {
    let device = idoris_router::cloud_tier::DeviceStore::new(state_dir)
        .read()
        .unwrap()
        .unwrap();
    device.secret.expose_secret().to_string()
}

// ================================================================================================
// idoris cloud pair / status / unpair
// ================================================================================================

#[tokio::test]
async fn the_pairing_lifecycle_works_through_the_real_binary_and_never_prints_a_secret() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    let admin = cloud.issue_agent24_admin();

    // 还没配对
    let status = f.cloud_cmd(Some(&config), &["status"], None).await;
    assert_eq!(status.status.code(), Some(0), "{:?}", text(&status));
    assert_eq!(json_of(&status)["paired"], false);

    // 配对：配对码从 stdin 来
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    let paired = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(paired.status.code(), Some(0), "{:?}", text(&paired));
    let secret = device_secret(&f.state_dir());
    assert_no_secret(&paired, &[&secret]);
    let out = json_of(&paired);
    assert_eq!(out["paired"], true);
    assert_eq!(out["membership"], "m-agent-1");
    let prefix = out["credential_id_prefix"].as_str().unwrap().to_string();
    assert_eq!(prefix.len(), 11);
    // 凭证文件：0600；目录 0700
    let device_file = idoris_router::cloud_tier::DeviceStore::new(f.state_dir()).path();
    assert_eq!(
        std::fs::metadata(&device_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(f.state_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    // 配对码和密钥都不在命令行输出里，也不在 argv（配对码走 stdin：上面传的就是 stdin）
    assert!(!text(&paired).0.contains(&code) && !text(&paired).1.contains(&code));

    // status：账号绑定、凭证 id 前缀，没有密钥
    let status = f.cloud_cmd(Some(&config), &["status"], None).await;
    assert_eq!(status.status.code(), Some(0));
    assert_no_secret(&status, &[&secret]);
    let status_json = json_of(&status);
    assert_eq!(status_json["paired"], true);
    assert_eq!(status_json["membership"], "m-agent-1");
    assert_eq!(status_json["credential_id_prefix"], prefix);
    assert_eq!(status_json["origin_matches_config"], true);
    assert!(status_json.get("secret").is_none());
    // 这张凭证在云端是有效的：调用得通（付款人是成员账户 a1）
    let cred = Cred {
        id: String::new(),
        secret: secret.clone(),
    };
    let reply = cloud.world.send(&Req::chat(&cred)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());

    // 重复配对：拒绝，而且不烧掉新的配对码
    let second_code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    let again = f
        .cloud_cmd(Some(&config), &["pair"], Some(&second_code))
        .await;
    assert_eq!(again.status.code(), Some(1));
    assert!(text(&again).1.contains("已经配对"), "{:?}", text(&again));
    assert_no_secret(&again, &[&secret]);
    assert_eq!(
        device_secret(&f.state_dir()),
        secret,
        "the existing pairing must be untouched"
    );

    // 解绑（云端可达）：云端撤销 + 本机删除
    let unpaired = f.cloud_cmd(Some(&config), &["unpair"], None).await;
    assert_eq!(unpaired.status.code(), Some(0), "{:?}", text(&unpaired));
    assert_no_secret(&unpaired, &[&secret]);
    assert_eq!(json_of(&unpaired)["cloud_revoked"], true);
    assert!(!device_file.exists());
    let reply = cloud.world.send(&Req::chat(&cred)).await;
    assert_eq!(
        reply.status, 401,
        "the cloud must have revoked the credential"
    );
    let status = f.cloud_cmd(Some(&config), &["status"], None).await;
    assert_eq!(json_of(&status)["paired"], false);
    // 没有配对时再解绑：什么都不做，退出码 0
    let nothing = f.cloud_cmd(Some(&config), &["unpair"], None).await;
    assert_eq!(nothing.status.code(), Some(0));
    assert_eq!(json_of(&nothing)["unpaired"], false);

    // 没烧掉的第二个配对码现在可以用了（证明重复配对没有消耗它，解绑没有留下状态）
    let paired = f
        .cloud_cmd(
            Some(&config),
            &["pair", "--label", "Jason's MacBook"],
            Some(&second_code),
        )
        .await;
    assert_eq!(paired.status.code(), Some(0), "{:?}", text(&paired));
    // 已经用过的码：再来一次（先解绑）被云端拒绝
    let unpaired = f.cloud_cmd(Some(&config), &["unpair"], None).await;
    assert_eq!(unpaired.status.code(), Some(0));
    let reused = f
        .cloud_cmd(Some(&config), &["pair"], Some(&second_code))
        .await;
    assert_eq!(reused.status.code(), Some(1));
    assert!(text(&reused).1.contains("用过"), "{:?}", text(&reused));
    assert!(
        !device_file.exists(),
        "a refused code must leave no credential behind"
    );
}

#[tokio::test]
async fn wrong_expired_and_malformed_codes_are_refused_with_clear_messages_and_leave_nothing_behind()
 {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    let admin = cloud.issue_agent24_admin();
    let device_file = idoris_router::cloud_tier::DeviceStore::new(f.state_dir()).path();

    for (stdin, expected) in [
        ("ABCD-EFGH-JKMN", "配对码不对"), // 格式对，查无此码
        ("not a code", "配对码不对"),     // 格式不对（云端同一个答案）
        ("", "没有从 stdin 读到配对码"),
        (&"X".repeat(100), "太长"),
    ] {
        let out = f.cloud_cmd(Some(&config), &["pair"], Some(stdin)).await;
        assert_eq!(out.status.code(), Some(1), "{stdin:?}: {:?}", text(&out));
        assert!(
            text(&out).1.contains(expected),
            "{stdin:?}: {:?}",
            text(&out)
        );
        assert!(!device_file.exists());
        assert_no_secret(&out, &[]);
    }
    // 过期：有效期最短 30 秒，把云端的时钟拨过去
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 30).await;
    cloud.world.clock.advance_ms(60_000);
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("过期"), "{:?}", text(&out));
    assert!(!device_file.exists());
    // 新鲜的码照常可以用
    let fresh = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&fresh)).await;
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out));
}

#[tokio::test]
async fn an_unreachable_cloud_does_not_burn_the_code_and_unpair_still_removes_the_local_credential()
{
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let mut cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;

    // 云端不可达时配对：失败，没有留下凭证，码没有被消耗
    cloud.stop().await;
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("连接不上云端"), "{:?}", text(&out));
    let device_file = idoris_router::cloud_tier::DeviceStore::new(f.state_dir()).path();
    assert!(!device_file.exists());
    cloud.restart().await;
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out));
    let secret = device_secret(&f.state_dir());

    // 云端不可达时解绑：本机凭证仍然被删除，明确提示云端那张可能仍然有效
    cloud.stop().await;
    let out = f.cloud_cmd(Some(&config), &["unpair"], None).await;
    assert_eq!(out.status.code(), Some(2), "{:?}", text(&out));
    assert!(
        !device_file.exists(),
        "the local credential must be deleted regardless"
    );
    let (stdout, stderr) = text(&out);
    assert!(stderr.contains("可能仍然有效"), "{stderr}");
    assert!(stderr.contains("控制面撤销"), "{stderr}");
    let out_json: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(out_json["cloud_revoked"], false);
    assert_eq!(out_json["local_credential_deleted"], true);
    assert_no_secret(&out, &[&secret]);
    // 证据：云端那张凭证确实还有效（提示不是危言耸听）
    cloud.restart().await;
    let reply = cloud
        .world
        .send(&Req::chat(&Cred {
            id: String::new(),
            secret,
        }))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
}

/// 跨进程（审查 r4 Medium#4）：这个测试进程占着状态目录的锁，真实的 `idoris cloud pair` / `unpair` 子进程都不动手——
/// 不兑换配对码（码不会被烧掉）、不碰云端、不碰凭证文件。锁放开之后同一个码照常能用。
#[tokio::test]
async fn pair_and_unpair_do_nothing_while_another_process_holds_the_state_directory_lock() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    cloud.log.clear();
    let store = idoris_router::cloud_tier::DeviceStore::new(f.state_dir());
    // 这个测试进程就是「另一个进程」
    let lock = store.try_lock(true).unwrap().expect("a lock");

    let (pair, unpair) = tokio::join!(
        f.cloud_cmd(Some(&config), &["pair"], Some(&code)),
        f.cloud_cmd(Some(&config), &["unpair"], None)
    );
    for (name, out) in [("pair", &pair), ("unpair", &unpair)] {
        assert_eq!(out.status.code(), Some(1), "{name}: {:?}", text(out));
        assert!(
            text(out).1.contains("正在这台机器上运行"),
            "{name}: {:?}",
            text(out)
        );
        assert!(
            text(out).1.contains("什么都没做"),
            "{name}: {:?}",
            text(out)
        );
    }
    assert_eq!(
        cloud.log.count(),
        0,
        "a command that could not take the lock must not reach the cloud (and must not burn the code)"
    );
    assert!(!store.path().exists());

    drop(lock);
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "the code must still be unused once the lock is free: {:?}",
        text(&out)
    );
}

#[tokio::test]
async fn pairing_checks_that_it_can_save_the_credential_before_it_spends_the_one_time_code() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    // 设备凭证目录不可写
    let locked = f.path("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::File::create(locked.join(".probe")).is_ok() {
        return; // root：只读目录不拦人，这个用例没有意义
    }
    let config = f.path("locked.yaml");
    std::fs::write(
        &config,
        tier_config_yaml(&cloud, Some(&locked.join("tier")), ""),
    )
    .unwrap();
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("没有兑换配对码"), "{:?}", text(&out));
    assert!(
        cloud
            .log
            .all()
            .iter()
            .all(|seen| !seen.path.ends_with("/pairing/redeem")),
        "the code was spent although nothing could be saved"
    );
    // 目录修好之后，同一个码仍然可用
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = f.cloud_cmd(Some(&config), &["pair"], Some(&code)).await;
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out));
}

#[tokio::test]
async fn the_cloud_commands_need_a_configuration_and_only_make_sense_for_the_personal_form() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    // 没有配置：没有云端档
    for args in [&["pair"][..], &["status"], &["unpair"]] {
        let out = f.cloud_cmd(None, args, Some("ABCD-EFGH-JKMN")).await;
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out).1.contains("IDORIS_CLOUD_TIER_CONFIG"),
            "{:?}",
            text(&out)
        );
    }
    // 组织形态的配置（Node 凭证）：不配对
    let node_config = f.path("node.yaml");
    std::fs::write(
        &node_config,
        tier_config_yaml(
            &cloud,
            None,
            "credential: {source: node_env, env: IDORIS_TEST_NODE_CREDENTIAL}",
        ),
    )
    .unwrap();
    for args in [&["pair"][..], &["unpair"]] {
        let out = f
            .cloud_cmd(Some(&node_config), args, Some("ABCD-EFGH-JKMN"))
            .await;
        assert_eq!(out.status.code(), Some(1));
        assert!(text(&out).1.contains("个人形态"), "{:?}", text(&out));
    }
    assert_eq!(
        cloud.log.count(),
        0,
        "a refused command must not reach the cloud"
    );
    // 坏用法
    for args in [
        &["pair", "--code", "x"][..],
        &["bogus"],
        &["status", "extra"],
        &[],
    ] {
        let out = f.cloud_cmd(Some(&node_config), args, None).await;
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            text(&out).1.contains("idoris cloud pair"),
            "{:?}",
            text(&out)
        );
    }
    // 不带 `cloud` 子命令参数的整体用法里也有它
    let mut command = f.command(&[]);
    command.arg("nope");
    let out = run(command, None).await;
    assert!(text(&out).1.contains("idoris cloud status"));
}

// ================================================================================================
// 真实进程里的 serve
// ================================================================================================

struct Serving {
    child: Option<Child>,
    port: u16,
    admin_port: u16,
    admin_token: String,
    data_key: String,
    http: reqwest::Client,
}

impl Drop for Serving {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Serving {
    /// 停掉进程，返回它的 (stdout, stderr)。
    fn stop(mut self) -> (String, String) {
        let mut child = self.child.take().unwrap();
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn chat(&self, headers: &[(&str, &str)]) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
        let mut request = self
            .http
            .post(self.url("/v1/chat/completions"))
            .bearer_auth(&self.data_key)
            .json(&json!({"model": "small", "messages": [{"role": "user", "content": "hello cloud tier"}]}))
            .timeout(Duration::from_secs(20));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        (status, headers, response.bytes().await.unwrap().to_vec())
    }

    async fn admin(&self, path: &str) -> (u16, String) {
        let response = self
            .http
            .get(format!(
                "http://127.0.0.1:{}/admin/api/v1/{path}",
                self.admin_port
            ))
            .bearer_auth(&self.admin_token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }
}

fn random_hex64() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 起一个真实的 `idoris serve`（本机 oMLX 不在线），等它健康。启动失败时 panic（要测启动被拒的用 `spawn_expecting_failure`）。
async fn serve(f: &Fixture, env: &[(&str, &str)], tenant: Option<&str>) -> Serving {
    let key = issue_data_key(f, env, tenant).await;
    let port = free_port();
    let admin_port = loop {
        let candidate = free_port();
        if candidate != port {
            break candidate;
        }
    };
    let admin_token = random_hex64();
    let mut command = f.command(env);
    command
        .arg("serve")
        .arg("--admin-token-stdin")
        .env("IDORIS_COMPONENTS_DIR", f.components.path())
        .env("IDORIS_ROUTING_POLICY", &f.policy)
        .env("IDORIS_CATALOG", repo().join("config/catalog.yaml"))
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(format!("{admin_token}\n").as_bytes())
        .unwrap();
    drop(stdin);
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut serving = Serving {
        child: Some(child),
        port,
        admin_port,
        admin_token,
        data_key: key,
        http: http.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(response) = http
            .get(serving.url("/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && response.status().is_success()
        {
            return serving;
        }
        if serving
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some()
        {
            let (out, err) = serving.stop();
            panic!("the process exited before it became healthy\nstdout: {out}\nstderr: {err}");
        }
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 离线签发一把本地数据密钥（和 launcher 一样：`idoris key issue --spec-stdin`）。
async fn issue_data_key(f: &Fixture, env: &[(&str, &str)], tenant: Option<&str>) -> String {
    let mut command = f.command(env);
    command.args(["key", "issue", "--spec-stdin"]);
    let spec = json!({
        "owner": "s7-test",
        "tenant_id": tenant,
        "allowed_privacy": ["local_only", "any"],
        "allowed_roles": ["fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto"],
    });
    let out = run(command, Some(spec.to_string())).await;
    assert!(out.status.success(), "{:?}", text(&out));
    json_of(&out)["secret"].as_str().unwrap().to_string()
}

/// 启动必须失败：以 1 退出，stderr 含 `expected`，从来没有监听过（stdout 为空）。
async fn start_must_fail(f: &Fixture, env: &[(&str, &str)], expected: &str) {
    let mut command = f.command(env);
    command
        .arg("serve")
        .env("IDORIS_COMPONENTS_DIR", f.components.path())
        .env("IDORIS_ROUTING_POLICY", &f.policy)
        .env("IDORIS_CATALOG", repo().join("config/catalog.yaml"))
        .env("IDORIS_PORT", free_port().to_string())
        .env("IDORIS_ADMIN_PORT", free_port().to_string());
    let output = tokio::time::timeout(Duration::from_secs(60), run(command, None))
        .await
        .expect("start-up did not fail in time");
    assert_eq!(output.status.code(), Some(1), "{:?}", text(&output));
    assert!(
        text(&output).0.is_empty(),
        "must not have started listening: {:?}",
        text(&output)
    );
    assert!(text(&output).1.contains(expected), "{:?}", text(&output));
}

/// 用真实的 CLI 配对，返回设备凭证的明文（测试里要拿它去扫描）。
async fn pair_with_cli(f: &Fixture, cloud: &TcpCloud, config: &Path) -> String {
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    let out = f.cloud_cmd(Some(config), &["pair"], Some(&code)).await;
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out));
    device_secret(&f.state_dir())
}

const ANY: (&str, &str) = ("x-idoris-privacy", "any");
const CONSENT: (&str, &str) = ("x-idoris-cloud-fallback", "allow");

#[tokio::test]
async fn a_running_personal_node_sends_nothing_at_start_up_and_nothing_for_local_only() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    pair_with_cli(&f, &cloud, &config).await;
    cloud.log.clear();

    let node = serve(
        &f,
        &[("IDORIS_CLOUD_TIER_CONFIG", config.to_str().unwrap())],
        None,
    )
    .await;
    // 启动之后、处理任何请求之前：零出网
    assert_eq!(
        cloud.log.count(),
        0,
        "the node dialled the cloud while starting up"
    );
    // 既有的面：health、models、capabilities、Admin v0 的五个只读接口——都不碰云
    assert!(
        node.http
            .get(node.url("/health"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    for path in ["/v1/models", "/capabilities"] {
        let _ = node
            .http
            .get(node.url(path))
            .bearer_auth(&node.data_key)
            .send()
            .await
            .unwrap();
    }
    for path in ["status", "backends", "models", "roles", "runtimes"] {
        let (status, _) = node.admin(path).await;
        assert_eq!(status, 200, "{path}");
    }
    assert_eq!(cloud.log.count(), 0);

    // local_only（显式、默认）、同意头、降级声明一应俱全：仍然零出网
    for headers in [
        vec![("x-idoris-privacy", "local_only")],
        vec![],
        vec![
            ("x-idoris-privacy", "local_only"),
            CONSENT,
            ("x-idoris-fallback", "next_in_chain"),
        ],
        vec![CONSENT],
    ] {
        let (status, response_headers, body) = node.chat(&headers).await;
        assert_eq!(
            status,
            503,
            "{headers:?}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
            "local_only_unavailable"
        );
        assert!(response_headers.get("x-idoris-cloud-attempt").is_none());
    }
    // any 但没有同意：零出网
    let (status, response_headers, _) = node.chat(&[ANY]).await;
    assert_eq!(status, 503);
    assert!(response_headers.get("x-idoris-cloud-attempt").is_none());
    assert_eq!(
        cloud.log.count(),
        0,
        "a request without consent or with local_only reached the cloud"
    );
    let (_, stderr) = node.stop();
    assert!(stderr.contains("云端档已启用"), "{stderr}");
}

#[tokio::test]
async fn a_running_node_falls_back_end_to_end_and_the_secret_appears_nowhere_but_its_own_file() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    let secret = pair_with_cli(&f, &cloud, &config).await;
    cloud.log.clear();
    let node = serve(
        &f,
        &[("IDORIS_CLOUD_TIER_CONFIG", config.to_str().unwrap())],
        None,
    )
    .await;

    // 本机运行时不在线 + any + 同意：云端档答
    let (status, headers, body) = node.chat(&[ANY, CONSENT]).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["x-idoris-served-locality"], "remote");
    assert_eq!(headers["x-idoris-degraded"], "true");
    assert!(headers.contains_key("x-idoris-cloud-record-id"));
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["choices"][0]["message"]["content"],
        "hello"
    );
    assert_eq!(cloud.log.chats().len(), 1);
    // 云端只认配对出来的那张凭证（Authorization 头里的就是文件里的那个，没有别的）
    assert!(cloud.log.chats()[0].has_authorization);

    // Admin v0 的每个输出、health / models / capabilities：没有云凭证，没有云端档的任何东西
    let mut observed = Vec::new();
    for path in ["status", "backends", "models", "roles", "runtimes"] {
        let (code, body) = node.admin(path).await;
        assert_eq!(code, 200);
        observed.push((format!("admin/{path}"), body));
    }
    for path in ["/health", "/v1/models", "/capabilities"] {
        let response = node
            .http
            .get(node.url(path))
            .bearer_auth(&node.data_key)
            .send()
            .await
            .unwrap();
        observed.push((path.to_string(), response.text().await.unwrap()));
    }
    let origin = cloud.origin();
    for (name, body) in &observed {
        assert!(
            !body.contains(&secret) && !body.contains("idk_"),
            "{name} leaked a key-shaped value: {body}"
        );
        assert!(
            !body.contains("idoris-cloud") && !body.contains(&origin),
            "{name} reveals the cloud tier: {body}"
        );
    }
    // 云凭证拿到本机的 Admin 端口：401；拿到本机数据面：401（本机库里没有它）
    let response = node
        .http
        .get(format!(
            "http://127.0.0.1:{}/admin/api/v1/status",
            node.admin_port
        ))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "a cloud credential must not open the local Admin"
    );
    let response = node
        .http
        .post(node.url("/v1/chat/completions"))
        .bearer_auth(&secret)
        .json(&json!({"model": "small", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "a cloud credential must not open the local data plane"
    );

    // 进程的 stdout / stderr：没有云凭证（也没有 prompt）
    let (stdout, stderr) = node.stop();
    for stream in [&stdout, &stderr] {
        assert!(
            !stream.contains(&secret),
            "the cloud credential reached the process output: {stream}"
        );
        assert!(
            !stream.contains("hello cloud tier"),
            "a prompt reached the process output"
        );
    }
    // 磁盘：凭证只在 0600 的设备凭证文件里；库、事件日志、配置、锁文件里一个字节都没有
    let device_file = idoris_router::cloud_tier::DeviceStore::new(f.state_dir()).path();
    let mut checked = 0;
    for entry in walk(f.work.path()) {
        if entry == device_file {
            continue;
        }
        let bytes = std::fs::read(&entry).unwrap_or_default();
        let content = String::from_utf8_lossy(&bytes);
        assert!(
            !content.contains(&secret),
            "{} contains the cloud credential",
            entry.display()
        );
        assert!(
            !content.contains("hello cloud tier"),
            "{} contains a prompt",
            entry.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 3,
        "the scan must actually have looked at the database files ({checked})"
    );
    // 本机留下的对账指针在库里（有云端 record id，没有金额和正文）
    let conn = rusqlite::Connection::open(f.db()).unwrap();
    let (outcome, cloud_id, tokens_in): (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT outcome, cloud_record_id, tokens_in FROM cloud_tier_calls",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(outcome, "served");
    assert!(cloud_id.is_some());
    assert_eq!(tokens_in, Some(12));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn without_the_configuration_variable_the_node_behaves_exactly_as_before() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let node = serve(&f, &[], None).await;
    // 同意头、any、本机服务不了：没有云端档，什么都不会出网
    let (status, headers, body) = node.chat(&[ANY, CONSENT]).await;
    assert_eq!(status, 503);
    assert!(headers.get("x-idoris-cloud-attempt").is_none());
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
        "local_only_unavailable"
    );
    let (stdout, stderr) = node.stop();
    assert!(
        !stdout.contains("云端档") && !stderr.contains("云端档"),
        "{stdout}{stderr}"
    );
    assert_eq!(cloud.log.count(), 0);
    // 数据库里没有云端档的表（没配置就不碰）
    let conn = rusqlite::Connection::open(f.db()).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'cloud_tier%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tables, 0,
        "an unconfigured node must not create cloud tier tables"
    );
    assert!(!f.state_dir().exists());
}

// ================================================================================================
// 三类密钥互不通用
// ================================================================================================

#[tokio::test]
async fn the_three_secrets_are_not_interchangeable_in_any_direction() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    let cloud_secret = pair_with_cli(&f, &cloud, &config).await;
    cloud.log.clear();
    let node = serve(
        &f,
        &[("IDORIS_CLOUD_TIER_CONFIG", config.to_str().unwrap())],
        None,
    )
    .await;

    // 1. 本机数据密钥调云：云不认（云的库里没有它）
    let reply = cloud
        .world
        .send(&Req::chat(&Cred {
            id: String::new(),
            secret: node.data_key.clone(),
        }))
        .await;
    assert_eq!(
        reply.status, 401,
        "a local data key must not open the cloud"
    );
    // 2. Admin token 调云：云不认
    let reply = cloud
        .world
        .send(&Req::chat(&Cred {
            id: String::new(),
            secret: node.admin_token.clone(),
        }))
        .await;
    assert_eq!(reply.status, 401, "the Admin token must not open the cloud");
    // 3. 云凭证拿到本机 Admin / 数据面：本机认不得它
    let response = node
        .http
        .get(format!(
            "http://127.0.0.1:{}/admin/api/v1/status",
            node.admin_port
        ))
        .bearer_auth(&cloud_secret)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "a cloud credential must not open the local Admin"
    );
    let response = node
        .http
        .post(node.url("/v1/chat/completions"))
        .bearer_auth(&cloud_secret)
        .json(&json!({"model": "small", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "a cloud credential must not open the local data plane"
    );
    // 本机数据密钥和 Admin token 互不通用
    let response = node
        .http
        .get(format!(
            "http://127.0.0.1:{}/admin/api/v1/status",
            node.admin_port
        ))
        .bearer_auth(&node.data_key)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "a local data key must not open Admin"
    );
    let response = node
        .http
        .post(node.url("/v1/chat/completions"))
        .bearer_auth(&node.admin_token)
        .json(&json!({"model": "small", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        401,
        "the Admin token must not open the data plane"
    );
    // 这一切之后，云端的唯一请求是上面两次被拒的探测（401），云端档自己没有发过任何请求
    assert!(cloud.log.chats().iter().all(|seen| seen.has_authorization));
    drop(node);

    // 4. Admin token 被当成 Node 凭证：配置层面就拒绝（格式不是 idk_ 加 64 位十六进制），启动失败，且没有发出任何请求
    let before = cloud.log.count();
    let admin_like = f.path("node-admin-token");
    std::fs::write(&admin_like, format!("{}\n", random_hex64())).unwrap();
    std::fs::set_permissions(&admin_like, std::fs::Permissions::from_mode(0o600)).unwrap();
    let org_config = f.path("org.yaml");
    std::fs::write(
        &org_config,
        tier_config_yaml(
            &cloud,
            None,
            &format!(
                "credential: {{source: node_file, file: {}}}",
                admin_like.display()
            ),
        ),
    )
    .unwrap();
    let tenants = f.path("tenants.yaml");
    std::fs::write(&tenants, "tenants:\n  - tenant_id: acme\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n").unwrap();
    let org_db = f.path("org.sqlite3");
    let org_env = [
        ("IDORIS_DB_PATH", org_db.to_str().unwrap()),
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_FORM", "organization"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
        ("IDORIS_CLOUD_TIER_CONFIG", org_config.to_str().unwrap()),
    ];
    start_must_fail(&f, &org_env, "格式不对").await;
    // 5. 本机的本地数据密钥被当成 Node 凭证：库里认得它，启动拒绝（本地密钥不发给云端）
    let local_key_file = f.path("node-local-key");
    let key = issue_data_key(
        &f,
        &[
            ("IDORIS_DB_PATH", org_db.to_str().unwrap()),
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_FORM", "organization"),
        ],
        Some("acme"),
    )
    .await;
    std::fs::write(&local_key_file, format!("{key}\n")).unwrap();
    std::fs::set_permissions(&local_key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        &org_config,
        tier_config_yaml(
            &cloud,
            None,
            &format!(
                "credential: {{source: node_file, file: {}}}",
                local_key_file.display()
            ),
        ),
    )
    .unwrap();
    start_must_fail(&f, &org_env, "本地数据密钥").await;
    assert_eq!(
        cloud.log.count(),
        before,
        "a refused credential must never be sent"
    );
}

// ================================================================================================
// 启动时该拒绝的
// ================================================================================================

#[tokio::test]
async fn start_up_refuses_the_forms_and_credentials_the_cloud_tier_must_not_accept() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let personal_config = f.tier_config(&cloud);
    let tenants = f.path("tenants.yaml");
    std::fs::write(&tenants, "tenants:\n  - tenant_id: acme\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n").unwrap();
    let node_secret = cloud.world.cred("node_agent24").secret.clone();
    let node_file_path = node_file(f.work.path(), &node_secret);
    let node_config = f.path("node.yaml");
    std::fs::write(
        &node_config,
        tier_config_yaml(
            &cloud,
            None,
            &format!(
                "credential: {{source: node_file, file: {}}}",
                node_file_path.display()
            ),
        ),
    )
    .unwrap();
    let tenants_str = tenants.to_str().unwrap().to_string();
    let personal = personal_config.to_str().unwrap().to_string();
    let node = node_config.to_str().unwrap().to_string();
    let fresh = |name: &str| f.path(name).to_str().unwrap().to_string();

    // 存量的 tenant 部署（没声明 IDORIS_FORM）
    let db = fresh("legacy.sqlite3");
    start_must_fail(
        &f,
        &[
            ("IDORIS_DB_PATH", &db),
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_TENANTS_CONFIG", &tenants_str),
            ("IDORIS_CLOUD_TIER_CONFIG", &node),
        ],
        "IDORIS_FORM=organization",
    )
    .await;
    // 组织形态配了个人设备配对
    let db = fresh("org.sqlite3");
    start_must_fail(
        &f,
        &[
            ("IDORIS_DB_PATH", &db),
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_FORM", "organization"),
            ("IDORIS_TENANTS_CONFIG", &tenants_str),
            ("IDORIS_CLOUD_TIER_CONFIG", &personal),
        ],
        "Node 凭证",
    )
    .await;
    // 个人形态配了 Node 凭证
    let db = fresh("personal.sqlite3");
    start_must_fail(
        &f,
        &[("IDORIS_DB_PATH", &db), ("IDORIS_CLOUD_TIER_CONFIG", &node)],
        "设备凭证",
    )
    .await;
    // 组织形态：Node 凭证文件其他用户可读 -> 启动失败
    std::fs::set_permissions(&node_file_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let db = fresh("org2.sqlite3");
    start_must_fail(
        &f,
        &[
            ("IDORIS_DB_PATH", &db),
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_FORM", "organization"),
            ("IDORIS_TENANTS_CONFIG", &tenants_str),
            ("IDORIS_CLOUD_TIER_CONFIG", &node),
        ],
        "不可用",
    )
    .await;
    // 配置文件坏了 / 不存在
    let broken = fresh("broken.yaml");
    std::fs::write(&broken, "schema_version: 1\nsurprise: true\n").unwrap();
    let db = fresh("personal2.sqlite3");
    start_must_fail(
        &f,
        &[
            ("IDORIS_DB_PATH", &db),
            ("IDORIS_CLOUD_TIER_CONFIG", &broken),
        ],
        "云端档配置",
    )
    .await;
    start_must_fail(
        &f,
        &[
            ("IDORIS_DB_PATH", &db),
            ("IDORIS_CLOUD_TIER_CONFIG", "/nonexistent/cloud-tier.yaml"),
        ],
        "无法读取云端档配置",
    )
    .await;
    assert_eq!(
        cloud.log.count(),
        0,
        "a refused start-up must not contact the cloud"
    );
}

#[tokio::test]
async fn an_organization_node_started_for_real_serves_through_its_node_credential() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let tenants = f.path("tenants.yaml");
    std::fs::write(&tenants, "tenants:\n  - tenant_id: acme\n    limit_minor: 1000000\n    billing_timezone: UTC\n    scope: paid_only\n").unwrap();
    let node_secret = cloud.world.cred("node_agent24").secret.clone();
    let file = node_file(f.work.path(), &node_secret);
    let config = f.path("node.yaml");
    std::fs::write(
        &config,
        tier_config_yaml(
            &cloud,
            None,
            &format!(
                "credential: {{source: node_file, file: {}}}",
                file.display()
            ),
        ),
    )
    .unwrap();
    let env = [
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_FORM", "organization"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
        ("IDORIS_CLOUD_TIER_CONFIG", config.to_str().unwrap()),
    ];
    let node = serve(&f, &env, Some("acme")).await;
    assert_eq!(cloud.log.count(), 0, "start-up must not contact the cloud");
    let (status, headers, body) = node
        .chat(&[ANY, CONSENT, ("x-idoris-tenant", "acme")])
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["x-idoris-served-locality"], "remote");
    // 云端只认组织付款账户，没有成员头
    assert_eq!(cloud.world.request_rows()[0].0, "agent24");
    assert!(!cloud.log.chats()[0].has_member_header);
    // local_only 照旧不出网
    cloud.log.clear();
    let (status, _, _) = node
        .chat(&[
            ("x-idoris-privacy", "local_only"),
            CONSENT,
            ("x-idoris-tenant", "acme"),
        ])
        .await;
    assert_eq!(status, 503);
    assert_eq!(cloud.log.count(), 0);
    let (stdout, stderr) = node.stop();
    assert!(!stdout.contains(&node_secret) && !stderr.contains(&node_secret));
    // Node 凭证不在数据库里
    let bytes = std::fs::read(f.db()).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(&node_secret));
}

#[tokio::test]
async fn a_key_that_does_not_allow_any_never_gets_as_far_as_the_cloud_tier() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start().await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    pair_with_cli(&f, &cloud, &config).await;
    cloud.log.clear();
    let env = [("IDORIS_CLOUD_TIER_CONFIG", config.to_str().unwrap())];
    let node = serve(&f, &env, None).await;
    // 第二把 key：隐私上限只有 local_only
    let mut command = f.command(&env);
    command.args(["key", "issue", "--spec-stdin"]);
    let spec = json!({
        "owner": "local-only-key", "tenant_id": null,
        "allowed_privacy": ["local_only"],
        "allowed_roles": ["fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto"],
    });
    let out = run(command, Some(spec.to_string())).await;
    assert!(out.status.success(), "{:?}", text(&out));
    let narrow_key = json_of(&out)["secret"].as_str().unwrap().to_string();
    let response = node
        .http
        .post(node.url("/v1/chat/completions"))
        .bearer_auth(&narrow_key)
        .header("x-idoris-privacy", "any")
        .header("x-idoris-cloud-fallback", "allow")
        .json(&json!({"model": "small", "messages": [{"role": "user", "content": "hello cloud tier"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["reason_code"],
        "VIRTUAL_KEY_PRIVACY_FORBIDDEN"
    );
    assert_eq!(
        cloud.log.count(),
        0,
        "a key that may not use any must not reach the cloud tier"
    );
}

#[tokio::test]
async fn a_caller_that_hangs_up_during_the_fallback_cancels_the_cloud_call_and_it_is_recorded() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let cloud = TcpCloud::start_with(WorldOptions::with_script(
        (0..4)
            .map(|_| {
                h::ingress::Behavior::slow_json(
                    200,
                    h::ingress::completion("ok", "slow", Some(h::ingress::usage(12, 30))),
                    Duration::from_millis(600),
                )
            })
            .collect(),
    ))
    .await;
    let f = fixture();
    let config = f.tier_config(&cloud);
    pair_with_cli(&f, &cloud, &config).await;
    cloud.log.clear();
    let node = serve(
        &f,
        &[("IDORIS_CLOUD_TIER_CONFIG", config.to_str().unwrap())],
        None,
    )
    .await;
    // 调用方只愿意等 250 ms：它挂断时，云端调用正在途中
    let result = node
        .http
        .post(node.url("/v1/chat/completions"))
        .bearer_auth(&node.data_key)
        .header("x-idoris-privacy", "any")
        .header("x-idoris-cloud-fallback", "allow")
        .json(&json!({"model": "small", "messages": [{"role": "user", "content": "hello cloud tier"}]}))
        .timeout(Duration::from_millis(250))
        .send()
        .await;
    assert!(result.is_err(), "the caller should have given up");
    // 本机把这次调用记成 cancelled（出网了，但没有答复，没有金额）
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcome = loop {
        let conn = rusqlite::Connection::open(f.db()).unwrap();
        let row: Option<(String, Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT outcome, cloud_record_id, tokens_in FROM cloud_tier_calls",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        if let Some(row) = row
            && row.0 != "pending"
        {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the call never reached a final outcome"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(outcome.0, "cancelled");
    assert_eq!(outcome.1, None);
    assert_eq!(outcome.2, None);
    assert_eq!(
        cloud.log.chats().len(),
        1,
        "nothing was retried after the caller left"
    );
}
