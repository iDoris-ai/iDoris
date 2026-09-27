//! [`Supervisor`] — the single-writer event loop that owns every model's
//! lifecycle for one [`RuntimeAdapter`]. Per
//! `docs/research/Rust基础选型-2026-09-27.md` §4 (llama-swap
//! `internal/router/design.md`; the root cause of llama-swap Issue #946):
//! **one tokio task holds all state**; every other task talks to it only
//! through [`SupervisorHandle`]'s mpsc-backed commands.
//!
//! This PR adds `load`: singleflight (same-id + identical policy merge),
//! an OOM circuit breaker (one self-healing retry, §4 "熔断"), and
//! `probe_ready` polling for `Loading -> Ready`. `unload`, eviction, and
//! a real wait queue are follow-ups — a different id fails fast with
//! `BackendError::Busy` meanwhile, and capacity isn't checked yet.

use std::collections::HashMap;
use std::sync::Arc;

use idoris_contracts::LoadPolicy;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapter::RuntimeAdapter;
use crate::error::BackendError;
use crate::eviction::{ModelState, occupies_budget};
use crate::types::{BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure};

type LoadReply = oneshot::Sender<Result<(), BackendError>>;

/// Tunables for the load/probe loops. `budget_gb` is the global memory
/// ledger's ceiling (§4: "全局内存账本：budget_gb 由配置给定").
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub budget_gb: f64,
    /// Caps how many `list`/`chat` calls the loop will have in flight to
    /// the adapter at once (see [`Supervisor::spawn`]'s doc comment on
    /// bounded concurrency). Independent of the mpsc command channel's own
    /// capacity: that one bounds queued *commands*, this one bounds
    /// in-flight *adapter calls*.
    pub max_concurrent_adapter_calls: usize,
    /// `probe_ready` poll interval; `probe_max_attempts` bounds retries.
    pub probe_interval: std::time::Duration,
    pub probe_max_attempts: u32,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            budget_gb: 24.0,
            max_concurrent_adapter_calls: 64,
            probe_interval: std::time::Duration::from_millis(20),
            probe_max_attempts: 50,
        }
    }
}

struct ModelSlot {
    memory_gb: f64,
    state: ModelState,
    /// Reserved for eviction (LRU) bookkeeping, landing in a follow-up PR.
    last_used_seq: u64,
    /// Policy last accepted; compared by exact equality to decide merge vs. a new op.
    policy: LoadPolicy,
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
    Load {
        id: String,
        memory_gb: f64,
        policy: LoadPolicy,
        reply: LoadReply,
    },
}

/// What a background load op reports to the single-writer loop; only the
/// loop applies this to `models` (`unload` lands in a follow-up PR).
enum OpOutcome {
    Load { result: Result<(), BackendError> },
}

enum ActorMsg {
    Cmd(Command),
    OpDone { id: String, outcome: OpOutcome },
}

enum ActiveKind {
    Load {
        policy: LoadPolicy,
        waiters: Vec<LoadReply>,
    },
}

struct ActiveOp {
    id: String,
    kind: ActiveKind,
}

/// A cheaply-`Clone`-able front door to a running [`Supervisor`]. Every
/// method sends one command over the actor's mpsc channel and awaits a
/// oneshot reply; `BackendError::supervisor_unavailable` means the actor
/// task is gone (panicked, or every handle including this one is the last
/// reference and got dropped mid-flight).
#[derive(Debug, Clone)]
pub struct SupervisorHandle {
    tx: mpsc::Sender<ActorMsg>,
}

impl SupervisorHandle {
    async fn send(&self, msg: ActorMsg) -> Result<(), BackendError> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| BackendError::supervisor_unavailable())
    }

    pub async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(ActorMsg::Cmd(Command::List { reply })).await?;
        rx.await
            .map_err(|_| BackendError::supervisor_unavailable())?
    }

    pub async fn status(&self) -> Result<BackendStatus, BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(ActorMsg::Cmd(Command::Status { reply })).await?;
        rx.await.map_err(|_| BackendError::supervisor_unavailable())
    }

    pub async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(ActorMsg::Cmd(Command::Chat { req, cancel, reply }))
            .await?;
        rx.await
            .map_err(|_| BackendError::supervisor_unavailable())?
    }

    pub async fn load(
        &self,
        id: impl Into<String>,
        memory_gb: f64,
        policy: LoadPolicy,
    ) -> Result<(), BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(ActorMsg::Cmd(Command::Load {
            id: id.into(),
            memory_gb,
            policy,
            reply,
        }))
        .await?;
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
        let self_tx = tx.clone();
        tokio::spawn(run_actor(adapter, config, rx, call_slots, self_tx));
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

/// Ledger entries are only transitioned, never removed, so every id here
/// is guaranteed present; a missing entry means "不静默" must fail loudly.
fn set_state_or_panic(models: &mut HashMap<String, ModelSlot>, id: &str, state: ModelState) {
    match models.get_mut(id) {
        Some(slot) => slot.state = state,
        None => panic!("Supervisor invariant violated: ledger has no entry for {id:?}"),
    }
}

/// The pure-IO side of a load: calls the adapter and reports the outcome.
/// Capacity isn't checked yet — that lands with eviction in a follow-up.
async fn run_load_flow(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    id: String,
    policy: LoadPolicy,
) -> OpOutcome {
    // OOM circuit breaker: exactly one self-healing retry, never more.
    let mut attempt = adapter.load(&id, Some(&policy)).await;
    if let Err(err) = &attempt
        && err.is_oom()
    {
        attempt = adapter.load(&id, Some(&policy)).await;
    }
    if let Err(err) = attempt {
        return OpOutcome::Load { result: Err(err) };
    }

    for _ in 0..config.probe_max_attempts {
        match adapter.probe_ready(&id).await {
            Ok(true) => return OpOutcome::Load { result: Ok(()) },
            Ok(false) => tokio::time::sleep(config.probe_interval).await,
            Err(err) => return OpOutcome::Load { result: Err(err) },
        }
    }
    OpOutcome::Load {
        result: Err(BackendError::probe_timed_out(id)),
    }
}

/// Bundles what dispatch helpers need but never mutate (clippy arg-count).
struct Env<'a> {
    adapter: &'a Arc<dyn RuntimeAdapter>,
    config: &'a SupervisorConfig,
    self_tx: &'a mpsc::Sender<ActorMsg>,
}

fn start_load(
    id: String,
    policy: LoadPolicy,
    models: &mut HashMap<String, ModelSlot>,
    env: &Env<'_>,
) {
    set_state_or_panic(models, &id, ModelState::Loading);
    let adapter = env.adapter.clone();
    let config = env.config.clone();
    let self_tx = env.self_tx.clone();
    let id_for_task = id.clone();
    tokio::spawn(async move {
        let outcome = run_load_flow(adapter, config, id_for_task.clone(), policy).await;
        let _ = self_tx
            .send(ActorMsg::OpDone {
                id: id_for_task,
                outcome,
            })
            .await;
    });
}

/// Handles `Command::Load`, the single decision point every load path
/// funnels through (§4); no queueing yet — a different id than the
/// active op fails fast with `BackendError::Busy`.
fn handle_load(
    id: String,
    memory_gb: f64,
    policy: LoadPolicy,
    reply: LoadReply,
    models: &mut HashMap<String, ModelSlot>,
    active_op: &mut Option<ActiveOp>,
    env: &Env<'_>,
) {
    if let Some(active) = active_op.as_mut()
        && active.id == id
    {
        if let ActiveKind::Load {
            policy: active_policy,
            waiters,
        } = &mut active.kind
            && *active_policy == policy
        {
            waiters.push(reply);
        } else {
            let _ = reply.send(Err(BackendError::busy()));
        }
        return;
    }
    // Before the busy-fallback: an already-Ready model with a matching
    // policy is a no-op, even while an unrelated id is mid-load.
    if let Some(slot) = models.get(&id)
        && slot.state == ModelState::Ready
        && slot.policy == policy
    {
        let _ = reply.send(Ok(()));
        return;
    }
    if active_op.is_some() {
        let _ = reply.send(Err(BackendError::busy()));
        return;
    }

    let last_used_seq = models.get(&id).map_or(0, |s| s.last_used_seq);
    models.insert(
        id.clone(),
        ModelSlot {
            memory_gb,
            state: ModelState::Launching,
            last_used_seq,
            policy,
        },
    );
    *active_op = Some(ActiveOp {
        id: id.clone(),
        kind: ActiveKind::Load {
            policy,
            waiters: vec![reply],
        },
    });
    start_load(id, policy, models, env);
}

async fn run_actor(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    mut rx: mpsc::Receiver<ActorMsg>,
    call_slots: Arc<Semaphore>,
    self_tx: mpsc::Sender<ActorMsg>,
) {
    let mut models: HashMap<String, ModelSlot> = HashMap::new();
    let mut active_op: Option<ActiveOp> = None;
    let mut next_seq: u64 = 0;
    let env = Env {
        adapter: &adapter,
        config: &config,
        self_tx: &self_tx,
    };

    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Cmd(Command::List { reply }) => {
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

            ActorMsg::Cmd(Command::Status { reply }) => {
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

            ActorMsg::Cmd(Command::Chat { req, cancel, reply }) => match models.get(&req.model) {
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
                    next_seq += 1;
                    if let Some(slot) = models.get_mut(&req.model) {
                        slot.last_used_seq = next_seq;
                    }
                    let adapter = adapter.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = reply.send(adapter.chat(req, cancel).await);
                    });
                }
            },

            ActorMsg::Cmd(Command::Load {
                id,
                memory_gb,
                policy,
                reply,
            }) => {
                handle_load(
                    id,
                    memory_gb,
                    policy,
                    reply,
                    &mut models,
                    &mut active_op,
                    &env,
                );
            }

            ActorMsg::OpDone { id, outcome } => {
                let OpOutcome::Load { result } = outcome;
                match &result {
                    Ok(()) => {
                        next_seq += 1;
                        if let Some(slot) = models.get_mut(&id) {
                            slot.state = ModelState::Ready;
                            slot.last_used_seq = next_seq;
                        } else {
                            panic!("Supervisor invariant violated: ledger lost {id:?} mid-load");
                        }
                    }
                    Err(_) => set_state_or_panic(&mut models, &id, ModelState::Error),
                }
                let waiters = match active_op.take() {
                    Some(ActiveOp {
                        kind: ActiveKind::Load { waiters, .. },
                        ..
                    }) => waiters,
                    None => panic!("Supervisor invariant violated: OpDone with no active op"),
                };
                for waiter in waiters {
                    let _ = waiter.send(result.clone());
                }
            }
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

    /// Negative contrast: `chat` before any `load` fails `model_not_found`.
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

    // Concurrency tests (singleflight, OOM retry, load/unload round-trip,
    // "different id -> Busy") land in the next PR.
}
