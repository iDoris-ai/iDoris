use std::path::Path;

use crate::catalog::{Catalog, load_catalog};
use crate::memory::{KvQuant, WiredMode, apple_usable_gb, recommended_wired_limit_mb};
use crate::probe::HostFacts;

use super::forced::apply_core_override;
use super::resident::{blocked_choices, budget_breakdown, choose_resident};
use super::temp::temp_admission;
use super::types::{
    BudgetBreakdown, HEADROOM_GB, PartialPolicy, RecommendError, Recommendation, RecommenderPolicy,
    SysctlRecommendation, TempStatus,
};

/// Recommend within an externally allocated TOTAL when `Some(bytes)` is supplied.
/// The allocation replaces host usable memory; host facts still gate `min_ram_gb`.
/// Bytes convert to decimal GB once here via `budget_breakdown`. Footprints are
/// weights + KV at policy context + 1GB overhead; loading peaks are not modeled.
/// `None` preserves TS parity, including oversized temp/forced legacy exceptions.
pub fn recommend(
    hardware: &HostFacts,
    catalog: &Catalog,
    partial_policy: Option<&PartialPolicy>,
    forced: Option<&str>,
    allocated_budget_bytes: Option<u64>,
) -> Result<Recommendation, RecommendError> {
    let policy = RecommenderPolicy::merged(partial_policy);
    let budget = budget_breakdown(hardware, &policy, allocated_budget_bytes);
    let mut blocked = blocked_choices(catalog, hardware, &policy, &budget)?;
    let automatic = choose_resident(catalog, hardware, &policy, &budget);
    let automatic_score = automatic.as_ref().map(|current| current.score);
    let forced_result = apply_core_override(catalog, hardware, &policy, forced, automatic, &budget);
    let headroom_breach_option = if forced_result.override_choice.is_some() {
        None
    } else {
        // resident_budget + HEADROOM == usable - temp_reserve, never above usable.
        let relaxed = BudgetBreakdown {
            resident_budget_gb: (budget.resident_budget_gb + HEADROOM_GB).min(budget.usable_gb),
            ..budget
        };
        choose_resident(catalog, hardware, &policy, &relaxed)
            .filter(|candidate| automatic_score.is_none_or(|current| candidate.score > current))
    };
    let resident = forced_result.resident;
    let temp_result = temp_admission(catalog, hardware, &policy, resident.as_ref(), &budget);
    for (id, alternative) in &temp_result.blocked_alternatives {
        if let Some(entry) = blocked.iter_mut().find(|entry| &entry.id == id) {
            entry.degraded_alternatives.push(alternative.clone());
        }
    }

    let budget_source = allocated_budget_bytes.map_or_else(
        || format!("{}GB {}", compact(hardware.ram_gb), wired_mode(&policy)),
        |bytes| format!("外部分配 {bytes} bytes"),
    );
    let mut warnings = vec![format!(
        "预算: {} → usable {}GB（reserve {}GB）；常驻预算 {}GB（预留临时 {}GB + headroom {}GB）",
        budget_source,
        fixed2(budget.usable_gb),
        fixed2(budget.reserve_gb),
        fixed2(budget.resident_budget_gb),
        fixed2(budget.temp_reserve_gb),
        fixed2(HEADROOM_GB),
    )];
    warnings.extend(
        blocked
            .iter()
            .map(|choice| format!("{}: BLOCKED（{}）", choice.id, choice.reason)),
    );
    warnings.extend(forced_result.warnings);
    warnings.extend(temp_result.warnings);
    if resident.is_none() {
        warnings.push("没有模型能放进常驻预算：请提高 wired_mode 或降低 context_target".into());
    }

    let resident_label = resident
        .as_ref()
        .map(|choice| format!("{}@{}", choice.id, choice.label));
    let tradeoff = build_tradeoff(
        hardware,
        &policy,
        &budget,
        resident.as_ref(),
        resident_label.as_deref(),
        &temp_result.choices,
        &blocked,
    );

    let host_usable_gb = apple_usable_gb(hardware.ram_gb, policy.wired_mode);
    Ok(Recommendation {
        hardware: hardware.clone(),
        policy,
        usable_gb: budget.usable_gb,
        reserve_gb: budget.reserve_gb,
        temp_reserve_gb: budget.temp_reserve_gb,
        resident_budget_gb: budget.resident_budget_gb,
        resident,
        resident_label,
        temp: temp_result.choices,
        blocked,
        warnings,
        recommended_sysctl: SysctlRecommendation {
            iogpu_wired_limit_mb: recommended_wired_limit_mb(host_usable_gb),
        },
        tradeoff,
        override_choice: forced_result.override_choice,
        headroom_breach_option,
    })
}

/// Load a catalog and apply the same budget contract as [`recommend`].
pub fn recommend_from_file(
    path: impl AsRef<Path>,
    hardware: &HostFacts,
    partial_policy: Option<&PartialPolicy>,
    forced: Option<&str>,
    allocated_budget_bytes: Option<u64>,
) -> Result<Recommendation, RecommendError> {
    let catalog = load_catalog(path)?;
    recommend(
        hardware,
        &catalog,
        partial_policy,
        forced,
        allocated_budget_bytes,
    )
}

fn build_tradeoff(
    hardware: &HostFacts,
    policy: &RecommenderPolicy,
    budget: &super::types::BudgetBreakdown,
    resident: Option<&super::types::ResidentChoice>,
    resident_label: Option<&str>,
    temp: &[super::types::TempChoice],
    blocked: &[super::types::BlockedChoice],
) -> String {
    let mut lines = vec![format!(
        "硬件 {} / {}GB（{}，ctx {}，KV {}）",
        hardware.chip,
        compact(hardware.ram_gb),
        wired_mode(policy),
        policy.context_target,
        kv_quant(policy),
    )];
    if let Some(resident) = resident {
        lines.push(format!(
            "常驻 {}：权重 {}GB + KV {}GB + 开销 = {}GB，预算 {}GB；能力×质量={}",
            resident_label.unwrap_or("?"),
            fixed2(resident.weights_gb),
            fixed2(resident.kv_gb),
            fixed2(resident.footprint_gb),
            fixed2(budget.resident_budget_gb),
            fixed4(resident.score),
        ));
    } else {
        lines.push(format!(
            "常驻：无（预算 {}GB 放不下任何 daily 模型）",
            fixed2(budget.resident_budget_gb)
        ));
    }
    let ready = temp
        .iter()
        .filter(|choice| choice.status == TempStatus::Ready)
        .map(|choice| format!("{}@{}", choice.id, choice.quant.label))
        .collect::<Vec<_>>();
    if !ready.is_empty() {
        lines.push(format!("共存临时：{}", ready.join("、")));
    }
    let evict = temp
        .iter()
        .filter(|choice| choice.status == TempStatus::RequiresEviction)
        .map(|choice| format!("{}@{}", choice.id, choice.quant.label))
        .collect::<Vec<_>>();
    if !evict.is_empty() {
        lines.push(format!("需驱逐：{}", evict.join("、")));
    }
    lines.push(if blocked.is_empty() {
        "BLOCKED：无".into()
    } else {
        format!(
            "BLOCKED：{}",
            blocked
                .iter()
                .map(|choice| {
                    if budget.allocated_budget_bytes.is_some() {
                        format!("{}({})", choice.id, choice.reason)
                    } else {
                        format!("{}(需{}GB)", choice.id, compact(choice.min_ram_gb))
                    }
                })
                .collect::<Vec<_>>()
                .join("、")
        )
    });
    lines.push(format!(
        "预算：usable {}GB（reserve {}GB，临时预留 {}GB）",
        fixed2(budget.usable_gb),
        fixed2(budget.reserve_gb),
        fixed2(budget.temp_reserve_gb),
    ));
    lines.join("\n")
}

fn wired_mode(policy: &RecommenderPolicy) -> &'static str {
    match policy.wired_mode {
        WiredMode::Conservative => "conservative",
        WiredMode::Moderate => "moderate",
        WiredMode::Aggressive => "aggressive",
    }
}

fn kv_quant(policy: &RecommenderPolicy) -> &'static str {
    match policy.kv_quant {
        KvQuant::Fp16 => "fp16",
        KvQuant::Q8 => "q8",
        KvQuant::Q4 => "q4",
    }
}

fn fixed2(value: f64) -> String {
    format!("{value:.2}")
}

fn fixed4(value: f64) -> String {
    format!("{value:.4}")
}

fn compact(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_precision_matches_javascript_boundary_expectations() {
        assert_eq!(fixed2(1.005), "1.00");
        assert_eq!(fixed2(2.675), "2.67");
        assert_eq!(fixed2(1.335), "1.33");
        assert_eq!(fixed2(15.845), "15.85");
        assert_eq!(fixed4(0.12345), "0.1235");
    }
}
