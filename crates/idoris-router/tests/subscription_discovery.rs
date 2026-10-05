#![allow(clippy::unwrap_used)]

use idoris_router::health::HealthTracker;
use idoris_router::models;
use idoris_router::subscription::config::{SANDBOX_PROFILE_ID, SubscriptionConfig};
use idoris_router::subscription::runtime::{
    SubscriptionRuntimeHandle, SubscriptionRuntimeRegistry, authorize_subscription,
};

fn card() -> idoris_contracts::ComponentCard {
    serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap()
}

fn config(enable: bool, disable: bool, cli: &str) -> SubscriptionConfig {
    SubscriptionConfig::snapshot(
        Some("personal"),
        enable.then_some("1"),
        disable.then_some("1"),
        Some(SANDBOX_PROFILE_ID),
        Some(cli),
    )
}

#[tokio::test]
async fn enabled_runtime_is_listed_statically_without_spawning() {
    let card = card();
    let authorized = authorize_subscription(&config(true, false, "codex"), &card)
        .unwrap()
        .unwrap();
    let handle = SubscriptionRuntimeHandle::build(authorized, &card).unwrap();
    let mut subscriptions = SubscriptionRuntimeRegistry::default();
    subscriptions.insert(handle).unwrap();

    assert_eq!(subscriptions.len(), 1);
    assert_eq!(
        subscriptions.discovery(),
        [
            idoris_router::subscription::runtime::SubscriptionDiscovery {
                provider_id: "subscription".into(),
                model_id: "codex-subscription".into(),
            }
        ]
    );

    let response = models::list_models_with_subscriptions(
        &reqwest::Client::new(),
        &[],
        &HealthTracker::default(),
        &subscriptions,
    )
    .await
    .unwrap();
    assert_eq!(response.data.len(), 1);
    assert_eq!(response.data[0].id, "codex-subscription");
    assert_eq!(response.data[0].owned_by, "subscription");
}

#[tokio::test]
async fn disabled_or_default_off_runtime_has_no_discovery_entry() {
    let card = card();
    for cfg in [config(false, false, "claude"), config(true, true, "claude")] {
        let subscriptions = SubscriptionRuntimeRegistry::default();
        assert!(authorize_subscription(&cfg, &card).unwrap().is_none());
        assert!(subscriptions.is_empty());
        assert_eq!(subscriptions.len(), 0);
        let response = models::list_models_with_subscriptions(
            &reqwest::Client::new(),
            &[],
            &HealthTracker::default(),
            &subscriptions,
        )
        .await
        .unwrap();
        assert!(response.data.is_empty());
    }
}

#[tokio::test]
async fn subscription_discovery_does_not_fabricate_local_capability_rows() {
    let card = card();
    let authorized = authorize_subscription(&config(true, false, "claude"), &card)
        .unwrap()
        .unwrap();
    let handle = SubscriptionRuntimeHandle::build(authorized, &card).unwrap();
    let mut subscriptions = SubscriptionRuntimeRegistry::default();
    subscriptions.insert(handle).unwrap();

    assert_eq!(subscriptions.len(), 1);
    // B1 /capabilities remains recommender/runtime-backed. Subscription
    // discovery only exposes identity; it owns no local resident or memory
    // measurement to feed into that surface.
    assert_eq!(subscriptions.discovery()[0].model_id, "claude-subscription");
}
