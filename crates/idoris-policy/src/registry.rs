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

#[path = "card_validation.rs"]
mod card_validation;

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
    /// `locality: loopback` 的网络端点卡用了不在白名单里的 scheme——不能靠
    /// "反正不是 http(s)" 就整体放行，那等于放行 `ftp://`/`file://` 之类
    /// 可能真的指向外部的地址。
    UnsupportedScheme { id: String, scheme: String },
    /// `spawn_cli`/订阅类 provider 却声明 `local_only`/`tier: local`：语义矛盾。
    ContradictoryRelayClaim { id: String },
    /// 组件卡通过结构校验，但违反 TS 参考实现的交叉策略规则。
    CardPolicyViolation { id: String, rule: &'static str },
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
            RegistrationError::UnsupportedScheme { id, scheme } => {
                write!(
                    f,
                    "组件卡 \"{id}\" 的 endpoint scheme \"{scheme}\" 不在白名单（http/https/ws/wss）"
                )
            }
            RegistrationError::ContradictoryRelayClaim { id } => {
                write!(
                    f,
                    "组件卡 \"{id}\" 语义矛盾：spawn_cli/订阅类 provider 不允许同时声明 privacy_class: local_only 或 tier: local"
                )
            }
            RegistrationError::CardPolicyViolation { id, rule } => {
                write!(f, "组件卡 \"{id}\" 违反策略规则：{rule}")
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

/// 声明了真实网络端点的 form——`endpoint` 是一个网络地址，需要做 locality
/// 一致性校验。`spawn_cli`/`bundled_binary`/`batch_job` 的 `endpoint` 是
/// argv 模板/二进制路径/任务定义，不是网络地址，不在这个集合里。
fn declares_network_endpoint(form: Form) -> bool {
    matches!(form, Form::HttpService | Form::NostrNode | Form::MitmProxy)
}

/// "内存 mock 后端"scheme——纯进程内计算，`locality: loopback` 本身没有
/// 说谎，允许跳过 host 校验。**A-1 更正**：`config/components/mock.yaml`
/// 这份真实配置就是 `http_service + loopback + mock://in-memory`（R2-D 接线
/// 会用到），不是"真实 yaml 永远不会出现 mock://"——之前那句注释是错的。
/// 用 `cfg(test)` 放行会导致这份真实 yaml 在非测试构建里被 `UnsupportedScheme`
/// 拒掉；改用显式的 `dev-mock` cargo feature（默认关闭）控制，本单测仍然
/// 需要放行（`cfg(test)` 保留，方便本 crate 自身的注册校验测试），生产构建
/// 必须两者都不开，才会一律按未知 scheme 拒绝。是否放行跟"是不是在跑测试"
/// 无关，跟"有没有显式打开 mock 开关"有关。
#[cfg(any(test, feature = "dev-mock"))]
fn is_dev_mock_scheme_allowed(scheme: &str) -> bool {
    scheme == "mock"
}
#[cfg(not(any(test, feature = "dev-mock")))]
fn is_dev_mock_scheme_allowed(_scheme: &str) -> bool {
    false
}

/// M6 + H1：`locality: loopback` 的网络端点卡（见 [`declares_network_endpoint`]），
/// endpoint scheme 必须在白名单里（`http`/`https`/`ws`/`wss`），host 必须在
/// [`LOOPBACK_HOSTS`] 里，否则拒绝注册（fail-closed）。用真正的 WHATWG URL
/// 解析器（不是手写 `split(':')`，后者曾被 userinfo 绕过）。**顺序关键**：
/// 先看 scheme 再要求 host——`mock:in-memory` 这种没有 host 的合法非
/// http(s) URL 必须先放行，不能被误判成 `EndpointUnparseable`。
///
/// 之前的实现对"非 http(s) scheme"一律跳过校验，等价于放行任意
/// `ws://evil.example`/`file://…`/`ftp://…` 的 loopback 声明——不能只挡
/// http(s)，白名单之外的 scheme 必须显式拒绝，不能靠"反正我们只认识
/// http(s)"这种默认放行的逻辑蒙混过去。
fn assert_endpoint_locality_consistent(card: &Card) -> Result<(), RegistrationError> {
    parse_loopback_endpoint_url(card).map(|_| ())
}

/// L1：注册校验和"执行层"（真正拿这张卡去发起连接的调用方）必须共用**同一个**
/// 解析结果，不能各自 `Url::parse` 一遍——两处分别解析曾经在 H1 里就是隐患的
/// 根源（手写解析和这里用的 WHATWG 解析器行为不一致）。这是唯一允许对
/// `component.endpoint` 调用 `Url::parse` 的地方；执行层需要这张卡的
/// endpoint URL 时应该调用这个函数复用校验阶段的解析/判定逻辑，而不是自己
/// 再解析一次字符串。
///
/// 返回值：`Ok(None)` 表示这张卡不需要（也没有）做 loopback URL 校验（非网络
/// 端点 form、非 loopback、或测试专用 `mock` scheme）；`Ok(Some(url))` 是校验
/// 通过后解析出的 URL；`Err` 是注册校验失败。
pub fn parse_loopback_endpoint_url(card: &Card) -> Result<Option<Url>, RegistrationError> {
    let component = &card.component;
    if !declares_network_endpoint(component.form)
        || component.provider.locality != Locality::Loopback
    {
        return Ok(None);
    }
    let unparseable = || RegistrationError::EndpointUnparseable {
        id: card.id().to_string(),
        endpoint: component.endpoint.clone(),
    };
    let Ok(parsed) = Url::parse(&component.endpoint) else {
        return Err(unparseable());
    };
    let scheme = parsed.scheme();
    if is_dev_mock_scheme_allowed(scheme) {
        return Ok(None);
    }
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return Err(RegistrationError::UnsupportedScheme {
            id: card.id().to_string(),
            scheme: scheme.to_string(),
        });
    }
    let Some(host) = parsed.host_str() else {
        return Err(unparseable());
    };
    if !LOOPBACK_HOSTS.contains(&host) {
        return Err(RegistrationError::LoopbackHostMismatch {
            id: card.id().to_string(),
            host: host.to_string(),
        });
    }
    Ok(Some(parsed))
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
        card_validation::validate_card_policy(card)?;
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

    /// L1：`parse_loopback_endpoint_url` 是唯一允许解析 `endpoint` 的地方，
    /// "执行层"要复用这个解析结果，不能自己再 `Url::parse` 一遍——这里验证
    /// 它确实把校验通过后的 `Url` 原样交还给调用方（host/scheme 都对得上）。
    #[test]
    fn parse_loopback_endpoint_url_returns_the_parsed_url_on_success() {
        let card = sample_card("loopback-1", &[]);
        match parse_loopback_endpoint_url(&card) {
            Ok(Some(parsed)) => {
                assert_eq!(parsed.scheme(), "http");
                assert_eq!(parsed.host_str(), Some("127.0.0.1"));
            }
            other => panic!("expected Ok(Some(url)), got {other:?}"),
        }

        let mut lan = sample_card("lan-1", &[]);
        lan.component.provider.locality = Locality::Lan;
        assert_eq!(parse_loopback_endpoint_url(&lan), Ok(None));
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

    /// H1 回归：非白名单 scheme（`ftp://`/`file://`）不能靠"反正不是
    /// http(s)"整体跳过校验就放行——之前的实现会把这些都当成"跳过"。
    #[test]
    fn rejects_endpoints_with_schemes_outside_the_allowlist() {
        for (id, endpoint) in [
            ("ftp-evil", "ftp://evil.example/"),
            ("file-evil", "file:///etc/passwd"),
        ] {
            let mut card = sample_card(id, &[]);
            card.component.endpoint = endpoint.to_string();
            let scheme = endpoint.split(':').next().unwrap_or_default().to_string();
            assert_eq!(
                validate_registration(&[card]),
                Err(RegistrationError::UnsupportedScheme {
                    id: id.to_string(),
                    scheme,
                })
            );
        }
    }

    /// H1 + M3：`ws`/`wss` 在白名单里，且 `NostrNode`/`MitmProxy` 这两个
    /// "声明了真实网络端点"的 form 也要做 locality 校验，不能只校验
    /// `HttpService`。
    #[test]
    fn ws_scheme_and_non_http_service_network_forms_are_validated() {
        let mut ws_loopback = sample_card("ws-ok", &[]);
        ws_loopback.component.endpoint = "ws://127.0.0.1:8740".to_string();
        assert_eq!(validate_registration(&[ws_loopback]), Ok(()));

        let mut nostr_evil = sample_card("nostr-evil", &[]);
        nostr_evil.component.form = Form::NostrNode;
        nostr_evil.component.endpoint = "wss://evil.example/relay".to_string();
        assert_eq!(
            validate_registration(&[nostr_evil]),
            Err(RegistrationError::LoopbackHostMismatch {
                id: "nostr-evil".to_string(),
                host: "evil.example".to_string(),
            })
        );

        let mut proxy_evil = sample_card("proxy-evil", &[]);
        proxy_evil.component.form = Form::MitmProxy;
        proxy_evil.component.endpoint = "ftp://evil.example/".to_string();
        assert_eq!(
            validate_registration(&[proxy_evil]),
            Err(RegistrationError::UnsupportedScheme {
                id: "proxy-evil".to_string(),
                scheme: "ftp".to_string(),
            })
        );
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
        spawn.component.provider.privacy_class = PrivacyClass::Any;
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
        card.component.provider.privacy_class = PrivacyClass::Any;
        assert_eq!(validate_registration(&[card]), Ok(()));
    }
}
