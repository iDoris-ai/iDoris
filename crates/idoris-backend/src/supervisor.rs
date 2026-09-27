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

use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapter::RuntimeAdapter;
use crate::error::BackendError;
use crate::eviction::{ModelState, occupies_budget};
use crate::types::{BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure};

/// Tunables for the (future) load/probe/eviction-confirm loops. `budget_gb`
/// is the global memory ledger's ceiling (§4: "全局内存账本：budget_gb 由
/// 配置给定"). Already present here (even though `load` doesn't land until
/// a follow-up PR) because [`SupervisorConfig::budget_gb`] also drives this
/// PR's `status` pressure computation.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub budget_gb: f64,
    /// Caps how many `list`/`chat` calls the loop will have in flight to
    /// the adapter at once (see [`Supervisor::spawn`]'s doc comment on
    /// bounded concurrency). Independent of the mpsc command channel's own
    /// capacity: that one bounds queued *commands*, this one bounds
    /// in-flight *adapter calls*.
    pub max_concurrent_adapter_calls: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            budget_gb: 24.0,
            max_concurrent_adapter_calls: 64,
        }
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
#[derive(Debug, Clone)]
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
///
/// **Bounded concurrency**: `list`/`chat` calls are serviced by spawning a
/// task per call (so a slow adapter call never blocks the single-writer
/// loop from handling unrelated commands) — but only after the loop itself
/// synchronously grabs a permit from a `config.max_concurrent_adapter_calls`
/// -sized [`Semaphore`] via `try_acquire_owned`. This is deliberately
/// non-blocking and fail-fast, not "queue and wait": if every permit is
/// taken, the loop replies `BackendError::Busy` immediately instead of
/// spawning a task that would just sit blocked on the semaphore — that
/// distinction matters, since a task blocked *inside* `acquire().await`
/// would still count as one more accumulating detached task, defeating the
/// point of bounding concurrency in the first place. A permanently-hanging
/// adapter therefore stalls at most `max_concurrent_adapter_calls` real
/// calls, never an unbounded number of tasks. Full graceful-shutdown task
/// tracking/cancellation (stopping still-running spawned calls when the
/// last handle drops) is a deliberately deferred concern for a future PR —
/// today those tasks simply run to completion and their (by-then-unwanted)
/// reply is dropped silently, which is safe but not resource-optimal.
pub struct Supervisor;

impl Supervisor {
    /// # Panics
    ///
    /// Like any API built on [`tokio::spawn`], this must be called from
    /// within a running Tokio runtime (e.g. inside `#[tokio::main]` or
    /// `#[tokio::test]`) — calling it outside one panics, per `tokio::spawn`'s
    /// own documented behavior.
    pub fn spawn(
        adapter: Arc<dyn RuntimeAdapter>,
        config: SupervisorConfig,
    ) -> Result<SupervisorHandle, BackendError> {
        if !config.budget_gb.is_finite() || config.budget_gb < 0.0 {
            return Err(BackendError::internal(format!(
                "SupervisorConfig::budget_gb must be finite and >= 0, got {}",
                config.budget_gb
            )));
        }
        let (tx, rx) = mpsc::channel(64);
        let call_slots = Arc::new(Semaphore::new(config.max_concurrent_adapter_calls.max(1)));
        tokio::spawn(run_actor(adapter, config, rx, call_slots));
        Ok(SupervisorHandle { tx })
    }
}

/// Maps a model's lifecycle state to the error `chat` reports for it.
/// `Launching`/`Loading` are transient (the caller may retry shortly);
/// `Error`/`Stopping`/`Stopped` are terminal/conflicting states a caller
/// must not simply wait out (see [`BackendError::ModelLoading`] and
/// [`BackendError::ModelUnavailable`]'s doc comments).
fn not_ready_error(model_id: &str, state: ModelState) -> BackendError {
    match state {
        ModelState::Launching | ModelState::Loading => BackendError::model_loading(model_id),
        ModelState::Ready => unreachable!("not_ready_error must not be called for Ready"),
        ModelState::Error | ModelState::Stopping | ModelState::Stopped => {
            BackendError::model_unavailable(model_id)
        }
    }
}

async fn run_actor(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    mut rx: mpsc::Receiver<Command>,
    call_slots: Arc<Semaphore>,
) {
    // Always empty until a follow-up PR adds `load`/`unload` — see the
    // module doc comment.
    let models: HashMap<String, ModelSlot> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::List { reply } => {
                let Ok(permit) = call_slots.clone().try_acquire_owned() else {
                    let _ = reply.send(Err(BackendError::busy()));
                    continue;
                };
                let adapter = adapter.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = reply.send(adapter.list().await);
                });
            }

            Command::Status { reply } => {
                let used_gb: f64 = models
                    .values()
                    .filter(|slot| occupies_budget(slot.state))
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
                    let _ = reply.send(Err(not_ready_error(&req.model, slot.state)));
                }
                Some(_) => {
                    let Ok(permit) = call_slots.clone().try_acquire_owned() else {
                        let _ = reply.send(Err(BackendError::busy()));
                        continue;
                    };
                    let adapter = adapter.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
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
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        let models = handle.list().await.expect("list should succeed");
        assert_eq!(models, catalog());
    }

    #[tokio::test]
    async fn status_reports_an_empty_ledger_before_any_load_lands() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
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
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
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

    #[test]
    fn spawn_rejects_a_non_finite_budget() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let err = Supervisor::spawn(
            adapter,
            SupervisorConfig {
                budget_gb: f64::NAN,
                ..SupervisorConfig::default()
            },
        )
        .expect_err("NaN budget must be rejected");
        assert_eq!(err.reason_code(), "internal");
    }

    /// Negative contrast: the identical config shape with a valid budget
    /// succeeds — isolates that it was specifically the invalid value being
    /// rejected, not something else about the config.
    #[tokio::test]
    async fn spawn_accepts_a_valid_budget() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        Supervisor::spawn(adapter, SupervisorConfig::default())
            .expect("a valid default config must be accepted");
    }
}
