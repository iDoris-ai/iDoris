#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, GlobalCapacityLedger, MockAdapter,
    ModelInfo, RuntimeAdapter, Supervisor, SupervisorConfig,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

fn catalog() -> Vec<ModelInfo> {
    ["a", "b"]
        .into_iter()
        .map(|id| ModelInfo {
            id: id.into(),
            memory_gb: 2.0,
        })
        .collect()
}

fn pinned() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    }
}

enum FirstStatus {
    Sample(BackendStatus),
    Error,
    Delay,
    Panic,
}

/// Uses the public MockAdapter for ordinary bookkeeping and scripts only the startup sample.
struct StartupAdapter {
    mock: MockAdapter,
    first: FirstStatus,
    unknown: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl RuntimeAdapter for StartupAdapter {
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
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return match &self.first {
                FirstStatus::Sample(sample) => Ok(sample.clone()),
                FirstStatus::Error => Err(BackendError::Upstream {
                    message: "startup status failed".into(),
                }),
                FirstStatus::Delay => std::future::pending().await,
                FirstStatus::Panic => panic!("injected startup panic"),
            };
        }
        let mut status = self.mock.status().await?;
        if self.unknown {
            status.used_gb += 20.0;
            status.loaded.push("inherited".into());
        }
        Ok(status)
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

fn sample(used_gb: f64, loaded: &[&str]) -> BackendStatus {
    BackendStatus {
        pressure: idoris_backend::Pressure::Ok,
        used_gb,
        model_memory_max_gb: 64.0,
        loaded: loaded.iter().map(|id| (*id).into()).collect(),
    }
}

fn adapter(first: FirstStatus) -> Arc<StartupAdapter> {
    let unknown =
        matches!(&first, FirstStatus::Sample(s) if s.loaded.iter().any(|id| id == "inherited"));
    Arc::new(StartupAdapter {
        mock: MockAdapter::new(catalog()),
        first,
        unknown,
        calls: AtomicUsize::new(0),
    })
}

fn config() -> SupervisorConfig {
    SupervisorConfig {
        budget_gb: 24.0,
        adapter_call_timeout: Duration::from_secs(1),
        ..SupervisorConfig::default()
    }
}

#[tokio::test]
async fn startup_capacity_tracking_adopts_observed_truth_even_over_budget() {
    let adapter = adapter(FirstStatus::Sample(sample(20.0, &["inherited"])));
    let ledger = Arc::new(GlobalCapacityLedger::new(8.0).unwrap());
    let handle = Supervisor::spawn_with_startup_capacity_tracking(
        adapter,
        config(),
        ledger.clone(),
        "runtime-a",
    )
    .unwrap();

    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.budget_gb, 8.0);
    assert_eq!(snapshot.reserved_gb, 20.0);
    assert_eq!(snapshot.allocations["runtime:runtime-a"], 20.0);
}

#[tokio::test]
async fn startup_capacity_tracking_aggregates_distinct_runtimes_without_double_counting() {
    let ledger = Arc::new(GlobalCapacityLedger::new(8.0).unwrap());
    let a = Supervisor::spawn_with_startup_capacity_tracking(
        adapter(FirstStatus::Sample(sample(7.0, &["inherited-a"]))),
        config(),
        ledger.clone(),
        "runtime-a",
    )
    .unwrap();
    let b = Supervisor::spawn_with_startup_capacity_tracking(
        adapter(FirstStatus::Sample(sample(9.0, &["inherited-b"]))),
        config(),
        ledger.clone(),
        "runtime-b",
    )
    .unwrap();

    assert_eq!(a.status().await.unwrap().used_gb, 7.0);
    assert_eq!(b.status().await.unwrap().used_gb, 9.0);
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.reserved_gb, 16.0);
    assert_eq!(snapshot.allocations["runtime:runtime-a"], 7.0);
    assert_eq!(snapshot.allocations["runtime:runtime-b"], 9.0);
}

#[tokio::test]
async fn startup_capacity_inconsistency_fails_closed() {
    let adapter = adapter(FirstStatus::Sample(sample(2.0, &["inherited"])));
    let ledger = Arc::new(GlobalCapacityLedger::new(24.0).unwrap());
    ledger
        .adopt_observed("runtime:runtime-a", None, 1.0)
        .unwrap();
    let handle = Supervisor::spawn_with_startup_capacity_tracking(
        adapter,
        config(),
        ledger.clone(),
        "runtime-a",
    )
    .unwrap();

    let error = handle.status().await.unwrap_err();
    assert_eq!(error.reason_code(), "state_invariant_violated");
    assert_eq!(
        ledger.snapshot().unwrap().allocations["runtime:runtime-a"],
        1.0
    );
}

#[tokio::test]
async fn consecutive_pinned_loads_fit_alongside_unknown_startup_residency() {
    let adapter = adapter(FirstStatus::Sample(sample(20.0, &["inherited"])));
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();

    assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
    assert!(handle.load("a", 5.0, pinned()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("a"), 0);
    handle.load("a", 2.0, pinned()).await.unwrap();
    handle.load("a", 2.0, pinned()).await.unwrap();
    let evictable = LoadPolicy {
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        ..pinned()
    };
    handle.load("b", 2.0, evictable).await.unwrap();
    handle.load("b", 2.0, evictable).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    assert_eq!(adapter.mock.load_call_count("a"), 1);
    assert_eq!(adapter.mock.load_call_count("b"), 1);
    assert_eq!(adapter.mock.unload_call_count("inherited"), 0);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.status().await.unwrap().used_gb, 24.0);

    handle.unload("a").await.unwrap();
    handle.load("a", 2.0, pinned()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    assert_eq!(adapter.mock.load_call_count("a"), 2);
    handle.unload("a").await.unwrap();
    handle.load("a", 4.0, pinned()).await.unwrap();
    assert_eq!(adapter.mock.unload_call_count("b"), 1);
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    assert_eq!(adapter.mock.unload_call_count("inherited"), 0);
}

#[tokio::test(start_paused = true)]
async fn invalid_or_failed_startup_status_fails_closed_before_adapter_load() {
    let bad = [
        FirstStatus::Error,
        FirstStatus::Sample(sample(f64::NAN, &[])),
        FirstStatus::Sample(sample(-1.0, &[])),
        FirstStatus::Sample(sample(f64::INFINITY, &[])),
        FirstStatus::Sample(sample(0.0, &["inherited"])),
        FirstStatus::Panic,
    ];
    for first in bad {
        let adapter = adapter(first);
        let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
        assert!(handle.status().await.is_err());
        assert!(handle.load("a", 2.0, pinned()).await.is_err());
        assert_eq!(adapter.mock.load_call_count("a"), 0);
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    }
    let adapter = adapter(FirstStatus::Delay);
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), handle.status())
            .await
            .unwrap()
            .is_err()
    );
    assert!(handle.load("a", 2.0, pinned()).await.is_err());
    assert_eq!(adapter.mock.load_call_count("a"), 0);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}
