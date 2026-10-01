#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
    Supervisor, SupervisorConfig,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Probe {
    Error,
    NeverReady,
}
#[derive(Clone, Copy)]
enum RetryLoad {
    Succeeds,
    TimedOut,
    Unconfirmed,
    Panics,
    Rejected,
}

struct Adapter {
    inner: MockAdapter,
    probe: Probe,
    retry: RetryLoad,
    loads: AtomicU32,
}

impl Adapter {
    fn new(probe: Probe, retry: RetryLoad) -> Self {
        Self {
            inner: MockAdapter::new(
                [("a", 1.0), ("b", 20.0)]
                    .into_iter()
                    .map(|(id, memory_gb)| ModelInfo {
                        id: id.into(),
                        memory_gb,
                    })
                    .collect(),
            ),
            probe,
            retry,
            loads: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl RuntimeAdapter for Adapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.inner.list().await
    }
    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        if id == "a" && self.loads.fetch_add(1, Ordering::SeqCst) == 1 {
            match self.retry {
                RetryLoad::TimedOut => return std::future::pending().await,
                RetryLoad::Unconfirmed => return Err(BackendError::load_unconfirmed(id, "test")),
                RetryLoad::Panics => panic!("injected load panic"),
                RetryLoad::Rejected => return Err(BackendError::internal("definite rejection")),
                RetryLoad::Succeeds => {}
            }
        }
        self.inner.load(id, policy).await
    }
    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.inner.unload(id).await
    }
    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.inner.status().await
    }
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        if id != "a" {
            return self.inner.probe_ready(id).await;
        }
        match self.probe {
            Probe::Error => Err(BackendError::internal("probe failed")),
            Probe::NeverReady => Ok(false),
        }
    }
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.inner.chat(req, cancel).await
    }
}

fn policy() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 30 },
        admission: Admission::Coexist,
    }
}

fn config() -> SupervisorConfig {
    SupervisorConfig {
        budget_gb: 24.0,
        probe_interval: std::time::Duration::from_millis(1),
        probe_max_attempts: 2,
        adapter_call_timeout: std::time::Duration::from_millis(20),
        release_confirm_interval: std::time::Duration::from_millis(1),
        release_confirm_max_attempts: 2,
        oom_retry_backoff: std::time::Duration::from_millis(1),
        ..SupervisorConfig::default()
    }
}

async fn assert_retry_failure_keeps_a_and_blocks_b(probe: Probe, retry: RetryLoad) {
    let adapter = Arc::new(Adapter::new(probe, retry));
    adapter.inner.set_unload_script(
        "a",
        vec![
            idoris_backend::mock::UnloadOutcome::Fail,
            idoris_backend::mock::UnloadOutcome::Fail,
            idoris_backend::mock::UnloadOutcome::Fail,
        ],
    );
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    assert!(handle.load("a", 1.0, policy()).await.is_err());
    assert_eq!(handle.status().await.unwrap().used_gb, 1.0);
    let expected_error = match retry {
        RetryLoad::TimedOut => "adapter_timed_out",
        RetryLoad::Unconfirmed => "load_unconfirmed",
        RetryLoad::Panics => "adapter_panicked",
        _ => match probe {
            Probe::Error => "internal",
            Probe::NeverReady => "probe_timed_out",
        },
    };
    assert_eq!(
        handle
            .load("a", 8.0, policy())
            .await
            .unwrap_err()
            .reason_code(),
        expected_error
    );
    assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    assert_eq!(
        handle
            .chat(
                ChatRequest {
                    model: "a".into(),
                    messages: vec![]
                },
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .reason_code(),
        "model_unavailable"
    );
    let unloads_before_b = adapter.inner.unload_call_count("a");
    assert_eq!(
        handle
            .load("b", 20.0, policy())
            .await
            .unwrap_err()
            .reason_code(),
        "eviction_failed"
    );
    assert_eq!(adapter.inner.unload_call_count("a"), unloads_before_b + 1);
    assert_eq!(
        adapter.inner.load_call_count("b"),
        0,
        "B must not load while A's release failed"
    );
    assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    adapter.inner.set_unload_script("a", vec![]);
    handle.load("b", 20.0, policy()).await.unwrap();
    let events = adapter.inner.event_log();
    let unload = events.iter().rposition(|e| e == "unload:a:end").unwrap();
    let load = events.iter().position(|e| e == "load:b:start").unwrap();
    assert!(
        unload < load,
        "A must finish unloading before B starts loading: {events:?}"
    );
    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
}

#[tokio::test(start_paused = true)]
async fn probe_error_retry_keeps_larger_footprint_until_release() {
    assert_retry_failure_keeps_a_and_blocks_b(Probe::Error, RetryLoad::Succeeds).await;
}

#[tokio::test(start_paused = true)]
async fn probe_exhaustion_retry_keeps_larger_footprint_until_release() {
    assert_retry_failure_keeps_a_and_blocks_b(Probe::NeverReady, RetryLoad::Succeeds).await;
}

#[tokio::test(start_paused = true)]
async fn timed_out_retry_keeps_larger_footprint_until_release() {
    assert_retry_failure_keeps_a_and_blocks_b(Probe::Error, RetryLoad::TimedOut).await;
}

#[tokio::test(start_paused = true)]
async fn unconfirmed_retry_keeps_larger_footprint_until_release() {
    assert_retry_failure_keeps_a_and_blocks_b(Probe::Error, RetryLoad::Unconfirmed).await;
}

#[tokio::test(start_paused = true)]
async fn panicking_retry_keeps_larger_footprint_until_release() {
    assert_retry_failure_keeps_a_and_blocks_b(Probe::Error, RetryLoad::Panics).await;
}

#[tokio::test(start_paused = true)]
async fn definite_retry_rejection_keeps_old_estimate_and_unload_releases_it() {
    let adapter = Arc::new(Adapter::new(Probe::Error, RetryLoad::Rejected));
    adapter
        .inner
        .set_unload_script("a", vec![idoris_backend::mock::UnloadOutcome::Fail]);
    let handle = Supervisor::spawn(adapter, config()).unwrap();
    assert!(handle.load("a", 1.0, policy()).await.is_err());
    assert_eq!(handle.status().await.unwrap().used_gb, 1.0);
    assert!(handle.load("a", 8.0, policy()).await.is_err());
    assert_eq!(handle.status().await.unwrap().used_gb, 1.0);
    handle.unload("a").await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
}

#[tokio::test(start_paused = true)]
async fn successful_retry_cleanup_releases_both_estimates() {
    for retry in [
        RetryLoad::Succeeds,
        RetryLoad::TimedOut,
        RetryLoad::Unconfirmed,
    ] {
        let adapter = Arc::new(Adapter::new(Probe::Error, retry));
        adapter
            .inner
            .set_unload_script("a", vec![idoris_backend::mock::UnloadOutcome::Fail]);
        let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
        assert!(handle.load("a", 1.0, policy()).await.is_err());
        assert_eq!(handle.status().await.unwrap().used_gb, 1.0);
        assert!(handle.load("a", 8.0, policy()).await.is_err());
        assert_eq!(adapter.inner.unload_call_count("a"), 2);
        assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
    }
}
