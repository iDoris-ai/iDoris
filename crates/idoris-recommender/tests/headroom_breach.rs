#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::{KvQuant, ModelArch};
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{PartialPolicy, recommend};

fn resident_model(id: &str, reasoning: f64, weights: f64) -> CatalogModel {
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
        capability: Some(BTreeMap::from([("reasoning".into(), reasoning)])),
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

struct Case {
    name: &'static str,
    models: Vec<CatalogModel>,
    forced: Option<&'static str>,
    expected_option: Option<&'static str>,
}

// Footprint = weights + 1GB overhead (KV negligible at ctx=1). With
// temp_slots=0 and a 10GB allocation the resident budget is 9GB and the
// headroom-relaxed budget is 10GB (the allocated total).
const TOTAL: Option<u64> = Some(10_000_000_000);

#[test]
fn headroom_breach_option_table() {
    let cases = vec![
        Case {
            name: "better model fits only by spending headroom",
            models: vec![
                resident_model("ok", 1.0, 2.0),
                resident_model("good", 2.0, 8.5),
            ],
            forced: None,
            expected_option: Some("good"),
        },
        Case {
            name: "better model exceeds allocated total even with headroom",
            models: vec![
                resident_model("ok", 1.0, 2.0),
                resident_model("over", 2.0, 9.2),
            ],
            forced: None,
            expected_option: None,
        },
        Case {
            name: "best already fits without headroom",
            models: vec![
                resident_model("ok", 1.0, 2.0),
                resident_model("good", 2.0, 5.0),
            ],
            forced: None,
            expected_option: None,
        },
        Case {
            name: "forced model suppresses option",
            models: vec![
                resident_model("ok", 1.0, 2.0),
                resident_model("good", 2.0, 8.5),
            ],
            forced: Some("ok"),
            expected_option: None,
        },
        Case {
            name: "no automatic resident but one fits with headroom",
            models: vec![resident_model("only", 2.0, 8.5)],
            forced: None,
            expected_option: Some("only"),
        },
    ];
    let host = make_host_facts(HostFactsInput {
        ram_gb: 64.0,
        ..Default::default()
    })
    .unwrap();
    let policy = PartialPolicy {
        context_target: Some(1),
        kv_quant: Some(KvQuant::Q8),
        temp_slots: Some(0),
        needed_capabilities: Some(vec![]),
        ..Default::default()
    };
    for case in cases {
        let catalog = Catalog {
            version: 1.0,
            catalog: case.models,
            excluded: None,
        };
        let rec = recommend(&host, &catalog, Some(&policy), case.forced, TOTAL).unwrap();
        let actual = rec.headroom_breach_option.as_ref().map(|c| c.id.as_str());
        assert_eq!(actual, case.expected_option, "{}", case.name);
        if let Some(option) = &rec.headroom_breach_option {
            assert!(option.footprint_gb <= rec.usable_gb, "{}", case.name);
            assert_ne!(
                rec.resident.as_ref().map(|r| &r.id),
                Some(&option.id),
                "{}: option must not replace resident",
                case.name
            );
        }
    }
}
