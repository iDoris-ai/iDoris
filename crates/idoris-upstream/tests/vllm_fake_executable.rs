//! Adapter lifecycle for `vllm serve` using a fake executable.
//!
//! This is not Linux CUDA acceptance and it does not download a model.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use idoris_backend::{BackendError, ChatMessage, ChatRequest, RuntimeAdapter};
use idoris_upstream::{
    LocalHttpRuntimeAdapter, LocalHttpRuntimeConfig, LocalRuntimeLaunch, UpstreamBearer, VllmLaunch,
};
use tokio_util::sync::CancellationToken;

const SERVED: &str = "served-id";

fn gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn revision() -> String {
    "a".repeat(40)
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_fake(dir: &Path, behavior: &str) {
    let script = r#"#!/usr/bin/env python3
import json, os, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

here = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
with open(os.path.join(here, "argv.json"), "w", encoding="utf-8") as handle:
    json.dump(args, handle, separators=(",", ":"))
with open(os.path.join(here, "pid.txt"), "w", encoding="utf-8") as handle:
    handle.write(str(os.getpid()))

behavior = {}
behavior_path = os.path.join(here, "behavior.json")
if os.path.exists(behavior_path):
    with open(behavior_path, encoding="utf-8") as handle:
        behavior = json.load(handle)
if behavior.get("exit_immediately"):
    raise SystemExit(0)

def flag(name):
    if name not in args:
        return None
    return args[args.index(name) + 1]

host = flag("--host")
port = int(flag("--port"))
served = flag("--served-model-name")
api_key = flag("--api-key")
expected = behavior.get("expect_bearer", api_key)
model_id = behavior.get("model_id", served)

class Handler(BaseHTTPRequestHandler):
    def _auth_ok(self):
        if expected is None:
            return True
        return self.headers.get("Authorization", "") == f"Bearer {expected}"

    def _reject(self, status):
        self.send_response(status)
        self.end_headers()

    def do_GET(self):
        if self.path != "/v1/models":
            self._reject(404)
            return
        if not self._auth_ok():
            self._reject(401)
            return
        body = json.dumps({"data": [{"id": model_id}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        with open(os.path.join(here, "chat.json"), "wb") as handle:
            handle.write(raw)
        if self.path != "/v1/chat/completions" or not self._auth_ok():
            self._reject(401 if not self._auth_ok() else 404)
            return
        body = json.dumps({"choices": [{"message": {"content": "pong"}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        return

ThreadingHTTPServer.allow_reuse_address = False
ThreadingHTTPServer((host, port), Handler).serve_forever()
"#;
    let path = dir.join("vllm");
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    fs::write(dir.join("behavior.json"), behavior).unwrap();
}

fn adapter(dir: &Path, port: u16, secret: Option<&str>, repo: bool) -> LocalHttpRuntimeAdapter {
    let model = if repo {
        "Qwen/Qwen2.5-7B".to_string()
    } else {
        "/models/qwen".to_string()
    };
    let launch = LocalRuntimeLaunch::vllm(VllmLaunch {
        executable: dir.join("vllm"),
        model,
        port,
        served_model_name: SERVED.to_string(),
        engine_version: "0.11.0".to_string(),
        gpu_total_gib: 24.0,
        reserve_gib: 18.0,
        model_revision: repo.then(revision),
    })
    .unwrap();
    let mut config = LocalHttpRuntimeConfig::new(
        launch,
        SERVED,
        18.0,
        std::env::temp_dir().join(format!("idoris-vllm-fake-{port}.pending")),
    )
    .unwrap();
    if let Some(secret) = secret {
        config.bearer = Some(UpstreamBearer::from_secret(secret).unwrap());
    }
    LocalHttpRuntimeAdapter::new(config).unwrap()
}

fn port_accepts(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn pid_of(dir: &Path) -> u32 {
    fs::read_to_string(dir.join("pid.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

async fn wait_until(dir: &Path, port: u16) -> bool {
    for _ in 0..100 {
        if dir.join("argv.json").exists() && port_accepts(port) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

async fn wait_ready(adapter: &LocalHttpRuntimeAdapter) -> bool {
    for _ in 0..100 {
        match adapter.probe_ready(SERVED).await {
            Ok(true) => return true,
            Ok(false) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(error) => panic!("readiness failed before the server answered: {error}"),
        }
    }
    false
}

fn recorded_args(dir: &Path) -> String {
    fs::read_to_string(dir.join("argv.json")).unwrap()
}

fn chat_request() -> ChatRequest {
    ChatRequest {
        model: SERVED.to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: "ping".to_string(),
        }],
    }
}

#[tokio::test]
async fn fake_vllm_starts_serves_the_named_model_and_releases_the_port() {
    let _guard = gate().lock().await;
    assert!(PathBuf::from("/usr/bin/python3").is_file());
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_fake(dir.path(), "{}");
    let adapter = adapter(dir.path(), port, None, true);
    adapter.load(SERVED, None).await.unwrap();
    assert!(wait_ready(&adapter).await);
    assert!(port_accepts(port));
    assert_eq!(
        recorded_args(dir.path()),
        format!(
            r#"["serve","Qwen/Qwen2.5-7B","--host","127.0.0.1","--port","{port}","--served-model-name","{SERVED}","--revision","{}","--gpu-memory-utilization","0.75"]"#,
            revision()
        )
    );
    let response = adapter
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.content, "pong");
    let chat = fs::read_to_string(dir.path().join("chat.json")).unwrap();
    assert!(chat.contains(r#""model":"served-id""#), "{chat}");
    let pid = pid_of(dir.path());
    assert!(pid_alive(pid));
    adapter.unload(SERVED).await.unwrap();
    assert!(!pid_alive(pid));
    assert!(!port_accepts(port));
    assert!(adapter.status().await.unwrap().loaded.is_empty());
}

#[tokio::test]
async fn fake_vllm_http_200_without_the_served_model_is_not_ready() {
    let _guard = gate().lock().await;
    assert!(PathBuf::from("/usr/bin/python3").is_file());
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_fake(dir.path(), r#"{"model_id":"other-model"}"#);
    let adapter = adapter(dir.path(), port, None, false);
    adapter.load(SERVED, None).await.unwrap();
    assert!(wait_until(dir.path(), port).await);
    assert!(!adapter.probe_ready(SERVED).await.unwrap());
    let error = adapter
        .chat(
            ChatRequest {
                model: SERVED.to_string(),
                messages: vec![],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, BackendError::ModelUnavailable { .. }));
    assert!(!dir.path().join("chat.json").exists());
    adapter.unload(SERVED).await.unwrap();
    assert!(!port_accepts(port));
}

#[tokio::test]
async fn fake_vllm_bearer_is_the_api_key_and_a_mismatch_does_not_chat() {
    let _guard = gate().lock().await;
    assert!(PathBuf::from("/usr/bin/python3").is_file());
    let matched_dir = tempfile::tempdir().unwrap();
    let matched_port = free_port();
    write_fake(matched_dir.path(), "{}");
    let matched = adapter(
        matched_dir.path(),
        matched_port,
        Some("fixture-token"),
        false,
    );
    matched.load(SERVED, None).await.unwrap();
    assert!(wait_ready(&matched).await);
    let args = recorded_args(matched_dir.path());
    assert!(args.ends_with(r#","--api-key","fixture-token"]"#), "{args}");
    let response = matched
        .chat(chat_request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.content, "pong");
    matched.unload(SERVED).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_fake(dir.path(), r#"{"expect_bearer":"fixture-token"}"#);
    let denied = adapter(dir.path(), port, Some("other-token"), false);
    denied.load(SERVED, None).await.unwrap();
    assert!(wait_until(dir.path(), port).await);
    let args = recorded_args(dir.path());
    assert!(args.ends_with(r#","--api-key","other-token"]"#), "{args}");
    let probe = denied.probe_ready(SERVED).await.unwrap_err();
    assert!(probe.to_string().contains("401"));
    let chat = denied
        .chat(
            ChatRequest {
                model: SERVED.to_string(),
                messages: vec![],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(chat.to_string().contains("401"));
    assert!(!dir.path().join("chat.json").exists());
    denied.unload(SERVED).await.unwrap();
    assert!(!port_accepts(port));
}

#[tokio::test]
async fn fake_vllm_exit_leaves_the_model_unready_and_the_port_closed() {
    let _guard = gate().lock().await;
    assert!(PathBuf::from("/usr/bin/python3").is_file());
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_fake(dir.path(), r#"{"exit_immediately":true}"#);
    let adapter = adapter(dir.path(), port, None, false);
    adapter.load(SERVED, None).await.unwrap();
    let mut started = false;
    for _ in 0..100 {
        if dir.path().join("pid.txt").exists() {
            started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(started);
    assert!(!adapter.probe_ready(SERVED).await.unwrap());
    assert!(!port_accepts(port));
    assert!(adapter.status().await.unwrap().loaded.is_empty());
    assert!(!pid_alive(pid_of(dir.path())));
}
