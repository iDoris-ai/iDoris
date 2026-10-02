#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use idoris_backend::{
    ChatRequest, MockAdapter, ModelInfo, RuntimeAdapter, Supervisor, SupervisorConfig,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

fn policy(mode: LoadMode, keepalive: Keepalive) -> LoadPolicy {
    LoadPolicy {
        mode,
        keepalive,
        admission: Admission::Coexist,
    }
}

fn on_demand() -> LoadPolicy {
    policy(LoadMode::OnDemand, Keepalive::IdleTtl { idle_ttl_s: 300 })
}

fn resident() -> LoadPolicy {
    policy(LoadMode::Resident, Keepalive::Pinned { pinned: true })
}

fn config(budget_gb: f64) -> SupervisorConfig {
    SupervisorConfig {
        budget_gb,
        release_confirm_interval: Duration::from_millis(1),
        release_confirm_max_attempts: 2,
        ..SupervisorConfig::default()
    }
}

fn models(entries: &[(&str, f64)]) -> Vec<ModelInfo> {
    entries
        .iter()
        .map(|(id, memory_gb)| ModelInfo {
            id: (*id).into(),
            memory_gb: *memory_gb,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn failed_different_policy_oom_reload_keeps_original_capacity_reserved() {
    use idoris_backend::mock::{LoadOutcome, UnloadOutcome};

    for (old_gb, replacement_gb) in [(8.0, 1.0), (1.0, 8.0)] {
        let adapter = Arc::new(MockAdapter::new(models(&[("a", old_gb), ("b", 7.0)])));
        adapter.set_load_script("a", vec![LoadOutcome::Ok, LoadOutcome::Oom]);
        adapter.set_unload_script("a", vec![UnloadOutcome::Fail]);
        let handle = Supervisor::spawn(adapter.clone(), config(8.0)).unwrap();

        handle.load("a", old_gb, resident()).await.unwrap();
        assert!(handle.load("a", replacement_gb, on_demand()).await.is_err());
        assert_eq!(adapter.load_call_count("a"), 2);
        assert_eq!(adapter.unload_call_count("a"), 1);
        let status = handle.status().await.unwrap();
        assert_eq!(status.used_gb, old_gb.max(replacement_gb));
        assert!(status.loaded.is_empty());
        let chat_error = handle
            .chat(
                ChatRequest {
                    model: "a".into(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(chat_error.reason_code(), "model_unavailable");

        assert!(handle.load("b", 7.0, on_demand()).await.is_err());
        assert_eq!(adapter.load_call_count("b"), 0);
        assert_eq!(
            handle.status().await.unwrap().used_gb,
            old_gb.max(replacement_gb)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn reload_can_evict_another_model_and_load_when_release_is_confirmed() {
    let adapter = Arc::new(MockAdapter::new(models(&[("a", 4.0), ("b", 4.0)])));
    let handle = Supervisor::spawn(adapter.clone(), config(8.0)).unwrap();

    handle.load("a", 4.0, on_demand()).await.unwrap();
    handle.load("b", 4.0, on_demand()).await.unwrap();
    handle.load("a", 6.0, resident()).await.unwrap();

    let status = handle.status().await.unwrap();
    assert_eq!(status.used_gb, 6.0);
    assert_eq!(status.loaded, vec!["a"]);
    assert_eq!(adapter.unload_call_count("b"), 1);
    assert_eq!(adapter.load_call_count("a"), 2);

    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
    assert_eq!(adapter.unload_call_count("a"), 1);
}

#[tokio::test(start_paused = true)]
async fn standalone_unload_preserves_other_models_memory_accounting() {
    let adapter = Arc::new(MockAdapter::new(models(&[("a", 4.0), ("b", 4.0)])));
    let handle = Supervisor::spawn(adapter.clone(), config(8.0)).unwrap();
    handle.load("a", 4.0, on_demand()).await.unwrap();
    handle.load("b", 4.0, on_demand()).await.unwrap();

    handle.unload("a").await.unwrap();
    let status = handle.status().await.unwrap();
    assert_eq!(status.used_gb, 4.0);
    assert_eq!(status.loaded, vec!["b"]);
    let engine_status = adapter.status().await.unwrap();
    assert_eq!(engine_status.used_gb, 4.0);
    assert_eq!(engine_status.loaded, vec!["b"]);

    handle.unload("b").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
    assert_eq!(adapter.unload_call_count("a"), 1);
    assert_eq!(adapter.unload_call_count("b"), 1);
}
