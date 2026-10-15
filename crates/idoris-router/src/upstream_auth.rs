//! Startup resolution of optional component-card and runtime-launch bearers.
//! The secret is read from the named environment variable once, then kept off
//! the card. A declared name with no usable value refuses startup.

use std::collections::BTreeMap;
use std::sync::Arc;

use idoris_contracts::{ComponentAuth, ComponentCard};
use idoris_upstream::{LocalHttpRuntimeConfig, UpstreamBearer};

use crate::proxy::ChatProxy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvLookup {
    Missing,
    NotUnicode,
    Value(String),
}

pub fn read_process_env(name: &str) -> EnvLookup {
    match std::env::var(name) {
        Ok(value) => EnvLookup::Value(value),
        Err(std::env::VarError::NotPresent) => EnvLookup::Missing,
        Err(std::env::VarError::NotUnicode(_)) => EnvLookup::NotUnicode,
    }
}

pub fn resolve_bearer(
    auth: &ComponentAuth,
    lookup: &impl Fn(&str) -> EnvLookup,
) -> Result<UpstreamBearer, String> {
    auth.validate_name().map_err(|error| error.to_string())?;
    let name = auth.env_var();
    let value = match lookup(name) {
        EnvLookup::Missing => {
            return Err(format!("环境变量 {name} 未设置，拒绝启动"));
        }
        EnvLookup::NotUnicode => {
            return Err(format!("环境变量 {name} 不是 Unicode，拒绝启动"));
        }
        EnvLookup::Value(value) => value,
    };
    if value.is_empty() {
        return Err(format!("环境变量 {name} 为空，拒绝启动"));
    }
    UpstreamBearer::from_secret(&value)
        .map_err(|error| format!("环境变量 {name} 不能作为 Bearer 凭证：{error}"))
}

pub fn prepare_upstream_auth(
    cards: &[ComponentCard],
    configs: &mut BTreeMap<String, LocalHttpRuntimeConfig>,
    lookup: &impl Fn(&str) -> EnvLookup,
) -> Result<BTreeMap<String, UpstreamBearer>, String> {
    let mut by_provider = BTreeMap::new();
    for card in cards {
        let Some(auth) = &card.auth else {
            continue;
        };
        let bearer = resolve_bearer(auth, lookup)?;
        let provider_id = card.provider.id.as_str();
        if let Some(config) = configs.get_mut(provider_id) {
            match &config.bearer {
                Some(existing) if existing != &bearer => {
                    return Err(format!(
                        "本地 runtime {provider_id} 的 auth 与组件卡 auth 解析结果不一致，拒绝启动"
                    ));
                }
                Some(_) => {}
                None => config.bearer = Some(bearer.clone()),
            }
        } else if !crate::dispatch::is_resident_http_service(card) {
            return Err(format!(
                "组件卡 {provider_id} 声明了 auth，但当前启动路径不会把该凭证发给上游，拒绝启动"
            ));
        }
        by_provider.insert(provider_id.to_string(), bearer);
    }

    let mut by_endpoint: BTreeMap<String, Option<UpstreamBearer>> = BTreeMap::new();
    for card in cards {
        if !crate::dispatch::is_resident_http_service(card) {
            continue;
        }
        let declared = by_provider.get(&card.provider.id).cloned();
        if let Some(existing) = by_endpoint.get(&card.endpoint) {
            let same = match (existing, &declared) {
                (None, None) => true,
                (Some(left), Some(right)) => left == right,
                _ => false,
            };
            if !same {
                return Err(format!(
                    "组件卡 {} 与另一张卡共用 endpoint 但 Bearer 声明不一致，拒绝启动",
                    card.provider.id
                ));
            }
        } else {
            by_endpoint.insert(card.endpoint.clone(), declared);
        }
    }
    Ok(by_endpoint
        .into_iter()
        .filter_map(|(endpoint, bearer)| bearer.map(|bearer| (endpoint, bearer)))
        .collect())
}

pub fn install_upstream_bearers(
    state: &mut crate::AppState,
    bearers: BTreeMap<String, UpstreamBearer>,
) {
    let client = state.http_client.clone();
    state.proxy = Arc::new(ChatProxy::new(client).with_upstream_bearers(bearers));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
    use idoris_upstream::{LocalRuntimeKind, LocalRuntimeLaunch};

    fn bearer_auth(env: &str) -> ComponentAuth {
        ComponentAuth::Bearer {
            env: env.to_string(),
        }
    }

    fn card(id: &str, resident: bool) -> ComponentCard {
        let mut card: ComponentCard = serde_json::from_value(serde_json::json!({
            "provider": {
                "id": id, "family": "local", "tier": "local", "capabilities": ["chat"],
                "privacy_class": "local_only", "cost": {"input_per_m": 0, "output_per_m": 0},
                "locality": "loopback"
            },
            "form": "http_service",
            "endpoint": "http://127.0.0.1:9",
            "version_pin": "test",
            "privacy_class": "local_only",
            "allowed_egress": ["loopback"],
            "fallback_policy": "fail_closed",
            "fail_closed": true
        }))
        .unwrap();
        if resident {
            card.load_policy = Some(LoadPolicy {
                mode: LoadMode::Resident,
                keepalive: Keepalive::Pinned { pinned: true },
                admission: Admission::Coexist,
            });
        }
        card
    }

    fn lookup(values: &[(&str, &str)]) -> impl Fn(&str) -> EnvLookup {
        let values: BTreeMap<String, String> = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name: &str| match values.get(name) {
            Some(value) => EnvLookup::Value(value.clone()),
            None => EnvLookup::Missing,
        }
    }

    fn local_config(port: u16) -> LocalHttpRuntimeConfig {
        let launch = LocalRuntimeLaunch::new(
            LocalRuntimeKind::MlxLmServer,
            "/usr/bin/false",
            "/models/qwen-mlx",
            port,
        )
        .unwrap();
        LocalHttpRuntimeConfig::new(launch, "model", 1.0, std::env::temp_dir().join("fence"))
            .unwrap()
    }

    #[test]
    fn missing_env_refuses_startup_and_the_error_has_no_secret() {
        let mut card = card("resident", true);
        card.auth = Some(bearer_auth("IDORIS_UPSTREAM_TOKEN_FOR_TEST"));
        let error = prepare_upstream_auth(&[card], &mut BTreeMap::new(), &lookup(&[])).unwrap_err();
        assert!(error.contains("IDORIS_UPSTREAM_TOKEN_FOR_TEST"));
        assert!(error.contains("未设置"));
        assert!(!error.contains("Bearer "));
    }

    #[test]
    fn empty_env_refuses_startup() {
        let mut card = card("resident", true);
        card.auth = Some(bearer_auth("IDORIS_UPSTREAM_TOKEN_FOR_TEST"));
        let error = prepare_upstream_auth(
            &[card],
            &mut BTreeMap::new(),
            &lookup(&[("IDORIS_UPSTREAM_TOKEN_FOR_TEST", "")]),
        )
        .unwrap_err();
        assert!(error.contains("为空"));
    }

    #[test]
    fn auth_on_a_non_resident_card_without_a_local_runtime_refuses_startup() {
        let mut card = card("omlx", false);
        card.auth = Some(bearer_auth("IDORIS_UPSTREAM_TOKEN_FOR_TEST"));
        let error = prepare_upstream_auth(
            &[card],
            &mut BTreeMap::new(),
            &lookup(&[("IDORIS_UPSTREAM_TOKEN_FOR_TEST", "token-value")]),
        )
        .unwrap_err();
        assert!(error.contains("不会把该凭证发给上游"));
        assert!(!error.contains("token-value"));
    }

    #[test]
    fn card_auth_is_copied_onto_the_local_runtime_and_not_logged() {
        let mut card = card("local-http", false);
        card.endpoint = "http://127.0.0.1:18111".into();
        card.auth = Some(bearer_auth("IDORIS_UPSTREAM_TOKEN_FOR_TEST"));
        let mut configs = BTreeMap::from([("local-http".to_string(), local_config(18111))]);
        let endpoints = prepare_upstream_auth(
            &[card],
            &mut configs,
            &lookup(&[("IDORIS_UPSTREAM_TOKEN_FOR_TEST", "copied-token")]),
        )
        .unwrap();
        assert!(endpoints.is_empty());
        assert_eq!(
            configs["local-http"]
                .bearer
                .as_ref()
                .unwrap()
                .authorization(),
            "Bearer copied-token"
        );
        let rendered = format!("{:?}", configs["local-http"].bearer);
        assert!(!rendered.contains("copied-token"));
    }

    #[test]
    fn conflicting_card_and_launch_bearers_refuse_without_printing_either_secret() {
        let mut card = card("local-http", false);
        card.endpoint = "http://127.0.0.1:18111".into();
        card.auth = Some(bearer_auth("IDORIS_CARD_TOKEN"));
        let mut config = local_config(18111);
        config.bearer = Some(UpstreamBearer::from_secret("launch-secret").unwrap());
        let mut configs = BTreeMap::from([("local-http".to_string(), config)]);
        let error = prepare_upstream_auth(
            &[card],
            &mut configs,
            &lookup(&[("IDORIS_CARD_TOKEN", "card-secret")]),
        )
        .unwrap_err();
        assert!(error.contains("不一致"));
        assert!(!error.contains("launch-secret"));
        assert!(!error.contains("card-secret"));
    }

    #[test]
    fn resident_cards_that_share_an_endpoint_must_agree_on_auth() {
        let mut authed = card("daily", true);
        authed.endpoint = "http://127.0.0.1:9".into();
        authed.auth = Some(bearer_auth("IDORIS_SHARED_ENDPOINT_TOKEN"));
        let plain = card("nightly", true);
        let error = prepare_upstream_auth(
            &[authed, plain],
            &mut BTreeMap::new(),
            &lookup(&[("IDORIS_SHARED_ENDPOINT_TOKEN", "shared-token")]),
        )
        .unwrap_err();
        assert!(error.contains("共用 endpoint"));
        assert!(!error.contains("shared-token"));
    }
}
