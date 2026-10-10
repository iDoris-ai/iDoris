//! Construct and index lifecycle-managed runtimes by component provider.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use idoris_backend::{
    BackendError, BackendStatus, GlobalCapacityLedger, GlobalCapacitySnapshot, RuntimeAdapter,
    Supervisor, SupervisorConfig, SupervisorHandle,
};
use idoris_contracts::{ComponentCard, component_card::Form};
use idoris_policy::is_subscription_provider_id;
use idoris_upstream::factory::create_adapter;
use idoris_upstream::{LocalHttpRuntimeAdapter, LocalHttpRuntimeConfig};

use crate::dispatch::BoundSupervisor;

#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistry {
    supervisors: BTreeMap<String, BoundSupervisor>,
    startup_capacity: Option<Arc<GlobalCapacityLedger>>,
    #[cfg(test)]
    ready_snapshot_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl RuntimeRegistry {
    pub fn spawn(cards: &[ComponentCard]) -> Result<Self, String> {
        Self::spawn_with_factory(cards, create_adapter)
    }

    /// Construct lifecycle runtimes using an explicitly supplied adapter
    /// factory. The production default remains [`create_adapter`]; this seam
    /// lets trusted startup configuration provide process-owned local runtimes
    /// without smuggling executable/model paths into portable component cards.
    pub fn spawn_with_factory<F>(cards: &[ComponentCard], factory: F) -> Result<Self, String>
    where
        F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
    {
        let budget_gb = SupervisorConfig::default().budget_gb;
        let startup_capacity = Arc::new(
            GlobalCapacityLedger::new(budget_gb)
                .map_err(|error| format!("startup capacity ledger: {error}"))?,
        );
        let mut registry = Self {
            supervisors: BTreeMap::new(),
            startup_capacity: Some(startup_capacity.clone()),
            #[cfg(test)]
            ready_snapshot_calls: Arc::default(),
        };
        for card in cards {
            let Some(handle) = spawn_runtime_with_factory(card, &factory, Some(&startup_capacity))?
            else {
                continue;
            };
            let provider_id = card.provider.id.clone();
            if registry
                .supervisors
                .insert(provider_id.clone(), BoundSupervisor::new(card, handle))
                .is_some()
            {
                return Err(format!(
                    "duplicate lifecycle runtime for provider {provider_id}"
                ));
            }
        }
        Ok(registry)
    }

    pub fn spawn_with_local_configs(
        cards: &[ComponentCard],
        local: &BTreeMap<String, LocalHttpRuntimeConfig>,
    ) -> Result<Self, String> {
        for (provider_id, config) in local {
            let card = cards
                .iter()
                .find(|card| card.provider.id == *provider_id)
                .ok_or_else(|| {
                    format!("本地 runtime {provider_id:?} 没有对应的组件卡，拒绝启动")
                })?;
            if crate::dispatch::is_resident_http_service(card) {
                return Err(format!(
                    "本地 runtime {provider_id:?} 不能绑定 Resident 直连组件卡"
                ));
            }
            if card.form != Form::HttpService
                || card.provider.locality != idoris_contracts::provider::Locality::Loopback
                || card.endpoint != config.launch.endpoint()
            {
                return Err(format!(
                    "本地 runtime {provider_id:?} 与组件卡的 form/locality/endpoint 不一致"
                ));
            }
        }

        let mut registry = Self::spawn_with_factory(cards, |card| {
            if let Some(config) = local.get(&card.provider.id) {
                return LocalHttpRuntimeAdapter::new(config.clone()).map(|adapter| {
                    std::sync::Arc::new(adapter) as std::sync::Arc<dyn RuntimeAdapter>
                });
            }
            create_adapter(card)
        })?;
        for (provider_id, config) in local {
            if let Some(bound) = registry.supervisors.get_mut(provider_id) {
                *bound = bound
                    .clone()
                    .with_admission_model(config.model_id.clone(), config.memory_gb);
            }
        }
        Ok(registry)
    }

    pub fn get(&self, provider_id: &str) -> Option<&BoundSupervisor> {
        self.supervisors.get(provider_id)
    }

    pub(crate) async fn status_observations(
        &self,
    ) -> Vec<(String, Result<BackendStatus, BackendError>)> {
        let mut observations = Vec::with_capacity(self.supervisors.len());
        for (provider_id, supervisor) in &self.supervisors {
            observations.push((provider_id.clone(), supervisor.status().await));
        }
        observations
    }

    #[cfg(test)]
    pub(crate) fn from_supervisors(supervisors: impl IntoIterator<Item = BoundSupervisor>) -> Self {
        Self {
            supervisors: supervisors
                .into_iter()
                .map(|bound| (bound.provider_id().to_string(), bound))
                .collect(),
            startup_capacity: None,
            ready_snapshot_calls: Arc::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.supervisors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.supervisors.is_empty()
    }

    /// Wait for every lifecycle runtime's startup reconciliation, then return
    /// the shared residency snapshot. This deliberately does not claim
    /// load/unload admission authority yet.
    pub async fn startup_capacity_snapshot(
        &self,
    ) -> Result<Option<GlobalCapacitySnapshot>, BackendError> {
        let Some(ledger) = &self.startup_capacity else {
            return Ok(None);
        };
        for supervisor in self.supervisors.values() {
            supervisor.status().await?;
        }
        ledger.snapshot().map(Some)
    }

    pub async fn queue_depth(&self) -> u64 {
        let mut depth = 0_u64;
        for (provider_id, supervisor) in &self.supervisors {
            depth = add_status_depth(depth, provider_id, supervisor.status().await);
        }
        depth
    }

    /// Return providers whose exact requested lifecycle target is currently
    /// chat-ready. Errors are conservatively treated as not ready.
    pub async fn exact_ready_providers(&self, targets: Vec<(String, String)>) -> BTreeSet<String> {
        #[cfg(test)]
        self.ready_snapshot_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut ready = BTreeSet::new();
        for (provider_id, model_id) in targets {
            if let Some(supervisor) = self.supervisors.get(&provider_id)
                && ready_for_affinity(supervisor.is_ready(&model_id).await)
            {
                ready.insert(provider_id);
            }
        }
        ready
    }

    #[cfg(test)]
    pub(crate) fn ready_snapshot_calls(&self) -> usize {
        self.ready_snapshot_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn ready_for_affinity(result: Result<bool, BackendError>) -> bool {
    result.unwrap_or(false)
}

fn add_status_depth(
    depth: u64,
    provider_id: &str,
    status: Result<idoris_backend::BackendStatus, idoris_backend::BackendError>,
) -> u64 {
    match status {
        Ok(status) => depth.saturating_add(status.loaded.len() as u64),
        Err(error) => {
            eprintln!(
                "[idoris] capabilities: provider {provider_id} status failed ({}), excluded from queue depth",
                error.reason_code()
            );
            depth
        }
    }
}

impl From<BoundSupervisor> for RuntimeRegistry {
    fn from(bound: BoundSupervisor) -> Self {
        let mut supervisors = BTreeMap::new();
        supervisors.insert(bound.provider_id().to_string(), bound);
        Self {
            supervisors,
            startup_capacity: None,
            #[cfg(test)]
            ready_snapshot_calls: Arc::default(),
        }
    }
}

impl From<Option<BoundSupervisor>> for RuntimeRegistry {
    fn from(bound: Option<BoundSupervisor>) -> Self {
        bound.map_or_else(Self::default, Self::from)
    }
}

/// Return `None` for resident HTTP services, which use the direct proxy path.
///
/// # Panics
/// Lifecycle construction requires a running Tokio runtime, like `Supervisor::spawn`.
pub fn spawn_runtime(card: &ComponentCard) -> Result<Option<SupervisorHandle>, String> {
    spawn_runtime_with_factory(card, &create_adapter, None)
}

fn spawn_runtime_with_factory<F>(
    card: &ComponentCard,
    factory: &F,
    startup_capacity: Option<&Arc<GlobalCapacityLedger>>,
) -> Result<Option<SupervisorHandle>, String>
where
    F: Fn(&ComponentCard) -> Result<std::sync::Arc<dyn RuntimeAdapter>, BackendError>,
{
    if is_subscription_provider_id(&card.provider.id) {
        return Ok(None);
    }
    if crate::dispatch::is_resident_http_service(card) {
        return Ok(None);
    }
    if card.form != Form::HttpService {
        return Err(format!(
            "provider {} with form {:?} has no runtime constructor",
            card.provider.id, card.form
        ));
    }
    let adapter =
        factory(card).map_err(|error| format!("provider {}: {error}", card.provider.id))?;
    let config = SupervisorConfig::default();
    let supervisor = if let Some(ledger) = startup_capacity {
        Supervisor::spawn_with_startup_capacity_tracking(
            adapter,
            config,
            ledger.clone(),
            card.provider.id.clone(),
        )
    } else {
        Supervisor::spawn(adapter, config)
    };
    supervisor
        .map(Some)
        .map_err(|error| format!("provider {}: {error}", card.provider.id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::dispatch::{DispatchFailure, DispatchInput, dispatch_local};
    use crate::profile::{IntentSource, ParsedProfile};
    use idoris_backend::{ChatMessage, MockAdapter, ModelInfo};
    use idoris_contracts::TaskProfile;
    use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn card() -> ComponentCard {
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml"))
            .expect("fixture is a valid card")
    }

    fn named_card(id: &str) -> ComponentCard {
        let mut card = card();
        card.provider.id = id.to_string();
        card
    }

    fn mock_bound(card: &ComponentCard) -> BoundSupervisor {
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: card.provider.id.clone(),
            memory_gb: 1.0,
        }]));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        BoundSupervisor::new(card, handle)
    }

    fn profile() -> ParsedProfile {
        ParsedProfile {
            task: TaskProfile::default(),
            role: None,
            tenant_id: None,
            intent_source: IntentSource::Default,
        }
    }

    #[tokio::test]
    async fn on_demand_and_omitted_policy_construct_handles() {
        let mut card = card();
        assert!(spawn_runtime(&card).expect("on-demand runtime").is_some());
        card.load_policy = None;
        assert!(
            spawn_runtime(&card)
                .expect("default lifecycle runtime")
                .is_some()
        );
    }

    #[tokio::test]
    async fn unknown_lifecycle_provider_is_rejected() {
        let mut card = card();
        card.provider.id = "unknown".into();
        let error = spawn_runtime(&card).expect_err("unknown provider rejected");
        assert!(error.contains("provider unknown"));
    }

    #[test]
    fn resident_http_service_uses_proxy_path() {
        let mut card = card();
        card.provider.id = "generic".into();
        card.load_policy = Some(LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        });
        assert!(
            spawn_runtime(&card)
                .expect("resident HTTP bypass")
                .is_none()
        );
    }

    #[test]
    fn unsupported_non_http_form_is_rejected() {
        let mut card = card();
        card.form = Form::SpawnCli;
        assert!(
            spawn_runtime(&card)
                .unwrap_err()
                .contains("no runtime constructor")
        );
    }

    #[tokio::test]
    async fn registry_keeps_mock_supervisors_bound_to_their_provider() {
        let a = named_card("a");
        let b = named_card("b");
        let registry = RuntimeRegistry {
            supervisors: BTreeMap::from([
                ("a".into(), mock_bound(&a)),
                ("b".into(), mock_bound(&b)),
            ]),
            startup_capacity: None,
            ready_snapshot_calls: Arc::default(),
        };
        for card in [&a, &b] {
            let outcome = dispatch_local(
                std::slice::from_ref(card),
                registry.get(&card.provider.id),
                None,
                &profile(),
                DispatchInput::new("hello"),
                vec![ChatMessage {
                    role: "user".into(),
                    content: "hello".into(),
                }],
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let response = outcome.result.unwrap();
            assert_eq!(response.model, card.provider.id);
        }
    }

    #[tokio::test]
    async fn exact_ready_snapshot_requires_the_requested_model_to_be_ready() {
        let card = named_card("runtime-b");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "target".into(),
            memory_gb: 1.0,
        }]));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        handle
            .load("target", 1.0, card.load_policy.unwrap())
            .await
            .unwrap();
        let registry = RuntimeRegistry::from(BoundSupervisor::new(&card, handle));

        let ready = registry
            .exact_ready_providers(vec![
                ("runtime-b".into(), "target".into()),
                ("runtime-b".into(), "other".into()),
            ])
            .await;
        assert_eq!(ready, ["runtime-b".to_string()].into());
    }

    #[tokio::test]
    async fn startup_inherited_residency_is_not_claimed_as_chat_ready() {
        let card = named_card("runtime-b");
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "target".into(),
            memory_gb: 1.0,
        }]));
        adapter
            .load("target", card.load_policy.as_ref())
            .await
            .unwrap();
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let registry = RuntimeRegistry::from(BoundSupervisor::new(&card, handle));

        assert!(
            registry
                .exact_ready_providers(vec![("runtime-b".into(), "target".into())])
                .await
                .is_empty()
        );
    }

    #[test]
    fn readiness_query_errors_are_neutral_for_affinity() {
        assert!(!ready_for_affinity(Err(BackendError::internal(
            "readiness unavailable"
        ))));
    }

    #[tokio::test]
    async fn resident_direct_path_never_receives_lifecycle_readiness_boost() {
        let mut resident = named_card("resident");
        resident.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
        let registry = RuntimeRegistry::spawn_with_factory(std::slice::from_ref(&resident), |_| {
            panic!("Resident direct path must not construct a lifecycle runtime")
        })
        .unwrap();

        assert!(
            registry
                .exact_ready_providers(vec![("resident".into(), "resident".into())])
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn injected_factory_constructs_non_omlx_lifecycle_runtime_without_card_extensions() {
        let mut custom = named_card("local-http");
        custom.extensions = None;
        let registry = RuntimeRegistry::spawn_with_factory(std::slice::from_ref(&custom), |card| {
            Ok(Arc::new(MockAdapter::new(vec![ModelInfo {
                id: card.provider.id.clone(),
                memory_gb: 2.0,
            }])) as Arc<dyn RuntimeAdapter>)
        })
        .unwrap();

        assert_eq!(registry.len(), 1);
        assert!(registry.get("local-http").is_some());
    }

    #[tokio::test]
    async fn registry_tracks_each_runtime_startup_in_one_shared_capacity_snapshot() {
        let a = named_card("runtime-a");
        let b = named_card("runtime-b");
        let registry = RuntimeRegistry::spawn_with_factory(&[a, b], |card| {
            Ok(Arc::new(MockAdapter::new(vec![ModelInfo {
                id: card.provider.id.clone(),
                memory_gb: 2.0,
            }])) as Arc<dyn RuntimeAdapter>)
        })
        .unwrap();

        let snapshot = registry.startup_capacity_snapshot().await.unwrap().unwrap();
        assert_eq!(snapshot.budget_gb, SupervisorConfig::default().budget_gb);
        assert_eq!(snapshot.reserved_gb, 0.0);
        assert_eq!(snapshot.allocations["runtime:runtime-a"], 0.0);
        assert_eq!(snapshot.allocations["runtime:runtime-b"], 0.0);
    }

    #[tokio::test]
    async fn injected_factory_is_not_called_for_subscription_or_resident_proxy_cards() {
        let mut resident = named_card("resident");
        resident.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
        let mut subscription = named_card(idoris_policy::SUBSCRIPTION_PROVIDER_ID);
        subscription.load_policy = None;
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let registry = RuntimeRegistry::spawn_with_factory(&[resident, subscription], |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(BackendError::internal("factory must not run"))
        })
        .unwrap();

        assert!(registry.is_empty());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_config_requires_exact_card_binding() {
        let mut custom = named_card("local-http");
        custom.endpoint = "http://127.0.0.1:18111".into();
        let launch = idoris_upstream::LocalRuntimeLaunch::new(
            idoris_upstream::LocalRuntimeKind::MlxLmServer,
            "/usr/bin/python3",
            "/models/qwen-mlx",
            18111,
        )
        .unwrap();
        let config = LocalHttpRuntimeConfig::new(
            launch,
            "local-http",
            2.0,
            "/tmp/idoris/local-http.pending".into(),
        )
        .unwrap();
        let configs = BTreeMap::from([("local-http".to_string(), config)]);
        assert!(RuntimeRegistry::spawn_with_local_configs(&[custom.clone()], &configs).is_ok());

        custom.endpoint = "http://127.0.0.1:19999".into();
        assert!(RuntimeRegistry::spawn_with_local_configs(&[custom], &configs).is_err());
        assert!(RuntimeRegistry::spawn_with_local_configs(&[], &configs).is_err());
    }

    #[tokio::test]
    async fn missing_runtime_fails_closed_without_breaking_another_backend() {
        let a = named_card("a");
        let b = named_card("b");
        let registry: RuntimeRegistry = mock_bound(&a).into();
        let missing = dispatch_local(
            std::slice::from_ref(&b),
            registry.get("b"),
            None,
            &profile(),
            DispatchInput::new(""),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            missing.result,
            Err(DispatchFailure::Backend(ref err))
                if err.reason_code() == "supervisor_unavailable"
        ));

        let healthy = dispatch_local(
            std::slice::from_ref(&a),
            registry.get("a"),
            None,
            &profile(),
            DispatchInput::new("hello"),
            vec![ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(healthy.result.unwrap().model, "a");
    }

    #[test]
    fn queue_depth_excludes_a_failed_backend_without_losing_healthy_depth() {
        let healthy = idoris_backend::BackendStatus {
            pressure: idoris_backend::Pressure::Ok,
            used_gb: 2.0,
            model_memory_max_gb: 24.0,
            loaded: vec!["a".into(), "b".into()],
        };
        let depth = add_status_depth(0, "healthy", Ok(healthy));
        let depth = add_status_depth(
            depth,
            "failed",
            Err(idoris_backend::BackendError::supervisor_unavailable()),
        );
        assert_eq!(depth, 2);
    }
}
