//! Construction of adapters for explicitly selected provider cards.

use std::sync::Arc;

use idoris_backend::{BackendError, RuntimeAdapter};
use idoris_contracts::ComponentCard;

/// Construct the adapter named by `card`; host detection is only a recommendation.
pub fn create_adapter(card: &ComponentCard) -> Result<Arc<dyn RuntimeAdapter>, BackendError> {
    match card.provider.id.as_str() {
        "omlx" => Ok(Arc::new(create_omlx_adapter(card)?)),
        provider => Err(BackendError::invalid_request(format!(
            "no adapter for provider {provider}"
        ))),
    }
}

/// The production oMLX adapter for `card` (IDORIS-CORE S6, Codex audit H2), concrete so a caller can still chain builder
/// calls on it.
///
/// **Whether it may carry a key at all is decided per card, by the same check the proxy uses**
/// ([`super::omlx::key::card_may_carry_omlx_key`]) — not by `provider.id`, which a handwritten card can claim. An eligible
/// card gets an adapter that looks the key up on **every request** and caches none (the engine manager may replace the
/// engine behind this endpoint at any time). Any other card — a handwritten `provider.id = "omlx"` card that carries
/// `extensions.idoris_upstream`, a non-loopback endpoint, a different product pin — gets an adapter with **no key and no
/// registry lookup**: no key from the environment, from `settings.json` or from the engine manager is ever sent to it.
pub fn create_omlx_adapter(card: &ComponentCard) -> Result<super::OmlxAdapter, BackendError> {
    // A load is not a management call (IDORIS-CORE S8): it gets its own, much longer deadline.
    // An invalid override fails here, at construction (start-up), instead of silently using the default.
    let load_timeout = super::omlx::load_timeout_from_env(
        std::env::var_os(super::omlx::LOAD_TIMEOUT_ENV).as_deref(),
    )?;
    // No key is read at construction either way: an eligible adapter resolves it per request.
    let config = super::OmlxAdapterConfig {
        base_url: card.endpoint.clone(),
        api_key: None,
        call_timeout: super::omlx::DEFAULT_CALL_TIMEOUT,
    };
    let adapter = super::OmlxAdapter::new(config)?.with_load_timeout(load_timeout);
    Ok(if super::omlx::key::card_may_carry_omlx_key(card) {
        adapter.with_per_request_key()
    } else {
        adapter
    })
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

    fn omlx_card(
        endpoint: &str,
        pin: &str,
        extensions: Option<serde_json::Value>,
    ) -> ComponentCard {
        let mut card = card("omlx", endpoint);
        card.version_pin = pin.to_owned();
        card.extensions = extensions.map(|value| value.as_object().unwrap().clone());
        card
    }

    async fn authorizations(server: &MockServer) -> Vec<Option<String>> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| {
                request
                    .headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_owned())
            })
            .collect()
    }

    /// IDORIS-CORE S6 (Codex audit H2a): whether the production adapter may carry a key is decided per card, by the proxy's
    /// own check, not by `provider.id` (which a handwritten card can claim). Whatever key the engine manager has registered
    /// for the endpoint, an adapter for any other card sends none.
    #[tokio::test]
    async fn only_a_card_the_proxy_would_give_a_key_gets_an_adapter_that_carries_one() {
        use crate::omlx::key::{EndpointKey, govern_endpoint, release_endpoint};
        use idoris_backend::RuntimeAdapter;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;
        let endpoint = server.uri();
        govern_endpoint(&endpoint, EndpointKey::Private("engine-key".into()));

        let eligible = create_adapter(&omlx_card(&endpoint, "omlx@0.6.4", None)).unwrap();
        assert!(
            eligible.engine_generation().is_some(),
            "looked up per request"
        );
        eligible.list().await.unwrap();

        let disguised = [
            // a handwritten card that claims to be oMLX but is a custom upstream
            omlx_card(
                &endpoint,
                "omlx@0.6.4",
                Some(
                    serde_json::json!({"idoris_upstream": {"kind": "openai_compat", "flavor": "omlx"}}),
                ),
            ),
            // not an oMLX product pin
            omlx_card(&endpoint, "openai_compat@unknown", None),
        ];
        for card in &disguised {
            let adapter = create_adapter(card).unwrap();
            assert!(
                adapter.engine_generation().is_none(),
                "{}",
                card.version_pin
            );
            adapter.list().await.unwrap();
        }
        release_endpoint(&endpoint);

        let sent = authorizations(&server).await;
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].as_deref(), Some("Bearer engine-key"));
        assert_eq!(
            sent[1], None,
            "a card with idoris_upstream never gets a key"
        );
        assert_eq!(
            sent[2], None,
            "a card that is not an oMLX product pin never gets a key"
        );
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
