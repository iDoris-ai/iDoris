//! Construction of adapters for explicitly selected provider cards.

use std::sync::Arc;
use std::time::Duration;

use idoris_backend::{BackendError, RuntimeAdapter};
use idoris_contracts::ComponentCard;

/// Process-environment parameters an adapter needs at construction, resolved **once at start-up** by
/// [`AdapterParams::from_env`] (an invalid value is a start-up failure) and handed to [`create_adapter_with`] when
/// the adapter is built. With managed engines the adapters are only built after the start-up gate settles; reading
/// the environment then made an invalid `IDORIS_LOAD_TIMEOUT_S` fail start-up only when the engine happened to be
/// ready (a degraded gate skipped the card and never looked at the variable).
///
/// Not in here, on purpose: the oMLX API key. It depends on the engine's state (a managed engine's private key is
/// only registered after the engine is up) and is resolved when the adapter is constructed, so adapters themselves
/// cannot be built early; only parameters that do not depend on the engine can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterParams {
    /// Deadline of oMLX's `POST /v1/models/{id}/load` (`IDORIS_LOAD_TIMEOUT_S`, IDORIS-CORE S8).
    pub omlx_load_timeout: Duration,
}

impl Default for AdapterParams {
    /// What an unset environment resolves to.
    fn default() -> Self {
        Self {
            omlx_load_timeout: super::omlx::DEFAULT_LOAD_TIMEOUT,
        }
    }
}

impl AdapterParams {
    /// Read and validate every adapter parameter from the process environment. An invalid override fails here
    /// instead of silently using the default.
    pub fn from_env() -> Result<Self, BackendError> {
        Ok(Self {
            omlx_load_timeout: super::omlx::load_timeout_from_env(
                std::env::var_os(super::omlx::LOAD_TIMEOUT_ENV).as_deref(),
            )?,
        })
    }
}

/// Construct the adapter named by `card`; host detection is only a recommendation. Reads the environment
/// parameters itself (and only for a provider that has any); start-up code that must fail early resolves them with
/// [`AdapterParams::from_env`] and calls [`create_adapter_with`].
pub fn create_adapter(card: &ComponentCard) -> Result<Arc<dyn RuntimeAdapter>, BackendError> {
    match card.provider.id.as_str() {
        "omlx" => create_adapter_with(card, &AdapterParams::from_env()?),
        provider => Err(no_adapter(provider)),
    }
}

/// [`create_adapter`] with already-resolved parameters: reads nothing from the environment except the oMLX API key
/// (see [`AdapterParams`]).
pub fn create_adapter_with(
    card: &ComponentCard,
    params: &AdapterParams,
) -> Result<Arc<dyn RuntimeAdapter>, BackendError> {
    build_adapter(card, params, true)
}

/// [`create_adapter_with`] for a start-up **dry run**: the same provider check and the same construction (so a card that
/// has no adapter, or whose parameters are refused, fails the same way), but **no credential is resolved**: the key is not
/// read from the environment or from `~/.omlx/settings.json`. Credentials are resolved only when the adapter is really
/// built (a managed engine's private key is registered only after the engine is up, and an engine that ends up degraded
/// never needs an adapter at all), so the dry run must not touch the user's key file early.
pub fn create_adapter_unkeyed(
    card: &ComponentCard,
    params: &AdapterParams,
) -> Result<Arc<dyn RuntimeAdapter>, BackendError> {
    build_adapter(card, params, false)
}

fn build_adapter(
    card: &ComponentCard,
    params: &AdapterParams,
    resolve_credentials: bool,
) -> Result<Arc<dyn RuntimeAdapter>, BackendError> {
    match card.provider.id.as_str() {
        "omlx" => {
            let config = if resolve_credentials {
                super::OmlxAdapterConfig::for_base_url(card.endpoint.clone())
            } else {
                super::OmlxAdapterConfig::unkeyed_for_base_url(card.endpoint.clone())
            };
            // A load is not a management call (IDORIS-CORE S8): it gets its own, much longer deadline.
            Ok(Arc::new(
                super::OmlxAdapter::new(config)?.with_load_timeout(params.omlx_load_timeout),
            ))
        }
        provider => Err(no_adapter(provider)),
    }
}

fn no_adapter(provider: &str) -> BackendError {
    BackendError::invalid_request(format!("no adapter for provider {provider}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::detect::{BackendKind, HostArch, HostFacts, HostPlatform, detect_backend};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn card(provider: &str, endpoint: &str) -> ComponentCard {
        serde_json::from_value(serde_json::json!({
            "provider": {"id": provider, "family": "local", "tier": "local",
                "capabilities": ["chat"], "privacy_class": "local_only",
                "cost": {"input_per_m": 0, "output_per_m": 0}, "locality": "loopback"},
            "form": "http_service", "endpoint": endpoint, "version_pin": "test",
            "privacy_class": "local_only", "allowed_egress": ["loopback"],
            "fallback_policy": "fail_closed", "fail_closed": true
        }))
        .expect("valid component card")
    }

    #[tokio::test]
    async fn explicit_omlx_constructs_without_io_and_lists_upstream_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"id": "qwen3-8b", "estimated_size": 6_u64 * 1024 * 1024 * 1024}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let recommendation = detect_backend(HostFacts {
            platform: HostPlatform::Linux,
            arch: HostArch::Arm64,
            has_nvidia_gpu: false,
        });
        assert_eq!(recommendation.kind, BackendKind::LlamaCpp);
        assert!(recommendation.implemented);
        let adapter = create_adapter(&card("omlx", &server.uri())).expect("adapter constructs");
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
        let models = adapter.list().await.expect("upstream list succeeds");
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["qwen3-8b"]
        );
    }

    /// The parameters resolved at start-up are what the adapter is built with: `create_adapter_with` reads none of
    /// them from the environment (IDORIS-CORE audit, erratum 12 (3)).
    #[test]
    fn create_adapter_with_builds_the_adapter_from_the_resolved_parameters() {
        let params = AdapterParams {
            omlx_load_timeout: Duration::from_secs(7),
        };
        let adapter = create_adapter_with(&card("omlx", "http://127.0.0.1:8088"), &params)
            .expect("adapter constructs");
        assert_eq!(adapter.load_timeout(), Some(Duration::from_secs(7)));
        // What an unset environment resolves to is the documented 180 seconds.
        assert_eq!(
            AdapterParams::default().omlx_load_timeout,
            Duration::from_secs(180)
        );
        let adapter = create_adapter_with(
            &card("omlx", "http://127.0.0.1:8088"),
            &AdapterParams::default(),
        )
        .expect("adapter constructs");
        assert_eq!(adapter.load_timeout(), Some(Duration::from_secs(180)));
        // Same fail-closed answer as `create_adapter` for a provider that has no adapter.
        let Err(error) = create_adapter_with(
            &card("vllm", "http://127.0.0.1:8088"),
            &AdapterParams::default(),
        ) else {
            panic!("unknown provider must not fall through to oMLX");
        };
        assert!(error.to_string().contains("no adapter for provider vllm"));
    }

    #[test]
    fn unknown_providers_fail_closed() {
        for provider in ["omlx_typo", "vllm", "llama_cpp", "mock", "subscription"] {
            let error = match create_adapter(&card(provider, "http://127.0.0.1:8088")) {
                Err(error) => error,
                Ok(_) => panic!("unknown provider must not fall through to oMLX"),
            };
            assert!(
                error
                    .to_string()
                    .contains(&format!("no adapter for provider {provider}"))
            );
        }
    }
}
