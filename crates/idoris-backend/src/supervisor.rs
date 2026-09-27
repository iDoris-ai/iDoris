//! [`Supervisor`] — the single-writer event loop that owns every model's
//! lifecycle for one [`RuntimeAdapter`]. Per
//! `docs/research/Rust基础选型-2026-09-27.md` §4 (llama-swap
//! `internal/router/design.md`; the root cause of llama-swap Issue #946):
//! **one tokio task holds all state**; every other task talks to it only
//! through [`SupervisorHandle`]'s mpsc-backed commands.
//!
//! This PR lands the scaffolding and the read-only/serving commands
//! (`list`/`status`/`chat`) — `load`/`unload`, the eviction integration,
//! singleflight, and the global load/evict mutex land in follow-up PRs.
//! Until then the ledger is always empty, so `chat` can only ever report
//! `model_not_found`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapter::RuntimeAdapter;
use crate::error::BackendError;
use crate::eviction::ModelState;
use crate::types::{BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure};

/// Tunables for the (future) load/probe/eviction-confirm loops. `budget_gb`
/// is the global memory ledger's ceiling (§4: "全局内存账本：budget_gb 由
/// 配置给定"). Already present here (even though `load` doesn't land until
/// a follow-up PR) because [`SupervisorConfig::budget_gb`] also drives this
/// PR's `status` pressure computation.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub budget_gb: f64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self { budget_gb: 24.0 }
    }
}

struct ModelSlot {
    memory_gb: f64,
    state: ModelState,
}

enum Command {
    List {
        reply: oneshot::Sender<Result<Vec<ModelInfo>, BackendError>>,
    },
    Status {
        reply: oneshot::Sender<BackendStatus>,
    },
    Chat {
        req: ChatRequest,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<ChatResponse, BackendError>>,
    },
}

/// A cheaply-`Clone`-able front door to a running [`Supervisor`]. Every
/// method sends one command over the actor's mpsc channel and awaits a
/// oneshot reply; `BackendError::supervisor_unavailable` means the actor
/// task is gone (panicked, or every handle including this one is the last
/// reference and got dropped mid-flight).
#[derive(Clone)]
pub struct SupervisorHandle {
    tx: mpsc::Sender<Command>,
}

impl SupervisorHandle {
    async fn send(&self, cmd: Command) -> Result<(), BackendError> {
        self.tx
            .send(cmd)
            .await
            .map_err(|_| BackendError::supervisor_unavailable())
    }

    pub async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::List { reply }).await?;
        rx.await
            .map_err(|_| BackendError::supervisor_unavailable())?
    }

    pub async fn status(&self) -> Result<BackendStatus, BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Status { reply }).await?;
        rx.await.map_err(|_| BackendError::supervisor_unavailable())
    }

    pub async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Chat { req, cancel, reply }).await?;
        rx.await
            .map_err(|_| BackendError::supervisor_unavailable())?
    }
}

/// Owns nothing after [`Supervisor::spawn`] returns but the join handle
/// implicitly held by the spawned task — all state lives inside the actor
/// loop, reachable only via [`SupervisorHandle`].
pub struct Supervisor;

impl Supervisor {
    pub fn spawn(adapter: Arc<dyn RuntimeAdapter>, config: SupervisorConfig) -> SupervisorHandle {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(run_actor(adapter, config, rx));
        SupervisorHandle { tx }
    }
}

async fn run_actor(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    mut rx: mpsc::Receiver<Command>,
) {
    // Always empty until a follow-up PR adds `load`/`unload` — see the
    // module doc comment.
    let models: HashMap<String, ModelSlot> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::List { reply } => {
                let adapter = adapter.clone();
                tokio::spawn(async move {
                    let _ = reply.send(adapter.list().await);
                });
            }

            Command::Status { reply } => {
                let used_gb: f64 = models
                    .values()
                    .filter(|slot| matches!(slot.state, ModelState::Ready | ModelState::Loading))
                    .map(|slot| slot.memory_gb)
                    .sum();
                let pressure = if used_gb >= config.budget_gb {
                    Pressure::Hard
                } else if used_gb >= config.budget_gb * 0.8 {
                    Pressure::Soft
                } else {
                    Pressure::Ok
                };
                let loaded: Vec<String> = models
                    .iter()
                    .filter(|(_, slot)| slot.state == ModelState::Ready)
                    .map(|(id, _)| id.clone())
                    .collect();
                let _ = reply.send(BackendStatus {
                    pressure,
                    used_gb,
                    model_memory_max_gb: config.budget_gb,
                    loaded,
                });
            }

            Command::Chat { req, cancel, reply } => match models.get(&req.model) {
                None => {
                    let _ = reply.send(Err(BackendError::model_not_found(&req.model)));
                }
                Some(slot) if slot.state != ModelState::Ready => {
                    let _ = reply.send(Err(BackendError::model_unavailable(&req.model)));
                }
                Some(_) => {
                    let adapter = adapter.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(adapter.chat(req, cancel).await);
                    });
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::mock::MockAdapter;
    use crate::types::ModelInfo;

    fn catalog() -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "a".to_string(),
            memory_gb: 4.0,
        }]
    }

    #[tokio::test]
    async fn list_forwards_to_the_adapter() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default());
        let models = handle.list().await.expect("list should succeed");
        assert_eq!(models, catalog());
    }

    #[tokio::test]
    async fn status_reports_an_empty_ledger_before_any_load_lands() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default());
        let status = handle.status().await.expect("status should succeed");
        assert!(status.loaded.is_empty());
        assert_eq!(status.pressure, Pressure::Ok);
    }

    /// Negative contrast: `chat` for a model that has never been loaded
    /// (impossible to have been, since `load` doesn't exist yet in this
    /// PR) must fail with `model_not_found`, not silently succeed.
    #[tokio::test]
    async fn chat_before_any_load_lands_is_not_found() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default());
        let err = handle
            .chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .expect_err("chat before load must fail");
        assert_eq!(err.reason_code(), "model_not_found");
    }
}
