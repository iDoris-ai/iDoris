//! `idoris setup omlx` against a fake oMLX: probe classification, bindings,
//! validated output, and that the API key never reaches disk or stdout.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "omlx-secret-key-588-zzz";

/// Fake oMLX; with `key`, only that exact Bearer is accepted (else 401).
async fn fake(key: Option<&str>, models: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    let list = json!({"data": models.iter().map(|m| json!({"id": m})).collect::<Vec<_>>()});
    let status = json!({"models": models.iter().map(|m| json!({"id": m, "estimated_size": 1})).collect::<Vec<_>>()});
    for (p, body) in [
        ("/v1/models", list),
        ("/v1/models/status", status),
        ("/api/status", json!({})),
    ] {
        let ok = ResponseTemplate::new(200).set_body_json(body);
        match key {
            None => {
                Mock::given(method("GET"))
                    .and(path(p))
                    .respond_with(ok)
                    .mount(&server)
                    .await
            }
            Some(k) => {
                Mock::given(method("GET"))
                    .and(path(p))
                    .and(header("authorization", format!("Bearer {k}").as_str()))
                    .respond_with(ok)
                    .with_priority(1)
                    .mount(&server)
                    .await;
                Mock::given(method("GET"))
                    .and(path(p))
                    .respond_with(ResponseTemplate::new(401))
                    .with_priority(5)
                    .mount(&server)
                    .await;
            }
        }
    }
    server
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
    json: Value,
}

async fn setup(endpoint: &str, out: &Path, extra: &[&str], stdin: Option<&str>) -> Out {
    let (endpoint, out, extra, stdin) = (
        endpoint.to_string(),
        out.to_owned(),
        extra.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        stdin.map(str::to_string),
    );
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
        cmd.args(["setup", "omlx", "--endpoint", &endpoint, "--out-dir"])
            .arg(&out)
            .args(&extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            pipe.write_all(s.as_bytes()).unwrap();
        }
        drop(pipe);
        let o = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
        Out {
            code: o.status.code().unwrap(),
            json: serde_json::from_str(&stdout).unwrap_or(Value::Null),
            stdout,
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    })
    .await
    .unwrap()
}

fn err_code(o: &Out) -> &str {
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(o.json["status"], "error");
    o.json["code"].as_str().unwrap()
}

fn assert_no_key(o: &Out, dir: &Path) {
    assert!(!o.stdout.contains(KEY) && !o.stderr.contains(KEY));
    if dir.exists() {
        for e in fs::read_dir(dir).unwrap() {
            assert!(!fs::read_to_string(e.unwrap().path()).unwrap().contains(KEY));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ok_without_auth_writes_loadable_dir() {
    let server = fake(None, &["m1", "m2"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=m1", "--bind", "fast=m2"],
        None,
    )
    .await;
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.json["status"], "ok");
    assert_eq!(o.json["auth"], "none");
    assert_eq!(o.json["models"], json!(["m1", "m2"]));
    assert_eq!(o.json["bindings"], json!({"daily": "m1", "fast": "m2"}));
    assert!(o.json["next"].get("needs_env").is_none());
    let cards = idoris_router::components::load_components(&out, false).unwrap();
    assert_eq!(cards.len(), 2);
    let omlx = fs::read_to_string(out.join("omlx.yaml")).unwrap();
    assert!(omlx.contains(&server.uri()) && omlx.contains("idoris_role_models"));
    assert!(out.join("subscription.yaml").exists());
    assert_eq!(
        fs::read_dir(tmp.path()).unwrap().count(),
        1,
        "staging dir cleaned"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ok_with_auth_never_persists_key_and_server_starts() {
    let server = fake(Some(KEY), &["m1"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(
        &server.uri(),
        &out,
        &["--bind", "daily=m1", "--api-key-stdin"],
        Some(&format!("{KEY}\n")),
    )
    .await;
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.json["auth"], "required");
    assert_eq!(o.json["next"]["needs_env"], json!(["IDORIS_OMLX_API_KEY"]));
    assert_no_key(&o, &out);
    assert_eq!(
        o.json["next"]["env"]["IDORIS_COMPONENTS_DIR"],
        out.canonicalize().unwrap().to_str().unwrap()
    );

    // The real binary boots from the generated directory.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (port, admin_port) = (
        probe.local_addr().unwrap().port(),
        admin.local_addr().unwrap().port(),
    );
    drop((probe, admin));
    let cfg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (k, _) in std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_")) {
        cmd.env_remove(k);
    }
    let mut child = cmd
        .current_dir(tmp.path())
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env("IDORIS_DB_PATH", tmp.path().join("t.sqlite3"))
        .env("IDORIS_COMPONENTS_DIR", &out)
        .env("IDORIS_ROUTING_POLICY", cfg.join("routing-policy.yaml"))
        .env("IDORIS_CATALOG", cfg.join("catalog.yaml"))
        .env("IDORIS_OMLX_API_KEY", KEY)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let healthy = loop {
        if let Ok(r) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            && r.status().is_success()
        {
            break r.json::<Value>().await.unwrap()["status"] == "ok";
        }
        if child.try_wait().unwrap().is_some() || Instant::now() > deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        healthy,
        "idoris serve did not become healthy from the generated dir"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_classification() {
    let server = fake(Some(KEY), &["m1"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(&server.uri(), &out, &[], None).await;
    assert_eq!(err_code(&o), "auth_required");
    let o = setup(
        &server.uri(),
        &out,
        &["--api-key-stdin"],
        Some("wrong-key\n"),
    )
    .await;
    assert_eq!(err_code(&o), "auth_failed");
    assert!(!o.stdout.contains("wrong-key"));
    let o = setup(&server.uri(), &out, &["--api-key-stdin"], Some("\n")).await;
    assert_eq!(err_code(&o), "invalid_api_key_input");
    assert!(!out.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn bindings_endpoint_and_probe_errors() {
    let server = fake(None, &["m1", "bad "]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(&server.uri(), &out, &["--bind", "daily=nope"], None).await;
    assert_eq!(err_code(&o), "model_not_found");
    assert!(!o.stdout.contains("m1"), "no model listing in errors");
    let o = setup(&server.uri(), &out, &["--bind", "bogus=m1"], None).await;
    assert_eq!(err_code(&o), "invalid_role");
    // Probe passes (model exists) but the server's loader rejects the binding:
    // validation must run and must leave nothing behind.
    let o = setup(&server.uri(), &out, &["--bind", "daily=bad "], None).await;
    assert_eq!(err_code(&o), "validation_failed");
    assert!(!out.exists());
    assert_eq!(
        fs::read_dir(tmp.path()).unwrap().count(),
        0,
        "staging dir cleaned"
    );
    for bad in [
        "http://example.com:8000",
        "http://10.0.0.5:8000",
        "http://127.0.0.1.evil.com",
        "http://user:pw@127.0.0.1:8000",
        "ftp://127.0.0.1",
        "http://127.0.0.1:8000/v1",
        "nonsense",
    ] {
        assert_eq!(
            err_code(&setup(bad, &out, &[], None).await),
            "invalid_endpoint",
            "{bad}"
        );
    }
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", closed.local_addr().unwrap().port());
    drop(closed);
    assert_eq!(err_code(&setup(&url, &out, &[], None).await), "unreachable");

    let alien = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"hello": 1})))
        .mount(&alien)
        .await;
    assert_eq!(
        err_code(&setup(&alien.uri(), &out, &[], None).await),
        "not_omlx"
    );
    // on_demand needs the lifecycle endpoints; resident does not.
    let thin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}]})))
        .mount(&thin)
        .await;
    assert_eq!(
        err_code(&setup(&thin.uri(), &out, &[], None).await),
        "not_omlx"
    );
    let o = setup(&thin.uri(), &out, &["--load-mode", "resident"], None).await;
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert!(
        fs::read_to_string(out.join("omlx.yaml"))
            .unwrap()
            .contains("resident")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn out_dir_rules() {
    let server = fake(None, &["m1"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("other.txt"), "x").unwrap();
    assert_eq!(
        err_code(&setup(&server.uri(), &out, &["--force"], None).await),
        "out_dir_not_empty"
    );
    fs::remove_file(out.join("other.txt")).unwrap();
    fs::write(out.join("omlx.yaml"), "stale").unwrap();
    assert_eq!(
        err_code(&setup(&server.uri(), &out, &[], None).await),
        "out_dir_not_empty"
    );
    assert_eq!(fs::read_to_string(out.join("omlx.yaml")).unwrap(), "stale");
    let o = setup(&server.uri(), &out, &["--force"], None).await;
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert!(
        fs::read_to_string(out.join("omlx.yaml"))
            .unwrap()
            .contains(&server.uri())
    );
    idoris_router::components::load_components(&out, false).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn redirects_are_not_followed() {
    let target = fake(None, &["m1"]).await;
    let redirector = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/v1/models", target.uri()).as_str()),
        )
        .mount(&redirector)
        .await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(&redirector.uri(), &out, &[], None).await;
    assert_eq!(err_code(&o), "not_omlx", "{}", o.stdout);
    assert!(!out.exists());
    assert!(
        target.received_requests().await.unwrap().is_empty(),
        "redirect target must never be requested"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_models_body_is_rejected() {
    let server = MockServer::start().await;
    let pad = "x".repeat(3 * 1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}], "pad": pad})),
        )
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(&server.uri(), &out, &["--load-mode", "resident"], None).await;
    assert_eq!(err_code(&o), "not_omlx", "{}", o.stdout);
    assert!(!out.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn models_body_just_under_the_cap_is_accepted() {
    let server = MockServer::start().await;
    let pad = "x".repeat(1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}], "pad": pad})),
        )
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let o = setup(
        &server.uri(),
        &tmp.path().join("comp"),
        &["--load-mode", "resident"],
        None,
    )
    .await;
    assert_eq!(o.code, 0, "{}", o.stdout);
}
