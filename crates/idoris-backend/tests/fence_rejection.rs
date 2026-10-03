#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatMessage, ChatRequest, ChatResponse, MockAdapter, ModelInfo,
    RuntimeAdapter, Supervisor, SupervisorConfig,
};
use idoris_contracts::{
    LoadPolicy,
    load_policy::{Admission, Keepalive, LoadMode},
};
use tokio_util::sync::CancellationToken;

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
        admission: Admission::Coexist,
    }
}

fn catalog() -> Vec<ModelInfo> {
    [("a", 8.0), ("b", 4.0), ("c", 20.0)]
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

/// Keeps MockAdapter's real state and makes B's uncertain result synchronous.
struct UnconfirmedBAdapter {
    mock: MockAdapter,
}

#[async_trait]
impl RuntimeAdapter for UnconfirmedBAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        self.mock.load_fence_path()
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        self.mock.list().await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        if id == "b" {
            return Err(BackendError::LoadUnconfirmed {
                model_id: id.into(),
                message: "injected lost load confirmation".into(),
            });
        }
        self.mock.load(id, policy).await
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
async fn unresolved_load_fence_blocks_eviction_of_ready_model() {
    let adapter = Arc::new(UnconfirmedBAdapter {
        mock: MockAdapter::new(catalog()),
    });
    let fence_path = adapter.load_fence_path().unwrap();
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();

    handle.load("a", 8.0, on_demand()).await.unwrap();
    let uncertain = handle.load("b", 4.0, on_demand()).await.unwrap_err();
    assert_eq!(uncertain.reason_code(), "load_unconfirmed");
    assert!(fence_path.exists());
    assert_eq!(handle.status().await.unwrap().used_gb, 12.0);

    let blocked = handle.load("c", 20.0, on_demand()).await.unwrap_err();
    assert!(
        blocked.to_string().contains("load fence"),
        "unexpected rejection: {blocked:?}"
    );
    assert_eq!(adapter.mock.load_call_count("c"), 0);
    assert_eq!(adapter.mock.unload_call_count("a"), 0);

    let status = handle.status().await.unwrap();
    assert_eq!(status.used_gb, 12.0);
    assert!(fence_path.exists());
    let chat = handle
        .chat(
            ChatRequest {
                model: "a".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "still ready".into(),
                }],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(chat.model, "a");
    assert_eq!(chat.content, "mock reply to: still ready");
    assert_eq!(handle.status().await.unwrap().loaded, vec!["a"]);
    assert!(fence_path.exists());
}

#[tokio::test]
async fn ready_on_demand_model_is_evictable_without_a_load_fence() {
    let adapter = Arc::new(MockAdapter::new(catalog()));
    let handle = Supervisor::spawn(adapter.clone(), config()).unwrap();

    handle.load("a", 8.0, on_demand()).await.unwrap();
    handle.load("c", 20.0, on_demand()).await.unwrap();

    assert_eq!(adapter.unload_call_count("a"), 1);
    assert_eq!(adapter.load_call_count("c"), 1);
    assert_eq!(handle.status().await.unwrap().loaded, vec!["c"]);
}
