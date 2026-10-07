//! Construction of adapters for explicitly selected provider cards.

use std::sync::Arc;

use idoris_backend::{BackendError, RuntimeAdapter};
use idoris_contracts::ComponentCard;

/// Construct the adapter named by `card`; host detection is only a recommendation.
pub fn create_adapter(card: &ComponentCard) -> Result<Arc<dyn RuntimeAdapter>, BackendError> {
    match card.provider.id.as_str() {
        "omlx" => {
            let mut config = super::OmlxAdapterConfig::default();
            config.base_url.clone_from(&card.endpoint);
            Ok(Arc::new(super::OmlxAdapter::new(config)?))
        }
        provider => Err(BackendError::invalid_request(format!(
            "no adapter for provider {provider}"
        ))),
    }
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
