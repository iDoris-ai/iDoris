#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use idoris_contracts::common::Capability;
use idoris_policy::Role;
use idoris_recommender::catalog::{Catalog, CatalogModel, CatalogQuant, CatalogRole, load_catalog};
use idoris_recommender::memory::ModelArch;
use idoris_recommender::probe::{HostFactsInput, make_host_facts};
use idoris_recommender::recommend::{PartialPolicy, Recommendation, TempStatus, recommend};

#[derive(Debug, PartialEq)]
struct CapacityRow {
    id: String,
    capability: Capability,
    resident: bool,
    estimated_memory_gb: f64,
    ctx_limit: u64,
    admission_status: &'static str,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn hardware(ram_gb: f64) -> idoris_recommender::probe::HostFacts {
    make_host_facts(HostFactsInput {
        ram_gb,
        chip: Some("M4".into()),
        gpu_cores: Some(10),
        os: Some("darwin".into()),
    })
    .unwrap()
}

fn top_capability(model: Option<&CatalogModel>) -> Capability {
    let priority = [
        Capability::Reasoning,
        Capability::Coding,
        Capability::Vision,
        Capability::Asr,
        Capability::Tts,
        Capability::Embedding,
        Capability::Rerank,
        Capability::Chat,
    ];
    let Some(scores) = model.and_then(|model| model.capability.as_ref()) else {
        return Capability::Chat;
    };
    priority
        .into_iter()
        .filter_map(|capability| {
            scores
                .get(capability_name(capability))
                .copied()
                .filter(|score| score.is_finite())
                .map(|score| (capability, score))
        })
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or(Capability::Chat, |(capability, _)| capability)
}

fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Chat => "chat",
        Capability::Reasoning => "reasoning",
        Capability::Vision => "vision",
        Capability::Asr => "asr",
        Capability::Tts => "tts",
        Capability::Coding => "coding",
        Capability::Embedding => "embedding",
        Capability::Rerank => "rerank",
    }
}

fn consumer_rows(rec: &Recommendation, catalog: &Catalog) -> Vec<CapacityRow> {
    let mut rows = Vec::new();
    if let Some(resident) = &rec.resident {
        rows.push(CapacityRow {
            id: resident.id.clone(),
            capability: Capability::Reasoning,
            resident: true,
            estimated_memory_gb: resident.footprint_gb,
            ctx_limit: resident.ctx,
            admission_status: "ready",
        });
    }
    rows.extend(rec.temp.iter().map(|temp| CapacityRow {
        id: temp.id.clone(),
        capability: temp.capability,
        resident: false,
        estimated_memory_gb: temp.quant.footprint_gb,
        ctx_limit: rec.policy.context_target,
        admission_status: match temp.status {
            TempStatus::Ready => "ready",
            TempStatus::RequiresEviction => "requires_eviction",
        },
    }));
    rows.extend(rec.blocked.iter().map(|blocked| CapacityRow {
        id: blocked.id.clone(),
        capability: top_capability(catalog.catalog.iter().find(|model| model.id == blocked.id)),
        resident: false,
        estimated_memory_gb: blocked.estimated_memory_gb,
        ctx_limit: rec.policy.context_target,
        admission_status: "blocked",
    }));
    rows
}

#[test]
fn public_recommendation_maps_to_b1_capacity_fields_at_24_and_32gb() {
    let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
    let at_24 = recommend(&hardware(24.0), &catalog, None, None).unwrap();
    let rows_24 = consumer_rows(&at_24, &catalog);
    assert!(
        rows_24
            .iter()
            .any(|row| row.id == "ornith-1.0-9b" && row.resident)
    );
    assert!(
        rows_24
            .iter()
            .any(|row| row.admission_status == "requires_eviction")
    );
    assert!(rows_24.iter().any(|row| row.admission_status == "blocked"));

    let resident = at_24.resident.as_ref().unwrap();
    let resident_row = rows_24
        .iter()
        .find(|row| row.id == resident.id && row.resident)
        .unwrap();
    assert_eq!(resident_row.ctx_limit, resident.ctx);
    assert_eq!(resident_row.estimated_memory_gb, resident.footprint_gb);
    assert_ne!(resident_row.estimated_memory_gb, resident.weights_gb);

    let at_32 = recommend(&hardware(32.0), &catalog, None, None).unwrap();
    let rows_32 = consumer_rows(&at_32, &catalog);
    assert_eq!(at_32.resident_label.as_deref(), Some("ornith-1.0-9b@q8_0"));
    assert!(
        rows_32.iter().any(|row| {
            row.id == "agents-a1-35b" && row.admission_status == "requires_eviction"
        })
    );
    assert!(
        rows_32
            .iter()
            .any(|row| { row.id == "qwen3.6-35b-a3b" && row.admission_status == "blocked" })
    );
}

#[test]
fn forced_resident_can_remain_blocked_without_consumer_deduplication() {
    let catalog = Catalog {
        version: 1.0,
        catalog: vec![CatalogModel {
            id: "forced-big".into(),
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
            capability: None,
            quant_options: vec![CatalogQuant {
                label: "q".into(),
                bpp: None,
                weights_gb: Some(50.0),
                quality: 0.98,
            }],
            license: None,
            min_ram_gb: 64.0,
            status: None,
            note: None,
            scenarios: None,
        }],
        excluded: None,
    };
    let policy = PartialPolicy {
        temp_slots: Some(0),
        needed_capabilities: Some(Vec::new()),
        ..Default::default()
    };
    let rec = recommend(&hardware(24.0), &catalog, Some(&policy), Some("forced-big")).unwrap();
    let rows = consumer_rows(&rec, &catalog);
    assert_eq!(rows.iter().filter(|row| row.id == "forced-big").count(), 2);
    assert!(
        rows.iter()
            .any(|row| row.id == "forced-big" && row.resident)
    );
    assert!(
        rows.iter()
            .any(|row| { row.id == "forced-big" && row.admission_status == "blocked" })
    );
}
