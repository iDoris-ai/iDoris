#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::post;
use idoris_backend::{ChatMessage, ChatRequest};
use idoris_router::connection::{
    ConnectionInfo, ConnectionListener, RequestLifecycle, request_lifecycle_middleware,
};
use idoris_upstream::subscription::profile::{SandboxProfile, SubscriptionCli};
use idoris_upstream::subscription::relay::{SubscriptionRelay, SubscriptionRelayConfig};
use idoris_upstream::subscription::service::SubscriptionService;
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct HarnessState {
    service: SubscriptionService,
}

async fn relay_handler(
    State(state): State<HarnessState>,
    Extension(lifecycle): Extension<RequestLifecycle>,
    body: Bytes,
) -> impl IntoResponse {
    let request = ChatRequest {
        model: "claude-subscription".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: String::from_utf8_lossy(&body).to_string(),
        }],
    };
    match state
        .service
        .chat(request, lifecycle.cancellation_token())
        .await
    {
        Ok(response) => (StatusCode::OK, response.content).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error.reason_code()).into_response(),
    }
}

struct FakeBin {
    _root: tempfile::TempDir,
    bin_dir: PathBuf,
    marker_dir: PathBuf,
}

impl FakeBin {
    fn hanging() -> Self {
        Self::install(
            r#"
payload="$(cat)"
marker="$TEST_MARKER_DIR/$payload"
pgid="$(ps -o pgid= -p $$ | tr -d ' ')"
printf 'parent:%s:%s\n' "$$" "$pgid" > "$marker"
printf 'cwd:%s\n' "$PWD" >> "$marker"
/bin/sh -c '
  marker="$1"
  trap "exit 0" TERM INT
  printf "child:%s:%s\n" "$$" "$(ps -o pgid= -p $$ | tr -d " ")" >> "$marker"
  while :; do sleep 1; done
' sh "$marker" &
child=$!
trap 'wait "$child" 2>/dev/null || true; exit 0' TERM INT
wait "$child"
"#,
        )
    }

    fn slow_success() -> Self {
        Self::install(
            r#"
payload="$(cat)"
sleep 0.15
printf 'ok:%s' "$payload"
"#,
        )
    }

    fn install(script_body: &str) -> Self {
        let root = tempfile::TempDir::new().unwrap();
        let bin_dir = root.path().join("bin");
        let marker_dir = root.path().join("markers");
        fs::create_dir(&bin_dir).unwrap();
        fs::create_dir(&marker_dir).unwrap();
        for name in ["claude", "codex"] {
            let path = bin_dir.join(name);
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{script_body}\n")).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        Self {
            _root: root,
            bin_dir,
            marker_dir,
        }
    }

    fn env(&self) -> BTreeMap<OsString, OsString> {
        BTreeMap::from([
            (
                OsString::from("PATH"),
                OsString::from(format!("{}:/usr/bin:/bin", self.bin_dir.display())),
            ),
            (
                OsString::from("TEST_MARKER_DIR"),
                self.marker_dir.as_os_str().to_os_string(),
            ),
        ])
    }

    fn marker(&self, name: &str) -> PathBuf {
        self.marker_dir.join(name)
    }
}

fn service(fake: &FakeBin) -> SubscriptionService {
    let mut config = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        fake.env(),
    );
    config.process_timeout = Duration::from_secs(30);
    config.termination_grace = Duration::from_millis(100);
    SubscriptionService::new(SubscriptionRelay::new(config), Duration::from_secs(3))
}

struct Server {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(service: SubscriptionService) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let app = Router::new()
            .route("/relay", post(relay_handler))
            .layer(middleware::from_fn(request_lifecycle_middleware))
            .with_state(HarnessState { service });
        let task = tokio::spawn(async move {
            axum::serve(
                ConnectionListener::new(listener, shutdown).unwrap(),
                app.into_make_service_with_connect_info::<ConnectionInfo>(),
            )
            .await
            .unwrap();
        });
        Self { addr, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn send_request(stream: &mut tokio::net::TcpStream, body: &str) {
    let request = format!(
        "POST /relay HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(request.as_bytes()).await.unwrap();
}

async fn read_response(stream: &mut tokio::net::TcpStream) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut response = Vec::new();
        loop {
            if let Some(header_end) = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
            {
                let headers = String::from_utf8_lossy(&response[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while response.len() < header_end + content_length {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0, "connection closed during response");
                    response.extend_from_slice(&chunk[..read]);
                }
                return String::from_utf8_lossy(&response).to_string();
            }
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "connection closed before response");
            response.extend_from_slice(&chunk[..read]);
        }
    })
    .await
    .expect("response timeout")
}

#[derive(Debug)]
struct Marker {
    parent_group: i32,
    child_group: i32,
    workspace: PathBuf,
}

async fn wait_marker(path: &Path) -> Marker {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                let mut parent_group = None;
                let mut child_group = None;
                let mut workspace = None;
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("parent:") {
                        parent_group = value.split(':').nth(1).and_then(|value| value.parse().ok());
                    } else if let Some(value) = line.strip_prefix("child:") {
                        child_group = value.split(':').nth(1).and_then(|value| value.parse().ok());
                    } else if let Some(value) = line.strip_prefix("cwd:") {
                        workspace = Some(PathBuf::from(value));
                    }
                }
                if let (Some(parent_group), Some(child_group), Some(workspace)) =
                    (parent_group, child_group, workspace)
                {
                    return Marker {
                        parent_group,
                        child_group,
                        workspace,
                    };
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake CLI handshake")
}

fn group_is_alive(group: i32) -> bool {
    killpg(Pid::from_raw(group), None).is_ok()
}

async fn wait_clean(marker: &Marker) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !group_is_alive(marker.parent_group) && !marker.workspace.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process group and workspace must be cleaned");
}

#[tokio::test]
async fn full_body_then_disconnect_reaps_group_and_removes_workspace() {
    let fake = FakeBin::hanging();
    let service = service(&fake);
    let server = Server::start(service.clone()).await;
    let mut client = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    send_request(&mut client, "single").await;
    let marker = wait_marker(&fake.marker("single")).await;
    assert_eq!(marker.parent_group, marker.child_group);
    assert!(marker.workspace.exists());
    assert_eq!(service.active_requests().await, 1);

    drop(client);
    wait_clean(&marker).await;
    assert_eq!(service.active_requests().await, 0);
}

#[tokio::test]
async fn open_keep_alive_connection_allows_slow_requests_to_finish() {
    let fake = FakeBin::slow_success();
    let service = service(&fake);
    let server = Server::start(service.clone()).await;
    let mut client = tokio::net::TcpStream::connect(server.addr).await.unwrap();

    for body in ["first", "second"] {
        send_request(&mut client, body).await;
        let response = read_response(&mut client).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains(&format!("ok:{body}")));
    }
    assert_eq!(service.active_requests().await, 0);
}

#[tokio::test]
async fn disconnecting_a_does_not_cancel_concurrent_b() {
    let fake = FakeBin::hanging();
    let service = service(&fake);
    let server = Server::start(service.clone()).await;
    let mut a = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let mut b = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    send_request(&mut a, "a").await;
    send_request(&mut b, "b").await;
    let marker_a = wait_marker(&fake.marker("a")).await;
    let marker_b = wait_marker(&fake.marker("b")).await;
    assert_eq!(service.active_requests().await, 2);

    drop(a);
    wait_clean(&marker_a).await;
    assert!(group_is_alive(marker_b.parent_group));
    assert!(marker_b.workspace.exists());
    assert_eq!(service.active_requests().await, 1);

    drop(b);
    wait_clean(&marker_b).await;
    assert_eq!(service.active_requests().await, 0);
}

#[tokio::test]
async fn repeated_disconnects_leave_no_groups_or_workspaces() {
    let fake = FakeBin::hanging();
    let service = service(&fake);
    let server = Server::start(service.clone()).await;

    for body in ["repeat-a", "repeat-b"] {
        let mut client = tokio::net::TcpStream::connect(server.addr).await.unwrap();
        send_request(&mut client, body).await;
        let marker = wait_marker(&fake.marker(body)).await;
        drop(client);
        wait_clean(&marker).await;
        assert_eq!(service.active_requests().await, 0);
    }
}
