//! idoris.ai/api 计算服务 **S4：云供应商契约**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §6（请求状态机与失败切换）、§8（端点级许可表）、
//! §12 的 S4 一行。这一片决定「钱算没算对、会不会重复扣费、儿童数据会不会被送到不合规的上游」，
//! 所以**每一条规则都由 S0 契约裁决**（`idoris-compute-contracts`），这里只负责用契约的类型和纯函数驱动真实的上游调用：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`executor`] | 执行器：候选选择（`select_compliant`）→ 准入 → 尝试（先落库再写字节）→ 首字节前切换 → 发布边界 → 结算 |
//! | [`answer`] | 两种结果：等待发布的完整回答（非流式）、一条流；发布边界与客户端断开时的收尾 |
//! | [`admission`] | 候选准入的接口（S3 的接缝）与 S4 的简单实现（只算预留额，一律放行，TODO(S3)） |
//! | [`config`] | 云上游配置（YAML）：地址、密钥来源、价目、路由、静态调用方 |
//! | [`clock`] | 时钟（许可表的核验过期按「现在」判，测试要能拨） |
//!
//! 协议与传输（OpenAI 兼容的请求、SSE、usage 解析、超时、密钥不外泄）在 `idoris-upstream::cloud`；
//! 尝试记录与结算分录（SQLite，只追加，触发器兜底）在 `idoris-tenancy::attempts`。
//!
//! # 默认关闭
//!
//! 只有设了环境变量 [`config::CONFIG_ENV`]（`IDORIS_CLOUD_UPSTREAM_CONFIG`）、并且 `deploy_mode=tenant`
//! （云形态映射到既有的 `tenant`）才会启用；不设就是 `AppState::cloud == None`，`build_app` 挂的还是原来的
//! 处理函数，**现有的本地 oMLX 路径、订阅中转和全部既有 HTTP 行为一字不变**。个人形态永远不会把请求送到云上游
//! （「个人 iDoris 不改成 SaaS」，设计稿 §1 决定 4；`local_only` 失败即关闭）。
//!
//! # 交接给 S1 / S3 / S5 / S6
//!
//! - **S3**：实现 [`admission::CandidateAdmission`]（五项准入、有担保的负值、宽限），换掉 [`admission::SimpleAdmission`]；
//! - **S1 / S2 / S5**：[`config::CallerBook`] 是静态替身，换成按凭证解析的付款上下文（`resolve_payment_context`）
//!   和租户数据策略；云入口（`/api/v1`、从凭证推导租户）接在 `handler` 前面；
//! - **S6**：非流式回答在发布前停在 [`answer::PendingAnswer`]，完整输出检查放在 `publish()` 之前，不通过就
//!   `withhold(..)`（用户付 0）；edu 一律非流式的拒绝已经在 `handler` 里（先于任何上游调用）。

use std::path::Path;
use std::sync::Arc;

use idoris_compute_contracts::ids::TimestampMs;
use idoris_tenancy::attempts::AttemptStore;

pub mod admission;
pub mod answer;
pub mod clock;
pub mod config;
pub mod executor;
mod handler;
mod request;
mod run;

pub use admission::{AdmissionError, AdmissionRequest, CandidateAdmission, SimpleAdmission};
pub use answer::{AnswerMeta, CloudOutcome, CloudStream, PendingAnswer, PublishedAnswer};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{
    CallerBook, CallerPolicy, CloudLimits, CloudParts, EndpointRegistry, EndpointRuntime, RouteBook,
};
pub use executor::{CloudCall, CloudError, CloudExecutor, FailureInfo, status_allows_failover};
pub use run::Settled;

pub(crate) use handler::chat_completions;

/// 云上游：执行器 + 路由簿 + 调用方簿。挂在 `AppState.cloud` 上。
pub struct CloudUpstream {
    pub(crate) executor: CloudExecutor,
    pub(crate) callers: CallerBook,
    pub(crate) routes: RouteBook,
}

impl std::fmt::Debug for CloudUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudUpstream")
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl CloudUpstream {
    /// 用加载好的部件、尝试记录库、时钟和准入实现组装。
    pub fn new(
        parts: CloudParts,
        store: Arc<AttemptStore>,
        clock: Arc<dyn Clock>,
        admission: Arc<dyn CandidateAdmission>,
    ) -> Self {
        let CloudParts {
            permits,
            endpoints,
            routes,
            callers,
            limits,
            reserve_policy,
        } = parts;
        Self {
            executor: CloudExecutor::new(
                Arc::new(permits),
                Arc::new(endpoints),
                admission,
                store,
                clock,
                reserve_policy,
                limits,
            ),
            callers,
            routes,
        }
    }

    /// 从配置文件加载，用系统时钟和 S4 的简单准入；尝试记录库打开在 `db_path`（与其它 store 共用同一个 SQLite 文件）。
    pub fn from_file(path: &Path, db_path: &Path) -> Result<Self, String> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let parts = config::load(path, clock.now())?;
        let store = AttemptStore::open(db_path)
            .map_err(|e| format!("无法打开尝试记录数据库 \"{}\"：{e}", db_path.display()))?;
        Ok(Self::new(
            parts,
            Arc::new(store),
            clock,
            Arc::new(SimpleAdmission),
        ))
    }

    /// 读环境变量 [`config::CONFIG_ENV`]：没设（或为空）就是关闭（返回 `None`，什么都不改变）。
    /// 设了就必须是云形态（`deploy_mode=tenant`），否则启动失败：个人形态不会把请求送到云上游。
    pub fn from_env(
        deploy_mode: idoris_contracts::DeployMode,
    ) -> Result<Option<Arc<Self>>, String> {
        let path = crate::config::optional_env_path(config::CONFIG_ENV)?;
        Self::from_env_path(path, deploy_mode, crate::storage::process_db_path)
    }

    /// [`Self::from_env`] 的可测版本：配置路径与数据库路径由调用方给。
    /// 先查形态、后碰文件和数据库：个人形态下设了开关也不会去读任何东西。
    pub fn from_env_path(
        config_path: Option<std::path::PathBuf>,
        deploy_mode: idoris_contracts::DeployMode,
        db_path: impl FnOnce() -> Result<std::path::PathBuf, String>,
    ) -> Result<Option<Arc<Self>>, String> {
        let Some(path) = config_path else {
            return Ok(None);
        };
        if deploy_mode != idoris_contracts::DeployMode::Tenant {
            return Err(format!(
                "{} 只能在 IDORIS_DEPLOY_MODE=tenant（云形态）下启用：个人形态不会把请求送到云上游",
                config::CONFIG_ENV
            ));
        }
        Ok(Some(Arc::new(Self::from_file(&path, &db_path()?)?)))
    }

    /// 测试和装配用：直接取执行器。
    pub fn executor(&self) -> &CloudExecutor {
        &self.executor
    }

    /// 解析时用的「现在」：给静态付款上下文用（配置里的凭证永不过期，这里只是满足函数签名）。
    pub fn config_time() -> TimestampMs {
        SystemClock.now()
    }
}
