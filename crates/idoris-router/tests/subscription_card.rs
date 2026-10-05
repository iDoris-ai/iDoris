#![allow(clippy::unwrap_used)]

use idoris_contracts::ComponentCard;
use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::component_card::Form;
use idoris_router::subscription::card::{
    CardBoundary, SubscriptionCardError, classify_subscription_card,
};

fn subscription_card() -> ComponentCard {
    serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap()
}

#[test]
fn shipped_subscription_card_is_the_only_valid_subscription_identity_shape() {
    assert_eq!(
        classify_subscription_card(&subscription_card()),
        Ok(CardBoundary::Subscription)
    );

    let mut wrong_form = subscription_card();
    wrong_form.form = Form::HttpService;
    assert_eq!(
        classify_subscription_card(&wrong_form),
        Err(SubscriptionCardError::WrongForm)
    );

    let mut wrong_endpoint = subscription_card();
    wrong_endpoint.endpoint = "spawn://other".into();
    assert_eq!(
        classify_subscription_card(&wrong_endpoint),
        Err(SubscriptionCardError::WrongEndpoint)
    );

    let mut impostor = subscription_card();
    impostor.provider.id = "not-subscription".into();
    assert_eq!(
        classify_subscription_card(&impostor),
        Err(SubscriptionCardError::SpawnCliImpersonation)
    );
}

#[test]
fn subscription_cannot_claim_local_or_local_only() {
    let mut local_tier = subscription_card();
    local_tier.provider.tier = Tier::Local;
    assert_eq!(
        classify_subscription_card(&local_tier),
        Err(SubscriptionCardError::WrongTier)
    );

    for provider_level in [false, true] {
        let mut card = subscription_card();
        if provider_level {
            card.provider.privacy_class = PrivacyClass::LocalOnly;
        } else {
            card.privacy_class = PrivacyClass::LocalOnly;
        }
        assert_eq!(
            classify_subscription_card(&card),
            Err(SubscriptionCardError::WrongPrivacy)
        );
    }
}

#[test]
fn subscription_cost_must_be_known_zero_and_ordinary_http_stays_ordinary() {
    for (input, output) in [
        (1.0, 0.0),
        (0.0, 1.0),
        (f64::NAN, 0.0),
        (0.0, f64::INFINITY),
    ] {
        let mut card = subscription_card();
        card.provider.cost.input_per_m = input;
        card.provider.cost.output_per_m = output;
        assert_eq!(
            classify_subscription_card(&card),
            Err(SubscriptionCardError::InvalidCost)
        );
    }

    let ordinary: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    assert_eq!(
        classify_subscription_card(&ordinary),
        Ok(CardBoundary::Ordinary)
    );
}
