//! IDORIS-CORE S8：`load` 有自己的超时。适配器声明了 [`RuntimeAdapter::load_timeout`]，Supervisor 才用它（外加一段固定的余量）
//! 给 `adapter.load` 这一次调用兜底；没声明的适配器、以及 `unload`/`status`/`probe_ready`/`list` 仍受 `adapter_call_timeout` 约束。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use idoris_backend::mock::LoadOutcome;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, RuntimeAdapter,
    Supervisor, SupervisorConfig,
};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use tokio_util::sync::CancellationToken;

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
        admission: Admission::Coexist,
    }
}

/// `adapter_call_timeout` 缩到 200 毫秒，别的都是缺省。
fn short_calls() -> SupervisorConfig {
    SupervisorConfig {
        adapter_call_timeout: Duration::from_millis(200),
        oom_retry_backoff: Duration::from_millis(10),
        ..SupervisorConfig::default()
    }
}

/// `MockAdapter` 加上可声明的加载预算。
struct Declared {
    inner: MockAdapter,
    load_timeout: Option<Duration>,
}

impl Declared {
    fn new(load_timeout: Option<Duration>) -> Self {
        Self {
            inner: MockAdapter::new(vec![ModelInfo {
                id: "m".into(),
                memory_gb: 1.0,
            }]),
            load_timeout,
        }
    }
}

#[async_trait]
impl RuntimeAdapter for Declared {
    fn load_timeout(&self) -> Option<Duration> {
        self.load_timeout
    }

    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.inner.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.inner.list().await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.inner.load(id, policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.inner.unload(id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.inner.status().await
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.inner.probe_ready(id).await
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.inner.chat(req, cancel).await
    }
}

#[tokio::test]
async fn an_adapter_without_a_load_budget_is_still_bounded_by_the_generic_call_timeout() {
    let adapter = Arc::new(Declared::new(None));
    adapter
        .inner
        .set_load_delay("m", Duration::from_millis(3_000));
    let fence = adapter.inner.load_fence_path().unwrap();
    let handle = Supervisor::spawn(adapter, short_calls()).unwrap();
    let started = Instant::now();
    let error = handle.load("m", 1.0, on_demand()).await.unwrap_err();
    assert_eq!(error.reason_code(), "adapter_timed_out", "{error}");
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
        "the 200 ms generic deadline fired, not the engine's 3 s: {:?}",
        started.elapsed()
    );
    assert!(
        fence.exists(),
        "an unknown outcome keeps the durable fence (unchanged behaviour)"
    );
}

#[tokio::test]
async fn an_adapter_that_declares_a_load_budget_may_load_for_longer_than_the_generic_timeout() {
    let adapter = Arc::new(Declared::new(Some(Duration::from_secs(5))));
    adapter
        .inner
        .set_load_delay("m", Duration::from_millis(600));
    let fence = adapter.inner.load_fence_path().unwrap();
    let handle = Supervisor::spawn(adapter.clone(), short_calls()).unwrap();
    handle.load("m", 1.0, on_demand()).await.unwrap();
    assert_eq!(adapter.inner.load_call_count("m"), 1);
    assert!(handle.is_ready("m").await.unwrap());
    assert!(
        !fence.exists(),
        "a load that finished inside its budget leaves no fence"
    );
}

#[tokio::test]
async fn the_oom_retry_gets_the_same_load_budget() {
    let adapter = Arc::new(Declared::new(Some(Duration::from_secs(5))));
    adapter
        .inner
        .set_load_delay("m", Duration::from_millis(400));
    adapter
        .inner
        .set_load_script("m", vec![LoadOutcome::Oom, LoadOutcome::Ok]);
    let handle = Supervisor::spawn(adapter.clone(), short_calls()).unwrap();
    handle.load("m", 1.0, on_demand()).await.unwrap();
    assert_eq!(
        adapter.inner.load_call_count("m"),
        2,
        "both attempts took 400 ms, longer than the 200 ms generic deadline"
    );
}

#[tokio::test]
async fn only_the_load_call_gets_the_longer_deadline_unload_keeps_the_generic_one() {
    let adapter = Arc::new(Declared::new(Some(Duration::from_secs(5))));
    adapter
        .inner
        .set_unload_delay("m", Duration::from_millis(3_000));
    let handle = Supervisor::spawn(adapter.clone(), short_calls()).unwrap();
    handle.load("m", 1.0, on_demand()).await.unwrap();
    let started = Instant::now();
    let error = handle.unload("m").await.unwrap_err();
    assert_eq!(error.reason_code(), "adapter_timed_out", "{error}");
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
        "{:?}",
        started.elapsed()
    );
}
