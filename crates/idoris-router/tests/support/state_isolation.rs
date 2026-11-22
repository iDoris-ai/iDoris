//! 测试起的子进程用的状态目录隔离（见 `child_guard.rs` 顶部说明）。
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

/// 每个测试进程一个私有根目录（`<tmp>/idoris-test-state-<pid>`）。进程内线程共用它没有问题：这里只在首次调用时建目录，
/// 之后只读；各子进程的端点（端口）不同，围栏目录互不相干，需要固定状态目录的测试自己 `.env("IDORIS_STATE_DIR", …)`。
/// 不设置进程环境（`std::env::set_var` 在 edition 2024 是 unsafe，工作区又禁了 unsafe；改环境对并行线程也不安全）。
fn isolation_root() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::temp_dir();
        // 顺手回收一天前的旧目录（上次崩掉的测试进程留下的）；只动自己命名模式的目录，出错一律忽略。
        if let Ok(entries) = std::fs::read_dir(&base) {
            let day = std::time::Duration::from_secs(24 * 3600);
            for entry in entries.flatten() {
                let name = entry.file_name();
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > day);
                if old && name.to_string_lossy().starts_with("idoris-test-state-") {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let root = base.join(format!("idoris-test-state-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create the test state root");
        root
    })
}

/// 给要起的子进程设置隔离环境。
pub trait IsolateState {
    fn isolate_state(&mut self) -> &mut Self;
}

impl IsolateState for Command {
    fn isolate_state(&mut self) -> &mut Self {
        let explicit = |command: &Command, key: &str| {
            command
                .get_envs()
                .any(|(name, _)| name == std::ffi::OsStr::new(key))
        };
        let root = isolation_root();
        // 测试自己设了 HOME 就按它的来：状态目录由「IDORIS_STATE_DIR，缺省 $HOME/.local/state/idoris」解析，
        // 不能用我们的 IDORIS_STATE_DIR 盖掉它选的 HOME。
        let own_home = explicit(self, "HOME");
        if !own_home {
            self.env("HOME", root.join("home"));
        }
        if !explicit(self, "XDG_STATE_HOME") {
            self.env("XDG_STATE_HOME", root.join("xdg-state"));
        }
        if !own_home && !explicit(self, "IDORIS_STATE_DIR") {
            self.env("IDORIS_STATE_DIR", root.join("state"));
        }
        // 不让子进程读到真实的 oMLX key；测试要用 key 会自己显式设置（显式设置不被这里删）。
        for key in ["IDORIS_OMLX_API_KEY", "OMLX_API_KEY"] {
            if !explicit(self, key) {
                self.env_remove(key);
            }
        }
        self
    }
}

/// 进程内测试用：一个全新的、测试私有的状态目录（`BoundSupervisor::spawn_omlx_in_state_dir` 之类接受显式状态目录的构造函数用）。
/// 每次调用一个新子目录，互不共享围栏。
pub fn fresh_state_dir() -> std::path::PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = isolation_root().join(format!(
        "in-process-{}",
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create the in-process test state dir");
    dir
}
