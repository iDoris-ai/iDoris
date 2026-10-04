#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{
    RecommenderPolicy, apply_core_override, blocked_choices, choose_resident,
};

fn host(ram_gb: f64) -> idoris_recommender::probe::HostFacts {
    make_host_facts(HostFactsInput {
        ram_gb,
        ..Default::default()
    })
    .unwrap()
}

fn model(id: &str, min_ram_gb: f64, experiment: bool) -> CatalogModel {
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
        capability: Some(BTreeMap::from([("reasoning".into(), 1.0)])),
        quant_options: vec![
            CatalogQuant {
                label: "small".into(),
                bpp: None,
                weights_gb: Some(1.0),
                quality: 0.98,
            },
            CatalogQuant {
                label: "huge".into(),
                bpp: None,
                weights_gb: Some(50.0),
                quality: 1.0,
            },
        ],
        license: None,
        min_ram_gb,
        status: experiment.then(|| "experiment".into()),
        note: None,
        scenarios: None,
    }
}

#[test]
fn absent_or_blank_override_is_a_noop() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("auto", 1.0, false)],
        excluded: None,
    };
    let policy = RecommenderPolicy::default();
    let current = choose_resident(&catalog, &host(24.0), &policy);
    for forced in [None, Some(""), Some("   ")] {
        let result = apply_core_override(&catalog, &host(24.0), &policy, forced, current.clone());
        assert_eq!(result.resident, current);
        assert!(result.override_choice.is_none());
        assert!(result.warnings.is_empty());
    }
}

#[test]
fn trims_whole_input_uses_only_first_two_at_segments_and_exact_quant_can_exceed_budget() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("forced", 1.0, false)],
        excluded: None,
    };
    let policy = RecommenderPolicy::default();
    let result = apply_core_override(
        &catalog,
        &host(24.0),
        &policy,
        Some(" forced@huge@ignored "),
        None,
    );
    assert_eq!(result.resident.as_ref().unwrap().id, "forced");
    assert_eq!(result.resident.as_ref().unwrap().label, "huge");
    assert_eq!(result.override_choice.as_ref().unwrap().id, "forced");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("footprint"))
    );
}

#[test]
fn unknown_model_preserves_auto_and_unknown_quant_falls_back() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("auto", 1.0, false)],
        excluded: None,
    };
    let policy = RecommenderPolicy::default();
    let current = choose_resident(&catalog, &host(24.0), &policy);
    let missing = apply_core_override(
        &catalog,
        &host(24.0),
        &policy,
        Some("missing"),
        current.clone(),
    );
    assert_eq!(missing.resident, current);
    assert!(missing.override_choice.is_none());
    assert!(missing.warnings[0].contains("不在目录中"));

    let quant = apply_core_override(
        &catalog,
        &host(24.0),
        &policy,
        Some("auto@missing"),
        current,
    );
    assert_eq!(quant.resident.as_ref().unwrap().label, "small");
    assert!(
        quant
            .warnings
            .iter()
            .any(|warning| warning.contains("量化") && warning.contains("不存在"))
    );
}

#[test]
fn oversized_experiment_is_forced_but_blocked_list_remains_intact_and_warnings_are_loud() {
    let mut experimental = model("forced-big", 64.0, true);
    experimental.quant_options.remove(0);
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![experimental],
        excluded: None,
    };
    let policy = RecommenderPolicy::default();
    let before = blocked_choices(&catalog, &host(24.0), &policy).unwrap();
    assert_eq!(before.len(), 1);
    assert!(choose_resident(&catalog, &host(24.0), &policy).is_none());

    let result = apply_core_override(&catalog, &host(24.0), &policy, Some("forced-big"), None);
    assert_eq!(result.resident.as_ref().unwrap().id, "forced-big");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("OOM"))
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("绕过 min_ram_gb"))
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("yields"))
    );
    assert_eq!(
        blocked_choices(&catalog, &host(24.0), &policy).unwrap(),
        before
    );
}

#[test]
fn unknown_quant_then_no_fitting_auto_quant_uses_lowest_and_warns_about_oom() {
    let mut oversized = model("forced-big", 1.0, false);
    oversized.quant_options.remove(0);
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![oversized],
        excluded: None,
    };
    let result = apply_core_override(
        &catalog,
        &host(24.0),
        &RecommenderPolicy::default(),
        Some("forced-big@missing"),
        None,
    );
    assert_eq!(result.resident.as_ref().unwrap().label, "huge");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("量化") && warning.contains("不存在"))
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("OOM"))
    );
}
