//! 组件卡交叉策略：移植 TS `contracts/src/validate.ts`，结构校验由契约层负责。

use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::component_card::Egress;
use idoris_contracts::load_policy::{Keepalive, LoadMode};
use idoris_contracts::provider::Locality;

use crate::card::Card;
use crate::registry::RegistrationError;

pub(super) fn validate_card_policy(card: &Card) -> Result<(), RegistrationError> {
    let c = &card.component;
    // 非空原生扩展必须在自身层级声明降级方式，不能借用另一层的声明。
    for extensions in [&c.extensions, &c.provider.extensions]
        .into_iter()
        .flatten()
    {
        if !extensions.is_empty()
            && !extensions
                .get("_degradation")
                .and_then(|value| value.as_str())
                .is_some_and(has_js_non_whitespace)
        {
            return Err(RegistrationError::CardPolicyViolation {
                id: card.id().to_string(),
                rule: "EXTENSIONS_MISSING_DEGRADATION",
            });
        }
    }
    let local_only = c.privacy_class == PrivacyClass::LocalOnly;
    let local_tier = c.provider.tier == Tier::Local;
    let mut rules = vec![
        (
            local_only && !c.fail_closed,
            "LOCAL_ONLY_REQUIRES_FAIL_CLOSED",
        ),
        (
            c.provider.privacy_class == PrivacyClass::LocalOnly && !local_only,
            "PRIVACY_CLASS_MISMATCH",
        ),
        (
            local_tier && c.provider.locality == Locality::Remote,
            "LOCAL_TIER_CANNOT_BE_REMOTE_LOCALITY",
        ),
        (
            local_tier && c.load_policy.is_none(),
            "LOCAL_TIER_REQUIRES_LOAD_POLICY",
        ),
    ];
    if let Some(policy) = c.load_policy {
        rules.push((
            (policy.mode == LoadMode::Resident)
                != matches!(policy.keepalive, Keepalive::Pinned { pinned: true }),
            "LOAD_POLICY_MODE_KEEPALIVE_MISMATCH",
        ));
    }
    rules.extend([
        (
            c.allowed_egress.contains(&Egress::None) && c.allowed_egress.len() != 1,
            "EGRESS_NONE_MUST_BE_EXCLUSIVE",
        ),
        (
            local_only && c.allowed_egress.contains(&Egress::Internet),
            "LOCAL_ONLY_CANNOT_ALLOW_INTERNET",
        ),
        (
            local_only
                && c.allowed_egress
                    .iter()
                    .any(|e| !matches!(e, Egress::None | Egress::Loopback)),
            "LOCAL_ONLY_EGRESS_MUST_BE_LOOPBACK",
        ),
    ]);
    for (violated, rule) in rules {
        if violated {
            return Err(RegistrationError::CardPolicyViolation {
                id: card.id().to_string(),
                rule,
            });
        }
    }
    Ok(())
}

/// 对齐 TS String.trim：Unicode 空白不含 NEL，另包含 BOM。
fn has_js_non_whitespace(value: &str) -> bool {
    !value
        .trim_matches(|c: char| (c.is_whitespace() && c != '\u{0085}') || c == '\u{feff}')
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::test_support::sample_card;
    use crate::registry::validate_registration;

    #[test]
    fn accepts_absent_and_empty_extensions_at_both_levels() {
        let mut card = sample_card("extensions", &[]);
        assert_eq!(validate_registration(&[card.clone()]), Ok(()));
        card.component.extensions = Some(Default::default());
        card.component.provider.extensions = Some(Default::default());
        assert_eq!(validate_registration(&[card]), Ok(()));
    }

    #[test]
    fn requires_each_extensions_level_to_declare_its_own_degradation() {
        for provider in [false, true] {
            for (value, accepted) in [
                ("fallback".into(), true),
                (" 降级 ".into(), true),
                ("\u{0085}".into(), true),
                ("".into(), false),
                (" \t\r\n\u{00a0}\u{3000}\u{feff}".into(), false),
                (false.into(), false),
                (7.into(), false),
                (None::<String>.into(), false),
            ] {
                let mut card = sample_card("extensions", &[]);
                let valid = Some(
                    [("_degradation".into(), "fallback".into())]
                        .into_iter()
                        .collect(),
                );
                card.component.extensions = valid.clone();
                card.component.provider.extensions = valid;
                let native = Some([("native".into(), true.into())].into_iter().collect());
                if provider {
                    card.component.provider.extensions = native;
                } else {
                    card.component.extensions = native;
                }
                assert_eq!(
                    validate_registration(&[card.clone()]),
                    Err(missing_degradation())
                );
                let target = if provider {
                    &mut card.component.provider.extensions
                } else {
                    &mut card.component.extensions
                };
                *target = Some(
                    [
                        ("native".into(), true.into()),
                        ("_degradation".into(), value),
                    ]
                    .into_iter()
                    .collect(),
                );
                let expected = if accepted {
                    Ok(())
                } else {
                    Err(missing_degradation())
                };
                assert_eq!(validate_registration(&[card]), expected);
            }
        }
    }

    fn missing_degradation() -> RegistrationError {
        RegistrationError::CardPolicyViolation {
            id: "extensions".into(),
            rule: "EXTENSIONS_MISSING_DEGRADATION",
        }
    }
}
