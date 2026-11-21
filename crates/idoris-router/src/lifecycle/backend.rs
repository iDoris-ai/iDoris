//! 引擎种类相关的一切，放在状态机后面（接口规范 §2.12.1「配方」之上，管理器要多知道的那一部分）。
//!
//! S3 的 [`EngineRecipe`] 回答「怎么发现、怎么拼命令、怎么判就绪」；管理器还要知道：拉起之前怎么准备这种引擎自己的目录与
//! 参数（oMLX：iDoris 自己的 base、模型目录）、它支不支持私有 key、外部的这种引擎怎么校验监听者、加载围栏在哪、
//! key 在进程里登记到哪。v0.6.0 只有 oMLX；加一种引擎是写一个新的 [`EngineBackend`]，管理器与状态机一行不改。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use idoris_upstream::engine::{
    CodesignCheck, CodesignPolicy, EngineKey, EngineKind, EngineRecipe, ListenerPolicy,
};
use idoris_upstream::omlx::engine::{OMLX_TEAM_ID, OmlxRecipe, base_path_in, prepare_base_dir};
use idoris_upstream::omlx::key::{
    EndpointKey, govern_endpoint, release_endpoint, resolve_omlx_api_key_for,
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

    /// 撤销登记（引擎停了、换了、失效了）：旧的私有 key 不能留在进程里。
    fn release(&self, endpoint: &str);

    /// 现在发往这个 endpoint 的 key（私有 key，或外部引擎按这种引擎自己的顺序解析出来的）。
    fn current_key(&self, endpoint: &str) -> Option<String>;
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

    fn release(&self, endpoint: &str) {
        release_endpoint(endpoint);
    }

    fn current_key(&self, endpoint: &str) -> Option<String> {
        resolve_omlx_api_key_for(endpoint)
    }
}
