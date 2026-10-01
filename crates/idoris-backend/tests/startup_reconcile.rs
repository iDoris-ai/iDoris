#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatMessage, ChatRequest, ChatResponse, ModelInfo, Pressure,
    RuntimeAdapter, Supervisor, SupervisorConfig,
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
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
        admission: Admission::Coexist,
    }
}

#[derive(Default)]
struct State {
    loaded: HashSet<String>,
    used_gb: f64,
    unloads: Vec<String>,
    loads: Vec<String>,
}

struct TestAdapter {
    catalog: Vec<ModelInfo>,
    state: Mutex<State>,
    block_next_status: Mutex<bool>,
    fail_next_status: Mutex<bool>,
    status_entered: Notify,
    status_release: Notify,
    block_chat: Mutex<bool>,
    chat_entered: Notify,
    chat_release: Notify,
    status_calls: std::sync::atomic::AtomicUsize,
}

impl TestAdapter {
    fn new(catalog: Vec<ModelInfo>, inherited: &[(&str, f64)]) -> Arc<Self> {
        Arc::new(Self {
            catalog,
            state: Mutex::new(State {
                loaded: inherited.iter().map(|(id, _)| (*id).to_string()).collect(),
                used_gb: inherited.iter().map(|(_, gb)| gb).sum(),
                unloads: Vec::new(),
                loads: Vec::new(),
            }),
            block_next_status: Mutex::new(false),
            fail_next_status: Mutex::new(false),
            status_entered: Notify::new(),
            status_release: Notify::new(),
            block_chat: Mutex::new(false),
            chat_entered: Notify::new(),
            chat_release: Notify::new(),
            status_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn block_status(&self) {
        *self.block_next_status.lock().unwrap() = true;
    }

    fn fail_status_once(&self) {
        *self.fail_next_status.lock().unwrap() = true;
    }

    fn add_external_usage(&self, id: &str, gb: f64) {
        let mut state = self.state.lock().unwrap();
        state.loaded.insert(id.to_string());
        state.used_gb += gb;
    }

    fn unload_count(&self, id: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .unloads
            .iter()
            .filter(|v| v.as_str() == id)
            .count()
    }

    fn load_count(&self, id: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .loads
            .iter()
            .filter(|v| v.as_str() == id)
            .count()
    }
}

#[async_trait]
impl RuntimeAdapter for TestAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(self.catalog.clone())
    }

    async fn load(&self, id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        let mut state = self.state.lock().unwrap();
        if state.loaded.insert(id.to_string()) {
            state.used_gb += self
                .catalog
                .iter()
                .find(|m| m.id == id)
                .map_or(0.0, |m| m.memory_gb);
        }
        state.loads.push(id.to_string());
        Ok(())
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        let mut state = self.state.lock().unwrap();
        if state.loaded.remove(id) {
            state.used_gb -= self
                .catalog
                .iter()
                .find(|m| m.id == id)
                .map_or(0.0, |m| m.memory_gb);
        }
        state.unloads.push(id.to_string());
        Ok(())
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.status_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let fail = {
            let mut fail = self.fail_next_status.lock().unwrap();
            let value = *fail;
            *fail = false;
            value
        };
        if fail {
            return Err(BackendError::Upstream {
                message: "injected status failure".into(),
            });
        }

        let captured = {
            let state = self.state.lock().unwrap();
            (
                state.loaded.iter().cloned().collect::<Vec<_>>(),
                state.used_gb,
            )
        };
        let block = {
            let mut block = self.block_next_status.lock().unwrap();
            let value = *block;
            *block = false;
            value
        };
        if block {
            self.status_entered.notify_one();
            self.status_release.notified().await;
        }
        Ok(BackendStatus {
            pressure: Pressure::Ok,
            used_gb: captured.1,
            model_memory_max_gb: 64.0,
            loaded: captured.0,
        })
    }

    async fn probe_ready(&self, _id: &str) -> Result<bool, BackendError> {
        Ok(true)
    }

    async fn chat(
        &self,
        req: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        if *self.block_chat.lock().unwrap() {
            self.chat_entered.notify_one();
            self.chat_release.notified().await;
        }
        Ok(ChatResponse {
            model: req.model,
            content: "ok".into(),
        })
    }
}

fn catalog() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "a".into(),
            memory_gb: 4.0,
        },
        ModelInfo {
            id: "b".into(),
            memory_gb: 4.0,
        },
    ]
}

fn config(budget_gb: f64) -> SupervisorConfig {
    SupervisorConfig {
        budget_gb,
        adapter_call_timeout: std::time::Duration::from_secs(30),
        ..SupervisorConfig::default()
    }
}

#[tokio::test]
async fn repeat_load_unload_then_reload_keeps_only_unknown_residency_reserved() {
    let adapter = TestAdapter::new(catalog(), &[("inherited", 20.0)]);
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    handle.load("a", 4.0, policy()).await.unwrap();
    handle.load("a", 4.0, policy()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);

    // The unknown 20 GB plus A's 4 GB fills the budget, so B must be
    // rejected before the unload creates room for A to be loaded again.
    assert_eq!(
        handle
            .load("b", 8.0, policy())
            .await
            .unwrap_err()
            .reason_code(),
        "eviction_impossible"
    );
    assert_eq!(adapter.load_count("b"), 0);

    handle.unload("a").await.unwrap();
    // Unload leaves the actor's old reservation in place. A later load
    // needs a valid sample before it can rebuild that reservation.
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    adapter.fail_status_once();
    assert_eq!(
        handle
            .load("a", 4.0, policy())
            .await
            .unwrap_err()
            .reason_code(),
        "upstream_error"
    );
    assert_eq!(adapter.load_count("a"), 1);
    assert_eq!(
        handle.status().await.unwrap_err().reason_code(),
        "upstream_error"
    );

    // The one-shot sampling failure has cleared; a fresh sample now permits
    // rebuilding the reservation from the 20 GB unknown residency.
    handle.load("a", 4.0, policy()).await.unwrap();
    assert_eq!(handle.status().await.unwrap().used_gb, 24.0);
    assert_eq!(adapter.load_count("a"), 2);
    assert_eq!(adapter.load_count("b"), 0);
    assert_eq!(adapter.unload_count("inherited"), 0);
}

#[tokio::test]
async fn estimated_managed_memory_cannot_offset_unknown_residency_after_eviction() {
    let adapter = TestAdapter::new(
        vec![
            ModelInfo {
                id: "a".into(),
                memory_gb: 4.0,
            },
            ModelInfo {
                id: "b".into(),
                memory_gb: 8.0,
            },
        ],
        &[],
    );
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    let estimated_evictable = LoadPolicy {
        mode: LoadMode::EvictToLoad,
        admission: Admission::RequiresEviction,
        ..policy()
    };
    handle.load("a", 8.0, estimated_evictable).await.unwrap();
    adapter.add_external_usage("unknown", 20.0);

    assert!(handle.load("b", 8.0, policy()).await.is_err());
    assert_eq!(adapter.load_count("b"), 0);
}

#[tokio::test(start_paused = true)]
async fn hanging_reconcile_does_not_block_status_chat_unload_or_chat_done_drain() {
    let adapter = TestAdapter::new(catalog(), &[]);
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    handle.load("a", 4.0, policy()).await.unwrap();
    *adapter.block_chat.lock().unwrap() = true;
    adapter.block_status();
    let loading = {
        let h = handle.clone();
        tokio::spawn(async move { h.load("b", 4.0, policy()).await })
    };
    adapter.status_entered.notified().await;

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_millis(50), handle.status())
            .await
            .unwrap()
            .unwrap()
            .used_gb,
        4.0
    );
    let chat = {
        let h = handle.clone();
        tokio::spawn(async move {
            h.chat(
                ChatRequest {
                    model: "a".into(),
                    messages: vec![ChatMessage {
                        role: "user".into(),
                        content: "hi".into(),
                    }],
                },
                CancellationToken::new(),
            )
            .await
        })
    };
    adapter.chat_entered.notified().await;
    let unloading = {
        let h = handle.clone();
        tokio::spawn(async move { h.unload("a").await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            loop {
                if !handle
                    .status()
                    .await
                    .unwrap()
                    .loaded
                    .iter()
                    .any(|id| id == "a")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .is_ok(),
        "unload should enter draining while chat is in flight"
    );
    adapter.chat_release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), chat)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), unloading)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    assert_eq!(adapter.unload_count("a"), 1);
    adapter.status_release.notify_one();
    let _ = loading.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reconcile_is_singleflight_or_fast_busy_and_never_reuses_pre_unload_snapshot() {
    let adapter = TestAdapter::new(catalog(), &[]);
    let handle = Supervisor::spawn(adapter.clone(), config(8.0)).unwrap();
    handle.load("a", 4.0, policy()).await.unwrap();
    adapter.block_status();
    let first = {
        let h = handle.clone();
        tokio::spawn(async move { h.load("b", 4.0, policy()).await })
    };
    adapter.status_entered.notified().await;
    let observed_calls = adapter
        .status_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    let duplicate = {
        let h = handle.clone();
        tokio::spawn(async move { h.load("b", 4.0, policy()).await })
    };
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        adapter
            .status_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        observed_calls,
        "same request should merge without a second reconciliation"
    );
    assert!(
        !duplicate.is_finished(),
        "same request waits on the shared reconciliation"
    );
    let started = tokio::time::Instant::now();
    let conflict = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        handle.load(
            "b",
            4.0,
            LoadPolicy {
                keepalive: Keepalive::IdleTtl { idle_ttl_s: 301 },
                ..policy()
            },
        ),
    )
    .await;
    assert!(conflict.is_ok(), "conflicting request must fail fast");
    assert!(conflict.unwrap().is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(30));

    // The blocked status call captured a=4. Unload a while it is suspended,
    // then make the fresh engine state unsafe before releasing that stale sample.
    adapter.add_external_usage("external", 8.0);
    let unload = {
        let h = handle.clone();
        tokio::spawn(async move { h.unload("a").await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), unload)
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    adapter.status_release.notify_one();
    let first_result = tokio::time::timeout(std::time::Duration::from_millis(50), first)
        .await
        .unwrap()
        .unwrap();
    assert!(
        first_result.is_err(),
        "fresh residency must be reconciled after unload"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), duplicate)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(adapter.unload_count("a"), 1);
    assert_eq!(adapter.load_count("b"), 0);
    assert_eq!(
        handle
            .load("b", 4.0, policy())
            .await
            .unwrap_err()
            .reason_code(),
        "eviction_impossible"
    );
    assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    assert_eq!(adapter.load_count("b"), 0);
}

#[tokio::test(start_paused = true)]
async fn ready_same_policy_load_is_noop_during_another_models_reconciliation() {
    let adapter = TestAdapter::new(catalog(), &[]);
    let handle = Supervisor::spawn(adapter.clone(), config(24.0)).unwrap();
    handle.load("a", 4.0, policy()).await.unwrap();
    let a_loads = adapter.load_count("a");
    adapter.block_status();
    let loading_b = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.load("b", 4.0, policy()).await })
    };
    adapter.status_entered.notified().await;

    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            handle.load("a", 4.0, policy()),
        )
        .await
        .unwrap()
        .is_ok()
    );
    assert_eq!(adapter.load_count("a"), a_loads);

    let different_policy = LoadPolicy {
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 301 },
        ..policy()
    };
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            handle.load("a", 4.0, different_policy),
        )
        .await
        .unwrap()
        .is_err()
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            handle.load("b", 5.0, policy()),
        )
        .await
        .unwrap()
        .is_err()
    );
    assert!(handle.load("a", f64::NAN, policy()).await.is_err());
    assert_eq!(adapter.load_count("a"), a_loads);

    adapter.status_release.notify_one();
    assert!(loading_b.await.unwrap().is_ok());
    assert_eq!(adapter.load_count("b"), 1);
}
