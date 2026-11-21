//! IDORIS-CORE S4：`setup` 的通用流程不认识任何具体的引擎。
//!
//! 这里的假引擎（种类 `fakeserve`）与 oMLX 没有任何共同代码：自己的探测接口（`GET /fake/ids`）、自己的卡文件名、
//! 自己的候选端口、自己的配方（可执行文件叫 `fakeserve`）、自己的安装说明。`run_setup` 一行不改照样走完采纳、
//! 自动探测与托管三条路径，输出与写出的卡里一个「oMLX」字样都没有：多引擎的抽象（规范 §2.12.1）是真的，
//! 不是把 oMLX 写死在通用路径里再换个名字。托管一个本版本不认识的引擎种类，卡在落盘之前就被引擎身份校验拒绝
//! （规范 §2.3：`idoris_managed.engine` 不认识按卡校验失败），也是通用流程自己的守卫。
//!
//! 假引擎用 wiremock 与临时目录里的假可执行文件；不启动任何推理进程，不连外网。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::LoadMode;
use idoris_router::cli::{ManagedOptions, SetupOptions, SetupTarget};
use idoris_router::setup_engine::{
    FindFailure, FoundExecutable, Probed, SetupEngine, SetupError, recipe_backed, run_setup,
};
use idoris_upstream::engine::{
    DiscoveryPlan, EngineKind, EngineProbe, EngineRecipe, LaunchError, LaunchInput, LaunchPlan,
    probe_http,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FAKE_CARD: &str = r#"
provider:
  id: fakeserve
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:9"
version_pin: "fakeserve@1"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: on_demand, keepalive: { idle_ttl_s: 300 }, admission: requires_eviction }
"#;

#[derive(Debug)]
struct FakeRecipe {
    search_dir: PathBuf,
}

#[async_trait]
impl EngineRecipe for FakeRecipe {
    fn kind(&self) -> EngineKind {
        EngineKind::parse("fakeserve").unwrap()
    }

    fn discovery_plan(&self) -> DiscoveryPlan {
        DiscoveryPlan {
            search_dirs: vec![self.search_dir.clone()],
            binary_names: vec!["fakeserve".to_owned()],
            ..DiscoveryPlan::default()
        }
    }

    fn launch_plan(&self, _input: &LaunchInput<'_>) -> Result<LaunchPlan, LaunchError> {
        Ok(LaunchPlan::default())
    }

    async fn probe_ready(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> EngineProbe {
        probe_http(
            client,
            &format!("{endpoint}/fake/ids"),
            bearer,
            timeout,
            |_| true,
        )
        .await
    }
}

struct FakeSetup {
    recipe: FakeRecipe,
    ports: Vec<u16>,
    installed: Vec<String>,
    kind: &'static str,
    version_pin: &'static str,
    /// 模板自己带了 `idoris_managed` 声明。
    declares_management: bool,
    /// 这个平台上不能托管（`discover_executable` 报 `Unsupported`）。
    unsupported: bool,
    /// `managed_models_dir` 报错的原因（`None` = 给出 `<search_dir>/models`）。
    models_dir_error: Option<&'static str>,
    /// `discover_executable` 被调用的次数。
    discover_calls: AtomicUsize,
    /// 匿名探测被拒（`auth_required`），带 key 才通过。
    wants_key: bool,
    /// 带 key 的探测的次数。
    keyed_probes: AtomicUsize,
}

impl FakeSetup {
    fn recipe_dir(&self) -> &Path {
        &self.recipe.search_dir
    }

    fn new(search_dir: &Path, ports: Vec<u16>) -> Self {
        Self {
            recipe: FakeRecipe {
                search_dir: search_dir.to_owned(),
            },
            ports,
            installed: vec!["fake-model".to_owned()],
            kind: "fakeserve",
            version_pin: "fakeserve@1",
            declares_management: false,
            unsupported: false,
            models_dir_error: None,
            discover_calls: AtomicUsize::new(0),
            wants_key: false,
            keyed_probes: AtomicUsize::new(0),
        }
    }
}

impl SetupEngine for FakeSetup {
    fn kind(&self) -> &str {
        self.kind
    }

    fn display_name(&self) -> &'static str {
        "FakeServe"
    }

    fn install_hint(&self) -> &'static str {
        "run the fakeserve installer first"
    }

    fn key_env(&self) -> &'static str {
        "FAKESERVE_API_KEY"
    }

    async fn locate_executable(&self) -> Option<PathBuf> {
        recipe_backed::locate(&self.recipe, None).await
    }

    async fn discover_executable(&self) -> Result<FoundExecutable, FindFailure> {
        self.discover_calls.fetch_add(1, Ordering::SeqCst);
        if self.unsupported {
            return Err(FindFailure::Unsupported);
        }
        recipe_backed::discover(&self.recipe, None).await
    }

    fn auto_ports(&self) -> &[u16] {
        &self.ports
    }

    fn installed_models(&self) -> Option<Vec<String>> {
        Some(self.installed.clone())
    }

    fn managed_models_dir(&self) -> Result<PathBuf, SetupError> {
        match self.models_dir_error {
            Some(why) => Err(SetupError::new("validation_failed", why)),
            None => Ok(self.recipe_dir().join("models")),
        }
    }

    async fn probe(
        &self,
        endpoint: &str,
        key: Option<&str>,
        _lifecycle: bool,
    ) -> Result<Probed, SetupError> {
        if key.is_some() {
            self.keyed_probes.fetch_add(1, Ordering::SeqCst);
        }
        if self.wants_key && key.is_none() {
            return Err(SetupError::new("auth_required", "FakeServe wants a key"));
        }
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = client
            .get(format!("{endpoint}/fake/ids"))
            .send()
            .await
            .map_err(|_| SetupError::new("unreachable", "cannot connect"))?;
        if !response.status().is_success() {
            return Err(SetupError::new("not_fakeserve", "not a FakeServe"));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| SetupError::new("not_fakeserve", "not a FakeServe"))?;
        let models = body["ids"]
            .as_array()
            .ok_or_else(|| SetupError::new("not_fakeserve", "not a FakeServe"))?
            .iter()
            .filter_map(|id| id.as_str().map(str::to_owned))
            .collect();
        Ok(Probed {
            models,
            auth_required: false,
        })
    }

    fn card_file(&self) -> &'static str {
        "fakeserve.yaml"
    }

    fn card_template(&self) -> Result<ComponentCard, SetupError> {
        let mut card: ComponentCard = serde_yaml::from_str(FAKE_CARD).unwrap();
        card.version_pin = self.version_pin.to_owned();
        if self.declares_management {
            card.extensions = json!({
                "_degradation": "d",
                "idoris_managed": {"engine": "omlx", "mode": "auto"}
            })
            .as_object()
            .cloned();
        }
        Ok(card)
    }
}

async fn fake_server(ids: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fake/ids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ids": ids})))
        .mount(&server)
        .await;
    server
}

fn options(out: &Path, target: SetupTarget) -> SetupOptions {
    SetupOptions {
        target,
        out_dir: out.to_owned(),
        bindings: Vec::new(),
        load_mode: LoadMode::OnDemand,
        api_key_stdin: false,
        force: false,
    }
}

fn closed_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 报告与写出的全部文件里，一个「oMLX」字样都没有（不分大小写）。
fn assert_no_omlx(report: &Value, out: &Path) {
    let mut text = report.to_string().to_lowercase();
    for entry in std::fs::read_dir(out).unwrap() {
        text.push_str(
            &std::fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .to_lowercase(),
        );
    }
    assert!(!text.contains("omlx"), "通用路径里漏出了 oMLX：{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn adopting_an_endpoint_uses_the_engines_probe_card_and_file_name() {
    let server = fake_server(&["fake-model", "other"]).await;
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("out");
    let engine = FakeSetup::new(tmp.path(), vec![]);
    let mut opts = options(
        &out,
        SetupTarget::Endpoint {
            url: server.uri(),
            probe: true,
        },
    );
    opts.bindings = vec![("daily".into(), "fake-model".into())];
    let report = run_setup(&engine, &opts, None).await.unwrap();
    assert_eq!(report["mode"], "adopt");
    assert_eq!(report["models"], json!(["fake-model", "other"]));
    assert_eq!(report["engine"]["engine_kind"], "fakeserve");
    assert_eq!(
        report["engine"]["installed"], false,
        "搜索目录里没有 fakeserve"
    );
    let names: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        ["fakeserve.yaml"],
        "引擎没有声明固定文件，就只有它自己的卡"
    );
    let card = std::fs::read_to_string(out.join("fakeserve.yaml")).unwrap();
    assert!(card.contains("fakeserve@1") && card.contains("idoris_role_models"));
    assert_no_omlx(&report, &out);
    // 绑定对照的是这个引擎探测出来的列表，错误里叫它自己的名字。
    let mut opts = options(
        &tmp.path().join("out2"),
        SetupTarget::Endpoint {
            url: server.uri(),
            probe: true,
        },
    );
    opts.bindings = vec![("daily".into(), "nope".into())];
    let error = run_setup(&engine, &opts, None).await.unwrap_err();
    assert_eq!(error.code, "model_not_found");
    assert!(error.message.contains("FakeServe") && !error.message.contains("oMLX"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_uses_the_engines_candidate_ports_and_names_it_in_the_error() {
    let server = fake_server(&["fake-model"]).await;
    let tmp = TempDir::new().unwrap();
    let engine = FakeSetup::new(tmp.path(), vec![closed_port(), server.address().port()]);
    let out = tmp.path().join("out");
    let report = run_setup(
        &engine,
        &options(&out, SetupTarget::Auto { managed: None }),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        report["endpoint"],
        format!("http://127.0.0.1:{}", server.address().port())
    );
    assert_no_omlx(&report, &out);
    // 一个都没找到：错误说的是这个引擎与它的候选端口。
    let nothing = FakeSetup::new(tmp.path(), vec![closed_port(), closed_port()]);
    let error = run_setup(
        &nothing,
        &options(
            &tmp.path().join("out3"),
            SetupTarget::Auto { managed: None },
        ),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "unreachable");
    assert!(error.message.contains("FakeServe") && !error.message.contains("oMLX"));
    for port in &nothing.ports {
        assert!(
            error.message.contains(&port.to_string()),
            "{}",
            error.message
        );
    }
}

/// 通用流程里「能不能把 key 发给候选端口」是引擎的事（`verify_listener`）：没有实现它的引擎（缺省）永远确认不了监听者，
/// `--auto` 带了 key 也一次带 key 的探测都不发，报 `auth_required` 并带候选的地址（#619）。
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_cannot_verify_its_listener_never_gets_a_key_from_auto() {
    let server = fake_server(&["fake-model"]).await;
    let tmp = TempDir::new().unwrap();
    let mut engine = FakeSetup::new(tmp.path(), vec![server.address().port()]);
    engine.wants_key = true;
    let out = tmp.path().join("out");
    let mut with_key = options(&out, SetupTarget::Auto { managed: None });
    with_key.api_key_stdin = true;
    let error = run_setup(&engine, &with_key, Some("generic-secret"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "auth_required");
    assert_eq!(
        error.endpoint.as_deref(),
        Some(format!("http://127.0.0.1:{}", server.address().port()).as_str())
    );
    assert!(!error.message.contains("generic-secret"));
    assert_eq!(
        engine.keyed_probes.load(Ordering::SeqCst),
        0,
        "no probe carried the key: the engine cannot confirm who listens"
    );
    assert!(!out.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn managed_discovery_goes_through_the_engines_recipe_and_hint() {
    let tmp = TempDir::new().unwrap();
    let engine = FakeSetup::new(&tmp.path().join("empty"), vec![]);
    let out = tmp.path().join("out");
    let target = || SetupTarget::Managed(ManagedOptions::default());
    // 搜索目录里没有：用这个引擎自己的安装说明。
    let error = run_setup(&engine, &options(&out, target()), None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "engine_not_installed");
    assert!(
        error.message.contains("FakeServe")
            && error.message.contains("run the fakeserve installer first")
            && !error.message.contains("oMLX"),
        "{}",
        error.message
    );
    assert!(!out.exists());
}

/// 找得到可执行文件之后，托管一个本版本不认识的引擎种类：卡在落盘之前就被引擎身份校验拒绝，什么也不写。
#[tokio::test(flavor = "multi_thread")]
async fn managing_an_engine_kind_this_version_does_not_know_is_refused_before_anything_is_written()
{
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("fakeserve");
    std::fs::write(&exe, "#!/bin/sh\necho 1.0\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let engine = FakeSetup::new(&bin, vec![]);
    let out = tmp.path().join("out");
    let error = run_setup(
        &engine,
        &options(&out, SetupTarget::Managed(ManagedOptions::default())),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("fakeserve"), "{}", error.message);
    assert!(!out.exists());
    assert_eq!(
        std::fs::read_dir(tmp.path()).unwrap().count(),
        1,
        "只有 bin：暂存目录没有留下"
    );
}

/// 引擎给的卡模板与它自己的种类不一致（`version_pin` 的产品名不是种类名）：规范 §2.3 要求 setup 生成的卡
/// 两者一致，通用流程在落盘之前核对。
#[tokio::test(flavor = "multi_thread")]
async fn a_card_template_that_disagrees_with_the_engine_kind_is_refused() {
    let server = fake_server(&["fake-model"]).await;
    let tmp = TempDir::new().unwrap();
    let mut engine = FakeSetup::new(tmp.path(), vec![]);
    engine.version_pin = "otherengine@1";
    let out = tmp.path().join("out");
    let error = run_setup(
        &engine,
        &options(
            &out,
            SetupTarget::Endpoint {
                url: server.uri(),
                probe: true,
            },
        ),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("不一致"), "{}", error.message);
    assert!(!out.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn force_replaces_only_the_files_this_engine_owns() {
    let server = fake_server(&["fake-model"]).await;
    let tmp = TempDir::new().unwrap();
    let engine = FakeSetup::new(tmp.path(), vec![]);
    let out = tmp.path().join("out");
    let adopt = || SetupTarget::Endpoint {
        url: server.uri(),
        probe: true,
    };
    run_setup(&engine, &options(&out, adopt()), None)
        .await
        .unwrap();
    let mut forced = options(&out, adopt());
    forced.force = true;
    run_setup(&engine, &forced, None).await.unwrap();
    // 另一个引擎留下的文件不在这个引擎的覆盖范围里：不带 --force 拒绝，说明里只点这个引擎自己的文件。
    std::fs::write(out.join("omlx.yaml"), "x").unwrap();
    let error = run_setup(&engine, &options(&out, adopt()), None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "out_dir_not_empty");
    assert!(
        error.message.contains("fakeserve.yaml"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("subscription.yaml"));
    // 带 --force 也不替换它（#626）：它不是合法的卡，整体校验失败，说明里点名那个文件，它原样留着。
    let error = run_setup(&engine, &forced, None).await.unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("omlx.yaml"), "{}", error.message);
    assert_eq!(std::fs::read(out.join("omlx.yaml")).unwrap(), b"x");
}

/// 卡的托管声明与这次 setup 的模式必须一致：采纳（没有 `idoris_managed`）却拿到一张自己声明了托管的模板，
/// 在落盘之前就被拒绝。（这里借用保留名 `omlx` 让声明本身合法，单独考验「托管与否」这一项核对。）
#[tokio::test(flavor = "multi_thread")]
async fn a_template_that_already_declares_management_cannot_be_adopted() {
    let server = fake_server(&["fake-model"]).await;
    let tmp = TempDir::new().unwrap();
    let mut engine = FakeSetup::new(tmp.path(), vec![]);
    engine.kind = "omlx";
    engine.version_pin = "omlx@1";
    engine.declares_management = true;
    let out = tmp.path().join("out");
    let error = run_setup(
        &engine,
        &options(
            &out,
            SetupTarget::Endpoint {
                url: server.uri(),
                probe: true,
            },
        ),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(error.message.contains("托管声明"), "{}", error.message);
    assert!(!out.exists());
}

/// 这个平台不能托管引擎进程：说清楚是平台的原因，不是「没装」。
#[tokio::test(flavor = "multi_thread")]
async fn a_platform_that_cannot_manage_engines_says_so() {
    let tmp = TempDir::new().unwrap();
    let mut engine = FakeSetup::new(tmp.path(), vec![]);
    engine.unsupported = true;
    let out = tmp.path().join("out");
    let error = run_setup(
        &engine,
        &options(&out, SetupTarget::Managed(ManagedOptions::default())),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "engine_not_installed");
    assert!(
        error.message.contains("不能由 iDoris 托管"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("installer"),
        "不是没装：{}",
        error.message
    );
    assert!(!out.exists());
}

/// 托管引擎拉起时要显式传模型目录：这个目录取不到或引擎吃不了，托管在找引擎之前就拒绝，
/// 引擎的发现一次也没有被调用；采纳一个端点不拉起引擎，用不着这个目录。
#[tokio::test(flavor = "multi_thread")]
async fn a_managed_engine_without_a_usable_model_directory_is_refused_before_it_is_looked_for() {
    let server = fake_server(&["fake-model"]).await;
    let tmp = TempDir::new().unwrap();
    let mut engine = FakeSetup::new(tmp.path(), vec![]);
    engine.models_dir_error = Some("the models directory cannot be used");
    let out = tmp.path().join("out");
    let error = run_setup(
        &engine,
        &options(&out, SetupTarget::Managed(ManagedOptions::default())),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(
        error.message.contains("models directory"),
        "{}",
        error.message
    );
    assert_eq!(engine.discover_calls.load(Ordering::SeqCst), 0);
    assert!(!out.exists());
    // 采纳一个端点：照常。
    let adopt_out = tmp.path().join("adopt");
    run_setup(
        &engine,
        &options(
            &adopt_out,
            SetupTarget::Endpoint {
                url: server.uri(),
                probe: true,
            },
        ),
        None,
    )
    .await
    .unwrap();
    assert!(adopt_out.join("fakeserve.yaml").exists());
}
