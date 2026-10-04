use crate::catalog::{CatalogModel, CatalogQuant};
use crate::memory::{DEFAULT_OVERHEAD_GB, kv_cache_gb, weights_gb};

use super::types::{QuantPick, RecommenderPolicy};

pub fn pick_quant(
    model: &CatalogModel,
    budget_gb: f64,
    policy: &RecommenderPolicy,
    min_quality: f64,
) -> Option<QuantPick> {
    let mut best: Option<QuantPick> = None;
    for quant in &model.quant_options {
        if quant.quality < min_quality {
            continue;
        }
        let pick = to_pick(quant, model, policy)?;
        if pick.footprint_gb > budget_gb {
            continue;
        }
        let replace = best.as_ref().is_none_or(|current| {
            pick.quality > current.quality
                || (pick.quality == current.quality && pick.footprint_gb < current.footprint_gb)
        });
        if replace {
            best = Some(pick);
        }
    }
    best
}

pub fn lowest_footprint_pick(
    model: &CatalogModel,
    policy: &RecommenderPolicy,
) -> Option<QuantPick> {
    let mut best: Option<QuantPick> = None;
    for quant in &model.quant_options {
        let pick = to_pick(quant, model, policy)?;
        if best
            .as_ref()
            .is_none_or(|current| pick.footprint_gb < current.footprint_gb)
        {
            best = Some(pick);
        }
    }
    best
}

pub fn quant_by_label(
    model: &CatalogModel,
    label: &str,
    policy: &RecommenderPolicy,
) -> Option<QuantPick> {
    model
        .quant_options
        .iter()
        .find(|quant| quant.label == label)
        .and_then(|quant| to_pick(quant, model, policy))
}

fn to_pick(
    quant: &CatalogQuant,
    model: &CatalogModel,
    policy: &RecommenderPolicy,
) -> Option<QuantPick> {
    let weights_gb = weights_gb(model.params_total_b, &quant.as_quant_spec()).ok()?;
    let kv_gb = kv_cache_gb(model.arch, policy.context_target, policy.kv_quant);
    Some(QuantPick {
        label: quant.label.clone(),
        quality: quant.quality,
        weights_gb,
        kv_gb,
        footprint_gb: weights_gb + kv_gb + DEFAULT_OVERHEAD_GB,
    })
}
