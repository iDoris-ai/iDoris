//! `GET /v1/models`: aggregates every registered `http_service` card's own
//! `/v1/models` listing, mirroring `packages/router/src/server.ts`'s
//! handler — best-effort per card, except authentication failures, which
//! fail the request to prevent an incomplete list from appearing complete.

use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_contracts::component_card::Form;
use idoris_contracts::provider::Locality;
use reqwest::header::{AUTHORIZATION, HeaderValue};
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

#[derive(Debug)]
pub enum ModelsError {
    UpstreamAuthenticationFailed { locality: Locality },
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
    api_key: Option<&str>,
) -> Result<Vec<ModelEntry>, ModelsError> {
    if card.form != Form::HttpService {
        return Ok(Vec::new());
    }
    let url = format!("{}/v1/models", card.endpoint.trim_end_matches('/'));
    let mut request = client.get(&url).timeout(MODELS_TIMEOUT);
    if is_loopback_omlx(card)
        && let Some(key) = api_key.filter(|key| !key.is_empty())
    {
        let Ok(mut authorization) = HeaderValue::from_str(&format!("Bearer {key}")) else {
            eprintln!(
                "{{\"event\":\"upstream_model_listing_authentication_failed\",\"reason\":\"invalid_authorization_header\",\"provider_id\":{},\"locality\":{}}}",
                serde_json::to_string(&card.provider.id)
                    .unwrap_or_else(|_| "\"unknown\"".to_string()),
                serde_json::to_string(crate::locality_str(card.provider.locality))
                    .unwrap_or_else(|_| "\"unknown\"".to_string())
            );
            return Err(ModelsError::UpstreamAuthenticationFailed {
                locality: card.provider.locality,
            });
        };
        authorization.set_sensitive(true);
        request = request.header(AUTHORIZATION, authorization);
    }
    let Ok(resp) = request.send().await else {
        return Ok(Vec::new());
    };
    if matches!(resp.status().as_u16(), 401 | 403) {
        eprintln!(
            "{{\"event\":\"upstream_model_listing_authentication_failed\",\"provider_id\":{},\"locality\":{},\"status\":{}}}",
            serde_json::to_string(&card.provider.id).unwrap_or_else(|_| "\"unknown\"".to_string()),
            serde_json::to_string(crate::locality_str(card.provider.locality))
                .unwrap_or_else(|_| "\"unknown\"".to_string()),
            resp.status().as_u16()
        );
        return Err(ModelsError::UpstreamAuthenticationFailed {
            locality: card.provider.locality,
        });
    }
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Ok(Vec::new());
    };
    let Some(items) = body.get("data").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
        .map(|id| ModelEntry {
            id: id.to_string(),
            object: "model",
            owned_by: card.provider.id.clone(),
        })
        .collect())
}

/// Sequential, matching TS's own `for (const { card, backend } of
/// registered)` loop — not a locked ordering contract, just parity with the
/// reference (a concurrent `join_all` would be a valid follow-up, not a
/// behavior change any test here depends on).
async fn list_models_with_key(
    client: &reqwest::Client,
    cards: &[ComponentCard],
    api_key: Option<&str>,
) -> Result<ModelsResponse, ModelsError> {
    let mut data = Vec::new();
    for card in cards {
        data.extend(list_one(client, card, api_key).await?);
    }
    Ok(ModelsResponse {
        object: "list",
        data,
    })
}

pub async fn list_models(
    client: &reqwest::Client,
    cards: &[ComponentCard],
) -> Result<ModelsResponse, ModelsError> {
    let config = idoris_upstream::OmlxAdapterConfig::default();
    list_models_with_key(client, cards, config.api_key.as_deref()).await
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
            version_pin: "omlx@0.6.4".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            allowed_egress: vec![Egress::Loopback],
            fallback_policy: FallbackPolicy::FailClosed,
            fail_closed: true,
            load_policy: None,
            extensions: None,
        }
    }

    async fn models_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[]})))
            .mount(&server)
            .await;
        server
    }

    async fn assert_no_authorization(
        client: &reqwest::Client,
        server: &MockServer,
        entry: ComponentCard,
        case_name: &str,
    ) {
        list_models_with_key(client, &[entry], Some("secret"))
            .await
            .unwrap_or_else(|_| panic!("{case_name}: listing should succeed"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "{case_name}: request must reach upstream"
        );
        assert!(
            !requests[0].headers.contains_key("authorization"),
            "{case_name}: authorization must be omitted"
        );
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
        let resp = list_models_with_key(&client, &cards, None).await.unwrap();
        assert_eq!(resp.object, "list");
        let ids: Vec<&str> = resp.data.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["model-a", "model-b"]);
        for m in &resp.data {
            assert_eq!(m.object, "model");
            assert_eq!(m.owned_by, "omlx");
        }
    }

    #[tokio::test]
    async fn auth_failure_rejects_even_a_partial_successful_listing() {
        let good = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"id":"good"}]})),
            )
            .mount(&good)
            .await;
        for status in [401, 403] {
            let denied = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&denied)
                .await;
            let cards = [
                card("good", &good.uri(), Form::HttpService),
                card("Qwen3-0.6B-4bit", &denied.uri(), Form::HttpService),
            ];
            assert!(matches!(
                list_models_with_key(&reqwest::Client::new(), &cards, None).await,
                Err(ModelsError::UpstreamAuthenticationFailed { .. })
            ));
        }
    }

    #[tokio::test]
    async fn a_non_http_service_card_is_skipped_without_any_network_call() {
        let client = reqwest::Client::new();
        let cards = vec![card("subscription", "spawn://x", Form::SpawnCli)];
        let resp = list_models_with_key(&client, &cards, None).await.unwrap();
        assert!(resp.data.is_empty());
    }

    #[tokio::test]
    async fn invalid_authorization_keys_fail_before_any_upstream_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        for key in ["bad\nkey", "bad\rkey", "bad\r\nkey"] {
            let result = list_models_with_key(
                &reqwest::Client::new(),
                &[card("omlx", &server.uri(), Form::HttpService)],
                Some(key),
            )
            .await;
            assert!(matches!(
                result,
                Err(ModelsError::UpstreamAuthenticationFailed { .. })
            ));
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn credentials_are_isolated_from_non_loopback_locality() {
        let server = models_server().await;
        let mut entry = card("omlx", &server.uri(), Form::HttpService);
        entry.provider.locality = Locality::Remote;
        assert_no_authorization(&reqwest::Client::new(), &server, entry, "remote locality").await;
    }

    #[tokio::test]
    async fn credentials_are_isolated_from_non_omlx_version() {
        let server = models_server().await;
        let mut entry = card("omlx", &server.uri(), Form::HttpService);
        entry.version_pin = "other@1.0".into();
        assert_no_authorization(&reqwest::Client::new(), &server, entry, "non-oMLX version").await;
    }

    #[tokio::test]
    async fn credentials_are_isolated_from_non_loopback_url_host() {
        let server = models_server().await;
        let port = reqwest::Url::parse(&server.uri()).unwrap().port().unwrap();
        let endpoint = format!("http://upstream.test:{port}");
        let socket = server.address();
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve("upstream.test", *socket)
            .build()
            .unwrap();
        let entry = card("omlx", &endpoint, Form::HttpService);
        assert_no_authorization(&client, &server, entry, "non-loopback URL host").await;
    }

    #[tokio::test]
    async fn credentials_are_isolated_from_non_local_family() {
        let server = models_server().await;
        let mut entry = card("omlx", &server.uri(), Form::HttpService);
        entry.provider.family = Family::Other;
        assert_no_authorization(&reqwest::Client::new(), &server, entry, "non-local family").await;
    }
}
