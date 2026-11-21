//! 引擎启动命令的通用部分（设计稿 §2.7.4，规范 §2.7.6「启动命令」）。
//!
//! 参数与引擎专属的环境变量由 [`super::EngineRecipe::launch_plan`] 给；这里校验输入，并**从零**搭环境，
//! 然后把引擎的 stdout/stderr 接到日志文件、工作目录设为状态目录、独立进程组：
//!
//! ```text
//! 环境:   PATH=/usr/bin:/bin:/usr/sbin:/sbin，HOME/TMPDIR/LANG/LC_ALL/LC_CTYPE/TZ（父进程有才传），
//!         再加配方给的专属变量（oMLX：OMLX_API_KEY、OMLX_BASE_PATH）
//! cwd:    状态目录
//! stdio:  stdin=null；stdout 与 stderr 追加到 <状态目录>/logs/<engine_kind>.log（见 logfile）
//! 进程组: process_group(0)
//! ```
//!
//! 通用层强制、配方绕不过去的规矩：
//! - **清洗环境**：iDoris 自己的 `IDORIS_*`（数据库路径、Admin token、数据 key……）以及父进程的其他一切变量
//!   都不会进入引擎。白名单之外的变量，只有配方在 [`super::LaunchPlan::env`] 里显式给值的才有，
//!   而且配方不能给 `PATH`、不能给 `IDORIS_` 开头的名字（[`LaunchError::ForbiddenEnv`]）；
//! - **key 不进 argv**（argv 同 UID 可见）：配方把 key 放进 `args` 就是 [`LaunchError::KeyInArgv`]；
//! - **额度为 0 不拉起**（[`LaunchError::BudgetZero`]）：什么都装不下。

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::instance::EngineInstance;
use super::key::EngineKey;
use super::logfile::EngineLog;
use super::recipe::{EngineKind, LaunchInput};

/// 引擎的 `PATH`：固定值，不继承父进程的。
pub const ENGINE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// 从父进程原样传给引擎的变量（有才传）。
pub const INHERITED_ENV: [&str; 6] = ["HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];
/// 引擎监听的地址：固定 loopback。通用层拿 `http://127.0.0.1:<port>` 当引擎的 endpoint。
pub const ENGINE_HOST: &str = "127.0.0.1";

/// iDoris 退出时怎么处置自己拉起的引擎（launcher 帧 `engine.stop_on_exit`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopOnExit {
    /// 退出时停掉自己拉起的引擎，避免孤儿占内存。缺省。
    #[default]
    Owned,
    /// 正常退出也保留引擎和所有权记录，下一个 iDoris 接管（升级重启时不想重新加载大模型）。
    Never,
}

impl StopOnExit {
    /// 帧里的写法：`"owned"` / `"never"`。
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "owned" => Some(Self::Owned),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum LaunchError {
    #[error("engine executable path must be absolute")]
    ExecutableNotAbsolute,
    /// 额度为 0：什么都装不下，不拉起。
    #[error("allocated budget is zero: nothing would fit, the engine is not started")]
    BudgetZero,
    /// 配方给的环境变量不合规：`PATH`、`IDORIS_` 开头、名字或值不合法。
    #[error("the engine recipe asked for an environment variable that is not allowed")]
    ForbiddenEnv,
    /// 配方把 API key 放进了命令行参数。
    #[error("the engine recipe put the API key on the command line")]
    KeyInArgv,
    /// 配方自己的配置不合法（字段名）。
    #[error("invalid engine configuration: {0}")]
    Invalid(&'static str),
}

impl LaunchError {
    /// 稳定的原因码（对应 `/health.upstreams[].reason`）：`BudgetZero` 是 `budget_zero`；`Invalid("base_path")`（引擎的
    /// 私有数据目录没配或核对不过，oMLX 配方，规范 §2.7.6）是 `base_unavailable`；其余是调用方或配方的编程错误。
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::BudgetZero => "budget_zero",
            Self::Invalid("base_path") => "base_unavailable",
            _ => "invalid_launch",
        }
    }
}

/// [`EngineLaunch::new`] 的输入（引擎无关的部分；引擎专属的配置在配方里，端口在实例里）。
#[derive(Debug)]
pub struct EngineLaunchSpec {
    /// 发现并校验过的可执行文件（[`super::EngineExecutable::path`]）。
    pub executable: PathBuf,
    /// `IDORIS_ALLOCATED_BUDGET_BYTES`；`None` = 没设额度。
    pub allocated_budget_bytes: Option<u64>,
    /// 这一次拉起的私有 key；`None` = 不设（引擎种类不支持私有 key，或调用方不要）。
    pub api_key: Option<EngineKey>,
    pub stop_on_exit: StopOnExit,
}

/// 一次引擎拉起的全部输入，已校验、已清洗。
pub struct EngineLaunch {
    instance: EngineInstance,
    executable: PathBuf,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    api_key: Option<EngineKey>,
    log: EngineLog,
    stop_on_exit: StopOnExit,
}

impl EngineLaunch {
    /// `parent_env` 是调用方的环境（生产传 `std::env::vars_os()`）；只有 [`INHERITED_ENV`] 里的名字会被取用，
    /// 其余全部丢弃。环境由参数传入而不是在这里读，是为了让「不泄露」能被确定性地测试
    /// （工作区禁 `unsafe`，测试改不了自己的进程环境）。
    pub fn new(
        instance: &EngineInstance,
        spec: EngineLaunchSpec,
        parent_env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, LaunchError> {
        if !spec.executable.is_absolute() {
            return Err(LaunchError::ExecutableNotAbsolute);
        }
        if spec.allocated_budget_bytes == Some(0) {
            return Err(LaunchError::BudgetZero);
        }

        let plan = instance.recipe().launch_plan(&LaunchInput {
            executable: &spec.executable,
            port: instance.port(),
            allocated_budget_bytes: spec.allocated_budget_bytes,
            api_key: spec.api_key.as_ref(),
        })?;
        if let Some(key) = &spec.api_key
            && plan
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains(key.expose()))
        {
            return Err(LaunchError::KeyInArgv);
        }

        let mut env = BTreeMap::new();
        env.insert(OsString::from("PATH"), OsString::from(ENGINE_PATH));
        for (name, value) in parent_env {
            if INHERITED_ENV
                .iter()
                .any(|allowed| OsStr::new(allowed) == name)
                && !value.is_empty()
            {
                env.insert(name, value);
            }
        }
        for (name, value) in plan.env {
            if !allowed_recipe_env(&name, &value) {
                return Err(LaunchError::ForbiddenEnv);
            }
            env.insert(name, value);
        }

        Ok(Self {
            log: EngineLog::new(instance),
            instance: instance.clone(),
            executable: spec.executable,
            args: plan.args,
            env,
            api_key: spec.api_key,
            stop_on_exit: spec.stop_on_exit,
        })
    }

    /// 要拉起的是哪个实例（配方、端口、状态目录）。
    pub fn instance(&self) -> &EngineInstance {
        &self.instance
    }

    /// 引擎种类（写进所有权记录的那个）。
    pub fn kind(&self) -> &EngineKind {
        self.instance.kind()
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn port(&self) -> u16 {
        self.instance.port()
    }

    /// `http://127.0.0.1:<port>`。
    pub fn endpoint(&self) -> String {
        self.instance.endpoint()
    }

    /// 引擎的工作目录：状态目录。
    pub fn working_dir(&self) -> &Path {
        self.instance.state_dir()
    }

    /// 传给可执行文件的参数（不含可执行文件本身）。
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// 引擎看到的完整环境。
    pub fn env(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }

    pub fn api_key(&self) -> Option<&EngineKey> {
        self.api_key.as_ref()
    }

    pub fn log(&self) -> &EngineLog {
        &self.log
    }

    /// 换日志的上限与份数（测试用小值）。
    pub fn with_log(mut self, log: EngineLog) -> Self {
        self.log = log;
        self
    }

    pub fn stop_on_exit(&self) -> StopOnExit {
        self.stop_on_exit
    }
}

impl std::fmt::Debug for EngineLaunch {
    /// 环境里可能有 key：只打印变量名，不打印值。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineLaunch")
            .field("instance", &self.instance)
            .field("executable", &self.executable)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("stop_on_exit", &self.stop_on_exit)
            .finish_non_exhaustive()
    }
}

/// 配方可以显式给的环境变量：名字是 `[A-Za-z_][A-Za-z0-9_]*`，不是 `PATH`，不以 `IDORIS_` 开头；值里没有 NUL。
fn allowed_recipe_env(name: &OsStr, value: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    let name_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
    name_ok
        && bytes != b"PATH"
        && !bytes.starts_with(b"IDORIS_")
        && !value.as_encoded_bytes().contains(&0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::engine::test_support::{TestRecipe, instance_with};

    const STATE: &str = "/var/state/idoris";

    fn spec() -> EngineLaunchSpec {
        EngineLaunchSpec {
            executable: "/opt/engines/fake/bin/fake-server".into(),
            allocated_budget_bytes: None,
            api_key: Some(EngineKey::from_hex(&"ab".repeat(32)).unwrap()),
            stop_on_exit: StopOnExit::Owned,
        }
    }

    fn instance() -> EngineInstance {
        instance_with(Path::new(STATE), TestRecipe::new("fakeserve"), 19731)
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn argv(launch: &EngineLaunch) -> Vec<String> {
        launch
            .args()
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_recipe_decides_the_argv_and_the_layout_comes_from_the_instance() {
        let launch = EngineLaunch::new(&instance(), spec(), env(&[])).unwrap();
        assert_eq!(argv(&launch), ["--listen", "127.0.0.1:19731"]);
        assert_eq!(launch.endpoint(), "http://127.0.0.1:19731");
        assert_eq!(launch.port(), 19731);
        assert_eq!(launch.kind().as_str(), "fakeserve");
        assert_eq!(
            launch.working_dir(),
            Path::new(STATE),
            "cwd is the state directory"
        );
        assert_eq!(
            launch.log().path(),
            Path::new("/var/state/idoris/logs/fakeserve.log")
        );
    }

    #[test]
    fn the_budget_reaches_the_recipe_but_zero_never_does() {
        let mut s = spec();
        s.allocated_budget_bytes = Some(17_179_869_184);
        let launch = EngineLaunch::new(&instance(), s, env(&[])).unwrap();
        assert_eq!(argv(&launch)[2..], ["--budget", "17179869184"]);

        let mut s = spec();
        s.allocated_budget_bytes = Some(0);
        let error = EngineLaunch::new(&instance(), s, env(&[])).unwrap_err();
        assert_eq!(error, LaunchError::BudgetZero);
        assert_eq!(error.reason_code(), "budget_zero");
    }

    #[test]
    fn the_environment_is_the_allowlist_plus_what_the_recipe_asks_for_and_nothing_else() {
        let mut recipe = TestRecipe::new("fakeserve");
        recipe.key_env = Some("FAKE_API_KEY");
        recipe
            .env
            .insert("FAKE_MODELS".into(), "/Volumes/models".into());
        let inst = instance_with(Path::new(STATE), recipe, 19731);
        let parent = env(&[
            ("PATH", "/opt/homebrew/bin:/Users/x/bin"),
            ("HOME", "/Users/x"),
            ("TMPDIR", "/var/folders/xx/T/"),
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "en_US.UTF-8"),
            ("LC_CTYPE", "UTF-8"),
            ("TZ", "Asia/Bangkok"),
            // none of these may reach the engine:
            ("IDORIS_OMLX_API_KEY", "super-secret"),
            ("IDORIS_ADMIN_TOKEN", "admin-secret"),
            ("IDORIS_DB_PATH", "/Users/x/idoris.sqlite3"),
            ("IDORIS_STATE_DIR", "/Users/x/state"),
            ("IDORIS_ALLOCATED_BUDGET_BYTES", "17179869184"),
            ("OMLX_API_KEY", "user-env-key"),
            ("FAKE_API_KEY", "user-env-key-2"),
            ("FAKE_MODELS", "/Users/x/elsewhere"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("ANTHROPIC_API_KEY", "anthropic"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("PYTHONPATH", "/evil"),
            ("DYLD_INSERT_LIBRARIES", "/evil.dylib"),
            ("LC_MESSAGES", "C"),
        ]);
        let launch = EngineLaunch::new(&inst, spec(), parent).unwrap();
        let seen: BTreeMap<String, String> = launch
            .env()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_str().unwrap().to_owned(),
                    v.to_str().unwrap().to_owned(),
                )
            })
            .collect();
        let expected: BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
            ("HOME", "/Users/x"),
            ("TMPDIR", "/var/folders/xx/T/"),
            ("LANG", "en_US.UTF-8"),
            ("LC_ALL", "en_US.UTF-8"),
            ("LC_CTYPE", "UTF-8"),
            ("TZ", "Asia/Bangkok"),
            ("FAKE_API_KEY", &"ab".repeat(32)),
            ("FAKE_MODELS", "/Volumes/models"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn inherited_variables_are_optional_and_empty_ones_are_dropped() {
        let launch =
            EngineLaunch::new(&instance(), spec(), env(&[("HOME", ""), ("TZ", "UTC")])).unwrap();
        let names: Vec<_> = launch.env().keys().map(|k| k.to_str().unwrap()).collect();
        assert_eq!(names, ["PATH", "TZ"]);
    }

    #[test]
    fn a_recipe_cannot_bring_idoris_variables_or_a_different_path_into_the_engine() {
        for (name, value) in [
            ("IDORIS_ADMIN_TOKEN", "x"),
            ("IDORIS_", "x"),
            ("PATH", "/opt/homebrew/bin"),
            ("", "x"),
            ("1BAD", "x"),
            ("A=B", "x"),
            ("A B", "x"),
            ("OK_NAME", "nul\0inside"),
        ] {
            let mut recipe = TestRecipe::new("fakeserve");
            recipe.env.insert(name.into(), value.into());
            let inst = instance_with(Path::new(STATE), recipe, 19731);
            assert_eq!(
                EngineLaunch::new(&inst, spec(), env(&[])).unwrap_err(),
                LaunchError::ForbiddenEnv,
                "{name:?}"
            );
        }
        // Ordinary names, including ones that merely contain or resemble the forbidden ones.
        let mut recipe = TestRecipe::new("fakeserve");
        for name in ["FAKE_HOME", "_PRIVATE", "path", "XIDORIS_Y", "OLLAMA_HOST"] {
            recipe.env.insert(name.into(), "v".into());
        }
        let inst = instance_with(Path::new(STATE), recipe, 19731);
        assert!(EngineLaunch::new(&inst, spec(), env(&[])).is_ok());
    }

    #[test]
    fn a_recipe_cannot_put_the_key_on_the_command_line() {
        let mut recipe = TestRecipe::new("fakeserve");
        recipe.key_in_args = true;
        let inst = instance_with(Path::new(STATE), recipe, 19731);
        assert_eq!(
            EngineLaunch::new(&inst, spec(), env(&[])).unwrap_err(),
            LaunchError::KeyInArgv
        );
        // Without a key there is nothing to leak.
        let mut s = spec();
        s.api_key = None;
        assert!(EngineLaunch::new(&inst, s, env(&[])).is_ok());
    }

    #[test]
    fn a_relative_executable_fails_closed_before_the_recipe_is_asked() {
        let mut s = spec();
        s.executable = "fake-server".into();
        assert_eq!(
            EngineLaunch::new(&instance(), s, env(&[])).unwrap_err(),
            LaunchError::ExecutableNotAbsolute
        );
    }

    #[test]
    fn a_recipe_that_rejects_its_own_configuration_is_reported_as_invalid() {
        let inst = instance_with(
            Path::new(STATE),
            TestRecipe::new("fakeserve").failing("model_dir"),
            19731,
        );
        let error = EngineLaunch::new(&inst, spec(), env(&[])).unwrap_err();
        assert_eq!(error, LaunchError::Invalid("model_dir"));
        assert_eq!(error.reason_code(), "invalid_launch");
    }

    #[test]
    fn an_unavailable_base_has_its_own_reason_code() {
        assert_eq!(
            LaunchError::Invalid("base_path").reason_code(),
            "base_unavailable"
        );
        assert_eq!(
            LaunchError::Invalid("model_dir").reason_code(),
            "invalid_launch"
        );
    }

    #[test]
    fn debug_shows_variable_names_but_never_the_key() {
        let mut recipe = TestRecipe::new("fakeserve");
        recipe.key_env = Some("FAKE_API_KEY");
        let inst = instance_with(Path::new(STATE), recipe, 19731);
        let launch = EngineLaunch::new(&inst, spec(), env(&[("HOME", "/Users/x")])).unwrap();
        let shown = format!("{launch:?}");
        assert!(!shown.contains(&"ab".repeat(32)));
        assert!(shown.contains("FAKE_API_KEY"));
    }

    #[test]
    fn instances_of_one_kind_share_the_log_but_not_the_endpoint_and_kinds_share_nothing() {
        let a = instance_with(Path::new(STATE), TestRecipe::new("fakeserve"), 19731);
        let b = instance_with(Path::new(STATE), TestRecipe::new("fakeserve"), 19732);
        let c = instance_with(Path::new(STATE), TestRecipe::new("otherkind"), 19731);
        let (la, lb, lc) = (
            EngineLaunch::new(&a, spec(), env(&[])).unwrap(),
            EngineLaunch::new(&b, spec(), env(&[])).unwrap(),
            EngineLaunch::new(&c, spec(), env(&[])).unwrap(),
        );
        assert_eq!(la.log().path(), lb.log().path());
        assert_ne!(la.endpoint(), lb.endpoint());
        assert_ne!(la.log().path(), lc.log().path());
        assert_ne!(
            la.instance().record_path(),
            lc.instance().record_path(),
            "even on the same port"
        );
    }

    #[test]
    fn stop_on_exit_wire_names() {
        assert_eq!(StopOnExit::from_wire("owned"), Some(StopOnExit::Owned));
        assert_eq!(StopOnExit::from_wire("never"), Some(StopOnExit::Never));
        for bad in ["", "Owned", "always", "none"] {
            assert_eq!(StopOnExit::from_wire(bad), None);
        }
        assert_eq!(StopOnExit::default(), StopOnExit::Owned);
        assert_eq!(StopOnExit::Never.as_wire(), "never");
    }
}
