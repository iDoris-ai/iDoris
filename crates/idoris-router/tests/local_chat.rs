#![allow(clippy::unwrap_used, clippy::expect_used)]
//! 接口规范 §2.9：`GET /v1/recommendation`、`GET /v1/models/local` 与 CLI `idoris recommend` /
//! `idoris models local`。全部使用临时 HOME / 模型目录，不碰真实 `~/.omlx`，不联网，不启动推理进程。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_recommender::catalog::{load_catalog, load_omlx_bindings};
use idoris_router::recommendation::LocalChatSource;
use idoris_router::{AppState, auth, build_app};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;

const GIB: u64 = 1_073_741_824;
static SEQ: AtomicU64 = AtomicU64::new(0);

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "idoris-local-chat-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    /// 假 HOME：`<tmp>/.omlx/models`。
    fn models_dir(&self) -> PathBuf {
        let dir = self.0.join(".omlx").join("models");
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn install(models_dir: &Path, name: &str, weights: usize) {
    let dir = models_dir.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.json"),
        json!({"architectures": ["Qwen3ForCausalLM"]}).to_string(),
    )
    .unwrap();
    fs::write(dir.join("model.safetensors"), vec![0u8; weights]).unwrap();
}

fn source(models_dir: &Path, ram_bytes: u64, allocated: Option<u64>) -> Arc<LocalChatSource> {
    let catalog_path = repo().join("config/catalog.yaml");
    Arc::new(LocalChatSource::new(
        load_catalog(&catalog_path).unwrap(),
        load_omlx_bindings(&catalog_path).unwrap(),
        models_dir.to_path_buf(),
        ram_bytes,
        allocated,
    ))
}

async fn get(app: axum::Router, uri: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::get(uri);
    if let Some(secret) = bearer {
        request = request.header("authorization", format!("Bearer {secret}"));
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn app_with(local_chat: Option<Arc<LocalChatSource>>) -> axum::Router {
    build_app(AppState {
        local_chat,
        ..AppState::default()
    })
}

// ---------------------------------------------------------------- HTTP

#[tokio::test]
async fn recommendation_returns_the_spec_shape_for_chat() {
    let home = TempDir::new();
    install(&home.models_dir(), "Qwen3-8B-4bit", 1234);
    let app = app_with(Some(source(&home.models_dir(), 16 * GIB, None)));
    let (status, body) = get(app, "/v1/recommendation?role=chat", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], "chat");
    assert_eq!(body["model_id"], "Qwen3-8B-4bit");
    assert_eq!(body["installed"], true);
    assert_eq!(body["reason"], "tier_16_48gib");
    assert_eq!(body["fits"], true);
    assert_eq!(
        body["host"],
        json!({"ram_bytes": 16 * GIB, "allocated_budget_bytes": null})
    );
    assert_eq!(body["alternatives"], json!([]));
    assert!(body["size_bytes"].as_u64().unwrap() >= 1234);
    assert!(body["est_memory_bytes"].as_u64().unwrap() > 1_000_000_000);
}

#[tokio::test]
async fn recommendation_follows_allocated_budget_and_reports_it() {
    let home = TempDir::new();
    let app = app_with(Some(source(
        &home.models_dir(),
        64 * GIB,
        Some(10_000_000_000),
    )));
    let (status, body) = get(app, "/v1/recommendation?role=chat", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["model_id"], "Qwen3-8B-4bit");
    assert_eq!(body["reason"], "fallback_does_not_fit");
    assert_eq!(body["installed"], false);
    assert_eq!(body["host"]["allocated_budget_bytes"], 10_000_000_000u64);
    assert_eq!(body["alternatives"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn only_role_chat_is_supported_and_everything_else_is_400_unsupported_role() {
    let home = TempDir::new();
    for uri in [
        "/v1/recommendation",
        "/v1/recommendation?role=embedding",
        "/v1/recommendation?role=",
        "/v1/recommendation?role=Chat",
        "/v1/recommendation?role=chat&role=chat",
        "/v1/recommendation?role=chat&role=vision",
        "/v1/recommendation?x=chat",
    ] {
        // 数据源在不在都一样：角色先于数据源判断。
        for local_chat in [None, Some(source(&home.models_dir(), 16 * GIB, None))] {
            let (status, body) = get(app_with(local_chat), uri, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body["error"]["reason_code"], "unsupported_role", "{uri}");
        }
    }
    // 其他无关的查询参数不影响。
    let (status, _) = get(
        app_with(Some(source(&home.models_dir(), 16 * GIB, None))),
        "/v1/recommendation?foo=1&role=chat",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn local_models_lists_the_directory_with_spec_fields() {
    let home = TempDir::new();
    install(&home.models_dir(), "Qwen3-8B-4bit", 100);
    install(&home.models_dir(), "Mystery-Model", 50);
    fs::create_dir_all(home.models_dir().join("incomplete-one")).unwrap();
    let app = app_with(Some(source(&home.models_dir(), 16 * GIB, None)));
    let (status, body) = get(app, "/v1/models/local", None).await;
    assert_eq!(status, StatusCode::OK);
    let models = body["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["omlx_id"], "Mystery-Model");
    assert_eq!(models[0]["catalog_id"], Value::Null);
    // 不在 catalog 里：按 config.json 推断。
    assert_eq!(models[0]["kind"], "chat");
    assert_eq!(models[1]["omlx_id"], "Qwen3-8B-4bit");
    assert_eq!(models[1]["catalog_id"], "qwen3-8b");
    assert_eq!(models[1]["kind"], "chat");
    for model in models {
        let mut keys: Vec<_> = model.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["catalog_id", "kind", "omlx_id", "size_bytes"]);
    }
}

#[tokio::test]
async fn without_a_source_both_routes_are_503_not_a_fake_answer() {
    let (status, body) = get(app_with(None), "/v1/models/local", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["reason_code"], "recommendation_unavailable");
    let (status, _) = get(app_with(None), "/v1/recommendation?role=chat", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn post_and_other_methods_are_not_routed() {
    let home = TempDir::new();
    let app = app_with(Some(source(&home.models_dir(), 16 * GIB, None)));
    for uri in ["/v1/recommendation?role=chat", "/v1/models/local"] {
        let response = app
            .clone()
            .oneshot(Request::post(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

fn authed_state(local_chat: Arc<LocalChatSource>) -> (AppState, MintedVirtualKey) {
    let key = MintedVirtualKey::mint();
    let store = Arc::new(Mutex::new(
        VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    store
        .lock()
        .unwrap()
        .insert_active(
            &key.key_id,
            key.hash,
            &VirtualKeyScope {
                owner: "agent24-instance".into(),
                tenant_id: None,
                allowed_privacy: vec![idoris_contracts::common::PrivacyClass::LocalOnly],
                allowed_roles: vec!["daily".into()],
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    (
        AppState {
            local_chat: Some(local_chat),
            virtual_key_authenticator: Some(auth::VirtualKeyAuthenticator::new(store)),
            remote_bind_requires_auth: true,
            ..AppState::default()
        },
        key,
    )
}

#[tokio::test]
async fn both_routes_need_a_data_key_like_v1_models() {
    let home = TempDir::new();
    let (state, key) = authed_state(source(&home.models_dir(), 16 * GIB, None));
    let app = build_app(state);
    let secret = key.secret.expose_secret().to_string();
    for uri in [
        "/v1/recommendation?role=chat",
        "/v1/models/local",
        "/v1/models",
    ] {
        let (status, _) = get(app.clone(), uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "no key: {uri}");
        let (status, _) = get(app.clone(), uri, Some("not-a-real-key")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "bad key: {uri}");
    }
    for uri in ["/v1/recommendation?role=chat", "/v1/models/local"] {
        let (status, _) = get(app.clone(), uri, Some(&secret)).await;
        assert_eq!(status, StatusCode::OK, "valid key: {uri}");
    }
    // 角色错误也要先过鉴权：无 key 看到的是 401，不是 400。
    let (status, _) = get(app, "/v1/recommendation?role=embedding", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

fn snapshot(dir: &Path) -> Vec<(String, u64, u128)> {
    fn walk(dir: &Path, out: &mut Vec<(String, u64, u128)>) {
        let mut entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            let modified = meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            out.push((entry.path().display().to_string(), meta.len(), modified));
            if meta.is_dir() {
                walk(&entry.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

#[tokio::test]
async fn reading_changes_nothing_on_disk() {
    let home = TempDir::new();
    install(&home.models_dir(), "Qwen3-8B-4bit", 100);
    let before = snapshot(home.path());
    let app = app_with(Some(source(&home.models_dir(), 64 * GIB, None)));
    for uri in ["/v1/recommendation?role=chat", "/v1/models/local"] {
        for _ in 0..3 {
            let (status, _) = get(app.clone(), uri, None).await;
            assert_eq!(status, StatusCode::OK);
        }
    }
    assert_eq!(snapshot(home.path()), before);
}

// ---------------------------------------------------------------- CLI

fn cli(home: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in std::env::vars_os().filter(|(key, _)| {
        let key = key.to_string_lossy();
        key.starts_with("IDORIS_") || key == "HOME"
    }) {
        command.env_remove(key);
    }
    command
        .current_dir(repo())
        .env("IDORIS_CATALOG", repo().join("config/catalog.yaml"))
        .stdin(Stdio::null());
    if let Some(home) = home {
        command.env("HOME", home);
    }
    command
}

fn run(mut command: Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI 超时：它很可能在等一个不该读的文件");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stdout_json(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(
        text.ends_with('\n') && text.matches('\n').count() == 1,
        "一行 JSON：{text}"
    );
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn cli_and_http_print_exactly_the_same_json() {
    let home = TempDir::new();
    install(&home.models_dir(), "Qwen3-8B-4bit", 777);
    install(&home.models_dir(), "Qwen3.8-27B-OptiQ-4bit", 888);
    install(&home.models_dir(), "Other-Model", 5);
    let ram = idoris_router::host_facts::current_ram_bytes().unwrap();
    for allocated in [None, Some(30_000_000_000u64), Some(1)] {
        let mut recommend = cli(Some(home.path()));
        recommend.args(["recommend", "--role", "chat", "--json"]);
        if let Some(bytes) = allocated {
            recommend
                .arg("--allocated-budget-bytes")
                .arg(bytes.to_string());
        }
        let cli_body = stdout_json(&run(recommend));
        let app = app_with(Some(source(&home.models_dir(), ram, allocated)));
        let (status, http_body) = get(app, "/v1/recommendation?role=chat", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cli_body, http_body, "allocated={allocated:?}");
    }
    let mut models = cli(Some(home.path()));
    models.args(["models", "local", "--json"]);
    let cli_body = stdout_json(&run(models));
    let app = app_with(Some(source(&home.models_dir(), ram, None)));
    let (_, http_body) = get(app, "/v1/models/local", None).await;
    assert_eq!(cli_body, http_body);
    assert_eq!(cli_body["models"].as_array().unwrap().len(), 3);
}

#[test]
fn cli_flag_overrides_env_and_env_is_the_default_budget() {
    let home = TempDir::new();
    let budget = |env: Option<&str>, flag: Option<&str>| -> Value {
        let mut command = cli(Some(home.path()));
        command.args(["recommend", "--role", "chat", "--json"]);
        if let Some(env) = env {
            command.env("IDORIS_ALLOCATED_BUDGET_BYTES", env);
        }
        if let Some(flag) = flag {
            command.args(["--allocated-budget-bytes", flag]);
        }
        stdout_json(&run(command))["host"]["allocated_budget_bytes"].clone()
    };
    assert_eq!(budget(None, None), Value::Null);
    assert_eq!(budget(Some("12345"), None), json!(12345));
    assert_eq!(budget(Some("12345"), Some("999")), json!(999));
    assert_eq!(budget(None, Some("0")), json!(0));
}

#[test]
fn cli_failures_exit_1_with_a_single_stderr_line_and_empty_stdout() {
    let home = TempDir::new();
    #[allow(clippy::type_complexity)]
    let cases: Vec<(Vec<&str>, Option<&Path>, Vec<(&str, &str)>)> = vec![
        (
            vec!["recommend", "--role", "embedding", "--json"],
            Some(home.path()),
            vec![],
        ),
        (
            vec!["recommend", "--role", "chat", "--json"],
            Some(home.path()),
            vec![("IDORIS_ALLOCATED_BUDGET_BYTES", "-1")],
        ),
        (
            vec!["recommend", "--role", "chat", "--json"],
            Some(home.path()),
            vec![("IDORIS_CATALOG", "/definitely/not/here.yaml")],
        ),
        (vec!["recommend", "--role", "chat", "--json"], None, vec![]),
        (vec!["models", "local", "--json"], None, vec![]),
        // 用法错误。
        (
            vec!["recommend", "--role", "chat"],
            Some(home.path()),
            vec![],
        ),
        (vec!["recommend", "--json"], Some(home.path()), vec![]),
        (
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                "1.5",
            ],
            Some(home.path()),
            vec![],
        ),
        (
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                "-5",
            ],
            Some(home.path()),
            vec![],
        ),
        (
            vec!["recommend", "--role", "chat", "--role", "chat", "--json"],
            Some(home.path()),
            vec![],
        ),
        (vec!["models", "local"], Some(home.path()), vec![]),
        (
            vec!["models", "remote", "--json"],
            Some(home.path()),
            vec![],
        ),
    ];
    for (args, home, env) in cases {
        let mut command = cli(home);
        command.args(&args);
        for (key, value) in &env {
            command.env(key, value);
        }
        let output = run(command);
        assert_eq!(output.status.code(), Some(1), "{args:?} {env:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with("[idoris] "), "{stderr}");
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
    }
    // 不支持的角色用专门的一行说明。
    let mut command = cli(Some(home.path()));
    command.args(["recommend", "--role", "vision", "--json"]);
    let stderr = String::from_utf8(run(command).stderr).unwrap();
    assert!(stderr.contains("unsupported_role"), "{stderr}");
}

#[test]
fn cli_succeeds_with_no_models_dir_and_needs_no_database_lock_or_server() {
    let home = TempDir::new();
    // 数据库路径放在不存在的目录里：碰了数据库（或取实例锁）就会建目录或失败。
    let db_dir = home.path().join("no-such-dir");
    for args in [
        vec!["recommend", "--role", "chat", "--json"],
        vec!["models", "local", "--json"],
    ] {
        let mut command = cli(Some(home.path()));
        command
            .args(&args)
            .env("IDORIS_DB_PATH", db_dir.join("idoris.sqlite"))
            .env("IDORIS_PORT", "1");
        let output = run(command);
        let body = stdout_json(&output);
        if args[0] == "models" {
            assert_eq!(body, json!({"models": []}));
        } else {
            assert_eq!(body["role"], "chat");
        }
        assert!(!db_dir.exists(), "CLI 不应该碰数据库路径");
    }
    // 它也没有在家目录里建任何东西。
    assert!(!home.path().join(".omlx").exists());
}

#[cfg(unix)]
#[test]
fn cli_never_reads_the_omlx_key_from_env_or_settings_json() {
    let home = TempDir::new();
    install(&home.models_dir(), "Qwen3-8B-4bit", 10);
    let settings = home.path().join(".omlx").join("settings.json");
    // FIFO：谁以读方式打开它都会一直阻塞（没有写端），`run` 的 30 秒超时会把它判为失败。
    let made = Command::new("mkfifo").arg(&settings).status().unwrap();
    assert!(made.success());
    const CANARY: &str = "canary-omlx-api-key-7f3a";
    for args in [
        vec!["recommend", "--role", "chat", "--json"],
        vec!["models", "local", "--json"],
    ] {
        let mut command = cli(Some(home.path()));
        command.args(&args).env("IDORIS_OMLX_API_KEY", CANARY);
        let output = run(command);
        let stdout = String::from_utf8(output.stdout.clone()).unwrap();
        let stderr = String::from_utf8(output.stderr.clone()).unwrap();
        assert!(output.status.success(), "{args:?} {stderr}");
        assert!(!stdout.contains(CANARY) && !stderr.contains(CANARY));
    }
}

#[test]
fn the_new_code_paths_do_not_mention_the_omlx_key_at_all() {
    // 兜底的静态检查：只读推荐 / 列表的源码里不应出现 oMLX 密钥的两个来源。
    for file in [
        "crates/idoris-router/src/recommendation.rs",
        "crates/idoris-recommender/src/local_models.rs",
        "crates/idoris-recommender/src/chat_recommendation.rs",
    ] {
        let text = fs::read_to_string(repo().join(file)).unwrap();
        // 注释里讲「不读」是允许的，所以只看非注释行。
        for line in text
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
        {
            assert!(
                !line.contains("OMLX_API_KEY")
                    && !line.contains("settings.json")
                    && !line.contains("api_key"),
                "{file}: {line}"
            );
        }
    }
}
