#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use idoris_contracts::common::Capability;
use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::{KvQuant, ModelArch};
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{PartialPolicy, recommend};

fn model(id: &str, scores: &[(&str, f64)], weights: f64, daily: bool) -> CatalogModel {
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
        roles: if daily {
            vec![CatalogRole::new(Role::Daily).unwrap()]
        } else {
            vec![]
        },
        load_hint: None,
        capability: Some(
            scores
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect::<BTreeMap<_, _>>(),
        ),
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
    needed: Vec<Capability>,
    /// Expected (capability, alternative id) pairs on blocked model `huge`.
    expected: Vec<(Capability, &'static str)>,
}

fn resident() -> CatalogModel {
    model("res", &[("reasoning", 1.0)], 1.0, true)
}

fn huge() -> CatalogModel {
    model("huge", &[("vision", 1.0), ("coding", 1.0)], 50.0, false)
}

fn vision_only() -> CatalogModel {
    model("vision-only", &[("vision", 0.8)], 1.0, false)
}

fn coding_only() -> CatalogModel {
    model("coding-only", &[("coding", 0.8)], 1.0, false)
}

#[test]
fn recommend_merges_blocked_alternatives_per_capability() {
    let cases = vec![
        Case {
            name: "one model best for two capabilities keeps both alternatives",
            models: vec![resident(), huge(), vision_only(), coding_only()],
            needed: vec![Capability::Vision, Capability::Coding],
            expected: vec![
                (Capability::Vision, "vision-only"),
                (Capability::Coding, "coding-only"),
            ],
        },
        Case {
            name: "order of needed capabilities does not drop entries",
            models: vec![resident(), huge(), vision_only(), coding_only()],
            needed: vec![Capability::Coding, Capability::Vision],
            expected: vec![
                (Capability::Coding, "coding-only"),
                (Capability::Vision, "vision-only"),
            ],
        },
        Case {
            name: "only one capability has a fitting alternative",
            models: vec![resident(), huge(), vision_only()],
            needed: vec![Capability::Vision, Capability::Coding],
            expected: vec![(Capability::Vision, "vision-only")],
        },
        Case {
            name: "duplicate needed capability is not duplicated",
            models: vec![resident(), huge(), vision_only()],
            needed: vec![Capability::Vision, Capability::Vision],
            expected: vec![(Capability::Vision, "vision-only")],
        },
        Case {
            name: "no fitting alternative leaves the list empty",
            models: vec![resident(), huge()],
            needed: vec![Capability::Vision, Capability::Coding],
            expected: vec![],
        },
    ];
    let host = make_host_facts(HostFactsInput {
        ram_gb: 64.0,
        ..Default::default()
    })
    .unwrap();
    for case in cases {
        let policy = PartialPolicy {
            context_target: Some(1),
            kv_quant: Some(KvQuant::Q8),
            temp_slots: Some(0),
            needed_capabilities: Some(case.needed),
            ..Default::default()
        };
        let catalog = Catalog {
            version: 1.0,
            catalog: case.models,
            excluded: None,
        };
        let rec = recommend(&host, &catalog, Some(&policy), None, Some(10_000_000_000)).unwrap();
        let blocked = rec.blocked.iter().find(|b| b.id == "huge").unwrap();
        let actual: Vec<(Capability, &str)> = blocked
            .degraded_alternatives
            .iter()
            .map(|alt| (alt.capability, alt.id.as_str()))
            .collect();
        assert_eq!(actual, case.expected, "{}", case.name);
        assert!(
            rec.blocked
                .iter()
                .filter(|b| b.id != "huge")
                .all(|b| b.degraded_alternatives.is_empty()),
            "{}",
            case.name
        );
    }
}
