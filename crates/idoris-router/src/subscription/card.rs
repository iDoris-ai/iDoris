use idoris_contracts::ComponentCard;
use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::component_card::Form;
use idoris_policy::SUBSCRIPTION_PROVIDER_ID;

pub const SUBSCRIPTION_ENDPOINT: &str = "spawn://subscription";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardBoundary {
    Ordinary,
    Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionCardError {
    SpawnCliImpersonation,
    WrongForm,
    WrongEndpoint,
    WrongTier,
    WrongPrivacy,
    InvalidCost,
}

impl std::fmt::Display for SubscriptionCardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::SpawnCliImpersonation => {
                "only provider.id=subscription may use spawn_cli in the subscription boundary"
            }
            Self::WrongForm => "subscription provider must use spawn_cli",
            Self::WrongEndpoint => "subscription provider must use spawn://subscription",
            Self::WrongTier => "subscription provider must be tier remote",
            Self::WrongPrivacy => "subscription provider/card privacy must both be any",
            Self::InvalidCost => "subscription provider cost must be finite and exactly zero",
        };
        f.write_str(message)
    }
}

impl std::error::Error for SubscriptionCardError {}

pub fn classify_subscription_card(
    card: &ComponentCard,
) -> Result<CardBoundary, SubscriptionCardError> {
    let is_subscription = card.provider.id == SUBSCRIPTION_PROVIDER_ID;
    if !is_subscription {
        return if card.form == Form::SpawnCli {
            Err(SubscriptionCardError::SpawnCliImpersonation)
        } else {
            Ok(CardBoundary::Ordinary)
        };
    }

    if card.form != Form::SpawnCli {
        return Err(SubscriptionCardError::WrongForm);
    }
    if card.endpoint != SUBSCRIPTION_ENDPOINT {
        return Err(SubscriptionCardError::WrongEndpoint);
    }
    if card.provider.tier != Tier::Remote {
        return Err(SubscriptionCardError::WrongTier);
    }
    if card.provider.privacy_class != PrivacyClass::Any || card.privacy_class != PrivacyClass::Any {
        return Err(SubscriptionCardError::WrongPrivacy);
    }
    let cost = card.provider.cost;
    if !cost.input_per_m.is_finite()
        || !cost.output_per_m.is_finite()
        || cost.input_per_m != 0.0
        || cost.output_per_m != 0.0
    {
        return Err(SubscriptionCardError::InvalidCost);
    }

    Ok(CardBoundary::Subscription)
}
