//! oMLX 作为一种引擎：[`crate::engine::EngineRecipe`] 的第一个实现（IDORIS-CORE S3，设计稿 §2.7.3、§2.7.4；
//! 规范草案 §2.7.6「oMLX 配方」）。
//!
//! 所有「是 oMLX」才有的东西都在这里，通用的进程管理（[`crate::engine`]）不知道它们：
//!
//! - **去哪里找**：launcher 帧 `engine.executables.omlx` 给的显式路径（调用方取出来，作为
//!   [`crate::engine::EngineRecipe::discover`] 的 `explicit` 参数传进来；给了就只看它）；没给则
//!   `/Applications/oMLX.app/Contents/MacOS/omlx-cli`、`~/Applications/oMLX.app/…`；
//!   `.app` 里的候选要过 codesign（`TeamIdentifier == PSK5Q5T46L`，本机 oMLX 0.6.4 的签名团队，设计稿附录 A）。
//!   不查 PATH：Agent24 启动 iDoris 时把 PATH 固定了，Homebrew 路径本来就不在里面；
//!   `~/.omlx/bin/omlx`、`/opt/homebrew/bin/omlx`、`/usr/local/bin/omlx` 都只是指向 app 内 `omlx-cli`
//!   的包装或符号链接，不作为发现目标。
//! - **怎么拉起**：`serve --host 127.0.0.1 --port <port> [--memory-guard-gb <N>] [--model-dir <dir>]`
//!   （flags 对照本机 oMLX 0.6.4 的 `omlx serve --help`）；私有 key 走 `OMLX_API_KEY`，根目录走
//!   `OMLX_BASE_PATH`；不使用 `--api-key`（argv 同 UID 可见）。
//!   - `--memory-guard-gb`：额度字节数除以 2^30 向下取整（整数 GiB）。目的是让 oMLX 自己的上限不高于 iDoris 的额度
//!     账本（`docs/13-…md` B-11：不开 memory guard，第二个模型载入会让 server 无响应）。`--help` 没写单位，也没有对真实
//!     oMLX 验证过：这个取法在两种解释下都不超过额度（按 GiB 解释时正好是向下取整，按十进制 GB 解释时更小）。
//!     不足 1 GiB 的额度不拉起，原因码 `budget_zero`：取不到不超过额度的整数；不传这个 flag 会让 oMLX 用它自己的缺省，
//!     传 0 的含义没有实测，两者都可能超过额度。额度为 0 由通用层拒绝，不足 1 GiB 由本配方拒绝。
//!   - `--model-dir` 默认不传（设计稿 §2.7.3：不传则引擎用默认 `~/.omlx/models`；宿主有 oMLX.app 的
//!     base-path 引导文件时由 `OMLX_BASE_PATH` 指向，见 [`bootstrap_base_path`]）；
//!     [`OmlxRecipe::with_model_dir`] 是显式覆盖的口子（附录 A 已确认这个 flag 存在）。
//! - **怎么判就绪**：`GET /v1/models` 返回 200 且 JSON 里有 `data` 数组即可（空列表也算：模型目录可能是空的）。
//!   oMLX 没有 `/health`。这是**引擎**就绪，不是某个**模型**就绪（后者是 [`crate::OmlxAdapter::probe_ready`]）。
//! - **版本**：默认的 `<exe> --version`（`omlx/cli.py` 的 argparse `version=__version__`，只打印版本号；
//!   静态阅读源码确认，没有运行过真实的 oMLX）。
//!
//! 一个 [`OmlxRecipe`] 就是一份配置好的 oMLX 引擎定义：家目录、搜索目录、签名策略、模型目录、根目录都在实例里。
//! 用 [`crate::engine::EngineInstance::new`] 给它一个端口（与状态目录）就能被管理；它的 `engine_kind` 是
//! 规范的保留名 `omlx`（[`crate::engine::EngineKind::OMLX`]）。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use crate::engine::{
    CodesignCheck, CodesignPolicy, DiscoveryPlan, ENGINE_HOST, EngineKind, EngineProbe,
    EngineRecipe, LaunchError, LaunchInput, LaunchPlan, probe_http,
};

/// oMLX 发行包的签名团队（本机 0.6.4，设计稿附录 A）。
pub const OMLX_TEAM_ID: &str = "PSK5Q5T46L";
/// 系统级安装位置。
pub const SYSTEM_APP_EXECUTABLE: &str = "/Applications/oMLX.app/Contents/MacOS/omlx-cli";
/// 用户级安装位置，相对家目录。
pub const USER_APP_EXECUTABLE: &str = "Applications/oMLX.app/Contents/MacOS/omlx-cli";
/// 在额外搜索目录里找的文件名。
pub const BINARY_NAME: &str = "omlx";
/// oMLX 读取 API key 的环境变量（`omlx/settings.py` 的 env 覆盖）。
pub const API_KEY_ENV: &str = "OMLX_API_KEY";
/// oMLX 读取根目录的环境变量（`omlx/settings.py` 的 `resolve_default_base_path`）。
pub const BASE_PATH_ENV: &str = "OMLX_BASE_PATH";
/// oMLX.app 的 base-path 引导文件，相对家目录。
pub const BASE_PATH_BOOTSTRAP: &str = "Library/Application Support/oMLX/base-path";
const BASE_PATH_MAX_FILE_BYTES: u64 = 4096;
const BASE_PATH_MAX_BYTES: usize = 1024;

/// 一份配置好的 oMLX 引擎定义（配方）。
#[derive(Debug, Clone)]
pub struct OmlxRecipe {
    home: Option<PathBuf>,
    search_dirs: Vec<PathBuf>,
    codesign: CodesignPolicy,
    model_dir: Option<PathBuf>,
    base_path: Option<PathBuf>,
}

impl Default for OmlxRecipe {
    fn default() -> Self {
        Self::new()
    }
}

impl OmlxRecipe {
    /// 设计稿 §2.7.3 的缺省：系统级的 `oMLX.app`，签名检查开，不查 PATH，不传模型目录。
    pub fn new() -> Self {
        Self {
            home: None,
            search_dirs: Vec::new(),
            codesign: CodesignPolicy::Verify(CodesignCheck::new(OMLX_TEAM_ID)),
            model_dir: None,
            base_path: None,
        }
    }

    /// 家目录：加上 `~/Applications/oMLX.app/…` 这个发现位置。相对路径不算家目录。
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// 额外的搜索目录（找其中的 `omlx`）。缺省为空：不查 PATH。
    pub fn with_search_dirs(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.search_dirs.extend(dirs);
        self
    }

    /// 签名策略；缺省是对 `.app` 里的候选做 codesign 检查。
    pub fn with_codesign(mut self, policy: CodesignPolicy) -> Self {
        self.codesign = policy;
        self
    }

    /// 显式的 `--model-dir`（绝对路径）；缺省不传。
    pub fn with_model_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.model_dir = Some(dir.into());
        self
    }

    /// `OMLX_BASE_PATH`（绝对路径）；缺省不传。宿主上有 oMLX.app 的引导文件时用 [`bootstrap_base_path`] 取。
    pub fn with_base_path(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base_path = Some(dir.into());
        self
    }
}

const BYTES_PER_GIB: u64 = 1 << 30;

/// 传给 `--memory-guard-gb` 的整数：额度字节数除以 2^30 向下取整（整数 GiB）；不足 1 时是 `None`，调用方据此拒绝拉起。
fn memory_guard_gib(budget_bytes: u64) -> Option<u64> {
    let gib = budget_bytes / BYTES_PER_GIB;
    (gib >= 1).then_some(gib)
}

#[async_trait]
impl EngineRecipe for OmlxRecipe {
    fn kind(&self) -> EngineKind {
        EngineKind::OMLX
    }

    fn discovery_plan(&self) -> DiscoveryPlan {
        let mut app_executables = vec![PathBuf::from(SYSTEM_APP_EXECUTABLE)];
        if let Some(home) = self.home.as_deref().filter(|home| home.is_absolute()) {
            app_executables.push(home.join(USER_APP_EXECUTABLE));
        }
        DiscoveryPlan {
            app_executables,
            search_dirs: self.search_dirs.clone(),
            binary_names: vec![BINARY_NAME.to_owned()],
            codesign: self.codesign.clone(),
        }
    }

    fn launch_plan(&self, input: &LaunchInput<'_>) -> Result<LaunchPlan, LaunchError> {
        if self
            .model_dir
            .as_deref()
            .is_some_and(|dir| !dir.is_absolute())
        {
            return Err(LaunchError::Invalid("model_dir"));
        }
        if self
            .base_path
            .as_deref()
            .is_some_and(|dir| !dir.is_absolute())
        {
            return Err(LaunchError::Invalid("base_path"));
        }

        let mut args: Vec<OsString> = vec![
            "serve".into(),
            "--host".into(),
            ENGINE_HOST.into(),
            "--port".into(),
            input.port.to_string().into(),
        ];
        if let Some(budget) = input.allocated_budget_bytes {
            // 整数 GiB 的粒度下，不足 1 GiB 的额度没有不超过额度的值可传：不传会让 oMLX 用它自己的缺省，传 0 的含义
            // 没有实测。按「额度为 0 不拉起」的同一精神拒绝，复用原因码 `budget_zero`，不新增原因码。
            let Some(gib) = memory_guard_gib(budget) else {
                return Err(LaunchError::BudgetZero);
            };
            args.push("--memory-guard-gb".into());
            args.push(gib.to_string().into());
        }
        if let Some(dir) = &self.model_dir {
            args.push("--model-dir".into());
            args.push(dir.as_os_str().to_owned());
        }

        let mut env = BTreeMap::new();
        if let Some(key) = input.api_key {
            env.insert(OsString::from(API_KEY_ENV), OsString::from(key.expose()));
        }
        if let Some(base_path) = &self.base_path {
            env.insert(
                OsString::from(BASE_PATH_ENV),
                base_path.as_os_str().to_owned(),
            );
        }
        Ok(LaunchPlan { args, env })
    }

    async fn probe_ready(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> EngineProbe {
        let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
        probe_http(client, &url, bearer, timeout, |body| {
            serde_json::from_slice::<serde_json::Value>(body)
                .is_ok_and(|value| value.get("data").is_some_and(serde_json::Value::is_array))
        })
        .await
    }
}

/// oMLX.app 的 base-path 引导文件 `<home>/Library/Application Support/oMLX/base-path` 的内容，
/// 用作引擎的 `OMLX_BASE_PATH`（设计稿 §2.7.3；`~/.omlx/bin/omlx` 包装脚本做的是同一件事）。
///
/// 只取第一行，去掉首尾空白；必须是非空的绝对路径、不含控制字符。文件不存在、读不了、不是普通文件
/// （符号链接也不跟随）、超过 4 KiB、内容不合格，一律 `None`：不传 `OMLX_BASE_PATH`，
/// 引擎自己会按它的规则（环境变量，再引导文件，再 `~/.omlx`）找。以 `~` 开头的路径同样不算：
/// 那是引擎自己展开的写法，由它去读。
pub fn bootstrap_base_path(home: &Path) -> Option<PathBuf> {
    if !home.is_absolute() {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(home.join(BASE_PATH_BOOTSTRAP))
        .ok()?;
    let meta = file.metadata().ok()?;
    let euid = rustix::process::geteuid().as_raw();
    if !meta.file_type().is_file()
        || meta.len() > BASE_PATH_MAX_FILE_BYTES
        || (meta.uid() != euid && meta.uid() != 0)
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(BASE_PATH_MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let line = text.lines().next()?.trim();
    let path = PathBuf::from(line);
    (!line.is_empty()
        && line.len() <= BASE_PATH_MAX_BYTES
        && !line.chars().any(char::is_control)
        && path.is_absolute())
    .then_some(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::ffi::OsStr;
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    use idoris_backend::RuntimeAdapter;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::engine::{
        DiscoveryError, EngineInstance, EngineKey, EngineLaunch, EngineLaunchSpec,
        ExecutableSource, StopOnExit, default_state_dir, discover_executable,
    };

    fn instance(engine: OmlxRecipe) -> EngineInstance {
        EngineInstance::new(Arc::new(engine), Path::new("/var/state/idoris"), 19731).unwrap()
    }

    fn spec() -> EngineLaunchSpec {
        EngineLaunchSpec {
            executable: SYSTEM_APP_EXECUTABLE.into(),
            allocated_budget_bytes: None,
            api_key: Some(EngineKey::from_hex(&"ab".repeat(32)).unwrap()),
            stop_on_exit: StopOnExit::Owned,
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn launch(engine: OmlxRecipe, spec: EngineLaunchSpec) -> EngineLaunch {
        EngineLaunch::new(&instance(engine), spec, env(&[])).unwrap()
    }

    fn argv(launch: &EngineLaunch) -> Vec<String> {
        launch
            .args()
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_kind_is_the_specs_reserved_name_omlx() {
        let kind = OmlxRecipe::new().kind();
        assert_eq!(kind, EngineKind::OMLX);
        assert_eq!(kind.as_str(), "omlx");
        assert_eq!(EngineKind::parse("omlx").unwrap(), kind);
        assert!(kind.is_reserved());
    }

    // ---- launch ----

    #[test]
    fn the_default_argv_is_exactly_the_design_form() {
        let l = launch(OmlxRecipe::new(), spec());
        assert_eq!(
            argv(&l),
            ["serve", "--host", "127.0.0.1", "--port", "19731"]
        );
        assert_eq!(l.endpoint(), "http://127.0.0.1:19731");
        assert_eq!(l.kind().as_str(), "omlx");
        // Spec 2.7.6: the log is `logs/omlx.log`, the record is per kind and per endpoint, and
        // the engine's working directory is the state directory itself.
        assert_eq!(l.log().path(), Path::new("/var/state/idoris/logs/omlx.log"));
        assert_eq!(
            l.instance().record_path(),
            Path::new(
                "/var/state/idoris/engines-owned/omlx/http%3A%2F%2F127%2E0%2E0%2E1%3A19731.json"
            )
        );
        assert_eq!(l.working_dir(), Path::new("/var/state/idoris"));
    }

    #[test]
    fn memory_guard_is_whole_gibibytes_rounded_down() {
        const GIB: u64 = 1 << 30;
        let guard = |bytes: u64| {
            let mut s = spec();
            s.allocated_budget_bytes = Some(bytes);
            argv(&launch(OmlxRecipe::new(), s))
        };
        assert_eq!(
            argv(&launch(OmlxRecipe::new(), spec())).len(),
            5,
            "no budget set: no flag"
        );
        assert_eq!(guard(GIB)[5..], ["--memory-guard-gb", "1"]);
        assert_eq!(guard(2 * GIB - 1)[5..], ["--memory-guard-gb", "1"]);
        // 16 GiB, the budget the design uses as its example: exactly 16; one byte less: 15.
        assert_eq!(guard(16 * GIB)[5..], ["--memory-guard-gb", "16"]);
        assert_eq!(guard(16 * GIB - 1)[5..], ["--memory-guard-gb", "15"]);
        assert_eq!(guard(16 * GIB + 1)[5..], ["--memory-guard-gb", "16"]);
        // The point of the unit choice: whatever oMLX takes the number for (GiB or decimal GB),
        // the limit it is given does not exceed the budget.
        for budget in [
            GIB,
            GIB + 1,
            3 * GIB - 1,
            16 * GIB - 1,
            16 * GIB,
            64 * GIB + 12_345,
            u64::MAX,
        ] {
            let n: u64 = guard(budget)[6].parse().unwrap();
            assert!(
                u128::from(n) * u128::from(GIB) <= u128::from(budget),
                "{budget}: as GiB"
            );
            assert!(
                u128::from(n) * 1_000_000_000 <= u128::from(budget),
                "{budget}: as decimal GB"
            );
        }
    }

    #[test]
    fn a_budget_below_one_gibibyte_is_refused_like_zero() {
        const GIB: u64 = 1 << 30;
        // Below one whole GiB there is no integer that stays within the budget: leaving the flag
        // out keeps oMLX's own default, and what 0 means was not tried. Same reason code as a
        // zero budget (which the generic layer refuses before oMLX is asked).
        for bytes in [1, 999_999_999, 1_000_000_000, GIB - 1] {
            let mut s = spec();
            s.allocated_budget_bytes = Some(bytes);
            let error = EngineLaunch::new(&instance(OmlxRecipe::new()), s, env(&[])).unwrap_err();
            assert_eq!(error, LaunchError::BudgetZero, "{bytes}");
            assert_eq!(error.reason_code(), "budget_zero");
        }
        let mut s = spec();
        s.allocated_budget_bytes = Some(GIB);
        assert!(EngineLaunch::new(&instance(OmlxRecipe::new()), s, env(&[])).is_ok());
    }

    #[test]
    fn a_zero_budget_is_refused_by_the_generic_layer_before_omlx_is_asked() {
        let mut s = spec();
        s.allocated_budget_bytes = Some(0);
        let error = EngineLaunch::new(&instance(OmlxRecipe::new()), s, env(&[])).unwrap_err();
        assert_eq!(error, LaunchError::BudgetZero);
    }

    #[test]
    fn model_dir_and_base_path_are_passed_only_when_configured() {
        let l = launch(OmlxRecipe::new().with_model_dir("/Volumes/models"), spec());
        assert_eq!(argv(&l)[5..], ["--model-dir", "/Volumes/models"]);
        assert!(!l.env().contains_key(OsStr::new(BASE_PATH_ENV)));

        let l = launch(
            OmlxRecipe::new().with_base_path("/Volumes/omlx-base"),
            spec(),
        );
        assert_eq!(
            l.env().get(OsStr::new(BASE_PATH_ENV)).unwrap(),
            "/Volumes/omlx-base"
        );
        assert_eq!(argv(&l).len(), 5);
    }

    #[test]
    fn relative_model_dir_or_base_path_is_an_invalid_configuration() {
        let e = |engine: OmlxRecipe| {
            EngineLaunch::new(&instance(engine), spec(), env(&[])).unwrap_err()
        };
        assert_eq!(
            e(OmlxRecipe::new().with_model_dir("models")),
            LaunchError::Invalid("model_dir")
        );
        assert_eq!(
            e(OmlxRecipe::new().with_base_path("base")),
            LaunchError::Invalid("base_path")
        );
    }

    #[test]
    fn the_key_goes_to_the_environment_never_to_argv() {
        let l = launch(OmlxRecipe::new(), spec());
        let key = "ab".repeat(32);
        assert_eq!(l.env().get(OsStr::new(API_KEY_ENV)).unwrap(), key.as_str());
        assert!(l.args().iter().all(|a| !a.to_string_lossy().contains(&key)));
        assert!(l.args().iter().all(|a| a != "--api-key"));

        // No key, no variable: an engine started without a private key has none to read.
        let mut s = spec();
        s.api_key = None;
        let l = launch(OmlxRecipe::new(), s);
        assert!(!l.env().contains_key(OsStr::new(API_KEY_ENV)));
    }

    #[test]
    fn the_environment_is_the_allowlist_plus_the_engine_key_and_nothing_else() {
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
            ("OMLX_BASE_PATH", "/Users/x/elsewhere"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("ANTHROPIC_API_KEY", "anthropic"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("PYTHONPATH", "/evil"),
            ("DYLD_INSERT_LIBRARIES", "/evil.dylib"),
            ("LC_MESSAGES", "C"),
        ]);
        let l = EngineLaunch::new(&instance(OmlxRecipe::new()), spec(), parent).unwrap();
        let seen: BTreeMap<String, String> = l
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
            ("OMLX_API_KEY", &"ab".repeat(32)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(
            seen, expected,
            "the user's own OMLX_* variables do not pass through either"
        );
    }

    // ---- discovery ----

    #[test]
    fn the_default_discovery_plan_follows_the_design() {
        let plan = OmlxRecipe::new().discovery_plan();
        assert_eq!(
            plan.app_executables,
            [PathBuf::from(
                "/Applications/oMLX.app/Contents/MacOS/omlx-cli"
            )]
        );
        assert!(plan.search_dirs.is_empty(), "no PATH lookup");
        assert_eq!(plan.binary_names, ["omlx"]);
        let CodesignPolicy::Verify(check) = plan.codesign else {
            panic!("codesign is on by default");
        };
        assert_eq!(check.team_id, "PSK5Q5T46L");
        assert_eq!(check.tool, Path::new("/usr/bin/codesign"));
    }

    #[test]
    fn home_extends_the_plan_and_a_relative_home_is_no_home() {
        let plan = OmlxRecipe::new().with_home("/Users/x").discovery_plan();
        assert_eq!(
            plan.app_executables,
            [
                PathBuf::from("/Applications/oMLX.app/Contents/MacOS/omlx-cli"),
                PathBuf::from("/Users/x/Applications/oMLX.app/Contents/MacOS/omlx-cli"),
            ]
        );
        let plan = OmlxRecipe::new().with_home("relative").discovery_plan();
        assert_eq!(plan.app_executables.len(), 1);
    }

    #[tokio::test]
    async fn discovery_runs_the_generic_validation_over_the_omlx_plan() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("omlx");
        std::fs::write(&exe, "#!/bin/sh\necho 0.6.4\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Hermetic: whether the real oMLX.app happens to be installed on this machine is not
        // the test's business, so the app locations are taken out of the plan.
        let without_apps = |engine: OmlxRecipe| {
            let mut plan = engine.discovery_plan();
            plan.app_executables.clear();
            plan
        };

        // No search directory, no PATH lookup: a perfectly good `omlx` somewhere is not found.
        let plan = without_apps(OmlxRecipe::new().with_codesign(CodesignPolicy::Off));
        assert_eq!(
            discover_executable(&plan, None).await.unwrap_err(),
            DiscoveryError::NotFound
        );

        // Given a search directory, the plan looks for the file named `omlx` in it, as a
        // user-installed CLI (no app-level signature check).
        let plan = without_apps(OmlxRecipe::new().with_search_dirs([bin.clone()]));
        let found = discover_executable(&plan, None).await.unwrap();
        assert_eq!(found.source, ExecutableSource::SearchDir);
        assert_eq!(found.path, std::fs::canonicalize(&exe).unwrap());
        assert_eq!(found.team_id, None);

        // An explicit path (the launcher's `engine.executables.omlx`) is used as given, through the
        // trait's default `discover`, and does not fall back to anything when it is no good.
        let found = OmlxRecipe::new().discover(Some(&exe)).await.unwrap();
        assert_eq!(found.source, ExecutableSource::Explicit);
        assert_eq!(found.path, std::fs::canonicalize(&exe).unwrap());
        let missing = root.path().join("no-such-omlx");
        let error = OmlxRecipe::new()
            .with_search_dirs([bin.clone()])
            .discover(Some(&missing))
            .await
            .unwrap_err();
        assert_eq!(error.reason_code(), "engine_not_found");
        assert!(matches!(error, DiscoveryError::Rejected(_)), "{error:?}");

        // The default `version` runs `<exe> --version` (this script prints a version for any argument).
        let version = OmlxRecipe::new()
            .version(&exe, Duration::from_secs(60))
            .await;
        assert_eq!(version.as_deref(), Ok("0.6.4"));
    }

    /// The real tool against the real app, read-only: `codesign` hashes the bundle (about 5 s on
    /// oMLX 0.6.4) and nothing of oMLX itself is executed. Not part of the default run.
    /// `cargo test -p idoris-upstream --lib real_codesign -- --ignored`
    #[tokio::test]
    #[ignore = "runs the real codesign on the installed oMLX.app (about 5 s)"]
    async fn real_codesign_accepts_the_installed_omlx_app() {
        let app = Path::new(SYSTEM_APP_EXECUTABLE);
        if !app.is_file() {
            eprintln!("skipping: {SYSTEM_APP_EXECUTABLE} is not installed");
            return;
        }
        let found = OmlxRecipe::new().discover(None).await.unwrap();
        assert_eq!(found.source, ExecutableSource::AppBundle);
        assert_eq!(found.team_id.as_deref(), Some(OMLX_TEAM_ID));
        assert_eq!(found.path, std::fs::canonicalize(app).unwrap());
    }

    // ---- readiness ----

    async fn probe_with(server: &MockServer, bearer: Option<&str>) -> EngineProbe {
        let client = crate::http_client().unwrap();
        OmlxRecipe::new()
            .probe_ready(&client, &server.uri(), bearer, Duration::from_secs(10))
            .await
    }

    async fn serving(template: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_models_list_is_ready_even_when_empty() {
        for body in [
            serde_json::json!({"object": "list", "data": []}),
            serde_json::json!({"object": "list", "data": [{"id": "m", "object": "model"}]}),
        ] {
            let server = serving(ResponseTemplate::new(200).set_body_json(body)).await;
            assert_eq!(probe_with(&server, None).await, EngineProbe::Ready);
        }
    }

    #[tokio::test]
    async fn only_the_omlx_models_shape_counts_as_omlx() {
        for template in [
            ResponseTemplate::new(200).set_body_string("<html>hello</html>"),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": "nope"})),
            ResponseTemplate::new(200).set_body_json(serde_json::json!([])),
            ResponseTemplate::new(404),
        ] {
            let server = serving(template).await;
            assert_eq!(probe_with(&server, None).await, EngineProbe::NotThisEngine);
        }
        let server = serving(ResponseTemplate::new(401)).await;
        assert_eq!(
            probe_with(&server, None).await,
            EngineProbe::AuthRejected(401)
        );
        let server = serving(ResponseTemplate::new(503)).await;
        assert_eq!(probe_with(&server, None).await, EngineProbe::Unavailable);
    }

    #[tokio::test]
    async fn the_probe_asks_v1_models_with_the_bearer_it_is_given() {
        let key = "cd".repeat(32);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {key}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(probe_with(&server, Some(&key)).await, EngineProbe::Ready);
        assert_eq!(probe_with(&server, None).await, EngineProbe::NotThisEngine);
    }

    // ---- base path ----

    fn home_with(content: &[u8]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(BASE_PATH_BOOTSTRAP);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
        home
    }

    #[test]
    fn the_bootstrap_files_first_line_is_the_base_path() {
        for (content, expected) in [
            (&b"/Volumes/omlx-base\n"[..], "/Volumes/omlx-base"),
            (b"/Volumes/omlx-base", "/Volumes/omlx-base"),
            (
                b"  /Volumes/with space/base  \r\n",
                "/Volumes/with space/base",
            ),
            (b"/first\n/second\n", "/first"),
        ] {
            let home = home_with(content);
            assert_eq!(
                bootstrap_base_path(home.path()),
                Some(PathBuf::from(expected)),
                "{content:?}"
            );
        }
    }

    #[test]
    fn anything_else_means_no_override() {
        for content in [
            &b""[..],
            b"\n",
            b"relative/base\n",
            b"~/omlx\n",
            b"\n/not-the-first-line\n",
            b"/with\x07bell\n",
            b"\xff\xfe/not-utf8\n",
        ] {
            let home = home_with(content);
            assert_eq!(bootstrap_base_path(home.path()), None, "{content:?}");
        }
        let huge = vec![b'/'; BASE_PATH_MAX_FILE_BYTES as usize + 1];
        assert_eq!(bootstrap_base_path(home_with(&huge).path()), None);
        let long = format!("/{}", "a".repeat(BASE_PATH_MAX_BYTES));
        assert_eq!(bootstrap_base_path(home_with(long.as_bytes()).path()), None);
    }

    #[test]
    fn a_missing_symlinked_or_non_regular_file_is_no_override() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(bootstrap_base_path(home.path()), None, "missing");
        assert_eq!(
            bootstrap_base_path(Path::new("relative")),
            None,
            "relative home"
        );

        let file = home.path().join(BASE_PATH_BOOTSTRAP);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let target = home.path().join("elsewhere");
        std::fs::write(&target, b"/Volumes/omlx-base\n").unwrap();
        symlink(&target, &file).unwrap();
        assert_eq!(bootstrap_base_path(home.path()), None, "symlink");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert_eq!(bootstrap_base_path(home.path()), None, "directory");
    }

    // ---- state ----

    #[test]
    fn the_engine_state_dir_is_the_same_root_the_load_fence_uses() {
        // Two functions state the same rule (IDORIS_STATE_DIR, else $HOME/.local/state/idoris); this
        // pins them together. The fence path is `<root>/omlx/<endpoint>/load.pending`.
        let adapter = crate::OmlxAdapter::new(crate::OmlxAdapterConfig {
            base_url: "http://127.0.0.1:19731".into(),
            api_key: None,
            call_timeout: Duration::from_secs(1),
        })
        .unwrap();
        match (adapter.load_fence_path(), default_state_dir()) {
            (Ok(fence), Some(root)) => {
                assert_eq!(fence.ancestors().nth(3).unwrap(), root.as_path());
                // The ownership record is named by the very same endpoint encoding as the fence's
                // directory: `<root>/omlx/<encoding>/load.pending` and `<encoding>.json`.
                let encoding = fence
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap();
                let inst = EngineInstance::new(Arc::new(OmlxRecipe::new()), &root, 19731).unwrap();
                assert_eq!(
                    inst.record_path().file_name().unwrap().to_str().unwrap(),
                    format!("{encoding}.json")
                );
            }
            (Err(_), None) => {} // neither IDORIS_STATE_DIR nor HOME is usable: both refuse
            (fence, root) => panic!("the two rules disagree: {fence:?} vs {root:?}"),
        }
    }
}
