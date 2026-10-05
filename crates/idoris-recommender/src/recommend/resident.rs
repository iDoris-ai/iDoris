use idoris_policy::Role;

use crate::catalog::{Catalog, CatalogRole};
use crate::memory::{apple_reserve_gb, apple_usable_gb};
use crate::probe::HostFacts;
use crate::roles::role_candidates;

use super::quant::{lowest_footprint_pick, pick_quant};
use super::types::{
    BlockedChoice, BudgetBreakdown, HEADROOM_GB, RecommenderPolicy, ResidentChoice, ResidentError,
    TEMP_SLOT_RESERVE_GB,
};

pub fn budget_breakdown(host: &HostFacts, policy: &RecommenderPolicy) -> BudgetBreakdown {
    let reserve_gb = apple_reserve_gb(host.ram_gb);
    let usable_gb = apple_usable_gb(host.ram_gb, policy.wired_mode);
    let temp_reserve_gb = f64::from(policy.temp_slots) * TEMP_SLOT_RESERVE_GB;
    BudgetBreakdown {
        reserve_gb,
        usable_gb,
        temp_reserve_gb,
        resident_budget_gb: usable_gb - temp_reserve_gb - HEADROOM_GB,
    }
}

pub fn blocked_choices(
    catalog: &Catalog,
    host: &HostFacts,
    policy: &RecommenderPolicy,
) -> Result<Vec<BlockedChoice>, ResidentError> {
    let mut blocked = Vec::new();
    for model in &catalog.catalog {
        if host.ram_gb >= model.min_ram_gb {
            continue;
        }
        let pick = lowest_footprint_pick(model, policy).ok_or_else(|| {
            ResidentError::EmptyQuantOptions {
                id: model.id.clone(),
            }
        })?;
        blocked.push(BlockedChoice {
            id: model.id.clone(),
            min_ram_gb: model.min_ram_gb,
            estimated_memory_gb: pick.footprint_gb,
            reason: format!("min_ram_gb={} > ram_gb={}", model.min_ram_gb, host.ram_gb),
        });
    }
    Ok(blocked)
}

pub fn choose_resident(
    catalog: &Catalog,
    host: &HostFacts,
    policy: &RecommenderPolicy,
) -> Option<ResidentChoice> {
    let daily = CatalogRole::new(Role::Daily).ok()?;
    let candidates = role_candidates(catalog, daily, None, Some(host.ram_gb));
    let budget = budget_breakdown(host, policy).resident_budget_gb;
    let mut best: Option<ResidentChoice> = None;

    for id in candidates {
        let Some(model) = catalog.catalog.iter().find(|model| model.id == id) else {
            continue;
        };
        let Some(pick) = pick_quant(model, budget, policy, policy.quality_threshold) else {
            continue;
        };
        let reasoning = model
            .capability
            .as_ref()
            .and_then(|capability| capability.get("reasoning"))
            .copied()
            .unwrap_or(0.0);
        let score = reasoning * pick.quality;
        let replace = best.as_ref().is_none_or(|current| {
            score > current.score
                || (score == current.score && pick.footprint_gb < current.footprint_gb)
        });
        if replace {
            best = Some(ResidentChoice::from_pick(
                model.id.clone(),
                policy.context_target,
                score,
                pick,
            ));
        }
    }
    best
}
