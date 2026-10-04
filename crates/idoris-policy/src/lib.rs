//! R2-B：纯函数的准入与路由决策管道。**没有 IO，没有随机数**。
//!
//! TS 参考实现（porting 时的行为基准）：`packages/router/src/{policy,dispatch,
//! locality,registry,roles}.ts` + `packages/tenancy/src/budget.ts`。这些文件
//! 目前在 `main` 分支；本 crate 所在的 Rust 骨架分支线尚未合并它们，读取时用
//! `git show origin/main:<path>` 而不是假设它们在本分支的工作区里存在。
//!
//! 目前有决策候选类型 [`Card`]、完整的 `role` 模块（解析 + 目录候选筛选）、
//! 隐私判定 [`privacy`]、注册校验 [`registry`]、预算查询接口 [`budget`]、
//! 以及完整的决策管道 [`decide`]（这个 crate 按依赖顺序叠加的多个 PR
//! landing，场景测试在下一个 PR 里加入）。

pub mod budget;
pub mod card;
pub mod pipeline;
pub mod privacy;
pub mod registry;
pub mod role;

pub use budget::{BudgetSnapshot, BudgetView};
pub use card::{AdmissionStatus, Card};
pub use pipeline::{
    Decision, Degradation, PolicyCtx, ReasonCode, Rejection, RequestProfile, Stage, decide,
};
pub use privacy::{
    SUBSCRIPTION_PROVIDER_ID, effective_privacy, effective_served_locality,
    is_subscription_provider_id,
};
pub use registry::{RegistrationError, parse_loopback_endpoint_url, validate_registration};
pub use role::{
    ROLES, Role, RoleParseError, is_catalog_eligible, is_catalog_metadata_eligible,
    is_eligible_for_role, is_role_metadata_eligible, parse_model_role,
};
/// 重新导出，好让"执行层"调用 [`parse_loopback_endpoint_url`] 时不需要自己
/// 再引入 `url` 依赖（也避免两边的 `url` 版本不一致导致类型对不上）。
pub use url::Url;
