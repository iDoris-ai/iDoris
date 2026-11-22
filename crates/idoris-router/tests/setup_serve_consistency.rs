//! Codex 集成审计的两条（#M2、#M3）：`setup` 与 `serve` 对同一个组件目录要看到同一个集合、同一套校验。
//!
//! - M2：serve 读卡时对组件目录拿共享 `flock`，与 `setup` 落盘时的独占锁是同一把，所以 serve 在 setup 落盘中途启动，
//!   读到的要么是完整的旧集合，要么是完整的新集合，不会是挪动到一半的中间状态；
//! - M3：`setup` 在发布之前对暂存目录里的卡集合过 serve 启动时的纯配置校验（`preflight`），setup 放行的目录 serve 不会再
//!   因为托管卡的计划（同种引擎两个实例、端口重复、endpoint 形状）退出 1。
//!
//! 库级测试，用落盘事务的测试挂钩在「旧文件已挪走、新文件还没放入」这一刻让 serve 的读卡发生：不起进程、不连网。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::LoadMode;
use idoris_router::cli::{SetupOptions, SetupTarget};
use idoris_router::components::{LoadError, load_components_locked};
use idoris_router::lifecycle::plan_managed;
use idoris_router::preflight::validate_card_set;
use idoris_router::readiness::plan_upstreams;
use idoris_router::setup_engine::{
    CommitHook, CommitPhase, SetupError, lock_out_dir, run_setup, write_added, write_validated_with,
};
use idoris_router::setup_omlx::OmlxSetup;
use serde_json::Value;
use tempfile::TempDir;

const ENDPOINT: &str = "http://127.0.0.1:8088";

struct Env {
    home: TempDir,
    work: TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            work: TempDir::new().unwrap(),
        }
    }

    fn out(&self) -> PathBuf {
        self.work.path().join("comp")
    }

    async fn run(
        &self,
        endpoint: &str,
        force: bool,
        hook: Option<CommitHook>,
    ) -> Result<Value, SetupError> {
        let mut setup = OmlxSetup::new(Some(self.home.path().to_path_buf()));
        if let Some(hook) = hook {
            setup = setup.with_commit_hook(hook);
        }
        let options = SetupOptions {
            target: SetupTarget::Endpoint {
                url: endpoint.to_string(),
                probe: false,
            },
            out_dir: self.out(),
            bindings: Vec::new(),
            load_mode: LoadMode::OnDemand,
            api_key_stdin: false,
            force,
        };
        run_setup(&setup, &options, None).await
    }

    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(self.out())
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().into_string().unwrap(),
                    fs::read(e.path()).unwrap_or_default(),
                )
            })
            .collect()
    }
}

/// loopback 的 resident 卡（`setup upstream` 生成的同形状）。
fn upstream_card(id: &str, port: u16) -> String {
    format!(
        r#"provider:
  id: {id}
  family: local
  tier: local
  capabilities:
  - chat
  privacy_class: local_only
  cost:
    input_per_m: 0
    output_per_m: 0
  locality: loopback
form: http_service
endpoint: http://127.0.0.1:{port}
version_pin: openai_compat@unknown
privacy_class: local_only
allowed_egress:
- loopback
fallback_policy: fail_closed
fail_closed: true
load_policy:
  mode: resident
  keepalive:
    pinned: true
  admission: coexist
extensions:
  _degradation: d
  idoris_upstream:
    kind: openai_compat
    flavor: openai_compat
"#
    )
}

/// 托管的 oMLX 卡：`idoris_managed`，endpoint 由调用方给（形状对不对是被测对象）。
fn managed_card(id: &str, endpoint: &str) -> String {
    format!(
        r#"provider:
  id: {id}
  family: local
  tier: local
  capabilities:
  - chat
  privacy_class: local_only
  cost:
    input_per_m: 0
    output_per_m: 0
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: omlx@0.6.4
privacy_class: local_only
allowed_egress:
- loopback
fallback_policy: fail_closed
fail_closed: true
load_policy:
  mode: on_demand
  keepalive:
    pinned: false
  admission: coexist
extensions:
  _degradation: d
  idoris_managed:
    engine: omlx
    mode: auto
"#
    )
}

// ---------------------------------------------------------------------------------------------
// M2：serve 读卡与 setup 落盘互斥
// ---------------------------------------------------------------------------------------------

/// 在 setup 落盘到一半（旧文件已挪走、新文件还没放入）时，serve 开始读卡：读卡被挡住，等 setup 落盘结束，读到的是完整的新
/// 集合（`omlx`、`subscription`、被保留的 `lmstudio`，`omlx` 的端点是新的），不是缺了文件的中间状态。
#[tokio::test]
async fn a_serve_read_that_starts_mid_commit_sees_the_complete_new_set() {
    let env = Env::new();
    env.run(ENDPOINT, false, None).await.unwrap();
    fs::write(
        env.out().join("lmstudio.yaml"),
        upstream_card("lmstudio", 1234),
    )
    .unwrap();

    type Loaded = Result<Vec<ComponentCard>, LoadError>;
    let readers: Arc<Mutex<Vec<mpsc::Receiver<Loaded>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&readers);
    let during_commit: CommitHook = Arc::new(move |phase, out: &Path| {
        if phase != CommitPhase::BackedUp {
            return;
        }
        // 此刻 out-dir 里 omlx.yaml 与 subscription.yaml 已经被挪走。serve 的读卡在另一个线程里开始。
        let (tx, rx) = mpsc::channel();
        let dir = out.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(load_components_locked(&dir, false, Duration::from_secs(20)));
        });
        // 读卡被挡住：在 setup 落盘结束之前拿不到结果。
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "setup 落盘中途，serve 的读卡不该返回"
        );
        sink.lock().unwrap().push(rx);
    });

    env.run("http://127.0.0.1:8001", true, Some(during_commit))
        .await
        .unwrap();

    let rx = readers.lock().unwrap().pop().expect("钩子里启动了读卡线程");
    let cards = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("setup 结束后读卡完成")
        .expect("读卡成功");
    let mut ids: Vec<_> = cards.iter().map(|c| c.provider.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["lmstudio", "omlx", "subscription"]);
    let omlx = cards.iter().find(|c| c.provider.id == "omlx").unwrap();
    assert!(omlx.endpoint.contains("8001"), "{}", omlx.endpoint);
}

/// setup 没在运行时，读卡立刻完成，读到的是完整的旧集合；多个读者共享锁，互不阻塞。
#[tokio::test]
async fn readers_share_the_lock_and_see_the_complete_old_set_when_no_setup_runs() {
    let env = Env::new();
    env.run(ENDPOINT, false, None).await.unwrap();
    let first = load_components_locked(&env.out(), false, Duration::from_secs(5)).unwrap();
    let second = load_components_locked(&env.out(), false, Duration::from_secs(5)).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
}

/// 另一次 setup（独占锁）还没放开：serve 的读卡等到上限，启动失败，说明点出目录与原因；放开之后同样的读卡成功。
#[test]
fn a_serve_read_times_out_with_a_clear_error_while_setup_holds_the_lock() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("a.yaml"), upstream_card("a", 1)).unwrap();
    let held = lock_out_dir(&out).unwrap();
    let error = load_components_locked(&out, false, Duration::from_millis(200)).unwrap_err();
    assert!(matches!(error, LoadError::DirLocked { .. }), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("setup") && text.contains(&out.display().to_string()),
        "{text}"
    );
    drop(held);
    assert_eq!(
        load_components_locked(&out, false, Duration::from_millis(200))
            .unwrap()
            .len(),
        1
    );
}

/// 反方向：serve 正在读（持有共享锁）时，setup 的落盘等它读完，不会在读到一半时换掉文件。
#[test]
fn setup_waits_for_a_reader_that_holds_the_shared_lock() {
    use nix::fcntl::{Flock, FlockArg};
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("kept.yaml"), upstream_card("kept", 1)).unwrap();
    let reader = Flock::lock(fs::File::open(&out).unwrap(), FlockArg::LockShared)
        .map_err(|(_, e)| e)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let target = out.clone();
    let text = upstream_card("fresh", 2);
    let handle = std::thread::spawn(move || {
        write_validated_with(&target, true, &[("fresh.yaml", text.as_str())], &[]).unwrap();
        tx.send(()).unwrap();
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "有读者持有共享锁时，setup 不该落盘"
    );
    drop(reader);
    rx.recv_timeout(Duration::from_secs(20))
        .expect("读者放开之后 setup 落盘");
    handle.join().unwrap();
    assert!(out.join("fresh.yaml").exists());
}

// ---------------------------------------------------------------------------------------------
// M3：setup 落盘前过 serve 的纯配置校验
// ---------------------------------------------------------------------------------------------

/// 目录里已有两张托管卡（同一种引擎、id 与端口都不同）：serve 在托管计划里以「同一种引擎两个实例」退出 1。
/// setup 现在在发布之前就拒绝（`validation_failed`），说明是 serve 会报的那一句，目录一个字节不变。
#[tokio::test]
async fn setup_refuses_a_set_serve_would_reject_for_two_managed_instances_of_one_engine() {
    let env = Env::new();
    env.run(ENDPOINT, false, None).await.unwrap();
    fs::write(
        env.out().join("m1.yaml"),
        managed_card("m1", "http://127.0.0.1:19001"),
    )
    .unwrap();
    fs::write(
        env.out().join("m2.yaml"),
        managed_card("m2", "http://127.0.0.1:19002"),
    )
    .unwrap();
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(
        error.message.contains("m1") && error.message.contains("m2"),
        "{}",
        error.message
    );
    assert_eq!(env.snapshot(), before);

    // 对照：只有一张托管卡，同样的命令成功。
    fs::remove_file(env.out().join("m2.yaml")).unwrap();
    env.run("http://127.0.0.1:8001", true, None).await.unwrap();
}

/// 端口重复、endpoint 形状不合（带路径、不是 127.0.0.1）：同样在 setup 一侧就拒绝。
#[tokio::test]
async fn setup_refuses_shared_ports_and_unmanageable_endpoints() {
    let cases: [(&str, [(&str, &str); 2]); 2] = [
        (
            "shared port",
            [
                ("m1", "http://127.0.0.1:19001"),
                ("m2", "http://127.0.0.1:19001"),
            ],
        ),
        (
            "unmanageable endpoint",
            [
                ("m1", "http://127.0.0.1:19001/v1"),
                ("other", "http://127.0.0.1:19003"),
            ],
        ),
    ];
    for (label, kept) in cases {
        let env = Env::new();
        env.run(ENDPOINT, false, None).await.unwrap();
        for (id, endpoint) in kept {
            // 第二张用非托管的普通卡占位，只让第一类问题单独出现。
            let text = if label == "unmanageable endpoint" && id == "other" {
                upstream_card(id, 19003)
            } else {
                managed_card(id, endpoint)
            };
            fs::write(env.out().join(format!("{id}.yaml")), text).unwrap();
        }
        let before = env.snapshot();
        let error = env
            .run("http://127.0.0.1:8001", true, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.code, "validation_failed",
            "{label}: {}",
            error.message
        );
        // 说明里是 serve 会报的那一句：端口重复与 endpoint 形状各有自己的话，不是笼统的失败。
        let expected = if label == "shared port" {
            "同一个端口"
        } else {
            "必须是 http://127.0.0.1"
        };
        assert!(
            error.message.contains(expected),
            "{label}: {}",
            error.message
        );
        assert_eq!(env.snapshot(), before, "{label}");
    }
}

/// `setup upstream` 一侧（`write_added`）同样过这道校验：目录里已经有 serve 起不来的托管卡组合，加一张无关的上游卡也不放行；
/// 去掉多余的托管卡之后放行。
#[test]
fn setup_upstream_also_runs_the_serve_preflight() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    fs::write(
        out.join("m1.yaml"),
        managed_card("m1", "http://127.0.0.1:19001"),
    )
    .unwrap();
    fs::write(
        out.join("m2.yaml"),
        managed_card("m2", "http://127.0.0.1:19002"),
    )
    .unwrap();
    let text = upstream_card("fresh", 1300);
    let card: ComponentCard = serde_yaml::from_str(&text).unwrap();
    let error = write_added(&out, false, "fresh.yaml", &text, &card).unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(!out.join("fresh.yaml").exists());
    fs::remove_file(out.join("m2.yaml")).unwrap();
    write_added(&out, false, "fresh.yaml", &text, &card).unwrap();
}

/// setup 一侧的校验与 serve 启动时的校验是同一套：对这几种坏组合，`validate_card_set` 的错误与 serve 走的
/// `plan_upstreams` 加 `plan_managed` 的错误逐字相同；好组合两边都过。
#[test]
fn the_shared_preflight_agrees_with_the_path_serve_takes() {
    let cases: Vec<(&str, Vec<String>)> = vec![
        (
            "two of a kind",
            vec![
                managed_card("m1", "http://127.0.0.1:19001"),
                managed_card("m2", "http://127.0.0.1:19002"),
            ],
        ),
        (
            "shared port",
            vec![
                managed_card("m1", "http://127.0.0.1:19001"),
                managed_card("m2", "http://127.0.0.1:19001"),
            ],
        ),
        (
            "bad endpoint",
            vec![managed_card("m1", "http://127.0.0.1:19001/v1")],
        ),
        (
            "ok",
            vec![
                managed_card("m1", "http://127.0.0.1:19001"),
                upstream_card("u", 1300),
            ],
        ),
        ("no managed", vec![upstream_card("u", 1300)]),
    ];
    for (label, texts) in cases {
        let cards: Vec<ComponentCard> = texts
            .iter()
            .map(|t| serde_yaml::from_str(t).unwrap())
            .collect();
        let serve_side = plan_upstreams(&cards)
            .map_err(|e| e.to_string())
            .and_then(|plans| plan_managed(&plans).map_err(|e| e.to_string()))
            .map(|_| ());
        assert_eq!(validate_card_set(&cards), serve_side, "{label}");
    }
}
