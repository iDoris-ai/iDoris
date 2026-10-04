#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{fs, path::Path, process::Command};

#[test]
fn invalid_commands_fail_before_startup_with_usage() {
    for args in [vec!["nope"], vec!["serve", "extra"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_idoris"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("用法: idoris [serve]"), "{stderr}");
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

    let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
    {
        command.env_remove(key);
    }
    let output = command
        .arg("serve")
        .current_dir(&repo)
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_COMPONENTS_DIR", components.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            repo.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_CATALOG", repo.join("config/catalog.yaml"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("无法绑定"), "{stderr}");
    assert!(stderr.contains(&port.to_string()), "{stderr}");
}
