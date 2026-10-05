#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
    Supervisor, SupervisorConfig, mock::LoadOutcome,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

fn pinned() -> LoadPolicy {
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

fn catalog() -> Vec<ModelInfo> {
    [("a", 20.0), ("b", 4.0), ("c", 20.0)]
        .into_iter()
        .map(|(id, memory_gb)| ModelInfo {
            id: id.into(),
            memory_gb,
        })
        .collect()
}

fn config() -> SupervisorConfig {
    SupervisorConfig {
        budget_gb: 24.0,
        release_confirm_interval: Duration::ZERO,
        release_confirm_max_attempts: 1,
        oom_retry_backoff: Duration::ZERO,
        ..SupervisorConfig::default()
    }
}

#[tokio::test]
async fn unloading_inherited_model_does_not_double_count_other_residency() {
    let adapter = Arc::new(MockAdapter::new(catalog()));
    adapter.load("a", Some(&inherited_policy())).await.unwrap();
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

    handle.load("b", 4.0, pinned()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    handle.unload("a").await.unwrap();

    let settled = handle.status().await.unwrap();
    assert_eq!(settled.used_gb, 4.0);
    assert_eq!(settled.loaded, vec!["b"]);
    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 4.0);
    handle.load("c", 20.0, pinned()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    assert_eq!(adapter.load_call_count("c"), 1);
}

/// Simulates an engine that releases an inherited allocation while rejecting
/// its policy update with OOM, before the Supervisor can issue cleanup.
struct OomReleasesBeforeReturnAdapter {
    mock: MockAdapter,
}

#[async_trait]
impl RuntimeAdapter for OomReleasesBeforeReturnAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        let result = self.mock.load(id, policy).await;
        if result.as_ref().is_err_and(BackendError::is_oom) {
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
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(request, cancel).await
    }
}

#[tokio::test]
async fn oom_release_settlement_does_not_double_count_other_residency() {
    let mock = MockAdapter::new(catalog());
    mock.load("a", Some(&inherited_policy())).await.unwrap();
    mock.set_load_script("a", vec![LoadOutcome::Oom, LoadOutcome::Oom]);
    let adapter = Arc::new(OomReleasesBeforeReturnAdapter { mock });
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

    handle.load("b", 4.0, pinned()).await.unwrap();
    assert!(handle.load("a", 20.0, pinned()).await.is_err());
    assert_eq!(adapter.status().await.unwrap().used_gb, 4.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 4.0);
    handle.load("c", 20.0, pinned()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    assert_eq!(adapter.mock.load_call_count("c"), 1);
}

/// Adds engine memory that has no corresponding catalog or ledger slot.
struct AnonymousResidencyAdapter {
    mock: MockAdapter,
}

#[async_trait]
impl RuntimeAdapter for AnonymousResidencyAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.mock.load(id, policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.mock.unload(id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let mut status = self.mock.status().await?;
        status.used_gb += 4.0;
        status.loaded.push("anonymous-engine-residency".into());
        Ok(status)
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.mock.probe_ready(id).await
    }

    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.mock.chat(request, cancel).await
    }
}

#[tokio::test]
async fn anonymous_engine_residency_stays_reserved_after_inherited_unload() {
    let mut models = catalog();
    models[0].memory_gb = 16.0;
    let mock = MockAdapter::new(models);
    mock.load("a", Some(&inherited_policy())).await.unwrap();
    let adapter = Arc::new(AnonymousResidencyAdapter { mock });
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);

    handle.load("b", 4.0, pinned()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);

    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    handle.unload("b").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 4.0);
    assert!(handle.load("c", 21.0, pinned()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("c"), 0);
}

#[tokio::test]
async fn anonymous_residency_is_not_hidden_by_a_larger_same_model_estimate() {
    let mut models = catalog();
    models[0].memory_gb = 16.0;
    let mock = MockAdapter::new(models);
    mock.load("a", Some(&inherited_policy())).await.unwrap();
    let adapter = Arc::new(AnonymousResidencyAdapter { mock });
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("b", 4.0, pinned()).await.unwrap();
    handle.unload("a").await.unwrap();

    assert!(handle.load("b", 24.0, inherited_policy()).await.is_err());
    handle.load("b", 20.0, inherited_policy()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
}

#[tokio::test]
async fn larger_b_estimate_does_not_erase_anonymous_residency_on_unloads() {
    let mut models = catalog();
    models[0].memory_gb = 12.0;
    let mock = MockAdapter::new(models);
    mock.load("a", Some(&inherited_policy())).await.unwrap();
    let adapter = Arc::new(AnonymousResidencyAdapter { mock });
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 16.0);

    handle.load("b", 8.0, pinned()).await.unwrap();
    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    handle.unload("b").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 4.0);

    assert!(handle.load("c", 21.0, pinned()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("c"), 0);
}

#[tokio::test]
async fn eviction_keeps_anonymous_residency_in_capacity_check() {
    let mut models = catalog();
    models[0].memory_gb = 12.0;
    let mock = MockAdapter::new(models);
    mock.load("a", Some(&inherited_policy())).await.unwrap();
    let adapter = Arc::new(AnonymousResidencyAdapter { mock });
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("b", 8.0, inherited_policy()).await.unwrap();
    handle.unload("a").await.unwrap();

    assert!(handle.load("c", 24.0, pinned()).await.is_err());
    assert_eq!(adapter.mock.unload_call_count("b"), 1);
    assert_eq!(adapter.mock.load_call_count("c"), 0);
    assert!(handle.status().await.unwrap().used_gb >= 4.0);
}

#[tokio::test]
async fn confirmed_victim_release_frees_capacity_for_larger_model() {
    let mut models = catalog();
    models[2].memory_gb = 24.0;
    let adapter = Arc::new(MockAdapter::new(models));
    adapter.load("a", Some(&inherited_policy())).await.unwrap();
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    handle.load("b", 4.0, inherited_policy()).await.unwrap();
    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 4.0);

    handle.load("c", 24.0, pinned()).await.unwrap();
    assert_eq!(adapter.unload_call_count("b"), 1);
    assert_eq!(adapter.status().await.unwrap().used_gb, 24.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
}

#[tokio::test]
async fn pre_reload_snapshot_does_not_cover_new_target_estimate() {
    let mut models = catalog();
    models[0].memory_gb = 12.0;
    models[2].memory_gb = 4.0;
    let mock = MockAdapter::new(models);
    mock.load("a", Some(&inherited_policy())).await.unwrap();
    let adapter = Arc::new(AnonymousResidencyAdapter { mock });
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();

    handle.load("b", 4.0, inherited_policy()).await.unwrap();
    handle.load("c", 4.0, pinned()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 12.0);

    handle.load("c", 20.0, inherited_policy()).await.unwrap();
    assert_eq!(adapter.mock.unload_call_count("b"), 1);
    assert_eq!(adapter.status().await.unwrap().used_gb, 8.0);
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
}
