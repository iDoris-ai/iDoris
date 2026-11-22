//! 引擎种类相关的一切，放在状态机后面（接口规范 §2.12.1「配方」之上，管理器要多知道的那一部分）。
//!
//! S3 的 [`EngineRecipe`] 回答「怎么发现、怎么拼命令、怎么判就绪」；管理器还要知道：拉起之前怎么准备这种引擎自己的目录与
//! 参数（oMLX：iDoris 自己的 base、模型目录）、它支不支持私有 key、外部的这种引擎怎么校验监听者、加载围栏在哪、
//! key 在进程里登记到哪。v0.6.0 只有 oMLX；加一种引擎是写一个新的 [`EngineBackend`]，再在 [`super::plan_managed`] 的
//! 「引擎种类到 backend」的对应里加一行，状态机不改。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use idoris_upstream::engine::{
    CodesignCheck, CodesignPolicy, EngineKey, EngineKind, EngineLaunch, EngineRecipe,
    ListenerPolicy,
};
use idoris_upstream::omlx::engine::{
    BASE_PATH_ENV, OMLX_TEAM_ID, OmlxRecipe, base_path_in, prepare_base_dir,
};
use idoris_upstream::omlx::key::{
    EndpointKey, block_endpoint, govern_endpoint, resolve_omlx_api_key_for,
};

use crate::readiness::UpstreamReason;
use crate::setup_engine::{SetupEngine, SetupError};
use crate::setup_omlx::OmlxSetup;

/// 拉起之前的准备失败：稳定的原因码（没有合适的码就是 `None`）与给人看的说明（只进日志，不含秘密）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareFailure {
    pub reason: Option<UpstreamReason>,
    pub message: String,
}

/// 一种引擎在管理器里的全部专属知识。
pub trait EngineBackend: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> EngineKind;

    /// 只用来探测、发现可执行文件、认回孤儿的配方：不需要准备任何目录，不能用来拉起。
    fn probe_recipe(&self) -> Arc<dyn EngineRecipe>;

    /// 一次拉起用的配方。**每次尝试都重新调用**：重新取最新的模型目录，核对并建好这种引擎自己的私有目录
    /// （oMLX：`<状态目录>/omlx-base`，拿不到是 `base_unavailable`）。
    fn launch_recipe(&self, state_dir: &Path) -> Result<Arc<dyn EngineRecipe>, PrepareFailure>;

    /// 自己拉起的引擎要不要带私有 key（规范 §2.7.7）。
    fn supports_private_key(&self) -> bool;

    /// 外部引擎的监听者要满足什么（同一个用户、签名团队）。`None` = 这种引擎没法校验，外部的一律不接管。
    fn listener_policy(&self) -> Option<ListenerPolicy>;

    /// 这个 endpoint 上的加载围栏文件（`None` = 这种引擎没有围栏）。必须与它的适配器用的是同一个路径。
    fn fence_path(&self, state_dir: &Path, endpoint: &str) -> Option<PathBuf>;

    /// 登记：这个 endpoint 上的引擎是 iDoris 自己拉起的，发往它的请求只用这个私有 key。
    fn govern_private(&self, endpoint: &str, key: &EngineKey);

    /// 登记：这个 endpoint 上是外部引擎。`settings_key_verified` 为 true 时，才可以把读来的 `settings.json` key 发给它。
    fn govern_external(&self, endpoint: &str, settings_key_verified: bool);

    /// **封上**这个 endpoint：什么凭证都不发（私有 key、环境变量的 key、`settings.json` 的 key 都不发），直到下一个监听者通过校验、
    /// 被登记成私有或外部的口径（IDORIS-CORE S6，Codex 预审 3）。托管的 endpoint 在管理器启动时就先封上；引擎停了、正在被替换、
    /// 外部引擎的监听者没了、端口上换了别的东西的时候都回到它。**不是撤销登记**：撤销会让回退链（环境变量、`settings.json`）重新
    /// 适用于这个 endpoint，把用户的 key 发给之后占着这个端口、还没通过校验的别的东西；旧的私有 key 也随封上被换掉，不留在进程里。
    fn block(&self, endpoint: &str);

    /// 现在发往这个 endpoint 的 key（私有 key，或外部引擎按这种引擎自己的顺序解析出来的）。
    fn current_key(&self, endpoint: &str) -> Option<String>;

    /// 一次拉起的摘要，管理器拉起引擎时写成一行日志（IDORIS-CORE S6）。引擎会改写自己的进程标题（oMLX 就是），`ps` 里看不到端口、
    /// 模型目录、内存护栏、base 目录，排查时只能靠这一行。**只能含不是秘密的值**：私有 key 经环境变量交给引擎，命令行里
    /// 没有（`EngineLaunch::new` 校验过），这里也绝不读环境里的 key。缺省实现是可执行文件、端口与完整的命令行参数。
    fn launch_summary(&self, launch: &EngineLaunch) -> String {
        format!(
            "program={:?} port={} args={:?}",
            launch.executable(),
            launch.port(),
            launch.args()
        )
    }
}

/// 命令行里 `flag` 后面的那个值。
fn value_after(args: &[OsString], flag: &str) -> Option<String> {
    let position = args.iter().position(|arg| arg == flag)?;
    args.get(position + 1)
        .map(|value| value.to_string_lossy().into_owned())
}

/// 取模型目录的函数：生产是 S4 的 `managed_models_dir()`（`OmlxSetup::from_env()` 每次重新取），测试可以换。
type ModelsDir = Arc<dyn Fn() -> Result<PathBuf, SetupError> + Send + Sync>;

/// oMLX 的 [`EngineBackend`]。
#[derive(Clone)]
pub struct OmlxBackend {
    home: Option<PathBuf>,
    models_dir: ModelsDir,
    codesign: CodesignPolicy,
    listener: ListenerPolicy,
}

impl std::fmt::Debug for OmlxBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmlxBackend")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl OmlxBackend {
    /// 生产的配置：`HOME` 给出 `~/Applications/oMLX.app` 这个发现位置；模型目录**每次拉起时**再调一次 S4 的
    /// `managed_models_dir()`（不另写一份）；`.app` 里的可执行文件过 codesign（团队 `PSK5Q5T46L`）；外部引擎的监听者
    /// 要求同一个用户、同一个团队。
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        Self {
            home,
            models_dir: Arc::new(|| OmlxSetup::from_env().managed_models_dir()),
            codesign: CodesignPolicy::Verify(CodesignCheck::new(OMLX_TEAM_ID)),
            listener: ListenerPolicy::new(OMLX_TEAM_ID),
        }
    }

    /// 换掉取模型目录的函数（测试里数调用次数、造 HOME 取不到或路径含逗号的情形）。
    pub fn with_models_dir(
        mut self,
        models_dir: impl Fn() -> Result<PathBuf, SetupError> + Send + Sync + 'static,
    ) -> Self {
        self.models_dir = Arc::new(models_dir);
        self
    }

    /// 换掉可执行文件的签名策略（测试里没有真的 oMLX.app）。
    pub fn with_codesign(mut self, codesign: CodesignPolicy) -> Self {
        self.codesign = codesign;
        self
    }

    /// 换掉外部引擎监听者的要求（测试里用假检查器）。
    pub fn with_listener_policy(mut self, listener: ListenerPolicy) -> Self {
        self.listener = listener;
        self
    }

    fn recipe(&self) -> OmlxRecipe {
        let recipe = OmlxRecipe::new().with_codesign(self.codesign.clone());
        match &self.home {
            Some(home) => recipe.with_home(home),
            None => recipe,
        }
    }
}

impl EngineBackend for OmlxBackend {
    fn kind(&self) -> EngineKind {
        EngineKind::OMLX
    }

    fn probe_recipe(&self) -> Arc<dyn EngineRecipe> {
        Arc::new(self.recipe())
    }

    fn launch_recipe(&self, state_dir: &Path) -> Result<Arc<dyn EngineRecipe>, PrepareFailure> {
        // iDoris 自己的 base：oMLX 会把命令行参数落盘到 base，所以不能让它去用用户的 `~/.omlx`。建不出来、核对不过
        // 就不拉起（规范 §2.7.6 的 `base_unavailable`），不回退到用户的目录。
        let base = base_path_in(state_dir);
        prepare_base_dir(&base).map_err(|error| PrepareFailure {
            reason: Some(UpstreamReason::BaseUnavailable),
            message: format!("无法准备 iDoris 自己的 oMLX base 目录：{error}"),
        })?;
        // 模型目录：每次拉起都重新问 S4 的 `managed_models_dir()`（HOME 没了、路径含逗号这类情况在启动之后也会变），
        // 并显式传 `--model-dir`。没有稳定的原因码可用：只记日志。
        let models = (self.models_dir)().map_err(|error| PrepareFailure {
            reason: None,
            message: format!("无法确定 oMLX 的模型目录：{}", error.message),
        })?;
        Ok(Arc::new(
            self.recipe().with_base_path(base).with_model_dir(models),
        ))
    }

    fn supports_private_key(&self) -> bool {
        true
    }

    fn listener_policy(&self) -> Option<ListenerPolicy> {
        Some(self.listener.clone())
    }

    fn fence_path(&self, state_dir: &Path, endpoint: &str) -> Option<PathBuf> {
        idoris_upstream::omlx::load_fence_path_in(state_dir, endpoint).ok()
    }

    fn govern_private(&self, endpoint: &str, key: &EngineKey) {
        govern_endpoint(endpoint, EndpointKey::Private(key.expose().to_owned()));
    }

    fn govern_external(&self, endpoint: &str, settings_key_verified: bool) {
        govern_endpoint(
            endpoint,
            EndpointKey::External {
                settings_key_verified,
            },
        );
    }

    fn block(&self, endpoint: &str) {
        block_endpoint(endpoint);
    }

    fn current_key(&self, endpoint: &str) -> Option<String> {
        resolve_omlx_api_key_for(endpoint)
    }

    /// 可执行文件、端口、内存护栏（有额度是 `--memory-guard-gb` 的整数 GiB，没有额度是 `balanced`）、模型目录、iDoris 自己的 base
    /// 目录。只从命令行里按旗标取值，加上 `OMLX_BASE_PATH` 这一个环境变量；环境里的其余变量（含 `OMLX_API_KEY`）一概不读。
    /// 路径用调试格式（带引号），含空格的状态目录（`Application Support`）也读得清楚。
    fn launch_summary(&self, launch: &EngineLaunch) -> String {
        let args = launch.args();
        let guard = match (
            value_after(args, "--memory-guard-gb"),
            value_after(args, "--memory-guard"),
        ) {
            (Some(gib), _) => format!("{gib} GiB"),
            (None, Some(level)) => level,
            (None, None) => "-".to_owned(),
        };
        let model_dir = value_after(args, "--model-dir").unwrap_or_else(|| "-".to_owned());
        let base = launch.env().get(OsStr::new(BASE_PATH_ENV)).map_or_else(
            || "-".to_owned(),
            |base| base.to_string_lossy().into_owned(),
        );
        format!(
            "program={:?} port={} memory_guard={guard} model_dir={model_dir:?} base={base:?}",
            launch.executable(),
            launch.port()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_upstream::engine::{EngineInstance, EngineLaunch, EngineLaunchSpec, StopOnExit};

    use super::*;

    const GIB: u64 = 1 << 30;

    /// 一次真实构造的拉起（不执行任何东西）：oMLX 的配方、一个带空格的状态目录（`Application Support`）、一把私有 key。
    fn launch(budget: Option<u64>, state_dir: &Path, models_dir: &Path) -> (EngineLaunch, String) {
        let models = models_dir.to_owned();
        let backend = OmlxBackend::from_env().with_models_dir(move || Ok(models.clone()));
        let recipe = backend.launch_recipe(state_dir).unwrap();
        let instance = EngineInstance::new(recipe, state_dir, 19731).unwrap();
        let key = EngineKey::generate().unwrap();
        let secret = key.expose().to_owned();
        let launch = EngineLaunch::new(
            &instance,
            EngineLaunchSpec {
                executable: PathBuf::from("/usr/bin/true"),
                allocated_budget_bytes: budget,
                api_key: Some(key),
                stop_on_exit: StopOnExit::Owned,
            },
            std::iter::empty(),
        )
        .unwrap();
        (launch, secret)
    }

    #[test]
    fn the_omlx_summary_names_program_port_memory_guard_model_dir_and_base_and_nothing_secret() {
        let work = tempfile::tempdir().unwrap();
        let state = work.path().join("Application Support").join("idoris");
        let models = work.path().join("models");
        let backend = OmlxBackend::from_env();
        let (with_budget, secret) = launch(Some(16 * GIB + 12345), &state, &models);
        let line = backend.launch_summary(&with_budget);
        assert_eq!(
            line,
            format!(
                "program=\"/usr/bin/true\" port=19731 memory_guard=16 GiB model_dir={:?} base={:?}",
                models.display().to_string(),
                state.join("omlx-base").display().to_string()
            )
        );
        // The private key travels in the environment (`OMLX_API_KEY`), which the summary never reads.
        assert!(with_budget.env().contains_key(OsStr::new("OMLX_API_KEY")));
        assert!(!line.contains(&secret), "{line}");
        assert!(!line.contains("OMLX_API_KEY"));
        assert!(
            !line
                .as_bytes()
                .windows(32)
                .any(|window| window.iter().all(u8::is_ascii_hexdigit)),
            "a key-shaped run in {line}"
        );

        // No allocated budget: the engine's own default guard level, spelled out.
        let (without_budget, _) = launch(None, &state, &models);
        let line = backend.launch_summary(&without_budget);
        assert!(line.contains(" memory_guard=balanced "), "{line}");
        assert!(line.contains(" port=19731 "), "{line}");
    }

    /// An engine kind that does not override the summary gets the generic one.
    #[derive(Debug)]
    struct Plain(OmlxBackend);

    impl EngineBackend for Plain {
        fn kind(&self) -> EngineKind {
            self.0.kind()
        }
        fn probe_recipe(&self) -> Arc<dyn EngineRecipe> {
            self.0.probe_recipe()
        }
        fn launch_recipe(&self, state_dir: &Path) -> Result<Arc<dyn EngineRecipe>, PrepareFailure> {
            self.0.launch_recipe(state_dir)
        }
        fn supports_private_key(&self) -> bool {
            true
        }
        fn listener_policy(&self) -> Option<ListenerPolicy> {
            None
        }
        fn fence_path(&self, _state_dir: &Path, _endpoint: &str) -> Option<PathBuf> {
            None
        }
        fn govern_private(&self, _endpoint: &str, _key: &EngineKey) {}
        fn govern_external(&self, _endpoint: &str, _settings_key_verified: bool) {}
        fn block(&self, _endpoint: &str) {}
        fn current_key(&self, _endpoint: &str) -> Option<String> {
            None
        }
    }

    #[test]
    fn the_default_summary_is_program_port_and_the_whole_command_line_without_the_environment() {
        let work = tempfile::tempdir().unwrap();
        let models = work.path().join("models");
        let (launch, secret) = launch(Some(2 * GIB), work.path(), &models);
        let line = Plain(OmlxBackend::from_env()).launch_summary(&launch);
        assert!(
            line.starts_with("program=\"/usr/bin/true\" port=19731 args=["),
            "{line}"
        );
        for flag in ["--host", "--port", "--memory-guard-gb", "--model-dir"] {
            assert!(line.contains(flag), "{flag} in {line}");
        }
        assert!(!line.contains(&secret), "{line}");
        assert!(
            !line.contains("OMLX_BASE_PATH"),
            "the environment is not part of it"
        );
    }
}
