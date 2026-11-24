//! 守卫：测试不能解析到真实的状态目录（`~/.local/state/idoris/`）或真实 HOME（`~/.omlx`）。
//!
//! 为什么是静态检查：状态目录由生产代码从进程环境（`IDORIS_STATE_DIR`，缺省 `$HOME/.local/state/idoris`）读取。进程内测试
//! 不能靠改环境隔离（`std::env::set_var` 在 edition 2024 是 unsafe，工作区又 `forbid(unsafe_code)`，而且改环境对并行线程本来就不安全）；
//! 在 `ctor` 里一次性设置同样要 unsafe。所以隔离分两条路，本文件逐文件核对每个测试都走了其中一条：
//!
//! - **起子进程**（`idoris` 二进制、或用 `current_exe()` 重进自己）：必须经 `tests/support/state_isolation.rs` 的 `isolate_state()`
//!   （`spawn_guarded()` 与 `startup_wait::spawn_when_executable()` 会自动调用），子进程拿到测试私有的 `HOME`、`XDG_STATE_HOME`、
//!   `IDORIS_STATE_DIR`，且读不到真实的 `IDORIS_OMLX_API_KEY`。
//! - **进程内**：构造 oMLX 适配器时必须给显式的加载围栏路径（`with_load_fence_path(…)`）或显式状态目录
//!   （`BoundSupervisor::spawn_omlx_in_state_dir(…)`），不用环境解析出来的默认值。
//!
//! 守卫本身是纯读源文件，没有环境依赖，可以和别的测试并行跑。新增的测试若违反规则，这里会点名文件与规则。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn test_files() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for krate in ["idoris-router", "idoris-upstream"] {
        let mut stack = vec![crates.join(krate).join("tests")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
    }
    files.sort();
    files
}

/// 去掉整行注释（`//`、`//!`、`///`），免得文档里提到这些名字的句子触发规则。
fn code_of(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// 规则 1：起 `idoris` 二进制（`CARGO_BIN_EXE_idoris`）或重进自己（`current_exe()`）的文件，必须用隔离过的启动方式。
#[test]
fn every_test_that_spawns_the_binary_isolates_its_state_dir() {
    let mut offenders = Vec::new();
    for path in test_files() {
        let file = name(&path);
        // 本守卫自己、帮手本身、不起进程的支持文件不检查。
        if file == "no_real_state_dir.rs" || file == "state_isolation.rs" {
            continue;
        }
        let code = code_of(&fs::read_to_string(&path).unwrap());
        let spawns = code.contains("CARGO_BIN_EXE_idoris") || code.contains("current_exe()");
        if spawns
            && !(code.contains("isolate_state")
                || code.contains("spawn_guarded")
                || code.contains("spawn_when_executable"))
        {
            offenders.push(file);
        }
    }
    assert!(
        offenders.is_empty(),
        "these tests spawn a process without isolate_state()/spawn_guarded(): {offenders:?}"
    );
}

/// 规则 2：任何测试都不再用环境解析状态目录的 `BoundSupervisor::spawn_omlx(`（生产里没有调用方，只有测试用）。
#[test]
fn in_process_supervisors_get_an_explicit_state_dir() {
    let mut offenders = Vec::new();
    for path in test_files() {
        let file = name(&path);
        if file == "no_real_state_dir.rs" {
            continue;
        }
        let code = code_of(&fs::read_to_string(&path).unwrap());
        if code.contains("BoundSupervisor::spawn_omlx(") {
            offenders.push(file);
        }
    }
    assert!(
        offenders.is_empty(),
        "use BoundSupervisor::spawn_omlx_in_state_dir(card, &fresh_state_dir()) instead: {offenders:?}"
    );
}

/// 规则 3：直接构造 `OmlxAdapter` / 走 `create_adapter` 的测试文件，要么给了显式加载围栏，要么在下面的清单里（已确认该文件的
/// 用例从不触发加载围栏：只打 `/v1/models`、`status`、`chat` 之类不走 `load` 的路径）。清单里的文件若新增了会 `load` 的用例，
/// 应改成显式围栏并把文件从清单里去掉。
const NEVER_LOADS: &[&str] = &[
    "agent24_native_compat.rs",
    "omlx_admin_egress.rs",
    "omlx_integration.rs",
    "omlx_integration_load.rs",
    "upstream_egress.rs",
    "warmup.rs",
];

#[test]
fn directly_built_omlx_adapters_have_an_explicit_load_fence() {
    let mut offenders = Vec::new();
    for path in test_files() {
        let file = name(&path);
        if file == "no_real_state_dir.rs" {
            continue;
        }
        let code = code_of(&fs::read_to_string(&path).unwrap());
        let builds = code.contains("OmlxAdapter::new(") || code.contains("create_adapter(");
        if builds
            && !code.contains("with_load_fence_path(")
            && !NEVER_LOADS.contains(&file.as_str())
        {
            offenders.push(file);
        }
    }
    assert!(
        offenders.is_empty(),
        "these tests build an OmlxAdapter without .with_load_fence_path(temporary_load_fence_path()): {offenders:?}"
    );
}

/// 帮手自己的行为：没显式设置时给私有的 HOME / XDG_STATE_HOME / IDORIS_STATE_DIR；显式设置的不覆盖；真实 key 变量被去掉。
#[path = "support/state_isolation.rs"]
mod state_isolation;

#[test]
fn isolate_state_sets_private_dirs_and_respects_explicit_ones() {
    use state_isolation::IsolateState;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::process::Command;

    let envs = |command: &Command| -> HashMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
            .collect()
    };
    let tmp = std::env::temp_dir();

    let mut plain = Command::new("true");
    plain.isolate_state();
    let plain = envs(&plain);
    for key in ["HOME", "XDG_STATE_HOME", "IDORIS_STATE_DIR"] {
        let value = plain[&OsString::from(key)].clone().expect(key);
        let value = PathBuf::from(value);
        assert!(
            value.starts_with(&tmp),
            "{key}={value:?} must be under {tmp:?}"
        );
    }
    assert_eq!(plain[&OsString::from("IDORIS_OMLX_API_KEY")], None);
    assert_eq!(plain[&OsString::from("OMLX_API_KEY")], None);

    // 测试自己选了 HOME：状态目录由它决定，不再塞 IDORIS_STATE_DIR。
    let mut own_home = Command::new("true");
    own_home
        .env("HOME", "/nonexistent/own-home")
        .isolate_state();
    let own_home = envs(&own_home);
    assert_eq!(
        own_home[&OsString::from("HOME")],
        Some(OsString::from("/nonexistent/own-home"))
    );
    assert!(!own_home.contains_key(&OsString::from("IDORIS_STATE_DIR")));

    // 显式设置的 key 不被删。
    let mut own_key = Command::new("true");
    own_key.env("IDORIS_OMLX_API_KEY", "k").isolate_state();
    assert_eq!(
        envs(&own_key)[&OsString::from("IDORIS_OMLX_API_KEY")],
        Some(OsString::from("k"))
    );
}
