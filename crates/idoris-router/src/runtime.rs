//! Construct and index lifecycle-managed runtimes by component provider.

use std::collections::BTreeMap;

use idoris_backend::{Supervisor, SupervisorConfig, SupervisorHandle};
use idoris_contracts::{ComponentCard, component_card::Form};
use idoris_upstream::factory::create_adapter;

use crate::dispatch::BoundSupervisor;

#[derive(Debug, Clone, Default)]
pub struct RuntimeRegistry {
    supervisors: BTreeMap<String, BoundSupervisor>,
}

impl RuntimeRegistry {
    pub fn spawn(cards: &[ComponentCard]) -> Result<Self, String> {
        let mut registry = Self::default();
        for card in cards {
            let Some(handle) = spawn_runtime(card)? else {
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
        create_adapter(card).map_err(|error| format!("provider {}: {error}", card.provider.id))?;
    Supervisor::spawn(adapter, SupervisorConfig::default())
        .map(Some)
        .map_err(|error| format!("provider {}: {error}", card.provider.id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::dispatch::{DispatchFailure, dispatch_local};
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
                "hello",
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
    async fn missing_runtime_fails_closed_without_breaking_another_backend() {
        let a = named_card("a");
        let b = named_card("b");
        let registry: RuntimeRegistry = mock_bound(&a).into();
        let missing = dispatch_local(
            std::slice::from_ref(&b),
            registry.get("b"),
            None,
            &profile(),
            "",
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
            "hello",
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
}
