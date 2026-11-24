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

#[cfg(feature = "dev-mock")]
#[path = "support/child_guard.rs"]
mod child_guard;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::LoadMode;
use idoris_router::cli::{SetupOptions, SetupTarget};
use idoris_router::components::{
    LoadError, LoadPhase, canonical_components_dir, load_components_locked,
    load_components_locked_canonical, load_components_locked_with_hook, parse_lock_wait_ms,
};
use idoris_router::lifecycle::{EngineBackend, OmlxBackend, plan_managed, plan_managed_with};
use idoris_router::preflight::validate_card_set;
use idoris_router::readiness::plan_upstreams;
use idoris_router::runtime::RuntimeRegistry;
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

/// H1：serve 在等 `idoris setup` 放开组件目录的锁（最长 30 秒，可配到 10 分钟）时收到停止信号：等锁的每一轮（20 毫秒）看一眼取消标志，
/// 看到就返回 `Cancelled`（不是 `DirLocked`，没有读卡），不等够时限。锁一放开、能拿到的时候不看取消标志：读几个文件的事，读到一半放弃没有意义，
/// 信号由调用方在读完之后处理。
#[test]
fn a_serve_read_that_waits_for_the_lock_gives_up_when_the_stop_signal_arrives() {
    use idoris_router::components::{canonical_components_dir, load_components_locked_cancellable};
    use std::sync::atomic::{AtomicBool, Ordering};

    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("a.yaml"), upstream_card("a", 1)).unwrap();
    let canonical = canonical_components_dir(&out).unwrap();
    let held = lock_out_dir(&out).unwrap();

    let raised = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let error = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            raised.store(true, Ordering::SeqCst);
        });
        load_components_locked_cancellable(&canonical, false, Duration::from_secs(3), None, &|| {
            raised.load(Ordering::SeqCst)
        })
        .unwrap_err()
    });
    assert!(matches!(error, LoadError::Cancelled), "{error:?}");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(100), "{waited:?}");
    assert!(
        waited < Duration::from_millis(2500),
        "cut short, not run to the 3 s limit: {waited:?}"
    );

    // 锁放开之后：已经置位的取消标志不再拦住读卡（拿到锁就读完）。
    drop(held);
    let cards = load_components_locked_cancellable(
        &canonical,
        false,
        Duration::from_secs(60),
        None,
        &|| true,
    )
    .unwrap();
    assert_eq!(cards.len(), 1);
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

// ---------------------------------------------------------------------------------------------
// Codex 对 M2/M3 的预评审（#1 到 #9）
// ---------------------------------------------------------------------------------------------

/// 另开一个 fd 试独占锁（非阻塞）：被挡住（`EWOULDBLOCK`）就说明目录上有别的持有者。
fn exclusive_is_blocked(dir: &Path) -> bool {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    match Flock::lock(
        fs::File::open(dir).unwrap(),
        FlockArg::LockExclusiveNonblock,
    ) {
        Ok(_) => false,
        Err((_, Errno::EWOULDBLOCK)) => true,
        Err((_, other)) => panic!("{other}"),
    }
}

fn ids_of(cards: &[ComponentCard]) -> Vec<String> {
    let mut ids: Vec<_> = cards.iter().map(|c| c.provider.id.clone()).collect();
    ids.sort();
    ids
}

/// #1：目录不存在或不是目录：立刻 `NotADirectory`，不拿锁、不读卡（挂钩一次都不触发）；之后目录建出来了，
/// 同样的调用才进入「拿锁再读」。不存在「先看一眼不存在、再看一眼又存在」的缝。
#[test]
fn a_missing_or_non_directory_path_fails_at_once_and_never_reaches_loading() {
    let work = TempDir::new().unwrap();
    let missing = work.path().join("comp");
    let phases: Mutex<Vec<LoadPhase>> = Mutex::new(Vec::new());
    let record = |phase: LoadPhase, _: &Path| phases.lock().unwrap().push(phase);
    for path in [missing.clone(), {
        let file = work.path().join("file");
        fs::write(&file, "x").unwrap();
        file
    }] {
        let error = canonical_components_dir(&path).unwrap_err();
        assert!(matches!(error, LoadError::NotADirectory(_)), "{error:?}");
        let error = load_components_locked(&path, false, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(error, LoadError::NotADirectory(_)), "{error:?}");
    }
    assert!(phases.lock().unwrap().is_empty());
    fs::create_dir_all(&missing).unwrap();
    fs::write(missing.join("a.yaml"), upstream_card("a", 1)).unwrap();
    let canonical = canonical_components_dir(&missing).unwrap();
    let cards =
        load_components_locked_with_hook(&canonical, false, Duration::from_secs(1), Some(&record))
            .unwrap();
    assert_eq!(ids_of(&cards), ["a"]);
    assert_eq!(
        *phases.lock().unwrap(),
        [LoadPhase::Opened, LoadPhase::Locked]
    );
}

/// #1：每一条走到读卡的成功路径都持有共享锁：读卡开始的那一刻（`Locked` 挂钩里）另一个 fd 拿不到独占锁。
#[test]
fn the_shared_lock_is_held_whenever_loading_starts() {
    let work = TempDir::new().unwrap();
    fs::write(work.path().join("a.yaml"), upstream_card("a", 1)).unwrap();
    let canonical = canonical_components_dir(work.path()).unwrap();
    let blocked_when_locked = Mutex::new(None);
    let blocked_when_opened = Mutex::new(None);
    let probe = |phase: LoadPhase, dir: &Path| match phase {
        LoadPhase::Opened => *blocked_when_opened.lock().unwrap() = Some(exclusive_is_blocked(dir)),
        LoadPhase::Locked => *blocked_when_locked.lock().unwrap() = Some(exclusive_is_blocked(dir)),
    };
    load_components_locked_with_hook(&canonical, false, Duration::from_secs(1), Some(&probe))
        .unwrap();
    assert_eq!(
        *blocked_when_opened.lock().unwrap(),
        Some(false),
        "加锁之前没有人持有"
    );
    assert_eq!(
        *blocked_when_locked.lock().unwrap(),
        Some(true),
        "读卡时共享锁在手"
    );
    assert!(!exclusive_is_blocked(work.path()), "读完之后锁放开了");
}

/// #1：首次发布紧接着再更新：一个线程对还不存在的目录 `setup`（第一次发布）并立刻再 `setup --force`（更新），
/// 读者反复读：每次要么是 `NotADirectory`（还没发布），要么读到完整的集合（旧的或新的），绝不是中间状态。
#[test]
fn a_first_publish_followed_at_once_by_an_update_is_never_read_half_way() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    let target = out.clone();
    let writer = std::thread::spawn(move || {
        let first = upstream_card("first", 1);
        write_validated_with(
            &target,
            false,
            &[
                ("a.yaml", first.as_str()),
                ("b.yaml", first.replace("first", "second").as_str()),
            ],
            &[],
        )
        .unwrap();
        for round in 0..20u16 {
            let a = upstream_card("first", 10 + round);
            let b = upstream_card("second", 100 + round);
            write_validated_with(
                &target,
                true,
                &[("a.yaml", a.as_str()), ("b.yaml", b.as_str())],
                &[],
            )
            .unwrap();
        }
    });
    let mut complete = 0;
    for _ in 0..600 {
        match load_components_locked(&out, false, Duration::from_secs(20)) {
            Err(LoadError::NotADirectory(_)) => {}
            Ok(cards) => {
                assert_eq!(ids_of(&cards), ["first", "second"], "读到了中间状态");
                complete += 1;
            }
            Err(other) => panic!("{other:?}"),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    writer.join().unwrap();
    assert!(complete > 0, "读者至少读到过一次发布之后的完整集合");
}

/// #3：启动时只确定一次 canonical 路径。`current` 指向 v1，canonical 取到 v1 之后 `current` 被切到 v2：加锁、读卡用的
/// 都还是 v1（读到 v1 的卡；v1 上有共享锁，v2 上没有）；读卡时 canonical 路径就是发布给 launcher 的那一个（同一个值）。
#[test]
fn the_lock_the_cards_and_the_published_path_all_follow_the_directory_resolved_once() {
    use std::os::unix::fs::symlink;
    let work = TempDir::new().unwrap();
    let (v1, v2) = (work.path().join("v1"), work.path().join("v2"));
    fs::create_dir_all(&v1).unwrap();
    fs::create_dir_all(&v2).unwrap();
    fs::write(v1.join("c.yaml"), upstream_card("from-v1", 1)).unwrap();
    fs::write(v2.join("c.yaml"), upstream_card("from-v2", 2)).unwrap();
    let current = work.path().join("current");
    symlink(&v1, &current).unwrap();
    let canonical = canonical_components_dir(&current).unwrap();
    assert_eq!(canonical, fs::canonicalize(&v1).unwrap());
    // 解析之后、读卡之前，`current` 被切到 v2。
    fs::remove_file(&current).unwrap();
    symlink(&v2, &current).unwrap();
    let seen = Mutex::new(Vec::new());
    let probe = |phase: LoadPhase, dir: &Path| {
        if phase == LoadPhase::Locked {
            seen.lock().unwrap().push((
                dir.to_path_buf(),
                exclusive_is_blocked(&v1),
                exclusive_is_blocked(&v2),
            ));
        }
    };
    let cards =
        load_components_locked_with_hook(&canonical, false, Duration::from_secs(1), Some(&probe))
            .unwrap();
    assert_eq!(ids_of(&cards), ["from-v1"]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, canonical, "挂钩看到的就是确定过的那个路径");
    assert!(seen[0].1, "锁在 v1 上");
    assert!(!seen[0].2, "v2 上没有锁");
}

/// #3：读卡相对锁住的目录 fd（`openat`）：拿到锁之后目录本身被改名，读到的还是加锁的那个目录里的卡。
#[test]
fn cards_are_read_through_the_locked_directory_descriptor_even_if_the_directory_is_renamed() {
    let work = TempDir::new().unwrap();
    let dir = work.path().join("comp");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.yaml"), upstream_card("a", 1)).unwrap();
    let canonical = canonical_components_dir(&dir).unwrap();
    let moved = work.path().join("moved");
    let rename = |phase: LoadPhase, _: &Path| {
        if phase == LoadPhase::Locked {
            fs::rename(&dir, &moved).unwrap();
            // 原路径上换成另一个目录，里面是别的卡。
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("a.yaml"), upstream_card("impostor", 9)).unwrap();
        }
    };
    let cards =
        load_components_locked_with_hook(&canonical, false, Duration::from_secs(1), Some(&rename))
            .unwrap();
    assert_eq!(
        ids_of(&cards),
        ["a"],
        "读的是加锁的那个目录，不是原路径上后来冒出来的"
    );
}

/// #4：只保证当前目录。serve 读卡时，指向目录之外的符号链接是 `Invalid`（点名链接与目标位置）；
/// 目录之内的链接照常读；断链与名字像卡的目录读不出内容。
#[test]
fn serve_refuses_card_symlinks_outside_the_directory_and_reads_the_ones_inside() {
    use std::os::unix::fs::symlink;
    let work = TempDir::new().unwrap();
    let dir = work.path().join("comp");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("real.txt"), upstream_card("inside", 1)).unwrap();
    symlink("real.txt", dir.join("link.yaml")).unwrap();
    let cards = load_components_locked(&dir, false, Duration::from_secs(1)).unwrap();
    assert_eq!(ids_of(&cards), ["inside"], "目录之内的链接照常读");
    fs::remove_file(dir.join("link.yaml")).unwrap();

    let external = work.path().join("external.txt");
    fs::write(&external, upstream_card("outside", 2)).unwrap();
    for (label, target) in [
        ("absolute", external.clone()),
        ("relative", PathBuf::from("../external.txt")),
    ] {
        symlink(&target, dir.join("out.yaml")).unwrap();
        let error = load_components_locked(&dir, false, Duration::from_secs(1)).unwrap_err();
        assert!(
            matches!(error, LoadError::Invalid { .. }),
            "{label}: {error:?}"
        );
        let text = error.to_string();
        assert!(
            text.contains("out.yaml") && text.contains("目录之外"),
            "{label}: {text}"
        );
        fs::remove_file(dir.join("out.yaml")).unwrap();
    }
    symlink("nowhere.txt", dir.join("dangling.yaml")).unwrap();
    assert!(matches!(
        load_components_locked(&dir, false, Duration::from_secs(1)).unwrap_err(),
        LoadError::Io { .. }
    ));
    fs::remove_file(dir.join("dangling.yaml")).unwrap();
    fs::create_dir(dir.join("dir.yaml")).unwrap();
    let error = load_components_locked(&dir, false, Duration::from_secs(1)).unwrap_err();
    assert!(error.to_string().contains("不是普通文件"), "{error}");
}

/// #8：等锁的时间语义。`0` 是只试一次：没人持锁立刻成功，有人持锁立刻失败；`1` 毫秒同样很快失败；报告的毫秒数是实际等的，
/// 不是向下取整成秒；等待不会超出上限太多（睡的是剩余时间与轮询间隔里小的那个）。
#[test]
fn lock_wait_tries_once_then_waits_no_longer_than_asked_and_reports_milliseconds() {
    let work = TempDir::new().unwrap();
    fs::write(work.path().join("a.yaml"), upstream_card("a", 1)).unwrap();
    let canonical = canonical_components_dir(work.path()).unwrap();
    // 没人持锁：0 毫秒也能成功（先试一次）。
    assert_eq!(
        load_components_locked_canonical(&canonical, false, Duration::ZERO)
            .unwrap()
            .len(),
        1
    );
    let held = lock_out_dir(work.path()).unwrap();
    for (wait_ms, slack_ms) in [(0u64, 300u128), (1, 300), (150, 400)] {
        let started = std::time::Instant::now();
        let error =
            load_components_locked_canonical(&canonical, false, Duration::from_millis(wait_ms))
                .unwrap_err();
        let elapsed = started.elapsed().as_millis();
        let LoadError::DirLocked { waited_ms, .. } = error else {
            panic!("{error:?}");
        };
        assert!(
            waited_ms >= wait_ms,
            "{wait_ms}: 报告的 {waited_ms} 毫秒小于要求的等待"
        );
        assert!(
            u128::from(waited_ms) <= elapsed,
            "{wait_ms}: 报告的 {waited_ms} 毫秒超过实际用时 {elapsed}"
        );
        assert!(
            elapsed < u128::from(wait_ms) + slack_ms,
            "{wait_ms}: 实际等了 {elapsed} 毫秒，睡过头了"
        );
        assert!(error_text_has_ms(&LoadError::DirLocked {
            dir: "d".into(),
            waited_ms
        }));
    }
    drop(held);
}

fn error_text_has_ms(error: &LoadError) -> bool {
    error.to_string().contains("毫秒")
}

/// #8：`IDORIS_COMPONENTS_LOCK_WAIT_MS` 的取值：没设与空白用默认；`0`、`1`、普通数字按毫秒；过大的值按 10 分钟截断
/// （含装不进 u64 的数字）；非数字（负数、小数、带单位、乱写）是启动错误，说明里有变量名与取值。
#[test]
fn the_lock_wait_setting_has_defined_behaviour_for_every_kind_of_value() {
    use idoris_router::components::{DIR_LOCK_WAIT, DIR_LOCK_WAIT_MAX};
    for unset in [None, Some(""), Some("  ")] {
        assert_eq!(
            parse_lock_wait_ms(unset).unwrap(),
            DIR_LOCK_WAIT,
            "{unset:?}"
        );
    }
    for (raw, expected) in [
        ("0", 0u64),
        ("1", 1),
        ("250", 250),
        (" 300 ", 300),
        ("007", 7),
    ] {
        assert_eq!(
            parse_lock_wait_ms(Some(raw)).unwrap(),
            Duration::from_millis(expected),
            "{raw:?}"
        );
    }
    for huge in [
        "600001",
        "86400000",
        "99999999999999999999999999999999999999999",
    ] {
        assert_eq!(
            parse_lock_wait_ms(Some(huge)).unwrap(),
            DIR_LOCK_WAIT_MAX,
            "{huge}"
        );
    }
    assert_eq!(
        parse_lock_wait_ms(Some("600000")).unwrap(),
        DIR_LOCK_WAIT_MAX,
        "上限本身"
    );
    for bad in ["abc", "-5", "1.5", "10s", "0x10", "１２"] {
        let error = parse_lock_wait_ms(Some(bad)).unwrap_err();
        assert!(
            error.contains("IDORIS_COMPONENTS_LOCK_WAIT_MS") && error.contains(bad),
            "{bad}: {error}"
        );
    }
}

/// #9：真实的共享锁先拿在手里，另一个读者读卡：它在第一把锁放开之前就完成（共享锁互不阻塞），而不是等到放开才完成。
#[test]
fn a_second_reader_completes_while_a_first_shared_lock_is_still_held() {
    use nix::fcntl::{Flock, FlockArg};
    let work = TempDir::new().unwrap();
    fs::write(work.path().join("a.yaml"), upstream_card("a", 1)).unwrap();
    let first = Flock::lock(fs::File::open(work.path()).unwrap(), FlockArg::LockShared)
        .map_err(|(_, e)| e)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let dir = work.path().to_path_buf();
    let reader = std::thread::spawn(move || {
        tx.send(load_components_locked(&dir, false, Duration::from_secs(10)))
            .unwrap();
    });
    let result = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("第一把共享锁还在手里，第二个读者也该完成");
    assert_eq!(ids_of(&result.unwrap()), ["a"]);
    drop(first);
    reader.join().unwrap();
}

// ---------------------------------------------------------------------------------------------
// #2：preflight 与运行时对卡 `form` 的共同判断
// ---------------------------------------------------------------------------------------------

fn bundled_binary_card() -> String {
    upstream_card("odd", 1400).replace("form: http_service", "form: bundled_binary")
}

/// 一张 `form: bundled_binary` 的普通卡：运行时注册表在启动时以「没有运行时构造器」拒绝；preflight 与它同一句话；
/// setup 因此拒绝发布，目录一个字节不变。
#[tokio::test]
async fn a_card_form_the_runtime_cannot_construct_is_refused_by_setup_and_by_the_real_startup_entry()
 {
    let text = bundled_binary_card();
    let card: ComponentCard = serde_yaml::from_str(&text).unwrap();
    // 真实的启动入口（serve 构造运行时注册表用的就是它）。
    let startup =
        RuntimeRegistry::spawn_with_local_configs(std::slice::from_ref(&card), &BTreeMap::new())
            .expect_err("serve 会拒绝");
    assert!(startup.contains("no runtime constructor"), "{startup}");
    // 共享的预检给出同一句话。
    assert_eq!(validate_card_set(&[card]).unwrap_err(), startup);
    // setup：保留的卡里有这样一张，`--force` 不发布，目录不变。
    let env = Env::new();
    env.run(ENDPOINT, false, None).await.unwrap();
    fs::write(env.out().join("odd.yaml"), &text).unwrap();
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(
        error.message.contains("no runtime constructor"),
        "{}",
        error.message
    );
    assert_eq!(env.snapshot(), before);
    // setup upstream 一侧同样。
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("odd.yaml"), &text).unwrap();
    let fresh = upstream_card("fresh", 1500);
    let fresh_card: ComponentCard = serde_yaml::from_str(&fresh).unwrap();
    let error = write_added(&out, false, "fresh.yaml", &fresh, &fresh_card).unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(!out.join("fresh.yaml").exists());
}

// ---------------------------------------------------------------------------------------------
// #6：每张托管卡只解析一次 backend
// ---------------------------------------------------------------------------------------------

#[test]
fn the_managed_plan_resolves_each_card_once_and_keeps_the_validated_backend() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let one: Vec<ComponentCard> =
        vec![serde_yaml::from_str(&managed_card("m1", "http://127.0.0.1:19001")).unwrap()];
    let plans = plan_upstreams(&one).unwrap();
    let calls = AtomicUsize::new(0);
    let planned = plan_managed_with(&plans, |kind| {
        calls.fetch_add(1, Ordering::SeqCst);
        (kind == "omlx").then(|| Arc::new(OmlxBackend::from_env()) as Arc<dyn EngineBackend>)
    })
    .unwrap();
    assert_eq!(planned.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "一张卡只构造一次 backend");

    // 两张同种托管卡（会因为「每种引擎一个实例」被拒绝）：各解析一次，不是各两次。
    let two: Vec<ComponentCard> = vec![
        serde_yaml::from_str(&managed_card("m1", "http://127.0.0.1:19001")).unwrap(),
        serde_yaml::from_str(&managed_card("m2", "http://127.0.0.1:19002")).unwrap(),
    ];
    let plans = plan_upstreams(&two).unwrap();
    calls.store(0, Ordering::SeqCst);
    let error = plan_managed_with(&plans, |kind| {
        calls.fetch_add(1, Ordering::SeqCst);
        (kind == "omlx").then(|| Arc::new(OmlxBackend::from_env()) as Arc<dyn EngineBackend>)
    })
    .unwrap_err();
    assert!(error.to_string().contains("m1"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------------------------
// #7：setup 与 serve 共用 mock 开关（`dev-mock` 特性加 `IDORIS_ALLOW_MOCK=1`）
// ---------------------------------------------------------------------------------------------

/// 只在带 `dev-mock` 特性的构建里有意义（`cargo test -p idoris-router --features dev-mock --test setup_serve_consistency`）。
/// 目录里有两张同 id 的 mock 卡：没开 `IDORIS_ALLOW_MOCK` 时 serve 与 setup 都跳过它们，setup 成功；开了之后 serve 会因为
/// 重复的 id 起不来，所以 setup 也必须拒绝（`validation_failed`）。真二进制，`spawn_guarded()`。
#[cfg(feature = "dev-mock")]
#[test]
fn setup_sees_mock_cards_exactly_when_serve_does() {
    use child_guard::SpawnGuarded;
    let mock = "provider:\n  id: mock\n  family: local\n  tier: local\n  capabilities: [chat]\n  privacy_class: local_only\n  cost: { input_per_m: 0, output_per_m: 0 }\n  locality: loopback\nform: http_service\nendpoint: \"mock://in-memory\"\nversion_pin: \"mock@0.1.0\"\nprivacy_class: local_only\nallowed_egress: [none]\nfallback_policy: fail_closed\nfail_closed: true\nload_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }\n";
    let run = |allow: Option<&str>| {
        let work = TempDir::new().unwrap();
        let out = work.path().join("comp");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("m1.yaml"), mock).unwrap();
        fs::write(out.join("m2.yaml"), mock).unwrap();
        fs::write(out.join("kept.yaml"), upstream_card("kept", 1234)).unwrap();
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_idoris"));
        for (key, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("IDORIS_"))
        {
            cmd.env_remove(key);
        }
        if let Some(value) = allow {
            cmd.env("IDORIS_ALLOW_MOCK", value);
        }
        cmd.args([
            "setup",
            "omlx",
            "--endpoint",
            "http://127.0.0.1:8001",
            "--no-probe",
            "--force",
            "--out-dir",
        ])
        .arg(&out)
        .env("HOME", "/nonexistent/idoris-mock-test-home")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
        let output = cmd.spawn_guarded().unwrap().wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    };
    let (code, stdout) = run(None);
    assert_eq!(
        code,
        Some(0),
        "没开 IDORIS_ALLOW_MOCK：mock 卡被跳过，setup 成功：{stdout}"
    );
    let (code, stdout) = run(Some("1"));
    assert_eq!(
        code,
        Some(1),
        "开了：serve 会因重复的 mock id 起不来，setup 必须拒绝：{stdout}"
    );
    assert!(stdout.contains("validation_failed"), "{stdout}");
}
