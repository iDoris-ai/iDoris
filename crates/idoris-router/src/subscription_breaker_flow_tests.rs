#![cfg(unix)]
#![allow(clippy::unwrap_used)]

//! Black-box breaker flow over a real faulting fixture process.
//!
//! The oracle is deliberately external: how many times the fixture was spawned
//! and what the client saw. Nothing here reads private breaker state, so the
//! test cannot pass by agreeing with the implementation's own bookkeeping.

use super::*;
use axum::extract::ConnectInfo;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_contracts::common::Tier;
use idoris_upstream::subscription::profile::{SandboxProfile, SubscriptionCli};
use idoris_upstream::subscription::relay::SubscriptionRelayConfig;
use std::fs;
use std::os::unix::fs::symlink;
use tower::ServiceExt;

fn spawn_count(dir: &tempfile::TempDir) -> usize {
    fs::read_to_string(dir.path().join("calls"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

#[tokio::test]
async fn real_fixture_faults_open_the_subscription_breaker_and_stop_the_fourth_spawn() {
    let dir = tempfile::TempDir::new().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/subscription_oauth_fault.sh");
    symlink(&fixture, dir.path().join("claude")).unwrap();
    let card: idoris_contracts::ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap();
    let config = subscription::config::SubscriptionConfig::snapshot(
        Some("personal"),
        Some("1"),
        None,
        Some(subscription::config::SANDBOX_PROFILE_ID),
        Some("claude"),
    );
    let authorized = subscription::runtime::authorize_subscription(&config, &card)
        .unwrap()
        .unwrap();
    let relay = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        [("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))],
    );
    let handle = subscription::runtime::SubscriptionRuntimeHandle::build_with_relay_config(
        authorized, &card, relay,
    )
    .unwrap();
    let mut registry = subscription::runtime::SubscriptionRuntimeRegistry::default();
    registry.insert(handle).unwrap();
    let proxy = Arc::new(proxy::ChatProxy::new(
        idoris_upstream::http_client().unwrap(),
    ));
    proxy.set_breakers_enabled(true);
    let mut state = AppState {
        cards: vec![card.clone()],
        subscriptions: registry.clone(),
        proxy: proxy.clone(),
        ..AppState::default()
    };
    // Explicit test policy opts into the relay; the production default stays local.
    state.routing_policy.routing_policy.rules.clear();
    state.routing_policy.routing_policy.default.tiers = Some(vec![Tier::Remote]);
    let app = build_app(state);
    let request = || {
        let mut request = super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"fixture only"}]}"#,
            &[("x-idoris-privacy", "any")],
        );
        request
            .extensions_mut()
            .insert(ConnectInfo(connection::ConnectionInfo::from_parts(
                "127.0.0.1:1234".parse().unwrap(),
                tokio_util::sync::CancellationToken::new(),
            )));
        request
    };
    // Each of the first three attempts really reaches the CLI and fails there.
    for count in 1..=3 {
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["reason_code"], "RELAY_CLI_FAILED");
        assert_eq!(spawn_count(&dir), count);
    }
    // The breaker is now open: the fourth request is refused before any spawn,
    // so the fixture is still at three attempts.
    let refused = app.oneshot(request()).await.unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(spawn_count(&dir), 3);
    registry.shutdown_all().await.unwrap();
}
