//! Subprocess CLI coverage for `IDORIS_ALLOCATED_BUDGET_BYTES`.
//!
//! Uses the real `idoris` bin and the same `serve` entry points as `tests/cli.rs`,
//! plus a controllable loopback oMLX fixture (wiremock). Linux CI target; macOS /
//! Windows spawn+HOME isolation is unverified here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex},
};

const MODEL_ID: &str = "budget-fixture";
/// 1 GiB — oMLX `estimated_size` is converted with binary GiB in the adapter.
const MODEL_ESTIMATED_SIZE: u64 = 1024 * 1024 * 1024;
/// Explicit byte budget that admits MODEL_ESTIMATED_SIZE (decimal GB ledger).
const SUFFICIENT_BUDGET_BYTES: u64 = 2_000_000_000;
/// Positive but insufficient for a 1 GiB model under exact-byte admission.
const INSUFFICIENT_BUDGET_BYTES: u64 = 500_000_000;

static HOME_SEQ: AtomicU64 = AtomicU64::new(0);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn clean_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
    {
        command.env_remove(key);
    }
    command
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct IsolatedHome {
    _dir: TempDir,
    path: PathBuf,
}

impl IsolatedHome {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("home-{}", HOME_SEQ.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&path).unwrap();
        Self { _dir: dir, path }
    }
}

struct ServeChild {
    child: Child,
    data_port: u16,
    _home: IsolatedHome,
    _components: TempDir,
    _state: TempDir,
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_omlx_component(dir: &Path, endpoint: &str) {
    let yaml = format!(
        r#"
provider:
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
extensions:
  _degradation: "Removing these bindings disables aliases."
  idoris_role_models: {{ daily: {MODEL_ID}, fast: {MODEL_ID} }}
"#
    );
    fs::write(dir.join("omlx.yaml"), yaml).unwrap();
}

async fn mount_omlx_fixture(upstream: &MockServer, estimated_size: u64) {
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": [],
            "model_memory_used": 0,
            "model_memory_max": 16,
            "pressure": "ok"
        })))
        .mount(upstream)
        .await;
    // Always report loaded=true so on-demand post-load pin checks succeed,
    // matching tests/model_identity.rs::mount_omlx_success. estimated_size
    // still supplies the real pre-load footprint for budget admission.
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{
                "id": MODEL_ID,
                "loaded": true,
                "pinned": false,
                "estimated_size": estimated_size
            }]
        })))
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/models/.+/load$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-budget-fixture",
            "choices": [{"message": {"role": "assistant", "content": "budget-ok"}}]
        })))
        .mount(upstream)
        .await;
}

fn count_posts(requests: &[wiremock::Request]) -> (usize, usize) {
    let load = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST && r.url.path().ends_with("/load"))
        .count();
    let chat = requests
        .iter()
        .filter(|r| {
            r.method == wiremock::http::Method::POST && r.url.path() == "/v1/chat/completions"
        })
        .count();
    (load, chat)
}

fn count_gets(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::GET)
        .count()
}

async fn spawn_serve(upstream: &MockServer, budget: Option<&str>) -> ServeChild {
    let home = IsolatedHome::new();
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_omlx_component(components.path(), &upstream.uri());
    let repo = repo_root();
    let data_port = free_port();
    let mut command = clean_command();
    command
        .arg("serve")
        .current_dir(&repo)
        .env("HOME", &home.path)
        .env("IDORIS_COMPONENTS_DIR", components.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            repo.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_CATALOG", repo.join("config/catalog.yaml"))
        .env("IDORIS_PORT", data_port.to_string())
        .env("IDORIS_ADMIN_PORT", free_port().to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .env("IDORIS_DEV_NO_KEY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match budget {
        Some(value) => {
            command.env("IDORIS_ALLOCATED_BUDGET_BYTES", value);
        }
        None => {
            command.env_remove("IDORIS_ALLOCATED_BUDGET_BYTES");
        }
    }
    let mut child = command.spawn().unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "idoris exited before health: status={:?}\nstdout={}\nstderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{data_port}/health"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "health readiness timed out for budget={budget:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ServeChild {
        child,
        data_port,
        _home: home,
        _components: components,
        _state: state,
    }
}

async fn post_chat(port: u16) -> reqwest::Response {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("X-iDoris-Privacy", "local_only")
        .timeout(Duration::from_secs(10))
        .body(format!(
            r#"{{"model":"idoris/daily","stream":false,"messages":[{{"role":"user","content":"hi"}}]}}"#
        ))
        .send()
        .await
        .unwrap()
}

fn startup_with_budget(raw: &str) -> std::process::Output {
    let home = IsolatedHome::new();
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = repo_root();
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    clean_command()
        .arg("serve")
        .current_dir(&repo)
        .env("HOME", &home.path)
        .env("IDORIS_COMPONENTS_DIR", components.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            repo.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_CATALOG", repo.join("config/catalog.yaml"))
        .env("IDORIS_PORT", free_port().to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .env("IDORIS_ALLOCATED_BUDGET_BYTES", raw)
        .output()
        .unwrap()
}

#[test]
fn illegal_and_negative_budget_configs_fail_startup() {
    for raw in [
        "",
        "-1",
        "1.5",
        "GB",
        "18446744073709551616",
        " -1 ",
        "0x10",
    ] {
        let output = startup_with_budget(raw);
        assert_eq!(
            output.status.code(),
            Some(1),
            "raw={raw:?} should fail startup"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("IDORIS_ALLOCATED_BUDGET_BYTES"),
            "raw={raw:?} stderr={stderr}"
        );
    }
}

#[tokio::test]
async fn zero_budget_is_explicit_zero_and_never_posts_load_or_chat() {
    let upstream = MockServer::start().await;
    mount_omlx_fixture(&upstream, MODEL_ESTIMATED_SIZE).await;
    let mut serve = spawn_serve(&upstream, Some("0")).await;
    let response = post_chat(serve.data_port).await;
    assert!(
        !response.status().is_success(),
        "zero budget must deny chat: {}",
        response.status()
    );
    let requests = upstream.received_requests().await.unwrap();
    let (load, chat) = count_posts(&requests);
    assert_eq!(
        load, 0,
        "zero budget must never POST model load; got {requests:?}"
    );
    assert_eq!(
        chat, 0,
        "zero budget must never POST chat; got {requests:?}"
    );
    // GET metadata may still occur for preflight footprint discovery.
    let _ = count_gets(&requests);
    let _ = serve.child.kill();
    let _ = serve.child.wait();
}

#[tokio::test]
async fn insufficient_budget_never_posts_load_or_chat_while_get_may_occur() {
    let upstream = MockServer::start().await;
    mount_omlx_fixture(&upstream, MODEL_ESTIMATED_SIZE).await;
    let mut serve = spawn_serve(&upstream, Some(&INSUFFICIENT_BUDGET_BYTES.to_string())).await;
    let response = post_chat(serve.data_port).await;
    assert!(
        !response.status().is_success(),
        "insufficient budget must deny chat: {}",
        response.status()
    );
    let requests = upstream.received_requests().await.unwrap();
    let (load, chat) = count_posts(&requests);
    assert_eq!(
        load, 0,
        "insufficient budget must never POST load; {requests:?}"
    );
    assert_eq!(
        chat, 0,
        "insufficient budget must never POST chat; {requests:?}"
    );
    assert!(
        count_gets(&requests) >= 1,
        "preflight should still be able to GET model metadata; {requests:?}"
    );
    let _ = serve.child.kill();
    let _ = serve.child.wait();
}

#[tokio::test]
async fn sufficient_budget_reaches_real_load_and_chat_posts() {
    let upstream = MockServer::start().await;
    mount_omlx_fixture(&upstream, MODEL_ESTIMATED_SIZE).await;
    let mut serve = spawn_serve(&upstream, Some(&SUFFICIENT_BUDGET_BYTES.to_string())).await;
    let response = post_chat(serve.data_port).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(
        status.is_success(),
        "sufficient budget must reach chat success: {status} body={body}"
    );
    assert!(
        body.contains("budget-ok"),
        "chat body must come from loopback fixture: {body}"
    );
    let requests = upstream.received_requests().await.unwrap();
    let (load, chat) = count_posts(&requests);
    assert!(
        load >= 1,
        "sufficient budget must POST model load; requests={requests:?}"
    );
    assert!(
        chat >= 1,
        "sufficient budget must POST chat; requests={requests:?}"
    );
    let _ = serve.child.kill();
    let _ = serve.child.wait();
}

#[tokio::test]
async fn absent_budget_env_keeps_compatible_default_load_semantics() {
    let upstream = MockServer::start().await;
    mount_omlx_fixture(&upstream, MODEL_ESTIMATED_SIZE).await;
    let mut serve = spawn_serve(&upstream, None).await;
    let response = post_chat(serve.data_port).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(
        status.is_success(),
        "None budget must preserve default admit path: {status} body={body}"
    );
    let requests = upstream.received_requests().await.unwrap();
    let (load, chat) = count_posts(&requests);
    assert!(load >= 1, "None budget must still load: {requests:?}");
    assert!(chat >= 1, "None budget must still chat: {requests:?}");
    let _ = serve.child.kill();
    let _ = serve.child.wait();
}

#[tokio::test]
async fn zero_differs_from_absent_against_same_fixture_metadata() {
    // Same loopback metadata: `0` denies, unset admits — proves zero is not None.
    let upstream_zero = MockServer::start().await;
    mount_omlx_fixture(&upstream_zero, MODEL_ESTIMATED_SIZE).await;
    let mut zero = spawn_serve(&upstream_zero, Some("0")).await;
    let zero_response = post_chat(zero.data_port).await;
    assert!(!zero_response.status().is_success());
    let zero_requests = upstream_zero.received_requests().await.unwrap();
    let (zero_load, zero_chat) = count_posts(&zero_requests);
    assert_eq!((zero_load, zero_chat), (0, 0));
    let _ = zero.child.kill();
    let _ = zero.child.wait();

    let upstream_none = MockServer::start().await;
    mount_omlx_fixture(&upstream_none, MODEL_ESTIMATED_SIZE).await;
    let mut none = spawn_serve(&upstream_none, None).await;
    let none_response = post_chat(none.data_port).await;
    assert!(none_response.status().is_success());
    let none_requests = upstream_none.received_requests().await.unwrap();
    let (none_load, none_chat) = count_posts(&none_requests);
    assert!(none_load >= 1 && none_chat >= 1);
    let _ = none.child.kill();
    let _ = none.child.wait();
}
