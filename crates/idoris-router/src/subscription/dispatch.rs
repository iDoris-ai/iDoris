use std::net::SocketAddr;

use idoris_backend::{ChatMessage, ChatRequest, ChatResponse};
use idoris_contracts::common::PrivacyClass;
use idoris_policy::SUBSCRIPTION_PROVIDER_ID;
use tokio_util::sync::CancellationToken;

use crate::dispatch::Selected;

use super::runtime::{SubscriptionRuntimeError, SubscriptionRuntimeRegistry};
use super::source::{SubscriptionSourceError, check_subscription_source};

#[derive(Debug)]
pub enum SubscriptionDispatchError {
    NotSubscription,
    PrivacyForbidden,
    Source(SubscriptionSourceError),
    Runtime(SubscriptionRuntimeError),
    Relay(idoris_upstream::subscription::error::SubscriptionRelayError),
}

impl SubscriptionDispatchError {
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::NotSubscription => "SUBSCRIPTION_NOT_SELECTED",
            Self::PrivacyForbidden => "SUBSCRIPTION_PRIVACY_FORBIDDEN",
            Self::Source(error) => error.reason_code(),
            Self::Runtime(_) => "SUBSCRIPTION_RUNTIME_UNAVAILABLE",
            Self::Relay(error) => error.reason_code(),
        }
    }
}

impl std::fmt::Display for SubscriptionDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason_code())
    }
}

impl std::error::Error for SubscriptionDispatchError {}

pub async fn dispatch_selected(
    selected: &Selected,
    privacy: PrivacyClass,
    peer: Option<SocketAddr>,
    runtimes: &SubscriptionRuntimeRegistry,
    messages: Vec<ChatMessage>,
    cancel: CancellationToken,
) -> Result<ChatResponse, SubscriptionDispatchError> {
    if selected.card.provider.id != SUBSCRIPTION_PROVIDER_ID {
        return Err(SubscriptionDispatchError::NotSubscription);
    }
    if privacy == PrivacyClass::LocalOnly {
        return Err(SubscriptionDispatchError::PrivacyForbidden);
    }
    check_subscription_source(peer).map_err(SubscriptionDispatchError::Source)?;
    let handle = runtimes
        .get(&selected.card.provider.id)
        .map_err(SubscriptionDispatchError::Runtime)?;
    handle
        .service()
        .chat(
            ChatRequest {
                model: handle.model_id().to_string(),
                messages,
            },
            cancel,
        )
        .await
        .map_err(SubscriptionDispatchError::Relay)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
    use idoris_contracts::provider::Locality;
    use idoris_policy::{Decision, SUBSCRIPTION_PROVIDER_ID};
    use idoris_upstream::subscription::profile::SandboxProfile as UpstreamSandboxProfile;
    use idoris_upstream::subscription::relay::SubscriptionRelayConfig;

    use super::*;
    use crate::subscription::config::{SANDBOX_PROFILE_ID, SubscriptionConfig};
    use crate::subscription::runtime::{SubscriptionRuntimeHandle, authorize_subscription};

    fn card() -> idoris_contracts::ComponentCard {
        serde_yaml::from_str(include_str!(
            "../../../../config/components/subscription.yaml"
        ))
        .unwrap()
    }

    fn selected() -> Selected {
        Selected {
            decision: Decision {
                chosen_id: SUBSCRIPTION_PROVIDER_ID.into(),
                reason_codes: Vec::new(),
                degradations: Vec::new(),
            },
            served_locality: Locality::Remote,
            card: card(),
            load_policy: LoadPolicy {
                mode: LoadMode::OnDemand,
                keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
                admission: Admission::Coexist,
            },
            estimated_cost_minor: 0,
        }
    }

    fn config() -> SubscriptionConfig {
        SubscriptionConfig::snapshot(
            Some("personal"),
            Some("1"),
            None,
            Some(SANDBOX_PROFILE_ID),
            Some("claude"),
        )
    }

    fn runtime_registry() -> (tempfile::TempDir, SubscriptionRuntimeRegistry) {
        let dir = tempfile::TempDir::new().unwrap();
        let program = dir.path().join("claude");
        fs::write(&program, "#!/bin/sh\ncat\n").unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();

        let card = card();
        let authorized = authorize_subscription(&config(), &card).unwrap().unwrap();
        let relay_config = SubscriptionRelayConfig::with_environment(
            UpstreamSandboxProfile::fixed(
                idoris_upstream::subscription::profile::SubscriptionCli::Claude,
            ),
            BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from(format!("{}:/usr/bin:/bin", dir.path().display())),
            )]),
        );
        let handle =
            SubscriptionRuntimeHandle::build_with_relay_config(authorized, &card, relay_config)
                .unwrap();
        let mut registry = SubscriptionRuntimeRegistry::default();
        registry.insert(handle).unwrap();
        (dir, registry)
    }

    #[tokio::test]
    async fn privacy_and_source_fail_before_handle_lookup() {
        let empty = SubscriptionRuntimeRegistry::default();
        let selected = selected();
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "sentinel".into(),
        }];
        assert_eq!(
            dispatch_selected(
                &selected,
                PrivacyClass::LocalOnly,
                Some("127.0.0.1:1234".parse().unwrap()),
                &empty,
                messages.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .reason_code(),
            "SUBSCRIPTION_PRIVACY_FORBIDDEN"
        );
        assert_eq!(
            dispatch_selected(
                &selected,
                PrivacyClass::Any,
                Some("10.0.0.2:1234".parse().unwrap()),
                &empty,
                messages,
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .reason_code(),
            "SUBSCRIPTION_SOURCE_FORBIDDEN"
        );
    }

    #[tokio::test]
    async fn loopback_selected_subscription_calls_exactly_its_bound_relay() {
        let (_dir, registry) = runtime_registry();
        let response = dispatch_selected(
            &selected(),
            PrivacyClass::Any,
            Some("127.0.0.1:1234".parse().unwrap()),
            &registry,
            vec![ChatMessage {
                role: "user".into(),
                content: "relay-once".into(),
            }],
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(response.model, "claude-subscription");
        assert_eq!(response.content, "relay-once");
    }

    #[tokio::test]
    async fn missing_handle_is_terminal_and_never_falls_back() {
        let error = dispatch_selected(
            &selected(),
            PrivacyClass::Any,
            Some("127.0.0.1:1234".parse().unwrap()),
            &SubscriptionRuntimeRegistry::default(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.reason_code(), "SUBSCRIPTION_RUNTIME_UNAVAILABLE");
    }
}
