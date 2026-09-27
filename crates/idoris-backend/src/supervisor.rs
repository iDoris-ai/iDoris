//! [`Supervisor`] — the single-writer event loop that owns every model's
//! lifecycle for one [`RuntimeAdapter`]. Per
//! `docs/research/Rust基础选型-2026-09-27.md` §4 (llama-swap
//! `internal/router/design.md`; the root cause of llama-swap Issue #946):
//! **one tokio task holds all state**; every other task talks to it only
//! through [`SupervisorHandle`]'s mpsc-backed commands.
//!
//! This PR wires [`crate::eviction::plan_eviction`] into `load`: capacity
//! is now actually checked, and if a plan says to evict, the victims are
//! unloaded before the new model is loaded — all still behind the one
//! global load/evict mutex, so nothing else can touch a victim mid-flight.
//! A real wait queue is still a follow-up — a second id fails fast with
//! `BackendError::Busy` meanwhile.

use std::collections::HashMap;
use std::sync::Arc;

use idoris_contracts::LoadPolicy;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapter::RuntimeAdapter;
use crate::error::BackendError;
use crate::eviction::{
    EvictionPlan, ModelEntry, ModelReq, ModelState, PlanEvictionError, Snapshot, occupies_budget,
    plan_eviction,
};
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
    /// Bounds a single `load`/`unload`/`probe_ready` adapter call. Without
    /// this, a hanging call would hold `active_op` (the global load/evict
    /// mutex) forever — llama-swap Issue #946's root cause.
    pub adapter_call_timeout: std::time::Duration,
    /// Poll interval/attempt bound for [`confirm_memory_released`] — used
    /// after an eviction's `unload`s (before the new model's `load`) and
    /// before an OOM retry, never blocking either forever.
    pub release_confirm_interval: std::time::Duration,
    pub release_confirm_max_attempts: u32,
    /// Backoff before the OOM circuit breaker's one retry.
    pub oom_retry_backoff: std::time::Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            budget_gb: 24.0,
            max_concurrent_adapter_calls: 64,
            probe_interval: std::time::Duration::from_millis(20),
            probe_max_attempts: 50,
            adapter_call_timeout: std::time::Duration::from_secs(30),
            release_confirm_interval: std::time::Duration::from_millis(20),
            release_confirm_max_attempts: 10,
            oom_retry_backoff: std::time::Duration::from_millis(100),
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
    /// Number of `chat` calls currently dispatched to this model — see
    /// `ActorMsg::ChatDone`. Excludes it from eviction while nonzero, and
    /// an explicit `unload` defers its actual adapter call until this
    /// drains to 0 (see `handle_unload`).
    inflight: u32,
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
    Unload {
        id: String,
        reply: LoadReply,
    },
}

/// What a background op reports to the single-writer loop; only the loop
/// applies this to `models`.
enum OpOutcome {
    Load {
        result: Result<(), BackendError>,
        /// Every victim `plan_eviction` chose, and how its `unload` went —
        /// applied to the ledger by `OpDone` alongside `result`.
        victim_results: Vec<VictimResult>,
        /// Only consulted when `result` is `Err` — see `run_load_flow`'s
        /// doc comment on why this varies instead of always being `Error`.
        failure_state: ModelState,
    },
    Unload {
        result: Result<(), BackendError>,
    },
}

enum ActorMsg {
    Cmd(Command),
    OpDone {
        id: String,
        outcome: OpOutcome,
    },
    /// A dispatched `chat` call finished — the single-writer loop
    /// decrements `ModelSlot::inflight` here, never the spawned `chat`
    /// task itself (which only touches the adapter, not `models`).
    ChatDone {
        model: String,
    },
}

enum ActiveKind {
    Load {
        policy: LoadPolicy,
        waiters: Vec<LoadReply>,
    },
    Unload {
        waiters: Vec<LoadReply>,
        /// `false` while draining: the id had in-flight `chat` calls when
        /// `unload` was requested, so the actual adapter call is deferred
        /// (state is already `Stopping`, blocking *new* chats) until
        /// `ActorMsg::ChatDone` observes `inflight` reach 0 and flips this.
        started: bool,
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

    pub async fn unload(&self, id: impl Into<String>) -> Result<(), BackendError> {
        let (reply, rx) = oneshot::channel();
        self.send(ActorMsg::Cmd(Command::Unload {
            id: id.into(),
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
/// point of bounding concurrency in the first place. A hanging adapter call
/// is additionally bounded in *time* by `config.adapter_call_timeout` (see
/// [`with_adapter_timeout`]), applied to every `list`/`chat`/`load`/
/// `unload`/`probe_ready` call — so a permanently-hanging adapter stalls at
/// most `max_concurrent_adapter_calls` real calls, each for at most that
/// timeout, never an unbounded number of tasks held forever. Full
/// graceful-shutdown task
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

/// A victim's `unload` outcome, reported alongside the main load result so
/// the single-writer loop can apply both atomically in `OpDone`.
type VictimResult = (String, Result<(), BackendError>);

/// Bounds one `RuntimeAdapter` call so a hang can't hold `active_op` (the
/// global load/evict mutex) forever — see [`SupervisorConfig::adapter_call_timeout`].
async fn with_adapter_timeout<F, T>(
    fut: F,
    timeout: std::time::Duration,
    model_id: &str,
) -> Result<T, BackendError>
where
    F: std::future::Future<Output = Result<T, BackendError>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(BackendError::adapter_timed_out(model_id)),
    }
}

/// Polls `adapter.status().used_gb` until it's confirmed to have dropped
/// below `before_gb`, or gives up after
/// `SupervisorConfig::release_confirm_max_attempts` and falls back to
/// trusting the caller's own estimate that memory was actually freed (H2,
/// Opus Tier-2 review) — never blocks a load forever on a confirmation
/// that may never come. No logging facility exists in this crate yet, so
/// the "and warn" half of H2 is, for now, only this doc comment; wiring it
/// into real telemetry is left for whenever this crate adopts one.
async fn confirm_memory_released(
    adapter: &Arc<dyn RuntimeAdapter>,
    before_gb: f64,
    config: &SupervisorConfig,
) {
    for _ in 0..config.release_confirm_max_attempts {
        if let Ok(status) =
            with_adapter_timeout(adapter.status(), config.adapter_call_timeout, "status").await
            && status.used_gb < before_gb
        {
            return;
        }
        tokio::time::sleep(config.release_confirm_interval).await;
    }
}

/// The pure-IO side of a load: attempts every victim in `evict` (even
/// after an earlier one fails — see the loop below), then, only if all
/// succeeded, calls the adapter and reports the outcome. Any eviction
/// failure means the capacity assumption behind this load no longer
/// holds, so the load itself must not proceed. Every adapter call is
/// timeout-bounded (see [`with_adapter_timeout`]); a panic inside this
/// function is caught by the `tokio::spawn` wrapper in `start_load`, not
/// here — see its doc comment. **Known accepted gap**: if the panic lands
/// mid-eviction-loop, this function's local `victim_results` (including any
/// victims already successfully unloaded before the panic) is lost with
/// the panicking stack frame, so those victims are left in `Stopping`
/// rather than resolved to `Stopped` — recoverable via a manual follow-up
/// `unload`, not a permanent wedge, but not self-healing either. A full fix
/// needs `catch_unwind`-based partial-state recovery across `.await`
/// points, which is disproportionate to this already-rare (adapter panics
/// at all) x (specifically mid-loop) edge case.
///
/// **`failure_state` on error** (H1 in the Opus Tier-2 review): an
/// eviction failure or an explicit `adapter.load` rejection means the
/// adapter almost certainly never allocated anything for `id` — settling
/// on `Error` there would occupy the budget forever for a model that was
/// never actually resident, and a *different* future load could then fail
/// with a misleading `eviction_impossible` (no viable plan) when the real
/// problem is a stuck, never-cleaned-up ledger entry. Those two cases
/// settle on `Stopped` instead. Only a `probe_ready` failure/timeout is
/// genuinely ambiguous (the `load` call itself DID succeed) — there, a
/// best-effort `unload` is attempted first; `Stopped` if that confirms
/// release, `Error` (occupying budget, matching `occupies_budget`'s
/// documented conservatism) only if even that fails.
async fn run_load_flow(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    id: String,
    policy: LoadPolicy,
    evict: Vec<String>,
) -> OpOutcome {
    // Every chosen victim gets an actual unload attempt, even after an
    // earlier one fails: `OpDone` only resolves ids present in
    // `victim_results`, so stopping early would leave later victims stuck
    // in `Stopping` forever (occupying budget with no in-flight op to ever
    // resolve them) instead of landing on `Stopped`/`Error` like the rest.
    // H2 (Opus Tier-2 review): captured *before* any victim is unloaded,
    // so the confirmation below has a real baseline to compare against —
    // "did used_gb actually drop", not just "adapter.unload() returned
    // Ok" (which is only ever an estimate of what really happened).
    let used_gb_before_eviction = if evict.is_empty() {
        None
    } else {
        with_adapter_timeout(adapter.status(), config.adapter_call_timeout, &id)
            .await
            .ok()
            .map(|s| s.used_gb)
    };
    let mut victim_results: Vec<VictimResult> = Vec::with_capacity(evict.len());
    let mut eviction_failed = false;
    for victim in evict {
        let result = with_adapter_timeout(
            adapter.unload(&victim),
            config.adapter_call_timeout,
            &victim,
        )
        .await;
        eviction_failed |= result.is_err();
        victim_results.push((victim, result));
    }
    if eviction_failed {
        return OpOutcome::Load {
            result: Err(BackendError::eviction_failed(&id)),
            victim_results,
            failure_state: ModelState::Stopped,
        };
    }
    // Confirm before proceeding to `load`, not after: the whole point is
    // that the new model's admission was predicated on this freed
    // capacity actually existing, not merely on `unload()` having returned
    // `Ok`.
    if let Some(before) = used_gb_before_eviction {
        confirm_memory_released(&adapter, before, &config).await;
    }

    // OOM circuit breaker: exactly one self-healing retry, never more.
    let mut attempt = with_adapter_timeout(
        adapter.load(&id, Some(&policy)),
        config.adapter_call_timeout,
        &id,
    )
    .await;
    if let Err(err) = &attempt
        && err.is_oom()
    {
        // H2: confirm whatever the OOM'd attempt may have partially
        // allocated is actually released, then back off, before retrying
        // — retrying immediately into the same memory pressure is likely
        // to just OOM again.
        let used_gb_before_retry =
            with_adapter_timeout(adapter.status(), config.adapter_call_timeout, &id)
                .await
                .ok()
                .map(|s| s.used_gb);
        if let Some(before) = used_gb_before_retry {
            confirm_memory_released(&adapter, before, &config).await;
        }
        tokio::time::sleep(config.oom_retry_backoff).await;
        attempt = with_adapter_timeout(
            adapter.load(&id, Some(&policy)),
            config.adapter_call_timeout,
            &id,
        )
        .await;
    }
    if let Err(err) = attempt {
        return OpOutcome::Load {
            result: Err(err),
            victim_results,
            // `adapter.load` itself explicitly rejected the request — no
            // real allocation to have happened.
            failure_state: ModelState::Stopped,
        };
    }

    for _ in 0..config.probe_max_attempts {
        match with_adapter_timeout(adapter.probe_ready(&id), config.adapter_call_timeout, &id).await
        {
            Ok(true) => {
                return OpOutcome::Load {
                    result: Ok(()),
                    victim_results,
                    failure_state: ModelState::Error, // unused: `result` is `Ok`
                };
            }
            Ok(false) => tokio::time::sleep(config.probe_interval).await,
            Err(err) => {
                return OpOutcome::Load {
                    result: Err(err),
                    victim_results,
                    failure_state: best_effort_release(&adapter, &id, config.adapter_call_timeout)
                        .await,
                };
            }
        }
    }
    OpOutcome::Load {
        result: Err(BackendError::probe_timed_out(id.clone())),
        victim_results,
        failure_state: best_effort_release(&adapter, &id, config.adapter_call_timeout).await,
    }
}

/// After a `probe_ready` failure/timeout, `id`'s real state is ambiguous —
/// `adapter.load` itself succeeded, so something may genuinely be
/// resident. A best-effort `unload` resolves it: `Stopped` if that
/// confirms release, `Error` (still occupying budget, matching
/// `occupies_budget`'s documented conservatism) only if even that fails.
async fn best_effort_release(
    adapter: &Arc<dyn RuntimeAdapter>,
    id: &str,
    timeout: std::time::Duration,
) -> ModelState {
    match with_adapter_timeout(adapter.unload(id), timeout, id).await {
        Ok(()) => ModelState::Stopped,
        Err(_) => ModelState::Error,
    }
}

/// Bundles what dispatch helpers need but never mutate (clippy arg-count).
struct Env<'a> {
    adapter: &'a Arc<dyn RuntimeAdapter>,
    config: &'a SupervisorConfig,
    self_tx: &'a mpsc::Sender<ActorMsg>,
}

/// `models` as [`crate::eviction::plan_eviction`] sees it — every entry
/// *except* `exclude` (the id being requested; see [`Snapshot`]'s doc
/// comment on why it must not describe itself).
fn build_snapshot(models: &HashMap<String, ModelSlot>, budget_gb: f64, exclude: &str) -> Snapshot {
    let entries = models
        .iter()
        .filter(|(id, _)| id.as_str() != exclude)
        .map(|(id, slot)| ModelEntry {
            id: id.clone(),
            memory_gb: slot.memory_gb,
            state: slot.state,
            pinned: matches!(
                slot.policy.keepalive,
                idoris_contracts::load_policy::Keepalive::Pinned { pinned: true }
            ),
            last_used_seq: slot.last_used_seq,
            inflight: slot.inflight,
        })
        .collect();
    Snapshot {
        budget_gb,
        models: entries,
    }
}

/// `InsufficientCapacity` is a real, expected admission outcome
/// (`eviction_impossible`); the other two variants mean the Supervisor
/// handed `plan_eviction` a contract-violating input of its own making
/// (`budget_gb` is validated at `spawn`, and `build_snapshot` always
/// excludes `id`) — an invariant violation, not a normal rejection.
fn map_plan_error(err: PlanEvictionError, id: &str) -> BackendError {
    match err {
        PlanEvictionError::InsufficientCapacity { .. } => BackendError::eviction_impossible(id),
        PlanEvictionError::InvalidCapacity { field, value } => {
            BackendError::internal(format!("invalid capacity for {field}: {value}"))
        }
        PlanEvictionError::SnapshotContainsRequestedModel { .. } => {
            panic!("Supervisor invariant violated: snapshot for {id:?} contained itself")
        }
    }
}

fn start_load(
    id: String,
    policy: LoadPolicy,
    evict: Vec<String>,
    models: &mut HashMap<String, ModelSlot>,
    env: &Env<'_>,
) {
    set_state_or_panic(models, &id, ModelState::Loading);
    let adapter = env.adapter.clone();
    let config = env.config.clone();
    let self_tx = env.self_tx.clone();
    let id_for_task = id.clone();
    tokio::spawn(async move {
        // `run_load_flow` runs as its OWN spawned task so a panic inside it
        // (an adapter implementation bug) is isolated to that task — tokio
        // reports it here as `Err(JoinError)` on `.await` rather than
        // unwinding straight through this outer task and skipping the
        // `OpDone` send below. Without this, a single panicking adapter
        // call would leave `active_op` set forever (llama-swap Issue #946).
        let inner = tokio::spawn(run_load_flow(
            adapter,
            config,
            id_for_task.clone(),
            policy,
            evict,
        ));
        let outcome = match inner.await {
            Ok(outcome) => outcome,
            Err(join_err) => OpOutcome::Load {
                result: Err(BackendError::adapter_panicked(
                    &id_for_task,
                    join_err.to_string(),
                )),
                victim_results: Vec::new(),
                // A panic mid-flow leaves real state genuinely unknown —
                // conservative like a failed best-effort release, not the
                // "confirmed nothing happened" case.
                failure_state: ModelState::Error,
            },
        };
        let _ = self_tx
            .send(ActorMsg::OpDone {
                id: id_for_task,
                outcome,
            })
            .await;
    });
}

/// Handles `Command::Load`, the single decision point every load path
/// funnels through (§4), now backed by [`plan_eviction`] for a real
/// capacity check; no queueing yet — a different id than the active op
/// fails fast with `BackendError::Busy`.
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

    let snapshot = build_snapshot(models, env.config.budget_gb, &id);
    let evict = match plan_eviction(
        &snapshot,
        ModelReq {
            id: id.clone(),
            memory_gb,
        },
    ) {
        Ok(EvictionPlan::NotNeeded) => Vec::new(),
        Ok(EvictionPlan::Evict(victims)) => victims,
        Err(err) => {
            let _ = reply.send(Err(map_plan_error(err, &id)));
            return;
        }
    };
    for victim in &evict {
        set_state_or_panic(models, victim, ModelState::Stopping);
    }

    let last_used_seq = models.get(&id).map_or(0, |s| s.last_used_seq);
    // Carried forward, not reset to 0: a policy-changing reload of a
    // currently-Ready model with chats still in flight against its
    // previous instance must not let `ChatDone` underflow the fresh
    // slot's counter once those in-flight calls finish (see
    // `ActorMsg::ChatDone`'s `saturating_sub`, the other half of this
    // safety net). Blocking such a reload until drained is a further
    // improvement left for later — not required to avoid the underflow.
    let inflight = models.get(&id).map_or(0, |s| s.inflight);
    models.insert(
        id.clone(),
        ModelSlot {
            memory_gb,
            state: ModelState::Launching,
            last_used_seq,
            policy,
            inflight,
        },
    );
    *active_op = Some(ActiveOp {
        id: id.clone(),
        kind: ActiveKind::Load {
            policy,
            waiters: vec![reply],
        },
    });
    start_load(id, policy, evict, models, env);
}

/// The pure-IO side of a standalone `unload`. Unlike an eviction's own
/// `unload`s (see [`confirm_memory_released`] in `run_load_flow`), nothing
/// here is waiting on the freed capacity being real — a bare `unload`
/// command has no follow-on `load` whose admission depended on it — so
/// there is nothing to poll-confirm before proceeding.
fn start_unload(id: String, env: &Env<'_>) {
    let adapter = env.adapter.clone();
    let self_tx = env.self_tx.clone();
    let timeout = env.config.adapter_call_timeout;
    tokio::spawn(async move {
        // Same panic-isolation shape as `start_load` — see its doc comment.
        let id_for_inner = id.clone();
        let inner = tokio::spawn(async move {
            with_adapter_timeout(adapter.unload(&id_for_inner), timeout, &id_for_inner).await
        });
        let result = match inner.await {
            Ok(result) => result,
            Err(join_err) => Err(BackendError::adapter_panicked(&id, join_err.to_string())),
        };
        let _ = self_tx
            .send(ActorMsg::OpDone {
                id,
                outcome: OpOutcome::Unload { result },
            })
            .await;
    });
}

/// Handles `Command::Unload`, sharing `handle_load`'s global mutex and
/// singleflight discipline: concurrent unloads of the same id merge, an
/// unrelated id in flight fails fast with `Busy`, and an already-`Stopped`
/// id is a no-op checked before the busy-fallback (same ordering lesson as
/// `handle_load`'s already-Ready check).
fn handle_unload(
    id: String,
    reply: LoadReply,
    models: &mut HashMap<String, ModelSlot>,
    active_op: &mut Option<ActiveOp>,
    env: &Env<'_>,
) {
    if let Some(active) = active_op.as_mut()
        && active.id == id
    {
        match &mut active.kind {
            ActiveKind::Unload { waiters, .. } => waiters.push(reply),
            ActiveKind::Load { .. } => {
                let _ = reply.send(Err(BackendError::busy()));
            }
        }
        return;
    }
    // Decided without the mutex, and before it, so neither answer can be
    // masked by an unrelated id's `Busy` (mirrors `handle_load`'s ordering
    // lesson: a decidable-without-IO fact must not be hidden behind it).
    match models.get(&id).map(|slot| slot.state) {
        None => {
            let _ = reply.send(Err(BackendError::model_not_found(&id)));
            return;
        }
        Some(ModelState::Stopped) => {
            let _ = reply.send(Ok(()));
            return;
        }
        Some(_) => {}
    }
    if active_op.is_some() {
        let _ = reply.send(Err(BackendError::busy()));
        return;
    }
    // Setting `Stopping` here already blocks *new* chats (the `Chat`
    // handler's `slot.state != Ready` check) — but any chats already
    // dispatched must be allowed to finish before the adapter is actually
    // told to unload. If any are in flight, defer `start_unload` to
    // `ActorMsg::ChatDone`, which will call it once `inflight` reaches 0.
    set_state_or_panic(models, &id, ModelState::Stopping);
    let inflight = models.get(&id).map_or(0, |s| s.inflight);
    *active_op = Some(ActiveOp {
        id: id.clone(),
        kind: ActiveKind::Unload {
            waiters: vec![reply],
            started: inflight == 0,
        },
    });
    if inflight == 0 {
        start_unload(id, env);
    }
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
                let timeout = config.adapter_call_timeout;
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = reply.send(with_adapter_timeout(adapter.list(), timeout, "list").await);
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
                        slot.inflight += 1;
                    }
                    let adapter = adapter.clone();
                    let timeout = config.adapter_call_timeout;
                    let self_tx = self_tx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let model = req.model.clone();
                        let _ = reply.send(
                            with_adapter_timeout(adapter.chat(req, cancel), timeout, &model).await,
                        );
                        let _ = self_tx.send(ActorMsg::ChatDone { model }).await;
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

            ActorMsg::Cmd(Command::Unload { id, reply }) => {
                handle_unload(id, reply, &mut models, &mut active_op, &env);
            }

            ActorMsg::OpDone { id, outcome } => {
                let (result, done_state, failure_state, victim_results) = match outcome {
                    OpOutcome::Load {
                        result,
                        victim_results,
                        failure_state,
                    } => (result, ModelState::Ready, failure_state, victim_results),
                    OpOutcome::Unload { result } => {
                        (result, ModelState::Stopped, ModelState::Error, Vec::new())
                    }
                };
                for (victim_id, victim_result) in victim_results {
                    let victim_state = if victim_result.is_ok() {
                        ModelState::Stopped
                    } else {
                        ModelState::Error
                    };
                    set_state_or_panic(&mut models, &victim_id, victim_state);
                }
                match &result {
                    Ok(()) => {
                        next_seq += 1;
                        if let Some(slot) = models.get_mut(&id) {
                            slot.state = done_state;
                            if done_state == ModelState::Ready {
                                slot.last_used_seq = next_seq;
                            }
                        } else {
                            panic!("Supervisor invariant violated: ledger lost {id:?} mid-op");
                        }
                    }
                    Err(_) => set_state_or_panic(&mut models, &id, failure_state),
                }
                let waiters = match active_op.take() {
                    Some(ActiveOp {
                        kind: ActiveKind::Load { waiters, .. } | ActiveKind::Unload { waiters, .. },
                        ..
                    }) => waiters,
                    None => panic!("Supervisor invariant violated: OpDone with no active op"),
                };
                for waiter in waiters {
                    let _ = waiter.send(result.clone());
                }
            }

            ActorMsg::ChatDone { model } => {
                // Ledger entries are only ever transitioned, never removed
                // (the same invariant `set_state_or_panic` enforces for
                // `state`), so a `ChatDone` for an id `chat` was actually
                // dispatched to must find an entry — a missing one means
                // that discipline was violated somewhere.
                let inflight = match models.get_mut(&model) {
                    Some(slot) => {
                        slot.inflight = slot.inflight.saturating_sub(1);
                        slot.inflight
                    }
                    None => {
                        panic!("Supervisor invariant violated: ledger has no entry for {model:?}")
                    }
                };
                if inflight == 0
                    && let Some(active) = active_op.as_mut()
                    && active.id == model
                    && let ActiveKind::Unload { started, .. } = &mut active.kind
                    && !*started
                {
                    *started = true;
                    start_unload(model, &env);
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

    fn on_demand_policy() -> LoadPolicy {
        LoadPolicy {
            mode: idoris_contracts::load_policy::LoadMode::OnDemand,
            keepalive: idoris_contracts::load_policy::Keepalive::IdleTtl { idle_ttl_s: 60 },
            admission: idoris_contracts::load_policy::Admission::Coexist,
        }
    }

    fn resident_policy() -> LoadPolicy {
        LoadPolicy {
            mode: idoris_contracts::load_policy::LoadMode::Resident,
            keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
            admission: idoris_contracts::load_policy::Admission::Coexist,
        }
    }

    fn two_model_catalog() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "a".to_string(),
                memory_gb: 4.0,
            },
            ModelInfo {
                id: "b".to_string(),
                memory_gb: 4.0,
            },
        ]
    }

    #[tokio::test]
    async fn load_then_chat_succeeds() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("load should succeed");
        let resp = handle
            .chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .expect("chat after a successful load should succeed");
        assert_eq!(resp.model, "a");
    }

    /// Singleflight: two concurrent loads of the same id with an *identical*
    /// policy must merge into exactly one adapter call.
    #[tokio::test(start_paused = true)]
    async fn concurrent_identical_policy_loads_merge_into_one_adapter_call() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(50));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let h2 = handle.clone();
        let f1 = tokio::spawn(async move { handle.load("a", 4.0, on_demand_policy()).await });
        let f2 = tokio::spawn(async move { h2.load("a", 4.0, on_demand_policy()).await });
        f1.await.unwrap().expect("first load should succeed");
        f2.await.unwrap().expect("merged load should succeed");
        assert_eq!(adapter.load_call_count("a"), 1);
    }

    /// Negative contrast: a *different* policy for the same id must not be
    /// silently merged — it fails fast with `Busy` instead (queueing lands
    /// in a follow-up PR).
    #[tokio::test(start_paused = true)]
    async fn concurrent_different_policy_loads_do_not_merge() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(50));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let h2 = handle.clone();
        let f1 = tokio::spawn(async move { handle.load("a", 4.0, on_demand_policy()).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let err = h2
            .load("a", 4.0, resident_policy())
            .await
            .expect_err("a different policy for the same in-flight id must not merge");
        assert_eq!(err.reason_code(), "supervisor_busy");
        f1.await
            .unwrap()
            .expect("the original load should still succeed");
    }

    /// OOM circuit breaker: exactly one self-healing retry.
    #[tokio::test(start_paused = true)]
    async fn oom_self_heals_with_exactly_one_retry() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_script(
            "a",
            vec![crate::mock::LoadOutcome::Oom, crate::mock::LoadOutcome::Ok],
        );
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("load should self-heal after exactly one OOM retry");
        assert_eq!(adapter.load_call_count("a"), 2);
    }

    /// Negative contrast: a *second* OOM must not trigger a third attempt.
    #[tokio::test(start_paused = true)]
    async fn a_second_oom_is_not_retried_again() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_script(
            "a",
            vec![
                crate::mock::LoadOutcome::Oom,
                crate::mock::LoadOutcome::Oom,
                crate::mock::LoadOutcome::Ok,
            ],
        );
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let err = handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("a second OOM must surface, not be retried a third time");
        assert_eq!(err.reason_code(), "oom");
        assert_eq!(adapter.load_call_count("a"), 2);
    }

    /// A different id than whatever is currently active fails fast — no
    /// queueing yet (see the module doc comment).
    #[tokio::test(start_paused = true)]
    async fn a_different_id_is_busy_while_one_load_is_in_flight() {
        let adapter = Arc::new(MockAdapter::new(two_model_catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(200));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        let h2 = handle.clone();
        let load_a = tokio::spawn(async move { h2.load("a", 4.0, on_demand_policy()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let err = handle
            .load("b", 4.0, on_demand_policy())
            .await
            .expect_err("a different id while a load is active must be busy");
        assert_eq!(err.reason_code(), "supervisor_busy");
        load_a
            .await
            .unwrap()
            .expect("load a should eventually succeed");
    }

    /// Negative contrast: an already-`Ready` id with a matching policy must
    /// stay a no-op even while an *unrelated* id is mid-load — confirming
    /// the fix in this PR (an earlier version wrongly reported `Busy` here).
    #[tokio::test(start_paused = true)]
    async fn already_ready_load_is_noop_even_while_another_id_loads() {
        let adapter = Arc::new(MockAdapter::new(two_model_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("initial load of a should succeed");
        adapter.set_load_delay("b", std::time::Duration::from_millis(200));
        let h2 = handle.clone();
        let load_b = tokio::spawn(async move { h2.load("b", 4.0, on_demand_policy()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("already-Ready a with a matching policy must stay a no-op");
        assert_eq!(adapter.load_call_count("a"), 1);
        load_b
            .await
            .unwrap()
            .expect("load b should eventually succeed");
    }

    #[tokio::test]
    async fn unload_after_load_then_chat_becomes_unavailable() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("load should succeed");
        handle.unload("a").await.expect("unload should succeed");
        assert_eq!(adapter.unload_call_count("a"), 1);
        let err = handle
            .chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .expect_err("chat after unload must fail");
        assert_eq!(err.reason_code(), "model_unavailable");
    }

    /// Negative contrast: unloading an id that was never loaded is
    /// `model_not_found`, not silently accepted or confused with `Busy`.
    #[tokio::test]
    async fn unload_of_an_unknown_id_is_not_found() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        let err = handle
            .unload("a")
            .await
            .expect_err("unload before any load must fail");
        assert_eq!(err.reason_code(), "model_not_found");
    }

    /// A second unload of an already-`Stopped` model is a no-op: no second
    /// adapter call, no error.
    #[tokio::test]
    async fn a_second_unload_of_an_already_stopped_model_is_a_noop() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("load should succeed");
        handle
            .unload("a")
            .await
            .expect("first unload should succeed");
        handle
            .unload("a")
            .await
            .expect("second unload of an already-Stopped model must be a no-op");
        assert_eq!(adapter.unload_call_count("a"), 1);
    }

    /// Singleflight: concurrent unloads of the same id merge into one
    /// adapter call.
    #[tokio::test(start_paused = true)]
    async fn concurrent_unloads_of_the_same_id_merge_into_one_adapter_call() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_unload_delay("a", std::time::Duration::from_millis(50));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("load should succeed");
        let h2 = handle.clone();
        let f1 = tokio::spawn(async move { handle.unload("a").await });
        let f2 = tokio::spawn(async move { h2.unload("a").await });
        f1.await.unwrap().expect("first unload should succeed");
        f2.await.unwrap().expect("merged unload should succeed");
        assert_eq!(adapter.unload_call_count("a"), 1);
    }

    /// Negative contrast: unloading a genuinely unknown id must stay
    /// `model_not_found` even while an *unrelated* id is mid-load — that
    /// fact is decidable without the mutex and must not be masked as
    /// `Busy` (mirrors the fix in #85 for `load`'s already-Ready check).
    #[tokio::test(start_paused = true)]
    async fn unknown_id_unload_is_not_found_even_while_another_id_loads() {
        let adapter = Arc::new(MockAdapter::new(two_model_catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(200));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        let h2 = handle.clone();
        let load_a = tokio::spawn(async move { h2.load("a", 4.0, on_demand_policy()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let err = handle
            .unload("nonexistent")
            .await
            .expect_err("unloading a genuinely unknown id must be not_found, not busy");
        assert_eq!(err.reason_code(), "model_not_found");
        load_a
            .await
            .unwrap()
            .expect("load a should eventually succeed");
    }

    fn evictable_catalog() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "a".to_string(),
                memory_gb: 20.0,
            },
            ModelInfo {
                id: "b".to_string(),
                memory_gb: 20.0,
            },
        ]
    }

    fn tight_budget_config() -> SupervisorConfig {
        SupervisorConfig {
            budget_gb: 24.0,
            ..SupervisorConfig::default()
        }
    }

    /// LRU eviction frees exactly enough capacity to admit the new model.
    #[tokio::test]
    async fn eviction_frees_capacity_for_the_new_model() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect("load b should succeed after evicting a");
        assert_eq!(adapter.unload_call_count("a"), 1);
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(status.loaded, vec!["b".to_string()]);
    }

    /// Negative contrast: a pinned model is never chosen as a victim — the
    /// load fails outright instead of silently evicting it anyway.
    #[tokio::test]
    async fn pinned_model_is_not_evicted_load_fails_instead() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, resident_policy())
            .await
            .expect("pinned load of a should succeed");
        let err = handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect_err("b cannot be admitted: a is pinned, nothing evictable");
        assert_eq!(err.reason_code(), "eviction_impossible");
        assert_eq!(adapter.unload_call_count("a"), 0);
    }

    /// A request that would not fit even after evicting everything
    /// evictable is rejected before ever calling the adapter.
    #[tokio::test]
    async fn a_request_too_big_for_the_budget_is_rejected_without_calling_the_adapter() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        let err = handle
            .load("a", 100.0, on_demand_policy())
            .await
            .expect_err("100 GiB can never fit a 24 GiB budget");
        assert_eq!(err.reason_code(), "eviction_impossible");
        assert_eq!(adapter.load_call_count("a"), 0);
    }

    /// If a chosen victim's `unload` itself fails, the whole load fails
    /// with a distinct `eviction_failed` reason code (not the planning-time
    /// `eviction_impossible`), the new model is never admitted, and the
    /// failure does not block unrelated follow-up operations.
    #[tokio::test]
    async fn a_failed_eviction_reports_eviction_failed_and_admits_nothing() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        adapter.set_unload_script("a", vec![crate::mock::UnloadOutcome::Fail]);
        let err = handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect_err("b must not be admitted when evicting a fails");
        assert_eq!(err.reason_code(), "eviction_failed");
        let status = handle.status().await.expect("status should succeed");
        assert!(!status.loaded.contains(&"b".to_string()));
        // The failure must not leave the Supervisor stuck: a's ledger entry
        // was resolved (not left dangling), so a plain follow-up unload
        // (this time succeeding, since the one-shot script is spent) works.
        handle
            .unload("a")
            .await
            .expect("a follow-up unload of a must still work after the failed eviction");
    }

    /// Nothing interleaves with an in-flight load that is evicting a
    /// victim: a concurrent request touching the victim while it's mid
    /// -eviction fails fast with `Busy`, the same as any other unrelated
    /// id would.
    #[tokio::test(start_paused = true)]
    async fn nothing_interleaves_with_an_in_flight_eviction() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        adapter.set_load_delay("b", std::time::Duration::from_millis(200));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        let h2 = handle.clone();
        let load_b = tokio::spawn(async move { h2.load("b", 20.0, on_demand_policy()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let err = handle
            .unload("a")
            .await
            .expect_err("a is mid-eviction (Stopping): touching it must not interleave");
        assert_eq!(err.reason_code(), "supervisor_busy");
        load_b
            .await
            .unwrap()
            .expect("load b should eventually succeed");
    }

    /// A minimal `RuntimeAdapter` whose `load` always panics — for
    /// exercising the panic-isolation path in `start_load` without adding
    /// panic-injection scripting to `MockAdapter` itself.
    struct PanickingAdapter;

    #[async_trait::async_trait]
    impl RuntimeAdapter for PanickingAdapter {
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            panic!("PanickingAdapter::load always panics (test double)");
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            Ok(())
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            Ok(BackendStatus {
                pressure: Pressure::Ok,
                used_gb: 0.0,
                model_memory_max_gb: 0.0,
                loaded: Vec::new(),
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
            Ok(ChatResponse {
                model: req.model,
                content: String::new(),
            })
        }
    }

    /// C1: a panicking adapter call must not leave `active_op` set forever
    /// — the caller gets `adapter_panicked` instead of hanging, and the
    /// Supervisor accepts further requests right after (this is exactly
    /// llama-swap Issue #946's failure mode).
    #[tokio::test]
    async fn a_panicking_load_reports_adapter_panicked_and_does_not_wedge_the_supervisor() {
        let adapter = Arc::new(PanickingAdapter);
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        let err = handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("a panicking adapter call must surface as an error, not hang");
        assert_eq!(err.reason_code(), "adapter_panicked");
        // Negative contrast baked into the same test: if the panic *had*
        // wedged the Supervisor (active_op stuck `Some` forever), this
        // second, unrelated load would also fail with `Busy` — it must not.
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("still fails (PanickingAdapter always panics)");
    }

    /// C1: a single adapter call that never returns must not hold
    /// `active_op` forever — `adapter_call_timeout` cuts it off, and the
    /// Supervisor keeps serving other requests afterward.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_load_times_out_and_does_not_wedge_the_supervisor() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        // Far longer than the adapter_call_timeout below: from the
        // Supervisor's point of view this is indistinguishable from a
        // permanent hang.
        adapter.set_load_delay("a", std::time::Duration::from_secs(3600));
        let handle = Supervisor::spawn(
            adapter,
            SupervisorConfig {
                adapter_call_timeout: std::time::Duration::from_millis(50),
                ..SupervisorConfig::default()
            },
        )
        .expect("spawn should succeed");
        let err = handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("a hanging adapter call must time out, not hang forever");
        assert_eq!(err.reason_code(), "adapter_timed_out");
        // Negative contrast: the Supervisor must still be usable afterward
        // — a stuck `active_op` would make this also fail with `Busy`.
        let models = handle.list().await.expect("list must still work");
        assert_eq!(models, catalog());
    }

    /// C2: a model with an in-flight `chat` is never chosen as an
    /// eviction victim, even when it would otherwise be the LRU pick —
    /// unloading it mid-`chat` would abort real, in-progress work.
    #[tokio::test(start_paused = true)]
    async fn inflight_chats_exclude_a_model_from_eviction() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        adapter.set_chat_delay("a", std::time::Duration::from_millis(200));
        let h2 = handle.clone();
        let chat_a = tokio::spawn(async move {
            h2.chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let err = handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect_err("a has an in-flight chat: nothing evictable, b cannot be admitted");
        assert_eq!(err.reason_code(), "eviction_impossible");
        assert_eq!(adapter.unload_call_count("a"), 0);
        chat_a
            .await
            .unwrap()
            .expect("the in-flight chat should still complete normally");
    }

    /// C2 + M4: an explicit `unload` defers the actual adapter call until
    /// in-flight `chat`s drain to 0, but immediately blocks *new* ones
    /// (`Stopping` -> `model_unavailable`) — the drain lesson also covers
    /// the eviction case, since both go through the same `Stopping` state.
    #[tokio::test(start_paused = true)]
    async fn unload_drains_inflight_chats_before_calling_the_adapter() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        adapter.set_chat_delay("a", std::time::Duration::from_millis(200));
        let h2 = handle.clone();
        let chat_a = tokio::spawn(async move {
            h2.chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let h3 = handle.clone();
        let unload_a = tokio::spawn(async move { h3.unload("a").await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            adapter.unload_call_count("a"),
            0,
            "unload must not call the adapter while a chat is still in flight"
        );
        let err = handle
            .chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .expect_err("a new chat during the drain (Stopping) must be rejected");
        assert_eq!(err.reason_code(), "model_unavailable");
        chat_a
            .await
            .unwrap()
            .expect("the original in-flight chat should still complete");
        unload_a
            .await
            .unwrap()
            .expect("unload should complete once the drain finishes");
        assert_eq!(adapter.unload_call_count("a"), 1);
    }

    /// H1: an explicit `adapter.load` rejection must not occupy the ledger
    /// forever — settling on `Stopped` (not `Error`) so it doesn't keep
    /// hoarding budget a later, unrelated load might need.
    #[tokio::test]
    async fn a_load_failure_does_not_occupy_the_ledger() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_script("a", vec![crate::mock::LoadOutcome::Fail]);
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("the scripted Fail outcome must surface as an error");
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(
            status.used_gb, 0.0,
            "a rejected load must not occupy budget"
        );
    }

    /// A minimal `RuntimeAdapter`: `load` always succeeds but
    /// `probe_ready` always fails — exercises H1's "probe fails, but a
    /// best-effort `unload` confirms release" path. `unload_ok` controls
    /// whether that best-effort release itself succeeds.
    struct ProbeAlwaysFailsAdapter {
        unload_ok: bool,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for ProbeAlwaysFailsAdapter {
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Ok(())
        }
        async fn unload(&self, id: &str) -> Result<(), BackendError> {
            if self.unload_ok {
                Ok(())
            } else {
                Err(BackendError::Upstream {
                    message: format!(
                        "ProbeAlwaysFailsAdapter refuses to unload {id} (test double)"
                    ),
                })
            }
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            Ok(BackendStatus {
                pressure: Pressure::Ok,
                used_gb: 0.0,
                model_memory_max_gb: 0.0,
                loaded: Vec::new(),
            })
        }
        async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
            Err(BackendError::Upstream {
                message: format!(
                    "ProbeAlwaysFailsAdapter's probe for {id} always fails (test double)"
                ),
            })
        }
        async fn chat(
            &self,
            req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, BackendError> {
            Ok(ChatResponse {
                model: req.model,
                content: String::new(),
            })
        }
    }

    /// H1: a `probe_ready` failure that a best-effort `unload` *confirms*
    /// released must settle on `Stopped`, not the conservative `Error`.
    #[tokio::test]
    async fn probe_failure_confirmed_released_does_not_occupy_the_ledger() {
        let adapter = Arc::new(ProbeAlwaysFailsAdapter { unload_ok: true });
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("probe_ready always fails, so load must fail too");
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(
            status.used_gb, 0.0,
            "a confirmed-released model must not occupy budget"
        );
    }

    /// Negative contrast: the same probe failure, but the best-effort
    /// `unload` *also* fails — real state is genuinely unknown, so it must
    /// stay `Error` (occupying budget), never guessed as free.
    #[tokio::test]
    async fn probe_failure_with_failed_release_still_occupies_the_ledger() {
        let adapter = Arc::new(ProbeAlwaysFailsAdapter { unload_ok: false });
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("probe_ready always fails, so load must fail too");
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(
            status.used_gb, 4.0,
            "an unconfirmed-release model must stay conservatively occupying budget"
        );
    }

    /// A minimal `RuntimeAdapter` whose `status().used_gb` only drops after
    /// `calls_until_drop` calls — for proving H2's confirmation actually
    /// polls (multiple `status` calls) rather than trusting the first
    /// reading (or, worse, just trusting `unload()`'s own `Ok`).
    struct LaggingReleaseAdapter {
        status_calls: std::sync::Mutex<u32>,
        calls_until_drop: u32,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for LaggingReleaseAdapter {
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(evictable_catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Ok(())
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            Ok(())
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            let mut calls = self
                .status_calls
                .lock()
                .expect("test mutex is never poisoned");
            *calls += 1;
            let used_gb = if *calls >= self.calls_until_drop {
                0.0
            } else {
                100.0
            };
            Ok(BackendStatus {
                pressure: Pressure::Ok,
                used_gb,
                model_memory_max_gb: 100.0,
                loaded: Vec::new(),
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
            Ok(ChatResponse {
                model: req.model,
                content: String::new(),
            })
        }
    }

    /// H2: an eviction's memory-release confirmation genuinely polls
    /// `status()` until it reflects the drop, not just once.
    #[tokio::test(start_paused = true)]
    async fn eviction_confirms_memory_release_by_polling_status() {
        let adapter = Arc::new(LaggingReleaseAdapter {
            status_calls: std::sync::Mutex::new(0),
            calls_until_drop: 4,
        });
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect("load b should succeed once release is confirmed");
        let calls = *adapter
            .status_calls
            .lock()
            .expect("test mutex is never poisoned");
        assert!(
            calls >= 4,
            "confirmation must actually poll status() until it drops, not trust the first read (got {calls} calls)"
        );
    }

    /// Negative contrast: if `status()` never reflects the drop (the
    /// confirmation can never succeed), the load must still proceed once
    /// `release_confirm_max_attempts` is exhausted — falling back to the
    /// caller's own estimate rather than blocking forever.
    #[tokio::test(start_paused = true)]
    async fn eviction_proceeds_after_confirmation_gives_up() {
        let adapter = Arc::new(LaggingReleaseAdapter {
            status_calls: std::sync::Mutex::new(0),
            calls_until_drop: u32::MAX, // never drops
        });
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect("load b must still proceed once confirmation gives up, not hang forever");
    }
}
