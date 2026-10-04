use std::collections::BTreeMap;
use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_upstream::subscription::profile::{
    SandboxProfile as UpstreamSandboxProfile, SubscriptionCli as UpstreamSubscriptionCli,
};
use idoris_upstream::subscription::relay::{SubscriptionRelay, SubscriptionRelayConfig};
use idoris_upstream::subscription::service::SubscriptionService;

use super::card::{CardBoundary, SubscriptionCardError, classify_subscription_card};
use super::config::{RegistrationAction, SubscriptionCli, SubscriptionConfig, decide_registration};

const DEFAULT_SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionRuntimeError {
    RegistrationRefused(&'static str),
    Card(SubscriptionCardError),
    MissingSandbox,
    WrongProvider { expected: String, actual: String },
    MissingHandle(String),
    ProfileMismatch,
}

impl std::fmt::Display for SubscriptionRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RegistrationRefused(reason) => {
                write!(f, "subscription registration refused: {reason}")
            }
            Self::Card(error) => error.fmt(f),
            Self::MissingSandbox => {
                f.write_str("authorized subscription is missing its sandbox profile")
            }
            Self::WrongProvider { expected, actual } => {
                write!(
                    f,
                    "subscription runtime provider mismatch: expected {expected}, got {actual}"
                )
            }
            Self::MissingHandle(provider) => {
                write!(
                    f,
                    "subscription runtime handle unavailable for provider {provider}"
                )
            }
            Self::ProfileMismatch => {
                f.write_str("subscription relay config does not match authorized sandbox")
            }
        }
    }
}

impl std::error::Error for SubscriptionRuntimeError {}

impl From<SubscriptionCardError> for SubscriptionRuntimeError {
    fn from(value: SubscriptionCardError) -> Self {
        Self::Card(value)
    }
}

#[derive(Debug, Clone)]
pub struct AuthorizedSubscription {
    provider_id: String,
    profile: UpstreamSandboxProfile,
}

impl AuthorizedSubscription {
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

pub fn authorize_subscription(
    config: &SubscriptionConfig,
    card: &ComponentCard,
) -> Result<Option<AuthorizedSubscription>, SubscriptionRuntimeError> {
    let decision = decide_registration(config);
    match decision.action {
        RegistrationAction::Skip => return Ok(None),
        RegistrationAction::Refuse => {
            return Err(SubscriptionRuntimeError::RegistrationRefused(
                decision.reason,
            ));
        }
        RegistrationAction::Register => {}
    }

    if classify_subscription_card(card)? != CardBoundary::Subscription {
        return Err(SubscriptionRuntimeError::Card(
            SubscriptionCardError::WrongForm,
        ));
    }
    let profile = decision
        .sandbox
        .ok_or(SubscriptionRuntimeError::MissingSandbox)?;
    let cli = match profile.cli {
        SubscriptionCli::Claude => UpstreamSubscriptionCli::Claude,
        SubscriptionCli::Codex => UpstreamSubscriptionCli::Codex,
    };
    Ok(Some(AuthorizedSubscription {
        provider_id: card.provider.id.clone(),
        profile: UpstreamSandboxProfile::fixed(cli),
    }))
}

#[derive(Clone)]
pub struct SubscriptionRuntimeHandle {
    provider_id: String,
    model_id: &'static str,
    service: SubscriptionService,
}

impl SubscriptionRuntimeHandle {
    pub fn build(
        authorized: AuthorizedSubscription,
        card: &ComponentCard,
    ) -> Result<Self, SubscriptionRuntimeError> {
        let relay_config = SubscriptionRelayConfig::from_process(authorized.profile);
        Self::build_with_relay_config(authorized, card, relay_config)
    }

    pub(crate) fn build_with_relay_config(
        authorized: AuthorizedSubscription,
        card: &ComponentCard,
        relay_config: SubscriptionRelayConfig,
    ) -> Result<Self, SubscriptionRuntimeError> {
        if classify_subscription_card(card)? != CardBoundary::Subscription {
            return Err(SubscriptionRuntimeError::Card(
                SubscriptionCardError::WrongForm,
            ));
        }
        if card.provider.id != authorized.provider_id {
            return Err(SubscriptionRuntimeError::WrongProvider {
                expected: authorized.provider_id,
                actual: card.provider.id.clone(),
            });
        }
        if relay_config.profile != authorized.profile {
            return Err(SubscriptionRuntimeError::ProfileMismatch);
        }
        let model_id = match authorized.profile.cli {
            UpstreamSubscriptionCli::Claude => "claude-subscription",
            UpstreamSubscriptionCli::Codex => "codex-subscription",
        };
        let relay = SubscriptionRelay::new(relay_config);
        Ok(Self {
            provider_id: card.provider.id.clone(),
            model_id,
            service: SubscriptionService::new(relay, DEFAULT_SHUTDOWN_WAIT),
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn service(&self) -> &SubscriptionService {
        &self.service
    }

    pub fn model_id(&self) -> &'static str {
        self.model_id
    }
}

#[derive(Clone, Default)]
pub struct SubscriptionRuntimeRegistry {
    handles: BTreeMap<String, SubscriptionRuntimeHandle>,
}

impl SubscriptionRuntimeRegistry {
    pub fn insert(
        &mut self,
        handle: SubscriptionRuntimeHandle,
    ) -> Result<(), SubscriptionRuntimeError> {
        let provider = handle.provider_id.clone();
        if self.handles.insert(provider.clone(), handle).is_some() {
            return Err(SubscriptionRuntimeError::WrongProvider {
                expected: "unique subscription provider".into(),
                actual: provider,
            });
        }
        Ok(())
    }

    pub fn get(
        &self,
        provider_id: &str,
    ) -> Result<&SubscriptionRuntimeHandle, SubscriptionRuntimeError> {
        self.handles
            .get(provider_id)
            .ok_or_else(|| SubscriptionRuntimeError::MissingHandle(provider_id.to_string()))
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::subscription::config::SANDBOX_PROFILE_ID;

    fn subscription_card() -> ComponentCard {
        serde_yaml::from_str(include_str!(
            "../../../../config/components/subscription.yaml"
        ))
        .unwrap()
    }

    fn config(enable: bool, disable: bool, cli: &str) -> SubscriptionConfig {
        SubscriptionConfig::snapshot(
            Some("personal"),
            enable.then_some("1"),
            disable.then_some("1"),
            Some(SANDBOX_PROFILE_ID),
            Some(cli),
        )
    }

    #[test]
    fn disabled_or_default_off_never_constructs_a_runtime() {
        let card = subscription_card();
        for cfg in [config(false, false, "claude"), config(true, true, "claude")] {
            assert!(authorize_subscription(&cfg, &card).unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn only_authorized_subscription_card_can_build_a_bound_handle() {
        let card = subscription_card();
        let authorized = authorize_subscription(&config(true, false, "codex"), &card)
            .unwrap()
            .unwrap();
        assert_eq!(authorized.provider_id(), "subscription");
        let handle = SubscriptionRuntimeHandle::build(authorized, &card).unwrap();
        assert_eq!(handle.provider_id(), "subscription");
        assert_eq!(handle.service().active_requests().await, 0);
    }

    #[test]
    fn factory_revalidates_card_and_rejects_provider_mismatch() {
        let card = subscription_card();
        let authorized = authorize_subscription(&config(true, false, "claude"), &card)
            .unwrap()
            .unwrap();
        let mut changed = card.clone();
        changed.provider.id = "other".into();
        assert!(SubscriptionRuntimeHandle::build(authorized, &changed).is_err());
    }

    #[test]
    fn registry_is_provider_bound_and_missing_handle_is_fixed_error() {
        let card = subscription_card();
        let authorized = authorize_subscription(&config(true, false, "claude"), &card)
            .unwrap()
            .unwrap();
        let handle = SubscriptionRuntimeHandle::build(authorized, &card).unwrap();
        let mut registry = SubscriptionRuntimeRegistry::default();
        registry.insert(handle).unwrap();
        assert_eq!(
            registry.get("subscription").unwrap().provider_id(),
            "subscription"
        );
        let error = match registry.get("wrong") {
            Ok(_) => panic!("wrong provider must not resolve a runtime handle"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "subscription runtime handle unavailable for provider wrong"
        );
    }
}
