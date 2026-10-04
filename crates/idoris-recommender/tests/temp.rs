#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_contracts::common::Capability;
use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{
    RecommenderPolicy, ResidentChoice, TempStatus, temp_admission,
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

#[test]
fn highest_capability_model_is_kept_even_when_weaker_model_would_fit() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![
            model("strong-big", "vision", 1.0, 20.0),
            model("weak-small", "vision", 0.8, 1.0),
        ],
        excluded: None,
    };
    let result = temp_admission(&catalog, &host(), &policy(vec![Capability::Vision]), None);
    assert_eq!(result.choices.len(), 1);
    assert_eq!(result.choices[0].id, "strong-big");
    assert_eq!(result.choices[0].status, TempStatus::RequiresEviction);
    assert!(result.choices[0].reason.contains("需驱逐常驻后加载"));
    assert!(result.warnings[0].contains("需驱逐常驻"));
}

#[test]
fn each_capability_reuses_same_remaining_budget_without_cumulative_deduction() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![
            model("vision", "vision", 1.0, 5.0),
            model("coding", "coding", 1.0, 5.0),
        ],
        excluded: None,
    };
    let result = temp_admission(
        &catalog,
        &host(),
        &policy(vec![Capability::Vision, Capability::Coding]),
        None,
    );
    assert_eq!(result.choices.len(), 2);
    assert!(
        result
            .choices
            .iter()
            .all(|choice| choice.status == TempStatus::Ready)
    );
}

#[test]
fn temp_slots_does_not_truncate_and_duplicate_capabilities_remain_visible() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("multi", "vision", 1.0, 1.0)],
        excluded: None,
    };
    let mut policy = policy(vec![Capability::Vision, Capability::Vision]);
    policy.temp_slots = 0;
    let result = temp_admission(&catalog, &host(), &policy, None);
    assert_eq!(result.choices.len(), 2);
    assert_eq!(result.choices[0].id, "multi");
    assert_eq!(result.choices[1].id, "multi");
}

#[test]
fn resident_experiment_low_ram_and_missing_capability_are_excluded() {
    let mut experiment = model("experiment", "vision", 9.0, 1.0);
    experiment.status = Some("experiment".into());
    let mut too_big = model("too-big", "vision", 8.0, 1.0);
    too_big.min_ram_gb = 64.0;
    let resident_model = model("resident", "vision", 7.0, 1.0);
    let ordinary = model("ordinary", "vision", 6.0, 1.0);
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![experiment, too_big, resident_model, ordinary],
        excluded: None,
    };
    let resident = ResidentChoice {
        id: "resident".into(),
        ctx: 32_768,
        score: 1.0,
        label: "q".into(),
        quality: 1.0,
        weights_gb: 1.0,
        kv_gb: 0.0,
        footprint_gb: 2.0,
    };
    let result = temp_admission(
        &catalog,
        &host(),
        &policy(vec![Capability::Vision, Capability::Coding]),
        Some(&resident),
    );
    assert_eq!(result.choices.len(), 1);
    assert_eq!(result.choices[0].id, "ordinary");
}
