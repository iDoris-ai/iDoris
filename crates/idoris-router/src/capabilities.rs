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
use idoris_recommender::recommend::{PartialPolicy, Recommendation, TempStatus, recommend};
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
    runtimes: RuntimeRegistry,
}

impl LiveCapabilitiesProvider {
    pub fn new(catalog: Catalog, facts: HostFacts, runtimes: RuntimeRegistry) -> Self {
        Self {
            catalog: Arc::new(catalog),
            facts,
            policy: None,
            runtimes,
        }
    }

    pub fn with_policy(mut self, policy: PartialPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    async fn live_snapshot(&self) -> Result<Vec<CapabilityEntry>, CapabilitiesError> {
        let recommendation = recommend(&self.facts, &self.catalog, self.policy.as_ref(), None)
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
    if let Some(resident) = &recommendation.resident {
        entries.push(CapabilityEntry {
            id: resident.id.clone(),
            capability: "reasoning".into(),
            resident: true,
            estimated_memory_gb: round_gb(resident.footprint_gb),
            ctx_limit: resident.ctx,
            queue_depth,
            admission_status: AdmissionStatus::Ready,
        });
    }
    entries.extend(recommendation.temp.iter().map(|temp| CapabilityEntry {
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
