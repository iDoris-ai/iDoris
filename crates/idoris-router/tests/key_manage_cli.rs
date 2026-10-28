#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn idoris(db: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
    {
        command.env_remove(key);
    }
    let mut child = command
        .args(args)
        .env("IDORIS_DB_PATH", db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    if let Some(input) = stdin {
        pipe.write_all(input.as_bytes()).unwrap();
    }
    drop(pipe);
    child.wait_with_output().unwrap()
}

fn issue(db: &Path, role: &str, owner: &str) -> (String, String) {
    let spec = format!(
        r#"{{"owner":"{owner}","tenant_id":null,"allowed_privacy":["local_only"],"allowed_roles":["{role}"]}}"#
    );
    let out = idoris(db, &["key", "issue", "--spec-stdin"], Some(&spec));
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        json["key_id"].as_str().unwrap().to_string(),
        json["secret"].as_str().unwrap().to_string(),
    )
}

fn list(db: &Path) -> (serde_json::Value, String) {
    let out = idoris(db, &["key", "list"], None);
    assert!(out.status.success(), "{out:?}");
    let raw = String::from_utf8(out.stdout).unwrap();
    (serde_json::from_str(&raw).unwrap(), raw)
}

#[test]
fn list_and_revoke_round_trip_without_leaking_secrets() {
    let state = tempfile::tempdir().unwrap();
    let db = state.path().join("state.sqlite3");
    let (id_a, secret_a) = issue(&db, "fast", "alpha");
    let (id_b, secret_b) = issue(&db, "daily", "beta");

    let (json, raw) = list(&db);
    for secret in [&secret_a, &secret_b] {
        assert!(!raw.contains(secret.as_str()), "list leaked a secret");
        assert!(!raw.contains(&secret[secret.len() / 2..]));
    }
    assert!(!raw.contains("hash") && !raw.contains("secret"));
    let keys = json["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0]["key_id"], id_a);
    assert_eq!(keys[0]["owner"], "alpha");
    assert_eq!(keys[0]["allowed_roles"], serde_json::json!(["fast"]));
    assert_eq!(
        keys[0]["allowed_privacy"],
        serde_json::json!(["local_only"])
    );
    assert_eq!(keys[0]["tenant_id"], serde_json::Value::Null);
    assert_eq!(keys[0]["status"], "active");
    assert_eq!(keys[1]["key_id"], id_b);
    assert_eq!(keys[1]["allowed_roles"], serde_json::json!(["daily"]));
    assert_eq!(keys[1]["status"], "active");

    let out = idoris(&db, &["key", "revoke", &id_a], None);
    assert!(out.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!({"key_id": id_a, "revoked": true})
    );
    let (json, _) = list(&db);
    assert_eq!(json["keys"][0]["status"], "revoked");
    assert!(json["keys"][0]["revoked_at_ms"].as_i64().unwrap() > 0);
    assert_eq!(json["keys"][1]["status"], "active");

    let again = idoris(&db, &["key", "revoke", &id_a], None);
    assert!(again.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&again.stdout).unwrap(),
        serde_json::json!({"key_id": id_a, "revoked": false, "already_revoked": true})
    );

    let unknown = format!("vk_{}", "0".repeat(32));
    let out = idoris(&db, &["key", "revoke", &unknown], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("未找到"));

    let out = idoris(&db, &["key", "revoke", "vk_nope"], None);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn running_server_rejects_key_revoked_while_serving() {
    let state = tempfile::tempdir().unwrap();
    let db = state.path().join("state.sqlite3");
    let (id, secret) = issue(&db, "fast", "alpha");
    let (_other_id, _other_secret) = issue(&db, "daily", "beta");

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let components = tempfile::tempdir().unwrap();
    fs::copy(
        repo.join("config/components/omlx.yaml"),
        components.path().join("omlx.yaml"),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in
        std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("IDORIS_"))
    {
        command.env_remove(key);
    }
    let mut child = command
        .arg("serve")
        .current_dir(&repo)
        .env("IDORIS_COMPONENTS_DIR", components.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            repo.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_CATALOG", repo.join("config/catalog.yaml"))
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_DB_PATH", &db)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let result = std::panic::catch_unwind(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let url = format!("http://127.0.0.1:{port}/v1/models");
            let status = |client: &reqwest::Client| {
                let req = client
                    .get(&url)
                    .bearer_auth(&secret)
                    .timeout(Duration::from_secs(2));
                async move { req.send().await.map(|r| r.status().as_u16()) }
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            let before = loop {
                if let Ok(code) = status(&client).await {
                    break code;
                }
                assert!(Instant::now() < deadline, "server did not start");
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            assert_ne!(before, 401, "active key must authenticate");
            let out = idoris(&db, &["key", "revoke", &id], None);
            assert!(out.status.success(), "{out:?}");
            assert_eq!(status(&client).await.unwrap(), 401);
        });
    });
    let _ = child.kill();
    let _ = child.wait();
    result.unwrap();
}
