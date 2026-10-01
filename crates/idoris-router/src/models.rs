//! `GET /v1/models`: aggregates every registered `http_service` card's own
//! `/v1/models` listing, mirroring `packages/router/src/server.ts`'s
//! handler — best-effort per card: a card whose upstream call fails (network
//! error, non-2xx, unparseable body) is silently skipped, never fails the
//! whole request. Three consecutive failures trigger a 30-second provider
//! cooldown; a successful listing (including empty data) resets it. The
//! loop continues to the next registered provider either way.

use std::time::{Duration, Instant};

use crate::health::HealthTracker;

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

/// A successful empty list is distinct from a failed discovery request.
async fn list_one(client: &reqwest::Client, card: &ComponentCard) -> Option<Vec<ModelEntry>> {
    let url = format!("{}/v1/models", card.endpoint.trim_end_matches('/'));
    let Ok(resp) = client.get(&url).timeout(MODELS_TIMEOUT).send().await else {
        return None;
    };
    if !resp.status().is_success() {
        return None;
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return None;
    };
    let items = body.get("data").and_then(|v| v.as_array())?;
    let entries = items
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
        .map(|id| ModelEntry {
            id: id.to_string(),
            object: "model",
            owned_by: card.provider.id.clone(),
        })
        .collect();
    Some(entries)
}

/// Sequential, matching TS's own `for (const { card, backend } of
/// registered)` loop — not a locked ordering contract, just parity with the
/// reference (a concurrent `join_all` would be a valid follow-up, not a
/// behavior change any test here depends on).
pub async fn list_models(
    client: &reqwest::Client,
    cards: &[ComponentCard],
    health: &HealthTracker,
) -> ModelsResponse {
    let mut data = Vec::new();
    for card in cards {
        let id = &card.provider.id;
        if card.form != Form::HttpService || health.is_cooling_down(id, Instant::now()) {
            continue;
        }
        let result = list_one(client, card).await;
        health.record(id, result.is_some(), Instant::now());
        data.extend(result.unwrap_or_default());
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
        let resp = list_models(&client, &cards, &HealthTracker::default()).await;
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
        for response in [
            ResponseTemplate::new(500),
            ResponseTemplate::new(200).set_body_string("not-json"),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/v1/models"))
                .respond_with(response)
                .expect(3)
                .mount(&server)
                .await;
            let client = reqwest::Client::new();
            let cards = vec![card("omlx", &server.uri(), Form::HttpService)];
            let health = HealthTracker::default();
            for _ in 0..4 {
                assert!(list_models(&client, &cards, &health).await.data.is_empty());
            }
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn empty_success_resets_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .expect(4)
            .mount(&server)
            .await;
        let health = HealthTracker::default();
        let cards = vec![card("omlx", &server.uri(), Form::HttpService)];
        let client = reqwest::Client::new();
        for _ in 0..4 {
            health.record("omlx", false, Instant::now());
            assert!(list_models(&client, &cards, &health).await.data.is_empty());
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn a_non_http_service_card_is_skipped_without_any_network_call() {
        let client = reqwest::Client::new();
        let cards = vec![card("subscription", "spawn://x", Form::SpawnCli)];
        let resp = list_models(&client, &cards, &HealthTracker::default()).await;
        assert!(resp.data.is_empty());
    }
}
