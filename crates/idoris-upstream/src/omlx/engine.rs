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
//!   （flags 对照本机 oMLX 0.6.4 的 `omlx serve --help`）；私有 key 走 `OMLX_API_KEY`，base 目录走
//!   `OMLX_BASE_PATH`；不使用 `--api-key`（argv 同 UID 可见）。
//!   - **oMLX 会把命令行参数落盘到 base，所以 iDoris 用自己的 base，不用用户的。** oMLX 0.6.4 的 `serve` 只要命令行里有
//!     `--host`、`--port`、`--model-dir`、`--memory-guard-gb` 中的任何一个，就把命令行设置连同 base 里已有的设置写回
//!     `<base>/settings.json`（`omlx/cli.py` 的 `_has_cli_overrides`，`omlx/settings.py` 的 `save_cli_overrides`；
//!     0.6.4 没有不落盘的开关）；
//!     没给 `--base-path` 时 base 依次取 `OMLX_BASE_PATH`、oMLX.app 的引导文件
//!     `~/Library/Application Support/oMLX/base-path`、`~/.omlx`。本配方每次都带 `--host` 与 `--port`：让引擎自己找 base，
//!     就会改写用户自己的 oMLX 设置（端口、host、内存护栏档位），用户设置里的 `skip_api_key_verification` 与 sub keys
//!     也会对 iDoris 的私有引擎生效（`omlx/server.py` 的 `verify_api_key`），私有 key 的隔离随之失效。
//!     所以拉起必须配 [`OmlxRecipe::with_base_path`]：状态目录下的专用目录（约定 `<状态目录>/omlx-base`，见 [`base_path_in`]，
//!     由 [`prepare_base_dir`] 建成 0700）。[`OmlxRecipe::launch_plan`] 每次拉起时只读地核对它是当前用户自己的真目录
//!     （符号链接不算，组和其他人没有任何权限）；没配或核对不过，拒绝拉起（`LaunchError::Invalid("base_path")`，原因码
//!     `base_unavailable`），不让引擎自己找。`OMLX_BASE_PATH` 就是这个目录，父进程的同名变量进不了引擎的环境，
//!     oMLX.app 的引导文件不再读。落盘的设置下次启动照样生效：这次传过 `--model-dir` 或 `--memory-guard-gb`，下次不传，
//!     用的仍是落盘的值。
//!   - **base 之外的写入**：oMLX 0.6.4 的 `serve` 启动时还会写 `~/.omlx/bin/omlx-cluster-python`（给集群里别的 Mac 经 SSH
//!     找解释器的垫片，`omlx/cluster/worker_shim.py`，启动流程直接调用，内容变了才改写）。路径取自 `HOME`，而 `HOME`
//!     是通用层白名单里原样继承的变量，`OMLX_BASE_PATH` 管不到；本配方不改 `HOME`（会连带改掉 HF 缓存等其他位置，没有实测过）。
//!   - `--memory-guard-gb`：额度字节数除以 2^30 向下取整（整数 GiB），设置了额度才传，让 oMLX 自己的上限不高于 iDoris 的
//!     额度账本（`docs/13-…md` B-11：不开 memory guard，第二个模型载入会让 server 无响应）。oMLX 0.6.4 里它是正浮点
//!     （`cli.py` 的 `_positive_float`：0、负数、非有限数在命令行解析时就被拒绝，启动即退出），单位是 GiB
//!     （`process_memory_enforcer.py`：`int(gb * 1024**3)`）；取整数是本配方的选择，不是 oMLX 的限制。额度不足 1 GiB 时
//!     整数部分是 0；不传则 oMLX 用缺省的 balanced 档位，可能超过额度。所以不拉起，原因码 `budget_zero`（额度为 0 由通用层拒绝）。
//!   - `--model-dir` 默认不传：引擎用 iDoris 自己 base 下的 `models`，起初是空的；要用用户已有的模型，由调用方显式给
//!     [`OmlxRecipe::with_model_dir`]（落在 iDoris 自己的 `settings.json` 里）。oMLX 把这个值按逗号拆成多个目录，路径里带逗号会被拆开。
//!   - 以上 oMLX 的行为来自静态阅读 `/Applications/oMLX.app/Contents/Resources/omlx/` 里的 0.6.4 源码，没有运行过真实的 oMLX。
//! - **怎么判就绪**：`GET /v1/models` 返回 200 且 JSON 里有 `data` 数组即可（空列表也算：模型目录可能是空的）。
//!   oMLX 没有 `/health`。这是**引擎**就绪，不是某个**模型**就绪（后者是 [`crate::OmlxAdapter::probe_ready`]）。这个形状任何
//!   OpenAI 兼容服务都满足，证明不了对端是 oMLX；端口上的进程是不是我们拉起的，由通用层的 pid 封印（[`crate::engine::OwnedEngine::seal`]）确认。
//! - **版本**：默认的 `<exe> --version`（`omlx/cli.py` 的 argparse `version=__version__`，只打印版本号；
//!   静态阅读源码确认，没有运行过真实的 oMLX）。
//!
//! 一个 [`OmlxRecipe`] 就是一份配置好的 oMLX 引擎定义：家目录、搜索目录、签名策略、模型目录、base 目录都在实例里。
//! 用 [`crate::engine::EngineInstance::new`] 给它一个端口（与状态目录）就能被管理；它的 `engine_kind` 是
//! 规范的保留名 `omlx`（[`crate::engine::EngineKind::OMLX`]）。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use crate::engine::{
    CodesignCheck, CodesignPolicy, DirError, DiscoveryPlan, ENGINE_HOST, EngineKind, EngineProbe,
    EngineRecipe, LaunchError, LaunchInput, LaunchPlan, effective_uid, ensure_private_dir,
    probe_http,
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
/// iDoris 自己的 oMLX base 在状态目录下的目录名：`<状态目录>/omlx-base`（[`base_path_in`]）。
pub const BASE_DIR_NAME: &str = "omlx-base";

/// 一份配置好的 oMLX 引擎定义（配方）。拉起前要用 [`OmlxRecipe::with_base_path`] 配上 iDoris 自己的 base。
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
    /// 设计稿 §2.7.3 的缺省：系统级的 `oMLX.app`，签名检查开，不查 PATH，不传模型目录；没有 base（拉起前要配）。
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

    /// 显式的 `--model-dir`（绝对路径）；缺省不传，引擎用 iDoris 自己 base 下的 `models`（起初是空的）。
    /// 要共用用户已有的模型，传用户的模型目录。
    pub fn with_model_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.model_dir = Some(dir.into());
        self
    }

    /// iDoris 自己的 oMLX base（`OMLX_BASE_PATH`，绝对路径；约定 [`base_path_in`]，用 [`prepare_base_dir`] 建好）。
    /// 没配、或磁盘上的目录核对不过，[`OmlxRecipe::launch_plan`] 拒绝。同一状态目录下的两个 oMLX 实例各配各的，
    /// 共用会互相覆盖 `settings.json`。
    pub fn with_base_path(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base_path = Some(dir.into());
        self
    }
}

/// iDoris 自己的 oMLX base：`<状态目录>/omlx-base`（[`BASE_DIR_NAME`]）。只算路径，不碰磁盘。
pub fn base_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(BASE_DIR_NAME)
}

/// 建或核对 iDoris 自己的 oMLX base 失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BaseDirError {
    /// 路径不是绝对路径。
    #[error("the oMLX base directory path is not absolute")]
    NotAbsolute,
    /// 路径上的东西不合格，不碰它：不是当前用户自己的真目录（符号链接、别人的目录），或组和其他人有权限
    /// （拉起时的核对；[`prepare_base_dir`] 会先把自己的目录收紧）。
    #[error("the oMLX base directory is a symlink, someone else's, or open to other users")]
    Insecure,
    /// 读不了或建不了（路径上是个普通文件时是 `AlreadyExists`）。
    #[error("cannot create or inspect the oMLX base directory: {0:?}")]
    Io(std::io::ErrorKind),
}

/// 磁盘上的 base 合格：当前用户（`uid`）自己的真目录（`meta` 来自 `symlink_metadata`，符号链接不是目录），
/// 组和其他人没有任何权限（0700 或更严）。
fn base_dir_is_private(meta: &std::fs::Metadata, uid: u32) -> bool {
    meta.file_type().is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o077 == 0
}

/// 只读地核对磁盘上的 base：不存在、读不了、不合格都是错误。
fn check_base_dir(dir: &Path) -> Result<(), BaseDirError> {
    let meta = std::fs::symlink_metadata(dir).map_err(|error| BaseDirError::Io(error.kind()))?;
    if base_dir_is_private(&meta, effective_uid()) {
        Ok(())
    } else {
        Err(BaseDirError::Insecure)
    }
}

/// 建好 iDoris 自己的 oMLX base：不存在就建成 0700（缺的上级一并建）；已存在的要是当前用户自己的真目录（符号链接、别人的
/// 目录、不是目录的拒绝，不碰），比 0700 宽的收紧到 0700，与记录目录、日志目录一样。oMLX 自己建的 base 权限跟着 umask，
/// 所以由 iDoris 先建。每次拉起前调一次：`prepare_base_dir` → [`crate::engine::EngineLaunch::new`]（核对）→ [`crate::engine::OwnedEngine::spawn`]。
pub fn prepare_base_dir(dir: &Path) -> Result<(), BaseDirError> {
    if !dir.is_absolute() {
        return Err(BaseDirError::NotAbsolute);
    }
    ensure_private_dir(dir).map_err(|error| match error {
        DirError::Io(kind) => BaseDirError::Io(kind),
        DirError::Insecure => BaseDirError::Insecure,
    })
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
        // oMLX 会把命令行参数落盘到 base（模块文档）：base 要是 iDoris 自己的、配好的、磁盘上核对过的；
        // 没配就拒绝，不让引擎自己去找（`~/.omlx`、oMLX.app 的引导文件）。
        let Some(base_path) = self.base_path.as_deref() else {
            return Err(LaunchError::Invalid("base_path"));
        };
        if !base_path.is_absolute() || check_base_dir(base_path).is_err() {
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
            // 整数 GiB 的粒度下，不足 1 GiB 的额度取整得 0，oMLX 不接受 0（`--memory-guard-gb` 要正数）；
            // 不传则 oMLX 用它自己的缺省档位，可能超过额度。按「额度为 0 不拉起」的同一精神拒绝，
            // 复用原因码 `budget_zero`，不新增原因码。
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
        env.insert(
            OsString::from(BASE_PATH_ENV),
            base_path.as_os_str().to_owned(),
        );
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

    /// 一个准备好的、iDoris 自己的 base：`<临时状态目录>/omlx-base`，0700。
    struct Base {
        state: tempfile::TempDir,
    }

    impl Base {
        fn new() -> Self {
            let state = tempfile::tempdir().unwrap();
            prepare_base_dir(&base_path_in(state.path())).unwrap();
            Self { state }
        }

        fn path(&self) -> PathBuf {
            base_path_in(self.state.path())
        }

        /// 配好这个 base 的配方。
        fn recipe(&self) -> OmlxRecipe {
            OmlxRecipe::new().with_base_path(self.path())
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    /// 拉起这份配方，期望被拒绝。
    fn refused(engine: OmlxRecipe) -> LaunchError {
        EngineLaunch::new(&instance(engine), spec(), env(&[])).unwrap_err()
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
        let base = Base::new();
        let l = launch(base.recipe(), spec());
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
        let base = Base::new();
        let guard = |bytes: u64| {
            let mut s = spec();
            s.allocated_budget_bytes = Some(bytes);
            argv(&launch(base.recipe(), s))
        };
        assert_eq!(
            argv(&launch(base.recipe(), spec())).len(),
            5,
            "no budget set: no flag"
        );
        assert_eq!(guard(GIB)[5..], ["--memory-guard-gb", "1"]);
        assert_eq!(guard(2 * GIB - 1)[5..], ["--memory-guard-gb", "1"]);
        // 16 GiB, the budget the design uses as its example: exactly 16; one byte less: 15.
        assert_eq!(guard(16 * GIB)[5..], ["--memory-guard-gb", "16"]);
        assert_eq!(guard(16 * GIB - 1)[5..], ["--memory-guard-gb", "15"]);
        assert_eq!(guard(16 * GIB + 1)[5..], ["--memory-guard-gb", "16"]);
        // oMLX 0.6.4 reads the number as GiB (static reading of its source; it was not run). The
        // check also holds for the decimal reading, so a version that changed the unit would still
        // not be handed a limit above the budget.
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
        // Below one whole GiB the integer is 0, and oMLX does not accept 0 (`--memory-guard-gb`
        // takes a positive number); leaving the flag out keeps oMLX's own default tier, which may
        // exceed the budget. Same reason code as a zero budget (which the generic layer refuses
        // before oMLX is asked).
        let base = Base::new();
        for bytes in [1, 999_999_999, 1_000_000_000, GIB - 1] {
            let mut s = spec();
            s.allocated_budget_bytes = Some(bytes);
            let error = EngineLaunch::new(&instance(base.recipe()), s, env(&[])).unwrap_err();
            assert_eq!(error, LaunchError::BudgetZero, "{bytes}");
            assert_eq!(error.reason_code(), "budget_zero");
        }
        let mut s = spec();
        s.allocated_budget_bytes = Some(GIB);
        assert!(EngineLaunch::new(&instance(base.recipe()), s, env(&[])).is_ok());
    }

    #[test]
    fn a_zero_budget_is_refused_by_the_generic_layer_before_omlx_is_asked() {
        let mut s = spec();
        s.allocated_budget_bytes = Some(0);
        let error = EngineLaunch::new(&instance(OmlxRecipe::new()), s, env(&[])).unwrap_err();
        assert_eq!(error, LaunchError::BudgetZero);
    }

    #[test]
    fn the_model_dir_is_passed_only_when_configured() {
        let base = Base::new();
        let l = launch(base.recipe(), spec());
        assert_eq!(argv(&l).len(), 5, "no model dir configured: no flag");

        let l = launch(base.recipe().with_model_dir("/Volumes/models"), spec());
        assert_eq!(argv(&l)[5..], ["--model-dir", "/Volumes/models"]);
    }

    // ---- base directory: oMLX writes the command-line settings into it ----

    #[test]
    fn the_default_plan_carries_idoris_own_base_path() {
        // 配的是状态目录下准备好的专用目录；父进程环境里的 `OMLX_BASE_PATH` 与 `HOME` 指向用户自己的 oMLX，
        // 引擎拿到的 `OMLX_BASE_PATH` 仍然是 iDoris 自己的那个。
        let base = Base::new();
        let engine =
            EngineInstance::new(Arc::new(base.recipe()), base.state.path(), 19731).unwrap();
        let parent = env(&[("HOME", "/Users/x"), ("OMLX_BASE_PATH", "/Users/x/.omlx")]);
        let l = EngineLaunch::new(&engine, spec(), parent).unwrap();
        let seen = Path::new(l.env().get(OsStr::new(BASE_PATH_ENV)).unwrap());
        assert_eq!(seen, base.state.path().join(BASE_DIR_NAME));
        assert_eq!(seen.parent(), Some(l.working_dir()), "under the state dir");
        assert_eq!(mode(seen), 0o700);
    }

    #[test]
    fn an_unconfigured_base_is_refused_not_inherited() {
        // 没配 base，oMLX 会自己按环境变量、oMLX.app 的引导文件、`~/.omlx` 去找，再把命令行设置写进去：
        // 所以不拉起。用户的引导文件与 `OMLX_BASE_PATH` 哪怕指向一个完全合格的私有目录，也不能顶替。
        let user_home = tempfile::tempdir().unwrap();
        let users_base = tempfile::tempdir().unwrap();
        let bootstrap = user_home
            .path()
            .join("Library/Application Support/oMLX/base-path");
        std::fs::create_dir_all(bootstrap.parent().unwrap()).unwrap();
        std::fs::write(&bootstrap, users_base.path().to_str().unwrap()).unwrap();
        let parent = env(&[
            ("HOME", user_home.path().to_str().unwrap()),
            ("OMLX_BASE_PATH", users_base.path().to_str().unwrap()),
        ]);
        let recipe = OmlxRecipe::new().with_home(user_home.path());
        let error = EngineLaunch::new(&instance(recipe), spec(), parent).unwrap_err();
        assert_eq!(error, LaunchError::Invalid("base_path"));
        assert_eq!(error.reason_code(), "base_unavailable");
    }

    #[test]
    fn a_missing_base_is_refused_until_it_is_prepared() {
        let state = tempfile::tempdir().unwrap();
        let base = base_path_in(state.path());
        let recipe = || OmlxRecipe::new().with_base_path(&base);
        assert_eq!(refused(recipe()), LaunchError::Invalid("base_path"));
        prepare_base_dir(&base).unwrap();
        assert!(EngineLaunch::new(&instance(recipe()), spec(), env(&[])).is_ok());
    }

    #[test]
    fn a_symlinked_base_is_refused_even_when_it_points_at_a_private_directory() {
        // oMLX 用 `Path.resolve()` 解析 `OMLX_BASE_PATH`，会跟着符号链接走：链接指到哪，设置就写到哪。
        let state = tempfile::tempdir().unwrap();
        let real = state.path().join("real");
        prepare_base_dir(&real).unwrap();
        let link = base_path_in(state.path());
        symlink(&real, &link).unwrap();
        assert_eq!(
            refused(OmlxRecipe::new().with_base_path(&link)),
            LaunchError::Invalid("base_path")
        );
        // 对照：链接指向的那个目录本身没问题。
        let recipe = OmlxRecipe::new().with_base_path(&real);
        assert!(EngineLaunch::new(&instance(recipe), spec(), env(&[])).is_ok());
    }

    #[test]
    fn a_base_open_to_group_or_others_is_refused() {
        let base = Base::new();
        let chmod = |bits: u32| {
            std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(bits)).unwrap();
        };
        // 组与其他人的每一位都单独试：掩码少一位，就有一个放过去。
        for open in [
            0o710, 0o740, 0o720, 0o701, 0o704, 0o702, 0o770, 0o707, 0o755, 0o777,
        ] {
            chmod(open);
            assert_eq!(
                refused(base.recipe()),
                LaunchError::Invalid("base_path"),
                "{open:o}"
            );
        }
        chmod(0o700);
        assert!(EngineLaunch::new(&instance(base.recipe()), spec(), env(&[])).is_ok());
    }

    #[test]
    fn a_base_that_is_not_a_directory_is_refused() {
        // 普通文件，权限 0600、属主是自己：能拒绝它的是「是不是目录」这一道核对。
        let state = tempfile::tempdir().unwrap();
        let file = base_path_in(state.path());
        std::fs::write(&file, b"").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            refused(OmlxRecipe::new().with_base_path(&file)),
            LaunchError::Invalid("base_path")
        );
    }

    #[test]
    fn a_base_owned_by_someone_else_is_not_private() {
        // 测试进程不是 root，造不出别人的目录：把「当前用户」换成另一个 uid，去判同一个目录。
        let base = Base::new();
        let meta = std::fs::symlink_metadata(base.path()).unwrap();
        let me = effective_uid();
        assert!(base_dir_is_private(&meta, me));
        assert!(!base_dir_is_private(&meta, me.wrapping_add(1)));
    }

    #[test]
    fn a_relative_base_is_refused_even_when_it_names_a_private_directory() {
        // 相对路径按进程的当前目录解释，换个目录就指到别处。这里造一个从当前目录出发、落在准备好的 base 上的
        // 相对路径：磁盘上的核对本身会通过，能拒绝它的是「是不是绝对路径」这一道。
        let base = Base::new();
        let up: PathBuf = std::iter::repeat_n("..", 64).collect();
        let relative = up.join(base.path().strip_prefix("/").unwrap());
        assert!(!relative.is_absolute());
        assert_eq!(check_base_dir(&relative), Ok(()));
        assert_eq!(
            refused(OmlxRecipe::new().with_base_path(&relative)),
            LaunchError::Invalid("base_path")
        );
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
        let base = Base::new();
        let l = launch(base.recipe(), spec());
        let key = "ab".repeat(32);
        assert_eq!(l.env().get(OsStr::new(API_KEY_ENV)).unwrap(), key.as_str());
        assert!(l.args().iter().all(|a| !a.to_string_lossy().contains(&key)));
        assert!(l.args().iter().all(|a| a != "--api-key"));

        // No key, no variable: an engine started without a private key has none to read.
        let mut s = spec();
        s.api_key = None;
        let l = launch(base.recipe(), s);
        assert!(!l.env().contains_key(OsStr::new(API_KEY_ENV)));
    }

    #[test]
    fn the_environment_is_the_allowlist_plus_the_engine_key_and_base_and_nothing_else() {
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
        let base = Base::new();
        let l = EngineLaunch::new(&instance(base.recipe()), spec(), parent).unwrap();
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
            // iDoris 自己的 base，不是父进程环境里那个（`/Users/x/elsewhere`）
            ("OMLX_BASE_PATH", base.path().to_str().unwrap()),
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

    /// The real tool against the real app; it writes nothing: `codesign` hashes the bundle (about
    /// 5 s on oMLX 0.6.4) and nothing of oMLX itself is executed. Not part of the default run.
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

    // 「`data` 数组」这个形状任何 OpenAI 兼容服务都满足：这里测的是什么样的应答算就绪、其余怎么归类，不证明对端
    // 是 oMLX。端口上的进程是不是我们拉起的那个，由通用层的 pid 封印确认：见 tests/engine_lifecycle.rs 的
    // `another_server_answering_on_the_port_is_not_mistaken_for_the_engine`。
    #[tokio::test]
    async fn only_a_models_list_reply_is_ready_and_other_replies_are_classified() {
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

    // ---- prepare_base_dir ----

    #[test]
    fn prepare_builds_a_missing_base_private_and_tightens_a_wider_one() {
        let state = tempfile::tempdir().unwrap();
        // 不存在、上级也不存在：建成 0700。
        let nested = state.path().join("a/b").join(BASE_DIR_NAME);
        prepare_base_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(mode(&state.path().join("a")), 0o700, "created parents too");
        assert_eq!(
            mode(&state.path().join("a/b")),
            0o700,
            "created parents too"
        );
        // 已存在而权限宽：收紧到 0700；再调一次没有变化。
        let wide = state.path().join("wide");
        std::fs::create_dir(&wide).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_base_dir(&wide).unwrap();
        assert_eq!(mode(&wide), 0o700);
        prepare_base_dir(&wide).unwrap();
        assert_eq!(mode(&wide), 0o700);
        // 建好的目录过得了拉起时的核对。
        assert_eq!(check_base_dir(&nested), Ok(()));
        assert_eq!(check_base_dir(&wide), Ok(()));
    }

    #[test]
    fn prepare_leaves_alone_what_is_not_ours_to_touch() {
        let state = tempfile::tempdir().unwrap();
        // 符号链接：拒绝，也不去改它指向的目录。
        let target = state.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = state.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(prepare_base_dir(&link), Err(BaseDirError::Insecure));
        assert_eq!(mode(&target), 0o755, "the link's target is not touched");
        // 路径上是个普通文件。
        let file = state.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(
            prepare_base_dir(&file),
            Err(BaseDirError::Io(std::io::ErrorKind::AlreadyExists))
        );
        // 相对路径：不建。
        assert_eq!(
            prepare_base_dir(Path::new("relative-omlx-base")),
            Err(BaseDirError::NotAbsolute)
        );
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
