#![allow(clippy::unwrap_used)]

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole, LoadHint};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::roles::role_candidates;

fn model(id: &str, roles: &[Role], min_ram_gb: f64) -> CatalogModel {
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
        roles: roles
            .iter()
            .copied()
            .map(|role| CatalogRole::new(role).unwrap())
            .collect(),
        load_hint: None,
        capability: None,
        quant_options: vec![CatalogQuant {
            label: "q".into(),
            bpp: Some(1.0),
            weights_gb: None,
            quality: 1.0,
        }],
        license: None,
        min_ram_gb,
        status: None,
        note: None,
        scenarios: None,
    }
}

fn catalog(models: Vec<CatalogModel>) -> Catalog {
    Catalog {
        version: 1.0,
        catalog: models,
        excluded: None,
    }
}

#[test]
fn preserves_catalog_order_and_distinguishes_unknown_from_empty_installed_set() {
    let catalog = catalog(vec![
        model("first", &[Role::Daily], 8.0),
        model("second", &[Role::Daily], 8.0),
    ]);
    let daily = CatalogRole::new(Role::Daily).unwrap();
    assert_eq!(
        role_candidates(&catalog, daily, None, None),
        ["first", "second"]
    );
    assert!(role_candidates(&catalog, daily, Some(&[]), None).is_empty());
    assert_eq!(
        role_candidates(&catalog, daily, Some(&["second".into()]), None),
        ["second"]
    );
}

#[test]
fn ram_experiment_membership_and_nan_fail_closed() {
    let mut experimental = model("experiment", &[Role::Daily], 8.0);
    experimental.status = Some("experiment".into());
    let wrong_role = model("fast", &[Role::Fast], 8.0);
    let too_big = model("big", &[Role::Daily], 32.0);
    let nan = model("nan", &[Role::Daily], f64::NAN);
    let good = model("good", &[Role::Daily], 16.0);
    let catalog = catalog(vec![experimental, wrong_role, too_big, nan, good]);
    let daily = CatalogRole::new(Role::Daily).unwrap();
    assert_eq!(role_candidates(&catalog, daily, None, Some(16.0)), ["good"]);
    assert!(role_candidates(&catalog, daily, None, Some(f64::NAN)).is_empty());
}

#[test]
fn load_hint_does_not_change_role_membership() {
    let mut on_demand = model("ondemand", &[Role::Daily], 8.0);
    on_demand.load_hint = Some(LoadHint::OnDemand);
    let catalog = catalog(vec![on_demand]);
    assert_eq!(
        role_candidates(&catalog, CatalogRole::new(Role::Daily).unwrap(), None, None),
        ["ondemand"]
    );
}

#[test]
fn auto_cannot_enter_the_catalog_candidate_api() {
    assert!(CatalogRole::new(Role::Auto).is_err());
}
