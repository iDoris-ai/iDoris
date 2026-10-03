#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, Pressure,
    RuntimeAdapter, Supervisor, SupervisorConfig, mock::LoadOutcome,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    }
}

fn inherited_policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

fn config(budget_gb: f64) -> SupervisorConfig {
    SupervisorConfig {
        budget_gb,
        adapter_call_timeout: Duration::from_secs(1),
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

#[tokio::test]
async fn engine_ownership_survives_fence_clear_and_releases_when_actor_exits() {
    let adapter = Arc::new(MockAdapter::new(models(&[("a", 8.0), ("b", 8.0)])));
    let fence_path = adapter.load_fence_path().unwrap();
    let first = Supervisor::spawn(adapter.clone(), config(10.0)).unwrap();
    let second = Supervisor::spawn(adapter.clone(), config(10.0)).unwrap();

    // Both actors have completed startup reconciliation before either load.
    first.status().await.unwrap();
    assert!(second.status().await.is_err());

    first.load("a", 8.0, policy()).await.unwrap();
    assert!(
        !fence_path.exists(),
        "successful load should clear the marker"
    );
    assert!(second.load("b", 8.0, policy()).await.is_err());
    assert_eq!(adapter.load_call_count("b"), 0);
    let engine = adapter.status().await.unwrap();
    assert_eq!(engine.used_gb, 8.0);
    assert_eq!(engine.loaded, vec!["a"]);

    drop(second);
    drop(first);
    for _ in 0..1000 {
        if Arc::strong_count(&adapter) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        Arc::strong_count(&adapter),
        1,
        "actors should exit on last handle drop"
    );

    // The actor releases engine ownership on exit. A replacement can then
    // reconcile the still-loaded model and continue serving it.
    let replacement = Supervisor::spawn(adapter.clone(), config(10.0)).unwrap();
    assert_eq!(replacement.status().await.unwrap().used_gb, 8.0);
}

#[tokio::test]
async fn inherited_residency_uses_observed_memory_until_unload() {
    for estimate_gb in [20.0, 1.0] {
        let adapter = Arc::new(MockAdapter::new(models(&[("a", 20.0), ("b", 24.0)])));
        // Model A is already resident when this process starts, under a
        // different policy that the Supervisor must apply on adoption.
        adapter.load("a", Some(&inherited_policy())).await.unwrap();
        let initial_load_count = adapter.load_call_count("a");
        let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();

        let startup = handle.status().await.unwrap();
        assert_eq!(startup.used_gb, 20.0);

        handle.load("a", estimate_gb, policy()).await.unwrap();
        let adopted_load_count = adapter.load_call_count("a");
        assert_eq!(adopted_load_count, initial_load_count + 1);
        assert_eq!(adapter.effective_policy("a").unwrap(), Some(policy()));
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

        // A repeated exact-policy request is a no-op and the observed
        // allocation remains 20GB even when the caller estimated only 1GB.
        handle.load("a", estimate_gb, policy()).await.unwrap();
        assert_eq!(adapter.load_call_count("a"), adopted_load_count);
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
        handle
            .chat(
                ChatRequest {
                    model: "a".into(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(handle.load("b", 24.0, policy()).await.is_err());
        assert_eq!(adapter.load_call_count("b"), 0);
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

        handle.unload("a").await.unwrap();
        assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
        handle.load("b", 24.0, policy()).await.unwrap();
        assert_eq!(adapter.load_call_count("b"), 1);
        assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    }
}

/// Reports engine-observed residency independently from catalog estimates.
/// This lets the test model two inherited IDs sharing a 20GB startup
/// allocation while the catalog advertises only 1GB for each.
struct ObservedResidencyAdapter {
    mock: MockAdapter,
    resident_gb: Mutex<BTreeMap<String, f64>>,
}

#[async_trait::async_trait]
impl RuntimeAdapter for ObservedResidencyAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, load_policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.mock.load(id, load_policy).await?;
        let info = self
            .mock
            .list()
            .await?
            .into_iter()
            .find(|model| model.id == id)
            .ok_or_else(|| BackendError::model_not_found(id))?;
        self.resident_gb
            .lock()
            .expect("residency lock should be healthy")
            .entry(id.to_string())
            .or_insert(info.memory_gb);
        Ok(())
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await?;
        self.resident_gb
            .lock()
            .expect("residency lock should be healthy")
            .remove(id);
        Ok(())
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let resident = self
            .resident_gb
            .lock()
            .expect("residency lock should be healthy");
        Ok(BackendStatus {
            pressure: Pressure::Ok,
            used_gb: resident.values().sum(),
            model_memory_max_gb: 64.0,
            loaded: resident.keys().cloned().collect(),
        })
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        Ok(self
            .resident_gb
            .lock()
            .expect("residency lock should be healthy")
            .contains_key(id))
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(req, cancel).await
    }
}

#[tokio::test]
async fn aggregate_inherited_residency_is_released_per_id() {
    let adapter = Arc::new(ObservedResidencyAdapter {
        mock: MockAdapter::new(models(&[("a", 1.0), ("b", 1.0), ("c", 24.0)])),
        resident_gb: Mutex::new(BTreeMap::from([
            ("a".to_string(), 10.0),
            ("b".to_string(), 10.0),
        ])),
    });
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

    // Both identities remain attached to the aggregate observed reservation,
    // even when callers later provide smaller estimates.
    handle.load("a", 1.0, policy()).await.unwrap();
    handle.load("b", 1.0, policy()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
    assert!(handle.load("c", 24.0, policy()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("c"), 0);

    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 10.0);
    assert!(handle.load("c", 24.0, policy()).await.is_err());
    handle.unload("b").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
    handle.load("c", 24.0, policy()).await.unwrap();
    assert_eq!(adapter.mock.load_call_count("c"), 1);
}

#[tokio::test]
async fn inherited_model_can_be_unloaded_without_adoption() {
    let adapter = Arc::new(MockAdapter::new(models(&[("a", 20.0), ("b", 24.0)])));
    adapter.load("a", None).await.unwrap();
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    assert_eq!(handle.status().await.unwrap().loaded, vec!["a"]);
    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
    handle.load("b", 24.0, policy()).await.unwrap();
}

#[tokio::test]
async fn inherited_cleanup_after_oom_can_release_remaining_reservation() {
    let adapter = Arc::new(MockAdapter::new(models(&[("a", 20.0), ("b", 24.0)])));
    adapter.load("a", None).await.unwrap();
    adapter.set_load_script("a", vec![LoadOutcome::Oom]);
    let mut limits = config(24.0);
    limits.release_confirm_max_attempts = 1;
    limits.release_confirm_interval = Duration::ZERO;
    let handle = Supervisor::spawn(adapter.clone(), limits).unwrap();
    assert!(handle.load("a", 20.0, policy()).await.is_err());
    assert_eq!(adapter.status().await.unwrap().used_gb, 0.0);
    assert!(handle.status().await.unwrap().loaded.is_empty());
    // Confirmed cleanup releases A's inherited reservation immediately.
    handle.load("b", 24.0, policy()).await.unwrap();
}

#[tokio::test]
async fn confirmed_oom_cleanup_does_not_reuse_inherited_reservation_on_retry() {
    let adapter = Arc::new(MockAdapter::new(models(&[
        ("a", 8.0),
        ("b", 8.0),
        ("c", 8.0),
        ("d", 16.0),
    ])));
    for id in ["a", "b", "c"] {
        adapter.load(id, Some(&inherited_policy())).await.unwrap();
    }
    adapter.set_load_script("a", vec![LoadOutcome::Oom]);

    let mut limits = config(24.0);
    limits.release_confirm_max_attempts = 1;
    limits.release_confirm_interval = Duration::ZERO;
    let handle = Supervisor::spawn(adapter.clone(), limits).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);

    assert!(handle.load("a", 8.0, policy()).await.is_err());
    assert_eq!(adapter.status().await.unwrap().used_gb, 16.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 16.0);

    handle.unload("b").await.unwrap();
    assert_eq!(adapter.status().await.unwrap().used_gb, 8.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 8.0);

    // Retry A directly after confirmed cleanup; no explicit unload(A) should
    // be needed to clear its inherited identity or reservation.
    handle.load("a", 8.0, policy()).await.unwrap();
    assert_eq!(adapter.status().await.unwrap().used_gb, 16.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 16.0);

    // C and retried A use 16GB, so D's 16GB cannot fit in the 24GB budget.
    assert!(handle.load("d", 16.0, policy()).await.is_err());
    assert_eq!(adapter.load_call_count("d"), 0);
    assert_eq!(adapter.status().await.unwrap().used_gb, 16.0);
}

/// Models engines that free an inherited allocation as part of rejecting an
/// OOM load, before the Supervisor can issue a cleanup unload.
struct OomReleasesBeforeReturnAdapter {
    mock: MockAdapter,
}

#[async_trait::async_trait]
impl RuntimeAdapter for OomReleasesBeforeReturnAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, load_policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        let result = self.mock.load(id, load_policy).await;
        if result.as_ref().is_err_and(|err| err.is_oom()) {
            self.mock.unload(id).await?;
        }
        result
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.mock.status().await
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.mock.probe_ready(id).await
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(req, cancel).await
    }
}

#[tokio::test]
async fn inherited_oom_released_by_engine_requires_fresh_admission() {
    let mock = MockAdapter::new(models(&[("a", 8.0), ("b", 8.0), ("c", 8.0), ("d", 16.0)]));
    for id in ["a", "b", "c"] {
        mock.load(id, Some(&inherited_policy())).await.unwrap();
    }
    let initial_a_loads = mock.load_call_count("a");
    mock.set_load_script("a", vec![LoadOutcome::Oom]);
    let adapter = Arc::new(OomReleasesBeforeReturnAdapter { mock });

    let mut limits = config(24.0);
    limits.release_confirm_max_attempts = 1;
    limits.release_confirm_interval = Duration::ZERO;
    let handle = Supervisor::spawn(adapter.clone(), limits).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);

    assert!(handle.load("a", 8.0, policy()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("a"), initial_a_loads + 1);
    assert_eq!(adapter.status().await.unwrap().used_gb, 16.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 16.0);

    handle.unload("b").await.unwrap();
    handle.load("a", 8.0, policy()).await.unwrap();
    assert_eq!(adapter.status().await.unwrap().used_gb, 16.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 16.0);

    assert!(handle.load("d", 16.0, policy()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("d"), 0);
    assert_eq!(adapter.status().await.unwrap().used_gb, 16.0);
}

#[tokio::test]
async fn failed_inherited_verification_settles_evicted_victims() {
    let adapter = Arc::new(ObservedResidencyAdapter {
        mock: MockAdapter::new(models(&[("a", 20.0), ("b", 4.0)])),
        resident_gb: Mutex::new(BTreeMap::from([("a".to_string(), 20.0)])),
    });
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    handle.status().await.unwrap();
    handle.load("b", 4.0, inherited_policy()).await.unwrap();
    // The inherited model disappeared after startup. Its reservation cannot
    // fund a fresh load, even if admission first evicts a managed model.
    adapter.resident_gb.lock().unwrap().remove("a");
    assert!(handle.load("a", 24.0, policy()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("a"), 0);
    assert_eq!(adapter.mock.unload_call_count("b"), 1);
    handle.unload("b").await.unwrap();
    assert_eq!(adapter.mock.unload_call_count("b"), 1);
}

#[tokio::test]
async fn invalid_release_measurement_keeps_inherited_reservation() {
    let adapter = Arc::new(ObservedResidencyAdapter {
        mock: MockAdapter::new(models(&[("a", 10.0), ("b", 10.0)])),
        resident_gb: Mutex::new(BTreeMap::from([
            ("a".to_string(), 10.0),
            ("b".to_string(), 10.0),
        ])),
    });
    let mut limits = config(24.0);
    limits.release_confirm_max_attempts = 1;
    limits.release_confirm_interval = Duration::ZERO;
    let handle = Supervisor::spawn(adapter.clone(), limits).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
    adapter.resident_gb.lock().unwrap().insert("b".into(), 0.0);
    assert!(handle.unload("a").await.is_err());
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
}

struct GatedLoadAdapter {
    mock: MockAdapter,
    load_entered: Notify,
    release_load: Notify,
}

#[async_trait::async_trait]
impl RuntimeAdapter for GatedLoadAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, load_policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.load_entered.notify_one();
        self.release_load.notified().await;
        self.mock.load(id, load_policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.mock.status().await
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.mock.probe_ready(id).await
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(req, cancel).await
    }
}

#[tokio::test]
async fn in_flight_load_keeps_engine_owned_after_caller_and_handle_drop() {
    let adapter = Arc::new(GatedLoadAdapter {
        mock: MockAdapter::new(models(&[("a", 8.0)])),
        load_entered: Notify::new(),
        release_load: Notify::new(),
    });
    let handle = Supervisor::spawn(adapter.clone(), config(10.0)).unwrap();
    handle.status().await.unwrap();

    let caller_handle = handle.clone();
    let load_task = tokio::spawn(async move { caller_handle.load("a", 8.0, policy()).await });
    adapter.load_entered.notified().await;

    // Cancelling the caller drops its channel sender while the adapter call
    // remains parked. The actor can then exit after the last public handle
    // drops, while the operation still owns the engine lock.
    load_task.abort();
    let _ = load_task.await;
    drop(handle);

    for _ in 0..1000 {
        if Arc::strong_count(&adapter) <= 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        Arc::strong_count(&adapter) <= 2,
        "actor should exit with no handles"
    );

    let blocked = Supervisor::spawn(adapter.clone(), config(10.0)).unwrap();
    assert!(blocked.status().await.is_err());
    drop(blocked);

    adapter.release_load.notify_one();
    for _ in 0..1000 {
        if Arc::strong_count(&adapter) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        Arc::strong_count(&adapter),
        1,
        "in-flight adapter work should finish"
    );

    let replacement = Supervisor::spawn(adapter.clone(), config(10.0)).unwrap();
    replacement.status().await.unwrap();
}

struct UnreadyAdapter {
    mock: MockAdapter,
    probe_errors: bool,
}

#[async_trait::async_trait]
impl RuntimeAdapter for UnreadyAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, load_policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.mock.load(id, load_policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.mock.status().await
    }

    async fn probe_ready(&self, _id: &str) -> Result<bool, BackendError> {
        if self.probe_errors {
            Err(BackendError::Upstream {
                message: "injected readiness failure".into(),
            })
        } else {
            Ok(false)
        }
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(req, cancel).await
    }
}

#[tokio::test]
async fn inherited_load_with_false_or_failed_readiness_keeps_reservation() {
    for probe_errors in [false, true] {
        let mock = MockAdapter::new(models(&[("a", 20.0), ("b", 24.0)]));
        mock.load("a", Some(&inherited_policy())).await.unwrap();
        let adapter = Arc::new(UnreadyAdapter { mock, probe_errors });
        let mut supervisor_config = config(24.0);
        supervisor_config.probe_interval = Duration::ZERO;
        supervisor_config.probe_max_attempts = 1;
        let handle = Supervisor::spawn(adapter.clone(), supervisor_config).unwrap();
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

        assert!(handle.load("a", 20.0, policy()).await.is_err());
        assert_eq!(adapter.mock.load_call_count("a"), 1);
        assert_eq!(adapter.mock.unload_call_count("a"), 0);
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
        assert!(handle.load("b", 24.0, policy()).await.is_err());
        assert_eq!(adapter.mock.load_call_count("b"), 0);
    }
}
