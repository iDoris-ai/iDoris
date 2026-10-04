#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::{KvQuant, ModelArch};
use idoris_recommender::model::{ModelApiError, candidates_for_role, estimate_model};

fn model(id: &str, weights: [(&str, f64); 2], role: Role) -> CatalogModel {
    CatalogModel {
        id: id.into(),
        family: Some("local".into()),
        params_total_b: 1.0,
        params_active_b: None,
        arch: ModelArch {
            n_layers: 8.0,
            n_kv_heads: 4.0,
            head_dim: 64.0,
        },
        modality: None,
        roles: vec![CatalogRole::new(role).unwrap()],
        load_hint: None,
        capability: Some(BTreeMap::from([
            ("reasoning".into(), 0.8),
            ("coding".into(), 0.7),
        ])),
        quant_options: weights
            .into_iter()
            .map(|(label, weights_gb)| CatalogQuant {
                label: label.into(),
                bpp: None,
                weights_gb: Some(weights_gb),
                quality: 1.0,
            })
            .collect(),
        license: None,
        min_ram_gb: 8.0,
        status: None,
        note: None,
        scenarios: None,
    }
}

fn catalog() -> Catalog {
    Catalog {
        version: 1.0,
        catalog: vec![
            model("catalog-a", [("q4", 4.0), ("q8", 8.0)], Role::Daily),
            model("catalog-b", [("q4", 2.0), ("q8", 4.0)], Role::Fast),
        ],
        excluded: None,
    }
}

#[test]
fn estimates_vary_by_model_quant_and_context_instead_of_using_a_fixed_placeholder() {
    let catalog = catalog();
    let a_q4 = estimate_model(&catalog, "catalog-a", "q4", 4_096, KvQuant::Q8).unwrap();
    let a_q8 = estimate_model(&catalog, "catalog-a", "q8", 4_096, KvQuant::Q8).unwrap();
    let b_q4 = estimate_model(&catalog, "catalog-b", "q4", 4_096, KvQuant::Q8).unwrap();
    let long_ctx = estimate_model(&catalog, "catalog-a", "q4", 32_768, KvQuant::Q8).unwrap();
    assert_ne!(a_q4.footprint_gb, a_q8.footprint_gb);
    assert_ne!(a_q4.footprint_gb, b_q4.footprint_gb);
    assert!(long_ctx.kv_gb > a_q4.kv_gb);
    assert!(long_ctx.footprint_gb > a_q4.footprint_gb);
    assert_eq!(a_q4.catalog_id, "catalog-a");
    assert_eq!(a_q4.quant, "q4");
    assert_eq!(a_q4.roles, vec!["daily"]);
    assert_eq!(a_q4.capabilities["reasoning"], 0.8);
}

#[test]
fn unknown_catalog_id_quant_and_zero_context_are_distinct_fail_closed_errors() {
    let catalog = catalog();
    assert_eq!(
        estimate_model(&catalog, "provider-omlx", "q4", 4_096, KvQuant::Q8).unwrap_err(),
        ModelApiError::UnknownModel("provider-omlx".into())
    );
    assert_eq!(
        estimate_model(&catalog, "catalog-a", "missing", 4_096, KvQuant::Q8).unwrap_err(),
        ModelApiError::UnknownQuant {
            model: "catalog-a".into(),
            quant: "missing".into(),
        }
    );
    assert_eq!(
        estimate_model(&catalog, "catalog-a", "q4", 0, KvQuant::Q8).unwrap_err(),
        ModelApiError::InvalidContext
    );
}

#[test]
fn role_candidates_reuse_catalog_order_and_distinguish_no_candidate() {
    let catalog = catalog();
    assert_eq!(
        candidates_for_role(&catalog, Role::Daily, Some(16.0)).unwrap(),
        vec!["catalog-a"]
    );
    assert!(matches!(
        candidates_for_role(&catalog, Role::Vision, Some(16.0)),
        Err(ModelApiError::NoCandidates(role)) if role == "vision"
    ));
    assert_eq!(
        candidates_for_role(&catalog, Role::Auto, None).unwrap_err(),
        ModelApiError::InvalidRole
    );
}
