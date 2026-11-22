//! #626 的 Codex 预评审四点中与落盘事务有关的三点：
//!
//! - 备份不再放在用户目录里：用户目录里名字像备份的文件（`omlx.yaml.idoris-bak-<pid>`）是被保留的文件，成功与失败都原样不变；
//! - 落盘失败的回滚尽量完整，没能放回的文件要说清楚现在在哪；
//! - 校验之后、落盘之前，被保留的卡被改动（改内容、加卡、切换符号链接、mtime 变化）整体放弃，目录不变；
//!   并发的 setup 经同一把目录锁排队；最终集合里的默认卡至多一张。
//!
//! 库级测试，用落盘事务的测试挂钩（`CommitHook`）在关键点注入改动：不起进程、不连网。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::LoadMode;
use idoris_router::cli::{SetupOptions, SetupTarget};
use idoris_router::components::load_components;
use idoris_router::setup_engine::{
    CommitHook, CommitPhase, SetupError, lock_out_dir, run_setup, write_added, write_added_hooked,
    write_validated_hooked, write_validated_with,
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
        self.run_in(&self.out(), endpoint, force, hook).await
    }

    /// 同 `run`，out-dir 由调用方给（例如指向 `out()` 的符号链接）。
    async fn run_in(
        &self,
        out_dir: &Path,
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
            out_dir: out_dir.to_path_buf(),
            bindings: Vec::new(),
            load_mode: LoadMode::OnDemand,
            api_key_stdin: false,
            force,
        };
        run_setup(&setup, &options, None).await
    }

    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        snapshot(&self.out())
    }

    /// 暂存目录（含备份子目录）不该留下。
    fn assert_no_stage_left(&self) {
        let parent: Vec<_> = fs::read_dir(self.work.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(parent, ["comp"], "暂存目录没有清理");
    }
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let bytes = fs::read(e.path()).unwrap_or_default();
            (e.file_name().into_string().unwrap(), bytes)
        })
        .collect()
}

fn hook(f: impl Fn(CommitPhase, &Path) + Send + Sync + 'static) -> CommitHook {
    Arc::new(f)
}

/// 与 `setup upstream` 生成的卡同形状；`extra` 是接在 `extensions` 后面的缩进行。
fn upstream_card(id: &str, port: u16, extra: &str) -> String {
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
{extra}"#
    )
}

async fn seeded_with_kept_card(env: &Env) {
    env.run(ENDPOINT, false, None).await.unwrap();
    fs::write(
        env.out().join("lmstudio.yaml"),
        upstream_card("lmstudio", 1234, ""),
    )
    .unwrap();
}

fn pid_backup_names() -> [String; 2] {
    let pid = std::process::id();
    [
        format!("omlx.yaml.idoris-bak-{pid}"),
        format!("subscription.yaml.idoris-bak-{pid}"),
    ]
}

// ---------------------------------------------------------------------------------------------
// 一、备份撞名
// ---------------------------------------------------------------------------------------------

/// 用户目录里已经有 `omlx.yaml.idoris-bak-<本进程 pid>`（上次崩溃或 pid 复用留下的）：成功路径上它原样保留，不被覆盖、不被删除。
#[tokio::test]
async fn a_file_named_like_an_old_backup_survives_a_successful_force_run() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    let names = pid_backup_names();
    for name in &names {
        fs::write(env.out().join(name), format!("precious {name}")).unwrap();
    }
    env.run("http://127.0.0.1:8001", true, None).await.unwrap();
    let after = env.snapshot();
    for name in &names {
        assert_eq!(
            after[name],
            format!("precious {name}").into_bytes(),
            "{name}"
        );
    }
    assert!(String::from_utf8_lossy(&after["omlx.yaml"]).contains("127.0.0.1:8001"));
    load_components(&env.out(), false).unwrap();
    env.assert_no_stage_left();
}

/// 同一个撞名文件，在校验失败与落盘失败（回滚成功）两条失败路径上也原样不变，目录其余部分也不变。
#[tokio::test]
async fn a_file_named_like_an_old_backup_survives_both_failure_paths() {
    // 失败一：保留的卡与新卡 provider.id 冲突，整体校验失败。
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::write(
        env.out().join("clash.yaml"),
        upstream_card("omlx", 1235, ""),
    )
    .unwrap();
    let names = pid_backup_names();
    for name in &names {
        fs::write(env.out().join(name), format!("precious {name}")).unwrap();
    }
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert_eq!(env.snapshot(), before);
    env.assert_no_stage_left();

    // 失败二：旧文件已挪走之后，暂存目录里的新文件被拿掉，放入失败；回滚把旧文件全部放回。
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    for name in &names {
        fs::write(env.out().join(name), format!("precious {name}")).unwrap();
    }
    let before = env.snapshot();
    let sabotage = hook(|phase, out| {
        if phase == CommitPhase::BackedUp {
            remove_from_stage(out, "subscription.yaml");
        }
    });
    let error = env
        .run("http://127.0.0.1:8001", true, Some(sabotage))
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_failed", "{}", error.message);
    assert!(!error.message.contains("回滚没有完成"), "{}", error.message);
    assert_eq!(env.snapshot(), before, "回滚后目录与失败前逐字节相同");
    env.assert_no_stage_left();
}

/// 找到本次运行的暂存目录（与 out-dir 同级的 `.idoris-setup-*`），把里面的文件拿掉，造出「放入阶段失败」。
fn remove_from_stage(out: &Path, name: &str) {
    for entry in fs::read_dir(out.parent().unwrap()).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".idoris-setup-")
        {
            let _ = fs::remove_file(path.join(name));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 二、回滚做不完时说清楚文件在哪
// ---------------------------------------------------------------------------------------------

/// 放入阶段有人在 out-dir 里占了名字：新文件放不进去，旧文件也没法放回（不覆盖占位的文件）。错误说明点出每个没放回的
/// 文件的原位置与现在的位置；占位的文件不被覆盖；说明里的路径上确实有旧内容；暂存目录（连同备份）保留着。
#[tokio::test]
async fn a_rollback_that_cannot_finish_says_where_every_file_is() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    let old_main = fs::read(env.out().join("omlx.yaml")).unwrap();
    let intruder = hook(|phase, out| {
        if phase == CommitPhase::BackedUp {
            fs::write(out.join("omlx.yaml"), "someone else wrote this").unwrap();
        }
    });
    let error = env
        .run("http://127.0.0.1:8001", true, Some(intruder))
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_failed");
    let message = error.message.clone();
    assert!(message.contains("回滚没有完成"), "{message}");
    assert!(message.contains("omlx.yaml"), "{message}");
    // 占位的文件没有被覆盖。
    assert_eq!(
        fs::read(env.out().join("omlx.yaml")).unwrap(),
        b"someone else wrote this"
    );
    // 说明里给的备份路径上，是原来的内容。
    let marker = "它的原内容在 ";
    let start = message.find(marker).unwrap() + marker.len();
    let backup_path = message[start..]
        .split(['；', '\n'])
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert!(backup_path.contains(".idoris-backup"), "{backup_path}");
    assert_eq!(fs::read(&backup_path).unwrap(), old_main);
    // 保留的卡没碰。
    assert_eq!(
        fs::read(env.out().join("lmstudio.yaml")).unwrap(),
        upstream_card("lmstudio", 1234, "").into_bytes()
    );
    // 事务目录只有属主能进。
    let backup_dir = Path::new(&backup_path).parent().unwrap();
    assert_eq!(
        fs::metadata(backup_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// out-dir 变成只读（放不进、也放不回）：每个旧文件都在说明里点了名，恢复后内容完整。
#[tokio::test]
async fn a_read_only_directory_mid_commit_reports_the_backups() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    let old_main = fs::read(env.out().join("omlx.yaml")).unwrap();
    let old_sub = fs::read(env.out().join("subscription.yaml")).unwrap();
    let lock_down = hook(|phase, out| {
        if phase == CommitPhase::BackedUp {
            fs::set_permissions(out, fs::Permissions::from_mode(0o555)).unwrap();
        }
    });
    let result = env
        .run("http://127.0.0.1:8001", true, Some(lock_down))
        .await;
    fs::set_permissions(env.out(), fs::Permissions::from_mode(0o755)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.code, "write_failed");
    assert!(error.message.contains("回滚没有完成"), "{}", error.message);
    for (name, content) in [("omlx.yaml", &old_main), ("subscription.yaml", &old_sub)] {
        let line = error
            .message
            .split('；')
            .find(|part| part.contains(&format!("原有的 {name}")))
            .unwrap_or_else(|| panic!("{}", error.message));
        let path = line.split("它的原内容在 ").nth(1).unwrap().trim();
        assert_eq!(&fs::read(path).unwrap(), content, "{name}");
    }
}

// ---------------------------------------------------------------------------------------------
// 三、校验之后、落盘之前，被保留的卡被改动
// ---------------------------------------------------------------------------------------------

async fn abort_case(inject: impl Fn(&Path) + Send + Sync + 'static, expect_name: &str) {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::write(
        env.out().join("target.txt"),
        upstream_card("target", 1300, ""),
    )
    .unwrap();
    symlink("target.txt", env.out().join("link.yaml")).unwrap();
    let injector = hook(move |phase, out| {
        if phase == CommitPhase::Validated {
            inject(out);
        }
    });
    let error = env
        .run("http://127.0.0.1:8001", true, Some(injector))
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(
        error.message.contains(expect_name),
        "{expect_name}: {}",
        error.message
    );
    // 目录里 oMLX 自己的文件没被替换（仍是 8088），也没有新文件落下来。
    let after = env.snapshot();
    assert!(
        String::from_utf8_lossy(&after["omlx.yaml"]).contains("127.0.0.1:8088"),
        "旧的 oMLX 卡必须还在"
    );
    env.assert_no_stage_left();
}

#[tokio::test]
async fn a_kept_card_modified_after_validation_aborts_the_run() {
    abort_case(
        |out| fs::write(out.join("lmstudio.yaml"), "provider: [totally, broken").unwrap(),
        "lmstudio.yaml",
    )
    .await;
}

#[tokio::test]
async fn a_card_added_after_validation_aborts_the_run() {
    abort_case(
        |out| fs::write(out.join("late.yaml"), upstream_card("late", 1400, "")).unwrap(),
        "late.yaml",
    )
    .await;
}

#[tokio::test]
async fn a_kept_symlink_retargeted_after_validation_aborts_the_run() {
    abort_case(
        |out| {
            fs::write(out.join("other.txt"), upstream_card("other", 1500, "")).unwrap();
            fs::remove_file(out.join("link.yaml")).unwrap();
            symlink("other.txt", out.join("link.yaml")).unwrap();
        },
        "link.yaml",
    )
    .await;
}

/// 内容没变、只有 mtime 变了（有人 `touch` 了它）也算改动：这是「意外改动」的最小信号，宁可让用户重跑一次。
#[tokio::test]
async fn a_touched_kept_card_aborts_the_run() {
    abort_case(
        |out| {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(out.join("lmstudio.yaml"))
                .unwrap();
            file.set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
                .unwrap();
        },
        "lmstudio.yaml",
    )
    .await;
}

/// 大小、mtime、inode 都没变，只有内容变了（同样长度的改写，再把 mtime 改回去）：只有内容哈希能发现。
#[tokio::test]
async fn a_same_size_same_mtime_content_change_is_caught_by_the_content_hash() {
    abort_case(
        |out| {
            let path = out.join("lmstudio.yaml");
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            let text = fs::read_to_string(&path).unwrap().replace("1234", "4321");
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            std::io::Write::write_all(&mut &file, text.as_bytes()).unwrap();
            file.set_modified(mtime).unwrap();
        },
        "lmstudio.yaml",
    )
    .await;
}

/// 一个被保留的卡被同内容的新文件替换（换了 inode）：身份变了，放弃。
#[tokio::test]
async fn a_kept_card_replaced_by_an_identical_new_file_aborts_the_run() {
    abort_case(
        |out| {
            let path = out.join("lmstudio.yaml");
            let content = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            fs::write(&path, content).unwrap();
        },
        "lmstudio.yaml",
    )
    .await;
}

/// 对照：不是卡的文件（没有 `.yaml` 扩展名）在这期间出现，不影响校验过的集合，不放弃。
#[tokio::test]
async fn a_non_card_file_appearing_does_not_abort() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    let injector = hook(|phase, out| {
        if phase == CommitPhase::Validated {
            fs::write(out.join("notes.txt"), "hi").unwrap();
        }
    });
    env.run("http://127.0.0.1:8001", true, Some(injector))
        .await
        .unwrap();
    assert_eq!(env.snapshot()["notes.txt"], b"hi");
    env.assert_no_stage_left();
}

/// 对照：什么也没动，同样的流程成功（钩子本身不改变结果）。
#[tokio::test]
async fn an_untouched_directory_commits_with_the_hook_installed() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    env.run("http://127.0.0.1:8001", true, Some(hook(|_, _| {})))
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&env.snapshot()["omlx.yaml"]).contains("8001"));
}

/// `setup upstream` 一侧同样：现有的卡在校验之后被改，放弃，目录不变。
#[test]
fn setup_upstream_side_aborts_when_a_kept_card_changes_after_validation() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("kept.yaml"), upstream_card("kept", 1234, "")).unwrap();
    let new_text = upstream_card("fresh", 1235, "");
    let card: ComponentCard = serde_yaml::from_str(&new_text).unwrap();
    let injector = hook(|phase, out| {
        if phase == CommitPhase::Validated {
            fs::write(out.join("kept.yaml"), upstream_card("kept", 9999, "")).unwrap();
        }
    });
    let error = write_added_hooked(&out, false, "fresh.yaml", &new_text, &card, Some(&injector))
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(error.message.contains("kept.yaml"), "{}", error.message);
    assert!(!out.join("fresh.yaml").exists());
}

// ---------------------------------------------------------------------------------------------
// 四、保留的符号链接与目录（与现有行为一致，钉住）
// ---------------------------------------------------------------------------------------------

/// 指向卡的符号链接：加载器跟随链接读它，所以它按卡参与整体校验；链接本身原样保留（不被替换成普通文件）。
#[tokio::test]
async fn a_kept_symlink_to_a_card_is_validated_like_a_card_and_stays_a_symlink() {
    let env = Env::new();
    env.run(ENDPOINT, false, None).await.unwrap();
    fs::write(
        env.out().join("target.yaml"),
        upstream_card("target", 1300, ""),
    )
    .unwrap();
    symlink("target.yaml", env.out().join("link.yaml")).unwrap();
    // 链接与目标同一个 provider.id：整体校验看到了链接背后的卡，重复，失败。
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    // 对照：链接指向一个不叫 .yaml 的文件，里面是另一张合法的卡。加载器读得到它（跟随链接），整体校验通过，链接原样保留。
    fs::remove_file(env.out().join("link.yaml")).unwrap();
    fs::remove_file(env.out().join("target.yaml")).unwrap();
    fs::write(
        env.out().join("elsewhere.txt"),
        upstream_card("real", 1301, ""),
    )
    .unwrap();
    symlink("elsewhere.txt", env.out().join("alias.yaml")).unwrap();
    env.run("http://127.0.0.1:8001", true, None).await.unwrap();
    assert_eq!(
        fs::read_link(env.out().join("alias.yaml")).unwrap(),
        Path::new("elsewhere.txt")
    );
}

/// 断掉的符号链接、名字像卡的目录：读不出内容，不是能校验的卡，整体失败（`validation_failed`，点名），目录不变。
#[tokio::test]
async fn a_dangling_symlink_or_a_directory_named_like_a_card_fails_validation_and_changes_nothing()
{
    for (label, make) in [
        (
            "dangling",
            Box::new(|out: &Path| symlink("nowhere.yaml", out.join("dangling.yaml")).unwrap())
                as Box<dyn Fn(&Path)>,
        ),
        (
            "directory",
            Box::new(|out: &Path| fs::create_dir(out.join("dir.yaml")).unwrap()),
        ),
    ] {
        let env = Env::new();
        env.run(ENDPOINT, false, None).await.unwrap();
        make(&env.out());
        let before: Vec<_> = fs::read_dir(env.out())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let main_before = fs::read(env.out().join("omlx.yaml")).unwrap();
        let error = env
            .run("http://127.0.0.1:8001", true, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.code, "validation_failed",
            "{label}: {}",
            error.message
        );
        let after: Vec<_> = fs::read_dir(env.out())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            before.iter().collect::<std::collections::BTreeSet<_>>(),
            after.iter().collect::<std::collections::BTreeSet<_>>(),
            "{label}"
        );
        assert_eq!(fs::read(env.out().join("omlx.yaml")).unwrap(), main_before);
        env.assert_no_stage_left();
    }
}

// ---------------------------------------------------------------------------------------------
// 五、并发的 setup 经同一把锁排队
// ---------------------------------------------------------------------------------------------

fn finishes_only_after_release(run: impl FnOnce() + Send + 'static, out: &Path) {
    let lock = lock_out_dir(out).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        run();
        tx.send(()).unwrap();
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "拿着目录锁的时候，另一次 setup 不该完成"
    );
    drop(lock);
    rx.recv_timeout(Duration::from_secs(20))
        .expect("放开锁之后另一次 setup 应当完成");
    handle.join().unwrap();
}

#[test]
fn setup_omlx_style_writes_wait_for_the_directory_lock() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    let target = out.clone();
    finishes_only_after_release(
        move || {
            write_validated_with(
                &target,
                true,
                &[("a.yaml", upstream_card("a", 1, "").as_str())],
                &[],
            )
            .unwrap();
        },
        &out,
    );
    assert!(out.join("a.yaml").exists());
}

#[test]
fn setup_upstream_style_writes_wait_for_the_same_lock() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    let target = out.clone();
    let text = upstream_card("b", 2, "");
    let card: ComponentCard = serde_yaml::from_str(&text).unwrap();
    finishes_only_after_release(
        move || {
            write_added(&target, false, "b.yaml", &text, &card).unwrap();
        },
        &out,
    );
    assert!(out.join("b.yaml").exists());
}

/// out-dir 还不存在时锁它的父目录：同样排队，并且不在用户目录里留下锁文件。
#[test]
fn a_not_yet_existing_out_dir_is_serialised_through_its_parent_and_leaves_no_lock_file() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    let target = out.clone();
    finishes_only_after_release(
        move || {
            write_validated_with(
                &target,
                false,
                &[("a.yaml", upstream_card("a", 1, "").as_str())],
                &[],
            )
            .unwrap();
        },
        &out,
    );
    let mut names: Vec<_> = fs::read_dir(work.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["comp"]);
    assert_eq!(
        fs::read_dir(&out).unwrap().count(),
        1,
        "out-dir 里只有写入的那张卡"
    );
}

// ---------------------------------------------------------------------------------------------
// 六、最终集合里的默认卡至多一张
// ---------------------------------------------------------------------------------------------

/// 目录里已经有两张默认卡（手写的），这次新写的卡不是默认：照样 `validation_failed`，什么也不写。
#[test]
fn two_kept_default_cards_plus_a_non_default_new_card_is_refused_and_nothing_is_written() {
    let work = TempDir::new().unwrap();
    let out = work.path().join("comp");
    fs::create_dir_all(&out).unwrap();
    let default = "  idoris_default: true\n";
    fs::write(out.join("d1.yaml"), upstream_card("d1", 1, default)).unwrap();
    fs::write(out.join("d2.yaml"), upstream_card("d2", 2, default)).unwrap();
    let before = snapshot(&out);
    let fresh = upstream_card("fresh", 3, "");

    let error =
        write_validated_with(&out, true, &[("fresh.yaml", fresh.as_str())], &[]).unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert!(
        error.message.contains("d1") && error.message.contains("d2"),
        "{}",
        error.message
    );
    assert_eq!(snapshot(&out), before);

    let card: ComponentCard = serde_yaml::from_str(&fresh).unwrap();
    let error = write_added(&out, false, "fresh.yaml", &fresh, &card).unwrap_err();
    assert_eq!(error.code, "validation_failed");
    assert_eq!(snapshot(&out), before);
    assert_eq!(
        fs::read_dir(work.path()).unwrap().count(),
        1,
        "暂存目录已清理"
    );

    // 对照：只有一张默认卡，新卡不是默认，放行。
    fs::remove_file(out.join("d2.yaml")).unwrap();
    write_validated_hooked(&out, true, &[("fresh.yaml", fresh.as_str())], &[], None).unwrap();
}

// ---------------------------------------------------------------------------------------------
// 七、#633 评审的后续（N1 到 N4）
// ---------------------------------------------------------------------------------------------

/// N1：被保留的符号链接指向这次要删除的旧伴生卡（`alias.yaml -> omlx-embed.yaml`）：暂存目录里放的是解引用后的内容，
/// 校验会过，落盘后链接悬空。整体 `validation_failed`，点名链接与目标，目录不变。对照：链接指向不受影响的文件，照常。
#[tokio::test]
async fn a_kept_symlink_into_a_file_that_the_run_deletes_is_refused() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::write(
        env.out().join("omlx-embed.yaml"),
        upstream_card("omlx-embed", 1111, ""),
    )
    .unwrap();
    symlink("omlx-embed.yaml", env.out().join("alias.yaml")).unwrap();
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(
        error.message.contains("alias.yaml") && error.message.contains("omlx-embed.yaml"),
        "{}",
        error.message
    );
    assert_eq!(env.snapshot(), before);
    assert_eq!(
        fs::read_link(env.out().join("alias.yaml")).unwrap(),
        Path::new("omlx-embed.yaml")
    );
    env.assert_no_stage_left();

    // 对照：同样的链接指向一个不被替换的文件（不叫 .yaml），旧伴生卡被清掉，链接不悬空。
    fs::remove_file(env.out().join("alias.yaml")).unwrap();
    fs::write(
        env.out().join("kept-copy.txt"),
        upstream_card("copy", 1112, ""),
    )
    .unwrap();
    symlink("kept-copy.txt", env.out().join("alias.yaml")).unwrap();
    env.run("http://127.0.0.1:8001", true, None).await.unwrap();
    assert!(!env.out().join("omlx-embed.yaml").exists());
    assert!(
        fs::metadata(env.out().join("alias.yaml")).is_ok(),
        "链接没有悬空"
    );
}

/// N2：受管名字上是用户的目录（或符号链接）：`--force` 不挪、不删，`out_dir_not_empty`，目录里的内容原样。
#[tokio::test]
async fn force_never_moves_or_deletes_a_user_directory_at_a_managed_name() {
    for managed in ["omlx.yaml", "subscription.yaml"] {
        let env = Env::new();
        seeded_with_kept_card(&env).await;
        fs::remove_file(env.out().join(managed)).unwrap();
        fs::create_dir(env.out().join(managed)).unwrap();
        fs::write(
            env.out().join(managed).join("precious.txt"),
            "do not delete",
        )
        .unwrap();
        let error = env
            .run("http://127.0.0.1:8001", true, None)
            .await
            .unwrap_err();
        assert_eq!(
            error.code, "out_dir_not_empty",
            "{managed}: {}",
            error.message
        );
        assert!(error.message.contains(managed), "{}", error.message);
        assert_eq!(
            fs::read(env.out().join(managed).join("precious.txt")).unwrap(),
            b"do not delete",
            "{managed}"
        );
        env.assert_no_stage_left();
    }
    // 受管名字上是符号链接：同样不替换（链接与它的目标都原样）。
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::write(env.out().join("real.txt"), "target").unwrap();
    fs::remove_file(env.out().join("omlx.yaml")).unwrap();
    symlink("real.txt", env.out().join("omlx.yaml")).unwrap();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "out_dir_not_empty", "{}", error.message);
    assert_eq!(
        fs::read_link(env.out().join("omlx.yaml")).unwrap(),
        Path::new("real.txt")
    );
    assert_eq!(fs::read(env.out().join("real.txt")).unwrap(), b"target");
}

fn filesystem_is_case_insensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe.tmp");
    fs::write(&probe, b"x").unwrap();
    let insensitive = dir.join("caseprobe.tmp").exists();
    fs::remove_file(&probe).unwrap();
    insensitive
}

/// N3：用户的 `OMLX.yaml` 与要生成的 `omlx.yaml` 只差大小写。大小写不敏感的文件系统（macOS 默认的 APFS）上它们是同一个路径，
/// 不能悄悄换掉用户的文件；所以按不分大小写比较，撞了就 `validation_failed`，点名两个文件名，目录不变。大小写敏感的文件系统
/// （Linux CI）上它们是两个文件，同样拒绝，让同一个目录在不同机器上行为一致；只有「它们其实是同一个文件」这条断言
/// 要先在运行时探测文件系统，不是大小写不敏感的就跳过那一条。
#[tokio::test]
async fn a_user_file_differing_only_in_case_from_a_managed_name_is_refused() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::remove_file(env.out().join("omlx.yaml")).unwrap();
    let user_card = upstream_card("user", 1113, "").into_bytes();
    fs::write(env.out().join("OMLX.yaml"), &user_card).unwrap();
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert!(
        error.message.contains("OMLX.yaml") && error.message.contains("omlx.yaml"),
        "{}",
        error.message
    );
    assert_eq!(env.snapshot(), before);
    if filesystem_is_case_insensitive(env.work.path()) {
        // 同一个路径：用 `omlx.yaml` 读到的就是用户的字节，说明没有被生成的卡盖掉。
        assert_eq!(fs::read(env.out().join("omlx.yaml")).unwrap(), user_card);
    }
    env.assert_no_stage_left();
}

/// N4：目录非空，没有任何组件卡，也没有这次要替换的文件：`--force` 也不放行，说明点出多半是 `--out-dir` 写错了；
/// 目录原样。对照：有别的卡的目录，`--force` 照常。
#[tokio::test]
async fn force_refuses_a_non_empty_directory_with_no_cards_and_says_it_may_be_the_wrong_one() {
    let env = Env::new();
    fs::create_dir_all(env.out()).unwrap();
    fs::write(env.out().join("notes.txt"), "my notes").unwrap();
    fs::write(env.out().join("photo.jpg"), "jpg").unwrap();
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "out_dir_not_empty", "{}", error.message);
    assert!(error.message.contains("写错"), "{}", error.message);
    assert_eq!(env.snapshot(), before);
    env.assert_no_stage_left();

    // 对照：同一个目录里有一张别的卡，`--force` 照常（这是 #626 的用法）。
    fs::write(
        env.out().join("lmstudio.yaml"),
        upstream_card("lmstudio", 1234, ""),
    )
    .unwrap();
    env.run("http://127.0.0.1:8001", true, None).await.unwrap();
    assert_eq!(env.snapshot()["notes.txt"], b"my notes");
}

// ---------------------------------------------------------------------------------------------
// 八、#633 后续评审：大小写撞名在 setup upstream 一侧、符号链接各种形状、文件系统自己的折叠
// ---------------------------------------------------------------------------------------------

fn no_setup_leftovers(work: &Path) {
    let left: Vec<_> = fs::read_dir(work)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(".idoris-"))
        .collect();
    assert!(left.is_empty(), "暂存目录没有清理：{left:?}");
}

/// `setup upstream` 一侧（`write_added`）：已有的文件与要写的文件只差大小写，两个方向、带不带 `--force`，一律
/// `validation_failed`，目录一个字节不变。快照只排除名字完全相同的条目，所以只差大小写的那张卡不会被悄悄漏掉：
/// 大小写敏感的文件系统上它是被保留的卡、要参与校验，大小写不敏感的文件系统上它与新文件是同一个文件。
#[test]
fn write_added_refuses_a_name_that_differs_only_in_case_in_both_directions() {
    for (existing, new) in [("LM.yaml", "lm.yaml"), ("lm.yaml", "LM.yaml")] {
        for force in [false, true] {
            let work = TempDir::new().unwrap();
            let out = work.path().join("comp");
            fs::create_dir_all(&out).unwrap();
            fs::write(out.join(existing), upstream_card("lm", 1, "")).unwrap();
            let before = snapshot(&out);
            let text = upstream_card("lm2", 2, "");
            let card: ComponentCard = serde_yaml::from_str(&text).unwrap();
            let error = write_added(&out, force, new, &text, &card).unwrap_err();
            assert_eq!(
                error.code, "validation_failed",
                "{existing}/{new} force={force}: {}",
                error.message
            );
            assert!(
                error.message.contains(existing),
                "{existing}/{new}: {}",
                error.message
            );
            assert_eq!(snapshot(&out), before, "{existing}/{new} force={force}");
            no_setup_leftovers(work.path());
        }
    }
}

/// 文件系统自己的折叠（Codex 追问的 APFS Unicode 情形）：APFS 把 `ſubscription.yaml`（U+017F，长 s）与
/// `subscription.yaml` 当成同一个文件，ASCII 比较抓不到，所以撞名检查还比较两个路径背后的文件身份（设备与 inode）。
/// 先在运行时探测这个文件系统是否真的这样折叠，不是（例如 Linux 的 ext4、tmpfs）就跳过。受管名字都是 ASCII，
/// 没有 NFC 与 NFD 两种写法的问题；这里验证的是折叠本身。
#[tokio::test]
async fn a_name_the_filesystem_folds_onto_a_managed_name_is_refused() {
    let probe = TempDir::new().unwrap();
    let variant = "\u{17f}ubscription.yaml";
    fs::write(probe.path().join(variant), "x").unwrap();
    if !probe.path().join("subscription.yaml").exists() {
        eprintln!("skipped: this filesystem does not fold U+017F onto s");
        return;
    }
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::remove_file(env.out().join("subscription.yaml")).unwrap();
    let user_card = upstream_card("user", 1114, "").into_bytes();
    fs::write(env.out().join(variant), &user_card).unwrap();
    let before = env.snapshot();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert_eq!(env.snapshot(), before);
    env.assert_no_stage_left();
}

type MakeLink = fn(&Env, &str);

/// 在 `env.out()` 里造一个叫 `alias.yaml` 的符号链接，最终指向 `target`（`env.out()` 下的一个文件名）。
fn link_forms() -> Vec<(&'static str, MakeLink)> {
    vec![
        ("direct relative", |env, t| {
            symlink(t, env.out().join("alias.yaml")).unwrap();
        }),
        ("two hops in the same directory", |env, t| {
            symlink(t, env.out().join("hop.txt")).unwrap();
            symlink("hop.txt", env.out().join("alias.yaml")).unwrap();
        }),
        ("two hops through another directory", |env, t| {
            let ext = env.work.path().join("ext");
            fs::create_dir_all(&ext).unwrap();
            symlink(format!("../comp/{t}"), ext.join("hop.txt")).unwrap();
            symlink("../ext/hop.txt", env.out().join("alias.yaml")).unwrap();
        }),
        ("absolute target", |env, t| {
            symlink(env.out().join(t), env.out().join("alias.yaml")).unwrap();
        }),
        ("target containing ..", |env, t| {
            symlink(format!("../comp/{t}"), env.out().join("alias.yaml")).unwrap();
        }),
        (
            "absolute target through a symlinked path of the out-dir",
            |env, t| {
                let via = env.work.path().join("comp-via");
                symlink(env.out(), &via).unwrap();
                symlink(via.join(t), env.out().join("alias.yaml")).unwrap();
            },
        ),
    ]
}

/// N1 参数化：被保留的链接（各种写法）最终指向这次要删除（旧伴生卡）或要替换（`omlx.yaml`）的文件：整体
/// `validation_failed`，点名链接；原文件与链接都原样，暂存目录清理干净。
#[tokio::test]
async fn kept_links_of_every_shape_into_a_deleted_or_replaced_file_are_refused() {
    for target in ["omlx-embed.yaml", "omlx.yaml"] {
        for (label, make) in link_forms() {
            let env = Env::new();
            seeded_with_kept_card(&env).await;
            fs::write(
                env.out().join("omlx-embed.yaml"),
                upstream_card("omlx-embed", 1111, ""),
            )
            .unwrap();
            make(&env, target);
            let before = env.snapshot();
            let link_before = fs::read_link(env.out().join("alias.yaml")).unwrap();
            let error = env
                .run("http://127.0.0.1:8001", true, None)
                .await
                .unwrap_err();
            assert_eq!(
                error.code, "validation_failed",
                "{label} -> {target}: {}",
                error.message
            );
            assert!(
                error.message.contains("alias.yaml"),
                "{label} -> {target}: {}",
                error.message
            );
            assert_eq!(env.snapshot(), before, "{label} -> {target}");
            assert_eq!(
                fs::read_link(env.out().join("alias.yaml")).unwrap(),
                link_before,
                "{label} -> {target}"
            );
            no_setup_leftovers(env.work.path());
        }
    }
}

/// 同上，out-dir 本身是通过符号链接给出的（`--out-dir <指向目录的链接>`）。
#[tokio::test]
async fn a_kept_link_is_refused_when_the_out_dir_itself_is_given_through_a_symlink() {
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::write(
        env.out().join("omlx-embed.yaml"),
        upstream_card("omlx-embed", 1111, ""),
    )
    .unwrap();
    symlink("omlx-embed.yaml", env.out().join("alias.yaml")).unwrap();
    let via = env.work.path().join("comp-via");
    symlink(env.out(), &via).unwrap();
    let before = env.snapshot();
    let error = env
        .run_in(&via, "http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    assert_eq!(env.snapshot(), before);
    assert_eq!(
        fs::read_link(env.out().join("alias.yaml")).unwrap(),
        Path::new("omlx-embed.yaml")
    );
    no_setup_leftovers(env.work.path());
}

/// 对照：不该被拒绝的链接：指向子目录里同名的文件（还在这个目录之内）；
/// 另有加载器的口径：`.yml` 是卡（被拒），`.YML` 不是卡（加载器不读它，快照也不看它，所以不拒绝）。
/// 指向目录之外的链接现在是被拒的（只保证当前目录，见下一条测试）。
#[tokio::test]
async fn links_that_do_not_reach_a_replaced_file_pass_and_yml_follows_the_loader() {
    let valid_external: Vec<(&str, MakeLink)> =
        vec![("same file name in a sub directory", |env, t| {
            let sub = env.out().join("sub");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join(t), upstream_card("sub", 1602, "")).unwrap();
            symlink(format!("sub/{t}"), env.out().join("alias.yaml")).unwrap();
        })];
    for (label, make) in valid_external {
        let env = Env::new();
        seeded_with_kept_card(&env).await;
        make(&env, "omlx-embed.yaml");
        let link = fs::read_link(env.out().join("alias.yaml")).unwrap();
        env.run("http://127.0.0.1:8001", true, None)
            .await
            .unwrap_or_else(|e| panic!("{label}: {}", e.message));
        assert_eq!(
            fs::read_link(env.out().join("alias.yaml")).unwrap(),
            link,
            "{label}"
        );
        no_setup_leftovers(env.work.path());
    }

    // `.yml` 是加载器读的扩展名：同样被拒。
    let env = Env::new();
    seeded_with_kept_card(&env).await;
    fs::write(
        env.out().join("omlx-embed.yaml"),
        upstream_card("omlx-embed", 1111, ""),
    )
    .unwrap();
    symlink("omlx-embed.yaml", env.out().join("alias.yml")).unwrap();
    let error = env
        .run("http://127.0.0.1:8001", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "validation_failed", "{}", error.message);
    // `.YML` 不是加载器读的扩展名（区分大小写），不是卡：不拒绝；跑完旧伴生卡被清掉。
    fs::remove_file(env.out().join("alias.yml")).unwrap();
    symlink("omlx-embed.yaml", env.out().join("alias.YML")).unwrap();
    assert!(
        load_components(&env.out(), false)
            .unwrap()
            .iter()
            .all(|c| c.provider.id != "alias")
    );
    env.run("http://127.0.0.1:8001", true, None).await.unwrap();
    assert!(!env.out().join("omlx-embed.yaml").exists());
}

/// 决定（Codex 审计）：只保证当前目录。指向目录之外的卡符号链接（绝对、相对、经另一个目录）在 setup 一侧是
/// `validation_failed`，说明点名链接与它指向的位置，目录不变；对照：目录之内的链接不受影响（上一条测试）。
#[tokio::test]
async fn a_card_symlink_that_points_outside_the_directory_is_refused_by_setup() {
    let forms: Vec<(&str, MakeLink)> = vec![
        ("absolute external target", |env, _| {
            let ext = env.work.path().join("external");
            fs::create_dir_all(&ext).unwrap();
            fs::write(ext.join("card.txt"), upstream_card("ext", 1601, "")).unwrap();
            symlink(ext.join("card.txt"), env.out().join("alias.yaml")).unwrap();
        }),
        ("relative external target", |env, _| {
            let ext = env.work.path().join("external");
            fs::create_dir_all(&ext).unwrap();
            fs::write(ext.join("card.txt"), upstream_card("ext", 1601, "")).unwrap();
            symlink("../external/card.txt", env.out().join("alias.yaml")).unwrap();
        }),
    ];
    for (label, make) in forms {
        let env = Env::new();
        seeded_with_kept_card(&env).await;
        make(&env, "unused");
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
        assert!(
            error.message.contains("目录之外"),
            "{label}: {}",
            error.message
        );
        assert!(
            error.message.contains("alias.yaml"),
            "{label}: {}",
            error.message
        );
        assert_eq!(env.snapshot(), before, "{label}");
        no_setup_leftovers(env.work.path());
    }
}
