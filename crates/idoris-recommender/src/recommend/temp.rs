use idoris_contracts::common::Capability;

use crate::catalog::{Catalog, CatalogModel};
use crate::probe::HostFacts;

use super::quant::{lowest_footprint_pick, pick_quant};
use super::types::{
    BudgetBreakdown, QuantPick, RecommenderPolicy, ResidentChoice, TempAlternative, TempChoice,
    TempResult, TempStatus,
};

pub fn temp_admission(
    catalog: &Catalog,
    host: &HostFacts,
    policy: &RecommenderPolicy,
    resident: Option<&ResidentChoice>,
    budget: &BudgetBreakdown,
) -> TempResult {
    let usable = budget.usable_gb;
    let resident_footprint = resident.map_or(0.0, |choice| choice.footprint_gb);
    let remaining = usable - resident_footprint;
    let mut choices = Vec::new();
    let mut warnings = Vec::new();
    let mut blocked_alternatives = Vec::new();

    for capability in &policy.needed_capabilities {
        let Some(model) = best_model_for_capability(catalog, host, resident, *capability) else {
            continue;
        };
        let key = capability_key(*capability);
        let fitting = pick_quant(model, remaining, policy, policy.quality_threshold);
        let fits_quality_and_remaining = fitting.is_some();
        let Some(quant) = fitting.or_else(|| {
            pick_quant(model, usable, policy, policy.quality_threshold)
                .or_else(|| lowest_footprint_pick(model, policy))
        }) else {
            continue;
        };
        // `blocked_choices` exposes these models as unavailable. Eviction cannot
        // increase the allocation. Preserve the unbudgeted TS fixture exception.
        let allocated = budget.allocated_budget_bytes.is_some();
        if allocated && quant.footprint_gb > usable {
            if let Some(alt) = degraded_alternative(
                catalog,
                host,
                policy,
                resident,
                *capability,
                model,
                remaining,
            ) {
                blocked_alternatives.push((model.id.clone(), alt));
            }
            continue;
        }
        if fits_quality_and_remaining || (allocated && quant.footprint_gb <= remaining) {
            choices.push(TempChoice {
                id: model.id.clone(),
                capability: *capability,
                status: TempStatus::Ready,
                min_ram_gb: model.min_ram_gb,
                reason: format!(
                    "footprint {:.2}GB ≤ 剩余 {:.2}GB，可与常驻共存",
                    quant.footprint_gb, remaining
                ),
                quant,
                degraded_alternative: None,
            });
            continue;
        }

        warnings.push(format!(
            "{key}: {} 需驱逐常驻（{:.2}GB > 剩余 {:.2}GB）",
            model.id, quant.footprint_gb, remaining
        ));
        let alternative = degraded_alternative(
            catalog,
            host,
            policy,
            resident,
            *capability,
            model,
            remaining,
        );
        choices.push(TempChoice {
            id: model.id.clone(),
            capability: *capability,
            status: TempStatus::RequiresEviction,
            min_ram_gb: model.min_ram_gb,
            reason: format!(
                "footprint {:.2}GB > 剩余 {:.2}GB，需驱逐常驻后加载",
                quant.footprint_gb, remaining
            ),
            quant,
            degraded_alternative: alternative,
        });
    }

    TempResult {
        choices,
        warnings,
        blocked_alternatives,
    }
}

/// Best-scoring other eligible model for `capability` that fits `remaining`
/// without eviction. Eligibility matches `best_model_for_capability`.
fn degraded_alternative(
    catalog: &Catalog,
    host: &HostFacts,
    policy: &RecommenderPolicy,
    resident: Option<&ResidentChoice>,
    capability: Capability,
    best: &CatalogModel,
    remaining: f64,
) -> Option<TempAlternative> {
    let key = capability_key(capability);
    let mut found: Option<(f64, &CatalogModel, QuantPick)> = None;
    for model in &catalog.catalog {
        if model.id == best.id || !is_eligible(model, host, resident) {
            continue;
        }
        let value = model
            .capability
            .as_ref()
            .and_then(|scores| scores.get(key))
            .copied()
            .unwrap_or(0.0);
        if value <= 0.0 || found.as_ref().is_some_and(|(v, _, _)| value <= *v) {
            continue;
        }
        if let Some(pick) = pick_quant(model, remaining, policy, policy.quality_threshold) {
            found = Some((value, model, pick));
        }
    }
    found.map(|(_, model, quant)| TempAlternative {
        id: model.id.clone(),
        reason: format!(
            "不驱逐降级：footprint {:.2}GB ≤ 剩余 {:.2}GB",
            quant.footprint_gb, remaining
        ),
        quant,
    })
}

fn is_eligible(model: &CatalogModel, host: &HostFacts, resident: Option<&ResidentChoice>) -> bool {
    model.status.as_deref() != Some("experiment")
        && host.ram_gb >= model.min_ram_gb
        && resident.is_none_or(|choice| choice.id != model.id)
}

fn best_model_for_capability<'a>(
    catalog: &'a Catalog,
    host: &HostFacts,
    resident: Option<&ResidentChoice>,
    capability: Capability,
) -> Option<&'a CatalogModel> {
    let key = capability_key(capability);
    let mut best = None;
    let mut best_value = -1.0;
    for model in &catalog.catalog {
        if !is_eligible(model, host, resident) {
            continue;
        }
        let value = model
            .capability
            .as_ref()
            .and_then(|scores| scores.get(key))
            .copied()
            .unwrap_or(0.0);
        if value > 0.0 && value > best_value {
            best = Some(model);
            best_value = value;
        }
    }
    best
}

fn capability_key(capability: Capability) -> &'static str {
    match capability {
        Capability::Chat => "chat",
        Capability::Reasoning => "reasoning",
        Capability::Vision => "vision",
        Capability::Asr => "asr",
        Capability::Tts => "tts",
        Capability::Coding => "coding",
        Capability::Embedding => "embedding",
        Capability::Rerank => "rerank",
    }
}
