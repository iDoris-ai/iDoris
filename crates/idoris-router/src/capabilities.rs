//! `GET /capabilities` transport boundary (B1 task32).
//!
//! This task intentionally stops at the injectable provider surface. The live
//! recommender/backend-status aggregation belongs to task33, so production must
//! never fabricate a static capacity snapshot when no provider is configured.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use idoris_recommender::catalog::Catalog;
use idoris_recommender::probe::HostFacts;
use idoris_recommender::recommend::{
    PartialPolicy, Recommendation, TempAlternative, TempStatus, recommend,
};
use serde::Serialize;

use crate::runtime::RuntimeRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Ready,
    RequiresEviction,
    Blocked,
}

/// Current TS `/capabilities` entry shape. The newer Agent24 role-oriented
/// contract is a separate interface evolution; task32 mirrors the reference
/// implementation that B1 is porting.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapabilityEntry {
    pub id: String,
    pub capability: String,
    pub resident: bool,
    pub estimated_memory_gb: f64,
    pub ctx_limit: u64,
    pub queue_depth: u64,
    pub admission_status: AdmissionStatus,
    /// Only on `requires_eviction` entries: `null` when no no-eviction
    /// alternative exists. Absent on all other entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_alternative: Option<Option<AlternativeEntry>>,
    /// Only on `blocked` entries (may be `[]`). Absent on all other entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_alternatives: Option<Vec<AlternativeEntry>>,
    /// Only on the resident entry, and only when the recommender offers it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headroom_breach_option: Option<HeadroomBreachOption>,
}

/// Stable wire code for why a fallback is offered. The recommender's
/// human-readable reason text (with live numbers) never goes on the wire.
pub const REASON_FITS_WITHOUT_EVICTION: &str = "fits_without_eviction";

/// Wire form of a no-eviction fallback model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlternativeEntry {
    pub id: String,
    /// The capability this fallback covers. A blocked model can need several
    /// capabilities, each with its own fallback.
    pub capability: &'static str,
    pub estimated_memory_gb: f64,
    pub quant: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeadroomBreachOption {
    pub id: String,
    /// Quant label; the option can be the current resident model at a
    /// higher-precision quant, so `id` alone does not identify it.
    pub quant: String,
    pub estimated_memory_gb: f64,
    pub ctx_limit: u64,
}

fn alternative_entry(alt: &TempAlternative) -> AlternativeEntry {
    AlternativeEntry {
        id: alt.id.clone(),
        capability: capability_name(alt.capability),
        estimated_memory_gb: round_gb(alt.quant.footprint_gb),
        quant: alt.quant.label.clone(),
        reason: REASON_FITS_WITHOUT_EVICTION,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitiesError {
    message: String,
}

impl CapabilitiesError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CapabilitiesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CapabilitiesError {}

pub trait CapabilitiesProvider: Send + Sync {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CapabilityEntry>, CapabilitiesError>> + Send + '_>>;
}

#[derive(Clone)]
pub struct LiveCapabilitiesProvider {
    catalog: Arc<Catalog>,
    facts: HostFacts,
    policy: Option<PartialPolicy>,
    allocated_budget_bytes: Option<u64>,
    runtimes: RuntimeRegistry,
}

impl LiveCapabilitiesProvider {
    pub fn new(catalog: Catalog, facts: HostFacts, runtimes: RuntimeRegistry) -> Self {
        Self {
            catalog: Arc::new(catalog),
            facts,
            policy: None,
            allocated_budget_bytes: None,
            runtimes,
        }
    }

    pub fn with_policy(mut self, policy: PartialPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set the total externally allocated memory in integer bytes. `None` keeps
    /// legacy host-derived recommendations; this never changes physical facts.
    pub fn with_allocated_budget_bytes(mut self, allocated_budget_bytes: Option<u64>) -> Self {
        self.allocated_budget_bytes = allocated_budget_bytes;
        self
    }

    async fn live_snapshot(&self) -> Result<Vec<CapabilityEntry>, CapabilitiesError> {
        let recommendation = recommend(
            &self.facts,
            &self.catalog,
            self.policy.as_ref(),
            None,
            self.allocated_budget_bytes,
        )
        .map_err(|_| CapabilitiesError::new("recommender snapshot failed"))?;
        let queue_depth = self.runtimes.queue_depth().await;
        Ok(map_recommendation(
            &recommendation,
            &self.catalog,
            queue_depth,
        ))
    }
}

impl CapabilitiesProvider for LiveCapabilitiesProvider {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CapabilityEntry>, CapabilitiesError>> + Send + '_>>
    {
        Box::pin(self.live_snapshot())
    }
}

fn map_recommendation(
    recommendation: &Recommendation,
    catalog: &Catalog,
    queue_depth: u64,
) -> Vec<CapabilityEntry> {
    let mut entries = Vec::new();
    let headroom_option = recommendation
        .headroom_breach_option
        .as_ref()
        .map(|option| HeadroomBreachOption {
            id: option.id.clone(),
            quant: option.label.clone(),
            estimated_memory_gb: round_gb(option.footprint_gb),
            ctx_limit: option.ctx,
        });
    if let Some(resident) = &recommendation.resident {
        entries.push(CapabilityEntry {
            id: resident.id.clone(),
            capability: "reasoning".into(),
            resident: true,
            estimated_memory_gb: round_gb(resident.footprint_gb),
            ctx_limit: resident.ctx,
            queue_depth,
            admission_status: AdmissionStatus::Ready,
            degraded_alternative: None,
            degraded_alternatives: None,
            headroom_breach_option: headroom_option,
        });
    } else if let Some(option) = headroom_option {
        // No resident fits without the headroom: surface the option as a
        // blocked resident entry so the caller can still offer it.
        entries.push(CapabilityEntry {
            id: option.id.clone(),
            capability: "reasoning".into(),
            resident: true,
            estimated_memory_gb: option.estimated_memory_gb,
            ctx_limit: option.ctx_limit,
            queue_depth,
            admission_status: AdmissionStatus::Blocked,
            degraded_alternative: None,
            degraded_alternatives: Some(Vec::new()),
            headroom_breach_option: Some(option),
        });
    }
    entries.extend(recommendation.temp.iter().map(|temp| {
        CapabilityEntry {
            id: temp.id.clone(),
            capability: capability_name(temp.capability).to_string(),
            resident: false,
            estimated_memory_gb: round_gb(temp.quant.footprint_gb),
            ctx_limit: recommendation.policy.context_target,
            queue_depth,
            admission_status: match temp.status {
                TempStatus::Ready => AdmissionStatus::Ready,
                TempStatus::RequiresEviction => AdmissionStatus::RequiresEviction,
            },
            degraded_alternative: (temp.status == TempStatus::RequiresEviction)
                .then(|| temp.degraded_alternative.as_ref().map(alternative_entry)),
            degraded_alternatives: None,
            headroom_breach_option: None,
        }
    }));
    entries.extend(recommendation.blocked.iter().map(|blocked| {
        let model = catalog.catalog.iter().find(|model| model.id == blocked.id);
        CapabilityEntry {
            id: blocked.id.clone(),
            capability: top_capability(model).to_string(),
            resident: false,
            estimated_memory_gb: round_gb(blocked.estimated_memory_gb),
            ctx_limit: recommendation.policy.context_target,
            queue_depth,
            admission_status: AdmissionStatus::Blocked,
            degraded_alternative: None,
            degraded_alternatives: Some(
                blocked
                    .degraded_alternatives
                    .iter()
                    .map(alternative_entry)
                    .collect(),
            ),
            headroom_breach_option: None,
        }
    }));
    entries
}

fn top_capability(model: Option<&idoris_recommender::catalog::CatalogModel>) -> &'static str {
    const PRIORITY: [&str; 8] = [
        "reasoning",
        "coding",
        "vision",
        "asr",
        "tts",
        "embedding",
        "rerank",
        "chat",
    ];
    let Some(scores) = model.and_then(|model| model.capability.as_ref()) else {
        return "chat";
    };
    let mut best = None;
    let mut best_score = f64::NEG_INFINITY;
    for name in PRIORITY {
        if let Some(score) = scores.get(name).copied()
            && score.is_finite()
            && score > best_score
        {
            best = Some(name);
            best_score = score;
        }
    }
    best.unwrap_or("chat")
}

fn capability_name(capability: idoris_contracts::common::Capability) -> &'static str {
    use idoris_contracts::common::Capability;
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

fn round_gb(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;
    use std::sync::Arc;

    use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
    use idoris_contracts::ComponentCard;
    use idoris_policy::Role;
    use idoris_recommender::catalog::{
        Catalog, CatalogModel, CatalogQuant, CatalogRole, load_catalog,
    };
    use idoris_recommender::memory::ModelArch;
    use idoris_recommender::probe::{HostFactsInput, make_host_facts};
    use idoris_recommender::recommend::PartialPolicy;

    use super::*;
    use crate::dispatch::BoundSupervisor;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn facts(ram_gb: f64) -> HostFacts {
        make_host_facts(HostFactsInput {
            ram_gb,
            chip: Some("M4".into()),
            gpu_cores: Some(10),
            os: Some("darwin".into()),
        })
        .unwrap()
    }

    fn card() -> ComponentCard {
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap()
    }

    #[tokio::test]
    async fn real_catalog_24gb_maps_reference_resident_temp_blocked_and_rounds_memory() {
        let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
        let provider =
            LiveCapabilitiesProvider::new(catalog, facts(24.0), RuntimeRegistry::default());
        let entries = provider.snapshot().await.unwrap();

        let resident = entries.iter().find(|entry| entry.resident).unwrap();
        assert_eq!(resident.id, "ornith-1.0-9b");
        assert_eq!(resident.capability, "reasoning");
        assert_eq!(resident.estimated_memory_gb, 10.51);
        assert_eq!(resident.ctx_limit, 32_768);
        assert_eq!(resident.queue_depth, 0);
        assert_eq!(resident.admission_status, AdmissionStatus::Ready);

        let vision = entries
            .iter()
            .find(|entry| entry.id == "qwen2.5-vl-7b")
            .unwrap();
        assert_eq!(vision.capability, "vision");
        assert_eq!(vision.estimated_memory_gb, 10.23);
        assert_eq!(vision.admission_status, AdmissionStatus::RequiresEviction);

        let blocked = entries
            .iter()
            .find(|entry| entry.id == "agents-a1-35b")
            .unwrap();
        assert_eq!(blocked.capability, "coding");
        assert_eq!(blocked.estimated_memory_gb, 17.34);
        assert_eq!(blocked.admission_status, AdmissionStatus::Blocked);
    }

    #[tokio::test]
    async fn runtime_loaded_count_changes_queue_without_changing_model_estimates() {
        let card = card();
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "warm".into(),
            memory_gb: 1.0,
        }]));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let registry = RuntimeRegistry::from(BoundSupervisor::new(&card, handle.clone()));
        let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
        let provider = LiveCapabilitiesProvider::new(catalog, facts(24.0), registry);

        let before = provider.snapshot().await.unwrap();
        handle
            .load("warm", 1.0, card.load_policy.unwrap())
            .await
            .unwrap();
        let after = provider.snapshot().await.unwrap();

        assert!(before.iter().all(|entry| entry.queue_depth == 0));
        assert!(after.iter().all(|entry| entry.queue_depth == 1));
        assert_eq!(before.len(), after.len());
        for (before, after) in before.iter().zip(&after) {
            assert_eq!(before.id, after.id);
            assert_eq!(before.estimated_memory_gb, after.estimated_memory_gb);
            assert_eq!(before.ctx_limit, after.ctx_limit);
            assert_eq!(before.admission_status, after.admission_status);
        }
    }

    #[tokio::test]
    async fn context_policy_changes_kv_footprint_and_exposed_ctx_limit() {
        let model = CatalogModel {
            id: "only".into(),
            family: None,
            params_total_b: 1.0,
            params_active_b: None,
            arch: ModelArch {
                n_layers: 32.0,
                n_kv_heads: 8.0,
                head_dim: 128.0,
            },
            modality: None,
            roles: vec![CatalogRole::new(Role::Daily).unwrap()],
            load_hint: None,
            capability: Some(std::collections::BTreeMap::from([(
                "reasoning".into(),
                1.0,
            )])),
            quant_options: vec![CatalogQuant {
                label: "q".into(),
                bpp: None,
                weights_gb: Some(1.0),
                quality: 1.0,
            }],
            license: None,
            min_ram_gb: 1.0,
            status: None,
            note: None,
            scenarios: None,
        };
        let catalog = Catalog {
            version: 1.0,
            catalog: vec![model],
            excluded: None,
        };
        let default =
            LiveCapabilitiesProvider::new(catalog.clone(), facts(24.0), RuntimeRegistry::default())
                .snapshot()
                .await
                .unwrap();
        let short = LiveCapabilitiesProvider::new(catalog, facts(24.0), RuntimeRegistry::default())
            .with_policy(PartialPolicy {
                context_target: Some(4096),
                ..Default::default()
            })
            .snapshot()
            .await
            .unwrap();

        assert_eq!(default[0].ctx_limit, 32_768);
        assert_eq!(short[0].ctx_limit, 4096);
        assert!(short[0].estimated_memory_gb < default[0].estimated_memory_gb);
    }

    #[tokio::test]
    async fn allocated_provider_maps_ready_eviction_and_total_blocked_using_real_facts() {
        let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
        let policy = PartialPolicy {
            temp_slots: Some(0),
            ..Default::default()
        };
        let baseline = recommend(&facts(64.0), &catalog, Some(&policy), None, None).unwrap();
        let vision_bytes = (baseline
            .temp
            .iter()
            .find(|t| t.capability == idoris_contracts::common::Capability::Vision)
            .unwrap()
            .quant
            .footprint_gb
            * 1e9)
            .round() as u64;
        for (budget, expected) in [
            (None, AdmissionStatus::Ready),
            (Some(0), AdmissionStatus::Blocked),
            (Some(vision_bytes - 1), AdmissionStatus::Blocked),
            (Some(vision_bytes), AdmissionStatus::RequiresEviction),
            (Some(40_000_000_000), AdmissionStatus::Ready),
        ] {
            let provider = LiveCapabilitiesProvider::new(
                catalog.clone(),
                facts(64.0),
                RuntimeRegistry::default(),
            )
            .with_policy(policy.clone())
            .with_allocated_budget_bytes(budget);
            let entries = provider.snapshot().await.unwrap();
            let vision = entries
                .iter()
                .find(|entry| entry.id == "qwen2.5-vl-7b")
                .unwrap();
            assert_eq!(vision.admission_status, expected, "{budget:?}");
            assert_eq!(vision.capability, "vision");
            assert!(!vision.resident);
            assert_eq!(vision.queue_depth, 0);
            assert_eq!(entries.iter().filter(|e| e.id == vision.id).count(), 1);
        }
    }

    async fn choice_json(gb: u64) -> serde_json::Value {
        let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
        let entries =
            LiveCapabilitiesProvider::new(catalog, facts(64.0), RuntimeRegistry::default())
                .with_policy(PartialPolicy {
                    temp_slots: Some(1),
                    ..Default::default()
                })
                .with_allocated_budget_bytes(Some(gb * 1_000_000_000))
                .snapshot()
                .await
                .unwrap();
        serde_json::to_value(entries).unwrap()
    }

    fn entry<'a>(
        json: &'a serde_json::Value,
        id: &str,
    ) -> &'a serde_json::Map<String, serde_json::Value> {
        json.as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap()
            .as_object()
            .unwrap()
    }

    const CHOICE_KEYS: [&str; 3] = [
        "degraded_alternative",
        "degraded_alternatives",
        "headroom_breach_option",
    ];

    #[tokio::test]
    async fn requires_eviction_entries_expose_degraded_alternative_or_null() {
        let json = choice_json(18).await;
        let with_alt = entry(&json, "agents-a1-35b");
        assert_eq!(with_alt["admission_status"], "requires_eviction");
        let alt = with_alt["degraded_alternative"].as_object().unwrap();
        assert_eq!(alt["id"], "nanbeige4.2-3b");
        assert_eq!(alt["estimated_memory_gb"], 4.78);
        assert_eq!(alt["quant"], "q4_k_m");
        assert_eq!(alt["capability"], "coding");
        assert_eq!(alt["reason"], "fits_without_eviction");
        assert_eq!(alt.len(), 5);
        assert!(!with_alt.contains_key("degraded_alternatives"));
        assert!(!with_alt.contains_key("headroom_breach_option"));

        let none = entry(&json, "qwen2.5-vl-7b");
        assert_eq!(none["admission_status"], "requires_eviction");
        assert!(none.contains_key("degraded_alternative"));
        assert!(none["degraded_alternative"].is_null());
    }

    #[tokio::test]
    async fn blocked_entries_expose_degraded_alternatives_array_possibly_empty() {
        let json = choice_json(16).await;
        let with_alt = entry(&json, "agents-a1-35b");
        assert_eq!(with_alt["admission_status"], "blocked");
        let alts = with_alt["degraded_alternatives"].as_array().unwrap();
        assert_eq!(alts.len(), 1);
        assert_eq!(alts[0]["id"], "nanbeige4.2-3b");
        assert_eq!(alts[0]["estimated_memory_gb"], 4.78);
        assert_eq!(alts[0]["quant"], "q4_k_m");
        assert_eq!(alts[0]["capability"], "coding");
        assert_eq!(alts[0]["reason"], "fits_without_eviction");
        assert!(!with_alt.contains_key("degraded_alternative"));
        assert!(!with_alt.contains_key("headroom_breach_option"));

        let empty = entry(&json, "qwen3.6-35b-a3b");
        assert_eq!(empty["admission_status"], "blocked");
        assert_eq!(empty["degraded_alternatives"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn resident_entry_exposes_headroom_breach_option_only_when_offered() {
        let json = choice_json(12).await;
        let resident = json
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["resident"] == true)
            .unwrap()
            .as_object()
            .unwrap();
        let option = resident["headroom_breach_option"].as_object().unwrap();
        assert_eq!(option["id"], "qwen3-8b");
        assert!(option["quant"].is_string());
        assert_eq!(option["estimated_memory_gb"], 7.92);
        assert!(option["ctx_limit"].as_u64().unwrap() > 0);
        assert_eq!(option.len(), 4);
        assert!(!resident.contains_key("degraded_alternative"));
        assert!(!resident.contains_key("degraded_alternatives"));

        // No option offered: the key is omitted, not null.
        let json = choice_json(16).await;
        let resident = json
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["resident"] == true)
            .unwrap();
        assert!(
            !resident
                .as_object()
                .unwrap()
                .contains_key("headroom_breach_option")
        );
    }

    #[test]
    fn each_alternative_carries_its_own_capability() {
        // A blocked model can get one fallback per needed capability; the
        // wire must not collapse them onto the parent entry's capability.
        let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
        let policy = PartialPolicy {
            temp_slots: Some(1),
            ..Default::default()
        };
        let mut rec = recommend(
            &facts(64.0),
            &catalog,
            Some(&policy),
            None,
            Some(16_000_000_000),
        )
        .unwrap();
        let blocked = rec
            .blocked
            .iter_mut()
            .find(|b| !b.degraded_alternatives.is_empty())
            .unwrap();
        let mut vision = blocked.degraded_alternatives[0].clone();
        vision.capability = idoris_contracts::common::Capability::Vision;
        blocked.degraded_alternatives.push(vision);
        let id = blocked.id.clone();
        let json = serde_json::to_value(map_recommendation(&rec, &catalog, 0)).unwrap();
        let caps: Vec<_> = entry(&json, &id)["degraded_alternatives"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["capability"].clone())
            .collect();
        assert_eq!(caps, ["coding", "vision"]);
    }

    #[tokio::test]
    async fn headroom_option_quant_distinguishes_same_model_upgrade() {
        // 15GB: the option is the resident model itself at a larger quant.
        let json = choice_json(15).await;
        let resident = json
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["resident"] == true)
            .unwrap();
        let option = &resident["headroom_breach_option"];
        assert_eq!(option["id"], resident["id"]);
        assert!(option["estimated_memory_gb"].as_f64() > resident["estimated_memory_gb"].as_f64());
        assert_eq!(option["quant"], "q6_k");
    }

    #[tokio::test]
    async fn headroom_option_without_resident_is_a_blocked_resident_entry() {
        // 9GB: nothing fits as resident without spending the headroom.
        let json = choice_json(9).await;
        let residents: Vec<_> = json
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["resident"] == true)
            .collect();
        assert_eq!(residents.len(), 1);
        let entry = residents[0];
        assert_eq!(entry["admission_status"], "blocked");
        assert_eq!(entry["degraded_alternatives"], serde_json::json!([]));
        let option = &entry["headroom_breach_option"];
        assert_eq!(option["id"], "nanbeige4.2-3b");
        assert_eq!(option["quant"], "q4_k_m");
        assert_eq!(entry["id"], option["id"]);
        assert_eq!(entry["estimated_memory_gb"], option["estimated_memory_gb"]);
    }

    #[tokio::test]
    async fn ready_entries_and_legacy_snapshot_never_gain_choice_keys() {
        // Ready temp entries (24GB) and the legacy unallocated path.
        let catalog = load_catalog(repo_root().join("config/catalog.yaml")).unwrap();
        let legacy = serde_json::to_value(
            LiveCapabilitiesProvider::new(catalog, facts(24.0), RuntimeRegistry::default())
                .snapshot()
                .await
                .unwrap(),
        )
        .unwrap();
        for json in [choice_json(24).await, legacy] {
            for e in json.as_array().unwrap() {
                let obj = e.as_object().unwrap();
                let status = obj["admission_status"].as_str().unwrap();
                for key in CHOICE_KEYS {
                    let allowed = match key {
                        "degraded_alternative" => status == "requires_eviction",
                        "degraded_alternatives" => status == "blocked",
                        _ => obj["resident"] == true,
                    };
                    if !allowed {
                        assert!(!obj.contains_key(key), "{} has {key}", obj["id"]);
                    }
                }
                if status == "ready" && obj["resident"] == false {
                    assert_eq!(obj.len(), 7, "ready temp keeps the 1.0.x shape");
                }
            }
        }
    }

    #[test]
    fn blocked_capability_ties_preserve_the_declared_priority() {
        let model = CatalogModel {
            id: "tie".into(),
            family: None,
            params_total_b: 1.0,
            params_active_b: None,
            arch: ModelArch {
                n_layers: 1.0,
                n_kv_heads: 1.0,
                head_dim: 1.0,
            },
            modality: None,
            roles: Vec::new(),
            load_hint: None,
            capability: Some(std::collections::BTreeMap::from([
                ("coding".into(), 0.9),
                ("reasoning".into(), 0.9),
            ])),
            quant_options: Vec::new(),
            license: None,
            min_ram_gb: 1.0,
            status: None,
            note: None,
            scenarios: None,
        };
        assert_eq!(top_capability(Some(&model)), "reasoning");
    }
}
