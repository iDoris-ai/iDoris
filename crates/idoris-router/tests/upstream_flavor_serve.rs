//! #626 项 1：serve 校验 `extensions.idoris_upstream.flavor`（接口规范 §2.3）。
//!
//! 带 `idoris_upstream` 的卡，`flavor` 必须存在、格式合法，并与 `version_pin` 的产品名一致；不一致就是卡校验失败，serve
//! 退出码非 0，说明里带卡文件名与原因码。不带 `idoris_upstream` 的卡行为不变。真二进制的用例用 `spawn_guarded()`：断言失败
//! 或 panic 时守卫的 `Drop` 杀掉并收走子进程。上游是 wiremock 假服务，不启动任何推理进程。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/startup_wait.rs"]
mod startup_wait;

#[path = "support/child_guard.rs"]
mod child_guard;
use child_guard::SpawnGuarded;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use idoris_router::components::load_components;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 一张 loopback resident 的 chat 卡；`pin` 是 `version_pin`，`extensions` 是 YAML 片段（含缩进的 `extensions:` 段）。
fn card(id: &str, endpoint: &str, pin: &str, extensions: &str) -> String {
    format!(
        r#"
provider:
  id: {id}
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "{pin}"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
{extensions}
"#
    )
}

fn upstream_ext(declaration: &str) -> String {
    format!("extensions:\n  _degradation: d\n  idoris_upstream: {declaration}\n")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn serve_command(tmp: &Path, comp: &Path, port: u16, admin_port: u16) -> Command {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| startup_wait::warm_exec(Path::new(env!("CARGO_BIN_EXE_idoris"))));
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    std::fs::create_dir_all(tmp.join("home")).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
    {
        cmd.env_remove(key);
    }
    cmd.arg("serve")
        .env("HOME", tmp.join("home"))
        .env("IDORIS_STATE_DIR", tmp.join("state"))
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env("IDORIS_DB_PATH", tmp.join("idoris.sqlite3"))
        .env("IDORIS_COMPONENTS_DIR", comp)
        .env("IDORIS_ROUTING_POLICY", config.join("routing-policy.yaml"))
        .env("IDORIS_CATALOG", config.join("catalog.yaml"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// 起 serve 加载 `comp`，期望它因为卡校验失败而退出；返回（退出码，stdout 加 stderr）。
fn serve_expecting_failure(comp: &Path) -> (Option<i32>, String) {
    serve_expecting_failure_with(comp, &[])
}

fn serve_expecting_failure_with(comp: &Path, extra_env: &[(&str, &str)]) -> (Option<i32>, String) {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let admin = loop {
        let candidate = free_port();
        if candidate != port {
            break candidate;
        }
    };
    let mut command = serve_command(tmp.path(), comp, port, admin);
    command.envs(extra_env.iter().copied());
    let mut child = command.spawn_guarded().unwrap();
    let limit = startup_wait::startup_observe_limit();
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}s 内 serve 没有退出：坏卡没有被拒绝",
            limit.as_secs()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), text)
}

async fn fake_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"object": "list", "data": [{"id": "m1", "object": "model"}]}),
            ),
        )
        .mount(&server)
        .await;
    server
}

/// 坏卡：每个都带 `idoris_upstream`，但 `flavor` 与 `version_pin` 不一致、缺失或非法。serve 都不起来，说明里有卡文件名与原因码。
#[test]
fn serve_refuses_an_upstream_card_whose_flavor_is_wrong() {
    let cases: [(&str, &str, &str, &str); 7] = [
        // (标签, version_pin, idoris_upstream 的写法, 期望的原因码)
        (
            "flavor 与 version_pin 产品名不一致",
            "lmstudio@0",
            "{ kind: openai_compat, flavor: vllm }",
            "upstream_flavor_mismatch",
        ),
        (
            "oMLX 的 pin 配别的 flavor",
            "omlx@0.6.4",
            "{ kind: openai_compat, flavor: lmstudio }",
            "upstream_flavor_mismatch",
        ),
        (
            "version_pin 没有产品名",
            "lmstudio",
            "{ kind: openai_compat, flavor: lmstudio }",
            "upstream_flavor_mismatch",
        ),
        (
            "flavor 缺失",
            "lmstudio@0",
            "{ kind: openai_compat }",
            "upstream_flavor_invalid",
        ),
        (
            "flavor 格式非法（大写与连字符）",
            "lmstudio@0",
            "{ kind: openai_compat, flavor: LM-Studio }",
            "upstream_flavor_invalid",
        ),
        (
            "flavor 不是字符串",
            "lmstudio@0",
            "{ kind: openai_compat, flavor: 7 }",
            "upstream_flavor_invalid",
        ),
        (
            "idoris_upstream 不是对象",
            "lmstudio@0",
            "lmstudio",
            "upstream_flavor_invalid",
        ),
    ];
    for (label, pin, declaration, code) in cases {
        let comp = TempDir::new().unwrap();
        std::fs::write(
            comp.path().join("bad-card.yaml"),
            card("bad", "http://127.0.0.1:1", pin, &upstream_ext(declaration)),
        )
        .unwrap();
        let (status, text) = serve_expecting_failure(comp.path());
        assert!(
            matches!(status, Some(code) if code != 0),
            "{label}: 退出码 {status:?}\n{text}"
        );
        assert!(text.contains(code), "{label}: 缺原因码 {code}\n{text}");
        assert!(
            text.contains("bad-card.yaml"),
            "{label}: 缺卡文件名\n{text}"
        );
        assert!(text.contains("flavor"), "{label}\n{text}");
    }
}

/// 对照一：flavor 与 version_pin 一致的上游卡，serve 照常起来并报告该上游的引擎种类。
#[tokio::test(flavor = "multi_thread")]
async fn serve_starts_with_a_consistent_upstream_card() {
    let server = fake_upstream().await;
    let comp = TempDir::new().unwrap();
    std::fs::write(
        comp.path().join("lmstudio.yaml"),
        card(
            "lmstudio",
            &server.uri(),
            "lmstudio@0",
            &upstream_ext("{ kind: openai_compat, flavor: lmstudio }"),
        ),
    )
    .unwrap();
    // 库级：同一个加载器放行。
    assert_eq!(load_components(comp.path(), false).unwrap().len(), 1);
    let health = start_and_read_health(comp.path()).await;
    let upstream = health["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "lmstudio")
        .unwrap_or_else(|| panic!("{health}"));
    assert_eq!(upstream["engine_kind"], "lmstudio");
}

/// Codex #2：`form: bundled_binary` 的普通卡：serve 在构造运行时时确定地拒绝（与 setup 的预检同一句话）。
#[test]
fn serve_refuses_a_card_form_the_runtime_cannot_construct() {
    let comp = TempDir::new().unwrap();
    std::fs::write(
        comp.path().join("odd.yaml"),
        card("odd", "http://127.0.0.1:1", "openai_compat@unknown", "")
            .replace("form: http_service", "form: bundled_binary"),
    )
    .unwrap();
    let (status, text) = serve_expecting_failure(comp.path());
    assert!(
        matches!(status, Some(code) if code != 0),
        "{status:?}\n{text}"
    );
    assert!(text.contains("no runtime constructor"), "{text}");
}

/// Codex #8：`IDORIS_COMPONENTS_LOCK_WAIT_MS` 不是毫秒数：启动错误，说明里点名变量与取值，不悄悄退回默认。
#[test]
fn serve_rejects_a_non_numeric_lock_wait_setting() {
    let comp = TempDir::new().unwrap();
    std::fs::write(
        comp.path().join("plain.yaml"),
        card("plain", "http://127.0.0.1:1", "somethingelse@9", ""),
    )
    .unwrap();
    let (status, text) =
        serve_expecting_failure_with(comp.path(), &[("IDORIS_COMPONENTS_LOCK_WAIT_MS", "soon")]);
    assert!(
        matches!(status, Some(code) if code != 0),
        "{status:?}\n{text}"
    );
    assert!(
        text.contains("IDORIS_COMPONENTS_LOCK_WAIT_MS") && text.contains("soon"),
        "{text}"
    );
}

/// #M2：`idoris setup` 持有组件目录的独占锁（落盘中）时 serve 启动：serve 等到上限，启动失败（退出码非 0），说明点出是
/// setup 在改写这个目录；不会读到改了一半的卡。锁一放开，同样的 serve 正常起来。
#[tokio::test(flavor = "multi_thread")]
async fn serve_waits_for_setup_then_fails_clearly_and_starts_once_the_lock_is_released() {
    let server = fake_upstream().await;
    let comp = TempDir::new().unwrap();
    std::fs::write(
        comp.path().join("plain.yaml"),
        card("plain", &server.uri(), "somethingelse@9", ""),
    )
    .unwrap();
    let held = idoris_router::setup_engine::lock_out_dir(comp.path()).unwrap();
    let (status, text) =
        serve_expecting_failure_with(comp.path(), &[("IDORIS_COMPONENTS_LOCK_WAIT_MS", "300")]);
    assert!(
        matches!(status, Some(code) if code != 0),
        "{status:?}\n{text}"
    );
    assert!(text.contains("setup"), "{text}");
    assert!(text.contains(&comp.path().display().to_string()), "{text}");
    drop(held);
    let health = start_and_read_health(comp.path()).await;
    assert!(
        health["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == "plain"),
        "{health}"
    );
}

/// #L1：组件目录的路径末尾带空格（`setup` 输出的 `next.env.IDORIS_COMPONENTS_DIR` 原样交给 serve）：serve 不能把环境变量
/// 的值整个 `trim` 掉而去找另一个目录；它只用 `trim` 判断是不是整个都是空白。
#[tokio::test(flavor = "multi_thread")]
async fn serve_uses_a_components_dir_path_with_a_trailing_space_as_is() {
    let server = fake_upstream().await;
    let parent = TempDir::new().unwrap();
    let comp = parent.path().join("components ");
    std::fs::create_dir(&comp).unwrap();
    std::fs::write(
        comp.join("plain.yaml"),
        card("plain", &server.uri(), "somethingelse@9", ""),
    )
    .unwrap();
    // 去掉空格之后的目录不存在：只有原样使用带空格的路径才找得到卡。
    assert!(!parent.path().join("components").exists());
    let health = start_and_read_health(&comp).await;
    assert!(
        health["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == "plain"),
        "{health}"
    );
}

/// 对照二：不带 `idoris_upstream` 的卡不受这条校验影响，`version_pin` 写成什么产品名都照旧。
#[tokio::test(flavor = "multi_thread")]
async fn a_card_without_the_upstream_declaration_is_unaffected() {
    let server = fake_upstream().await;
    let comp = TempDir::new().unwrap();
    std::fs::write(
        comp.path().join("plain.yaml"),
        card("plain", &server.uri(), "somethingelse@9", ""),
    )
    .unwrap();
    assert_eq!(load_components(comp.path(), false).unwrap().len(), 1);
    let health = start_and_read_health(comp.path()).await;
    let upstream = health["upstreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "plain")
        .unwrap_or_else(|| panic!("{health}"));
    assert_eq!(upstream["engine_kind"], "somethingelse");
}

async fn start_and_read_health(comp: &Path) -> Value {
    let tmp = TempDir::new().unwrap();
    let port = free_port();
    let admin = loop {
        let candidate = free_port();
        if candidate != port {
            break candidate;
        }
    };
    let mut child = serve_command(tmp.path(), comp, port, admin)
        .spawn_guarded()
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let limit = startup_wait::startup_observe_limit();
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(startup_wait::HTTP_PROBE_TIMEOUT)
            .send()
            .await
            && response.status().is_success()
        {
            let health: Value = response.json().await.unwrap();
            let ready = health["upstreams"]
                .as_array()
                .is_some_and(|all| !all.is_empty() && all.iter().all(|u| u["state"] == "ready"));
            if health["phase"] == "ready" && ready {
                return health;
            }
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "serve 提前退出（好卡不该被拒绝）"
        );
        assert!(Instant::now() < deadline, "{}s 内没有就绪", limit.as_secs());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
