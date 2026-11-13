//! 计算服务 S6 儿童安全：**真实进程**里的验收。设计稿 §13：「edu 在安全服务超时时拒绝放行；改用具体模型名绕不过安全检查；
//! 全链路扫描确认没有正文落盘」。
//!
//! 前面的测试在进程内驱动路由；这里起真正的 `idoris` 二进制（真的监听端口、真的凭证认证、真的 SQLite 文件、
//! 真的 stderr 管道），证明启动接线是对的：
//!
//! - 配置里有儿童租户却没有 `safety:` 段、LLM 检查端点不满足许可表的儿童约束：**进程拒绝启动**（退出码 1，什么都没监听）；
//! - 只开规则检查器（`rules_only`）：启动日志里有一行警告；
//! - 正常启动后：走真实的 HTTP 和认证，成功 / 输入被拒 / 输出被拒 / 安全服务超时 / 旧路由 / 流式 / 冒用租户，
//!   然后**杀掉进程**（WAL 没有检查点，库里所有写过的东西都还在 WAL 里），扫描整个运行目录（数据库、WAL、SHM、配置、
//!   所有文件）和进程的 stdout / stderr：两个哨兵、孩子的泰语原文、模型的有害回答片段都不在任何地方。
//!
//! # 和云入口（S5）的衔接
//!
//! 云形态的数据面是云入口：调用方（租户、付款账户）由**凭证**推导，租户的数据处理要求来自云入口配置
//! （`IDORIS_CLOUD_INGRESS_CONFIG`），数据面只有 `/api/v1/models` 和 `/api/v1/chat/completions`。所以这里的儿童租户是
//! 云入口配置里声明了 `child_edu` 的 `eduai`（云上游配置里的静态调用方簿是空的），开户和凭证用本机运维命令
//! `idoris control bootstrap`，请求走 `/api/v1/chat/completions`；S6 原来针对旧路由的 403 在云形态变成 404（那些路由
//! 根本不挂载，请求连认证都过不了，更到不了任何上游），403 本身仍由进程内的 S6 测试在非云入口的应用上钉着。
//!
//! 全部对本地假上游进行，**不连接任何真实的推理或安全服务**。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_safety.rs"]
mod child_safety;
#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use child_safety::{
    ANSWER_SENTINEL, CHILD_SENTINEL, answer, benign_question, child_body, contains,
    content_needles, files_under, harmful_question, judge_allow,
};
use fake_upstream::{Behavior, FakeUpstream};
use harness::{Endpoint, Options};
use idoris_compute_contracts::ids::{AttemptId, AttemptIndex, RequestId};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::HoldState;
use serde_json::{Value, json};
use tempfile::TempDir;

static PROCESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 真实的二进制用真实的时钟判断端点许可表的核验有没有过期，所以夹具里的核验时间要按「现在」算，
/// 不能写死日期（写死的日期会在 90 天之后让这个测试悄悄变红）。
fn verified_ago(days: u64, hours: u64) -> &'static str {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let at = now_ms - (days as i64 * 24 + hours as i64) * 3_600_000;
    let text = idoris_compute_contracts::permit::format_verified_at(
        idoris_compute_contracts::ids::TimestampMs(at),
    )
    .unwrap();
    Box::leak(text.into_boxed_str())
}

fn recent() -> &'static str {
    verified_ago(0, 1)
}

/// 早就过期了（核验有效期最多 90 天）。
fn long_ago() -> &'static str {
    verified_ago(200, 0)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Running(Option<Child>);

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Running {
    /// 杀掉进程（不做任何优雅关闭：WAL 保持原样），返回 (stdout, stderr)。
    fn kill(mut self) -> (String, String) {
        let mut child = self.0.take().unwrap();
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

struct Fixture {
    root: TempDir,
    exe: PathBuf,
    cwd: PathBuf,
}

/// 和 `form_startup.rs` 同样的夹具：把二进制和它旁边的配置放进临时目录，让默认的相对路径能解析。
fn fixture() -> Fixture {
    let root = TempDir::new().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(bin.join("config/components")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_idoris"), bin.join("idoris")).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    for file in [
        "components/omlx.yaml",
        "routing-policy.yaml",
        "catalog.yaml",
    ] {
        let mut raw = fs::read_to_string(repo.join(file)).unwrap();
        if file == "components/omlx.yaml" {
            raw.push_str("\nextensions:\n  _degradation: Removing bindings disables aliases.\n  idoris_role_models: { daily: unavailable-chat }\n");
        }
        fs::write(bin.join("config").join(file), raw.replace(":8000", ":0")).unwrap();
    }
    let cwd = root.path().join("run");
    fs::create_dir_all(cwd.join("config/components")).unwrap();
    Fixture {
        exe: bin.join("idoris"),
        cwd,
        root,
    }
}

impl Fixture {
    fn db(&self) -> PathBuf {
        self.cwd.join("idoris-test.sqlite3")
    }

    fn command(&self, env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(&self.exe);
        for (key, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
        {
            cmd.env_remove(key);
        }
        cmd.current_dir(&self.cwd)
            .env("IDORIS_DB_PATH", self.db())
            .envs(env.iter().copied());
        cmd
    }

    fn tenants(&self) -> PathBuf {
        let path = self.cwd.join("tenants.yaml");
        fs::write(
            &path,
            "tenants:\n  - tenant_id: eduai\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n",
        )
        .unwrap();
        path
    }

    /// 写云上游配置（主上游 + 检查端点；静态调用方簿是空的）和云入口配置（儿童租户 `eduai`），返回云上游配置文件路径。
    fn cloud_config(&self, main_url: &str, judge_url: &str, safety_yaml: &str) -> PathBuf {
        self.cloud_config_with(
            Endpoint::child_edu_compliant("example-judge", judge_url.to_string())
                .verified(recent()),
            main_url,
            safety_yaml,
            true,
        )
    }

    fn cloud_config_with(
        &self,
        judge: Endpoint,
        main_url: &str,
        safety_yaml: &str,
        child: bool,
    ) -> PathBuf {
        let dir = self.cwd.join("cloud");
        fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("upstream.key");
        fs::write(&key_file, format!("{}\n", harness::KEY)).unwrap();
        let endpoints = vec![
            Endpoint::child_edu_compliant("example-zdr", main_url.to_string()).verified(recent()),
            judge,
        ];
        fs::write(
            dir.join("permits.yaml"),
            harness::permit_table_yaml(&endpoints),
        )
        .unwrap();
        // 租户 `eduai` 的数据处理要求在云入口配置里声明：child_edu（零留存 + 已签 DPA）或者成人。
        // 这里测的是儿童安全管线，不是限流（限流有自己的测试）：各级上限开到不会碰到。
        let tenant = if child {
            "  - account: eduai\n    data_classes: [child_edu]\n    max_retention: zdr\n    require_dpa: true\n    billing: paid\n"
        } else {
            "  - account: eduai\n    data_classes: [adult]\n    max_retention: with_exceptions\n    billing: paid\n"
        };
        let level = "{requests_per_minute: 6000, burst: 200, max_concurrent: 50}";
        let ingress = format!(
            "schema_version: 1\nlimits:\n  platform: {level}\n  tenant: {level}\n  member: {level}\ntenants:\n{tenant}"
        );
        fs::write(self.ingress_config(), ingress).unwrap();
        let options = Options {
            callers: Vec::new(),
            safety_yaml: safety_yaml.to_string(),
            routes: Some(vec![("idoris/agent", vec!["example-zdr"])]),
            ..Options::default()
        };
        let yaml = harness::config_yaml(&endpoints, key_file.to_str().unwrap(), &options);
        let path = dir.join("cloud.yaml");
        fs::write(&path, yaml).unwrap();
        path
    }

    fn ingress_config(&self) -> PathBuf {
        self.cwd.join("cloud").join("ingress.yaml")
    }

    fn server_env<'a>(
        &'a self,
        port: u16,
        admin: u16,
        tenants: &'a Path,
        cloud: &'a Path,
    ) -> Vec<(String, String)> {
        vec![
            ("IDORIS_PORT".into(), port.to_string()),
            ("IDORIS_ADMIN_PORT".into(), admin.to_string()),
            ("IDORIS_DEPLOY_MODE".into(), "tenant".into()),
            ("IDORIS_FORM".into(), "cloud".into()),
            (
                "IDORIS_TENANTS_CONFIG".into(),
                tenants.to_str().unwrap().into(),
            ),
            (
                "IDORIS_CLOUD_UPSTREAM_CONFIG".into(),
                cloud.to_str().unwrap().into(),
            ),
            (
                "IDORIS_CLOUD_INGRESS_CONFIG".into(),
                self.ingress_config().to_str().unwrap().into(),
            ),
        ]
    }

    fn spawn(&self, env: &[(String, String)]) -> Running {
        let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut cmd = self.command(&pairs);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match cmd.spawn() {
                Ok(child) => return Running(Some(child)),
                Err(err)
                    if err.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to spawn the copied idoris binary: {err}"),
            }
        }
    }

    /// 本机运维命令 `idoris control bootstrap --spec-stdin`（生产里运维开户、发凭证走的就是这条路），返回它输出的每一行 JSON。
    fn bootstrap(&self, env: &[(String, String)], spec: &Value) -> Vec<Value> {
        let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut child = self
            .command(&pairs)
            .args(["control", "bootstrap", "--spec-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(spec.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// 开户、充值、发凭证：`platform` -> `eduai`（和云入口配置里声明的租户同名，租户 = 付款账户祖先链里平台根之下的那个客户），
    /// 一张绑在 `eduai` 上的节点凭证（不带成员 id）。返回凭证明文。账户、凭证、充值行都不含正文。
    fn provision(&self, env: &[(String, String)]) -> String {
        let issued = self.bootstrap(
            env,
            &json!({
                "accounts": [
                    {"account_id": "platform", "kind": "platform", "unit": "THB"},
                    {"account_id": "eduai", "kind": "org", "parent": "platform", "unit": "THB"}
                ],
                "allocations": [{
                    "allocation_id": "a-eduai", "parent": "platform", "child": "eduai",
                    "amount": {"unit": "THB", "micros": 1_000_000_000_i64}, "purpose": "prepaid",
                    "valid_from_ms": 1_700_000_000_000_i64, "valid_until_ms": 4_000_000_000_000_i64
                }],
                "credentials": [{
                    "binding": {"kind": "node", "account": "eduai"},
                    "allowed_privacy": ["local_only"], "allowed_roles": []
                }]
            }),
        );
        issued
            .iter()
            .find(|line| line["kind"] == "credential")
            .expect("bootstrap issues the credential")["secret"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// 进程必须以退出码 1 退出、stderr 里有 `expected`，并且什么都没监听（stdout 空）。
fn must_fail(child: Running, expected: &str) -> String {
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            let output = child.0.take().unwrap().wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty(), "must not have started listening");
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(stderr.contains(expected), "{stderr}");
            return stderr;
        }
        assert!(Instant::now() < deadline, "start-up did not fail");
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn wait_healthy(child: &mut Running, port: u16) -> reqwest::Client {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && response.status().is_success()
        {
            return client;
        }
        assert!(
            child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "the process exited before it became healthy"
        );
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

struct Reply {
    status: u16,
    reason: Option<String>,
    /// 服务端给这次请求的记录号（`X-iDoris-Record-Id`），账本里的预留用它做身份。
    request_id: Option<String>,
    json: Value,
    raw: Vec<u8>,
}

/// 带凭证、不带任何多余头的请求（云入口里租户只来自凭证，请求用不着再说一遍）。
async fn chat(
    client: &reqwest::Client,
    port: u16,
    secret: &str,
    path: &str,
    body: &Value,
) -> Reply {
    chat_with(client, port, Some(secret), &[], path, body).await
}

/// 完整形式：凭证可以没有，还可以带任意额外的头（冒用租户、想关安全检查之类）。
async fn chat_with(
    client: &reqwest::Client,
    port: u16,
    secret: Option<&str>,
    headers: &[(&str, &str)],
    path: &str,
    body: &Value,
) -> Reply {
    let mut request = client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .json(body);
    if let Some(secret) = secret {
        request = request.bearer_auth(secret);
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let reason = response
        .headers()
        .get("x-idoris-reason")
        .map(|v| v.to_str().unwrap().to_string());
    let request_id = response
        .headers()
        .get("x-idoris-record-id")
        .map(|v| v.to_str().unwrap().to_string());
    let raw = response.bytes().await.unwrap().to_vec();
    Reply {
        status,
        reason,
        request_id,
        json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        raw,
    }
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_process_refuses_to_start_when_the_child_safety_configuration_is_missing_or_not_allowed()
 {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants();
    let port = free_port();
    let admin = free_port();
    let main = FakeUpstream::start(vec![]).await;
    let judge = FakeUpstream::start(vec![]).await;

    // 儿童调用方，没有 safety 段
    let cloud = f.cloud_config(&main.url(), &judge.url(), "");
    let env = f.server_env(port, admin, &tenants, &cloud);
    must_fail(f.spawn(&env), "必须配置 safety");

    // LLM 检查端点不允许 child_edu（只是成人端点）
    let mut adult_judge =
        Endpoint::child_edu_compliant("example-judge", judge.url()).verified(recent());
    adult_judge.classes = vec!["adult", "public"];
    let cloud = f.cloud_config_with(
        adult_judge,
        &main.url(),
        &child_safety::judge_yaml(1_000),
        true,
    );
    let env = f.server_env(port, admin, &tenants, &cloud);
    must_fail(f.spawn(&env), "不满足儿童数据的端点许可表硬约束");

    // 核验已过期的检查端点
    let stale = Endpoint::child_edu_compliant("example-judge", judge.url()).verified(long_ago());
    let cloud = f.cloud_config_with(stale, &main.url(), &child_safety::judge_yaml(1_000), true);
    let env = f.server_env(port, admin, &tenants, &cloud);
    must_fail(f.spawn(&env), "VerificationExpired");

    // mode 是必填的
    let cloud = f.cloud_config(&main.url(), &judge.url(), "safety:\n  timeout_ms: 1000\n");
    let env = f.server_env(port, admin, &tenants, &cloud);
    must_fail(f.spawn(&env), "mode");
    // 失败的启动没有留下任何服务的痕迹：没有监听
    assert!(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rules_only_starts_with_a_loud_warning_and_adult_only_configs_need_no_safety_section() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants();
    let main = FakeUpstream::start(vec![]).await;
    let judge = FakeUpstream::start(vec![]).await;

    // rules_only + 儿童调用方：能启动，但日志里有警告
    let cloud = f.cloud_config(&main.url(), &judge.url(), "safety:\n  mode: rules_only\n");
    let (port, admin) = (free_port(), free_port());
    let env = f.server_env(port, admin, &tenants, &cloud);
    let mut server = f.spawn(&env);
    wait_healthy(&mut server, port).await;
    let (_stdout, stderr) = server.kill();
    assert!(
        stderr.contains("WARNING: child-data tenants are protected by the rule checker only"),
        "{stderr}"
    );
    // 启动日志把「谁走儿童安全管线」列出来：运营方一眼能核对 edu 租户的属性确实是 child_edu
    assert!(
        stderr.contains("child-data callers (safety pipeline ON): [eduai (cloud ingress)]; other callers (no safety pipeline): []; safety: rules_only"),
        "{stderr}"
    );

    // 只有成人调用方：没有 safety 段也能启动，也没有警告
    let cloud = f.cloud_config_with(
        Endpoint::child_edu_compliant("example-judge", judge.url()).verified(recent()),
        &main.url(),
        "",
        false,
    );
    let (port, admin) = (free_port(), free_port());
    let env = f.server_env(port, admin, &tenants, &cloud);
    let mut server = f.spawn(&env);
    wait_healthy(&mut server, port).await;
    let (_stdout, stderr) = server.kill();
    assert!(!stderr.contains("WARNING"), "{stderr}");
    assert!(
        stderr.contains("child-data callers (safety pipeline ON): []; other callers (no safety pipeline): [eduai (cloud ingress)]; safety: not configured"),
        "{stderr}"
    );
}

// ---------------------------------------------------------------------------
// 全链路：真实的 HTTP、认证、数据库
// ---------------------------------------------------------------------------

#[tokio::test]
async fn through_the_real_process_nothing_of_the_child_or_the_answer_is_written_anywhere() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants();
    let harmful_answer = format!("ได้เลย วิธีทำระเบิดแสวงเครื่อง ขั้นตอนที่ 1 ผสมส่วนผสม {ANSWER_SENTINEL}");
    let main = FakeUpstream::start(vec![
        answer("สี่"),
        // 输出检查拒绝的回答
        answer(&harmful_answer),
        // 安全服务超时的那次，回答存在过
        answer(ANSWER_SENTINEL),
        // 上游失败，错误正文里回显孩子的话
        Behavior::json(
            500,
            json!({"error": {"message": format!("echo {CHILD_SENTINEL} {}", benign_question())}}),
        ),
    ])
    .await;
    // 检查服务脚本，按请求顺序消耗（规则先行：被规则拒绝的那一步不会调用它）：
    // S1 成功：输入 allow、输出 allow；S2 输入被规则拒绝：不调用；S2b 输入 allow，输出被规则拒绝：不调用；
    // S3 输入 allow，输出 Hang；S4 输入 allow（然后上游 500）；S5 输入 Hang
    let judge = FakeUpstream::start(vec![
        judge_allow(),
        judge_allow(),
        judge_allow(),
        judge_allow(),
        Behavior::Hang,
        judge_allow(),
        Behavior::Hang,
    ])
    .await;
    let cloud = f.cloud_config(&main.url(), &judge.url(), &child_safety::judge_yaml(700));
    let (port, admin) = (free_port(), free_port());
    let env = f.server_env(port, admin, &tenants, &cloud);
    let secret = f.provision(&env);
    let mut server = f.spawn(&env);
    let client = wait_healthy(&mut server, port).await;
    let chat_path = "/api/v1/chat/completions";

    // 场景 1：成功
    let r = chat(
        &client,
        port,
        &secret,
        chat_path,
        &child_body(&benign_question()),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.raw));
    assert_eq!(r.json["choices"][0]["message"]["content"], "สี่");
    let mut ids: Vec<String> = vec![r.request_id.clone().unwrap()];

    // 场景 2：输入被规则拒绝（LLM 不会被调用，所以不消耗检查服务的脚本）
    let r = chat(
        &client,
        port,
        &secret,
        chat_path,
        &child_body(&harmful_question()),
    )
    .await;
    assert_eq!(r.status, 422);
    assert_eq!(r.json["error"]["type"], "safety_input_blocked");
    assert_eq!(r.reason.as_deref(), Some("safety_input_blocked"));
    assert!(!contains(&r.raw, CHILD_SENTINEL));
    ids.push(r.request_id.clone().unwrap());

    // 场景 2b：输入检查（LLM）放行，输出被规则拒绝（有害回答），LLM 的输出检查不会被调用
    let r = chat(
        &client,
        port,
        &secret,
        chat_path,
        &child_body(&benign_question()),
    )
    .await;
    assert_eq!(r.status, 422, "{}", String::from_utf8_lossy(&r.raw));
    assert_eq!(r.json["error"]["type"], "safety_output_blocked");
    assert!(!contains(&r.raw, ANSWER_SENTINEL));
    ids.push(r.request_id.clone().unwrap());

    // 场景 3：输入放行，输出阶段安全服务超时：回答存在过，客户端什么都没收到
    let r = chat(
        &client,
        port,
        &secret,
        chat_path,
        &child_body(&benign_question()),
    )
    .await;
    assert_eq!(r.status, 503, "{}", String::from_utf8_lossy(&r.raw));
    assert_eq!(r.json["error"]["type"], "safety_unavailable");
    assert_eq!(r.json["error"]["stage"], "output");
    assert!(!contains(&r.raw, ANSWER_SENTINEL));
    ids.push(r.request_id.clone().unwrap());

    // 场景 4：输入放行，上游 500（错误正文回显孩子的话）
    let r = chat(
        &client,
        port,
        &secret,
        chat_path,
        &child_body(&benign_question()),
    )
    .await;
    assert_eq!(r.status, 502, "{}", String::from_utf8_lossy(&r.raw));
    assert!(!contains(&r.raw, CHILD_SENTINEL));
    ids.push(r.request_id.clone().unwrap());

    // 场景 5：输入阶段安全服务超时
    let r = chat(
        &client,
        port,
        &secret,
        chat_path,
        &child_body(&benign_question()),
    )
    .await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json["error"]["stage"], "input");
    ids.push(r.request_id.clone().unwrap());

    // 绕过：流式、不经过管线的路由、没有凭证、错误的凭证、冒用别的租户 / 想关掉安全检查
    let mut streaming = child_body(&benign_question());
    streaming["stream"] = json!(true);
    let r = chat(&client, port, &secret, chat_path, &streaming).await;
    assert_eq!(
        (r.status, &r.json["error"]["type"]),
        (400, &json!("streaming_not_allowed"))
    );
    // 云入口的数据面只有 /api/v1/models 和 /api/v1/chat/completions：不经过儿童安全管线的旧路由（还有它们在 /api/v1 下的
    // 同名路径）根本不挂载，404，请求连认证都过不了
    let other_body = json!({"model": "idoris/daily", "input": "x", "messages": [{"role": "user", "content": "x"}], "max_tokens": 4});
    for path in [
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/embeddings",
        "/v1/rerank",
        "/api/v1/messages",
        "/api/v1/embeddings",
        "/api/v1/rerank",
    ] {
        let r = chat(&client, port, &secret, path, &other_body).await;
        assert_eq!(r.status, 404, "{path}");
    }
    let r = chat_with(&client, port, None, &[], chat_path, &child_body("hi")).await;
    assert_eq!(r.status, 401);
    let r = chat(&client, port, "idk_not-a-key", chat_path, &child_body("hi")).await;
    assert_eq!(r.status, 401);
    // 冒用别的租户、写明自己不是儿童、要求关掉安全检查：租户和数据处理要求只从凭证推导，请求里写什么都没用，
    // 明显有害的输入照样在进程里被规则检查器拦下（不会调用任何上游，所以也不消耗上面的脚本）
    let r = chat_with(
        &client,
        port,
        Some(&secret),
        &[
            ("x-idoris-tenant", "someone-else"),
            ("x-idoris-safety", "off"),
            ("x-idoris-child", "false"),
        ],
        chat_path,
        &child_body(&harmful_question()),
    )
    .await;
    assert_eq!(r.status, 422, "{}", String::from_utf8_lossy(&r.raw));
    assert_eq!(r.json["error"]["type"], "safety_input_blocked");

    // 杀掉进程（WAL 保持原样），扫描整个运行目录和进程输出
    let (stdout, stderr) = server.kill();
    let mut paths = Vec::new();
    files_under(f.root.path(), &mut paths);
    let databases: Vec<&PathBuf> = paths
        .iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("idoris-test.sqlite3"))
        })
        .collect();
    assert!(
        databases
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "sqlite3"))
            && databases.len() >= 2,
        "the database and its WAL are scanned: {databases:?}"
    );
    let wal_bytes: usize = databases
        .iter()
        .filter(|p| p.to_string_lossy().ends_with("-wal"))
        .map(|p| fs::metadata(p).map_or(0, |m| m.len() as usize))
        .sum();
    assert!(
        wal_bytes > 0,
        "the WAL was not checkpointed away, so it holds everything that was written"
    );
    // 日志是真的在被捕获：有我们的日志行和稳定的码
    assert!(stderr.contains("[idoris] safety: request"), "{stderr}");
    assert!(stderr.contains("safety_input_blocked") && stderr.contains("safety_output_blocked"));
    assert!(stderr.contains("safety_unavailable"));
    for needle in content_needles() {
        for (name, bytes) in [("stdout", stdout.as_bytes()), ("stderr", stderr.as_bytes())] {
            assert!(
                !contains(bytes, &needle),
                "{needle:?} found in {name}:\n{stderr}"
            );
        }
        for path in &paths {
            let bytes = fs::read(path).unwrap_or_default();
            assert!(
                !contains(&bytes, &needle),
                "{needle:?} found in {}",
                path.display()
            );
        }
    }
    // 密钥同样不在任何地方（上游密钥、虚拟 key 明文）
    for secret_text in [harness::KEY, secret.as_str()] {
        for path in &paths {
            if path.ends_with("upstream.key") {
                continue;
            }
            let bytes = fs::read(path).unwrap_or_default();
            assert!(
                !contains(&bytes, secret_text),
                "a secret is in {}",
                path.display()
            );
        }
        assert!(!stderr.contains(secret_text) && !stdout.contains(secret_text));
    }

    // 最后才读账本（读完最后一个连接关闭会做 WAL 检查点，所以要在上面的扫描之后）：真实进程里安全检查的成本确实走了账本，
    // 每个阶段的预留都收了尾，没有留着不关的。场景顺序：成功 / 规则拦输入 / 规则拦输出 / 输出阶段检查服务超时 / 上游 500 / 输入阶段超时
    let accounts = AccountStore::open(f.db()).unwrap();
    let state = |id: &str, component: &str| {
        accounts
            .hold(
                &AttemptId::new(RequestId::new(id).unwrap(), AttemptIndex(0)),
                component,
            )
            .unwrap()
            .map(|row| row.state)
    };
    use HoldState::{Released, Settled};
    let expected = [
        // (输入检查, 输出检查, 回答本身)
        (Some(Settled), Some(Settled), Some(Settled)),
        // PR-Daemon S5 F4：免费的规则先于检查费用的预留，规则拦下的阶段根本没有预留（旧的预期是预留了再释放）
        (None, None, None),
        (Some(Settled), None, Some(Released)),
        (Some(Settled), Some(Released), Some(Released)),
        (Some(Settled), None, Some(Released)),
        (Some(Released), None, None),
    ];
    assert_eq!(ids.len(), expected.len());
    for (n, (id, (input, output, model))) in ids.iter().zip(expected).enumerate() {
        assert_eq!(
            state(id, "safety_input"),
            input,
            "scenario {n}: input check"
        );
        assert_eq!(
            state(id, "safety_output"),
            output,
            "scenario {n}: output check"
        );
        assert_eq!(state(id, "model"), model, "scenario {n}: the answer itself");
    }
}
