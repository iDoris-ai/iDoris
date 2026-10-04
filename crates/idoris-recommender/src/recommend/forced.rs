use crate::catalog::Catalog;
use crate::probe::HostFacts;

use super::quant::{lowest_footprint_pick, pick_quant, quant_by_label};
use super::resident::budget_breakdown;
use super::types::{CoreOverride, ForcedResult, RecommenderPolicy, ResidentChoice};

pub fn apply_core_override(
    catalog: &Catalog,
    host: &HostFacts,
    policy: &RecommenderPolicy,
    forced: Option<&str>,
    current_resident: Option<ResidentChoice>,
) -> ForcedResult {
    let Some(forced) = forced.map(str::trim).filter(|value| !value.is_empty()) else {
        return ForcedResult {
            resident: current_resident,
            override_choice: None,
            warnings: Vec::new(),
        };
    };

    let mut parts = forced.split('@');
    let forced_id = parts.next().unwrap_or_default();
    let forced_quant = parts.next();
    let Some(model) = catalog.catalog.iter().find(|model| model.id == forced_id) else {
        return ForcedResult {
            resident: current_resident,
            override_choice: None,
            warnings: vec![format!(
                "IDORIS_CORE_MODEL={forced} 不在目录中：忽略 override，回落自动选择"
            )],
        };
    };

    let resident_budget = budget_breakdown(host, policy).resident_budget_gb;
    let mut warnings = Vec::new();
    let mut pick = forced_quant.and_then(|label| quant_by_label(model, label, policy));
    let mut used_lowest_fallback = false;
    if let Some(label) = forced_quant
        && pick.is_none()
    {
        warnings.push(format!(
            "IDORIS_CORE_MODEL 指定的量化 {label} 不存在：改用自动量化"
        ));
    }
    if pick.is_none() {
        pick = pick_quant(model, resident_budget, policy, 0.0);
    }
    if pick.is_none() {
        pick = lowest_footprint_pick(model, policy);
        used_lowest_fallback = pick.is_some();
    }
    let Some(pick) = pick else {
        return ForcedResult {
            resident: current_resident,
            override_choice: None,
            warnings: vec![format!("IDORIS_CORE_MODEL={forced} 没有可用量化")],
        };
    };

    if used_lowest_fallback {
        warnings.push(format!(
            "IDORIS_CORE_MODEL={} 放不下常驻预算（{resident_budget:.2}GB）：已强制放行，请自行承担 OOM 风险",
            model.id
        ));
    } else if pick.footprint_gb > resident_budget {
        warnings.push(format!(
            "IDORIS_CORE_MODEL={} footprint {:.2}GB > 常驻预算 {resident_budget:.2}GB",
            model.id, pick.footprint_gb
        ));
    }
    if host.ram_gb < model.min_ram_gb {
        warnings.push(format!(
            "IDORIS_CORE_MODEL={} 绕过 min_ram_gb 硬门槛（需 {}GB，实际 {}GB）",
            model.id, model.min_ram_gb, host.ram_gb
        ));
    }
    warnings.push(format!(
        "IDORIS_CORE_MODEL={forced} 生效：推荐模块让路（yields），不覆盖用户强制"
    ));

    let reasoning = model
        .capability
        .as_ref()
        .and_then(|capability| capability.get("reasoning"))
        .copied()
        .unwrap_or(0.0);
    ForcedResult {
        resident: Some(ResidentChoice::from_pick(
            model.id.clone(),
            policy.context_target,
            reasoning * pick.quality,
            pick,
        )),
        override_choice: Some(CoreOverride {
            id: model.id.clone(),
            active: true,
        }),
        warnings,
    }
}
