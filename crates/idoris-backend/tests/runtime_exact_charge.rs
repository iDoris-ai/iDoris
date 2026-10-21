#![allow(clippy::unwrap_used)]

use idoris_backend::{
    GlobalCapacityLedger, MockAdapter, ModelInfo, RuntimeAdapter, Supervisor, SupervisorConfig,
};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use std::{sync::Arc, time::Duration};

fn policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::RequiresEviction,
    }
}
fn model(id: &str, memory_gb: f64) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        memory_gb,
    }
}

#[tokio::test]
async fn cancelled_tiny_load_keeps_exact_claim_until_confirmed_unload() {
    for (cap, full, tiny) in [
        (125_000_000_u64, 0.125, 1e-20),
        (16_777_216_000_000_000, 16_777_216.0, 0.5e-9),
    ] {
        let ledger = Arc::new(GlobalCapacityLedger::new_bytes(cap + 1));
        let a = Arc::new(MockAdapter::new(vec![
            model("full", full),
            model("tiny", tiny),
        ]));
        let b = Arc::new(MockAdapter::new(vec![model("tiny", tiny)]));
        a.set_load_delay("tiny", Duration::from_millis(50));
        let config = SupervisorConfig {
            budget_gb: (cap + 1) as f64 / 1e9,
            ..SupervisorConfig::default()
        };
        let ha = Supervisor::spawn_with_startup_capacity_tracking(
            a.clone(),
            config.clone(),
            ledger.clone(),
            "a",
        )
        .unwrap();
        let hb = Supervisor::spawn_with_startup_capacity_tracking(
            b.clone(),
            config,
            ledger.clone(),
            "b",
        )
        .unwrap();
        ha.load("full", full, policy()).await.unwrap();
        let task = {
            let ha = ha.clone();
            tokio::spawn(async move { ha.load("tiny", tiny, policy()).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while a.load_call_count("tiny") == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(hb.load("tiny", tiny, policy()).await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ha.is_ready("tiny").await.unwrap() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(hb.load("tiny", tiny, policy()).await.is_err());
        assert_eq!(b.load_call_count("tiny"), 0);
        ha.unload("tiny").await.unwrap();
        assert_eq!(a.unload_call_count("tiny"), 1);
        hb.load("tiny", tiny, policy()).await.unwrap();
        assert_eq!(b.load_call_count("tiny"), 1);
        assert!(ha.load("tiny", tiny, policy()).await.is_err());
        assert_eq!(a.load_call_count("tiny"), 1);
        ha.unload("full").await.unwrap();
        hb.unload("tiny").await.unwrap();
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 0.0);
    }
}

#[tokio::test]
async fn inherited_truth_is_retained_and_confirmed_release_frees_exact_charge() {
    for cap in [1_000_000_000, 4_000_000_000] {
        let ledger = Arc::new(GlobalCapacityLedger::new_bytes(cap));
        let adapter = Arc::new(MockAdapter::new(vec![
            model("inherited", 4.0),
            model("tiny", 1e-20),
        ]));
        adapter.load("inherited", Some(&policy())).await.unwrap();
        let handle = Supervisor::spawn_with_startup_capacity_tracking(
            adapter.clone(),
            SupervisorConfig {
                budget_gb: cap as f64 / 1e9,
                ..SupervisorConfig::default()
            },
            ledger.clone(),
            "inherited",
        )
        .unwrap();
        handle.status().await.unwrap();
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 4.0);
        if cap == 4_000_000_000 {
            handle.load("inherited", 4.0, policy()).await.unwrap();
            assert_eq!(ledger.snapshot().unwrap().reserved_gb, 4.0);
        }
        assert!(handle.load("tiny", 1e-20, policy()).await.is_err());
        assert_eq!(adapter.load_call_count("tiny"), 0);
        handle.unload("inherited").await.unwrap();
        handle.load("tiny", 1e-20, policy()).await.unwrap();
        assert_eq!(adapter.load_call_count("tiny"), 1);
        handle.unload("tiny").await.unwrap();
        assert_eq!(ledger.snapshot().unwrap().reserved_gb, 0.0);
    }
}

#[tokio::test]
async fn shrinking_replacement_must_fit_alongside_other_observed_runtime_truth() {
    let ledger = Arc::new(GlobalCapacityLedger::new_bytes(8_000_000_000));
    let a = Arc::new(MockAdapter::new(vec![
        model("old", 8.0),
        model("too-big", 7.0),
        model("fits", 3.0),
    ]));
    let config = SupervisorConfig {
        budget_gb: 8.0,
        ..SupervisorConfig::default()
    };
    let ha = Supervisor::spawn_with_startup_capacity_tracking(
        a.clone(),
        config.clone(),
        ledger.clone(),
        "a",
    )
    .unwrap();
    ha.load("old", 8.0, policy()).await.unwrap();
    let b = Arc::new(MockAdapter::new(vec![model("inherited", 4.0)]));
    b.load("inherited", Some(&policy())).await.unwrap();
    let hb =
        Supervisor::spawn_with_startup_capacity_tracking(b, config, ledger.clone(), "b").unwrap();
    hb.status().await.unwrap();
    assert_eq!(ledger.snapshot().unwrap().reserved_gb, 12.0);
    assert!(ha.load("too-big", 7.0, policy()).await.is_err());
    assert_eq!(a.load_call_count("too-big"), 0);
    assert_eq!(a.unload_call_count("old"), 0);
    ha.load("fits", 3.0, policy()).await.unwrap();
    assert_eq!(a.unload_call_count("old"), 1);
    assert_eq!(a.load_call_count("fits"), 1);
    assert_eq!(ledger.snapshot().unwrap().reserved_gb, 7.0);
}
