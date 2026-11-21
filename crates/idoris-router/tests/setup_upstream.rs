//! `idoris setup upstream`（最小版，接口规范 §2.7.8）：登记 loopback 上的 OpenAI 兼容上游，往组件目录里加一张卡。
//!
//! 全部走真的 `idoris` 二进制（调用方 Agent24 看到的就是它）：上游是 wiremock 假服务，目录是临时目录，家目录指向不存在的
//! 临时路径。**不启动任何推理进程、不连外网、不碰真实的 `~/.omlx` 与 `~/.agent24`**。起子进程的地方都有 `Reaped` 守卫
//! （Drop 时 SIGKILL 加 wait），断言失败或 panic 也不留进程。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/startup_wait.rs"]
mod startup_wait;

use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use idoris_contracts::ComponentCard;
use idoris_router::cli::{UpstreamKind, UpstreamOptions};
use idoris_router::components::load_components;
use idoris_router::setup_engine::{
    FindFailure, FoundExecutable, OutDirUse, Probed, SetupEngine, SetupError, run_setup,
    write_added,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const USAGE_MARK: &str = "idoris setup upstream";
const MODEL: &str = "qwen3-8b";
/// 环境里的 oMLX key 金丝雀：`setup upstream` 不读它，更不会发给上游。
const OMLX_KEY: &str = "omlx-key-canary-upstream-zzz";
/// stdin 上的 key 金丝雀：这个命令没有读 stdin 的通道。
const STDIN_KEY: &str = "stdin-key-canary-upstream-zzz";

// ---------------------------------------------------------------------------------------------
// 子进程守卫与命令行台架
// ---------------------------------------------------------------------------------------------

/// 子进程的守卫：测试断言失败或 panic 时（栈展开会运行 Drop），Drop 里 SIGKILL 并 wait，不留孤儿进程；
/// 测试进程自己被 SIGKILL 时 Drop 不会运行。
struct Reaped(Option<Child>);

impl Reaped {
    fn spawn(cmd: &mut Command) -> Self {
        Self(Some(startup_wait::spawn_when_executable(cmd)))
    }

    fn child(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }

    /// 正常等它跑完并取走输出（取走之后 Drop 不再杀）。
    fn finish(mut self) -> std::process::Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
    json: Value,
}

fn idoris() -> Command {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| startup_wait::warm_exec(Path::new(env!("CARGO_BIN_EXE_idoris"))));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
    {
        cmd.env_remove(key);
    }
    // 不碰真实的家目录。
    cmd.env("HOME", "/nonexistent/idoris-setup-upstream-test-home");
    cmd
}

/// 跑 `idoris setup upstream <args>`；`stdin` 有值就写进去再关掉。
async fn setup_in(cwd: Option<&Path>, args: &[String], stdin: Option<&str>) -> Out {
    let (cwd, args, stdin) = (
        cwd.map(Path::to_owned),
        args.to_vec(),
        stdin.map(str::to_owned),
    );
    tokio::task::spawn_blocking(move || {
        let mut cmd = idoris();
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }
        cmd.args(["setup", "upstream"])
            .args(&args)
            .env("IDORIS_OMLX_API_KEY", OMLX_KEY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Reaped::spawn(&mut cmd);
        let mut pipe = child.child().stdin.take().unwrap();
        if let Some(text) = stdin {
            // 命令不读 stdin 就退出了：写失败（EPIPE）不是测试要查的事。
            let _ = pipe.write_all(text.as_bytes());
        }
        drop(pipe);
        let output = child.finish();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Out {
            code: output.status.code().unwrap(),
            json: serde_json::from_str(&stdout).unwrap_or(Value::Null),
            stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// `--kind openai_compat --endpoint <endpoint> --out-dir <out>` 加 `extra`。
fn full(endpoint: &str, out: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = strings(&[
        "--kind",
        "openai_compat",
        "--endpoint",
        endpoint,
        "--out-dir",
    ]);
    args.push(out.display().to_string());
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

async fn setup(endpoint: &str, out: &Path, extra: &[&str]) -> Out {
    setup_in(None, &full(endpoint, out, extra), None).await
}

fn ok(out: &Out) -> &Value {
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    assert_eq!(out.json["status"], "ok", "{}", out.stdout);
    &out.json
}

fn err_code(out: &Out) -> &str {
    assert_eq!(out.code, 1, "{}{}", out.stdout, out.stderr);
    assert_eq!(out.json["status"], "error", "{}", out.stdout);
    out.json["code"].as_str().unwrap()
}

/// 用法错误：退出码 1、stdout 空、stderr 是用法说明。
fn assert_usage_error(out: &Out, why: &str) {
    assert_eq!(out.code, 1, "{why}: {}{}", out.stdout, out.stderr);
    assert!(out.stdout.is_empty(), "{why}: {}", out.stdout);
    assert!(out.stderr.contains(USAGE_MARK), "{why}: {}", out.stderr);
}

// ---------------------------------------------------------------------------------------------
// 假上游与目录台架
// ---------------------------------------------------------------------------------------------

/// 假 OpenAI 兼容上游（任何 OpenAI 兼容服务都有的两个接口），没有鉴权。聊天回答 `pong`。
async fn upstream(models: &[&str]) -> MockServer {
    upstream_replying(models, "pong").await
}

/// 同上，聊天回答 `reply`（两个上游同时在时，凭它认出请求落在了哪一个）。
async fn upstream_replying(models: &[&str], reply: &'static str) -> MockServer {
    let server = MockServer::start().await;
    let data: Vec<Value> = models
        .iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "fake"}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": data})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "model": body["model"],
                "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let count = body["input"].as_array().map_or(1, Vec::len);
            let data: Vec<Value> = (0..count)
                .map(|index| json!({"object": "embedding", "index": index, "embedding": [0.1, 0.2, 0.3]}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "model": body["model"],
                "data": data,
                "usage": {"prompt_tokens": 2, "total_tokens": 2}
            }))
        })
        .mount(&server)
        .await;
    server
}

async fn requests(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap()
}

/// 这些请求里没有任何凭据。
fn assert_anonymous(requests: &[wiremock::Request]) {
    for request in requests {
        for name in [
            "authorization",
            "proxy-authorization",
            "cookie",
            "x-api-key",
        ] {
            assert!(
                request.headers.get(name).is_none(),
                "{} {} 带了 {name}",
                request.method,
                request.url
            );
        }
    }
}

/// 目录里每个文件的 `(名字, 内容)`，按名字排序；不含暂存目录（它们以 `.idoris-setup-` 开头，应当已被清掉）。
fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut all: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                std::fs::read(entry.path()).unwrap_or_default(),
            )
        })
        .collect();
    all.sort();
    all
}

fn names(dir: &Path) -> Vec<String> {
    files(dir).into_iter().map(|(name, _)| name).collect()
}

fn read_card(dir: &Path, file: &str) -> ComponentCard {
    serde_yaml::from_str(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap()
}

/// 一个已有的组件目录：oMLX 卡加订阅卡（`setup omlx` 写出的那一组），再加一个不是卡的文件。
fn existing_components(root: &Path) -> PathBuf {
    let dir = root.join("components");
    std::fs::create_dir_all(&dir).unwrap();
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/components");
    for file in ["omlx.yaml", "subscription.yaml"] {
        std::fs::copy(config.join(file), dir.join(file)).unwrap();
    }
    std::fs::write(dir.join("notes.txt"), "operator notes\n").unwrap();
    dir
}

/// 没有人在听的端口（绑上又放掉）。
fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------------------------
// 成功路径
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn adopts_a_loopback_openai_server_and_writes_one_resident_card() {
    let server = upstream(&[MODEL, "other"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let result = setup(
        &server.uri(),
        &out,
        &[
            "--bind",
            "daily=qwen3-8b",
            "--bind",
            "deep=qwen3-8b",
            "--id",
            "lmstudio",
        ],
    )
    .await;
    let report = ok(&result);
    assert_eq!(report["models"], json!([MODEL, "other"]));
    assert_eq!(report["bindings"], json!({"daily": MODEL, "deep": MODEL}));
    assert_eq!(names(&out), ["lmstudio.yaml"], "只写这一张卡");
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        1,
        "暂存目录已清掉"
    );

    // 卡面：resident 的 loopback HTTP 卡，本地隐私，失败即关闭，免费，只有 chat 能力。
    let card = read_card(&out, "lmstudio.yaml");
    assert_eq!(card.provider.id, "lmstudio");
    assert_eq!(card.endpoint, server.uri());
    assert_eq!(
        card.form,
        idoris_contracts::component_card::Form::HttpService
    );
    assert_eq!(card.version_pin, "openai_compat@unknown");
    assert_eq!(
        card.provider.capabilities,
        [idoris_contracts::common::Capability::Chat]
    );
    assert_eq!(
        card.privacy_class,
        idoris_contracts::common::PrivacyClass::LocalOnly
    );
    assert_eq!(
        card.provider.privacy_class,
        idoris_contracts::common::PrivacyClass::LocalOnly
    );
    assert_eq!(
        card.allowed_egress,
        [idoris_contracts::component_card::Egress::Loopback]
    );
    assert!(card.fail_closed);
    assert_eq!(
        card.fallback_policy,
        idoris_contracts::common::FallbackPolicy::FailClosed
    );
    assert_eq!(card.provider.cost.input_per_m, 0.0);
    assert_eq!(card.provider.cost.output_per_m, 0.0);
    assert_eq!(
        card.provider.locality,
        idoris_contracts::provider::Locality::Loopback
    );
    let policy = card.load_policy.unwrap();
    assert_eq!(
        policy.mode,
        idoris_contracts::load_policy::LoadMode::Resident
    );
    assert_eq!(
        policy.keepalive,
        idoris_contracts::load_policy::Keepalive::Pinned { pinned: true }
    );
    assert_eq!(
        policy.admission,
        idoris_contracts::load_policy::Admission::Coexist
    );
    let extensions = card.extensions.as_ref().unwrap();
    assert_eq!(
        extensions["idoris_upstream"],
        json!({"kind": "openai_compat", "flavor": "openai_compat"})
    );
    assert_eq!(
        extensions["idoris_role_models"],
        json!({"daily": MODEL, "deep": MODEL})
    );
    assert!(!extensions.contains_key("idoris_default"));
    // `_degradation`：模板自带的那句（`idoris_upstream`、`idoris_default`）在前，通用流程加的（`idoris_role_models`）在后。
    let degradation = extensions["_degradation"].as_str().unwrap();
    let upstream_part = degradation.find("idoris_upstream").unwrap();
    let bindings_part = degradation.find("role aliases").unwrap();
    assert!(upstream_part < bindings_part, "{degradation}");
    // 与服务启动时同一个加载器。
    assert_eq!(load_components(&out, false).unwrap().len(), 1);

    // 探测：恰好一次 `GET /v1/models`，不带任何凭据；没有别的请求。
    let seen = requests(&server).await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].method, wiremock::http::Method::GET);
    assert_eq!(seen[0].url.path(), "/v1/models");
    assert_eq!(seen[0].url.query(), None);
    assert_anonymous(&seen);
}

/// 输出：`setup omlx` 的全部字段，加 `id`、`card_file`、`default`。
#[tokio::test(flavor = "multi_thread")]
async fn the_output_json_has_the_setup_omlx_fields_and_the_new_ones() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lm"],
    )
    .await;
    let report = ok(&result);
    let dir = out.canonicalize().unwrap().display().to_string();
    assert_eq!(
        *report,
        json!({
            "status": "ok",
            "mode": "adopt",
            "id": "lm",
            "card_file": "lm.yaml",
            "default": false,
            "endpoint": server.uri(),
            "auth": "none",
            "models": [MODEL],
            "bindings": {"daily": MODEL},
            "engine": {"engine_kind": "openai_compat", "installed": false, "executable": null},
            "components_dir": dir,
            "next": {"env": {"IDORIS_COMPONENTS_DIR": dir}}
        })
    );
    assert!(report["next"].get("needs_env").is_none());
}

/// 与 Agent24 在 v0.5.0 上手写并实测可用的 LM Studio 卡（`lmstudio_probe.py` 里的 CARD）对照：除了声明的标注
/// （`version_pin` 的产品名、`idoris_upstream`、`_degradation` 的措辞），逐字段相同，所以在 iDoris 里行为等价。
const AGENT24_LMSTUDIO_CARD: &str = r#"provider:
  id: lmstudio
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:{port}"
version_pin: "lmstudio@0"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
extensions:
  _degradation: "Removing these bindings disables role aliases on this backend."
  idoris_role_models:
    daily: qwen3-8b
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_generated_card_equals_agent24s_hand_written_lmstudio_card_apart_from_the_declared_annotations()
 {
    let server = upstream(&[MODEL]).await;
    let port = server.address().port();
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    ok(&setup(
        &format!("http://127.0.0.1:{port}"),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lmstudio"],
    )
    .await);
    let mut generated = read_card(&out, "lmstudio.yaml");
    let mut reference: ComponentCard =
        serde_yaml::from_str(&AGENT24_LMSTUDIO_CARD.replace("{port}", &port.to_string())).unwrap();

    // 声明的差别：`version_pin` 的产品名（规范 §2.3：`setup` 生成的卡，产品名就是种类名）、`idoris_upstream`、
    // `_degradation` 的措辞。
    assert_eq!(reference.version_pin, "lmstudio@0");
    assert_eq!(generated.version_pin, "openai_compat@unknown");
    let generated_extensions = generated.extensions.as_mut().unwrap();
    assert!(generated_extensions.remove("idoris_upstream").is_some());
    generated_extensions.remove("_degradation").unwrap();
    reference
        .extensions
        .as_mut()
        .unwrap()
        .remove("_degradation")
        .unwrap();
    generated.version_pin = reference.version_pin.clone();

    // 其余（provider 的每个字段、form、endpoint、隐私与出网、失败即关闭、加载策略、角色绑定）逐字段相同。
    assert_eq!(generated, reference);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_is_added_to_an_existing_components_directory_and_nothing_else_in_it_changes() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = existing_components(tmp.path());
    let before = files(&out);
    assert_eq!(names(&out), ["notes.txt", "omlx.yaml", "subscription.yaml"]);
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lmstudio"],
    )
    .await;
    ok(&result);
    let after = files(&out);
    assert_eq!(
        after
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [
            "lmstudio.yaml",
            "notes.txt",
            "omlx.yaml",
            "subscription.yaml"
        ]
    );
    for (name, content) in &before {
        assert_eq!(
            after.iter().find(|(n, _)| n == name).map(|(_, c)| c),
            Some(content),
            "{name} 被改了"
        );
    }
    let cards = load_components(&out, false).unwrap();
    let mut ids: Vec<_> = cards.iter().map(|c| c.provider.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["lmstudio", "omlx", "subscription"]);
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        1,
        "暂存目录已清掉，只剩 components"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_out_dir_is_created_with_only_the_new_card() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("deep/er/components");
    assert!(!out.exists());
    ok(&setup(&server.uri(), &out, &["--bind", "fast=qwen3-8b"]).await);
    assert_eq!(names(&out).len(), 1);
    assert_eq!(load_components(&out, false).unwrap().len(), 1);
}

/// `--out-dir .`：暂存目录就建在工作目录里，不能被当成外来文件。
#[tokio::test(flavor = "multi_thread")]
async fn out_dir_dot_adds_the_card_to_the_working_directory() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let cwd = existing_components(tmp.path());
    let args = full(
        &server.uri(),
        Path::new("."),
        &["--bind", "daily=qwen3-8b", "--id", "lm"],
    );
    ok(&setup_in(Some(&cwd), &args, None).await);
    assert_eq!(
        names(&cwd),
        ["lm.yaml", "notes.txt", "omlx.yaml", "subscription.yaml"]
    );
    // 再来一次：同名卡在，没有 --force 是 id_conflict；加 --force 成功。
    assert_eq!(
        err_code(&setup_in(Some(&cwd), &args, None).await),
        "id_conflict"
    );
    let mut forced = args.clone();
    forced.push("--force".into());
    ok(&setup_in(Some(&cwd), &forced, None).await);
    load_components(&cwd, false).unwrap();
}

/// 缺省的 id 由种类与端口派生，同一个端点每次得到同一个；加 `--force` 重新生成，文件逐字节相同。
#[tokio::test(flavor = "multi_thread")]
async fn the_default_id_comes_from_the_port_and_regenerating_with_force_is_byte_identical() {
    let server = upstream(&[MODEL]).await;
    let port = server.address().port();
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let result = setup(&server.uri(), &out, &["--bind", "daily=qwen3-8b"]).await;
    let report = ok(&result);
    let id = format!("openai_compat-{port}");
    assert_eq!(report["id"], id);
    assert_eq!(report["card_file"], format!("{id}.yaml"));
    assert_eq!(names(&out), [format!("{id}.yaml")]);
    let first = files(&out);
    // `localhost` 写法：同一个端口，同一个缺省 id。
    ok(&setup(
        &format!("http://localhost:{port}"),
        &out,
        &["--bind", "daily=qwen3-8b", "--force"],
    )
    .await);
    assert_eq!(names(&out), [format!("{id}.yaml")]);
    let second = read_card(&out, &format!("{id}.yaml"));
    assert_eq!(second.endpoint, format!("http://localhost:{port}"));
    // 同一条命令再来一次（--force）：逐字节相同。
    let third_run = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--force"],
    )
    .await;
    ok(&third_run);
    assert_eq!(files(&out), first);
}

// ---------------------------------------------------------------------------------------------
// 拒绝与守卫
// ---------------------------------------------------------------------------------------------

/// loopback 之外的主机、带路径、带凭据、不是 http(s) 的 endpoint：在任何探测与写入之前拒绝。
#[tokio::test(flavor = "multi_thread")]
async fn a_non_loopback_or_malformed_endpoint_is_refused_before_anything_is_contacted_or_written() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    for bad in [
        "http://example.com:1234",
        "http://10.0.0.5:1234",
        "http://192.168.1.2:1234",
        "http://0.0.0.0:1234",
        "http://127.0.0.2:1234",
        "http://127.0.0.1.evil.com:1234",
        "http://localhost.:1234",
        "http://[::ffff:127.0.0.1]:1234",
        "http://[::2]:1234",
        "http://user:pw@127.0.0.1:1234",
        "http://127.0.0.1:1234/v1",
        "http://127.0.0.1:1234/?x=1",
        "http://127.0.0.1:1234/#f",
        "ftp://127.0.0.1:1234",
        "127.0.0.1:1234",
        "nonsense",
        "",
    ] {
        let result = setup(bad, &out, &["--bind", "daily=m"]).await;
        assert_eq!(err_code(&result), "invalid_endpoint", "{bad:?}");
    }
    assert!(!out.exists(), "什么也没写");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    // 对照：三种回环写法都被接受（`[::1]` 上没有人听，所以走到探测、得 `unreachable`，而不是 `invalid_endpoint`）。
    let server = upstream(&[MODEL]).await;
    let port = server.address().port();
    for good in [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ] {
        let result = setup(
            &good,
            &tmp.path().join("ok"),
            &["--bind", "daily=qwen3-8b", "--force"],
        )
        .await;
        ok(&result);
    }
    let result = setup(
        &format!("http://[::1]:{}", closed_port()),
        &out,
        &["--bind", "daily=m"],
    )
    .await;
    assert_eq!(err_code(&result), "unreachable");
}

/// 没有密钥通道，也没有别的目标：这些旗标都是用法错误（退出码 1、stdout 空），在任何探测之前。
#[tokio::test(flavor = "multi_thread")]
async fn the_command_has_no_key_channel_and_no_other_target_or_load_flags() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let base = full(&server.uri(), &out, &["--bind", "daily=qwen3-8b"]);
    // 对照：不加任何旗标时成功。
    ok(&setup_in(None, &base, None).await);
    std::fs::remove_dir_all(&out).unwrap();
    let before = requests(&server).await.len();
    for extra in [
        vec!["--api-key-stdin"],
        vec!["--api-key", "secret"],
        vec!["--no-probe"],
        vec!["--auto"],
        vec!["--managed"],
        vec!["--managed-port", "19000"],
        vec!["--load-mode", "resident"],
        vec!["--load-mode", "on_demand"],
        // 重复的旗标。
        vec!["--force", "--force"],
        vec!["--default", "--default"],
        vec!["--id", "a", "--id", "b"],
    ] {
        let mut args = base.clone();
        args.extend(extra.iter().map(|s| s.to_string()));
        assert_usage_error(
            &setup_in(None, &args, Some(STDIN_KEY)).await,
            &format!("{extra:?}"),
        );
    }
    assert!(!out.exists());
    assert_eq!(requests(&server).await.len(), before, "用法错误不探测");
}

/// 缺必选项、`--kind` 不是 `openai_compat`、`--bind` 缺失或重复：用法错误。
#[tokio::test(flavor = "multi_thread")]
async fn missing_required_flags_a_bad_kind_and_missing_or_duplicate_bindings_are_usage_errors() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let out_str = out.display().to_string();
    let endpoint = server.uri();
    let good = |extra: &[&str]| {
        let mut args = strings(&[
            "--kind",
            "openai_compat",
            "--endpoint",
            &endpoint,
            "--out-dir",
            &out_str,
        ]);
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    };
    // 缺 `--bind`。
    assert_usage_error(&setup_in(None, &good(&[]), None).await, "没有 --bind");
    // 缺 `--kind`、`--endpoint`、`--out-dir`。
    for skip in ["--kind", "--endpoint", "--out-dir"] {
        let mut args = good(&["--bind", "daily=qwen3-8b"]);
        let at = args.iter().position(|a| a == skip).unwrap();
        args.drain(at..at + 2);
        assert_usage_error(&setup_in(None, &args, None).await, skip);
    }
    // `--kind` 只有 `openai_compat`：规范的别的保留名（`lmstudio`、`ollama`……）、大小写变体、空值都不是取值。
    for kind in [
        "lmstudio",
        "ollama",
        "omlx",
        "OpenAI_Compat",
        "openai_compat ",
        "",
    ] {
        let mut args = good(&["--bind", "daily=qwen3-8b"]);
        let at = args.iter().position(|a| a == "--kind").unwrap();
        args[at + 1] = kind.to_string();
        assert_usage_error(&setup_in(None, &args, None).await, kind);
    }
    // `--bind` 的形状：没有 `=`、空角色、空模型、同一个角色绑两次。
    for bind in ["daily", "=m", "daily="] {
        assert_usage_error(&setup_in(None, &good(&["--bind", bind]), None).await, bind);
    }
    assert_usage_error(
        &setup_in(
            None,
            &good(&["--bind", "daily=a", "--bind", "daily=b"]),
            None,
        )
        .await,
        "同一个角色绑两次",
    );
    assert!(!out.exists());
    assert!(requests(&server).await.is_empty(), "用法错误不探测");
}

/// 探测不带凭据：环境里有 oMLX key、stdin 上有 key，上游收到的请求里一个凭据也没有，输出与卡里也没有它们。
#[tokio::test(flavor = "multi_thread")]
async fn no_credentials_leave_the_process_even_with_an_omlx_key_in_the_environment_and_a_key_on_stdin()
 {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let args = full(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lm"],
    );
    let result = setup_in(None, &args, Some(&format!("{STDIN_KEY}\n"))).await;
    ok(&result);
    let seen = requests(&server).await;
    assert_eq!(seen.len(), 1);
    assert_anonymous(&seen);
    let everything = format!(
        "{}{}{}",
        result.stdout,
        result.stderr,
        std::fs::read_to_string(out.join("lm.yaml")).unwrap()
    );
    for canary in [OMLX_KEY, STDIN_KEY] {
        assert!(!everything.contains(canary), "{canary} 出现在输出或卡里");
    }
}

/// 命令不读 stdin：stdin 一直开着、什么也不写，它也照常结束。
#[tokio::test(flavor = "multi_thread")]
async fn the_command_does_not_wait_for_stdin() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let args = full(&server.uri(), &out, &["--bind", "daily=qwen3-8b"]);
    let code = tokio::task::spawn_blocking(move || {
        let mut cmd = idoris();
        cmd.args(["setup", "upstream"])
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = Reaped::spawn(&mut cmd);
        // 这个管道到命令退出为止都不关：读 stdin 的命令会一直等在这里。
        let _held = child.child().stdin.take().unwrap();
        let limit = startup_wait::startup_observe_limit();
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.child().try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "命令在 stdin 上等了 {}s",
                limit.as_secs()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    })
    .await
    .unwrap();
    assert_eq!(code, Some(0));
    assert_eq!(names(&out).len(), 1);
}

/// 上游要密钥（401 / 403）：最小版不带密钥，报 `auth_required`，不写卡；探测本身不带凭据。
#[tokio::test(flavor = "multi_thread")]
async fn an_upstream_that_wants_a_key_is_refused_and_nothing_is_written() {
    for status in [401u16, 403] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("components");
        let result = setup(&server.uri(), &out, &["--bind", "daily=m"]).await;
        assert_eq!(err_code(&result), "auth_required", "{status}");
        assert!(
            result.json["message"]
                .as_str()
                .unwrap()
                .contains("不带密钥"),
            "{}",
            result.stdout
        );
        assert!(!out.exists());
        let seen = requests(&server).await;
        assert_eq!(seen.len(), 1, "{status}：被拒之后不再带 key 重试");
        assert_anonymous(&seen);
    }
}

/// 只 chat：`embed`、`rerank`、`vision` 不能绑，未知角色也不行；排在任何探测之前（上游一个请求也没收到），什么也不写。
#[tokio::test(flavor = "multi_thread")]
async fn only_chat_roles_can_be_bound_and_the_refusal_comes_before_any_request() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    for role in ["embed", "rerank", "vision", "bogus"] {
        let bind = format!("{role}=qwen3-8b");
        let result = setup(&server.uri(), &out, &["--bind", &bind]).await;
        assert_eq!(err_code(&result), "invalid_role", "{role}");
        if role != "bogus" {
            assert!(
                result.json["message"].as_str().unwrap().contains("chat"),
                "{}",
                result.stdout
            );
        }
    }
    // 一个合格、一个不合格：整体拒绝。
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--bind", "embed=qwen3-8b"],
    )
    .await;
    assert_eq!(err_code(&result), "invalid_role");
    assert!(requests(&server).await.is_empty(), "拒绝在探测之前");
    assert!(!out.exists());
    // 五个 chat 角色都能绑。
    let all = [
        "--bind",
        "fast=qwen3-8b",
        "--bind",
        "daily=qwen3-8b",
        "--bind",
        "deep=qwen3-8b",
        "--bind",
        "decide=qwen3-8b",
        "--bind",
        "auto=qwen3-8b",
    ];
    let result = setup(&server.uri(), &out, &all).await;
    ok(&result);
    assert_eq!(
        read_card(
            &out,
            &format!("openai_compat-{}.yaml", server.address().port())
        )
        .extensions
        .unwrap()["idoris_role_models"]
            .as_object()
            .unwrap()
            .len(),
        5
    );
}

/// 响应不是 OpenAI 的模型列表：非 200、不是 JSON、没有 `data` 数组、`data` 里有没有 `id` 的项、过大；
/// 重定向不跟随（目标一个请求也没收到）。都是 `not_openai_compatible`，不写卡。
#[tokio::test(flavor = "multi_thread")]
async fn a_response_that_is_not_an_openai_model_list_is_refused() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let cases: Vec<(&str, ResponseTemplate)> = vec![
        ("404", ResponseTemplate::new(404)),
        ("500", ResponseTemplate::new(500)),
        // 状态码不是 200，正文却是一个合格的列表：状态码本身就够拒绝（出了错的服务，或者是别的东西，不是模型列表）。
        (
            "404 with a valid list",
            ResponseTemplate::new(404).set_body_json(json!({"data": [{"id": "m"}]})),
        ),
        (
            "500 with a valid list",
            ResponseTemplate::new(500).set_body_json(json!({"data": [{"id": "m"}]})),
        ),
        (
            "302 with a valid list",
            ResponseTemplate::new(302).set_body_json(json!({"data": [{"id": "m"}]})),
        ),
        (
            "201 with a valid list",
            ResponseTemplate::new(201).set_body_json(json!({"data": [{"id": "m"}]})),
        ),
        (
            "202 with a valid list",
            ResponseTemplate::new(202).set_body_json(json!({"data": [{"id": "m"}]})),
        ),
        (
            "not json",
            ResponseTemplate::new(200).set_body_string("<html>hi</html>"),
        ),
        (
            "no data",
            ResponseTemplate::new(200).set_body_json(json!({"hello": 1})),
        ),
        (
            "data is not an array",
            ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "m"}})),
        ),
        (
            "row without id",
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "m"}, {"object": "model"}]})),
        ),
        (
            "row id is not a string",
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": 7}]})),
        ),
        (
            "oversized",
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "m"}], "pad": "x".repeat(3 * 1024 * 1024)})),
        ),
    ];
    for (label, response) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(response)
            .mount(&server)
            .await;
        let result = setup(&server.uri(), &out, &["--bind", "daily=m"]).await;
        assert_eq!(
            err_code(&result),
            "not_openai_compatible",
            "{label}: {}",
            result.stdout
        );
        assert!(!out.exists(), "{label}");
    }
    // 对照：只差一点就合格的列表（空列表）是合格的形状；绑定因为模型不在其中而失败。
    let empty = upstream(&[]).await;
    let result = setup(&empty.uri(), &out, &["--bind", "daily=m"]).await;
    assert_eq!(err_code(&result), "model_not_found");

    let target = upstream(&[MODEL]).await;
    let redirector = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/v1/models", target.uri()).as_str()),
        )
        .mount(&redirector)
        .await;
    let result = setup(&redirector.uri(), &out, &["--bind", "daily=qwen3-8b"]).await;
    assert_eq!(err_code(&result), "not_openai_compatible");
    assert!(requests(&target).await.is_empty(), "重定向的目标不应被请求");
    assert!(!out.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_endpoint_is_unreachable_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let endpoint = format!("http://127.0.0.1:{}", closed_port());
    let result = setup(&endpoint, &out, &["--bind", "daily=m"]).await;
    assert_eq!(err_code(&result), "unreachable");
    assert!(!out.exists());
}

/// 每个绑定的模型都要在上游的列表里；错误里不列出模型。
#[tokio::test(flavor = "multi_thread")]
async fn every_bound_model_must_be_in_the_upstreams_list() {
    let server = upstream(&[MODEL, "second"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    for binds in [
        vec!["--bind", "daily=nope"],
        // 第二个才不在：不能只核对第一个。
        vec!["--bind", "daily=qwen3-8b", "--bind", "deep=nope"],
    ] {
        let result = setup(&server.uri(), &out, &binds).await;
        assert_eq!(err_code(&result), "model_not_found", "{binds:?}");
        assert!(
            !result.stdout.contains("second"),
            "错误里不列模型：{}",
            result.stdout
        );
        assert!(!out.exists());
    }
    ok(&setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--bind", "deep=second"],
    )
    .await);
}

/// `--id` 不合规或是保留名：在任何探测之前拒绝。
#[tokio::test(flavor = "multi_thread")]
async fn an_id_that_is_malformed_or_reserved_is_refused_before_any_request() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    for bad in [
        "",
        "LMStudio",
        "-x",
        "_x",
        ".x",
        "a b",
        "a.b",
        "a/b",
        "../x",
        "x\n",
        "中文",
        &"a".repeat(49),
        // 保留：`omlx`（oMLX 的 key 按这个 provider.id 注入）、伴生卡、会被加载器跳过的 `mock`、订阅、云端档。
        "omlx",
        "omlx-embed",
        "mock",
        "subscription",
        "idoris-cloud",
    ] {
        let result = setup(
            &server.uri(),
            &out,
            &["--bind", "daily=qwen3-8b", "--id", bad],
        )
        .await;
        assert_eq!(
            err_code(&result),
            "invalid_id",
            "{bad:?}: {}",
            result.stdout
        );
    }
    assert!(requests(&server).await.is_empty(), "拒绝在探测之前");
    assert!(!out.exists());
    // 对照：边界上的合法值。
    let longest = "a".repeat(48);
    for good in ["x", "0", "a-b_c", longest.as_str()] {
        ok(&setup(
            &server.uri(),
            &tmp.path().join(format!("c-{}", good.len())),
            &["--bind", "daily=qwen3-8b", "--id", good],
        )
        .await);
    }
}

/// 同名卡：没有 `--force` 是 `id_conflict`，文件原样；加 `--force` 只替换这一个文件，别的文件原样。
#[tokio::test(flavor = "multi_thread")]
async fn a_same_named_card_needs_force_and_only_that_file_is_replaced() {
    let server = upstream(&[MODEL, "second"]).await;
    let tmp = TempDir::new().unwrap();
    let out = existing_components(tmp.path());
    ok(&setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lm"],
    )
    .await);
    let first = files(&out);

    let probes_before = requests(&server).await.len();
    let again = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=second", "--id", "lm"],
    )
    .await;
    assert_eq!(err_code(&again), "id_conflict");
    assert!(again.json["message"].as_str().unwrap().contains("--force"));
    assert_eq!(files(&out), first, "没有 --force：什么也不变");
    assert_eq!(
        requests(&server).await.len(),
        probes_before,
        "同名卡的冲突在探测之前就发现了"
    );

    ok(&setup(
        &server.uri(),
        &out,
        &["--bind", "daily=second", "--id", "lm", "--force"],
    )
    .await);
    let second = files(&out);
    let changed: Vec<_> = first
        .iter()
        .zip(&second)
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.0.as_str())
        .collect();
    assert_eq!(changed, ["lm.yaml"], "只有同名的卡被替换");
    assert_eq!(
        read_card(&out, "lm.yaml").extensions.unwrap()["idoris_role_models"],
        json!({"daily": "second"})
    );

    // 同名的东西不是普通文件（目录、符号链接）：连 --force 也不替换。
    std::fs::remove_file(out.join("lm.yaml")).unwrap();
    std::fs::create_dir(out.join("lm.yaml")).unwrap();
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lm", "--force"],
    )
    .await;
    assert_eq!(err_code(&result), "id_conflict");
    std::fs::remove_dir(out.join("lm.yaml")).unwrap();
    std::fs::write(out.join("target.txt"), "keep").unwrap();
    symlink(out.join("target.txt"), out.join("lm.yaml")).unwrap();
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lm", "--force"],
    )
    .await;
    assert_eq!(err_code(&result), "id_conflict");
    assert_eq!(
        std::fs::read_to_string(out.join("target.txt")).unwrap(),
        "keep"
    );
}

/// 别的文件里已经有同一个 `provider.id`：`id_conflict`，连 `--force` 也不能覆盖（它只管同名文件）；目录不变。
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_id_already_declared_in_another_file_is_an_id_conflict_even_with_force() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = existing_components(tmp.path());
    // 手写的卡，文件名与 id 不同。
    let hand = std::fs::read_to_string(out.join("omlx.yaml"))
        .unwrap()
        .replace("id: omlx", "id: lmstudio");
    std::fs::write(out.join("legacy-lmstudio.yaml"), hand).unwrap();
    let before = files(&out);
    for extra in [vec![], vec!["--force"]] {
        let mut args = vec!["--bind", "daily=qwen3-8b", "--id", "lmstudio"];
        args.extend(extra);
        let result = setup(&server.uri(), &out, &args).await;
        assert_eq!(
            err_code(&result),
            "id_conflict",
            "{args:?}: {}",
            result.stdout
        );
        assert_eq!(files(&out), before);
    }
    // 也不能叫 oMLX 的 id：这里 `omlx.yaml` 在，`omlx` 又是保留名，先被 `invalid_id` 拦下。
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "omlx"],
    )
    .await;
    assert_eq!(err_code(&result), "invalid_id");
}

/// 现有的卡本身加载不了：不往一个本来就起不来的目录里加卡，说明里带出加载器的原因；目录不变。
#[tokio::test(flavor = "multi_thread")]
async fn an_existing_card_that_does_not_load_blocks_the_addition_and_the_directory_is_unchanged() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = existing_components(tmp.path());
    std::fs::write(out.join("broken.yaml"), "not: [valid").unwrap();
    let before = files(&out);
    let result = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lm"],
    )
    .await;
    assert_eq!(err_code(&result), "validation_failed");
    let message = result.json["message"].as_str().unwrap();
    assert!(message.contains("broken.yaml"), "{message}");
    assert!(
        message.contains(&out.display().to_string()) && !message.contains("idoris-setup"),
        "说明里是 out-dir 的路径，不是暂存目录：{message}"
    );
    assert_eq!(files(&out), before);
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        1,
        "暂存目录已清掉"
    );
}

/// 加载器不认的绑定（模型 id 前后有空白）：探测过了也落不了盘，什么也不留下。
#[tokio::test(flavor = "multi_thread")]
async fn a_binding_the_card_loader_rejects_leaves_nothing_behind() {
    let server = upstream(&[MODEL, "bad "]).await;
    let tmp = TempDir::new().unwrap();
    let out = existing_components(tmp.path());
    let before = files(&out);
    let result = setup(&server.uri(), &out, &["--bind", "daily=bad ", "--id", "lm"]).await;
    assert_eq!(err_code(&result), "validation_failed");
    assert_eq!(files(&out), before);
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
}

/// `--out-dir` 不是目录：普通文件、路径中间有一个普通文件（读元数据得到 ENOTDIR，不是「不存在」）、指向不存在的目标的
/// 符号链接（`is_dir()` 为假，但读元数据成功，再往下查同名卡得到「不存在」，只有「不是目录」那一档能拦住它，否则要等到
/// 落盘时才以 `write_failed` 失败）。都在探测之前拒绝，什么也不创建、不改。
#[tokio::test(flavor = "multi_thread")]
async fn an_out_dir_that_is_not_a_directory_is_refused_before_probing() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("file");
    std::fs::write(&file, "i am a file").unwrap();
    for (label, out) in [
        ("a file", file.clone()),
        ("below a file", file.join("sub").join("components")),
    ] {
        let result = setup(&server.uri(), &out, &["--bind", "daily=qwen3-8b"]).await;
        assert_eq!(err_code(&result), "out_dir_not_empty", "{label}");
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "i am a file");

    let nowhere = tmp.path().join("nowhere");
    let dangling = tmp.path().join("dangling");
    symlink(&nowhere, &dangling).unwrap();
    let result = setup(&server.uri(), &dangling, &["--bind", "daily=qwen3-8b"]).await;
    assert_eq!(err_code(&result), "out_dir_not_empty", "dangling symlink");
    assert_eq!(std::fs::read_link(&dangling).unwrap(), nowhere);
    assert!(!nowhere.exists());

    assert!(requests(&server).await.is_empty(), "拒绝在探测之前");
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        2,
        "只有 file 与 dangling，什么也没有多出来"
    );
}

/// `--default`：卡带 `extensions.idoris_default: true`，输出的 `default` 为真；至多一张默认卡。
#[tokio::test(flavor = "multi_thread")]
async fn default_is_written_to_the_card_and_at_most_one_card_may_be_the_default() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = existing_components(tmp.path());
    let bind = ["--bind", "daily=qwen3-8b"];
    // 没有 `--default`：卡上没有这个键，输出里 `default` 为假。
    let plain = setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "plain"]].concat(),
    )
    .await;
    assert_eq!(ok(&plain)["default"], false);
    assert!(
        !read_card(&out, "plain.yaml")
            .extensions
            .unwrap()
            .contains_key("idoris_default")
    );

    let first = setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "first", "--default"]].concat(),
    )
    .await;
    assert_eq!(ok(&first)["default"], true);
    assert_eq!(
        read_card(&out, "first.yaml").extensions.unwrap()["idoris_default"],
        json!(true)
    );
    let before = files(&out);
    // 第二张默认卡：拒绝，目录不变，说明里点名现有的默认卡。
    let second = setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "second", "--default"]].concat(),
    )
    .await;
    assert_eq!(err_code(&second), "validation_failed");
    assert!(
        second.json["message"].as_str().unwrap().contains("first"),
        "{}",
        second.stdout
    );
    assert_eq!(files(&out), before);
    // 不当默认的卡照常加；现有的默认卡自己重新生成（--force，仍当默认）也可以：被替换的那张不算「别的默认」。
    ok(&setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "third"]].concat(),
    )
    .await);
    let again = setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "first", "--default", "--force"]].concat(),
    )
    .await;
    assert_eq!(ok(&again)["default"], true);
    // 先去掉旧的默认（不带 --default 重新生成），新的才能当默认。
    ok(&setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "first", "--force"]].concat(),
    )
    .await);
    let moved = setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "second", "--default"]].concat(),
    )
    .await;
    assert_eq!(ok(&moved)["default"], true);
    // 目录里本来就有两张默认卡（手写的）：不是这次加卡造成的，加一张不当默认的卡不受影响。
    let hand = std::fs::read_to_string(out.join("second.yaml")).unwrap();
    std::fs::write(
        out.join("second-copy.yaml"),
        hand.replace("id: second", "id: second-copy"),
    )
    .unwrap();
    ok(&setup(
        &server.uri(),
        &out,
        &[&bind[..], &["--id", "fourth"]].concat(),
    )
    .await);
}

/// `idoris_default` 只认布尔真：字符串 `"true"`、数字 1 都不算默认。
#[test]
fn only_a_boolean_true_makes_a_card_the_default() {
    let mut card = card_of(&card_yaml("lm"));
    assert!(!idoris_router::engine_kind::declares_default(&card));
    for (value, expected) in [
        (json!(true), true),
        (json!(false), false),
        (json!("true"), false),
        (json!(1), false),
        (json!(null), false),
        (json!({"v": true}), false),
    ] {
        card.extensions
            .as_mut()
            .unwrap()
            .insert("idoris_default".into(), value.clone());
        assert_eq!(
            idoris_router::engine_kind::declares_default(&card),
            expected,
            "{value}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 与 `setup omlx` 的先后顺序
// ---------------------------------------------------------------------------------------------

/// 跑 `idoris setup omlx <args>`（不探测：`--no-probe`，所以不需要假的 oMLX）。
async fn setup_omlx_no_probe(out: &Path, extra: &[&str]) -> Out {
    let (out, extra) = (
        out.to_owned(),
        extra.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    );
    tokio::task::spawn_blocking(move || {
        let mut cmd = idoris();
        cmd.args([
            "setup",
            "omlx",
            "--endpoint",
            "http://127.0.0.1:8000",
            "--no-probe",
            "--out-dir",
        ])
        .arg(&out)
        .args(&extra)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let output = Reaped::spawn(&mut cmd).finish();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Out {
            code: output.status.code().unwrap(),
            json: serde_json::from_str(&stdout).unwrap_or(Value::Null),
            stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

/// 顺序是先 `setup omlx`（它要求目录是空的），再对每个上游 `setup upstream` 往里加卡。反过来不行：`setup omlx` 的
/// `--force` 只放行它自己的文件，目录里有 `setup upstream` 写的卡时报 `out_dir_not_empty`，卡原样不动。这条钉住
/// 规范回填与给 Agent24 的用法说明里的「先 omlx、后 upstream」；以后若让 `setup omlx --force` 容忍别的卡，它要随之改。
#[tokio::test(flavor = "multi_thread")]
async fn setup_omlx_comes_first_and_refuses_a_directory_that_already_holds_an_upstream_card() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    // 先 `setup omlx`：空（不存在的）目录，得到 oMLX 卡与订阅卡。
    let first = setup_omlx_no_probe(&out, &[]).await;
    assert_eq!(first.code, 0, "{}{}", first.stdout, first.stderr);
    assert_eq!(names(&out), ["omlx.yaml", "subscription.yaml"]);
    // 再 `setup upstream` 往里加卡。
    ok(&setup(
        &server.uri(),
        &out,
        &["--bind", "daily=qwen3-8b", "--id", "lmstudio"],
    )
    .await);
    assert_eq!(
        names(&out),
        ["lmstudio.yaml", "omlx.yaml", "subscription.yaml"]
    );
    let before = files(&out);
    // 反过来：再跑 `setup omlx`，不管有没有 `--force`，都因为目录里有它不管的卡而拒绝，什么也不变。
    for extra in [vec![], vec!["--force"]] {
        let result = setup_omlx_no_probe(&out, &extra).await;
        assert_eq!(err_code(&result), "out_dir_not_empty", "{extra:?}");
        assert_eq!(files(&out), before, "{extra:?}");
    }
    // 对照：没有上游卡的目录，`setup omlx --force` 照常整体重新生成。
    std::fs::remove_file(out.join("lmstudio.yaml")).unwrap();
    let again = setup_omlx_no_probe(&out, &["--force"]).await;
    assert_eq!(again.code, 0, "{}{}", again.stdout, again.stderr);
    assert_eq!(names(&out), ["omlx.yaml", "subscription.yaml"]);
}

// ---------------------------------------------------------------------------------------------
// 通用流程里「加一张卡」这条路径（库级，不经过命令行）
// ---------------------------------------------------------------------------------------------

fn card_yaml(id: &str) -> String {
    AGENT24_LMSTUDIO_CARD
        .replace("{port}", "1")
        .replace("id: lmstudio", &format!("id: {id}"))
}

fn card_of(yaml: &str) -> ComponentCard {
    serde_yaml::from_str(yaml).unwrap()
}

/// 检查之后才出现的同名文件不被盖掉：`write_added` 自己再核对一次（`hard_link` 遇到已存在的目标失败），不靠前面的检查。
#[test]
fn a_same_named_file_that_appears_after_the_checks_is_not_overwritten() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("lm.yaml"), "appeared meanwhile").unwrap();
    let yaml = card_yaml("lm");
    let error = write_added(&out, false, "lm.yaml", &yaml, &card_of(&yaml)).unwrap_err();
    assert_eq!(error.code, "id_conflict");
    assert_eq!(
        std::fs::read_to_string(out.join("lm.yaml")).unwrap(),
        "appeared meanwhile"
    );
    assert_eq!(names(&out), ["lm.yaml"], "暂存目录与残留都没有");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    // 对照：`force` 时原子替换这一个文件。
    write_added(&out, true, "lm.yaml", &yaml, &card_of(&yaml)).unwrap();
    assert_eq!(std::fs::read_to_string(out.join("lm.yaml")).unwrap(), yaml);
}

/// 落在不是目录的路径上（普通文件、悬空符号链接）：前置核对本来就会先拦下，这里直接调 `write_added` 看落盘那一层自己
/// 也站得住：报 `write_failed`，原有的文件、符号链接与它的目标都原样，暂存目录清掉，`force` 与否都一样。
#[test]
fn write_added_into_a_path_that_is_not_a_directory_fails_and_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("file");
    std::fs::write(&file, "i am a file").unwrap();
    let nowhere = tmp.path().join("nowhere");
    let dangling = tmp.path().join("dangling");
    symlink(&nowhere, &dangling).unwrap();
    let yaml = card_yaml("lm");
    for force in [false, true] {
        for (label, target) in [("a file", &file), ("a dangling symlink", &dangling)] {
            let error = write_added(target, force, "lm.yaml", &yaml, &card_of(&yaml)).unwrap_err();
            assert_eq!(error.code, "write_failed", "{label} force={force}");
        }
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "i am a file");
    assert_eq!(std::fs::read_link(&dangling).unwrap(), nowhere);
    assert!(!nowhere.exists());
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        2,
        "只有 file 与 dangling：暂存目录清掉了"
    );
}

/// 文件名要是纯文件名且带加载器会读的扩展名：路径分隔符、`..`、点开头、别的扩展名都拒绝，什么也不写。
#[test]
fn card_file_names_that_escape_or_would_not_be_loaded_are_refused() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let yaml = card_yaml("lm");
    for bad in [
        "",
        "../lm.yaml",
        "a/lm.yaml",
        "a\\lm.yaml",
        ".lm.yaml",
        "..",
        "lm.txt",
        "lm",
        "lm.yaml.bak",
        "lm\0.yaml",
    ] {
        let error = write_added(&out, false, bad, &yaml, &card_of(&yaml)).unwrap_err();
        assert_eq!(error.code, "validation_failed", "{bad:?}");
    }
    assert!(!out.exists());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    // 对照：`.yml` 也是加载器读的扩展名。
    write_added(&out, false, "lm.yml", &yaml, &card_of(&yaml)).unwrap();
    assert_eq!(names(&out), ["lm.yml"]);
}

/// 一个叫 `x.yaml` 的目录：加载器读不了它，镜像时就失败，不写新卡。
#[test]
fn an_unreadable_entry_that_the_loader_would_read_blocks_the_addition() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    std::fs::create_dir_all(out.join("trap.yaml")).unwrap();
    let yaml = card_yaml("lm");
    let error = write_added(&out, false, "lm.yaml", &yaml, &card_of(&yaml)).unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("trap.yaml"), "{}", error.message);
    assert_eq!(names(&out), ["trap.yaml"]);
}

/// 假引擎：声明「往已有目录里加一张卡」。文件名与是否另带固定文件由测试给；`probes` 数探测被调用了几次。
struct FakeAdd {
    file: &'static str,
    fixed_files: bool,
    probes: std::sync::atomic::AtomicUsize,
}

impl FakeAdd {
    fn new(file: &'static str, fixed_files: bool) -> Self {
        Self {
            file,
            fixed_files,
            probes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn probes(&self) -> usize {
        self.probes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl SetupEngine for FakeAdd {
    fn kind(&self) -> &str {
        "fakeadd"
    }

    fn display_name(&self) -> &'static str {
        "FakeAdd"
    }

    fn install_hint(&self) -> &'static str {
        ""
    }

    fn key_env(&self) -> &'static str {
        ""
    }

    async fn locate_executable(&self) -> Option<PathBuf> {
        None
    }

    async fn discover_executable(&self) -> Result<FoundExecutable, FindFailure> {
        Err(FindFailure::Unsupported)
    }

    fn auto_ports(&self) -> &[u16] {
        &[]
    }

    fn installed_models(&self) -> Option<Vec<String>> {
        None
    }

    fn managed_models_dir(&self) -> Result<PathBuf, SetupError> {
        Err(SetupError::new("validation_failed", "no"))
    }

    async fn probe(&self, _: &str, _: Option<&str>, _: bool) -> Result<Probed, SetupError> {
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Probed {
            models: vec!["m".into()],
            auth_required: false,
        })
    }

    fn card_file(&self) -> &str {
        self.file
    }

    fn card_template(&self) -> Result<ComponentCard, SetupError> {
        let mut card = card_of(&card_yaml("fakeadd"));
        card.version_pin = "fakeadd@1".into();
        card.extensions = None;
        Ok(card)
    }

    fn static_files(&self) -> &'static [(&'static str, &'static str)] {
        if self.fixed_files {
            &[("extra.yaml", "x")]
        } else {
            &[]
        }
    }

    fn out_dir_use(&self) -> OutDirUse {
        OutDirUse::AddCard
    }
}

fn add_options(out: &Path) -> idoris_router::cli::SetupOptions {
    use idoris_router::cli::{SetupOptions, SetupTarget};
    SetupOptions {
        target: SetupTarget::Endpoint {
            url: "http://127.0.0.1:1".into(),
            probe: true,
        },
        out_dir: out.to_path_buf(),
        bindings: vec![("daily".into(), "m".into())],
        load_mode: idoris_contracts::load_policy::LoadMode::Resident,
        api_key_stdin: false,
        force: false,
    }
}

/// 「加一张卡」的引擎不能再带固定文件：它们在这条路径上不会被写出，拒绝而不是悄悄丢掉；排在探测之前。
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_adds_one_card_cannot_also_ship_fixed_files() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    let engine = FakeAdd::new("fakeadd.yaml", true);
    let error = run_setup(&engine, &add_options(&out), None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("固定文件"), "{}", error.message);
    assert_eq!(engine.probes(), 0, "拒绝在探测之前");
    assert!(!out.exists());
    // 对照：没有固定文件的同一个引擎照常加卡。
    let engine = FakeAdd::new("fakeadd.yaml", false);
    run_setup(&engine, &add_options(&out), None).await.unwrap();
    assert_eq!(engine.probes(), 1);
    assert_eq!(names(&out), ["fakeadd.yaml"]);
}

/// 引擎给的卡文件名会跑出目录或不会被加载（带路径、点开头、别的扩展名）：在探测之前拒绝，什么也不写。
#[tokio::test(flavor = "multi_thread")]
async fn a_card_file_name_that_would_escape_or_not_load_is_refused_before_probing() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("components");
    for bad in [
        "../escape.yaml",
        "a/b.yaml",
        ".hidden.yaml",
        "card.txt",
        "card",
        "",
    ] {
        let engine = FakeAdd::new(bad, false);
        let error = run_setup(&engine, &add_options(&out), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, "validation_failed", "{bad:?}");
        assert_eq!(engine.probes(), 0, "{bad:?}：拒绝在探测之前");
    }
    assert!(!out.exists());
    assert!(!tmp.path().join("escape.yaml").exists());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

/// 公开的库入口：选项结构与命令行一致，`run` 与二进制给出同样的结果（不经子进程）。
#[tokio::test(flavor = "multi_thread")]
async fn the_library_entry_point_gives_the_same_report_as_the_binary() {
    let server = upstream(&[MODEL]).await;
    let tmp = TempDir::new().unwrap();
    let options = UpstreamOptions {
        kind: UpstreamKind::OpenAiCompat,
        endpoint: server.uri(),
        out_dir: tmp.path().join("lib"),
        bindings: vec![("daily".into(), MODEL.into())],
        id: Some("lm".into()),
        default: false,
        force: false,
    };
    let from_lib = idoris_router::setup_upstream::run(&options).await.unwrap();
    let from_binary = setup(
        &server.uri(),
        &tmp.path().join("bin"),
        &["--bind", "daily=qwen3-8b", "--id", "lm"],
    )
    .await;
    // 两次写到不同的目录，所以 `components_dir` 与 `next.env` 不同；其余逐字段相同。
    let mut a = from_lib;
    let mut b = ok(&from_binary).clone();
    for report in [&mut a, &mut b] {
        report.as_object_mut().unwrap().remove("components_dir");
        report.as_object_mut().unwrap().remove("next");
    }
    assert_eq!(a, b);
    // 库入口同样要求至少一个 chat 绑定（不靠命令行解析挡）。
    let mut none = options.clone();
    none.bindings.clear();
    none.out_dir = tmp.path().join("none");
    assert_eq!(
        idoris_router::setup_upstream::run(&none)
            .await
            .unwrap_err()
            .code,
        "invalid_role"
    );
    assert!(!tmp.path().join("none").exists());
    // 同一个角色绑两次：库入口也拒绝（命令行在解析期就拒绝，库调用方不经过它），不探测、不写。
    let probes_before = requests(&server).await.len();
    let mut twice = options.clone();
    twice.out_dir = tmp.path().join("twice");
    twice.bindings = vec![
        ("daily".into(), MODEL.into()),
        ("daily".into(), "other".into()),
    ];
    assert_eq!(
        idoris_router::setup_upstream::run(&twice)
            .await
            .unwrap_err()
            .code,
        "invalid_role"
    );
    assert_eq!(requests(&server).await.len(), probes_before);
    assert!(!tmp.path().join("twice").exists());
}

// ---------------------------------------------------------------------------------------------
// 端到端：生成的卡被真的 `idoris serve` 加载，`idoris/daily` 的聊天经假上游返回 200
// ---------------------------------------------------------------------------------------------

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `idoris key issue --spec-stdin`：签一把只放行 `roles` 的数据 key。
fn issue_key(db: &Path, roles: &[&str]) -> String {
    let spec = json!({"owner": "setup-upstream-e2e", "tenant_id": null,
                      "allowed_privacy": ["local_only"], "allowed_roles": roles});
    let mut cmd = idoris();
    cmd.args(["key", "issue", "--spec-stdin"])
        .env("IDORIS_DB_PATH", db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Reaped::spawn(&mut cmd);
    let mut pipe = child.child().stdin.take().unwrap();
    pipe.write_all(spec.to_string().as_bytes()).unwrap();
    drop(pipe);
    let output = child.finish();
    assert!(output.status.success(), "{output:?}");
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    json["secret"].as_str().unwrap().to_string()
}

/// 一个在跑的 `idoris serve`：Drop 时 SIGKILL 加 wait（经 `Reaped`）。
struct Serve {
    process: Reaped,
    base: String,
    log: PathBuf,
    client: reqwest::Client,
}

impl Serve {
    /// 起 serve，等到 `/health` 报 `phase: ready` 且每个上游都是 `ready`；一起返回那时的 `/health`。
    async fn start(tmp: &Path, comp: &Path, db: &Path) -> (Self, Value) {
        let port = free_port();
        // 数据面与 Admin 不能是同一个端口：绑上又放掉的端口号可能被马上再给出一次。
        let admin_port = loop {
            let candidate = free_port();
            if candidate != port {
                break candidate;
            }
        };
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let log = tmp.join("serve.log");
        std::fs::create_dir_all(tmp.join("home")).unwrap();
        let mut cmd = idoris();
        cmd.arg("serve")
            .env("HOME", tmp.join("home"))
            .env("IDORIS_STATE_DIR", tmp.join("state"))
            .env("IDORIS_PORT", port.to_string())
            .env("IDORIS_ADMIN_PORT", admin_port.to_string())
            .env("IDORIS_DB_PATH", db)
            .env("IDORIS_COMPONENTS_DIR", comp)
            .env("IDORIS_ROUTING_POLICY", config.join("routing-policy.yaml"))
            .env("IDORIS_CATALOG", config.join("catalog.yaml"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(std::fs::File::create(&log).unwrap()))
            .stderr(Stdio::from(
                std::fs::OpenOptions::new().append(true).open(&log).unwrap(),
            ));
        let mut serve = Self {
            process: Reaped::spawn(&mut cmd),
            base: format!("http://127.0.0.1:{port}"),
            log,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        };
        let limit = startup_wait::startup_observe_limit();
        let deadline = Instant::now() + limit;
        let health = loop {
            if let Ok(response) = serve
                .client
                .get(format!("{}/health", serve.base))
                .timeout(startup_wait::HTTP_PROBE_TIMEOUT)
                .send()
                .await
                && response.status().is_success()
            {
                let health: Value = response.json().await.unwrap();
                let all_ready = health["upstreams"]
                    .as_array()
                    .is_some_and(|all| all.iter().all(|u| u["state"] == "ready"));
                if health["phase"] == "ready" && all_ready {
                    break health;
                }
            }
            assert!(
                serve.process.child().try_wait().unwrap().is_none(),
                "serve 提前退出：{}",
                serve.logs()
            );
            assert!(
                Instant::now() < deadline,
                "{}s 内没有就绪：{}",
                limit.as_secs(),
                serve.logs()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        (serve, health)
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap()
    }

    /// `POST /v1/chat/completions`，`model` 是 `idoris/<role>` 或具体模型名。
    async fn chat(&self, key: &str, model: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/chat/completions", self.base))
            .bearer_auth(key)
            .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}))
            .timeout(startup_wait::startup_observe_limit())
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_loads_the_generated_card_and_chat_through_idoris_daily_reaches_the_fake_upstream() {
    let server = upstream(&[MODEL, "other"]).await;
    let tmp = TempDir::new().unwrap();
    let comp = tmp.path().join("components");
    ok(&setup(
        &server.uri(),
        &comp,
        &[
            "--bind",
            "daily=qwen3-8b",
            "--bind",
            "deep=other",
            "--id",
            "lmstudio",
        ],
    )
    .await);
    // 生成之后上游只被探测过一次；之后的请求都来自 serve。
    assert_eq!(requests(&server).await.len(), 1);

    let db = tmp.path().join("idoris.sqlite3");
    // Agent24 的数据 key 只放行角色 `daily`、`deep`、`embed`；另签一把全角色的，给不带角色的具体模型名用。
    let key = issue_key(&db, &["daily", "deep", "embed"]);
    let full_key = issue_key(
        &db,
        &[
            "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
        ],
    );
    let (serve, health) = Serve::start(tmp.path(), &comp, &db).await;
    // 引擎种类取自 `version_pin`：`openai_compat`（规范 §2.3）。
    assert_eq!(health["components"], 1);
    assert_eq!(
        health["upstreams"],
        json!([{"id": "lmstudio", "engine_kind": "openai_compat", "state": "ready",
                "managed": false, "owned": false}])
    );

    // `idoris/daily` 解析到绑定的模型：上游收到的是具体模型 id。
    let response = serve.chat(&key, "idoris/daily").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "pong");
    assert_eq!(body["model"], MODEL);
    // 另一个角色绑的是另一个模型。
    let response = serve.chat(&key, "idoris/deep").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["model"], "other");
    // 具体模型名（没有角色）原样转发，但按既有的 key 范围规则要全角色的 key：只放行三个角色的 key 得 403。
    let response = serve.chat(&key, MODEL).await;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["reason_code"],
        "VIRTUAL_KEY_ROLE_FORBIDDEN"
    );
    let response = serve.chat(&full_key, MODEL).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let posts: Vec<_> = requests(&server)
        .await
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 3, "{posts:?}");
    for post in &posts {
        assert_eq!(post.url.path(), "/v1/chat/completions");
    }
    let sent: Vec<Value> = posts
        .iter()
        .map(|p| serde_json::from_slice(&p.body).unwrap())
        .collect();
    assert_eq!(
        sent.iter()
            .map(|b| b["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [MODEL, "other", MODEL]
    );
    // 数据 key（idk_…）不会转给上游；这张卡没有凭据，上游收到的 POST 也没有。
    assert_anonymous(&posts);

    // 只 chat：这张卡没有 embedding 能力，`idoris/embed` 的请求到不了上游。
    let response = serve
        .client
        .post(format!("{}/v1/embeddings", serve.base))
        .bearer_auth(&key)
        .json(&json!({"model": "idoris/embed", "input": ["hello"]}))
        .timeout(startup_wait::startup_observe_limit())
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), reqwest::StatusCode::OK);
    assert!(
        requests(&server)
            .await
            .iter()
            .all(|r| r.url.path() != "/v1/embeddings"),
        "embeddings 请求不应到达只有 chat 的上游"
    );
    // serve 的日志里没有 key。
    assert!(!serve.logs().contains(&key));
    // Drop 杀掉并回收；放在最后，断言失败也不留进程。
    drop(serve);
}

/// Agent24 在 `lmstudio_embed_probe.py` 里手写的 chat 加 embedding 卡（能力 `[chat, embedding]`，`embed` 绑定）。最小版
/// `setup upstream` 只生成 chat 卡，embedding 要靠这种手写卡；卡契约没变，所以它在这个二进制上照常加载、照常服务。
const AGENT24_EMBED_CARD: &str = r#"provider:
  id: lmstudio
  family: local
  tier: local
  capabilities: [chat, embedding]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:{port}"
version_pin: "lmstudio@0"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
extensions:
  _degradation: "Removing these bindings disables role aliases on this backend."
  idoris_role_models:
    daily: qwen3-8b
    deep: qwen3-8b
    embed: text-embedding-bge-m3
"#;

/// 给 Agent24 的用法说明里写了「embedding 这个命令生成不了，手写的 `[chat, embedding]` 卡在 v0.6.0 上仍然能用」：
/// 这条用真二进制验证后一半，并记下与生成的卡的差别：手写卡的 `version_pin` 是 `lmstudio@0`，所以 `engine_kind` 是
/// `lmstudio`，不是 `openai_compat`。
#[tokio::test(flavor = "multi_thread")]
async fn a_hand_written_chat_and_embedding_card_still_serves_both_and_reports_its_own_engine_kind()
{
    let server = upstream(&[MODEL, "text-embedding-bge-m3"]).await;
    let tmp = TempDir::new().unwrap();
    let comp = tmp.path().join("components");
    std::fs::create_dir_all(&comp).unwrap();
    std::fs::write(
        comp.join("lmstudio.yaml"),
        AGENT24_EMBED_CARD.replace("{port}", &server.address().port().to_string()),
    )
    .unwrap();
    let db = tmp.path().join("idoris.sqlite3");
    let key = issue_key(&db, &["daily", "deep", "embed"]);
    let (serve, health) = Serve::start(tmp.path(), &comp, &db).await;
    assert_eq!(
        health["upstreams"],
        json!([{"id": "lmstudio", "engine_kind": "lmstudio", "state": "ready",
                "managed": false, "owned": false}])
    );
    let response = serve.chat(&key, "idoris/daily").await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = serve
        .client
        .post(format!("{}/v1/embeddings", serve.base))
        .bearer_auth(&key)
        .json(&json!({"model": "idoris/embed", "input": ["hello", "world"]}))
        .timeout(startup_wait::startup_observe_limit())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["model"], "text-embedding-bge-m3");
    let embeddings: Vec<_> = requests(&server)
        .await
        .into_iter()
        .filter(|r| r.url.path() == "/v1/embeddings")
        .collect();
    assert_eq!(embeddings.len(), 1);
    assert_anonymous(&embeddings);
    drop(serve);
}

/// 两张卡加进同一个目录（也是「加一张卡」不动别的卡的端到端证据）：`aaa` 不当默认，`zzz` 当默认，id 字典序靠后。
/// 其余条件相同时，选择按 id 字典序取小的（`idoris-policy` 的 `pick`），v0.6.0 不读 `idoris_default`：请求仍然落在
/// `aaa`。这条钉住规范 §2.7.8 回填的「路由不读这个键」；默认引擎的选择语义实现之后（§2.12.3），它要随之改。
#[tokio::test(flavor = "multi_thread")]
async fn idoris_default_is_only_an_annotation_in_v0_6_0_and_routing_ignores_it() {
    let aaa = upstream_replying(&[MODEL], "from-aaa").await;
    let zzz = upstream_replying(&[MODEL], "from-zzz").await;
    let tmp = TempDir::new().unwrap();
    let comp = tmp.path().join("components");
    ok(&setup(
        &aaa.uri(),
        &comp,
        &["--bind", "daily=qwen3-8b", "--id", "aaa"],
    )
    .await);
    let result = setup(
        &zzz.uri(),
        &comp,
        &["--bind", "daily=qwen3-8b", "--id", "zzz", "--default"],
    )
    .await;
    assert_eq!(ok(&result)["default"], true);
    assert_eq!(names(&comp), ["aaa.yaml", "zzz.yaml"]);

    let db = tmp.path().join("idoris.sqlite3");
    let key = issue_key(&db, &["daily"]);
    let (serve, health) = Serve::start(tmp.path(), &comp, &db).await;
    assert_eq!(health["components"], 2);
    let mut upstreams: Vec<(String, String, String)> = health["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["id"].as_str().unwrap().to_string(),
                u["engine_kind"].as_str().unwrap().to_string(),
                u["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    upstreams.sort();
    let expected = |id: &str| {
        (
            id.to_string(),
            "openai_compat".to_string(),
            "ready".to_string(),
        )
    };
    assert_eq!(upstreams, [expected("aaa"), expected("zzz")]);

    for _ in 0..3 {
        let response = serve.chat(&key, "idoris/daily").await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "from-aaa");
    }
    let chats = |requests: Vec<wiremock::Request>| {
        requests
            .into_iter()
            .filter(|r| r.method == wiremock::http::Method::POST)
            .count()
    };
    assert_eq!(chats(requests(&aaa).await), 3);
    assert_eq!(
        chats(requests(&zzz).await),
        0,
        "默认卡没有因为它是默认而被优先"
    );
    drop(serve);
}
