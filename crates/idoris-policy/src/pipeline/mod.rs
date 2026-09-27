//! 决策管道入口：隐私 → 角色/能力匹配 → 预算 → admission → 选择。
//!
//! 顺序即语义（`docs/iDoris-总体规划.md` 不变式 #1、接口规范 v1.1 §3.13）：
//! 每一段都是纯函数，按 Filter → Score → Pick 组织；同样的输入必须得到同样
//! 的输出（`tests/proptest_decide.rs` 覆盖）。
//!
//! 移植自 `packages/router/src/{policy,dispatch}.ts` 的顺序约束，`roles.ts`
//! 的角色匹配和 `packages/tenancy/src/budget.ts` 的预算过滤，融合成一个管道。
//! 数据类型定义在 [`types`]；这里只放阶段函数和管道入口 [`decide`]。

mod types;

pub use types::{Decision, Degradation, PolicyCtx, ReasonCode, Rejection, RequestProfile, Stage};

use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_contracts::provider::Locality;
use idoris_contracts::tenant::BudgetScope;

use crate::budget::BudgetSnapshot;
use crate::card::{AdmissionStatus, Card};
use crate::privacy::effective_served_locality;
use crate::role::{Role, is_eligible_for_role};

fn admission_rank(status: AdmissionStatus) -> u8 {
    match status {
        AdmissionStatus::Ready => 0,
        AdmissionStatus::RequiresEviction => 1,
        AdmissionStatus::Blocked => 2,
    }
}

fn has_capabilities(card: &Card, needed: &[Capability]) -> bool {
    needed
        .iter()
        .all(|c| card.component.provider.capabilities.contains(c))
}

fn cheapest_paid_cost(paid: &[&Card]) -> i64 {
    paid.iter()
        .filter_map(|c| c.estimated_cost_minor)
        .min()
        .unwrap_or(0)
}

fn budget_topup_hint(snapshot: &BudgetSnapshot, estimated_cost_minor: i64) -> String {
    format!(
        "预算不足：余额 {} minor units，本次预计花费 {estimated_cost_minor} minor units；\
         请充值，或在请求头声明 X-iDoris-Fallback 以改用本地路径。",
        snapshot.balance_minor(),
    )
}

/// [`budget_stage`] 的返回值：过闸后剩余的候选、本阶段追加的原因码、以及
/// （如果发生了）本阶段触发的降级。
type BudgetStageOutcome<'a> = (Vec<&'a Card>, Vec<ReasonCode>, Option<Degradation>);

/// 预算阶段（Filter）：只淘汰付费候选，绝不在剔除远程之后再静默改选本地——
/// 除非 `req.allow_fallback()`，且这次改选必须体现在返回的 [`Degradation`] 里。
fn budget_stage<'a>(
    req: &RequestProfile,
    ctx: &PolicyCtx<'_>,
    candidates: Vec<&'a Card>,
) -> Result<BudgetStageOutcome<'a>, Rejection> {
    let mut reasons = Vec::new();
    let snapshot = ctx
        .budget
        .zip(req.tenant_id.as_deref())
        .and_then(|(view, tenant)| view.snapshot(tenant));
    let Some(snapshot) = snapshot else {
        reasons.push(ReasonCode::BudgetNoTenantContext);
        return Ok((candidates, reasons, None));
    };
    if !snapshot.is_over() {
        reasons.push(ReasonCode::BudgetWithinLimit);
        return Ok((candidates, reasons, None));
    }

    let free: Vec<&Card> = candidates
        .iter()
        .copied()
        .filter(|c| c.estimated_cost_minor == Some(0))
        .collect();
    let paid: Vec<&Card> = candidates
        .iter()
        .copied()
        .filter(|c| c.estimated_cost_minor.is_some_and(|v| v > 0))
        .collect();

    if snapshot.scope == BudgetScope::All {
        // scope=all：预算耗尽是租户级冻结，连零成本候选都不放行，也没有可
        // 回落的「本地路径」概念——对齐 `budget.ts` 的 `scope === "all"` 分支。
        let estimated_cost_minor = cheapest_paid_cost(&paid);
        return Err(Rejection::BudgetExceeded {
            balance_minor: snapshot.balance_minor(),
            estimated_cost_minor,
            topup_hint: budget_topup_hint(&snapshot, estimated_cost_minor),
        });
    }

    if paid.is_empty() {
        // 这条路径本来就不需要付费，预算超限不影响它——不是降级。
        reasons.push(ReasonCode::BudgetWithinLimit);
        return Ok((free, reasons, None));
    }

    if req.allow_fallback() && !free.is_empty() {
        reasons.push(ReasonCode::BudgetFallbackToFreeCandidate);
        let estimated_cost_minor = cheapest_paid_cost(&paid);
        return Ok((
            free,
            reasons,
            Some(Degradation::BudgetFallback {
                estimated_cost_minor,
            }),
        ));
    }

    // 终态拒绝：不能在剔除远程候选之后再静默改选本地（不变式 #1）。
    let estimated_cost_minor = cheapest_paid_cost(&paid);
    Err(Rejection::BudgetExceeded {
        balance_minor: snapshot.balance_minor(),
        estimated_cost_minor,
        topup_hint: budget_topup_hint(&snapshot, estimated_cost_minor),
    })
}

/// 按 (admission 优先级, 估算成本, id 字典序) 排序取最小——最后一项永远打破
/// 平局，保证同样输入两次运行结果一致（确定性要求）。
fn pick<'a>(candidates: &[&'a Card]) -> Option<&'a Card> {
    candidates.iter().copied().min_by(|a, b| {
        admission_rank(a.admission_status)
            .cmp(&admission_rank(b.admission_status))
            .then_with(|| {
                a.estimated_cost_minor
                    .unwrap_or(0)
                    .cmp(&b.estimated_cost_minor.unwrap_or(0))
            })
            .then_with(|| a.id().cmp(b.id()))
    })
}

/// 决策管道入口：隐私 → 角色/能力匹配 → 预算 → admission → 选择。纯函数。
pub fn decide(
    req: &RequestProfile,
    cards: &[Card],
    ctx: &PolicyCtx<'_>,
) -> Result<Decision, Rejection> {
    let mut reasons = Vec::new();

    // ---- ① 隐私（不变式 #2：只能收紧，不能放宽） --------------------------
    let privacy = req.effective_privacy();
    if privacy == PrivacyClass::LocalOnly && req.content_tightening == Some(PrivacyClass::LocalOnly)
    {
        reasons.push(ReasonCode::PrivacyTightenedByContent);
    }
    let privacy_ok: Vec<&Card> = if privacy == PrivacyClass::LocalOnly {
        let loopback: Vec<&Card> = cards
            .iter()
            .filter(|c| effective_served_locality(c) == Locality::Loopback)
            .collect();
        if loopback.is_empty() {
            return Err(Rejection::LocalOnlyUnavailable);
        }
        reasons.push(ReasonCode::PrivacyLoopbackOnly);
        loopback
    } else {
        cards.iter().collect()
    };

    // ---- ② 角色 / 能力匹配 -------------------------------------------------
    let needed_capabilities = req.effective_capabilities();
    let role_matched: Vec<&Card> = match req.role {
        Some(role) if role.is_catalog_role() => privacy_ok
            .iter()
            .copied()
            .filter(|c| {
                is_eligible_for_role(c, role, ctx.min_ram_gb)
                    && has_capabilities(c, &needed_capabilities)
            })
            .collect(),
        _ => Vec::new(),
    };
    let (matched, role_degradation) = if !role_matched.is_empty() {
        if let Some(role) = req.role {
            reasons.push(ReasonCode::RoleMatched(role));
        }
        (role_matched, None)
    } else if req.role.is_some_and(Role::is_catalog_role) {
        // 请求指定了角色但没有候选命中；跨角色降级只在声明 Fallback 时才允许。
        #[allow(clippy::expect_used)] // 刚用 is_some_and 判过是 Some
        let requested = req.role.expect("role checked Some above");
        if !req.allow_fallback() {
            return Err(Rejection::NoEligibleCandidate {
                stage: Stage::RoleCapability,
            });
        }
        let capability_only: Vec<&Card> = privacy_ok
            .iter()
            .copied()
            .filter(|c| has_capabilities(c, &needed_capabilities))
            .collect();
        if capability_only.is_empty() {
            return Err(Rejection::NoEligibleCandidate {
                stage: Stage::RoleCapability,
            });
        }
        reasons.push(ReasonCode::RoleFallbackCapabilityOnly);
        (
            capability_only,
            Some(Degradation::RoleFallback { requested }),
        )
    } else {
        // 未声明角色（或 idoris/auto）：只按能力匹配，不算降级。
        let capability_only: Vec<&Card> = privacy_ok
            .iter()
            .copied()
            .filter(|c| has_capabilities(c, &needed_capabilities))
            .collect();
        if capability_only.is_empty() {
            return Err(Rejection::NoEligibleCandidate {
                stage: Stage::RoleCapability,
            });
        }
        (capability_only, None)
    };

    // ---- ③ 预算（只作用于付费候选；不变式 #3：价格未知 ≠ 免费） -----------
    // 价格未知（None）或非法（负数——调用方不应该产出，但类型层面不禁止）的
    // 候选一律剔除——本 crate 不做价格估算，宁可拒绝也不当成免费处理。
    let priced_known: Vec<&Card> = matched
        .into_iter()
        .filter(|c| c.estimated_cost_minor.is_some_and(|v| v >= 0))
        .collect();
    let (after_budget, budget_reasons, budget_degradation) = budget_stage(req, ctx, priced_known)?;
    reasons.extend(budget_reasons);

    let mut degradations = Vec::new();
    degradations.extend(role_degradation);
    degradations.extend(budget_degradation);

    // ---- ④ admission --------------------------------------------------------
    let admitted: Vec<&Card> = after_budget
        .into_iter()
        .filter(|c| c.admission_status != AdmissionStatus::Blocked)
        .collect();
    if admitted.is_empty() {
        return Err(Rejection::NoEligibleCandidate {
            stage: Stage::Admission,
        });
    }
    if admitted
        .iter()
        .any(|c| c.admission_status == AdmissionStatus::RequiresEviction)
    {
        reasons.push(ReasonCode::AdmissionRequiresEviction);
    }
    if admitted
        .iter()
        .any(|c| c.admission_status == AdmissionStatus::Ready)
    {
        reasons.push(ReasonCode::AdmissionReady);
    }

    // ---- ⑤ 选择（Score → Pick，确定性排序） ---------------------------------
    #[allow(clippy::expect_used)] // admitted 已确认非空
    let chosen = pick(&admitted).expect("admitted is non-empty, pick always returns Some");

    Ok(Decision {
        chosen_id: chosen.id().to_string(),
        reason_codes: reasons,
        degradations,
    })
}

#[cfg(test)]
mod tests;
