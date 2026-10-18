#![allow(clippy::unwrap_used)]

use idoris_contracts::common::Capability;
use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole};
use idoris_recommender::memory::{KvQuant, ModelArch, WiredMode};
use idoris_recommender::probe::{HostFacts, HostFactsInput, make_host_facts, nominal_ram_gb};
use idoris_recommender::recommend::{PartialPolicy, TempStatus, recommend, recommend_from_file};
use std::collections::BTreeMap;

// At ctx=1, KV is exactly 2 bytes; weights=1GB + overhead=1GB.
const MODEL_BYTES: u64 = 2_000_000_002;

fn facts(ram_gb: f64) -> HostFacts {
    make_host_facts(HostFactsInput {
        ram_gb,
        ..Default::default()
    })
    .unwrap()
}

fn model(id: &str, capability: &str, daily: bool) -> CatalogModel {
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
        capability: Some(BTreeMap::from([(capability.into(), 1.0)])),
        quant_options: vec![CatalogQuant {
            label: "q".into(),
            bpp: None,
            weights_gb: Some(1.0),
            quality: 1.0,
        }],
        license: None,
        min_ram_gb: 16.0,
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

fn policy() -> PartialPolicy {
    PartialPolicy {
        context_target: Some(1),
        kv_quant: Some(KvQuant::Q8),
        temp_slots: Some(0),
        needed_capabilities: Some(vec![Capability::Vision]),
        ..Default::default()
    }
}

#[test]
fn allocated_temp_total_and_remaining_boundaries() {
    let catalog = catalog(vec![
        model("resident", "reasoning", true),
        model("temp", "vision", false),
    ]);
    for (budget, resident, status, blocked) in [
        (None, true, Some(TempStatus::Ready), false),
        (Some(0), false, None, true),
        (Some(MODEL_BYTES - 1), false, None, true),
        (Some(MODEL_BYTES), false, Some(TempStatus::Ready), false),
        (
            Some(MODEL_BYTES + 1_000_000_000),
            true,
            Some(TempStatus::RequiresEviction),
            false,
        ),
        (
            Some(MODEL_BYTES * 2 - 1),
            true,
            Some(TempStatus::RequiresEviction),
            false,
        ),
        (Some(MODEL_BYTES * 2), true, Some(TempStatus::Ready), false),
    ] {
        let rec = recommend(&facts(64.0), &catalog, Some(&policy()), None, budget).unwrap();
        assert_eq!(rec.resident.is_some(), resident, "{budget:?}");
        assert_eq!(rec.temp.first().map(|t| t.status), status, "{budget:?}");
        assert_eq!(
            rec.blocked.iter().any(|b| b.id == "temp"),
            blocked,
            "{budget:?}"
        );
        if let Some(bytes) = budget {
            assert_eq!(rec.usable_gb, bytes as f64 / 1e9);
            assert_eq!(rec.reserve_gb, 0.0);
            assert!(
                rec.resident
                    .as_ref()
                    .is_none_or(|r| r.footprint_gb <= rec.usable_gb)
            );
        }
    }
}

#[test]
fn allocated_resident_boundary_and_real_host_gate() {
    let catalog = catalog(vec![
        model("daily", "reasoning", true),
        model("temp", "vision", false),
    ]);
    for (ram, budget, present, blocked) in [
        (64.0, None, true, false),
        (64.0, Some(0), false, true),
        (64.0, Some(MODEL_BYTES + 1_000_000_000 - 1), false, false),
        (64.0, Some(MODEL_BYTES + 1_000_000_000), true, false),
        (16.0, Some(MODEL_BYTES + 1_000_000_000), true, false),
        (15.0, Some(100_000_000_000), false, true),
    ] {
        let hardware = facts(ram);
        let rec = recommend(&hardware, &catalog, Some(&policy()), None, budget).unwrap();
        assert_eq!(rec.hardware, hardware);
        assert_eq!(
            rec.resident.is_some(),
            present,
            "ram={ram} budget={budget:?}"
        );
        assert_eq!(!rec.blocked.is_empty(), blocked);
        if ram < 16.0 {
            assert!(rec.temp.is_empty());
            assert_eq!(rec.blocked.len(), 2);
            assert!(
                rec.blocked
                    .iter()
                    .all(|b| b.reason.contains("min_ram_gb=16 > ram_gb=15"))
            );
        }
    }
}

#[test]
fn forced_override_cannot_bypass_allocated_total_or_real_min_ram() {
    let catalog = catalog(vec![model("forced", "reasoning", true)]);
    for (ram, budget, forced, present, active) in [
        (64.0, None, "forced", true, true),
        (64.0, Some(0), "forced", false, false),
        (64.0, Some(MODEL_BYTES - 1), "forced", false, false),
        (64.0, Some(MODEL_BYTES), "forced", true, true),
        (64.0, Some(MODEL_BYTES - 1), "forced@q", false, false),
        (64.0, Some(MODEL_BYTES), "forced@q", true, true),
        (15.0, Some(100_000_000_000), "forced", false, false),
        (15.0, None, "forced", true, true),
    ] {
        let rec = recommend(&facts(ram), &catalog, Some(&policy()), Some(forced), budget).unwrap();
        assert_eq!(rec.resident.is_some(), present, "{ram} {budget:?} {forced}");
        assert_eq!(rec.override_choice.as_ref().map(|o| o.active), Some(active));
        if !active {
            assert!(rec.warnings.iter().any(|w| w.contains("BLOCKED")));
            assert!(!rec.warnings.iter().any(|w| w.contains("OOM")));
        }
    }
}

#[test]
fn allocated_budget_replaces_host_reserve_and_wired_fraction_once() {
    let catalog = catalog(vec![model("daily", "reasoning", true)]);
    for mode in [
        WiredMode::Conservative,
        WiredMode::Moderate,
        WiredMode::Aggressive,
    ] {
        let policy = PartialPolicy {
            wired_mode: Some(mode),
            temp_slots: Some(2),
            ..policy()
        };
        let rec = recommend(
            &facts(64.0),
            &catalog,
            Some(&policy),
            None,
            Some(20_000_000_000),
        )
        .unwrap();
        assert_eq!(rec.usable_gb, 20.0);
        assert_eq!(rec.reserve_gb, 0.0);
        assert_eq!(rec.temp_reserve_gb, 7.0);
        assert_eq!(rec.resident_budget_gb, 12.0);
        let legacy = recommend(&facts(64.0), &catalog, Some(&policy), None, None).unwrap();
        assert_eq!(rec.recommended_sysctl, legacy.recommended_sysctl);
    }
}

#[test]
fn file_api_accepts_allocation() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/catalog.yaml");
    for budget in [None, Some(0), Some(12_000_000_000)] {
        let loaded = idoris_recommender::catalog::load_catalog(&path).unwrap();
        let rec = recommend_from_file(&path, &facts(64.0), None, None, budget).unwrap();
        assert_eq!(
            rec,
            recommend(&facts(64.0), &loaded, None, None, budget).unwrap()
        );
    }
}

#[test]
fn sampled_ram_preserves_legacy_rounding_before_min_ram_gating() {
    for (bytes, expected) in [
        (16_u64 << 30, 16.0),
        ((16_u64 << 30) - 1, 16.0),
        ((15_u64 << 30) + (3_u64 << 28), 16.0),
    ] {
        assert_eq!(nominal_ram_gb(bytes as f64).unwrap(), expected);
    }
}

#[test]
fn allocated_low_quality_fallback_never_evicts_when_it_already_fits_remaining() {
    let mut temp = model("temp", "vision", false);
    temp.quant_options[0].quality = 0.5;
    let catalog = catalog(vec![model("resident", "reasoning", true), temp]);
    for (budget, expected) in [
        (None, TempStatus::RequiresEviction),
        (Some(MODEL_BYTES), TempStatus::Ready),
        (
            Some(MODEL_BYTES + 1_000_000_000),
            TempStatus::RequiresEviction,
        ),
        (Some(MODEL_BYTES * 2), TempStatus::Ready),
    ] {
        let rec = recommend(&facts(64.0), &catalog, Some(&policy()), None, budget).unwrap();
        assert_eq!(rec.temp[0].status, expected);
    }
}

#[test]
fn rejected_explicit_quant_preserves_fitting_automatic_resident() {
    let mut forced = model("daily", "reasoning", true);
    forced.quant_options.push(CatalogQuant {
        label: "huge".into(),
        bpp: None,
        weights_gb: Some(50.0),
        quality: 1.0,
    });
    let catalog = catalog(vec![forced]);
    for name in ["daily@huge", "daily@missing", "missing"] {
        let rec = recommend(
            &facts(64.0),
            &catalog,
            Some(&policy()),
            Some(name),
            Some(5_000_000_000),
        )
        .unwrap();
        assert_eq!(rec.resident.as_ref().unwrap().label, "q");
        assert_eq!(
            rec.override_choice.as_ref().map(|o| o.active),
            match name {
                "daily@huge" => Some(false),
                "daily@missing" => Some(true),
                _ => None,
            }
        );
    }
}

#[test]
fn allocated_blocked_best_temp_does_not_select_a_weaker_model() {
    let mut huge = model("huge", "vision", false);
    huge.quant_options[0].weights_gb = Some(50.0);
    let mut weak = model("weak", "vision", false);
    weak.capability
        .as_mut()
        .unwrap()
        .insert("vision".into(), 0.8);
    let catalog = catalog(vec![huge, weak]);
    for budget in [None, Some(3_000_000_000)] {
        let rec = recommend(&facts(64.0), &catalog, Some(&policy()), None, budget).unwrap();
        if budget.is_none() {
            assert_eq!(rec.temp[0].id, "huge");
            assert_eq!(rec.temp[0].status, TempStatus::RequiresEviction);
        } else {
            assert!(rec.temp.is_empty());
            assert_eq!(rec.blocked.len(), 1);
            assert_eq!(rec.blocked[0].id, "huge");
            assert!(rec.blocked[0].reason.contains("allocated total"));
            assert!(
                rec.warnings
                    .iter()
                    .any(|w| w.contains("外部分配 3000000000 bytes"))
            );
        }
    }
}
