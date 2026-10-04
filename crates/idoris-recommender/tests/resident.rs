#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{
    PartialPolicy, RecommenderPolicy, blocked_choices, budget_breakdown, choose_resident,
};

fn host(ram_gb: f64) -> idoris_recommender::probe::HostFacts {
    make_host_facts(HostFactsInput {
        ram_gb,
        ..Default::default()
    })
    .unwrap()
}

fn model(id: &str, role: Role, min_ram_gb: f64, weight: f64, reasoning: f64) -> CatalogModel {
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
        roles: vec![CatalogRole::new(role).unwrap()],
        load_hint: None,
        capability: Some(BTreeMap::from([("reasoning".into(), reasoning)])),
        quant_options: vec![CatalogQuant {
            label: "q".into(),
            bpp: None,
            weights_gb: Some(weight),
            quality: 0.98,
        }],
        license: None,
        min_ram_gb,
        status: None,
        note: None,
        scenarios: None,
    }
}

fn real_catalog() -> Catalog {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/catalog.yaml");
    let raw = std::fs::read_to_string(path).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
    idoris_recommender::catalog::parse_catalog(&value).unwrap()
}

#[test]
fn budget_components_and_24gb_real_catalog_match_ts_acceptance() {
    let policy = RecommenderPolicy::default();
    let budget = budget_breakdown(&host(24.0), &policy);
    assert_eq!(budget.usable_gb, 15.84);
    assert_eq!(budget.temp_reserve_gb, 3.5);
    assert!((budget.resident_budget_gb - 11.34).abs() < 1e-12);

    let resident = choose_resident(&real_catalog(), &host(24.0), &policy).unwrap();
    assert_eq!(resident.id, "ornith-1.0-9b");
    assert_eq!(resident.label, "q6_k");
    assert!((resident.quality - 0.995).abs() < 1e-12);
}

#[test]
fn blocked_scans_all_models_but_experiment_never_becomes_resident() {
    let mut experimental = model("experiment", Role::Daily, 16.0, 1.0, 100.0);
    experimental.status = Some("experiment".into());
    let normal = model("normal", Role::Daily, 8.0, 1.0, 1.0);
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![experimental.clone(), normal],
        excluded: None,
    };
    let policy = RecommenderPolicy::default();

    let blocked = blocked_choices(&catalog, &host(8.0), &policy).unwrap();
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].id, "experiment");
    assert_eq!(
        choose_resident(&catalog, &host(64.0), &policy).unwrap().id,
        "normal"
    );
}

#[test]
fn ram_equality_is_admitted_and_score_then_footprint_then_order_decides() {
    let first = model("first", Role::Daily, 16.0, 2.0, 1.0);
    let smaller = model("smaller", Role::Daily, 16.0, 1.0, 1.0);
    let same = model("same", Role::Daily, 16.0, 1.0, 1.0);
    let better = model("better", Role::Daily, 16.0, 3.0, 2.0);
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![first, smaller, same, better],
        excluded: None,
    };
    let policy = RecommenderPolicy::merged(Some(&PartialPolicy {
        temp_slots: Some(0),
        ..Default::default()
    }));
    let resident = choose_resident(&catalog, &host(16.0), &policy).unwrap();
    assert_eq!(resident.id, "better");

    let tied = Catalog {
        version: 1.0,
        catalog: vec![
            model("first-small", Role::Daily, 16.0, 1.0, 1.0),
            model("second-small", Role::Daily, 16.0, 1.0, 1.0),
        ],
        excluded: None,
    };
    assert_eq!(
        choose_resident(&tied, &host(16.0), &policy).unwrap().id,
        "first-small"
    );
}

#[test]
fn empty_catalog_and_negative_resident_budget_return_none() {
    let empty = Catalog {
        version: 1.0,
        catalog: Vec::new(),
        excluded: None,
    };
    assert!(choose_resident(&empty, &host(24.0), &RecommenderPolicy::default()).is_none());

    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("m", Role::Daily, 1.0, 0.1, 1.0)],
        excluded: None,
    };
    let policy = RecommenderPolicy::merged(Some(&PartialPolicy {
        temp_slots: Some(100),
        ..Default::default()
    }));
    assert!(budget_breakdown(&host(24.0), &policy).resident_budget_gb < 0.0);
    assert!(choose_resident(&catalog, &host(24.0), &policy).is_none());
}
