//! 管道用到的数据类型：请求画像、上下文、决策结果、拒绝原因。
//! 逻辑（`decide`/`budget_stage`/`pick`）在同目录的 `mod.rs` 里。

use idoris_contracts::TaskProfile;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass};

use crate::budget::BudgetView;
use crate::privacy::effective_privacy;
use crate::role::Role;

/// 调用方声明的请求画像 + 本次决策专属的上下文（角色、租户、内容收紧结果）。
///
/// `task` 复用 [`idoris_contracts::TaskProfile`]（privacy/intent/complexity/
/// capabilities/fallback），避免重复定义同一组字段。未声明字段按
/// `task_profile.rs` 文档的缺省值处理：privacy 缺省 `local_only`
/// （fail-closed）；capabilities 缺省 `[chat]`。
#[derive(Debug, Clone)]
pub struct RequestProfile {
    pub task: TaskProfile,
    /// `model=idoris/<role>` 的解析结果（[`crate::role::parse_model_role`]）。
    /// `None` 或 `Some(Role::Auto)` 都表示「不按角色收紧候选，只按能力匹配」
    /// ——`auto` 是「交给 iDoris 选」，本管道把它当作没有强制角色约束处理。
    pub role: Option<Role>,
    pub tenant_id: Option<String>,
    /// 闸一内容检查的收紧结果（v1.1 §3.13：只能让隐私更严格，不能放宽）。
    pub content_tightening: Option<PrivacyClass>,
}

// 这几个方法目前只被 `pipeline::decide()` 使用，而 `decide()` 在下一个 PR
// 里才加入（这个 PR 只落地数据类型）——`pub(super)` 已经限定了可见性范围，
// 这里的 `dead_code` allow 只是暂时的，等 mod.rs 落地 decide() 就会自然解除。
#[allow(dead_code)]
impl RequestProfile {
    pub(super) fn effective_privacy(&self) -> PrivacyClass {
        effective_privacy(
            self.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
            self.content_tightening,
        )
    }

    pub(super) fn effective_capabilities(&self) -> Vec<Capability> {
        self.task
            .capabilities
            .clone()
            .unwrap_or_else(|| vec![Capability::Chat])
    }

    /// 请求是否显式声明允许降级（对应 `X-iDoris-Fallback` 请求头）。跨角色降级
    /// 和预算超限后的本地回落都只在这里返回 `true` 时才允许。
    pub(super) fn allow_fallback(&self) -> bool {
        matches!(self.task.fallback, Some(FallbackPolicy::NextInChain))
    }
}

/// 决策管道运行所需的、与单次请求无关的上下文。
#[derive(Clone, Copy, Default)]
pub struct PolicyCtx<'a> {
    /// 目录/硬件门槛（`recommend.ts` 的 `minRamGb`）。省略表示不做硬件过滤。
    pub min_ram_gb: Option<f64>,
    pub budget: Option<&'a dyn BudgetView>,
}

/// 管道中候选集合变空的阶段，用于 [`Rejection::NoEligibleCandidate`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    RoleCapability,
    Admission,
}

/// 决策过程中记录的原因，便于回放（不做任何格式化/展示决定，纯数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasonCode {
    PrivacyLoopbackOnly,
    PrivacyTightenedByContent,
    RoleMatched(Role),
    RoleFallbackCapabilityOnly,
    BudgetWithinLimit,
    BudgetNoTenantContext,
    BudgetFallbackToFreeCandidate,
    AdmissionReady,
    AdmissionRequiresEviction,
}

/// 一次显式的降级（只有 `allow_fallback()` 时才会发生，且必须体现在这里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Degradation {
    /// 请求指定的角色没有候选命中，退化为纯能力匹配。
    RoleFallback { requested: Role },
    /// 所需路径的付费候选超预算，退回免费/本地候选。
    BudgetFallback { estimated_cost_minor: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub chosen_id: String,
    pub reason_codes: Vec<ReasonCode>,
    pub degradations: Vec<Degradation>,
}

impl Decision {
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
    }
}

/// 拒绝原因，映射到接口规范 §3.11 的统一错误体（`{error:{type,...}}`）。
#[derive(Debug, Clone, PartialEq)]
pub enum Rejection {
    /// 400 `unknown_role`（多数情况下由 [`crate::role::parse_model_role`] 在
    /// 调用 `decide` 之前产生，通过 `From<RoleParseError>` 转换过来）。
    UnknownRole { requested: String },
    /// 503 `local_only_unavailable`：`local_only` 请求没有可信本地候选。
    LocalOnlyUnavailable,
    /// 402 `budget_exceeded`：终态拒绝，不是降级。
    BudgetExceeded {
        balance_minor: i64,
        estimated_cost_minor: i64,
        topup_hint: String,
    },
    /// 某一阶段过滤后候选集合为空，且没有（或用不上）显式 Fallback 声明。
    NoEligibleCandidate { stage: Stage },
}

impl Rejection {
    pub fn http_status(&self) -> u16 {
        match self {
            Rejection::UnknownRole { .. } => 400,
            Rejection::LocalOnlyUnavailable | Rejection::NoEligibleCandidate { .. } => 503,
            Rejection::BudgetExceeded { .. } => 402,
        }
    }

    /// 接口规范 §3.11 统一错误体的 `type` 字段。
    pub fn error_type(&self) -> &'static str {
        match self {
            Rejection::UnknownRole { .. } => "unknown_role",
            Rejection::LocalOnlyUnavailable => "local_only_unavailable",
            Rejection::BudgetExceeded { .. } => "budget_exceeded",
            Rejection::NoEligibleCandidate { .. } => "no_eligible_candidate",
        }
    }
}

impl From<crate::role::RoleParseError> for Rejection {
    fn from(err: crate::role::RoleParseError) -> Self {
        Rejection::UnknownRole {
            requested: err.requested,
        }
    }
}
