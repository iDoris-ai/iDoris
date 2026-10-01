//! `GET /v1/models`: aggregates every registered `http_service` card's own
//! `/v1/models` listing, mirroring `packages/router/src/server.ts`'s
//! handler — best-effort per card: a card whose upstream call fails (network
//! error, non-2xx, unparseable body) is silently skipped, never fails the
//! whole request (TS: `try { ... } catch { health.record(id, false) }`,
//! looping to the next `Registered` entry either way).

use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_contracts::component_card::Form;
use serde::Serialize;

/// TS's reference loop (`server.ts`'s `/v1/models` handler) applies no
/// timeout at all to each backend's `list()` call — an unresponsive
/// upstream blocks the whole request indefinitely. Rust applies one so
/// `/v1/models` fails closed against a hung upstream instead of hanging the
/// whole router; conformance doesn't pin this exact value (fake-upstream
/// always responds promptly for the fixtures that exercise this route), so
/// it's a defensive floor, not a locked contract.
const MODELS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub owned_by: String,
}

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
}

/// One card's contribution — `Vec::new()` on any failure (wrong `form`,
/// transport error, non-2xx, unparseable/unshaped body): best-effort, never
/// surfaced to the caller as a partial-failure error.
async fn list_one(client: &reqwest::Client, card: &ComponentCard) -> Vec<ModelEntry> {
    if card.form != Form::HttpService {
        return Vec::new();
    }
    let url = format!("{}/v1/models", card.endpoint.trim_end_matches('/'));
    let Ok(resp) = client.get(&url).timeout(MODELS_TIMEOUT).send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    let Some(items) = body.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
        .map(|id| ModelEntry {
            id: id.to_string(),
            object: "model",
            owned_by: card.provider.id.clone(),
        })
        .collect()
}

/// Sequential, matching TS's own `for (const { card, backend } of
/// registered)` loop — not a locked ordering contract, just parity with the
/// reference (a concurrent `join_all` would be a valid follow-up, not a
/// behavior change any test here depends on).
pub async fn list_models(client: &reqwest::Client, cards: &[ComponentCard]) -> ModelsResponse {
    let mut data = Vec::new();
    for card in cards {
        data.extend(list_one(client, card).await);
    }
    ModelsResponse {
        object: "list",
        data,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
    use idoris_contracts::component_card::Egress;
    use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn card(id: &str, endpoint: &str, form: Form) -> ComponentCard {
        ComponentCard {
            provider: ProviderDescriptor {
                id: id.to_string(),
                family: Family::Local,
                tier: Tier::Local,
                capabilities: vec![Capability::Chat],
                privacy_class: PrivacyClass::LocalOnly,
                cost: Cost {
                    input_per_m: 0.0,
                    output_per_m: 0.0,
                },
                locality: Locality::Loopback,
                extensions: None,
            },
            form,
            endpoint: endpoint.to_string(),
            version_pin: "test@0.0.0".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            allowed_egress: vec![Egress::Loopback],
            fallback_policy: FallbackPolicy::FailClosed,
            fail_closed: true,
            load_policy: None,
            extensions: None,
        }
    }

    #[tokio::test]
    async fn maps_id_object_owned_by_from_the_upstream_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "model-a"}, {"id": "model-b"}],
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let cards = vec![card("omlx", &server.uri(), Form::HttpService)];
        let resp = list_models(&client, &cards).await;
        assert_eq!(resp.object, "list");
        let ids: Vec<&str> = resp.data.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["model-a", "model-b"]);
        for m in &resp.data {
            assert_eq!(m.object, "model");
            assert_eq!(m.owned_by, "omlx");
        }
    }

    #[tokio::test]
    async fn a_failing_card_is_skipped_not_a_partial_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let cards = vec![card("omlx", &server.uri(), Form::HttpService)];
        let resp = list_models(&client, &cards).await;
        assert!(resp.data.is_empty());
    }

    #[tokio::test]
    async fn a_non_http_service_card_is_skipped_without_any_network_call() {
        let client = reqwest::Client::new();
        let cards = vec![card("subscription", "spawn://x", Form::SpawnCli)];
        let resp = list_models(&client, &cards).await;
        assert!(resp.data.is_empty());
    }
}
