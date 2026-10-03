//! `GET /v1/models`: aggregates every registered `http_service` card's own
//! `/v1/models` listing, mirroring `packages/router/src/server.ts`'s
//! handler — best-effort per card, except authentication failures, which
//! fail the request to prevent an incomplete list from appearing complete.
//! Other failures are skipped, with three consecutive failures triggering a
//! 30-second provider cooldown; a successful listing resets that state.

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use crate::health::HealthTracker;

use idoris_contracts::ComponentCard;
use idoris_contracts::component_card::Form;
use idoris_contracts::provider::Locality;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Serialize;
use serde_json::Value;

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

#[derive(Debug)]
pub enum ModelsError {
    UpstreamAuthenticationFailed { locality: Locality },
}

fn auth_failure(card: &ComponentCard, field: &str, value: Value) -> ModelsError {
    let mut event = serde_json::json!({
        "event": "upstream_model_listing_authentication_failed",
        "provider_id": card.provider.id,
        "locality": crate::locality_str(card.provider.locality),
    });
    event[field] = value;
    eprintln!("{event}");
    ModelsError::UpstreamAuthenticationFailed {
        locality: card.provider.locality,
    }
}

fn is_loopback_omlx(card: &ComponentCard) -> bool {
    if card.provider.family != idoris_contracts::provider::Family::Local
        || card.provider.locality != Locality::Loopback
        || !card.version_pin.starts_with("omlx@")
    {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(&card.endpoint) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

async fn list_one(
    client: &reqwest::Client,
    card: &ComponentCard,
    api_key: Option<&OsStr>,
) -> Result<Option<Vec<ModelEntry>>, ModelsError> {
    let url = format!("{}/v1/models", card.endpoint.trim_end_matches('/'));
    let mut request = client.get(&url).timeout(MODELS_TIMEOUT);
    if is_loopback_omlx(card)
        && let Some(key) = api_key.filter(|key| !key.is_empty())
    {
        // A present non-UTF-8 key is invalid, never an absent credential.
        let mut authorization = key
            .to_str()
            .and_then(|key| HeaderValue::from_str(&format!("Bearer {key}")).ok())
            .ok_or_else(|| auth_failure(card, "reason", "invalid_authorization_header".into()))?;
        authorization.set_sensitive(true);
        request = request.header(AUTHORIZATION, authorization);
    }
    let Ok(resp) = request.send().await else {
        return Ok(None);
    };
    if matches!(resp.status().as_u16(), 401 | 403) {
        return Err(auth_failure(card, "status", resp.status().as_u16().into()));
    }
    if !resp.status().is_success() {
        return Ok(None);
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Ok(None);
    };
    let Some(items) = body.get("data").and_then(|v| v.as_array()) else {
        return Ok(None);
    };
    Ok(Some(
        items
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
            .map(|id| ModelEntry {
                id: id.to_string(),
                object: "model",
                owned_by: card.provider.id.clone(),
            })
            .collect(),
    ))
}

/// Sequential, matching TS's own `for (const { card, backend } of
/// registered)` loop — not a locked ordering contract, just parity with the
/// reference (a concurrent `join_all` would be a valid follow-up, not a
/// behavior change any test here depends on).
pub async fn list_models(
    client: &reqwest::Client,
    cards: &[ComponentCard],
    health: &HealthTracker,
) -> Result<ModelsResponse, ModelsError> {
    let api_key = std::env::var_os(idoris_upstream::omlx::OMLX_API_KEY_ENV);
    let mut data = Vec::new();
    for card in cards {
        let id = &card.provider.id;
        if card.form != Form::HttpService || health.is_cooling_down(id, Instant::now()) {
            continue;
        }
        match list_one(client, card, api_key.as_deref()).await? {
            Some(entries) => {
                health.record(id, true, Instant::now());
                data.extend(entries);
            }
            None => health.record(id, false, Instant::now()),
        }
    }
    Ok(ModelsResponse {
        object: "list",
        data,
    })
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
        let resp = list_models(&client, &cards, &HealthTracker::default())
            .await
            .unwrap();
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
                .mount(&server)
                .await;
            let client = reqwest::Client::new();
            let cards = vec![card("omlx", &server.uri(), Form::HttpService)];
            let health = HealthTracker::default();
            for _ in 0..4 {
                let resp = list_models(&client, &cards, &health).await.unwrap();
                assert!(resp.data.is_empty());
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn successful_empty_listing_resets_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let cards = vec![card("omlx", &server.uri(), Form::HttpService)];
        let health = HealthTracker::default();
        for _ in 0..4 {
            health.record("omlx", false, Instant::now());
            let resp = list_models(&client, &cards, &health).await.unwrap();
            assert!(resp.data.is_empty());
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_non_http_service_card_is_skipped_without_any_network_call() {
        let client = reqwest::Client::new();
        let cards = vec![card("subscription", "spawn://x", Form::SpawnCli)];
        let resp = list_models(&client, &cards, &HealthTracker::default())
            .await
            .unwrap();
        assert!(resp.data.is_empty());
    }
}
