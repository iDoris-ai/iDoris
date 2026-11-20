//! 引擎无关的本地推理引擎进程管理（IDORIS-CORE S3，设计稿 `docs/design/idoris-core-design.md` §2.7.3–2.7.5；
//! 命名与布局对齐规范草案 §2.7.1–§2.7.7、§2.12.1）。
//!
//! 「引擎」= 一个本地推理服务进程（oMLX、LM Studio、Ollama、llama.cpp、mlx_lm……）；「模型」= 引擎里的权重。
//! 模型级的生命周期是 [`idoris_backend::RuntimeAdapter`]（纯 HTTP）；本模块在它下面，只管**进程**。
//! 这里只有原语：没有状态机、退避、熔断，没有切换逻辑，没有默认引擎配置，也没有接进 serve 的启动流程、
//! `/health`、CLI 与规范；那是 S5 及之后各片的事。
//!
//! # 分层
//!
//! - **引擎相关**的全部在 [`EngineRecipe`]（规范 §2.12.1 的「配方」；为什么是 trait：见 `recipe.rs` 的模块文档）：
//!   发现可执行文件、取版本、拼启动命令与环境、判就绪。oMLX 是第一个实现
//!   （`crate::omlx::engine::OmlxRecipe`），不在本模块里。
//! - **通用**的全部在本模块：拉起（环境清洗、日志、独立进程组）、所有权记录、孤儿接管、停止、
//!   状态目录布局。它们不知道引擎是谁，只认 [`EngineInstance`]（配方 + 端口 + 状态目录）。
//!
//! # 引擎种类与实例
//!
//! 引擎种类 [`EngineKind`]（`engine_kind`）是字符串，格式 `^[a-z][a-z0-9_]{0,31}$`；保留名与 `unknown` 有常量。
//! 实例 = 种类 + endpoint（`http://127.0.0.1:<port>`）。launcher 帧里的 `engine.executables`
//! （键为 `engine_kind`、值为可执行文件路径）本模块**不解析**；调用方取 `executables[<engine_kind>]`，
//! 作为 [`EngineRecipe::discover`] 的 `explicit` 参数传进来。
//!
//! # 状态目录里的布局（规范 §2.7.6，内部格式）
//!
//! ```text
//! <状态目录>/engines-owned/<engine_kind>/<endpoint 编码>.json   0600   所有权记录（目录 0700，按种类分）
//! <状态目录>/logs/<engine_kind>.log[.1-.3]                      0600   引擎的 stdout/stderr，10 MiB 滚动，保留 3 份
//! ```
//!
//! 引擎的工作目录是状态目录。多个引擎（同一种类的多个实例，或不同种类）因此可以并存，互不覆盖：
//! 记录按「种类 → endpoint」各占一个文件，日志按种类各占一个文件。
//!
//! # 一个引擎从起到停（设计稿 §2.7.2，S5 会这样串起它们）
//!
//! 1. [`OwnedEngine::recover`]：有所有权记录就核对身份，认回上一个 iDoris 留下的引擎
//!    （[`Recovery::Adopted`]）；启动途中被杀的（[`Recovery::Starting`]）停掉重拉；作废的记录已清掉。
//!    配置里已经没有的端口，用 [`recorded_ports`] 列出该种类目录里留下的记录再逐个 `recover`；
//! 2. [`EngineRecipe::discover`]：找可执行文件并校验；[`EngineRecipe::version`] 取版本；
//! 3. [`EngineLaunch::new`]：配方拼命令，通用层校验并清洗环境；
//! 4. [`OwnedEngine::spawn`]：拉起，立刻写未封印的所有权记录；
//! 5. [`OwnedEngine::wait_ready`]（轮询 [`EngineRecipe::probe_ready`]），然后 [`OwnedEngine::seal`]
//!    （确认监听端口的是这个进程）；
//! 6. 退出时 [`OwnedEngine::stop`]（`stop_on_exit: owned`；宽限的缺省是 [`DEFAULT_STOP_GRACE`]，规范的 8 秒）或直接
//!    丢弃句柄（`never`：引擎与记录留给下一个 iDoris）。
//!
//! 同一个实例上的 spawn / recover / stop 由调用方串行化（S5 的管理器每个实例一个任务）；原语本身不加锁。
//!
//! 外部引擎（用户自己的 oMLX.app 之类）没有对应的类型，也就没有任何一条路径能给它发信号。
//!
//! 全部只在 unix 上编译。不加依赖：取别的进程的身份用 `ps` / `lsof` 子进程（workspace 禁 unsafe，
//! 设计稿风险 R5），见 `inspect.rs`。

mod discovery;
mod fsutil;
mod inspect;
mod instance;
mod key;
mod launch;
mod logfile;
mod owned;
mod probe;
mod recipe;
mod record;
mod state;
mod version;

pub use discovery::{
    CODESIGN_TOOL, CodesignCheck, CodesignPolicy, DiscoveryError, DiscoveryPlan, EngineExecutable,
    ExecutableSource, RejectReason, Rejection, discover_executable,
};
pub use inspect::{InspectError, PidState, ProcessInspector, SystemInspector};
pub use instance::{ENGINES_OWNED_DIR, EngineInstance, InstanceError, LOGS_DIR, RECORD_EXTENSION};
pub use key::{ENGINE_KEY_HEX_LEN, EngineKey, KeyError};
pub use launch::{
    ENGINE_HOST, ENGINE_PATH, EngineLaunch, EngineLaunchSpec, INHERITED_ENV, LaunchError,
    StopOnExit,
};
pub use logfile::{DEFAULT_KEEP, DEFAULT_MAX_BYTES, EngineLog};
pub use owned::{
    DEFAULT_STOP_GRACE, EngineError, IdentityVerdict, MismatchReason, OwnedEngine, ReadyPolicy,
    Recovery, UnreadableReason, verify_identity,
};
pub use probe::{EngineProbe, probe_http};
pub use recipe::{
    EngineKind, EngineKindError, EngineRecipe, LaunchInput, LaunchPlan, RESERVED_ENGINE_KINDS,
    UNKNOWN_ENGINE_KIND,
};
pub use record::{
    MAX_COMMAND_BYTES, MAX_RECORD_BYTES, OwnershipRecord, OwnershipStore, RECORD_SCHEMA,
    RecordError, recorded_ports,
};
pub use state::{STATE_DIR_ENV, default_state_dir, state_dir_from};
pub use version::{VersionError, parse_version, run_version_command};

/// 通用层单元测试用的假配方（与任何真实引擎无关）。
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;

    /// 一个可调的假引擎：`--listen 127.0.0.1:<port>`，就绪是 `GET /health` 返回 `ok`。
    #[derive(Debug, Clone)]
    pub(crate) struct TestRecipe {
        pub kind: String,
        /// 计划里永远带上的环境变量。
        pub env: BTreeMap<OsString, OsString>,
        /// 私有 key 放进哪个环境变量（`None` = 不放）。
        pub key_env: Option<&'static str>,
        /// 违规：把 key 放进命令行。
        pub key_in_args: bool,
        /// 让 `launch_plan` 以 `Invalid(字段)` 失败。
        pub fail: Option<&'static str>,
        /// 发现计划里的搜索目录（空 = 什么也找不到）。
        pub search_dirs: Vec<PathBuf>,
        /// 发现计划里的可执行文件名。
        pub binary_names: Vec<String>,
    }

    impl TestRecipe {
        pub(crate) fn new(kind: &str) -> Self {
            Self {
                kind: kind.to_owned(),
                env: BTreeMap::new(),
                key_env: None,
                key_in_args: false,
                fail: None,
                search_dirs: Vec::new(),
                binary_names: Vec::new(),
            }
        }

        pub(crate) fn failing(mut self, field: &'static str) -> Self {
            self.fail = Some(field);
            self
        }
    }

    #[async_trait]
    impl EngineRecipe for TestRecipe {
        fn kind(&self) -> EngineKind {
            match EngineKind::parse(&self.kind) {
                Ok(kind) => kind,
                Err(_) => panic!("test engine kind must follow the spec's format"),
            }
        }

        fn discovery_plan(&self) -> DiscoveryPlan {
            DiscoveryPlan {
                search_dirs: self.search_dirs.clone(),
                binary_names: self.binary_names.clone(),
                ..DiscoveryPlan::default()
            }
        }

        fn launch_plan(&self, input: &LaunchInput<'_>) -> Result<LaunchPlan, LaunchError> {
            if let Some(field) = self.fail {
                return Err(LaunchError::Invalid(field));
            }
            let mut args: Vec<OsString> = vec![
                "--listen".into(),
                format!("{ENGINE_HOST}:{}", input.port).into(),
            ];
            if let Some(budget) = input.allocated_budget_bytes {
                args.push("--budget".into());
                args.push(budget.to_string().into());
            }
            let mut env = self.env.clone();
            if let (Some(key), Some(name)) = (input.api_key, self.key_env) {
                env.insert(name.into(), key.expose().into());
            }
            if self.key_in_args
                && let Some(key) = input.api_key
            {
                args.push(key.expose().into());
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
            probe_http(
                client,
                &format!("{endpoint}/health"),
                bearer,
                timeout,
                |body| body == b"ok",
            )
            .await
        }
    }

    /// 状态目录 `state` 下、端口 `port` 上的一个实例，配方是 `recipe`。
    pub(crate) fn instance_with(state: &Path, recipe: TestRecipe, port: u16) -> EngineInstance {
        match EngineInstance::new(Arc::new(recipe), state, port) {
            Ok(instance) => instance,
            Err(error) => panic!("test instance: {error}"),
        }
    }

    pub(crate) fn instance(state: &Path, kind: &str, port: u16) -> EngineInstance {
        instance_with(state, TestRecipe::new(kind), port)
    }
}
