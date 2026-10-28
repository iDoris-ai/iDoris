#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use idoris_backend::{ChatResponse, MockAdapter, ModelInfo};
use idoris_contracts::{ComponentCard, TaskProfile};
use idoris_router::{
    dispatch::{DispatchFailure, DispatchInput, dispatch_local},
    profile::{IntentSource, ParsedProfile},
    runtime::RuntimeRegistry,
};
use tokio_util::sync::CancellationToken;

/// Allocated budgets keep 1GB unreserved; tests add it to exercise the reduced cap.
const HEADROOM: u64 = 1_000_000_000;

fn card(id: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.provider.id = id.into();
    card
}

async fn dispatch(
    registry: &RuntimeRegistry,
    card: &ComponentCard,
    model: &str,
) -> Result<ChatResponse, DispatchFailure> {
    let profile = ParsedProfile {
        task: TaskProfile::default(),
        role: None,
        tenant_id: None,
        intent_source: IntentSource::Default,
    };
    dispatch_local(
        std::slice::from_ref(card),
        registry.get(&card.provider.id),
        None,
        &profile,
        DispatchInput::with_model(Some(model), "hello"),
        vec![],
        CancellationToken::new(),
    )
    .await
    .unwrap()
    .result
}

fn model(id: &str, memory_gb: f64) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        memory_gb,
    }
}

#[tokio::test]
async fn full_allocation_denies_positive_growth_lost_by_float_aggregation() {
    for (bytes, full, tiny) in [
        (125_000_000, 0.125, 1e-20),
        (16_777_216_000_000_000, 16_777_216.0, 0.5e-9),
    ] {
        let card = card("exact");
        let adapter = Arc::new(MockAdapter::new(vec![
            model("full", full),
            model("tiny", tiny),
        ]));
        let registry = RuntimeRegistry::spawn_with_factory_and_budget(
            std::slice::from_ref(&card),
            Some(bytes + HEADROOM),
            |_| Ok(adapter.clone()),
        )
        .unwrap();
        assert!(dispatch(&registry, &card, "full").await.is_ok());
        assert!(
            dispatch(&registry, &card, "tiny").await.is_err(),
            "cap={bytes}"
        );
        assert_eq!(adapter.load_call_count("full"), 1);
        assert_eq!(adapter.load_call_count("tiny"), 0);
        // Denial is before eviction or adapter mutation; original target stays ready.
        assert_eq!(adapter.unload_call_count("full"), 0);
        assert!(dispatch(&registry, &card, "full").await.is_ok());
        assert_eq!(adapter.load_call_count("full"), 1);
    }
}

#[tokio::test]
async fn each_model_rounds_up_before_same_runtime_and_cross_runtime_sums() {
    for (bytes, full, tiny) in [
        (125_000_000, 0.125, 1e-20),
        (16_777_216_000_000_000, 16_777_216.0, 0.5e-9),
    ] {
        let a = card("a");
        let b = card("b");
        let aa = Arc::new(MockAdapter::new(vec![
            model("full", full),
            model("tiny", tiny),
        ]));
        let ab = Arc::new(MockAdapter::new(vec![model("tiny", tiny)]));
        let registry = RuntimeRegistry::spawn_with_factory_and_budget(
            &[a.clone(), b.clone()],
            Some(bytes + 1 + HEADROOM),
            |card| {
                Ok(if card.provider.id == "a" {
                    aa.clone()
                } else {
                    ab.clone()
                })
            },
        )
        .unwrap();
        assert!(dispatch(&registry, &a, "full").await.is_ok());
        assert!(
            dispatch(&registry, &a, "tiny").await.is_ok(),
            "one byte of headroom"
        );
        assert!(
            dispatch(&registry, &b, "tiny").await.is_err(),
            "third model requires another byte"
        );
        assert_eq!(aa.load_call_count("full"), 1);
        assert_eq!(aa.load_call_count("tiny"), 1);
        assert_eq!(ab.load_call_count("tiny"), 0);
        assert!(dispatch(&registry, &a, "tiny").await.is_ok());
        assert_eq!(aa.load_call_count("tiny"), 1);
    }
}

#[tokio::test]
async fn separate_sub_byte_models_cannot_share_one_rounded_byte() {
    let card = card("fractional");
    let adapter = Arc::new(MockAdapter::new(vec![
        model("one", 0.25e-9),
        model("two", 0.25e-9),
    ]));
    let registry = RuntimeRegistry::spawn_with_factory_and_budget(
        std::slice::from_ref(&card),
        Some(1 + HEADROOM),
        |_| Ok(adapter.clone()),
    )
    .unwrap();
    assert!(dispatch(&registry, &card, "one").await.is_ok());
    assert!(dispatch(&registry, &card, "two").await.is_err());
    assert_eq!(adapter.load_call_count("one"), 1);
    assert_eq!(adapter.load_call_count("two"), 0);
}

#[tokio::test]
async fn none_keeps_legacy_aggregate_and_epsilon_behavior() {
    let card = card("legacy");
    let adapter = Arc::new(MockAdapter::new(vec![
        model("full", 24.0),
        model("tiny", 1e-20),
    ]));
    let registry =
        RuntimeRegistry::spawn_with_factory_and_budget(std::slice::from_ref(&card), None, |_| {
            Ok(adapter.clone())
        })
        .unwrap();
    assert!(dispatch(&registry, &card, "full").await.is_ok());
    assert!(dispatch(&registry, &card, "tiny").await.is_ok());
    assert_eq!(adapter.load_call_count("tiny"), 1);
}

#[tokio::test]
async fn valid_large_models_still_load_under_maximum_byte_allocation() {
    let card = card("large");
    let adapter = Arc::new(MockAdapter::new(vec![
        model("large", 17_179_869_184.0),
        model("extra", 1_000_000_000.0),
        model("over", 1_000_000_000.0),
    ]));
    let registry = RuntimeRegistry::spawn_with_factory_and_budget(
        std::slice::from_ref(&card),
        Some(u64::MAX),
        |_| Ok(adapter.clone()),
    )
    .unwrap();
    assert!(dispatch(&registry, &card, "large").await.is_ok());
    assert!(dispatch(&registry, &card, "extra").await.is_ok());
    assert!(dispatch(&registry, &card, "over").await.is_ok());
    assert_eq!(adapter.load_call_count("large"), 1);
    assert_eq!(adapter.load_call_count("extra"), 1);
    assert_eq!(adapter.load_call_count("over"), 1);
    assert_eq!(adapter.unload_call_count("large"), 1);
}
