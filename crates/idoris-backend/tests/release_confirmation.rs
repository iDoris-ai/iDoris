#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
    Supervisor, SupervisorConfig, SupervisorHandle,
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

/// Reports the selected model as no longer loaded after unload while keeping
/// its memory charged, modeling an engine whose successful unload response
/// does not prove that allocation has been released.
struct RetainedMemoryAdapter {
    inner: MockAdapter,
    retained_id: String,
    retain_after_unload: AtomicBool,
    fail_probe: bool,
    failed_probe: AtomicBool,
}

impl RetainedMemoryAdapter {
    fn new(catalog: Vec<ModelInfo>, retained_id: &str, fail_probe: bool) -> Self {
        Self {
            inner: MockAdapter::new(catalog),
            retained_id: retained_id.to_owned(),
            retain_after_unload: AtomicBool::new(false),
            fail_probe,
            failed_probe: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl RuntimeAdapter for RetainedMemoryAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.inner.list().await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.inner.load(id, policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.inner.unload(id).await?;
        if id == self.retained_id {
            self.retain_after_unload.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let mut status = self.inner.status().await?;
        if self.retain_after_unload.load(Ordering::SeqCst) {
            status.loaded.retain(|id| id != &self.retained_id);
            status.used_gb += self
                .inner
                .list()
                .await?
                .iter()
                .find(|model| model.id == self.retained_id)
                .map_or(0.0, |model| model.memory_gb);
        }
        Ok(status)
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        if self.fail_probe && id == self.retained_id {
            self.failed_probe.store(true, Ordering::SeqCst);
            return Err(BackendError::Upstream {
                message: "scripted probe failure".into(),
            });
        }
        self.inner.probe_ready(id).await
    }

    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.inner.chat(request, cancel).await
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

async fn assert_unconfirmed_memory_rejects_next_admission(
    handle: &SupervisorHandle,
    adapter: &Arc<RetainedMemoryAdapter>,
) {
    let status = handle.status().await.unwrap();
    assert!(!status.loaded.iter().any(|id| id == "a"));
    assert_eq!(status.used_gb, 20.0);
    let engine_status = adapter.status().await.unwrap();
    assert!(!engine_status.loaded.iter().any(|id| id == "a"));
    assert_eq!(engine_status.used_gb, 20.0);
    assert!(handle.load("b", 20.0, on_demand()).await.is_err());
    assert_eq!(adapter.inner.load_call_count("b"), 0);
}

#[tokio::test(start_paused = true)]
async fn standalone_unload_without_confirmed_release_keeps_memory_charged() {
    let adapter = Arc::new(RetainedMemoryAdapter::new(
        models(&[("a", 20.0), ("b", 20.0)]),
        "a",
        false,
    ));
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    handle.load("a", 20.0, on_demand()).await.unwrap();
    assert!(handle.unload("a").await.is_err());
    assert_eq!(adapter.inner.unload_call_count("a"), 1);
    assert_unconfirmed_memory_rejects_next_admission(&handle, &adapter).await;
}

#[tokio::test(start_paused = true)]
async fn probe_cleanup_without_confirmed_release_keeps_memory_charged() {
    let adapter = Arc::new(RetainedMemoryAdapter::new(
        models(&[("a", 20.0), ("b", 20.0)]),
        "a",
        true,
    ));
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    assert!(handle.load("a", 20.0, on_demand()).await.is_err());
    assert!(adapter.failed_probe.load(Ordering::SeqCst));
    assert_eq!(adapter.inner.unload_call_count("a"), 1);
    assert_unconfirmed_memory_rejects_next_admission(&handle, &adapter).await;
}

#[tokio::test(start_paused = true)]
async fn failed_different_policy_oom_reload_keeps_original_capacity_reserved() {
    use idoris_backend::mock::{LoadOutcome, UnloadOutcome};

    for (old_gb, replacement_gb) in [(8.0, 1.0), (1.0, 8.0)] {
        let adapter = Arc::new(MockAdapter::new(models(&[("a", old_gb), ("b", 7.0)])));
        adapter.set_load_script("a", vec![LoadOutcome::Ok, LoadOutcome::Oom]);
        adapter.set_unload_script("a", vec![UnloadOutcome::Fail]);
        let handle = Supervisor::spawn(adapter.clone(), config(8.0)).unwrap();

        handle.load("a", old_gb, on_demand()).await.unwrap();
        assert!(handle.load("a", replacement_gb, resident()).await.is_err());
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
