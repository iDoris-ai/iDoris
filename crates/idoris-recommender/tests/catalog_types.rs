#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole, LoadHint};
use idoris_recommender::memory::ModelArch;
use serde_json::json;

fn model() -> CatalogModel {
    CatalogModel {
        id: "example".into(),
        family: Some("qwen".into()),
        params_total_b: 9.0,
        params_active_b: None,
        arch: ModelArch {
            n_layers: 32.0,
            n_kv_heads: 4.0,
            head_dim: 256.0,
        },
        modality: Some(vec!["text".into()]),
        roles: vec![CatalogRole::new(Role::Daily).unwrap()],
        load_hint: Some(LoadHint::OnDemand),
        capability: Some(BTreeMap::from([
            ("coding".into(), 0.7),
            ("future-capability".into(), 0.2),
        ])),
        quant_options: vec![CatalogQuant {
            label: "q4_k_m".into(),
            bpp: Some(0.55),
            weights_gb: None,
            quality: 0.98,
        }],
        license: None,
        min_ram_gb: 16.0,
        status: None,
        note: None,
        scenarios: None,
    }
}

#[test]
fn catalog_role_reuses_policy_role_but_rejects_auto() {
    let daily = CatalogRole::new(Role::Daily).unwrap();
    assert_eq!(daily.role(), Role::Daily);
    assert_eq!(serde_json::to_value(daily).unwrap(), json!("daily"));
    assert!(CatalogRole::new(Role::Auto).is_err());
    assert!(CatalogRole::parse("auto").is_err());
}

#[test]
fn capability_keeps_unknown_keys_and_optional_fields_do_not_materialize() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model()],
        excluded: None,
    };
    let value = serde_json::to_value(&catalog).unwrap();
    assert_eq!(
        value["catalog"][0]["capability"]["future-capability"],
        json!(0.2)
    );
    assert!(value.get("excluded").is_none());
    assert!(value["catalog"][0].get("params_active_b").is_none());
    assert!(value["catalog"][0].get("license").is_none());
}

#[test]
fn roles_are_required_while_null_optionals_match_missing_and_unknown_fields_drop() {
    let base = json!({
        "id": "m",
        "params_total_b": 1.0,
        "arch": {"n_layers": 1, "n_kv_heads": 1, "head_dim": 1},
        "roles": [],
        "quant_options": [{"label":"q", "weights_gb":1.0, "quality":1.0}],
        "min_ram_gb": 1.0,
        "params_active_b": null,
        "future_field": {"ignored": true}
    });
    let parsed: CatalogModel = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(parsed.params_active_b, None);
    let serialized = serde_json::to_value(parsed).unwrap();
    assert!(serialized.get("future_field").is_none());
    assert!(serialized.get("params_active_b").is_none());

    let mut missing_roles = base;
    missing_roles.as_object_mut().unwrap().remove("roles");
    assert!(serde_json::from_value::<CatalogModel>(missing_roles).is_err());
}
