//! 计算服务 S5 的进程级测试：真实的 `idoris` 二进制，真实的环境变量，真实的 TCP。
//!
//! 证明接线是真的（库里的纯函数测试证明规则，这里证明规则被用上了）：
//! 云形态的启动要求、非云形态不能非 loopback、公网监听要有声明的反向代理、Admin v0 永远只听 loopback、
//! 云入口在真实进程里服务 `/api/v1`，以及一个从运维 bootstrap 到调用的完整路径。
//! 假上游是进程内的 `127.0.0.1` 假服务；不连接任何真实的推理供应商，也不启动任何推理进程。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::io::Write;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use fake_upstream::{Behavior, FakeUpstream, completion, usage};
use tempfile::TempDir;

static PROCESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    /// Stops the process and returns (stdout, stderr).
    fn stop(mut self) -> (String, String) {
        let mut child = self.0.take().unwrap();
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_port_except(other: u16) -> u16 {
    loop {
        let port = free_port();
        if port != other {
            return port;
        }
    }
}

/// 本机一个非 loopback 的 IPv4 地址。UDP `connect` 只是让内核选出站接口，不发任何包（和
/// `scripts/check-rust-egress.sh` 同一个办法，目标是文档保留网段 192.0.2.0/24）。没有就返回 `None`。
fn non_loopback_ip() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

struct Fixture {
    _root: TempDir,
    cwd: TempDir,
    exe: PathBuf,
}

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
    let cwd = TempDir::new().unwrap();
    fs::create_dir_all(cwd.path().join("config/components")).unwrap();
    Fixture {
        exe: bin.join("idoris"),
        _root: root,
        cwd,
    }
}

const INGRESS_YAML: &str = "schema_version: 1
tenants:
  - account: agent24
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
";

impl Fixture {
    fn db(&self) -> PathBuf {
        self.cwd.path().join("idoris-test.sqlite3")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.cwd.path().join(name)
    }

    /// 旧的租户预算清单（存量 tenant 部署和组织 / 云形态的启动仍然读它做一次性种子）。
    fn tenants_file(&self) -> PathBuf {
        let path = self.path("tenants.yaml");
        fs::write(
            &path,
            "tenants:\n  - tenant_id: acme\n    limit_minor: 100\n    billing_timezone: UTC\n    scope: all\n",
        )
        .unwrap();
        path
    }

    /// 云上游配置 + 许可表 + 假密钥文件；`callers: []`（付款上下文由凭证解析）。成人端点，默认的规则检查器 `safety:` 段。
    fn cloud_upstream_config(&self, upstream_url: &str) -> PathBuf {
        self.cloud_upstream_config_for(
            harness::Endpoint::adult("example-alpha", upstream_url.to_string()),
            None,
        )
    }

    /// 同上，端点和 `safety:` 段由调用方给（`Some("")` = 没有 `safety:` 段；`None` = 夹具默认的规则检查器）。
    fn cloud_upstream_config_for(
        &self,
        endpoint: harness::Endpoint,
        safety_yaml: Option<&str>,
    ) -> PathBuf {
        let endpoints = vec![endpoint];
        fs::write(self.path("upstream.key"), "sk-test-upstream-key\n").unwrap();
        fs::write(
            self.path("permits.yaml"),
            harness::permit_table_yaml(&endpoints),
        )
        .unwrap();
        let mut options = harness::Options {
            callers: Vec::new(),
            ..harness::Options::default()
        };
        if let Some(safety_yaml) = safety_yaml {
            options.safety_yaml = safety_yaml.to_string();
        }
        let yaml = harness::config_yaml(
            &endpoints,
            self.path("upstream.key").to_str().unwrap(),
            &options,
        );
        let path = self.path("cloud-upstream.yaml");
        fs::write(&path, yaml).unwrap();
        path
    }

    fn ingress_config(&self, extra: &str) -> PathBuf {
        let path = self.path("cloud-ingress.yaml");
        fs::write(&path, format!("{INGRESS_YAML}{extra}")).unwrap();
        path
    }

    fn command(&self, env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(&self.exe);
        for (key, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
        {
            cmd.env_remove(key);
        }
        cmd.current_dir(self.cwd.path())
            .env("IDORIS_DB_PATH", self.db())
            .envs(env.iter().copied());
        cmd
    }

    fn spawn(&self, port: u16, env: &[(&str, &str)]) -> Running {
        let admin_port = free_port_except(port);
        let mut cmd = self.command(env);
        cmd.env("IDORIS_PORT", port.to_string())
            .env("IDORIS_ADMIN_PORT", admin_port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match cmd.spawn() {
                Ok(child) => return Running(Some(child)),
                Err(err)
                    if err.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to spawn copied idoris binary: {err}"),
            }
        }
    }

    /// 本机运维命令 `idoris control bootstrap --spec-stdin`，返回它输出的每一行 JSON。
    fn bootstrap(&self, env: &[(&str, &str)], spec: &serde_json::Value) -> Vec<serde_json::Value> {
        let mut child = self
            .command(env)
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
}

/// 进程必须以 1 退出，stderr 含 `expected`，并且从来没有监听过（stdout 为空）。
fn failed(mut child: Running, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            let output = child.0.take().unwrap().wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty(), "must not have started listening");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(expected), "{stderr}");
            return;
        }
        assert!(Instant::now() < deadline, "start-up did not fail");
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn wait_health(child: &mut Running, host: &str, port: u16) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client
            .get(format!("http://{host}:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "the process exited before it became healthy"
        );
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

const TENANT_CLOUD: [(&str, &str); 2] =
    [("IDORIS_DEPLOY_MODE", "tenant"), ("IDORIS_FORM", "cloud")];

// ---------------------------------------------------------------------------
// 启动要求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cloud_form_requires_both_configurations_and_refuses_the_developer_escape_hatch() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants_file();
    let base = |extra: &[(&'static str, String)]| -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("IDORIS_DEPLOY_MODE", "tenant".to_string()),
            ("IDORIS_FORM", "cloud".to_string()),
            (
                "IDORIS_TENANTS_CONFIG",
                tenants.to_str().unwrap().to_string(),
            ),
        ];
        env.extend(extra.iter().cloned());
        env
    };
    let spawn = |env: Vec<(&'static str, String)>| {
        let borrowed: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        f.spawn(free_port(), &borrowed)
    };
    // 1. 有云入口配置，没有云上游配置
    let ingress_only = f.ingress_config("");
    failed(
        spawn(base(&[(
            "IDORIS_CLOUD_INGRESS_CONFIG",
            ingress_only.to_str().unwrap().to_string(),
        )])),
        "IDORIS_CLOUD_UPSTREAM_CONFIG",
    );
    // 一个都没有：云入口配置先加载（它声明的儿童租户要参与云上游的启动校验），所以先指出它
    failed(spawn(base(&[])), "IDORIS_CLOUD_INGRESS_CONFIG");
    // 2. 有云上游配置，没有云入口配置
    let upstream = f.cloud_upstream_config("http://127.0.0.1:1/v1");
    failed(
        spawn(base(&[(
            "IDORIS_CLOUD_UPSTREAM_CONFIG",
            upstream.to_str().unwrap().to_string(),
        )])),
        "IDORIS_CLOUD_INGRESS_CONFIG",
    );
    // 3. 云入口配置坏了：启动失败并指出文件
    let broken = f.path("broken-ingress.yaml");
    fs::write(&broken, "schema_version: 1\nsurprise: true\n").unwrap();
    failed(
        spawn(base(&[
            (
                "IDORIS_CLOUD_UPSTREAM_CONFIG",
                upstream.to_str().unwrap().to_string(),
            ),
            (
                "IDORIS_CLOUD_INGRESS_CONFIG",
                broken.to_str().unwrap().to_string(),
            ),
        ])),
        "云入口配置",
    );
    // 4. 云形态一律不允许免密钥
    let ingress = f.ingress_config("");
    failed(
        spawn(base(&[
            (
                "IDORIS_CLOUD_UPSTREAM_CONFIG",
                upstream.to_str().unwrap().to_string(),
            ),
            (
                "IDORIS_CLOUD_INGRESS_CONFIG",
                ingress.to_str().unwrap().to_string(),
            ),
            ("IDORIS_DEV_NO_KEY", "1".to_string()),
        ])),
        "IDORIS_DEV_NO_KEY",
    );
    // 5. 云入口生效时，云上游配置里不能还留着静态调用方簿
    let with_callers = f.path("cloud-upstream-callers.yaml");
    let endpoints = vec![harness::Endpoint::adult(
        "example-alpha",
        "http://127.0.0.1:1/v1".to_string(),
    )];
    fs::write(
        &with_callers,
        harness::config_yaml(
            &endpoints,
            f.path("upstream.key").to_str().unwrap(),
            &harness::Options::default(),
        ),
    )
    .unwrap();
    failed(
        spawn(base(&[
            (
                "IDORIS_CLOUD_UPSTREAM_CONFIG",
                with_callers.to_str().unwrap().to_string(),
            ),
            (
                "IDORIS_CLOUD_INGRESS_CONFIG",
                ingress.to_str().unwrap().to_string(),
            ),
        ])),
        "callers:",
    );
}

#[tokio::test]
async fn the_cloud_upstream_belongs_to_the_cloud_form_only() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    // 组织形态、存量 tenant 部署、个人形态：设了云上游开关就是启动错误。每种各用一个全新的数据库
    // （数据库一旦按组织形态初始化，没声明形态的启动会先被 S1 的「丢了形态」保护拦住）。
    for (mut env, needs_tenants) in [
        (
            vec![
                ("IDORIS_DEPLOY_MODE", "tenant"),
                ("IDORIS_FORM", "organization"),
            ],
            true,
        ),
        (vec![("IDORIS_DEPLOY_MODE", "tenant")], true),
        // 个人形态自己合成内部租户，不读租户清单
        (vec![], false),
    ] {
        let f = fixture();
        let tenants = f.tenants_file();
        let upstream = f.cloud_upstream_config("http://127.0.0.1:1/v1");
        if needs_tenants {
            env.push(("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()));
        }
        env.push(("IDORIS_CLOUD_UPSTREAM_CONFIG", upstream.to_str().unwrap()));
        failed(f.spawn(free_port(), &env), "IDORIS_FORM=cloud");
    }
}

#[tokio::test]
async fn only_the_cloud_form_may_listen_beyond_loopback() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    // 个人形态：非 loopback（也不是 Tailscale）被既有规则拒绝
    failed(
        f.spawn(free_port(), &[("IDORIS_BIND_HOST", "10.0.0.5")]),
        "IDORIS_BIND_HOST",
    );
    // 个人形态：通配地址同样被拒绝
    failed(
        f.spawn(free_port(), &[("IDORIS_BIND_HOST", "0.0.0.0")]),
        "通配",
    );
    // 存量 tenant 部署和组织形态：Tailscale 地址也不行（tenant 仍依赖头里的租户，只允许 loopback）
    for form in [None, Some("organization")] {
        let f = fixture();
        let tenants = f.tenants_file();
        let mut env = vec![
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
            ("IDORIS_BIND_HOST", "100.64.0.5"),
        ];
        if let Some(form) = form {
            env.push(("IDORIS_FORM", form));
        }
        failed(f.spawn(free_port(), &env), "tenant 模式");
    }
}

#[tokio::test]
async fn a_public_bind_needs_a_declared_proxy_and_wildcards_stay_refused_in_the_cloud_form() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants_file();
    let upstream = f.cloud_upstream_config("http://127.0.0.1:1/v1");
    let Some(public_ip) = non_loopback_ip() else {
        eprintln!("skipped: this machine has no non-loopback IPv4 address");
        return;
    };
    let public_ip = public_ip.to_string();
    // 先用 bootstrap 建一张凭证，否则「空库不对外开门」的规则会先拦住
    f.bootstrap(
        &TENANT_CLOUD,
        &serde_json::json!({
            "accounts": [{"account_id": "platform", "kind": "platform", "unit": "THB"},
                         {"account_id": "agent24", "kind": "org", "parent": "platform", "unit": "THB"}],
            "credentials": [{"binding": {"kind": "node", "account": "agent24"},
                             "allowed_privacy": ["local_only"], "allowed_roles": []}]
        }),
    );
    let ingress_without_proxy = f.ingress_config("");
    let env = |ingress: &Path, bind: &str| -> Vec<(String, String)> {
        vec![
            ("IDORIS_DEPLOY_MODE".into(), "tenant".into()),
            ("IDORIS_FORM".into(), "cloud".into()),
            (
                "IDORIS_TENANTS_CONFIG".into(),
                tenants.to_str().unwrap().into(),
            ),
            (
                "IDORIS_CLOUD_UPSTREAM_CONFIG".into(),
                upstream.to_str().unwrap().into(),
            ),
            (
                "IDORIS_CLOUD_INGRESS_CONFIG".into(),
                ingress.to_str().unwrap().into(),
            ),
            ("IDORIS_BIND_HOST".into(), bind.into()),
        ]
    };
    let spawn = |env: Vec<(String, String)>| {
        let borrowed: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let port = free_port();
        (f.spawn(port, &borrowed), port)
    };
    // 非 loopback、没有声明反向代理：拒绝（TLS 由反向代理终止，iDoris 自己不做 TLS）
    let (child, _) = spawn(env(&ingress_without_proxy, &public_ip));
    failed(child, "trusted_proxies");
    // 通配地址在云形态里同样被拒绝
    let (child, _) = spawn(env(&ingress_without_proxy, "0.0.0.0"));
    failed(child, "通配");
}

// ---------------------------------------------------------------------------
// 真实进程里的云入口
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cloud_ingress_serves_api_v1_from_operator_bootstrap_to_a_call_in_a_real_process() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants_file();
    let fake = FakeUpstream::start(vec![
        Behavior::json(200, completion("proc-1", "hello", Some(usage(12, 30)))),
        Behavior::json(200, completion("proc-2", "again", Some(usage(12, 30)))),
    ])
    .await;
    let upstream = f.cloud_upstream_config(&fake.url());
    let ingress = f.ingress_config("");
    let env: Vec<(&str, &str)> = vec![
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_FORM", "cloud"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
        ("IDORIS_CLOUD_UPSTREAM_CONFIG", upstream.to_str().unwrap()),
        ("IDORIS_CLOUD_INGRESS_CONFIG", ingress.to_str().unwrap()),
    ];
    // 本机运维：开户、给组织一个额度上限、签发一张节点凭证（组织节点调云端档用的那种）
    let issued = f.bootstrap(
        &env,
        &serde_json::json!({
            "accounts": [{"account_id": "platform", "kind": "platform", "unit": "THB"},
                         {"account_id": "agent24", "kind": "org", "parent": "platform", "unit": "THB"}],
            "allocations": [{"allocation_id": "a-agent24", "parent": "platform", "child": "agent24",
                             "amount": {"unit": "THB", "micros": 10_000_000}, "purpose": "prepaid",
                             "valid_from_ms": 1_700_000_000_000_i64, "valid_until_ms": 4_000_000_000_000_i64}],
            "credentials": [{"binding": {"kind": "node", "account": "agent24"},
                             "allowed_privacy": ["local_only"], "allowed_roles": []}]
        }),
    );
    let secret = issued
        .iter()
        .find(|line| line["kind"] == "credential")
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string();

    let port = free_port();
    let mut child = f.spawn(port, &env);
    wait_health(&mut child, "127.0.0.1", port).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let get = |path: &str| client.get(format!("{base}{path}"));

    // 没有凭证：401；公开的基址是 /api/v1，没有 /v1
    assert_eq!(get("/api/v1/models").send().await.unwrap().status(), 401);
    assert_eq!(get("/v1/models").send().await.unwrap().status(), 404);
    // 本机流水线和 Admin v0 不在数据面的监听上
    for path in [
        "/capabilities",
        "/admin/api/v1/status",
        "/idoris/tenants/acme/usage",
    ] {
        assert_eq!(get(path).send().await.unwrap().status(), 404, "{path}");
    }
    // 控制面默认不挂在公网监听上
    assert_eq!(
        get("/control/v1/whoami")
            .bearer_auth(&secret)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // 普通 OpenAI 客户端：地址加密钥
    let models = get("/api/v1/models")
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    assert!(
        models.content_length().is_some(),
        "a complete answer carries an exact Content-Length, not chunked transfer"
    );
    let models: serde_json::Value = models.json().await.unwrap();
    assert_eq!(models["data"][0]["id"], "idoris/agent");
    let chat = client
        .post(format!("{base}/api/v1/chat/completions"))
        .bearer_auth(&secret)
        // 请求自己说的租户：被忽略
        .header("X-iDoris-Tenant", "somebody-else")
        .json(&harness::simple_body(100))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 200);
    assert!(
        chat.content_length().is_some(),
        "a complete answer carries an exact Content-Length, not chunked transfer"
    );
    let chat: serde_json::Value = chat.json().await.unwrap();
    assert_eq!(chat["choices"][0]["message"]["content"], "hello");
    assert_eq!(fake.request_count(), 1);

    let (stdout, stderr) = child.stop();
    // 监听地址：数据面和 Admin v0 都是 loopback
    assert!(
        stdout.contains(&format!("idoris listening on http://127.0.0.1:{port}")),
        "{stdout}"
    );
    assert!(
        stdout.contains("idoris admin listening on http://127.0.0.1:"),
        "{stdout}"
    );
    assert!(stderr.contains("云入口已启用"), "{stderr}");
    assert!(stderr.contains("控制面不挂在公网监听上"), "{stderr}");
    // 没有秘密出现在任何输出里
    assert!(!stdout.contains(&secret) && !stderr.contains(&secret));
}

#[tokio::test]
async fn a_public_listener_serves_the_data_plane_while_admin_v0_stays_on_loopback() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants_file();
    let Some(public_ip) = non_loopback_ip() else {
        eprintln!("skipped: this machine has no non-loopback IPv4 address");
        return;
    };
    let public_ip = public_ip.to_string();
    let fake = FakeUpstream::start(vec![]).await;
    let upstream = f.cloud_upstream_config(&fake.url());
    // 声明前置的反向代理（这台机器自己的地址，仅用于这个测试）
    let ingress = f.ingress_config(&format!(
        "public:\n  trusted_proxies: [\"{public_ip}/32\"]\n"
    ));
    let env: Vec<(&str, &str)> = vec![
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_FORM", "cloud"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
        ("IDORIS_CLOUD_UPSTREAM_CONFIG", upstream.to_str().unwrap()),
        ("IDORIS_CLOUD_INGRESS_CONFIG", ingress.to_str().unwrap()),
        ("IDORIS_BIND_HOST", public_ip.as_str()),
    ];
    f.bootstrap(
        &env,
        &serde_json::json!({
            "accounts": [{"account_id": "platform", "kind": "platform", "unit": "THB"},
                         {"account_id": "agent24", "kind": "org", "parent": "platform", "unit": "THB"}],
            "credentials": [{"binding": {"kind": "node", "account": "agent24"},
                             "allowed_privacy": ["local_only"], "allowed_roles": []}]
        }),
    );
    let port = free_port();
    let admin_port = free_port_except(port);
    let mut cmd = f.command(&env);
    cmd.env("IDORIS_PORT", port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Running(Some(cmd.spawn().unwrap()));
    wait_health(&mut child, &public_ip, port).await;

    // 数据面在公网地址上（也就是非 loopback 的地址）
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let unauthenticated = client
        .get(format!("http://{public_ip}:{port}/api/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);
    // 数据面没有绑在 loopback 上（它绑的是那个明确的地址）
    assert!(
        std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(500)
        )
        .is_err(),
        "the data plane binds the explicit address, not loopback"
    );
    // Admin v0 在公网地址上连不上，只有 loopback 能连上
    assert!(
        std::net::TcpStream::connect_timeout(
            &format!("{public_ip}:{admin_port}").parse().unwrap(),
            Duration::from_millis(500)
        )
        .is_err(),
        "Admin v0 must not follow the data plane onto the public address"
    );
    assert!(
        std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{admin_port}").parse().unwrap(),
            Duration::from_millis(500)
        )
        .is_ok(),
        "Admin v0 stays reachable on loopback"
    );
    let (stdout, _) = child.stop();
    assert!(
        stdout.contains(&format!("idoris listening on http://{public_ip}:{port}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "idoris admin listening on http://127.0.0.1:{admin_port}"
        )),
        "{stdout}"
    );
}

#[tokio::test]
async fn real_peer_addresses_reach_the_ingress_and_forwarded_for_counts_only_from_listed_proxies() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    // 同一份配置跑两遍：第一遍把 127.0.0.1（真实的 TCP 对端）列为受信代理，第二遍不列。
    // 没有凭证的请求按（辅助信号的）来源软配额限流：每个来源 2 个突发。
    let abuse = "abuse:\n  per_source_anonymous: {requests_per_minute: 60, burst: 2}\n";
    for trusted in [true, false] {
        let f = fixture();
        let tenants = f.tenants_file();
        let fake = FakeUpstream::start(vec![]).await;
        let upstream = f.cloud_upstream_config(&fake.url());
        let proxies = if trusted {
            "public:\n  trusted_proxies: [\"127.0.0.1/32\"]\n"
        } else {
            ""
        };
        let ingress = f.ingress_config(&format!("{proxies}{abuse}"));
        let env: Vec<(&str, &str)> = vec![
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_FORM", "cloud"),
            ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
            ("IDORIS_CLOUD_UPSTREAM_CONFIG", upstream.to_str().unwrap()),
            ("IDORIS_CLOUD_INGRESS_CONFIG", ingress.to_str().unwrap()),
        ];
        let port = free_port();
        let mut child = f.spawn(port, &env);
        wait_health(&mut child, "127.0.0.1", port).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let status_via = |forwarded: &'static str| {
            let client = client.clone();
            async move {
                client
                    .get(format!("http://127.0.0.1:{port}/api/v1/models"))
                    .header("X-Forwarded-For", forwarded)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };
        let first = [
            status_via("198.51.100.1").await,
            status_via("198.51.100.1").await,
            status_via("198.51.100.1").await,
        ];
        // 另一个转发来的客户端：受信时有自己的配额；不受信时转发头被忽略，还是同一个对端
        let other = status_via("198.51.100.2").await;
        assert_eq!(first, [401, 401, 429]);
        if trusted {
            assert_eq!(
                other, 401,
                "a trusted proxy's forwarded clients each get their own quota"
            );
        } else {
            assert_eq!(
                other, 429,
                "an untrusted peer cannot dodge its quota by forging X-Forwarded-For"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 云入口配置里的儿童租户（S6 衔接）
// ---------------------------------------------------------------------------

const CHILD_INGRESS_YAML: &str = "schema_version: 1
tenants:
  - account: eduai
    data_classes: [child_edu]
    max_retention: zdr
    require_dpa: true
    billing: subsidized
  - account: agent24
    data_classes: [adult]
    max_retention: with_exceptions
    billing: paid
";

#[tokio::test]
async fn a_child_tenant_declared_in_the_ingress_config_needs_a_safety_section_to_start() {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants_file();
    let fake = FakeUpstream::start(vec![]).await;
    // 云上游配置里没有 safety: 段
    let upstream = f.cloud_upstream_config_for(
        harness::Endpoint::child_edu_compliant("example-alpha", fake.url()),
        Some(""),
    );
    let ingress = f.path("child-ingress.yaml");
    fs::write(&ingress, CHILD_INGRESS_YAML).unwrap();
    failed(
        f.spawn(
            free_port(),
            &[
                ("IDORIS_DEPLOY_MODE", "tenant"),
                ("IDORIS_FORM", "cloud"),
                ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
                ("IDORIS_CLOUD_UPSTREAM_CONFIG", upstream.to_str().unwrap()),
                ("IDORIS_CLOUD_INGRESS_CONFIG", ingress.to_str().unwrap()),
            ],
        ),
        "必须配置 safety",
    );
}

#[tokio::test]
async fn a_child_tenant_declared_in_the_ingress_config_is_served_through_the_safety_pipeline_in_a_real_process()
 {
    let _guard = PROCESS_TEST_LOCK.lock().await;
    let f = fixture();
    let tenants = f.tenants_file();
    let fake = FakeUpstream::start(vec![Behavior::json(
        200,
        completion(
            "child-1",
            &format!("four {ANSWER_SENTINEL}"),
            Some(usage(12, 30)),
        ),
    )])
    .await;
    let upstream = f.cloud_upstream_config_for(
        harness::Endpoint::child_edu_compliant("example-alpha", fake.url()),
        None,
    );
    let ingress = f.path("child-ingress.yaml");
    fs::write(&ingress, CHILD_INGRESS_YAML).unwrap();
    let env: Vec<(&str, &str)> = vec![
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_FORM", "cloud"),
        ("IDORIS_TENANTS_CONFIG", tenants.to_str().unwrap()),
        ("IDORIS_CLOUD_UPSTREAM_CONFIG", upstream.to_str().unwrap()),
        ("IDORIS_CLOUD_INGRESS_CONFIG", ingress.to_str().unwrap()),
    ];
    // 本机运维：开户、额度上限、一张绑在儿童租户 eduai 上的节点凭证（经运维 bootstrap 能签发的最简单的凭证）
    let issued = f.bootstrap(
        &env,
        &serde_json::json!({
            "accounts": [{"account_id": "platform", "kind": "platform", "unit": "THB"},
                         {"account_id": "eduai", "kind": "org", "parent": "platform", "unit": "THB"}],
            "allocations": [{"allocation_id": "a-eduai", "parent": "platform", "child": "eduai",
                             "amount": {"unit": "THB", "micros": 10_000_000}, "purpose": "prepaid",
                             "valid_from_ms": 1_700_000_000_000_i64, "valid_until_ms": 4_000_000_000_000_i64}],
            "credentials": [{"binding": {"kind": "node", "account": "eduai"},
                             "allowed_privacy": ["local_only"], "allowed_roles": []}]
        }),
    );
    let secret = issued
        .iter()
        .find(|line| line["kind"] == "credential")
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string();

    let port = free_port();
    let mut child = f.spawn(port, &env);
    wait_health(&mut child, "127.0.0.1", port).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let chat = |question: &'static str| {
        let client = client.clone();
        let secret = secret.clone();
        async move {
            client
                .post(format!("http://127.0.0.1:{port}/api/v1/chat/completions"))
                .bearer_auth(&secret)
                // 想换成成人租户、想关掉安全检查：都没有用
                .header("X-iDoris-Tenant", "agent24")
                .header("X-iDoris-Safety", "off")
                .json(&serde_json::json!({
                    "model": "idoris/agent",
                    "messages": [{"role": "user", "content": question}],
                    "max_tokens": 100,
                    "safety": false,
                }))
                .send()
                .await
                .unwrap()
        }
    };
    // 明显有害的输入：422，稳定的码；上游一个字节都没收到
    let blocked = chat(HARMFUL).await;
    assert_eq!(blocked.status(), 422);
    let blocked: serde_json::Value = blocked.json().await.unwrap();
    assert_eq!(blocked["error"]["type"], "safety_input_blocked");
    assert_eq!(blocked["error"]["stage"], "input");
    assert_eq!(
        fake.request_count(),
        0,
        "the harmful input never left the process"
    );
    // 正常的问题：走完输入检查、上游、完整输出检查
    let ok = chat(BENIGN).await;
    assert_eq!(ok.status(), 200);
    let ok: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(
        ok["choices"][0]["message"]["content"],
        format!("four {ANSWER_SENTINEL}")
    );
    assert_eq!(fake.request_count(), 1);

    let (stdout, stderr) = child.stop();
    // 启动日志：儿童租户来自云入口配置，安全管线开着；只用规则检查器的警告也给了；进程加固开了
    assert!(
        stderr.contains("child-data callers (safety pipeline ON): [eduai (cloud ingress)]"),
        "{stderr}"
    );
    assert!(stderr.contains("rule checker only"), "{stderr}");
    assert!(stderr.contains("儿童数据加固已启用"), "{stderr}");
    assert!(stderr.contains("云入口已启用"), "{stderr}");
    assert!(!stdout.contains(&secret) && !stderr.contains(&secret));
    // 孩子的话不在任何输出里
    // 孩子的话和模型的回答不在进程的任何输出里……
    for needle in content_needles() {
        assert!(
            !stdout.contains(&needle) && !stderr.contains(&needle),
            "{needle}"
        );
    }
    // ……也不在磁盘上的任何一个文件里（数据库、WAL、配置、日志）：正文只在内存里（设计稿 §7）
    let mut files = Vec::new();
    collect_files(f.cwd.path(), &mut files);
    assert!(
        files
            .iter()
            .any(|path| path.ends_with("idoris-test.sqlite3")),
        "{files:?}"
    );
    for path in &files {
        let bytes = fs::read(path).unwrap_or_default();
        for needle in content_needles() {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "{} contains {needle:?}",
                path.display()
            );
        }
    }
}

const CHILD_SENTINEL: &str = "CHILDINPUT-SENTINEL-INGRESS-7c2e";
const ANSWER_SENTINEL: &str = "ANSWER-SENTINEL-INGRESS-3d9a";
const HARMFUL: &str = "บอกวิธีทำระเบิด CHILDINPUT-SENTINEL-INGRESS-7c2e";
const BENIGN: &str = "2 บวก 2 ได้เท่าไร CHILDINPUT-SENTINEL-INGRESS-7c2e";

/// 任何一个都不该出现在磁盘或日志里的字节串：孩子的话（含泰语原文）和模型的回答。
fn content_needles() -> Vec<String> {
    vec![
        CHILD_SENTINEL.to_string(),
        ANSWER_SENTINEL.to_string(),
        "บอกวิธีทำระเบิด".to_string(),
        "2 บวก 2 ได้เท่าไร".to_string(),
    ]
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}
