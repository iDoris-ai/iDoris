//! Construct and index lifecycle-managed runtimes by component provider.

use std::collections::BTreeMap;

use idoris_backend::{
    BackendError, RuntimeAdapter, Supervisor, SupervisorConfig, SupervisorHandle,
};
use idoris_contracts::{ComponentCard, component_card::Form};
use idoris_policy::is_subscription_provider_id;
use idoris_upstream::factory::create_adapter;

use crate::dispatch::BoundSupervisor;

#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistry {
    supervisors: BTreeMap<String, BoundSupervisor>,
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
        let mut registry = Self::default();
        for card in cards {
            let Some(handle) = spawn_runtime_with_factory(card, &factory)? else {
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

    pub fn get(&self, provider_id: &str) -> Option<&BoundSupervisor> {
        self.supervisors.get(provider_id)
    }

    pub fn len(&self) -> usize {
        self.supervisors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.supervisors.is_empty()
    }

    pub async fn queue_depth(&self) -> u64 {
        let mut depth = 0_u64;
        for (provider_id, supervisor) in &self.supervisors {
            depth = add_status_depth(depth, provider_id, supervisor.status().await);
        }
        depth
    }
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
        Self { supervisors }
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
    spawn_runtime_with_factory(card, &create_adapter)
}

fn spawn_runtime_with_factory<F>(
    card: &ComponentCard,
    factory: &F,
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
    Supervisor::spawn(adapter, SupervisorConfig::default())
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
