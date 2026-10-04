use idoris_contracts::common::Capability;

use crate::catalog::{Catalog, CatalogModel};
use crate::probe::HostFacts;

use super::quant::{lowest_footprint_pick, pick_quant};
use super::resident::budget_breakdown;
use super::types::{RecommenderPolicy, ResidentChoice, TempChoice, TempResult, TempStatus};

pub fn temp_admission(
    catalog: &Catalog,
    host: &HostFacts,
    policy: &RecommenderPolicy,
    resident: Option<&ResidentChoice>,
) -> TempResult {
    let usable = budget_breakdown(host, policy).usable_gb;
    let resident_footprint = resident.map_or(0.0, |choice| choice.footprint_gb);
    let remaining = usable - resident_footprint;
    let mut choices = Vec::new();
    let mut warnings = Vec::new();

    for capability in &policy.needed_capabilities {
        let Some(model) = best_model_for_capability(catalog, host, resident, *capability) else {
            continue;
        };
        let key = capability_key(*capability);
        if let Some(quant) = pick_quant(model, remaining, policy, policy.quality_threshold) {
            choices.push(TempChoice {
                id: model.id.clone(),
                capability: *capability,
                status: TempStatus::Ready,
                min_ram_gb: model.min_ram_gb,
                reason: format!(
                    "footprint {:.2}GB <= remaining {:.2}GB",
                    quant.footprint_gb, remaining
                ),
                quant,
            });
            continue;
        }

        let Some(quant) = pick_quant(model, usable, policy, policy.quality_threshold)
            .or_else(|| lowest_footprint_pick(model, policy))
        else {
            continue;
        };
        warnings.push(format!(
            "{key}: {} requires resident eviction ({:.2}GB > remaining {:.2}GB)",
            model.id, quant.footprint_gb, remaining
        ));
        choices.push(TempChoice {
            id: model.id.clone(),
            capability: *capability,
            status: TempStatus::RequiresEviction,
            min_ram_gb: model.min_ram_gb,
            reason: format!(
                "footprint {:.2}GB > remaining {:.2}GB; evict resident before load",
                quant.footprint_gb, remaining
            ),
            quant,
        });
    }

    TempResult { choices, warnings }
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
        if model.status.as_deref() == Some("experiment")
            || host.ram_gb < model.min_ram_gb
            || resident.is_some_and(|choice| choice.id == model.id)
        {
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
