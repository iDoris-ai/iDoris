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

use crate::subscription::runtime::SubscriptionRuntimeRegistry;

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

pub(crate) enum HttpModelObservation {
    Observed(Vec<ModelEntry>),
    Unavailable,
    /// Kill switch engaged and the card is not trusted-local. No probe was sent.
    Suppressed,
}

enum OneList {
    Entries(Vec<ModelEntry>),
    Unavailable,
    Suppressed,
}

/// Trusted-local is the same three-part gate the router uses for `local_only`:
/// effective loopback, `local_only` privacy, and egress limited to none/loopback.
/// Shared with the `/health` upstream prober (`readiness`), so the kill switch treats
/// `/v1/models` and the prober identically.
pub(crate) fn is_trusted_local(card: &ComponentCard) -> bool {
    idoris_policy::privacy::is_local_capable(&idoris_policy::Card {
        component: card.clone(),
        roles: Vec::new(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: None,
        admission_status: idoris_policy::AdmissionStatus::Ready,
    })
}

fn suppress_untrusted_remote(card: &ComponentCard, remote_kill_switch: bool) -> bool {
    remote_kill_switch && !is_trusted_local(card)
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

/// 这张卡能不能带 oMLX 的私有 key（`GET /v1/models` 与代理转发共用这一处判断；接口规范修订 1.6）。
///
/// 规则按卡识别：本地族、loopback、`version_pin` 的产品名是 `omlx`、端点是不带凭据的 loopback 主机，
/// **且卡不带 `extensions.idoris_upstream`**。`omlx` 与伴生卡 `omlx-embed` 都满足；自定义 OpenAI 兼容上游
/// （`setup upstream` 的卡必带 `idoris_upstream`）即使把 `flavor`、`version_pin` 写成 oMLX、端口写成 oMLX 的，
/// 也不带 key。key 的来源不在这里（见 `idoris_upstream::omlx::key`），S5 换成私有 key 表时只改来源。
pub(crate) fn is_loopback_omlx(card: &ComponentCard) -> bool {
    if card.provider.family != idoris_contracts::provider::Family::Local
        || card.provider.locality != Locality::Loopback
        || !card.version_pin.starts_with("omlx@")
        || card
            .extensions
            .as_ref()
            .is_some_and(|ext| ext.contains_key(crate::engine_kind::UPSTREAM_EXTENSION))
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

async fn fetch_models_body(
    client: &reqwest::Client,
    card: &ComponentCard,
    api_key: Option<&OsStr>,
) -> Result<Option<Value>, ModelsError> {
    let url = format!("{}/v1/models", card.endpoint.trim_end_matches('/'));
    let mut request = client.get(&url).timeout(MODELS_TIMEOUT);
    // 引擎管理器接管的端点（托管卡，接口规范 §2.7.7）：自有引擎只用它的私有 key，外部引擎的 settings.json key 要先过
    // 端口属主校验；这个口径覆盖调用方传进来的 key。没有登记的端点沿用调用方的 key（行为不变）。
    let governed = idoris_upstream::omlx::key::resolve_governed_os(&card.endpoint);
    let api_key = governed.as_ref().map_or(api_key, |key| key.as_deref());
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
    Ok(Some(body))
}

async fn list_one(
    client: &reqwest::Client,
    card: &ComponentCard,
    api_key: Option<&OsStr>,
    remote_kill_switch: bool,
) -> Result<OneList, ModelsError> {
    if suppress_untrusted_remote(card, remote_kill_switch) {
        return Ok(OneList::Suppressed);
    }
    let Some(body) = fetch_models_body(client, card, api_key).await? else {
        return Ok(OneList::Unavailable);
    };
    let Some(items) = body.get("data").and_then(|v| v.as_array()) else {
        return Ok(OneList::Unavailable);
    };
    Ok(OneList::Entries(
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

pub(crate) async fn observe_http_models(
    client: &reqwest::Client,
    card: &ComponentCard,
    remote_kill_switch: bool,
) -> Result<HttpModelObservation, ModelsError> {
    if suppress_untrusted_remote(card, remote_kill_switch) {
        return Ok(HttpModelObservation::Suppressed);
    }
    // `fetch_models_body` attaches a key only to loopback oMLX cards, so the
    // settings.json key (loopback-only) is safe to resolve here.
    let api_key = idoris_upstream::omlx::key::resolve_omlx_api_key_os(true);
    let Some(body) = fetch_models_body(client, card, api_key.as_deref()).await? else {
        return Ok(HttpModelObservation::Unavailable);
    };
    let Some(items) = body.get("data").and_then(|value| value.as_array()) else {
        return Ok(HttpModelObservation::Unavailable);
    };
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let Some(id) = item.get("id").and_then(|value| value.as_str()) else {
            return Ok(HttpModelObservation::Unavailable);
        };
        entries.push(ModelEntry {
            id: id.to_string(),
            object: "model",
            owned_by: card.provider.id.clone(),
        });
    }
    Ok(HttpModelObservation::Observed(entries))
}

/// Sequential, matching TS's own `for (const { card, backend } of
/// registered)` loop — not a locked ordering contract, just parity with the
/// reference (a concurrent `join_all` would be a valid follow-up, not a
/// behavior change any test here depends on).
pub async fn list_models(
    client: &reqwest::Client,
    cards: &[ComponentCard],
    health: &HealthTracker,
    remote_kill_switch: bool,
) -> Result<ModelsResponse, ModelsError> {
    // `fetch_models_body` attaches a key only to loopback oMLX cards, so the
    // settings.json key (loopback-only) is safe to resolve here.
    let api_key = idoris_upstream::omlx::key::resolve_omlx_api_key_os(true);
    // 每条 (端点, 条目)：端点相同的卡列出同一批上游模型（主卡与伴生卡），后面按规则去重。
    let mut listed: Vec<(String, ModelEntry)> = Vec::new();
    for card in cards {
        let id = &card.provider.id;
        if card.form != Form::HttpService {
            continue;
        }
        // A trusted loopback oMLX card has credential semantics that must stay
        // fail-closed. Skipping its probe during cooldown could turn a new
        // 401/403 (or an invalid local credential) into a misleading partial
        // 200. Other providers retain the task-30 discovery cooldown.
        if !is_loopback_omlx(card) && health.is_cooling_down(id, Instant::now()) {
            continue;
        }
        match list_one(client, card, api_key.as_deref(), remote_kill_switch).await? {
            OneList::Entries(entries) => {
                health.record(id, true, Instant::now());
                let endpoint = endpoint_key(&card.endpoint);
                listed.extend(entries.into_iter().map(|entry| (endpoint.clone(), entry)));
            }
            OneList::Unavailable => health.record(id, false, Instant::now()),
            OneList::Suppressed => {}
        }
    }
    Ok(ModelsResponse {
        object: "list",
        data: dedupe_same_endpoint(listed),
    })
}

/// 端点的比较形式，按请求实际的发法来：请求是 `<endpoint 去掉末尾 '/'>/v1/models`，由 reqwest 解析成 URL，
/// 所以这里同样解析，只把 URL 规范化里本来就不分大小写的部分合并：scheme 与主机转小写、默认端口补全（`http` 的 80、
/// `https` 的 443）；用户信息、路径、查询保持原样（路径区分大小写），路径末尾的 `/` 去掉。`localhost` 与 `127.0.0.1`
/// 不做等价判断（当成两个端点，各列各的）。解析不了的端点只去末尾 `/`，不改大小写。
fn endpoint_key(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    let Ok(url) = reqwest::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return trimmed.to_string();
    };
    format!(
        "{}://{}{}:{}{}{}",
        url.scheme(),
        if url.username().is_empty() {
            String::new()
        } else {
            format!(
                "{}{}@",
                url.username(),
                url.password().map_or(String::new(), |p| format!(":{p}"))
            )
        },
        host,
        port,
        url.path().trim_end_matches('/'),
        url.query().map_or(String::new(), |q| format!("?{q}")),
    )
}

/// 同一个端点、同一个模型 id 只列一次（主卡 `omlx` 与伴生卡 `omlx-embed` 指向同一个引擎时，上游的模型不重复出现）。
/// 规则（确定性，与卡的加载顺序无关）：`provider.id` 字典序最小的卡胜出，所以主卡 `omlx` 压过 `omlx-embed`；
/// 条目的相对顺序保持原样。端点不同的卡各列各的，哪怕模型 id 相同：那是两个不同的引擎。
fn dedupe_same_endpoint(listed: Vec<(String, ModelEntry)>) -> Vec<ModelEntry> {
    let mut winner: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    for (endpoint, entry) in &listed {
        winner
            .entry((endpoint.clone(), entry.id.clone()))
            .and_modify(|best| {
                if entry.owned_by < *best {
                    best.clone_from(&entry.owned_by);
                }
            })
            .or_insert_with(|| entry.owned_by.clone());
    }
    let mut seen = std::collections::HashSet::new();
    listed
        .into_iter()
        .filter(|(endpoint, entry)| {
            let key = (endpoint.clone(), entry.id.clone());
            winner.get(&key) == Some(&entry.owned_by) && seen.insert(key)
        })
        .map(|(_, entry)| entry)
        .collect()
}

pub async fn list_models_with_subscriptions(
    client: &reqwest::Client,
    cards: &[ComponentCard],
    health: &HealthTracker,
    subscriptions: &SubscriptionRuntimeRegistry,
    remote_kill_switch: bool,
) -> Result<ModelsResponse, ModelsError> {
    let mut response = list_models(client, cards, health, remote_kill_switch).await?;
    response.data.extend(
        subscriptions
            .discovery()
            .into_iter()
            .map(|entry| ModelEntry {
                id: entry.model_id,
                object: "model",
                owned_by: entry.provider_id,
            }),
    );
    Ok(response)
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
        let resp = list_models(&client, &cards, &HealthTracker::default(), false)
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

    async fn models_server(ids: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        let data: Vec<_> = ids.iter().map(|id| serde_json::json!({"id": id})).collect();
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": data})),
            )
            .mount(&server)
            .await;
        server
    }

    fn owners(resp: &ModelsResponse) -> Vec<(String, String)> {
        resp.data
            .iter()
            .map(|m| (m.id.clone(), m.owned_by.clone()))
            .collect()
    }

    /// 主卡与伴生卡指向同一个端点：同一个模型 id 只出现一次。无论卡的加载顺序如何，`provider.id` 最小的卡（主卡）胜出。
    #[tokio::test]
    async fn main_and_companion_on_one_endpoint_list_each_model_once_and_the_main_card_wins() {
        let server = models_server(&["a", "b"]).await;
        let main = card("omlx", &server.uri(), Form::HttpService);
        let companion = card("omlx-embed", &server.uri(), Form::HttpService);
        for cards in [
            vec![main.clone(), companion.clone()],
            vec![companion.clone(), main.clone()],
        ] {
            let resp = list_models(
                &reqwest::Client::new(),
                &cards,
                &HealthTracker::default(),
                false,
            )
            .await
            .unwrap();
            assert_eq!(
                owners(&resp),
                [("a".into(), "omlx".into()), ("b".into(), "omlx".into())]
            );
        }
    }

    /// 端点写法只差末尾的 `/` 也算同一个端点。
    #[tokio::test]
    async fn a_trailing_slash_does_not_make_a_different_endpoint() {
        let server = models_server(&["a"]).await;
        let cards = vec![
            card("omlx", &server.uri(), Form::HttpService),
            card(
                "omlx-embed",
                &format!("{}/", server.uri()),
                Form::HttpService,
            ),
        ];
        let resp = list_models(
            &reqwest::Client::new(),
            &cards,
            &HealthTracker::default(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(owners(&resp), [("a".into(), "omlx".into())]);
    }

    /// 端点不同的卡各列各的，哪怕模型 id 相同；同一端点上两张卡各自独有的模型都保留。
    #[tokio::test]
    async fn separate_endpoints_still_list_separately_and_unique_models_survive() {
        let one = models_server(&["shared", "only-one"]).await;
        let two = models_server(&["shared"]).await;
        let cards = vec![
            card("omlx", &one.uri(), Form::HttpService),
            card("lmstudio", &two.uri(), Form::HttpService),
        ];
        let resp = list_models(
            &reqwest::Client::new(),
            &cards,
            &HealthTracker::default(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            owners(&resp),
            [
                ("shared".into(), "omlx".into()),
                ("only-one".into(), "omlx".into()),
                ("shared".into(), "lmstudio".into()),
            ]
        );
    }

    /// 端点比较：scheme、主机大小写与默认端口合并，末尾 `/` 去掉；路径的大小写、端口、用户信息、主机本身不同则不合并。
    #[test]
    fn endpoint_key_merges_host_case_and_default_ports_but_not_path_case() {
        let same = [
            ("http://127.0.0.1:8088", "HTTP://127.0.0.1:8088/"),
            ("http://LocalHost:8088", "http://localhost:8088"),
            ("http://example.test", "http://EXAMPLE.test:80/"),
            ("https://example.test/x", "https://example.test:443/x/"),
        ];
        for (a, b) in same {
            assert_eq!(endpoint_key(a), endpoint_key(b), "{a} {b}");
        }
        let different = [
            ("http://example.test/Api", "http://example.test/api"),
            ("http://example.test:8088", "http://example.test:8089"),
            ("http://example.test", "https://example.test"),
            ("http://127.0.0.1:8088", "http://localhost:8088"),
            ("http://a@example.test", "http://b@example.test"),
        ];
        for (a, b) in different {
            assert_ne!(endpoint_key(a), endpoint_key(b), "{a} {b}");
        }
        // 解析不了：只去末尾斜杠，大小写保持原样。
        assert_eq!(endpoint_key("Not A Url//"), "Not A Url");
    }

    /// 主机大小写不同的两张卡指向同一个端点：合并；路径大小写不同的是两个端点：不合并（各自请求各自的路径）。
    #[tokio::test]
    async fn host_case_merges_in_the_listing_and_path_case_does_not() {
        let server = models_server(&["a"]).await;
        let upper = server.uri().replace("http://", "HTTP://");
        let cards = vec![
            card("omlx", &server.uri(), Form::HttpService),
            card("omlx-embed", &upper, Form::HttpService),
        ];
        let resp = list_models(
            &reqwest::Client::new(),
            &cards,
            &HealthTracker::default(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(owners(&resp), [("a".into(), "omlx".into())]);

        // 路径不同的端点：两个 id 相同的条目都在（直接用去重函数，模拟两个路径下的上游各自返回同一个模型 id）。
        let entry = |endpoint: &str, owner: &str| {
            (
                endpoint_key(endpoint),
                ModelEntry {
                    id: "m".into(),
                    object: "model",
                    owned_by: owner.into(),
                },
            )
        };
        let out = dedupe_same_endpoint(vec![
            entry("http://example.test/Api", "p1"),
            entry("http://example.test/api", "p2"),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn dedupe_keeps_a_model_only_one_card_lists_and_drops_a_repeat_inside_one_card() {
        let entry = |id: &str, owner: &str| {
            (
                "http://x".to_string(),
                ModelEntry {
                    id: id.into(),
                    object: "model",
                    owned_by: owner.into(),
                },
            )
        };
        let out = dedupe_same_endpoint(vec![
            entry("a", "omlx-embed"),
            entry("only-embed", "omlx-embed"),
            entry("a", "omlx"),
            entry("a", "omlx"),
        ]);
        let got: Vec<_> = out
            .iter()
            .map(|m| (m.id.as_str(), m.owned_by.as_str()))
            .collect();
        assert_eq!(got, [("only-embed", "omlx-embed"), ("a", "omlx")]);
    }

    #[tokio::test]
    async fn data_plane_keeps_best_effort_item_filtering() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "kept"}, {"id": 7}, {"not_id": "ignored"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let response = list_models(
            &reqwest::Client::new(),
            &[card("plain", &server.uri(), Form::HttpService)],
            &HealthTracker::default(),
            false,
        )
        .await
        .unwrap();

        assert_eq!(response.data.len(), 1);
        assert_eq!(response.data[0].id, "kept");
        server.verify().await;
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
                let resp = list_models(&client, &cards, &health, false).await.unwrap();
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
            let resp = list_models(&client, &cards, &health, false).await.unwrap();
            assert!(resp.data.is_empty());
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn loopback_omlx_auth_failure_is_not_hidden_by_cooldown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let mut omlx = card("omlx", &server.uri(), Form::HttpService);
        omlx.version_pin = "omlx@0.6.4".into();
        let health = HealthTracker::default();
        let now = Instant::now();
        for _ in 0..3 {
            health.record("omlx", false, now);
        }
        assert!(health.is_cooling_down("omlx", now));

        assert!(matches!(
            list_models(&client, &[omlx], &health, false).await,
            Err(ModelsError::UpstreamAuthenticationFailed { .. })
        ));
        server.verify().await;
    }

    #[tokio::test]
    async fn a_non_http_service_card_is_skipped_without_any_network_call() {
        let client = reqwest::Client::new();
        let cards = vec![card("subscription", "spawn://x", Form::SpawnCli)];
        let resp = list_models(&client, &cards, &HealthTracker::default(), false)
            .await
            .unwrap();
        assert!(resp.data.is_empty());
    }
}
