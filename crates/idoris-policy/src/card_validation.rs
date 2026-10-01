//! 组件卡交叉策略：移植 TS `contracts/src/validate.ts`，结构校验由契约层负责。

use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::component_card::Egress;
use idoris_contracts::load_policy::{Keepalive, LoadMode};
use idoris_contracts::provider::Locality;

use crate::card::Card;
use crate::registry::RegistrationError;

pub(super) fn validate_card_policy(card: &Card) -> Result<(), RegistrationError> {
    let c = &card.component;
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
