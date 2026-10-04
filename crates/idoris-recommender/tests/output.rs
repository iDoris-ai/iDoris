#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{PartialPolicy, recommend, recommend_from_file};

fn model(id: &str, reasoning: f64, min_ram_gb: f64) -> CatalogModel {
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
        capability: Some(BTreeMap::from([
            ("reasoning".into(), reasoning),
            ("vision".into(), 1.0),
        ])),
        quant_options: vec![CatalogQuant {
            label: "q".into(),
            bpp: None,
            weights_gb: Some(1.0),
            quality: 1.0,
        }],
        license: None,
        min_ram_gb,
        status: None,
        note: None,
        scenarios: None,
    }
}

fn hardware() -> idoris_recommender::probe::HostFacts {
    make_host_facts(HostFactsInput {
        ram_gb: 24.0,
        chip: Some("M1 Max".into()),
        os: Some("darwin".into()),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn recommendation_contains_budget_resident_temp_blocked_warnings_and_tradeoff_in_order() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("resident", 2.0, 1.0), model("blocked", 9.0, 64.0)],
        excluded: None,
    };
    let result = recommend(&hardware(), &catalog, None, None).unwrap();
    assert_eq!(result.resident.as_ref().unwrap().id, "resident");
    assert_eq!(result.resident_label.as_deref(), Some("resident@q"));
    assert_eq!(result.blocked.len(), 1);
    assert!(result.warnings[0].starts_with("预算: 24GB conservative"));
    assert!(result.warnings[1].starts_with("blocked: BLOCKED"));
    assert!(result.tradeoff.starts_with("硬件 M1 Max / 24GB"));
    assert!(result.tradeoff.contains("常驻 resident@q"));
    assert!(result.tradeoff.contains("BLOCKED：blocked(需64GB)"));
    assert!(result.tradeoff.ends_with("临时预留 3.50GB）"));
    assert_eq!(result.recommended_sysctl.iogpu_wired_limit_mb, 16_220);
}

#[test]
fn explicit_override_is_reflected_without_mutating_process_environment() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("auto", 2.0, 1.0), model("forced", 1.0, 1.0)],
        excluded: None,
    };
    let result = recommend(
        &hardware(),
        &catalog,
        Some(&PartialPolicy::default()),
        Some("forced"),
    )
    .unwrap();
    assert_eq!(result.resident.as_ref().unwrap().id, "forced");
    assert_eq!(result.override_choice.as_ref().unwrap().id, "forced");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("yields"))
    );
}

#[test]
fn recommend_from_file_uses_the_same_public_path() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "idoris-recommender-output-{}-{nonce}.yaml",
        std::process::id()
    ));
    fs::write(
        &path,
        r#"version: 1
catalog:
  - id: demo
    params_total_b: 1
    arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 }
    roles: [daily]
    capability: { reasoning: 1 }
    quant_options:
      - { label: q, weights_gb: 1, quality: 1 }
    min_ram_gb: 1
"#,
    )
    .unwrap();
    let result = recommend_from_file(&path, &hardware(), None, None).unwrap();
    fs::remove_file(&path).unwrap();
    assert_eq!(result.resident.as_ref().unwrap().id, "demo");
}

#[test]
fn fixed_output_precision_is_visible_at_two_and_four_decimals() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![model("resident", 0.123456, 1.0)],
        excluded: None,
    };
    let result = recommend(&hardware(), &catalog, None, None).unwrap();
    assert!(result.tradeoff.contains("能力×质量=0.1235"));
    assert!(result.tradeoff.contains("usable 15.84GB"));
}
