//! Release binaries find bundled config beside the executable, independent of cwd.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::virtual_key::VirtualKeySecret;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const PORTABLE_TEST_KEY: &str =
    "idk_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Running(Option<Child>);
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn fixture() -> (TempDir, TempDir, std::path::PathBuf, u16) {
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
        let raw = fs::read_to_string(repo.join(file)).unwrap();
        fs::write(bin.join("config").join(file), raw.replace(":8000", ":0")).unwrap();
    }
    let cwd = TempDir::new().unwrap();
    fs::create_dir_all(cwd.path().join("config/components")).unwrap();
    fs::write(cwd.path().join("config/components/bad.yaml"), "not: [valid").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    (root, cwd, bin.join("idoris"), port)
}

fn spawn(exe: &Path, cwd: &Path, port: u16, env: &[(&str, &str)]) -> Running {
    let mut cmd = Command::new(exe);
    for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
    {
        cmd.env_remove(key);
    }
    cmd.current_dir(cwd)
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_DB_PATH", cwd.join("idoris-test.sqlite3"))
        .envs(env.iter().copied())
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
                // Linux may briefly reject exec immediately after fs::copy
                // closes a newly written executable. Retry only ETXTBSY;
                // every other spawn failure remains an immediate test error.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("failed to spawn copied idoris binary: {err}"),
        }
    }
}

fn seed_portable_key(cwd: &Path) {
    let secret = VirtualKeySecret::parse(PORTABLE_TEST_KEY).unwrap();
    let store = VirtualKeyStore::open(cwd.join("idoris-test.sqlite3")).unwrap();
    store
        .insert_active(
            "vk_portable_startup",
            secret.hash(),
            &VirtualKeyScope {
                owner: "portable-startup".into(),
                allowed_privacy: vec![PrivacyClass::LocalOnly, PrivacyClass::Any],
                allowed_roles: [
                    "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
}

fn failed(mut child: Running, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            let output = child.0.take().unwrap().wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(expected), "{stderr}");
            return;
        }
        assert!(Instant::now() < deadline, "startup did not fail");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn non_loopback_without_an_active_key_refuses_before_bind() {
    let (_root, cwd, exe, port) = fixture();
    failed(
        spawn(
            &exe,
            cwd.path(),
            port,
            &[("IDORIS_BIND_HOST", "100.64.0.5")],
        ),
        "非 loopback，但当前没有有效 virtual key；拒绝启动",
    );
}

#[test]
fn non_loopback_never_accepts_dev_no_key_even_when_a_key_exists() {
    let (_root, cwd, exe, port) = fixture();
    seed_portable_key(cwd.path());
    failed(
        spawn(
            &exe,
            cwd.path(),
            port,
            &[
                ("IDORIS_BIND_HOST", "100.64.0.5"),
                ("IDORIS_DEV_NO_KEY", "1"),
            ],
        ),
        "IDORIS_DEV_NO_KEY=1 只允许 loopback",
    );
}

async fn wait_health(child: &mut Running, port: u16) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && response.status().is_success()
        {
            return;
        }
        assert!(child.0.as_mut().unwrap().try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn explicit_relative_and_absolute_config_paths_start() {
    for absolute in [false, true] {
        let (_root, cwd, exe, port) = fixture();
        let bundled = exe.parent().unwrap().join("config");
        let (components, policy) = if absolute {
            (
                bundled.join("components").to_string_lossy().into_owned(),
                bundled
                    .join("routing-policy.yaml")
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            let explicit = cwd.path().join("explicit");
            fs::create_dir_all(explicit.join("components")).unwrap();
            fs::copy(
                bundled.join("components/omlx.yaml"),
                explicit.join("components/omlx.yaml"),
            )
            .unwrap();
            fs::copy(
                bundled.join("routing-policy.yaml"),
                explicit.join("routing-policy.yaml"),
            )
            .unwrap();
            (
                "explicit/components".to_string(),
                "explicit/routing-policy.yaml".to_string(),
            )
        };
        let env = [
            ("IDORIS_COMPONENTS_DIR", components.as_str()),
            ("IDORIS_ROUTING_POLICY", policy.as_str()),
        ];
        let mut child = spawn(&exe, cwd.path(), port, &env);
        wait_health(&mut child, port).await;
    }
}

#[tokio::test]
async fn bundled_config_starts_from_an_unrelated_working_directory() {
    let (_root, cwd, exe, port) = fixture();
    seed_portable_key(cwd.path());
    let mut child = spawn(&exe, cwd.path(), port, &[]);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
        {
            break response;
        }
        assert!(child.0.as_mut().unwrap().try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(response.status().is_success());
    let health: serde_json::Value = response.json().await.unwrap();
    assert_eq!(health["status"], "ok");
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(PORTABLE_TEST_KEY)
        .json(&serde_json::json!({"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "local_only_unavailable");

    // The bundled lifecycle card must retain OpenAI admission checks even
    // when its backend is unavailable and cwd contains unrelated config.
    for (field, value, reason) in [
        ("stream", serde_json::json!(true), "unsupported_stream"),
        (
            "temperature",
            serde_json::json!(0.2),
            "unsupported_parameter",
        ),
        ("max_tokens", serde_json::json!(64), "unsupported_parameter"),
    ] {
        let mut body = serde_json::json!({
            "model": "idoris/daily",
            "messages": [{"role":"user","content":"hi"}]
        });
        body[field] = value;
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .bearer_auth(PORTABLE_TEST_KEY)
            .json(&body)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{field}"
        );
        let error: serde_json::Value = response.json().await.unwrap();
        assert_eq!(error["error"]["type"], "unsupported_field", "{field}");
        assert_eq!(error["error"]["reason_code"], reason, "{field}");
        assert!(
            error["error"]["remediation"]
                .as_str()
                .unwrap()
                .contains(field)
        );
    }

    // Task 15: a concrete model id is no longer required to equal the
    // provider id. It reaches the selected backend, which is unavailable in
    // this portable-startup fixture, and therefore fails closed as a backend
    // error instead of being rejected as an unsupported field.
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(PORTABLE_TEST_KEY)
        .json(&serde_json::json!({
            "model": "another-model",
            "messages": [{"role":"user","content":"hi"}]
        }))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "local_only_unavailable");
}

#[test]
fn missing_or_invalid_bundled_policy_fails_startup() {
    for invalid in [false, true] {
        let (_root, cwd, exe, port) = fixture();
        let policy = exe.parent().unwrap().join("config/routing-policy.yaml");
        if invalid {
            fs::write(&policy, "routing_policy: [broken").unwrap();
        } else {
            fs::remove_file(policy).unwrap();
        }
        failed(spawn(&exe, cwd.path(), port, &[]), "无法加载路由策略");
    }
}

#[test]
fn explicit_missing_paths_do_not_fall_back() {
    let (_root, cwd, exe, port) = fixture();
    let components = [("IDORIS_COMPONENTS_DIR", "missing-components")];
    failed(
        spawn(&exe, cwd.path(), port, &components),
        "无法加载组件目录",
    );
    let policy = [("IDORIS_ROUTING_POLICY", "missing-policy.yaml")];
    failed(spawn(&exe, cwd.path(), port, &policy), "无法加载路由策略");
}

#[test]
fn explicit_storage_failures_do_not_fall_back() {
    let (_root, cwd, exe, port) = fixture();
    let blocker = cwd.path().join("not-a-directory");
    fs::write(&blocker, "file").unwrap();
    let bad_db = blocker.join("db.sqlite3");
    let bad_db = bad_db.to_str().unwrap();
    failed(
        spawn(&exe, cwd.path(), port, &[("IDORIS_DB_PATH", bad_db)]),
        "无法初始化持久化存储",
    );

    let missing = cwd.path().join("missing-tenants.yaml");
    let missing = missing.to_str().unwrap();
    failed(
        spawn(
            &exe,
            cwd.path(),
            port,
            &[
                ("IDORIS_DEPLOY_MODE", "tenant"),
                ("IDORIS_TENANTS_CONFIG", missing),
            ],
        ),
        "无法初始化持久化存储",
    );
}
