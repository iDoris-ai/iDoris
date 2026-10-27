#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_contracts::common::Capability;
use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{
    RecommenderPolicy, ResidentChoice, TempResult, TempStatus, budget_breakdown, temp_admission,
};

fn host() -> idoris_recommender::probe::HostFacts {
    make_host_facts(HostFactsInput {
        ram_gb: 24.0,
        ..Default::default()
    })
    .unwrap()
}

fn model(id: &str, capability: &str, score: f64, weights: f64) -> CatalogModel {
    CatalogModel {
        id: id.into(),
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
        capability: Some(BTreeMap::from([(capability.into(), score)])),
        quant_options: vec![CatalogQuant {
            label: "q".into(),
            bpp: None,
            weights_gb: Some(weights),
            quality: 1.0,
        }],
        license: None,
        min_ram_gb: 1.0,
        status: None,
        note: None,
        scenarios: None,
    }
}

fn policy(capabilities: Vec<Capability>) -> RecommenderPolicy {
    RecommenderPolicy {
        needed_capabilities: capabilities,
        ..Default::default()
    }
}

fn resident(footprint_gb: f64) -> ResidentChoice {
    ResidentChoice {
        id: "resident".into(),
        ctx: 32_768,
        score: 1.0,
        label: "q".into(),
        quality: 1.0,
        weights_gb: footprint_gb,
        kv_gb: 0.0,
        footprint_gb,
    }
}

fn run(
    models: Vec<CatalogModel>,
    resident: Option<&ResidentChoice>,
    allocated: Option<u64>,
) -> TempResult {
    let catalog = Catalog {
        version: 1.0,
        catalog: models,
        excluded: None,
    };
    let policy = policy(vec![Capability::Vision]);
    temp_admission(
        &catalog,
        &host(),
        &policy,
        resident,
        &budget_breakdown(&host(), &policy, allocated),
    )
}

/// Allocated total is 10 GB, resident takes 8 GB, so 2 GB remain.
/// Footprint is weights + 1 GB overhead (tiny KV).
const TOTAL: Option<u64> = Some(10_000_000_000);

#[test]
fn requires_eviction_offers_smaller_fitting_alternative() {
    let r = resident(8.0);
    let result = run(
        vec![
            model("big", "vision", 1.0, 5.0),
            model("mid", "vision", 0.9, 4.0),
            model("small", "vision", 0.8, 0.5),
            model("tiny", "vision", 0.5, 0.2),
        ],
        Some(&r),
        TOTAL,
    );
    let choice = &result.choices[0];
    assert_eq!(choice.id, "big");
    assert_eq!(choice.status, TempStatus::RequiresEviction);
    let alt = choice.degraded_alternative.as_ref().unwrap();
    assert_eq!(
        alt.id, "small",
        "best-scoring model that fits, not just any"
    );
    assert!(alt.quant.footprint_gb <= 2.0);
    assert!(result.blocked_alternatives.is_empty());
}

#[test]
fn requires_eviction_without_fitting_alternative_is_none() {
    let r = resident(8.0);
    let result = run(
        vec![
            model("big", "vision", 1.0, 5.0),
            model("mid", "vision", 0.9, 4.0),
        ],
        Some(&r),
        TOTAL,
    );
    assert_eq!(result.choices[0].status, TempStatus::RequiresEviction);
    assert_eq!(result.choices[0].degraded_alternative, None);
}

#[test]
fn ready_has_no_alternative() {
    let result = run(
        vec![
            model("best", "vision", 1.0, 1.0),
            model("small", "vision", 0.5, 0.5),
        ],
        None,
        TOTAL,
    );
    assert_eq!(result.choices[0].status, TempStatus::Ready);
    assert_eq!(result.choices[0].degraded_alternative, None);
}

#[test]
fn allocated_blocked_surfaces_alternative_without_temp_choice() {
    let result = run(
        vec![
            model("huge", "vision", 1.0, 50.0),
            model("small", "vision", 0.8, 1.0),
        ],
        None,
        TOTAL,
    );
    assert!(result.choices.is_empty());
    assert_eq!(result.blocked_alternatives.len(), 1);
    assert_eq!(result.blocked_alternatives[0].0, "huge");
    assert_eq!(result.blocked_alternatives[0].1.id, "small");
}

#[test]
fn allocated_blocked_without_fitting_alternative_is_none() {
    let result = run(vec![model("huge", "vision", 1.0, 50.0)], None, TOTAL);
    assert!(result.choices.is_empty());
    assert!(result.blocked_alternatives.is_empty());
}

#[test]
fn alternative_excludes_resident_experiment_and_low_ram_models() {
    let r = resident(8.0);
    let mut experiment = model("experiment", "vision", 0.9, 0.5);
    experiment.status = Some("experiment".into());
    let mut low_ram = model("low-ram", "vision", 0.8, 0.5);
    low_ram.min_ram_gb = 64.0;
    let resident_model = model("resident", "vision", 0.7, 0.5);
    let result = run(
        vec![
            model("big", "vision", 1.0, 5.0),
            experiment,
            low_ram,
            resident_model,
        ],
        Some(&r),
        TOTAL,
    );
    assert_eq!(result.choices[0].id, "big");
    assert_eq!(result.choices[0].degraded_alternative, None);
}

#[test]
fn alternative_must_match_capability() {
    let r = resident(8.0);
    let result = run(
        vec![
            model("big", "vision", 1.0, 5.0),
            model("other-cap", "coding", 1.0, 0.5),
        ],
        Some(&r),
        TOTAL,
    );
    assert_eq!(result.choices[0].degraded_alternative, None);
}

#[test]
fn none_mode_eviction_keeps_legacy_choice_and_gets_alternative() {
    let result = run(
        vec![
            model("strong-big", "vision", 1.0, 20.0),
            model("weak-small", "vision", 0.8, 1.0),
        ],
        None,
        None,
    );
    assert_eq!(result.choices.len(), 1);
    assert_eq!(result.choices[0].id, "strong-big");
    assert_eq!(result.choices[0].status, TempStatus::RequiresEviction);
    assert_eq!(
        result.choices[0].degraded_alternative.as_ref().unwrap().id,
        "weak-small"
    );
    assert!(result.blocked_alternatives.is_empty());
}
