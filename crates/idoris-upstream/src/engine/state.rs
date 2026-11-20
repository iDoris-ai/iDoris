//! iDoris 的状态目录（设计稿 §2.7.4、§2.7.5 的 `<IDORIS_STATE_DIR>`）。
//!
//! 引擎的所有权记录与日志在它下面：`engines-owned/<engine_kind>/…` 与 `logs/<engine_kind>.log`
//! （布局见 [`super::EngineInstance`]）；引擎的工作目录就是它。

use std::ffi::OsString;
use std::path::PathBuf;

/// 状态目录的环境变量。
pub const STATE_DIR_ENV: &str = "IDORIS_STATE_DIR";

/// 状态目录：`IDORIS_STATE_DIR`，缺省 `$HOME/.local/state/idoris`；必须是绝对路径，否则 `None`
/// （相对的 `IDORIS_STATE_DIR` 不会被悄悄换成 `HOME` 下的缺省值）。
///
/// 与 oMLX 加载围栏（`omlx/<endpoint>/load.pending`，`OmlxAdapter::load_fence_path`）用的是同一条规则：
/// 同一台机器上 iDoris 的状态只有一个根。两处各写一遍；`omlx::engine::tests::the_engine_state_dir_is_the_same_root_the_load_fence_uses`
/// 随 oMLX 配方一起提交（它要调 `OmlxAdapter` 的私有方法 `load_fence_path`），在测试进程当前的环境下比较两条规则的结果。
pub fn default_state_dir() -> Option<PathBuf> {
    state_dir_from(std::env::var_os(STATE_DIR_ENV), std::env::var_os("HOME"))
}

/// [`default_state_dir`] 的纯函数形式：`state_dir` 是 `IDORIS_STATE_DIR` 的值，`home` 是 `HOME` 的值。
pub fn state_dir_from(state_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    state_dir
        .map(PathBuf::from)
        .or_else(|| home.map(|home| PathBuf::from(home).join(".local/state/idoris")))
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_dir_rule_is_env_then_home_and_always_absolute() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            state_dir_from(os("/srv/idoris"), os("/Users/x")),
            Some("/srv/idoris".into())
        );
        assert_eq!(
            state_dir_from(None, os("/Users/x")),
            Some("/Users/x/.local/state/idoris".into())
        );
        // A relative or empty IDORIS_STATE_DIR is an error, not a reason to fall back to HOME.
        assert_eq!(state_dir_from(os("state"), os("/Users/x")), None);
        assert_eq!(state_dir_from(os(""), os("/Users/x")), None);
        // A relative HOME is no home.
        assert_eq!(state_dir_from(None, os("Users/x")), None);
        assert_eq!(state_dir_from(None, None), None);
    }

    #[test]
    fn the_default_state_dir_is_absolute_or_absent() {
        // Environment-dependent by design.
        if let Some(dir) = default_state_dir() {
            assert!(dir.is_absolute());
        }
    }
}
