#![allow(clippy::unwrap_used)]

use idoris_policy::Role;
use idoris_recommender::catalog::{CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::{KvQuant, ModelArch};
use idoris_recommender::recommend::{
    PartialPolicy, RecommenderPolicy, lowest_footprint_pick, pick_quant, quant_by_label,
};

fn model(quants: Vec<CatalogQuant>) -> CatalogModel {
    CatalogModel {
        id: "m".into(),
        family: None,
        params_total_b: 1.0,
        params_active_b: None,
        arch: ModelArch {
            n_layers: 1.0,
            n_kv_heads: 1.0,
            head_dim: 1.0,
        },
        modality: None,
        roles: vec![CatalogRole::new(Role::Daily).unwrap()],
        load_hint: None,
        capability: None,
        quant_options: quants,
        license: None,
        min_ram_gb: 1.0,
        status: None,
        note: None,
        scenarios: None,
    }
}

fn q(label: &str, weights_gb: f64, quality: f64) -> CatalogQuant {
    CatalogQuant {
        label: label.into(),
        bpp: None,
        weights_gb: Some(weights_gb),
        quality,
    }
}

#[test]
fn policy_defaults_and_partial_merge_match_ts() {
    let default = RecommenderPolicy::default();
    assert_eq!(default.context_target, 32_768);
    assert_eq!(default.kv_quant, KvQuant::Q8);
    assert_eq!(default.temp_slots, 1);
    assert_eq!(default.quality_threshold, 0.98);
    let merged = RecommenderPolicy::merged(Some(&PartialPolicy {
        context_target: Some(4096),
        quality_threshold: Some(0.95),
        ..Default::default()
    }));
    assert_eq!(merged.context_target, 4096);
    assert_eq!(merged.quality_threshold, 0.95);
    assert_eq!(merged.temp_slots, 1);
}

#[test]
fn pick_prefers_quality_then_smaller_footprint_and_keeps_first_exact_tie() {
    let model = model(vec![
        q("small-low", 1.0, 0.98),
        q("large-high", 2.0, 0.99),
        q("small-high", 1.5, 0.99),
        q("tie-high", 1.5, 0.99),
    ]);
    let policy = RecommenderPolicy::default();
    let pick = pick_quant(&model, 3.0, &policy, 0.98).unwrap();
    assert_eq!(pick.label, "small-high");
}

#[test]
fn threshold_and_budget_are_inclusive_and_low_quality_small_quant_is_skipped() {
    let model = model(vec![q("tiny-low", 0.1, 0.97), q("threshold", 1.0, 0.98)]);
    let policy = RecommenderPolicy::default();
    let exact = quant_by_label(&model, "threshold", &policy)
        .unwrap()
        .footprint_gb;
    assert_eq!(
        pick_quant(&model, exact, &policy, 0.98).unwrap().label,
        "threshold"
    );
    assert!(pick_quant(&model, exact - 1e-9, &policy, 0.98).is_none());
}

#[test]
fn lowest_ignores_quality_and_label_lookup_is_exact() {
    let model = model(vec![q("quality", 2.0, 1.0), q("small", 0.5, 0.1)]);
    let policy = RecommenderPolicy::default();
    assert_eq!(
        lowest_footprint_pick(&model, &policy).unwrap().label,
        "small"
    );
    assert_eq!(
        quant_by_label(&model, "quality", &policy).unwrap().label,
        "quality"
    );
    assert!(quant_by_label(&model, "QUALITY", &policy).is_none());
}
