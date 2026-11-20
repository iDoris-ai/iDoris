//! 引擎配方：「是哪一种引擎」相关的一切（规范 §2.12.1 的「配方」；IDORIS-CORE S3）。
//!
//! iDoris 以后要支持不止一种本地推理引擎（oMLX、LM Studio、Ollama、llama.cpp、mlx_lm……），引擎之间可以切换。
//! 本层把「引擎是哪一种」与「怎么管一个引擎进程」分开：
//!
//! - **引擎相关**的全部在 [`EngineRecipe`] 里：找可执行文件、取版本、拼启动命令与环境、判就绪。
//!   一个配方实例就是「一份配置好的引擎定义」：家目录、搜索目录、签名要求、模型目录这类宿主相关的配置都在实例里。
//! - **通用**的全部在 [`crate::engine`] 的其余部分：拉起（环境清洗、日志、独立进程组）、所有权记录、
//!   孤儿接管、停止、状态目录布局。它们只认 [`EngineRecipe`]，不知道也不关心引擎是谁。
//!
//! # 为什么是 trait，不是 enum 加描述结构
//!
//! - 差异是**行为**，不只是数据：怎么发现（oMLX 在 `.app` 里，llama.cpp 在 PATH 上，mlx_lm 是 python 模块）、
//!   怎么判就绪（`GET /v1/models`、`GET /health`、`GET /`，返回体各不相同）。纯数据的描述结构表达不了，
//!   最后还是要往 enum 的每个 `match` 里加分支。
//! - **开放**：规范把 `engine_kind` 定为开放集合（§2.12.1）。加第二个引擎 = 新写一个实现 `EngineRecipe` 的类型，
//!   通用层一行不改；enum 则要改通用层的每处 `match`，还得另留一个承载未知值的分支。
//! - **可以在 crate 之外实现**，所以测试里能用一个假的第二种引擎证明通用流程真的通用
//!   （见 `tests/engine_lifecycle.rs`）；enum 做不到，除非往生产的 enum 里塞一个只给测试用的变体。
//! - 与本 crate 的既有做法一致：`RuntimeAdapter`、`ProcessInspector` 都是 `async_trait` 加 `Arc<dyn …>`。
//!
//! 种类名本身是字符串（[`EngineKind`]）：格式 `^[a-z][a-z0-9_]{0,31}$`，规范的保留名有常量，
//! 不认识的取值照样合法、原样承载，不是错误。
//!
//! # 加一个引擎要实现什么
//!
//! 实现 [`EngineRecipe`]：必须的是 [`EngineRecipe::kind`]、[`EngineRecipe::discovery_plan`]、
//! [`EngineRecipe::launch_plan`]、[`EngineRecipe::probe_ready`]；默认实现不合适时再覆盖
//! [`EngineRecipe::discover`]（非常规的发现）和 [`EngineRecipe::version`]（版本不是 `--version`）。
//! 不需要改通用层，也不需要登记到什么地方；用 [`crate::engine::EngineInstance::new`] 给它一个端口就能被管理。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use super::discovery::{DiscoveryError, DiscoveryPlan, EngineExecutable, discover_executable};
use super::key::EngineKey;
use super::launch::LaunchError;
use super::probe::EngineProbe;
use super::version::{VersionError, run_version_command};

/// 规范保留的引擎种类名（§2.12.1），拼写现在就定下，不接受别名。保留名不等于已实现：v0.6.0 只有 `omlx`。
pub const RESERVED_ENGINE_KINDS: [&str; 6] = [
    "omlx",
    "lmstudio",
    "ollama",
    "llama_cpp",
    "mlx_lm",
    "openai_compat",
];
/// 表示「无法识别种类」的保留名（§2.12.1）。
pub const UNKNOWN_ENGINE_KIND: &str = "unknown";

/// 引擎种类 `engine_kind`（规范 §2.12.1）：开放集合的字符串，小写 ASCII 字母、数字、下划线，首字符为字母，
/// ≤ 32 字符（`^[a-z][a-z0-9_]{0,31}$`）。
///
/// 保留名有常量（[`EngineKind::OMLX`] 等），与字符串一一互转（[`EngineKind::parse`]、[`EngineKind::as_str`]）；
/// 本版本不认识的取值同样是合法的种类，原样承载。它同时是状态目录里的路径成分，格式保证了它不会逃出那里。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EngineKind(Cow<'static, str>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("engine_kind must match ^[a-z][a-z0-9_]{{0,31}}$")]
pub struct EngineKindError;

impl EngineKind {
    pub const OMLX: Self = Self::reserved("omlx");
    pub const LMSTUDIO: Self = Self::reserved("lmstudio");
    pub const OLLAMA: Self = Self::reserved("ollama");
    pub const LLAMA_CPP: Self = Self::reserved("llama_cpp");
    pub const MLX_LM: Self = Self::reserved("mlx_lm");
    pub const OPENAI_COMPAT: Self = Self::reserved("openai_compat");
    /// 「无法识别种类」。不是一个可以被管理的引擎（[`super::EngineInstance::new`] 拒绝它）。
    pub const UNKNOWN: Self = Self::reserved(UNKNOWN_ENGINE_KIND);

    /// 常量用：名字不合格是编译错误。
    const fn reserved(name: &'static str) -> Self {
        assert!(is_kind_name(name));
        Self(Cow::Borrowed(name))
    }

    /// 从字符串构造：格式不合格是错误，其余（含不认识的）一律接受。
    pub fn parse(name: &str) -> Result<Self, EngineKindError> {
        if !is_kind_name(name) {
            return Err(EngineKindError);
        }
        Ok(RESERVED_ENGINE_KINDS
            .iter()
            .chain([&UNKNOWN_ENGINE_KIND])
            .find(|reserved| **reserved == name)
            .map_or_else(
                || Self(Cow::Owned(name.to_owned())),
                |reserved| Self(Cow::Borrowed(*reserved)),
            ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 是规范的保留名（含 `unknown`）。
    pub fn is_reserved(&self) -> bool {
        self.is_unknown() || RESERVED_ENGINE_KINDS.contains(&self.as_str())
    }

    /// 是 `unknown`。
    pub fn is_unknown(&self) -> bool {
        self.as_str() == UNKNOWN_ENGINE_KIND
    }
}

impl std::fmt::Display for EngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `^[a-z][a-z0-9_]{0,31}$`。
const fn is_kind_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_') {
            return false;
        }
        index += 1;
    }
    true
}

/// 通用层交给配方、让它拼启动命令的输入。
#[derive(Debug, Clone, Copy)]
pub struct LaunchInput<'a> {
    /// 发现并校验过的可执行文件（绝对路径）。
    pub executable: &'a Path,
    /// 引擎要监听的端口（非 0；实例的端口）。
    pub port: u16,
    /// iDoris 分给这个引擎的内存额度（字节）；`None` = 没设。0 不会走到这里（通用层拒绝）。
    /// 怎么用由引擎决定：oMLX 映射成 `--memory-guard-gb`，没有对应开关的引擎可以忽略它。
    pub allocated_budget_bytes: Option<u64>,
    /// 这一次拉起的私有 API key；`None` = 不设（引擎种类不支持私有 key，或调用方不要）。
    pub api_key: Option<&'a EngineKey>,
}

/// 配方给出的启动命令：参数与引擎专属的环境变量。
///
/// 通用层在这之上强制的规矩（违反即 [`LaunchError`]，配方不必也不能绕过）：
/// - 环境由通用层从零搭：固定 `PATH`、父进程的 `HOME`/`TMPDIR`/`LANG`/`LC_ALL`/`LC_CTYPE`/`TZ`（有才传），
///   再加这里的 `env`。`env` 里不得有 `PATH`，不得有 `IDORIS_` 开头的名字，名字与值必须合法；
/// - **key 不得出现在 `args` 里**（argv 同 UID 可见）：只能走 `env`（或引擎自己的配置文件）；
/// - 引擎只准监听 `127.0.0.1`（`args` 里要带上对应的开关）：通用层拿 `http://127.0.0.1:<port>` 当它的 endpoint；
/// - 引擎必须留在**前台**：被拉起的进程自己就是引擎、是它那个进程组的 leader，不 daemonize，不是「fork 出
///   真正的服务后自己退出」的启动器。通用层靠它判断引擎还活着、靠整组信号收尾。
#[derive(Debug, Clone, Default)]
pub struct LaunchPlan {
    /// 传给可执行文件的参数（不含可执行文件本身）。
    pub args: Vec<OsString>,
    /// 引擎专属的环境变量（显式给值；不会从父进程继承任何东西）。
    pub env: BTreeMap<OsString, OsString>,
}

/// 一种本地推理引擎的配方（规范 §2.12.1）。见模块文档。
///
/// 实现必须是 `Send + Sync`，方法不得阻塞（`async` 方法里的系统调用要走异步，或足够快）。
#[async_trait]
pub trait EngineRecipe: Send + Sync + std::fmt::Debug {
    /// 引擎种类，如 [`EngineKind::OMLX`]。写进每份所有权记录，也是记录目录与日志文件的名字；
    /// 磁盘上的记录靠它认出「这是哪一种引擎留下的」。**发布之后不能改。** 不得是 `unknown`。
    fn kind(&self) -> EngineKind;

    /// 去哪里、按什么条件找可执行文件（纯数据；通用层负责校验）。
    fn discovery_plan(&self) -> DiscoveryPlan;

    /// 发现并校验可执行文件。`explicit` 是 launcher 显式给的路径（stdin 帧 `engine.executables[<engine_kind>]`，
    /// 规范 §2.7.1）：给了就只看它，不回退。默认：按 [`EngineRecipe::discovery_plan`] 走通用的发现与校验。
    /// 发现方式不是「几个位置上的一个文件」的引擎（例如要先找 python 再确认模块在）覆盖它。
    async fn discover(&self, explicit: Option<&Path>) -> Result<EngineExecutable, DiscoveryError> {
        discover_executable(&self.discovery_plan(), explicit).await
    }

    /// 取引擎版本。默认：`<exe> --version`，环境清空、限时，取第一行里的版本号。
    /// 失败不是致命错误（调用方只是少一个字段）。
    async fn version(&self, executable: &Path, timeout: Duration) -> Result<String, VersionError> {
        run_version_command(executable, &["--version"], timeout).await
    }

    /// 拼启动命令（参数与引擎专属的环境变量）。输入已经过通用校验；这里只校验引擎自己的配置。
    fn launch_plan(&self, input: &LaunchInput<'_>) -> Result<LaunchPlan, LaunchError>;

    /// 就绪探测：对 `endpoint`（`http://127.0.0.1:<port>`，无路径）发一次探测。`bearer` 是要带的 API key
    /// （没有就不带）；`timeout` 覆盖整个往返。结果要分清「连接被拒 / 认证被拒 / 不像这个引擎 / 暂不可用 / 就绪」
    /// （[`EngineProbe`]），启动决策靠它。通用的状态码与传输错误分类在 [`super::probe_http`] 里，
    /// 配方只需要说「响应体长什么样才算这个引擎」。
    async fn probe_ready(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> EngineProbe;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn engine_kind_follows_the_specs_format() {
        for good in [
            "omlx",
            "llama_cpp",
            "mlx_lm",
            "a",
            "a1",
            "something_new_2",
            &format!("a{}", "b".repeat(31)),
        ] {
            assert_eq!(EngineKind::parse(good).unwrap().as_str(), good, "{good}");
        }
        for bad in [
            "",
            "OMLX",
            "Omlx",
            // an uppercase letter anywhere, not only in front
            "oMLX",
            "omlX",
            "llama_Cpp",
            "aB",
            "1omlx",
            "_omlx",
            "omlx-1",
            "omlx.1",
            "omlx/x",
            "../omlx",
            "..",
            "omlx ",
            " omlx",
            "omlx\n",
            "omlx\0",
            "é",
            &format!("a{}", "b".repeat(32)),
        ] {
            assert_eq!(EngineKind::parse(bad), Err(EngineKindError), "{bad:?}");
        }
    }

    #[test]
    fn the_reserved_names_are_exactly_the_specs_and_round_trip_with_strings() {
        let all = [
            (EngineKind::OMLX, "omlx"),
            (EngineKind::LMSTUDIO, "lmstudio"),
            (EngineKind::OLLAMA, "ollama"),
            (EngineKind::LLAMA_CPP, "llama_cpp"),
            (EngineKind::MLX_LM, "mlx_lm"),
            (EngineKind::OPENAI_COMPAT, "openai_compat"),
            (EngineKind::UNKNOWN, "unknown"),
        ];
        for (kind, name) in all {
            assert_eq!(kind.as_str(), name);
            assert_eq!(EngineKind::parse(name).unwrap(), kind, "{name}");
            assert_eq!(kind.to_string(), name);
            assert!(kind.is_reserved(), "{name}");
        }
        assert_eq!(
            RESERVED_ENGINE_KINDS,
            [
                "omlx",
                "lmstudio",
                "ollama",
                "llama_cpp",
                "mlx_lm",
                "openai_compat"
            ]
        );
        assert!(EngineKind::UNKNOWN.is_unknown());
        assert!(!EngineKind::OMLX.is_unknown());
    }

    #[test]
    fn a_value_this_version_has_never_heard_of_is_a_valid_kind_carried_as_is() {
        let kind = EngineKind::parse("future_engine").unwrap();
        assert_eq!(kind.as_str(), "future_engine");
        assert!(!kind.is_reserved());
        assert!(!kind.is_unknown());
        assert_eq!(EngineKind::parse(kind.as_str()).unwrap(), kind);
        // Aliases are not accepted: the spelling is fixed.
        assert_eq!(EngineKind::parse("llama.cpp"), Err(EngineKindError));
        assert_eq!(EngineKind::parse("lm-studio"), Err(EngineKindError));
        assert_ne!(
            EngineKind::parse("lm_studio").unwrap(),
            EngineKind::LMSTUDIO
        );
    }
}
