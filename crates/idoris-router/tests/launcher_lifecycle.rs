//! Launcher-hosting lifecycle: SIGTERM graceful stop and single-instance DB lock.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Env {
    repo: std::path::PathBuf,
    components: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let components = tempfile::tempdir().unwrap();
        fs::copy(
            repo.join("config/components/omlx.yaml"),
            components.path().join("omlx.yaml"),
        )
        .unwrap();
        Self {
            repo,
            components,
            state: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, port: u16, admin_port: u16) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
        {
            command.env_remove(key);
        }
        command
            .current_dir(&self.repo)
            .env("IDORIS_COMPONENTS_DIR", self.components.path())
            .env(
                "IDORIS_ROUTING_POLICY",
                self.repo.join("config/routing-policy.yaml"),
            )
            .env("IDORIS_CATALOG", self.repo.join("config/catalog.yaml"))
            .env("IDORIS_DB_PATH", self.state.path().join("state.sqlite3"))
            .env("IDORIS_PORT", port.to_string())
            .env("IDORIS_ADMIN_PORT", admin_port.to_string())
            .env("IDORIS_DEV_NO_KEY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn serve(&self, port: u16, admin_port: u16) -> Child {
        let mut child = self
            .command(port, admin_port)
            .args(["serve", "--admin-token-stdin"])
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{SECRET}\n").as_bytes())
            .unwrap();
        child
    }
}

async fn wait_health(child: &mut Child, port: u16) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            && response.status().is_success()
        {
            return;
        }
        assert!(child.try_wait().unwrap().is_none(), "server exited early");
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "process did not exit in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_all(mut pipe: impl Read) -> String {
    let mut out = String::new();
    pipe.read_to_string(&mut out).unwrap();
    out
}

fn send(signal: &str, child: &Child) {
    let status = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn sigterm_and_sigint_take_the_graceful_path() {
    for signal in ["-TERM", "-INT"] {
        let env = Env::new();
        let (port, admin) = (free_port(), free_port());
        let mut child = env.serve(port, admin);
        wait_health(&mut child, port).await;
        send(signal, &child);
        let status = wait_exit(&mut child, Duration::from_secs(12));
        let stdout = read_all(child.stdout.take().unwrap());
        assert_eq!(status.code(), Some(0), "{signal}: {status:?}\n{stdout}");
        assert!(stdout.contains("idoris: 已停止"), "{signal}: {stdout}");
    }
}

#[tokio::test]
async fn second_instance_on_same_db_exits_3_and_first_stays_healthy() {
    let env = Env::new();
    let (port, admin) = (free_port(), free_port());
    let mut first = env.serve(port, admin);
    wait_health(&mut first, port).await;

    let (port2, admin2) = (free_port(), free_port());
    let mut second = env.serve(port2, admin2);
    let status = wait_exit(&mut second, Duration::from_secs(10));
    let stderr = read_all(second.stderr.take().unwrap());
    assert_eq!(status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains("[idoris] 启动失败：已有 iDoris 实例在使用同一数据库"),
        "{stderr}"
    );
    assert!(stderr.contains("state.sqlite3"), "{stderr}");
    wait_health(&mut first, port).await;

    send("-TERM", &first);
    wait_exit(&mut first, Duration::from_secs(12));
    // Lock is released on exit: a fresh instance can start again.
    let mut third = env.serve(port, admin);
    wait_health(&mut third, port).await;
    send("-TERM", &third);
    wait_exit(&mut third, Duration::from_secs(12));
}

#[tokio::test]
async fn offline_key_issue_works_while_server_holds_the_lock() {
    let env = Env::new();
    let (port, admin) = (free_port(), free_port());
    let mut server = env.serve(port, admin);
    wait_health(&mut server, port).await;

    let mut issue = env
        .command(port, admin)
        .args(["key", "issue", "--spec-stdin"])
        .spawn()
        .unwrap();
    issue
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"owner":"agent24","allowed_privacy":["local_only"],"allowed_roles":["fast"]}"#,
        )
        .unwrap();
    let status = wait_exit(&mut issue, Duration::from_secs(10));
    let stderr = read_all(issue.stderr.take().unwrap());
    assert_eq!(status.code(), Some(0), "{stderr}");

    send("-TERM", &server);
    wait_exit(&mut server, Duration::from_secs(12));
}

#[test]
fn unusable_lock_file_fails_startup_with_exit_1_and_never_serves() {
    let env = Env::new();
    fs::create_dir(env.state.path().join("state.sqlite3.lock")).unwrap();
    let (port, admin) = (free_port(), free_port());
    let mut child = env.serve(port, admin);
    let status = wait_exit(&mut child, Duration::from_secs(10));
    let stdout = read_all(child.stdout.take().unwrap());
    let stderr = read_all(child.stderr.take().unwrap());
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("[idoris] 启动失败：无法获取实例锁"),
        "{stderr}"
    );
    assert!(!stdout.contains("listening"), "{stdout}");
}
