//! IDORIS-CORE S4：`setup omlx` 的 `--auto`、`--managed`、`--no-probe` 与离线绑定核对（规范 §2.7.8）。
//!
//! 库级测试，不起 `idoris` 进程：候选端口、托管端口范围、可执行文件、家目录都由 `OmlxSetup` 的构造器注入，所以
//! 在这台机器上有没有真实的 oMLX（端口 8088、`/Applications/oMLX.app`、`~/.omlx`）都不影响结果。
//! 全部用假 oMLX（wiremock）、假的可执行文件（临时目录里的 shell 脚本）与临时目录：**不启动任何推理进程、不连外网、
//! 不碰真实的 `~/.omlx`**。进程级的用法错误与确定性的失败路径另见 `setup_omlx.rs`。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use idoris_contracts::load_policy::LoadMode;
use idoris_router::cli::{ManagedOptions, SetupOptions, SetupTarget};
use idoris_router::engine_kind::engine_identity;
use idoris_router::setup_engine::{SetupEngine, SetupError, run_setup};
use idoris_router::setup_omlx::{AUTO_PORTS, OmlxSetup};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "omlx-secret-key-s4-zzz";

// ---------------------------------------------------------------------------------------------
// 假 oMLX 与金丝雀
// ---------------------------------------------------------------------------------------------

/// 假 oMLX：三个接口都在；给了 `key` 时只认这一个 Bearer（否则 401）。
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

/// 只有 `/v1/models`（任何 OpenAI 兼容服务都有）：不是 oMLX 的完整形状。
async fn openai_only() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}]})))
        .mount(&server)
        .await;
    server
}

fn port_of(server: &MockServer) -> u16 {
    server.address().port()
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// 一个一直在听、数连接的 TCP 端口：任何连到它的动作（不管说不说 HTTP）都会被数到。
struct Canary {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Canary {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hits.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok(_) => {
                        h.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Self {
            port,
            hits,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// 给监听线程一点时间把积压的连接数完，再读计数。
    fn hits(&self) -> usize {
        std::thread::sleep(Duration::from_millis(60));
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 一个没人听的端口（绑上又放掉）。
fn closed_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------------------------
// 台架
// ---------------------------------------------------------------------------------------------

struct Rig {
    root: TempDir,
}

impl Rig {
    fn new() -> Self {
        let rig = Self {
            root: TempDir::new().unwrap(),
        };
        std::fs::create_dir_all(rig.home()).unwrap();
        rig
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn out(&self) -> PathBuf {
        self.root.path().join("out")
    }

    fn marker(&self) -> PathBuf {
        self.root.path().join("executed")
    }

    /// 假的 `omlx-cli`：被执行一次就往标记文件里追加一行；`--version` 打印版本号。
    fn fake_executable(&self) -> PathBuf {
        let dir = self.root.path().join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("omlx-cli");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\necho ran >> '{}'\n[ \"$1\" = --version ] && echo 0.6.4-fake || exit 9\n",
                self.marker().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    fn executed(&self) -> bool {
        self.marker().exists()
    }

    /// `~/.omlx/models/<id>`：一个完整的模型目录（`config.json` 加一个权重文件）。
    fn install_model(&self, id: &str) {
        let dir = self.home().join(".omlx/models").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"model_type":"bert"}"#).unwrap();
        std::fs::write(dir.join("model.safetensors"), b"weights").unwrap();
    }

    /// 默认的 setup：家目录是台架的、候选端口是给定的、可执行文件是假的。
    fn setup(&self, auto_ports: &[u16]) -> OmlxSetup {
        OmlxSetup::new(Some(self.home()))
            .with_explicit_executable(self.fake_executable())
            .with_auto_ports(auto_ports.iter().copied())
    }

    fn options(&self, target: SetupTarget) -> SetupOptions {
        SetupOptions {
            target,
            out_dir: self.out(),
            bindings: Vec::new(),
            load_mode: LoadMode::OnDemand,
            api_key_stdin: false,
            force: false,
        }
    }

    fn card(&self) -> Value {
        let raw = std::fs::read_to_string(self.out().join("omlx.yaml")).unwrap();
        serde_yaml::from_str(&raw).unwrap()
    }

    /// 目录树快照（相对路径加文件大小），用来证明没有写到别处。
    fn tree(&self, dir: &Path) -> Vec<(String, u64)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let meta = std::fs::symlink_metadata(entry.path()).unwrap();
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .display()
                    .to_string();
                out.push((rel, if meta.is_dir() { 0 } else { meta.len() }));
                if meta.is_dir() {
                    walk(root, &entry.path(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }
}

fn auto(managed: Option<ManagedOptions>) -> SetupTarget {
    SetupTarget::Auto { managed }
}

fn managed(port: Option<u16>) -> ManagedOptions {
    ManagedOptions { port }
}

fn code(result: &Result<Value, SetupError>) -> &'static str {
    match result {
        Ok(report) => panic!("expected an error, got {report}"),
        Err(error) => error.code,
    }
}

fn ok(result: Result<Value, SetupError>) -> Value {
    match result {
        Ok(report) => report,
        Err(error) => panic!("expected success, got {} {}", error.code, error.message),
    }
}

/// 起一串连续的、此刻空闲的端口，前 `held` 个由返回的监听占着，其余留空闲。
fn consecutive_ports(count: u16, held: u16) -> (u16, Vec<TcpListener>) {
    for base in (41000u16..60000).step_by(97) {
        let mut listeners = Vec::new();
        let mut all_free = true;
        for offset in 0..count {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, base + offset)) {
                Ok(listener) => listeners.push(listener),
                Err(_) => {
                    all_free = false;
                    break;
                }
            }
        }
        if all_free {
            listeners.truncate(held as usize);
            return (base, listeners);
        }
    }
    panic!("no run of {count} consecutive free ports found");
}

// ---------------------------------------------------------------------------------------------
// --auto
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn auto_takes_the_first_candidate_in_the_specs_order_that_is_shaped_like_omlx() {
    let (first, second) = (fake(None, &["m1"]).await, fake(None, &["m2"]).await);
    let rig = Rig::new();
    let report = ok(run_setup(
        &rig.setup(&[port_of(&first), port_of(&second)]),
        &rig.options(auto(None)),
        None,
    )
    .await);
    assert_eq!(report["mode"], "adopt");
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", port_of(&first))
    );
    assert_eq!(report["models"], json!(["m1"]));
    assert_eq!(report["auth"], "none");
    assert_eq!(requests(&second).await, 0, "先到先得：后面的候选没有被问到");
    // 顺序反过来，结果也跟着反过来：取的是顺序，不是某个端口。
    let rig = Rig::new();
    let report = ok(run_setup(
        &rig.setup(&[port_of(&second), port_of(&first)]),
        &rig.options(auto(None)),
        None,
    )
    .await);
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", port_of(&second))
    );
}

/// 规范 §2.7.8 的候选端口与顺序：8088、8000、8001（Agent24 原来探测的同一组）。
#[test]
fn the_default_candidates_are_8088_8000_8001_in_that_order() {
    assert_eq!(AUTO_PORTS, [8088, 8000, 8001]);
    let setup = OmlxSetup::new(None);
    assert_eq!(setup.auto_ports(), [8088, 8000, 8001]);
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_skips_unreachable_candidates_and_ones_that_are_not_shaped_like_omlx() {
    let alien = openai_only().await;
    let omlx = fake(None, &["m1"]).await;
    let rig = Rig::new();
    // 即便 --load-mode resident（采纳单个端点时不要求生命周期接口），自动探测也要求完整的 oMLX 形状：
    // 否则任何 OpenAI 兼容服务（LM Studio 之类）占着 8088 都会被当成 oMLX。
    let mut options = rig.options(auto(None));
    options.load_mode = LoadMode::Resident;
    let report = ok(run_setup(
        &rig.setup(&[closed_port(), port_of(&alien), port_of(&omlx)]),
        &options,
        None,
    )
    .await);
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", port_of(&omlx))
    );
    assert_eq!(
        requests(&alien).await,
        2,
        "只问了 /v1/models 和一个生命周期接口"
    );
    // 对照：同一台假的 OpenAI-only 服务，作为显式端点、resident 模式，照旧被接受（既有行为不变）。
    let rig = Rig::new();
    let mut options = rig.options(SetupTarget::Endpoint {
        url: alien.uri(),
        probe: true,
    });
    options.load_mode = LoadMode::Resident;
    ok(run_setup(&rig.setup(&[]), &options, None).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_that_finds_nothing_is_unreachable_and_writes_nothing() {
    let rig = Rig::new();
    let setup = rig.setup(&[closed_port(), closed_port()]);
    let result = run_setup(&setup, &rig.options(auto(None)), None).await;
    assert_eq!(code(&result), "unreachable");
    let message = &result.as_ref().unwrap_err().message;
    assert!(message.contains("--managed"), "{message}");
    assert!(!rig.out().exists());
    assert!(!rig.executed());
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_with_managed_falls_back_to_a_managed_card_only_when_nothing_was_found() {
    let rig = Rig::new();
    let setup = rig.setup(&[closed_port()]);
    let report = ok(run_setup(&setup, &rig.options(auto(Some(managed(None)))), None).await);
    assert_eq!(report["mode"], "managed");
    assert!(rig.card()["extensions"]["idoris_managed"].is_object());
}

/// 规范 §2.7.8：`--managed` 与 `--auto` 联用且找到了外部 oMLX 时，取保守的一边：采纳，不托管。
#[tokio::test(flavor = "multi_thread")]
async fn auto_with_managed_adopts_an_external_engine_instead_of_managing_it() {
    let external = fake(None, &["m1"]).await;
    let rig = Rig::new();
    let setup = rig.setup(&[port_of(&external)]);
    let report = ok(run_setup(&setup, &rig.options(auto(Some(managed(None)))), None).await);
    assert_eq!(report["mode"], "adopt");
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", port_of(&external))
    );
    let card = rig.card();
    assert!(
        card["extensions"].is_null(),
        "采纳的卡不带 idoris_managed：{card}"
    );
    assert_eq!(card["endpoint"], report["endpoint"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_reports_a_candidate_that_wants_a_key_instead_of_pretending_nothing_is_there() {
    let protected = fake(Some(KEY), &["m1"]).await;
    let rig = Rig::new();
    let setup = rig.setup(&[port_of(&protected)]);
    // 没给 key。有 --managed 也一样：不能在一个可能是 oMLX 的服务旁边悄悄再起一个。
    for target in [auto(None), auto(Some(managed(None)))] {
        let result = run_setup(&setup, &rig.options(target), None).await;
        assert_eq!(code(&result), "auth_required");
        let message = &result.as_ref().unwrap_err().message;
        assert!(message.contains("--api-key-stdin"), "{message}");
        assert!(
            message.contains(&port_of(&protected).to_string()),
            "{message}"
        );
        assert!(!rig.out().exists());
    }
    // 给了错的 key。
    let result = run_setup(&setup, &rig.options(auto(None)), Some("wrong-key")).await;
    assert_eq!(code(&result), "auth_failed");
    let message = &result.as_ref().unwrap_err().message;
    assert!(!message.contains("wrong-key"), "{message}");
    // 给了对的 key：采纳，输出说明要鉴权，key 不落盘不回显。
    let report = ok(run_setup(&setup, &rig.options(auto(None)), Some(KEY)).await);
    assert_eq!(report["auth"], "required");
    assert_eq!(report["next"]["needs_env"], json!(["IDORIS_OMLX_API_KEY"]));
    assert!(!report.to_string().contains(KEY));
    for entry in std::fs::read_dir(rig.out()).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains(KEY));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_continues_past_a_candidate_that_wants_a_key_to_a_good_one() {
    let protected = fake(Some(KEY), &["m1"]).await;
    let open = fake(None, &["m2"]).await;
    let rig = Rig::new();
    let setup = rig.setup(&[port_of(&protected), port_of(&open)]);
    let report = ok(run_setup(&setup, &rig.options(auto(None)), None).await);
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", port_of(&open))
    );
}

/// 自动探测的端口是猜的：key 只在匿名请求被拒之后才发给那个端口，对不要求鉴权的服务一个字节都不发。
#[tokio::test(flavor = "multi_thread")]
async fn auto_never_sends_the_key_to_a_candidate_that_did_not_ask_for_one() {
    let open = fake(None, &["m1"]).await;
    let rig = Rig::new();
    let setup = rig.setup(&[port_of(&open)]);
    ok(run_setup(&setup, &rig.options(auto(None)), Some(KEY)).await);
    let seen = open.received_requests().await.unwrap();
    assert!(!seen.is_empty());
    for request in seen {
        assert!(
            request.headers.get("authorization").is_none(),
            "{} 带了 Authorization 头",
            request.url
        );
    }
}

// ---------------------------------------------------------------------------------------------
// --managed
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn managed_writes_a_card_that_serve_loads_and_reads_as_a_managed_omlx_without_connecting() {
    let canary = Canary::start();
    let rig = Rig::new();
    // 候选端口里有金丝雀：纯 --managed 不探测任何东西，更不会去连候选端口。
    let setup = rig.setup(&[canary.port]);
    let report = ok(run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await);
    assert_eq!(canary.hits(), 0);
    assert_eq!(report["status"], "ok");
    assert_eq!(report["mode"], "managed");
    assert_eq!(report["auth"], "unknown");
    assert_eq!(report["models"], json!([]));
    let endpoint = report["endpoint"].as_str().unwrap();
    let port: u16 = endpoint
        .strip_prefix("http://127.0.0.1:")
        .unwrap()
        .parse()
        .unwrap();
    assert!((19000..=19999).contains(&port), "{endpoint}");
    // 规范 §2.7.8 的 engine 对象。
    let exe = std::fs::canonicalize(rig.fake_executable()).unwrap();
    assert_eq!(
        report["engine"],
        json!({
            "engine_kind": "omlx",
            "installed": true,
            "executable": exe.display().to_string(),
            "version": "0.6.4-fake",
        })
    );
    // 卡：托管声明、版本 pin、端点，没有宿主信息（可执行文件路径、模型目录不进卡，规范 §2.3）。
    let card = rig.card();
    assert_eq!(
        card["extensions"]["idoris_managed"],
        json!({"engine": "omlx", "mode": "auto"})
    );
    assert!(
        !card["extensions"]["_degradation"]
            .as_str()
            .unwrap()
            .trim()
            .is_empty()
    );
    assert_eq!(card["version_pin"], "omlx@0.6.4");
    assert_eq!(card["endpoint"], report["endpoint"]);
    assert_eq!(card["load_policy"]["mode"], "on_demand");
    let raw = std::fs::read_to_string(rig.out().join("omlx.yaml")).unwrap();
    assert!(
        !raw.contains(&exe.display().to_string()),
        "卡里不得有可执行文件路径"
    );
    assert!(!raw.contains("model_dir") && !raw.contains("base"), "{raw}");
    // serve 用同一个 loader 加载，并按规范 §2.3 把它读成 managed 的 omlx。
    let cards = idoris_router::components::load_components(&rig.out(), false).unwrap();
    assert_eq!(cards.len(), 2);
    let omlx = cards.iter().find(|c| c.provider.id == "omlx").unwrap();
    let identity = engine_identity(omlx).unwrap();
    assert_eq!(identity.engine_kind, "omlx");
    assert!(identity.managed);
}

/// 托管只生成卡片：不拉起引擎。唯一被执行的是 `<可执行文件> --version`（取版本），而且只执行一次。
#[tokio::test(flavor = "multi_thread")]
async fn managed_only_asks_the_executable_for_its_version_and_launches_nothing() {
    let rig = Rig::new();
    ok(run_setup(
        &rig.setup(&[]),
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await);
    let runs = std::fs::read_to_string(rig.marker()).unwrap();
    assert_eq!(runs.lines().count(), 1, "{runs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_resident_card_keeps_the_pinned_keepalive_and_the_managed_declaration() {
    let rig = Rig::new();
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.load_mode = LoadMode::Resident;
    ok(run_setup(&rig.setup(&[]), &options, None).await);
    let card = rig.card();
    assert_eq!(card["load_policy"]["mode"], "resident");
    assert_eq!(card["load_policy"]["keepalive"]["pinned"], true);
    assert_eq!(card["load_policy"]["admission"], "coexist");
    assert_eq!(card["extensions"]["idoris_managed"]["engine"], "omlx");
    idoris_router::components::load_components(&rig.out(), false).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_default_managed_port_is_the_first_free_one_in_the_private_range() {
    let (base, held) = consecutive_ports(3, 2);
    let rig = Rig::new();
    let setup = rig.setup(&[]).with_managed_port_range(base..=base + 2);
    let report = ok(run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await);
    drop(held);
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", base + 2),
        "前两个被占着，取第一个空闲的"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_private_range_with_no_free_port_is_port_in_use() {
    let (base, held) = consecutive_ports(2, 2);
    let rig = Rig::new();
    let setup = rig.setup(&[]).with_managed_port_range(base..=base + 1);
    let result = run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await;
    drop(held);
    assert_eq!(code(&result), "port_in_use");
    assert!(!rig.out().exists());
    assert!(!rig.executed());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_managed_port_is_used_when_free_and_refused_when_taken() {
    let (base, held) = consecutive_ports(2, 1);
    let rig = Rig::new();
    let setup = rig.setup(&[]);
    // 被占着的那个：port_in_use，不挑别的端口顶替，什么也不写。
    let result = run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(Some(base)))),
        None,
    )
    .await;
    assert_eq!(code(&result), "port_in_use");
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .message
            .contains(&base.to_string()),
        "{:?}",
        result
    );
    assert!(!rig.out().exists());
    // 空闲的那个：就用它，哪怕它在私有范围之外。
    drop(held);
    let report = ok(run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(Some(base + 1)))),
        None,
    )
    .await);
    assert_eq!(report["endpoint"], format!("http://127.0.0.1:{}", base + 1));
    assert_eq!(rig.card()["endpoint"], report["endpoint"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn managed_without_an_installed_engine_is_engine_not_installed_with_a_human_hint() {
    let rig = Rig::new();
    let missing = rig.root.path().join("nowhere/omlx-cli");
    let setup = OmlxSetup::new(Some(rig.home())).with_explicit_executable(&missing);
    let result = run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await;
    assert_eq!(code(&result), "engine_not_installed");
    let message = &result.as_ref().unwrap_err().message;
    assert!(
        message.contains("oMLX") && message.contains("/Applications"),
        "{message}"
    );
    assert!(!rig.out().exists(), "没装引擎：什么也不写");
}

#[tokio::test(flavor = "multi_thread")]
async fn managed_with_an_executable_that_fails_validation_says_why_without_running_it() {
    let rig = Rig::new();
    // `setup()` 写出脚本；写完再去掉可执行位。
    let setup = rig.setup(&[]);
    let exe = rig.root.path().join("bin/omlx-cli");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644)).unwrap();
    let result = run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await;
    assert_eq!(code(&result), "engine_not_installed");
    let message = &result.as_ref().unwrap_err().message;
    assert!(message.contains("not_executable"), "{message}");
    assert!(!rig.executed());
    assert!(!rig.out().exists());
}

// ---------------------------------------------------------------------------------------------
// 离线：--no-probe 与托管的 --bind 对照本机已装的模型
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn offline_bindings_are_checked_against_the_installed_models_not_the_engine() {
    let rig = Rig::new();
    rig.install_model("bge-m3-mlx-fp16");
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.bindings = vec![("embed".into(), "bge-m3-mlx-fp16".into())];
    let report = ok(run_setup(&rig.setup(&[]), &options, None).await);
    assert_eq!(report["bindings"], json!({"embed": "bge-m3-mlx-fp16"}));
    let card = rig.card();
    assert_eq!(
        card["extensions"]["idoris_role_models"],
        json!({"embed": "bge-m3-mlx-fp16"})
    );
    // 托管声明和绑定共存，降级说明两样都交代了。
    assert_eq!(card["extensions"]["idoris_managed"]["engine"], "omlx");
    let degradation = card["extensions"]["_degradation"].as_str().unwrap();
    assert!(
        degradation.contains("未托管") && degradation.contains("bindings"),
        "{degradation}"
    );
    idoris_router::components::load_components(&rig.out(), false).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offline_binding_to_a_model_that_is_not_installed_is_model_not_installed() {
    let rig = Rig::new();
    rig.install_model("have");
    // 不完整的模型目录（没有权重）不算已安装。
    let partial = rig.home().join(".omlx/models/partial");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::write(partial.join("config.json"), "{}").unwrap();
    for model in ["missing", "partial"] {
        let mut options = rig.options(SetupTarget::Managed(managed(None)));
        options.bindings = vec![("daily".into(), model.into())];
        let result = run_setup(&rig.setup(&[]), &options, None).await;
        assert_eq!(code(&result), "model_not_installed", "{model}");
        assert!(!rig.out().exists());
        assert!(!rig.executed(), "绑定的核对排在引擎发现之前");
    }
    // 对照：装了的那个通过。
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.bindings = vec![("daily".into(), "have".into())];
    ok(run_setup(&rig.setup(&[]), &options, None).await);
}

/// 一个绑定对不上就整个失败，不是只检查第一个。
#[tokio::test(flavor = "multi_thread")]
async fn every_offline_binding_is_checked_not_just_the_first() {
    let rig = Rig::new();
    rig.install_model("have");
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.bindings = vec![
        ("daily".into(), "have".into()),
        ("fast".into(), "missing".into()),
    ];
    let result = run_setup(&rig.setup(&[]), &options, None).await;
    assert_eq!(code(&result), "model_not_installed");
    assert!(result.as_ref().unwrap_err().message.contains("fast"));
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_bindings_without_a_home_cannot_be_checked_and_are_refused() {
    let rig = Rig::new();
    let setup = OmlxSetup::new(None).with_explicit_executable(rig.fake_executable());
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.bindings = vec![("daily".into(), "have".into())];
    let result = run_setup(&setup, &options, None).await;
    assert_eq!(code(&result), "model_not_installed");
    assert!(result.as_ref().unwrap_err().message.contains("模型目录"));
    // 没有绑定就不需要模型目录：HOME 缺失不影响没有 --bind 的托管。
    ok(run_setup(
        &setup,
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await);
    // 家目录不是绝对路径等于没有。
    let relative = OmlxSetup::new(Some("relative/home".into()));
    assert!(relative.installed_models().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_probe_adopts_an_endpoint_without_connecting_and_checks_bindings_offline() {
    let canary = Canary::start();
    let rig = Rig::new();
    rig.install_model("have");
    let mut options = rig.options(SetupTarget::Endpoint {
        url: canary.url(),
        probe: false,
    });
    options.bindings = vec![("daily".into(), "have".into())];
    let report = ok(run_setup(&rig.setup(&[canary.port]), &options, None).await);
    assert_eq!(canary.hits(), 0, "--no-probe：一个连接都不能有");
    assert_eq!(report["mode"], "adopt");
    assert_eq!(report["endpoint"], canary.url());
    assert_eq!(report["auth"], "unknown");
    assert_eq!(report["models"], json!([]));
    assert!(rig.card()["extensions"]["idoris_managed"].is_null());
    // 对照 --bind 的核对在没有探测时看的是本机已装模型。
    options.bindings = vec![("daily".into(), "missing".into())];
    options.force = true;
    let result = run_setup(&rig.setup(&[]), &options, None).await;
    assert_eq!(code(&result), "model_not_installed");
}

// ---------------------------------------------------------------------------------------------
// engine 对象
// ---------------------------------------------------------------------------------------------

/// 采纳模式只看位置上有没有可执行文件：不执行它（没有 `version`），也不校验签名。
#[tokio::test(flavor = "multi_thread")]
async fn adopt_mode_reports_presence_without_running_the_executable() {
    let omlx = fake(None, &["m1"]).await;
    let rig = Rig::new();
    let report = ok(run_setup(
        &rig.setup(&[]),
        &rig.options(SetupTarget::Endpoint {
            url: omlx.uri(),
            probe: true,
        }),
        None,
    )
    .await);
    let exe = std::fs::canonicalize(rig.fake_executable()).unwrap();
    assert_eq!(
        report["engine"],
        json!({"engine_kind": "omlx", "installed": true, "executable": exe.display().to_string()})
    );
    assert!(report["engine"].get("version").is_none());
    assert!(!rig.executed(), "采纳不需要拉起它，也就不执行它");
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_mode_with_no_executable_reports_installed_false_and_still_succeeds() {
    let omlx = fake(None, &["m1"]).await;
    let rig = Rig::new();
    let missing = rig.root.path().join("nowhere/omlx-cli");
    let setup = OmlxSetup::new(Some(rig.home())).with_explicit_executable(missing);
    let report = ok(run_setup(
        &setup,
        &rig.options(SetupTarget::Endpoint {
            url: omlx.uri(),
            probe: true,
        }),
        None,
    )
    .await);
    assert_eq!(
        report["engine"],
        json!({"engine_kind": "omlx", "installed": false, "executable": null})
    );
    assert_eq!(report["status"], "ok");
}

/// `OmlxSetup` 用的是 S3 的 oMLX 配方（与引擎管理器共用同一份），种类名是规范的保留名 `omlx`。
#[test]
fn the_setup_engine_is_the_reserved_omlx_kind() {
    use idoris_upstream::engine::EngineKind;
    assert_eq!(OmlxSetup::new(None).kind(), EngineKind::OMLX.as_str());
}

// ---------------------------------------------------------------------------------------------
// 副作用的边界
// ---------------------------------------------------------------------------------------------

/// 规范 §2.7「组件配置」行：只写 out-dir 及其同级的 `.idoris-setup-*` 暂存目录。托管模式不例外：
/// 不建状态目录、不建引擎的 base 目录，家目录里一个字节都不多。
#[tokio::test(flavor = "multi_thread")]
async fn a_managed_setup_writes_nothing_but_the_out_dir() {
    let rig = Rig::new();
    rig.install_model("bge-m3-mlx-fp16");
    let before_home = rig.tree(&rig.home());
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.bindings = vec![("embed".into(), "bge-m3-mlx-fp16".into())];
    ok(run_setup(&rig.setup(&[]), &options, None).await);
    assert_eq!(rig.tree(&rig.home()), before_home, "家目录没有变化");
    let mut top: Vec<String> = std::fs::read_dir(rig.root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    top.sort();
    assert_eq!(top, ["bin", "executed", "home", "out"], "暂存目录已清理");
    let mut written: Vec<String> = rig.tree(&rig.out()).into_iter().map(|(p, _)| p).collect();
    written.sort();
    assert_eq!(written, ["omlx.yaml", "subscription.yaml"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn force_replaces_a_managed_card_with_an_adopted_one_and_back() {
    let omlx = fake(None, &["m1"]).await;
    let rig = Rig::new();
    ok(run_setup(
        &rig.setup(&[]),
        &rig.options(SetupTarget::Managed(managed(None))),
        None,
    )
    .await);
    // 没有 --force：已有的卡不动。
    let again = run_setup(
        &rig.setup(&[]),
        &rig.options(SetupTarget::Endpoint {
            url: omlx.uri(),
            probe: true,
        }),
        None,
    )
    .await;
    assert_eq!(code(&again), "out_dir_not_empty");
    assert!(rig.card()["extensions"]["idoris_managed"].is_object());
    // --force：整体替换成采纳的卡，托管声明没有残留。
    let mut options = rig.options(SetupTarget::Endpoint {
        url: omlx.uri(),
        probe: true,
    });
    options.force = true;
    ok(run_setup(&rig.setup(&[]), &options, None).await);
    assert!(rig.card()["extensions"].is_null());
    let mut options = rig.options(SetupTarget::Managed(managed(None)));
    options.force = true;
    ok(run_setup(&rig.setup(&[]), &options, None).await);
    assert!(rig.card()["extensions"]["idoris_managed"].is_object());
}
