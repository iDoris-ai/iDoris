//! 决策管道入口：隐私 → 角色/能力匹配 → 价格过滤 → admission → 预算 → 选择。
//!
//! 顺序即语义（`docs/iDoris-总体规划.md` 不变式 #1、接口规范 v1.1 §3.13）：
//! 每一段都是纯函数，按 Filter → Score → Pick 组织；同样的输入必须得到同样
//! 的输出（`tests/proptest_decide.rs` 覆盖）。**L2**：admission 排在预算之前
//! 而不是之后——预算阶段要在"已经排除 admission=Blocked"的集合上用
//! [`pick`] 算出本应选中的路径，才能判断这条路径是否真的要花钱（H2 修复，
//! 见 [`budget_stage`] 文档）；一个反正会被 admission 挡掉的付费候选，不该
//! 在排除之前就先触发预算闸门。
//!
//! 移植自 `packages/router/src/{policy,dispatch}.ts` 的顺序约束，`roles.ts`
//! 的角色匹配和 `packages/tenancy/src/budget.ts` 的预算过滤，融合成一个管道。
//! 数据类型定义在 [`types`]；这里只放阶段函数和管道入口 [`decide`]。

mod types;

pub use types::{Decision, Degradation, PolicyCtx, ReasonCode, Rejection, RequestProfile, Stage};

use idoris_contracts::common::{Capability, PrivacyClass};
#[cfg(test)]
use idoris_contracts::provider::Locality;
use idoris_contracts::tenant::BudgetScope;

use crate::budget::BudgetSnapshot;
use crate::card::{AdmissionStatus, Card};
use crate::privacy::is_local_capable;
use crate::role::{Role, is_catalog_eligible, is_eligible_for_role};

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
///
/// `candidates` 必须是已经排除 admission=Blocked 之后的集合（见 [`decide`]）
/// ——H2 修复：预算闸看的是「按 [`pick`] 规则本应选中的那一条路径」，不是
/// 「候选集合里存不存在付费项」；一个从来不会被选中的付费候选（因为
/// admission 更差、成本更高……）不该触发 402 或假的 [`Degradation::BudgetFallback`]。
///
/// M2：`snapshot.is_over()` 只是租户级粗粒度信号，这里再叠加一次
/// `balance_minor() < 本次预估成本` 的判定；**这仍然只是只读预估**——
/// 是否真的扣得动、并发请求会不会先把余额抢走，以另一个 crate 的原子
/// `reserve`/`charge` 为准，这里给出的只是提前拦截的保守判断，不是最终真相。
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

    #[allow(clippy::expect_used)] // candidates 非空由调用方保证（admission 阶段已排除空集）
    let natural_pick = pick(&candidates).expect("candidates is non-empty");
    let natural_cost = natural_pick.estimated_cost_minor.unwrap_or(0);
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

    if snapshot.scope == BudgetScope::All && snapshot.is_over() {
        // scope=all：预算耗尽是租户级冻结，连零成本候选都不放行，也没有可
        // 回落的「本地路径」概念——对齐 `budget.ts` 的 `scope === "all"` 分支。
        // 这一条不看"本应选中谁"，无条件冻结。**只在 `is_over()` 时触发**：
        // M2 新增的"这一笔单独超过剩余余额"判定（下面 `this_pick_would_exceed`）
        // 不触发这条无条件冻结分支，走下面统一的 fallback/拒绝逻辑——scope=all
        // 的"连免费都不放行"是对齐 TS 原有的粗粒度冻结语义，M2 是纯本 crate
        // 新增的每笔精细化判定，两者不叠加。
        let estimated_cost_minor = cheapest_paid_cost(&paid);
        return Err(Rejection::BudgetExceeded {
            balance_minor: snapshot.balance_minor(),
            estimated_cost_minor,
            topup_hint: budget_topup_hint(&snapshot, estimated_cost_minor),
        });
    }

    let this_pick_would_exceed =
        natural_cost > 0 && (snapshot.is_over() || snapshot.balance_minor() < natural_cost);
    if !this_pick_would_exceed {
        // 本应选中的路径本来就不花钱，或者花的钱余额够——预算超限（如果有）
        // 不影响它，不是降级，也不剔除任何东西。
        reasons.push(ReasonCode::BudgetWithinLimit);
        return Ok((candidates, reasons, None));
    }

    if req.allow_fallback() && !free.is_empty() {
        reasons.push(ReasonCode::BudgetFallbackToFreeCandidate);
        return Ok((
            free,
            reasons,
            Some(Degradation::BudgetFallback {
                estimated_cost_minor: natural_cost,
            }),
        ));
    }

    // 终态拒绝：不能在剔除远程候选之后再静默改选本地（不变式 #1）。
    Err(Rejection::BudgetExceeded {
        balance_minor: snapshot.balance_minor(),
        estimated_cost_minor: natural_cost,
        topup_hint: budget_topup_hint(&snapshot, natural_cost),
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

/// 决策管道入口：隐私 → 角色/能力匹配 → 价格过滤 → admission → 预算 → 选择。
/// 纯函数。
pub fn decide(
    req: &RequestProfile,
    cards: &[Card],
    ctx: &PolicyCtx<'_>,
) -> Result<Decision, Rejection> {
    let mut reasons = Vec::new();

    // ---- ① 隐私（不变式 #2：只能收紧，不能放宽） --------------------------
    let privacy = req.effective_privacy();
    // 只有下限本来是 `Any`、真的被内容检查收紧成 `LocalOnly` 时才记这个
    // reason_code——下限本来就是 `LocalOnly` 时，内容检查同样返回
    // `LocalOnly` 不代表它"起了作用"，不该被算作一次收紧事件。
    let floor = req.task.privacy.unwrap_or(PrivacyClass::LocalOnly);
    if floor == PrivacyClass::Any && privacy == PrivacyClass::LocalOnly {
        reasons.push(ReasonCode::PrivacyTightenedByContent);
    }
    let privacy_ok: Vec<&Card> = if privacy == PrivacyClass::LocalOnly {
        let trusted_local: Vec<&Card> = cards.iter().filter(|c| is_local_capable(c)).collect();
        if trusted_local.is_empty() {
            return Err(Rejection::LocalOnlyUnavailable);
        }
        reasons.push(ReasonCode::PrivacyLoopbackOnly);
        trusted_local
    } else {
        cards.iter().collect()
    };

    // ---- ② 角色 / 能力匹配 -------------------------------------------------
    let needed_capabilities = req.effective_capabilities();
    // 三条不查具体角色成员关系的路径（无角色、`idoris/auto`、角色降级）都要
    // 经过同一个健康门槛（`is_catalog_eligible`：experiment/min_ram_gb），不能
    // 因为"没有指定角色"就绕过它——`is_eligible_for_role` 内部已经调用它，
    // 这里显式再调一次，保证四条路径口径一致。
    let is_capability_candidate = |c: &&Card| {
        is_catalog_eligible(c, ctx.min_ram_gb) && has_capabilities(c, &needed_capabilities)
    };
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
            .filter(is_capability_candidate)
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
            .filter(is_capability_candidate)
            .collect();
        if capability_only.is_empty() {
            return Err(Rejection::NoEligibleCandidate {
                stage: Stage::RoleCapability,
            });
        }
        (capability_only, None)
    };

    // ---- ③ 价格已知过滤（不变式 #3：价格未知 ≠ 免费） -----------------------
    // 价格未知（None）或非法（负数——调用方不应该产出，但类型层面不禁止）的
    // 候选一律剔除——本 crate 不做价格估算，宁可拒绝也不当成免费处理。
    let matched_count = matched.len();
    let priced_known: Vec<&Card> = matched
        .into_iter()
        .filter(|c| c.estimated_cost_minor.is_some_and(|v| v >= 0))
        .collect();
    if priced_known.is_empty() {
        // M1：matched 在这里必然非空（上面两个分支都在空时提前返回过了），
        // 所以这里一定是"全部因为价格未知/非法被剔除"——阶段码要如实反映
        // 真正原因，不能落到后面的 Admission（甚至 RoleCapability）身上。
        return Err(Rejection::NoEligibleCandidate {
            stage: Stage::Pricing,
        });
    }
    if priced_known.len() < matched_count {
        reasons.push(ReasonCode::PriceUnknownExcluded);
    }

    // ---- ④ admission（先排除 Blocked——H2：预算阶段要在这个集合上判断"本应
    // 选中谁"，不能反过来先按预算过滤再排除 admission，否则一个反正会被
    // admission 挡掉的付费候选也会错误地触发 402/假降级） -----------------------
    let admission_eligible: Vec<&Card> = priced_known
        .into_iter()
        .filter(|c| c.admission_status != AdmissionStatus::Blocked)
        .collect();
    if admission_eligible.is_empty() {
        return Err(Rejection::NoEligibleCandidate {
            stage: Stage::Admission,
        });
    }

    // ---- ⑤ 预算（只作用于"本应选中的那条路径"；见 [`budget_stage`]） ---------
    let (after_budget, budget_reasons, budget_degradation) =
        budget_stage(req, ctx, admission_eligible)?;
    reasons.extend(budget_reasons);

    let mut degradations = Vec::new();
    degradations.extend(role_degradation);
    degradations.extend(budget_degradation);

    if after_budget
        .iter()
        .any(|c| c.admission_status == AdmissionStatus::RequiresEviction)
    {
        reasons.push(ReasonCode::AdmissionRequiresEviction);
    }
    if after_budget
        .iter()
        .any(|c| c.admission_status == AdmissionStatus::Ready)
    {
        reasons.push(ReasonCode::AdmissionReady);
    }

    // ---- ⑥ 选择（Score → Pick，确定性排序） ---------------------------------
    #[allow(clippy::expect_used)]
    // after_budget 非空：budget_stage 只会原样返回非空输入或返回非空的 free 子集
    let chosen = pick(&after_budget).expect("after_budget is non-empty, pick always returns Some");

    Ok(Decision {
        chosen_id: chosen.id().to_string(),
        reason_codes: reasons,
        degradations,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod local_capable_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::card::test_support::sample_card;
    use idoris_contracts::TaskProfile;
    use idoris_contracts::common::FallbackPolicy;
    use idoris_contracts::component_card::Egress;

    fn request(privacy: Option<PrivacyClass>) -> RequestProfile {
        RequestProfile {
            task: TaskProfile {
                privacy,
                fallback: Some(FallbackPolicy::NextInChain),
                ..Default::default()
            },
            role: None,
            tenant_id: None,
            content_tightening: None,
        }
    }

    #[test]
    fn local_only_pipeline_rechecks_each_condition_without_registration() {
        let trusted = sample_card("trusted", &[]);
        let mut untrusted = trusted.clone();
        untrusted.component.privacy_class = PrivacyClass::Any;
        let mut lan = trusted.clone();
        lan.component.allowed_egress = vec![Egress::Loopback, Egress::Lan];
        let mut internet = trusted.clone();
        internet.component.allowed_egress = vec![Egress::Internet];
        for mut card in [untrusted, lan, internet] {
            card.component.provider.id = "a-untrusted".to_string();
            for privacy in [None, Some(PrivacyClass::LocalOnly)] {
                assert_eq!(
                    decide(&request(privacy), &[card.clone()], &PolicyCtx::default()),
                    Err(Rejection::LocalOnlyUnavailable)
                );
                let chosen = decide(
                    &request(privacy),
                    &[card.clone(), trusted.clone()],
                    &PolicyCtx::default(),
                )
                .unwrap();
                assert_eq!(chosen.chosen_id, "trusted");
            }
            let unrestricted = request(Some(PrivacyClass::Any));
            assert!(decide(&unrestricted, &[card], &PolicyCtx::default()).is_ok());
        }
    }

    #[test]
    fn content_tightening_uses_the_same_local_capable_gate() {
        let mut card = sample_card("untrusted", &[]);
        card.component.privacy_class = PrivacyClass::Any;
        let mut req = request(Some(PrivacyClass::Any));
        req.content_tightening = Some(PrivacyClass::LocalOnly);
        assert_eq!(
            decide(&req, &[card], &PolicyCtx::default()),
            Err(Rejection::LocalOnlyUnavailable)
        );
    }
}
