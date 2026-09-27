//! 注册校验：拒绝重复 / 矛盾 / 危险的组件卡，在它们进入路由决策管道之前。
//! 移植自 `packages/router/src/registry.ts` 的**纯校验**部分——不做文件系统
//! 访问，也不做订阅 provider 的部署模式/沙箱门禁（依赖 `process.env`，是
//! 运行时 IO，不属于本 crate）。

use std::collections::HashSet;

use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::component_card::Form;
use idoris_contracts::provider::Locality;
use url::Url;

use crate::card::Card;
use crate::privacy::is_subscription_provider_id;

/// M6 允许声明 `locality: loopback` 的 endpoint host（`url` 已规范化大小写）。
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "[::1]", "localhost"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    /// 同一个 `provider.id` 出现在两张卡里，直接拒绝启动。
    DuplicateProviderId { id: String },
    /// `locality: loopback` 的 `http_service` 卡，`endpoint` 不是能解析的 URL。
    EndpointUnparseable { id: String, endpoint: String },
    /// endpoint host 不在 loopback 白名单——会让 Served-Locality 谎报。
    LoopbackHostMismatch { id: String, host: String },
    /// `spawn_cli`/订阅类 provider 却声明 `local_only`/`tier: local`：语义矛盾。
    ContradictoryRelayClaim { id: String },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistrationError::DuplicateProviderId { id } => {
                write!(f, "provider.id \"{id}\" 重复声明")
            }
            RegistrationError::EndpointUnparseable { id, endpoint } => {
                write!(
                    f,
                    "组件卡 \"{id}\" 的 endpoint \"{endpoint}\" 不是一个能解析的 URL"
                )
            }
            RegistrationError::LoopbackHostMismatch { id, host } => {
                write!(
                    f,
                    "组件卡 \"{id}\" 声明 locality: loopback，但 endpoint host 是 \"{host}\"，不是 127.0.0.1/::1/localhost"
                )
            }
            RegistrationError::ContradictoryRelayClaim { id } => {
                write!(
                    f,
                    "组件卡 \"{id}\" 语义矛盾：spawn_cli/订阅类 provider 不允许同时声明 privacy_class: local_only 或 tier: local"
                )
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

/// M6：`locality: loopback` 的 `http_service` 卡，endpoint 必须指向允许的
/// loopback host，否则拒绝注册。用真正的 WHATWG URL 解析器（不是手写
/// `split(':')`，后者曾被 userinfo 绕过）。**顺序关键**：先看 scheme 再要求
/// host——`mock:in-memory` 这种没有 host 的合法非 http(s) URL 必须先放行，
/// 不能被误判成 `EndpointUnparseable`。
fn assert_endpoint_locality_consistent(card: &Card) -> Result<(), RegistrationError> {
    let component = &card.component;
    if component.form != Form::HttpService || component.provider.locality != Locality::Loopback {
        return Ok(());
    }
    let unparseable = || RegistrationError::EndpointUnparseable {
        id: card.id().to_string(),
        endpoint: component.endpoint.clone(),
    };
    let Ok(parsed) = Url::parse(&component.endpoint) else {
        return Err(unparseable());
    };
    // 非 http(s) scheme（如 mock://）一律跳过——原样对齐 TS `registry.ts`。
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Ok(());
    }
    let Some(host) = parsed.host_str() else {
        return Err(unparseable());
    };
    if LOOPBACK_HOSTS.contains(&host) {
        return Ok(());
    }
    Err(RegistrationError::LoopbackHostMismatch {
        id: card.id().to_string(),
        host: host.to_string(),
    })
}

/// H1：`spawn_cli`/订阅类 provider 声明 `local_only`/`tier: local` 会被
/// local_only 门禁误放行，一律拒绝。
fn assert_no_contradictory_relay_claim(card: &Card) -> Result<(), RegistrationError> {
    let component = &card.component;
    let is_relay_like = component.form == Form::SpawnCli || is_subscription_provider_id(card.id());
    if !is_relay_like {
        return Ok(());
    }
    if component.privacy_class != PrivacyClass::LocalOnly && component.provider.tier != Tier::Local
    {
        return Ok(());
    }
    Err(RegistrationError::ContradictoryRelayClaim {
        id: card.id().to_string(),
    })
}

/// 从 `registry.ts` `loadComponents` 移植的纯校验部分，遇到违规即拒绝。分
/// 两遍：先查全部候选的重复 id，再逐张查内容——只保证重复 id 永远先于内容
/// 错误被报告；同类错误之间报告哪一个仍取决于候选顺序。
pub fn validate_registration(cards: &[Card]) -> Result<(), RegistrationError> {
    let mut seen = HashSet::new();
    for card in cards {
        if !seen.insert(card.id()) {
            return Err(RegistrationError::DuplicateProviderId {
                id: card.id().to_string(),
            });
        }
    }
    for card in cards {
        assert_endpoint_locality_consistent(card)?;
        assert_no_contradictory_relay_claim(card)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::test_support::sample_card;
    use idoris_contracts::common::Tier;

    #[test]
    fn accepts_a_well_formed_registry() {
        let cards = [sample_card("a", &[]), sample_card("b", &[])];
        assert_eq!(validate_registration(&cards), Ok(()));
    }

    #[test]
    fn rejects_duplicate_provider_id() {
        let cards = [sample_card("dup", &[]), sample_card("dup", &[])];
        assert_eq!(
            validate_registration(&cards),
            Err(RegistrationError::DuplicateProviderId {
                id: "dup".to_string()
            })
        );
    }

    /// 即使排在前面的卡内容也有问题，重复 id 仍然先被报告。
    #[test]
    fn duplicate_id_is_reported_even_when_an_earlier_card_has_a_content_error() {
        let mut broken = sample_card("broken", &[]);
        broken.component.endpoint = "not a url".to_string();
        let cards = [broken, sample_card("dup", &[]), sample_card("dup", &[])];
        assert_eq!(
            validate_registration(&cards),
            Err(RegistrationError::DuplicateProviderId {
                id: "dup".to_string()
            })
        );
    }

    #[test]
    fn accepts_loopback_host_variants() {
        for endpoint in [
            "http://127.0.0.1:8740",
            "http://[::1]:8740",
            "http://localhost:8740",
            "HTTP://LOCALHOST:8740", // scheme/host 大小写不敏感（url crate 规范化）
            "https://127.0.0.1:8443",
        ] {
            let mut card = sample_card("loop", &[]);
            card.component.endpoint = endpoint.to_string();
            assert_eq!(validate_registration(&[card]), Ok(()));
        }
    }

    #[test]
    fn rejects_loopback_claim_with_mismatched_endpoint_host() {
        // 第二条是真实存在过的 fail-open 漏洞：`127.0.0.1:80@example.com` 里
        // `127.0.0.1:80` 只是 userinfo，真正的 host 是 `example.com`。
        for endpoint in ["http://example.com", "http://127.0.0.1:80@example.com/"] {
            let mut card = sample_card("liar", &[]);
            card.component.endpoint = endpoint.to_string();
            assert_eq!(
                validate_registration(&[card]),
                Err(RegistrationError::LoopbackHostMismatch {
                    id: "liar".to_string(),
                    host: "example.com".to_string(),
                })
            );
        }
    }

    #[test]
    fn rejects_unparseable_endpoint_on_a_loopback_claim() {
        let mut card = sample_card("broken", &[]);
        card.component.endpoint = "not a url".to_string();
        assert_eq!(
            validate_registration(&[card]),
            Err(RegistrationError::EndpointUnparseable {
                id: "broken".to_string(),
                endpoint: "not a url".to_string(),
            })
        );
    }

    #[test]
    fn rejects_malformed_endpoints_as_unparseable() {
        for (id, endpoint) in [
            ("bad-port", "http://127.0.0.1:99999"),
            ("no-scheme", "://localhost/path"),
            // IPv6 括号后跟垃圾字符是畸形 URL，不能截断成看似合法的 `[::1]`。
            ("ipv6-garbage", "http://[::1]garbage.example/"),
        ] {
            let mut card = sample_card(id, &[]);
            card.component.endpoint = endpoint.to_string();
            assert_eq!(
                validate_registration(&[card]),
                Err(RegistrationError::EndpointUnparseable {
                    id: id.to_string(),
                    endpoint: endpoint.to_string(),
                })
            );
        }
    }

    #[test]
    fn skips_endpoint_check_for_non_loopback_or_non_http_service_cards() {
        let mut lan = sample_card("lan", &[]);
        lan.component.provider.locality = Locality::Lan;
        lan.component.endpoint = "http://example.com".to_string();
        assert_eq!(validate_registration(std::slice::from_ref(&lan)), Ok(()));

        let mut spawn = sample_card("spawn", &[]);
        spawn.component.form = Form::SpawnCli;
        spawn.component.provider.locality = Locality::Loopback;
        spawn.component.endpoint = "not a url either".to_string();
        spawn.component.privacy_class = PrivacyClass::Any;
        spawn.component.provider.tier = Tier::Remote;
        assert_eq!(validate_registration(&[spawn]), Ok(()));

        // `mock:in-memory` 合法但没有 host（非 http(s) scheme）；曾经的实现
        // 会先无条件要求 host，误判成 EndpointUnparseable。
        for endpoint in ["mock://example.com", "mock:in-memory"] {
            let mut mock = sample_card("mock", &[]);
            mock.component.endpoint = endpoint.to_string();
            assert_eq!(validate_registration(&[mock]), Ok(()));
        }
    }

    #[test]
    fn rejects_spawn_cli_claiming_local_only() {
        let mut card = sample_card("relay", &[]);
        card.component.form = Form::SpawnCli;
        card.component.provider.tier = Tier::Remote;
        card.component.privacy_class = PrivacyClass::LocalOnly;
        assert_eq!(
            validate_registration(&[card]),
            Err(RegistrationError::ContradictoryRelayClaim {
                id: "relay".to_string()
            })
        );
    }

    #[test]
    fn rejects_subscription_id_claiming_tier_local_even_as_http_service() {
        let mut card = sample_card("subscription", &[]);
        card.component.form = Form::HttpService;
        card.component.provider.tier = Tier::Local;
        card.component.privacy_class = PrivacyClass::Any;
        assert_eq!(
            validate_registration(&[card]),
            Err(RegistrationError::ContradictoryRelayClaim {
                id: "subscription".to_string()
            })
        );
    }

    #[test]
    fn accepts_spawn_cli_that_correctly_declares_itself_remote() {
        let mut card = sample_card("relay-ok", &[]);
        card.component.form = Form::SpawnCli;
        card.component.provider.tier = Tier::Remote;
        card.component.privacy_class = PrivacyClass::Any;
        assert_eq!(validate_registration(&[card]), Ok(()));
    }
}
