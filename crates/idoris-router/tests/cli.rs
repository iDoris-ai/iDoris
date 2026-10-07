#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

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

fn base_command<'a>(repo: &'a Path, components: &'a Path) -> Command {
    let mut command = clean_command();
    command
        .arg("serve")
        .current_dir(repo)
        .env("IDORIS_COMPONENTS_DIR", components)
        .env(
            "IDORIS_ROUTING_POLICY",
            repo.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_CATALOG", repo.join("config/catalog.yaml"));
    command
}

#[test]
fn invalid_commands_fail_before_startup_with_usage() {
    for args in [vec!["nope"], vec!["serve", "extra"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_idoris"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("idoris admin <status|backends|models|roles|runtimes> --token-stdin"),
            "{stderr}"
        );
    }
}

#[test]
fn serve_reports_an_actionable_port_conflict() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let components = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();

    let output = base_command(&repo, components.path())
        .env("IDORIS_PORT", port.to_string())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("无法绑定"), "{stderr}");
    assert!(stderr.contains(&port.to_string()), "{stderr}");
}

#[test]
fn invalid_admin_port_fails_startup() {
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let output = base_command(&repo, components.path())
        .env("IDORIS_PORT", free_port().to_string())
        .env("IDORIS_ADMIN_PORT", "invalid")
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("IDORIS_ADMIN_PORT"));
}

#[tokio::test]
async fn occupied_admin_port_fails_before_data_health_is_served() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin_port = occupied.local_addr().unwrap().port();
    let data_port = free_port();
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let mut child = base_command(&repo, components.path())
        .arg("--admin-token-stdin")
        .env("IDORIS_PORT", data_port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        let health = client
            .get(format!("http://127.0.0.1:{data_port}/health"))
            .timeout(Duration::from_millis(50))
            .send()
            .await;
        assert!(
            !health.is_ok_and(|response| response.status().is_success()),
            "data plane served before Admin bind succeeded"
        );
        assert!(Instant::now() < deadline, "startup did not fail");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("无法绑定 Admin"), "{stderr}");
    assert!(stderr.contains(&admin_port.to_string()), "{stderr}");
}

#[tokio::test]
async fn launcher_token_stdin_authenticates_admin_without_leaking_to_logs() {
    let data_port = free_port();
    let mut admin_port = free_port();
    while admin_port == data_port {
        admin_port = free_port();
    }
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut child = base_command(&repo, components.path())
        .arg("--admin-token-stdin")
        .env("IDORIS_PORT", data_port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{secret}\n").as_bytes())
        .unwrap();

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{data_port}/health"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let admin_url = format!("http://127.0.0.1:{admin_port}/admin/api/v1/status");
    assert_eq!(
        client
            .get(&admin_url)
            .bearer_auth("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&admin_url)
            .bearer_auth(secret)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{data_port}/admin/api/v1/status"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!logs.contains(secret));
    assert!(!logs.contains("Bearer "));
}

#[tokio::test]
async fn admin_readonly_cli_uses_stdin_token_for_every_published_resource() {
    let data_port = free_port();
    let mut admin_port = free_port();
    while admin_port == data_port {
        admin_port = free_port();
    }
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut daemon = base_command(&repo, components.path())
        .arg("--admin-token-stdin")
        .env("IDORIS_PORT", data_port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    daemon
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{secret}\n").as_bytes())
        .unwrap();

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{data_port}/health"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        assert!(daemon.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "health readiness timed out");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    for resource in ["status", "backends", "models", "roles", "runtimes"] {
        let mut admin = clean_command();
        admin
            .args(["admin", resource, "--token-stdin"])
            .env("IDORIS_ADMIN_PORT", admin_port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut admin = admin.spawn().unwrap();
        admin
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{secret}\n").as_bytes())
            .unwrap();
        let output = admin.wait_with_output().unwrap();
        assert!(output.status.success(), "{resource}: {output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        match resource {
            "status" => assert!(value["instance_id"].is_string()),
            "backends" | "roles" => assert!(value.is_array()),
            "models" => assert!(value["sources"].is_array()),
            "runtimes" => assert!(value["runtimes"].is_array()),
            _ => unreachable!(),
        }
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!combined.contains(secret), "{resource}");
        assert!(!combined.contains("Bearer "), "{resource}");
    }

    daemon.kill().unwrap();
    let _ = daemon.wait();
}

#[tokio::test]
async fn malformed_launcher_token_fails_before_either_surface_serves() {
    let data_port = free_port();
    let mut admin_port = free_port();
    while admin_port == data_port {
        admin_port = free_port();
    }
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let mut child = base_command(&repo, components.path())
        .arg("--admin-token-stdin")
        .env("IDORIS_PORT", data_port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"too-short\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Admin session token") || stderr.contains("完整 Admin"));
    assert!(!stderr.contains("too-short"));

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for port in [data_port, admin_port] {
        assert!(
            client
                .get(format!("http://127.0.0.1:{port}/health"))
                .timeout(Duration::from_millis(100))
                .send()
                .await
                .is_err(),
            "listener {port} remained reachable after token rejection"
        );
    }
}

#[test]
fn same_data_and_admin_port_fails_cleanly() {
    let port = free_port();
    let components = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let output = base_command(&repo, components.path())
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_ADMIN_PORT", port.to_string())
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("无法绑定 Admin"), "{stderr}");
    assert!(stderr.contains(&port.to_string()), "{stderr}");
}
