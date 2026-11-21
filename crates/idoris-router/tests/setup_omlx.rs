//! `idoris setup omlx` against a fake oMLX: probe classification, bindings,
//! validated output, and that the API key never reaches disk or stdout.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
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

/// 子进程的守卫：不管测试怎么结束（包括断言失败、panic），Drop 时 SIGKILL 并回收，不会留下孤儿进程。
struct Reaped(Option<Child>);

impl Reaped {
    fn spawn(cmd: &mut Command) -> Self {
        Self(Some(cmd.spawn().unwrap()))
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

async fn setup(endpoint: &str, out: &Path, extra: &[&str], stdin: Option<&str>) -> Out {
    setup_in(None, endpoint, out, extra, stdin).await
}

async fn setup_in(
    cwd: Option<&Path>,
    endpoint: &str,
    out: &Path,
    extra: &[&str],
    stdin: Option<&str>,
) -> Out {
    let (cwd, endpoint, out, extra, stdin) = (
        cwd.map(Path::to_owned),
        endpoint.to_string(),
        out.to_owned(),
        extra.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        stdin.map(str::to_string),
    );
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
        if let Some(cwd) = &cwd {
            cmd.current_dir(cwd);
        }
        cmd.args(["setup", "omlx", "--endpoint", &endpoint, "--out-dir"])
            .arg(&out)
            .args(&extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Reaped::spawn(&mut cmd);
        let mut pipe = child.child().stdin.take().unwrap();
        if let Some(s) = stdin {
            pipe.write_all(s.as_bytes()).unwrap();
        }
        drop(pipe);
        let o = child.finish();
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
    let mut child = Reaped::spawn(
        cmd.current_dir(tmp.path())
            .env("IDORIS_PORT", port.to_string())
            .env("IDORIS_ADMIN_PORT", admin_port.to_string())
            .env("IDORIS_DB_PATH", tmp.path().join("t.sqlite3"))
            .env("IDORIS_COMPONENTS_DIR", &out)
            .env("IDORIS_ROUTING_POLICY", cfg.join("routing-policy.yaml"))
            .env("IDORIS_CATALOG", cfg.join("catalog.yaml"))
            .env("IDORIS_OMLX_API_KEY", KEY)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
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
        if child.child().try_wait().unwrap().is_some() || Instant::now() > deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    // Drop 杀掉并回收；放在断言之前，断言失败也不留进程。
    drop(child);
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
async fn out_dir_dot_is_the_working_directory() {
    // The staging dir for `.` sits inside it; it must not count as foreign.
    let server = fake(None, &["m1"]).await;
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("comp");
    fs::create_dir(&cwd).unwrap();
    let dot = Path::new(".");
    let o = setup_in(Some(&cwd), &server.uri(), dot, &[], None).await;
    assert_eq!(o.code, 0, "{}", o.stdout);
    let o = setup_in(Some(&cwd), &server.uri(), dot, &["--force"], None).await;
    assert_eq!(o.code, 0, "{}", o.stdout);
    let mut names: Vec<_> = fs::read_dir(&cwd)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["omlx.yaml", "subscription.yaml"]);
    idoris_router::components::load_components(&cwd, false).unwrap();

    fs::write(cwd.join("other.txt"), "x").unwrap();
    let o = setup_in(Some(&cwd), &server.uri(), dot, &["--force"], None).await;
    assert_eq!(err_code(&o), "out_dir_not_empty");
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

// ---------------------------------------------------------------------------------------------
// IDORIS-CORE S4：`--auto`、`--managed`、`--no-probe`（进程级）
//
// 这里只放不依赖这台机器上有没有真实 oMLX 的路径：用法错误、采纳模式的输出形状、`--no-probe`、以及托管模式里
// 排在「引擎发现」之前就确定会失败的两种（端口被占、绑定的模型没装）。成功的托管路径与 `--auto` 要换候选端口、
// 可执行文件与家目录，在 `setup_omlx_flows.rs` 里用库级注入测试。
// ---------------------------------------------------------------------------------------------

/// 任意参数跑 `idoris setup omlx …`；`home` 给了就把 `HOME` 设成它（没给就设成一个不存在的目录，不碰真实的家目录）。
async fn cli(args: &[&str], home: Option<&Path>, stdin: Option<&str>) -> Out {
    let (args, home, stdin) = (
        args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        home.map(Path::to_owned),
        stdin.map(str::to_string),
    );
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (k, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
        {
            cmd.env_remove(k);
        }
        cmd.env(
            "HOME",
            home.unwrap_or_else(|| "/nonexistent-home-for-tests".into()),
        )
        .args(["setup", "omlx"])
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = Reaped::spawn(&mut cmd);
        let mut pipe = child.child().stdin.take().unwrap();
        if let Some(s) = stdin {
            pipe.write_all(s.as_bytes()).unwrap();
        }
        drop(pipe);
        let o = child.finish();
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

fn install_model(home: &Path, id: &str) {
    let dir = home.join(".omlx/models").join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.json"), "{}").unwrap();
    fs::write(dir.join("model.safetensors"), b"weights").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_output_gains_mode_and_engine_and_keeps_every_existing_field() {
    let server = fake(None, &["m1"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let o = setup(&server.uri(), &out, &["--bind", "daily=m1"], None).await;
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    // 既有字段（规范：「既有字段（含 endpoint）不变」）。
    assert_eq!(o.json["status"], "ok");
    assert_eq!(o.json["endpoint"], server.uri());
    assert_eq!(o.json["auth"], "none");
    assert_eq!(o.json["models"], json!(["m1"]));
    assert_eq!(o.json["bindings"], json!({"daily": "m1"}));
    assert_eq!(
        o.json["components_dir"],
        out.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(o.json["next"]["env"]["IDORIS_COMPONENTS_DIR"].is_string());
    // 新增字段：mode 与 engine。有没有装 oMLX 取决于这台机器，所以只核对形状与一致性。
    assert_eq!(o.json["mode"], "adopt");
    let engine = &o.json["engine"];
    assert_eq!(engine["engine_kind"], "omlx");
    let installed = engine["installed"].as_bool().unwrap();
    assert_eq!(installed, engine["executable"].is_string(), "{engine}");
    assert!(
        engine.get("version").is_none(),
        "采纳模式不执行可执行文件，没有 version"
    );
    // 采纳的卡不带托管声明。
    let card = fs::read_to_string(out.join("omlx.yaml")).unwrap();
    assert!(!card.contains("idoris_managed"), "{card}");
}

#[tokio::test(flavor = "multi_thread")]
async fn contradictory_setup_flags_are_a_usage_error_before_anything_runs() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let out_str = out.to_str().unwrap();
    for args in [
        vec![
            "--out-dir",
            out_str,
            "--endpoint",
            "http://127.0.0.1:1",
            "--auto",
        ],
        vec![
            "--out-dir",
            out_str,
            "--endpoint",
            "http://127.0.0.1:1",
            "--managed",
        ],
        vec!["--out-dir", out_str, "--auto", "--no-probe"],
        vec!["--out-dir", out_str, "--managed-port", "19731"],
        vec!["--out-dir", out_str, "--managed", "--managed-port", "0"],
        vec!["--out-dir", out_str, "--managed", "--api-key-stdin"],
        // `--auto` 探测的端口是猜的：端口属主校验（#619）落地之前不发 key，要鉴权的引擎用 `--endpoint`。
        vec!["--out-dir", out_str, "--auto", "--api-key-stdin"],
        vec![
            "--out-dir",
            out_str,
            "--auto",
            "--managed",
            "--api-key-stdin",
        ],
    ] {
        let o = cli(&args, None, Some(&format!("{KEY}\n"))).await;
        assert_eq!(o.code, 1, "{args:?}");
        assert!(o.stderr.contains("用法"), "{args:?}: {}", o.stderr);
        assert!(o.stdout.is_empty(), "用法错误不输出 JSON：{}", o.stdout);
        assert!(!o.stderr.contains(KEY));
        assert!(!out.exists());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_probe_adopts_without_connecting_and_checks_bindings_against_the_homes_models() {
    let server = fake(None, &["engine-only-model"]).await;
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    install_model(&home, "installed-model");
    let out = tmp.path().join("comp");
    let o = cli(
        &[
            "--out-dir",
            out.to_str().unwrap(),
            "--endpoint",
            &server.uri(),
            "--no-probe",
            "--bind",
            "daily=installed-model",
        ],
        Some(&home),
        None,
    )
    .await;
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "--no-probe：引擎一个请求都没收到"
    );
    assert_eq!(o.json["mode"], "adopt");
    assert_eq!(o.json["auth"], "unknown");
    assert_eq!(o.json["models"], json!([]));
    assert_eq!(o.json["bindings"], json!({"daily": "installed-model"}));
    idoris_router::components::load_components(&out, false).unwrap();
    // 引擎自己的模型不算数：对照的是本机已装的。
    let o = cli(
        &[
            "--out-dir",
            tmp.path().join("comp2").to_str().unwrap(),
            "--endpoint",
            &server.uri(),
            "--no-probe",
            "--bind",
            "daily=engine-only-model",
        ],
        Some(&home),
        None,
    )
    .await;
    assert_eq!(err_code(&o), "model_not_installed");
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// 托管模式里，绑定的模型没装、端口被占都排在「找引擎」之前失败：不依赖这台机器上有没有 oMLX，也不执行任何东西。
#[tokio::test(flavor = "multi_thread")]
async fn managed_fails_on_an_uninstalled_model_or_a_busy_port_before_looking_for_the_engine() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let out = tmp.path().join("comp");
    let o = cli(
        &[
            "--out-dir",
            out.to_str().unwrap(),
            "--managed",
            "--bind",
            "daily=nope",
        ],
        Some(&home),
        None,
    )
    .await;
    assert_eq!(err_code(&o), "model_not_installed");
    assert!(!out.exists());

    let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = held.local_addr().unwrap().port().to_string();
    let o = cli(
        &[
            "--out-dir",
            out.to_str().unwrap(),
            "--managed",
            "--managed-port",
            &port,
        ],
        Some(&home),
        None,
    )
    .await;
    assert_eq!(err_code(&o), "port_in_use");
    assert!(o.json["message"].as_str().unwrap().contains(&port));
    assert!(!out.exists());
    drop(held);
}

/// stdin 上的 key 行不合规（含空白）：只给固定的错误码与说明，key 的内容不出现在任何输出里；
/// 这一步排在一切探测与写入之前。
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_key_line_is_rejected_without_being_echoed() {
    let server = fake(Some(KEY), &["m1"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("comp");
    let secretish = "not-a-key with spaces and secret-part-zzz";
    let o = setup(
        &server.uri(),
        &out,
        &["--api-key-stdin"],
        Some(&format!("{secretish}\n")),
    )
    .await;
    assert_eq!(err_code(&o), "invalid_api_key_input");
    for part in ["not-a-key", "spaces", "secret-part-zzz"] {
        assert!(
            !o.stdout.contains(part) && !o.stderr.contains(part),
            "{part}"
        );
    }
    assert!(!out.exists());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "key 行不合规：连探测都不发"
    );
}

/// 托管引擎的模型目录（运行用户的 `~/.omlx/models`）路径含逗号：oMLX 会把 `--model-dir` 按逗号拆开，
/// 托管在找端口、找引擎之前就失败（所以不依赖这台机器上有没有 oMLX，也不执行任何东西）。
#[tokio::test(flavor = "multi_thread")]
async fn managed_with_a_comma_in_the_models_path_is_refused_before_looking_for_the_engine() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home,with,commas");
    fs::create_dir_all(&home).unwrap();
    let out = tmp.path().join("comp");
    let o = cli(
        &["--out-dir", out.to_str().unwrap(), "--managed"],
        Some(&home),
        None,
    )
    .await;
    assert_eq!(err_code(&o), "validation_failed");
    assert!(o.json["message"].as_str().unwrap().contains("逗号"));
    assert!(!out.exists());
}
