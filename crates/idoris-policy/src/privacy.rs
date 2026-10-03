//! 隐私判定：Served-Locality 唯一口径（`packages/router/src/locality.ts` 移植）
//! + 隐私下限只能收紧、绝不放宽（接口规范 v1.1 §3.13）。

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::provider::Locality;

use crate::card::Card;

/// 订阅中转 provider 的固定 id（`packages/adapters/subscription/registration.ts`
/// 的 `SUBSCRIPTION_PROVIDER_ID`）。只认 id，不认 `form`。
pub const SUBSCRIPTION_PROVIDER_ID: &str = "subscription";

/// `provider_id` 是否是订阅中转 provider 的固定 id——只认 id，不认 `form`
/// （一张 `form: http_service` 的卡一样会命中，见 [`effective_served_locality`]
/// 文档里 H1 那段真实复现的教训）。
pub fn is_subscription_provider_id(provider_id: &str) -> bool {
    provider_id == SUBSCRIPTION_PROVIDER_ID
}

/// 唯一的「这张卡的推理实际会落在哪」判定（`locality.ts` 的
/// `effectiveServedLocality` 移植）。**路由门禁**、幂等缓存、响应头三处都必须
/// 用同一个函数——三处各算各的曾经是一个真实复现的洞（PR #46 复审 H1）。
///
/// - `spawn_cli` 或订阅类 provider（`is_subscription_provider_id`，只认 id 不认
///   `form`）一律判 [`Locality::Remote`]：它们的推理实际发生在背后的外部
///   CLI/服务里,不需要 form 是 spawn_cli。
/// - 其余情况按 `card.component.provider.locality` 原样返回。
///
/// fail-closed 的「locality 缺失或非法按 remote 处理」在 TS 侧是运行时防御
/// （`locality` 在那边只是弱类型的 `unknown`）；这里 [`Locality`] 是穷尽的强类型
/// 枚举，一张能通过 `Contract::validate` 的 `ComponentCard` 不存在「缺失/非法
/// locality」这个状态——Rust 的类型系统本身就是这条防线，不需要额外的运行时分支。
pub fn effective_served_locality(card: &Card) -> Locality {
    if card.component.form == Form::SpawnCli || is_subscription_provider_id(card.id()) {
        return Locality::Remote;
    }
    card.component.provider.locality
}

/// 可信本地三条件（TS `dispatch.ts:isLocalCapable`）：实际 loopback、卡只承载
/// local_only、出站仅 none/loopback。运行时复核不依赖注册校验已执行。
pub fn is_local_capable(card: &Card) -> bool {
    effective_served_locality(card) == Locality::Loopback
        && card.component.privacy_class == PrivacyClass::LocalOnly
        && card
            .component
            .allowed_egress
            .iter()
            .all(|e| matches!(e, Egress::None | Egress::Loopback))
}

/// 请求声明的隐私下限与「闸一」内容检查收紧结果的合并（v1.1 §3.13）：内容检查
/// 的结果只能让隐私更严格，绝不会让 `local_only` 的下限被放宽——`floor` 已经是
/// `LocalOnly` 时，无论 `content_tightening` 说什么，结果始终是 `LocalOnly`。
pub fn effective_privacy(
    floor: PrivacyClass,
    content_tightening: Option<PrivacyClass>,
) -> PrivacyClass {
    match content_tightening {
        Some(PrivacyClass::LocalOnly) => PrivacyClass::LocalOnly,
        Some(PrivacyClass::Any) | None => floor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::test_support::sample_card;
    use idoris_contracts::component_card::Form;

    #[test]
    fn local_capable_accepts_trusted_none_or_loopback_egress() {
        let mut card = sample_card("local", &[]);
        for egress in [Egress::None, Egress::Loopback] {
            card.component.allowed_egress = vec![egress];
            assert!(is_local_capable(&card));
        }
    }

    #[test]
    fn local_capable_rejects_privacy_egress_and_effective_locality_violations() {
        let mut card = sample_card("local", &[]);
        card.component.privacy_class = PrivacyClass::Any;
        assert!(!is_local_capable(&card));
        card.component.privacy_class = PrivacyClass::LocalOnly;
        for egress in [Egress::Lan, Egress::Internet] {
            for allowed in [vec![egress], vec![Egress::Loopback, egress]] {
                card.component.allowed_egress = allowed;
                assert!(!is_local_capable(&card));
            }
        }
        card.component.allowed_egress = vec![Egress::None];
        for locality in [Locality::Lan, Locality::Remote] {
            card.component.provider.locality = locality;
            assert!(!is_local_capable(&card));
        }
        card.component.provider.locality = Locality::Loopback;
        card.component.form = Form::SpawnCli;
        assert!(!is_local_capable(&card));
        card.component.form = Form::HttpService;
        card.component.provider.id = SUBSCRIPTION_PROVIDER_ID.to_string();
        assert!(!is_local_capable(&card));
    }

    #[test]
    fn effective_served_locality_passes_through_normal_http_service_cards() {
        let mut card = sample_card("local-1", &[]);
        card.component.provider.locality = Locality::Loopback;
        assert_eq!(effective_served_locality(&card), Locality::Loopback);
        card.component.provider.locality = Locality::Lan;
        assert_eq!(effective_served_locality(&card), Locality::Lan);
        card.component.provider.locality = Locality::Remote;
        assert_eq!(effective_served_locality(&card), Locality::Remote);
    }

    #[test]
    fn effective_served_locality_treats_spawn_cli_as_remote_even_if_it_claims_loopback() {
        let mut card = sample_card("relay-1", &[]);
        card.component.form = Form::SpawnCli;
        card.component.provider.locality = Locality::Loopback;
        assert_eq!(effective_served_locality(&card), Locality::Remote);
    }

    #[test]
    fn effective_served_locality_treats_subscription_id_as_remote_regardless_of_form() {
        let mut card = sample_card("subscription", &[]);
        card.component.form = Form::HttpService; // 不需要是 spawn_cli 也会触发
        card.component.provider.locality = Locality::Loopback;
        assert_eq!(effective_served_locality(&card), Locality::Remote);
    }

    #[test]
    fn effective_privacy_never_loosens_a_local_only_floor() {
        assert_eq!(
            effective_privacy(PrivacyClass::LocalOnly, Some(PrivacyClass::Any)),
            PrivacyClass::LocalOnly
        );
        assert_eq!(
            effective_privacy(PrivacyClass::LocalOnly, None),
            PrivacyClass::LocalOnly
        );
    }

    #[test]
    fn effective_privacy_tightens_an_any_floor_when_content_check_says_so() {
        assert_eq!(
            effective_privacy(PrivacyClass::Any, Some(PrivacyClass::LocalOnly)),
            PrivacyClass::LocalOnly
        );
        assert_eq!(
            effective_privacy(PrivacyClass::Any, Some(PrivacyClass::Any)),
            PrivacyClass::Any
        );
        assert_eq!(
            effective_privacy(PrivacyClass::Any, None),
            PrivacyClass::Any
        );
    }
}
