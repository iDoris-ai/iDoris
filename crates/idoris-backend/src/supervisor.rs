//! [`Supervisor`] — the single-writer event loop that owns every model's
//! lifecycle for one [`RuntimeAdapter`]. Per
//! `docs/research/Rust基础选型-2026-09-27.md` §4 (llama-swap
//! `internal/router/design.md`; the root cause of llama-swap Issue #946):
//! **one tokio task holds all state**; every other task talks to it only
//! through [`SupervisorHandle`]'s mpsc-backed commands.
//!
//! `load`/`unload` share one global mutex (`active_op`): same-id,
//! same-request concurrent calls singleflight-merge; a different id (or a
//! same-id call that differs in policy/`memory_gb`) fails fast with
//! `BackendError::Busy` — there is no real wait queue yet, a deliberate
//! simplification. Every `load` is checked against
//! [`crate::eviction::plan_eviction`] for real capacity, evicting chosen
//! victims (in-flight-aware, never a model mid-`chat`) before the new
//! model is admitted. Every adapter call is timeout-bounded and
//! panic-isolated (see [`with_adapter_timeout`]); a detected internal
//! invariant violation poisons the actor into failing closed rather than
//! panicking the whole event loop.

use std::collections::HashMap;
use std::sync::Arc;

use idoris_contracts::LoadPolicy;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::adapter::RuntimeAdapter;
use crate::error::BackendError;
use crate::eviction::{
    CAPACITY_EPSILON_GB, EvictionPlan, ModelEntry, ModelReq, ModelState, PlanEvictionError,
    Snapshot, occupies_budget, plan_eviction,
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
    /// Monotonic "last used" counter (bumped on load-completion and on
    /// `chat`) — `plan_eviction`'s LRU ordering reads this via
    /// [`build_snapshot`].
    last_used_seq: u64,
    /// Policy last accepted; compared by exact equality to decide merge vs. a new op.
    policy: LoadPolicy,
    /// Number of `chat` calls currently dispatched to this model — see
    /// `ActorMsg::ChatDone`. Excludes it from eviction while nonzero, and
    /// an explicit `unload` defers its actual adapter call until this
    /// drains to 0 (see `handle_unload`).
    inflight: u32,
    /// A load or readiness probe had an unconfirmed outcome. The engine
    /// may still finish allocating after any unload/status response; without
    /// an operation-completion signal the slot cannot safely be released.
    load_unconfirmed: bool,
    /// This slot names a model present in the one startup residency sample.
    /// Its memory is accounted by `reserved_gb` as one aggregate, because
    /// status has no per-model measurements.
    inherited: bool,
}

enum Command {
    List {
        reply: oneshot::Sender<Result<Vec<ModelInfo>, BackendError>>,
    },
    Status {
        reply: oneshot::Sender<Result<BackendStatus, BackendError>>,
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
        /// Fresh aggregate snapshot proving the selected victims were gone.
        aggregate_status: Option<BackendStatus>,
        /// Amount attributed to the requested ID in that pre-load snapshot.
        aggregate_target_credit_gb: f64,
        /// Only consulted when `result` is `Err` — see
        /// [`LoadFailureOutcome`]'s doc comment.
        failure_outcome: LoadFailureOutcome,
    },
    Unload {
        result: Result<(), BackendError>,
        status: Option<BackendStatus>,
    },
}

/// What `OpDone` applies to `id`'s ledger entry when a load's `result` is
/// `Err` (on `Ok`, unused — `handle_load` already put the right
/// policy/memory_gb in the slot for the newly-`Ready` state).
enum LoadFailureOutcome {
    /// Land on this state, keeping whatever policy/memory_gb `handle_load`
    /// already wrote into the slot.
    Settle(ModelState),
    /// Settle after a confirmed release and use its fresh aggregate snapshot
    /// to rebase startup residency accounting.
    SettleReleased(BackendStatus),
    /// A definite rejection leaves the previous instance's state, policy,
    /// and footprint untouched.
    RestorePrevious(PreviousModel),
    /// An uncertain release keeps Error and the larger footprint.
    RetainPreviousOnError(PreviousModel),
    /// The original load may still allocate after this operation returns.
    /// Keep the maximum known footprint and permanently block automatic
    /// release/retry until the adapter can provide completion evidence.
    RetainUnconfirmedLoad(Option<PreviousModel>),
}

#[derive(Clone, Copy)]
struct PreviousModel {
    state: ModelState,
    policy: LoadPolicy,
    memory_gb: f64,
    inherited: bool,
}

impl LoadFailureOutcome {
    /// Unconfirmed release retains the larger of the old and retry estimates.
    /// The state stays Error, never Ready.
    fn after_release(state: ModelState, previous: Option<PreviousModel>) -> Self {
        match previous {
            Some(previous) if occupies_budget(state) => Self::RetainPreviousOnError(previous),
            _ => Self::Settle(state),
        }
    }
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
        /// Compared alongside `policy` (both must match exactly) to decide
        /// singleflight-merge vs. a conflicting concurrent request for the
        /// same id (Low, Opus Tier-2 review) — two callers racing to load
        /// the same id with the same policy but *different* `memory_gb`
        /// must not be silently merged into whichever happened to arrive
        /// first, since the eviction plan (and therefore the outcome
        /// either caller can trust) was built against only one of them.
        memory_gb: f64,
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

#[derive(Clone, Copy)]
struct ReleaseLimits {
    /// Ledger total excluding the requested target and selected victims.
    target_absent_gb: f64,
    /// Same remainder plus the previous target, which may survive eviction.
    after_eviction_gb: f64,
}

struct LoadFlowContext {
    release_limits: ReleaseLimits,
    inherited: bool,
    load_fence: Option<Arc<crate::load_fence::LoadFence>>,
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
        rx.await
            .map_err(|_| BackendError::supervisor_unavailable())?
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
        let (tx, _join) = spawn_actor(adapter, config);
        Ok(SupervisorHandle { tx })
    }
}

/// Builds the channel and spawns the actor task, returning both the
/// command sender and the task's own `JoinHandle`. Kept private and
/// separate from `Supervisor::spawn` (which discards the `JoinHandle`,
/// matching its documented "fire and forget, reachable only via
/// `SupervisorHandle`" contract) purely so a test can assert the task
/// actually exits once every handle is dropped (M1, Opus Tier-2 review)
/// without adding an internal implementation detail to the public API.
fn spawn_actor(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
) -> (mpsc::Sender<ActorMsg>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let call_slots = Arc::new(Semaphore::new(config.max_concurrent_adapter_calls.max(1)));
    // `downgrade`, not `clone`: a *strong* self-clone would mean the actor
    // always holds one live sender on its own channel, so `rx.recv()`
    // could never return `None` — the actor would run forever even after
    // every `SupervisorHandle` is dropped. A `WeakSender` doesn't count
    // toward keeping the channel open, so `rx.recv()` correctly returns
    // `None`, and the loop exits, once the last external handle goes away.
    let self_tx = tx.downgrade();
    let join = tokio::spawn(run_actor(adapter, config, rx, call_slots, self_tx));
    (tx, join)
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

/// Ledger entries are only transitioned, never removed, so every id here is
/// guaranteed present. Used only from `handle_load`/`handle_unload`, where
/// `id` was read from `models` (directly, or via `plan_eviction`'s output —
/// itself built from `models`) *synchronously*, with no `.await` in
/// between — a missing entry here would mean this crate's own logic is
/// self-contradictory in a single, uninterrupted step, not a real runtime
/// race. Panicking (rather than the fail-closed `set_state_or_poison`
/// below, used from `OpDone`/`ChatDone` where a real `.await` gap exists
/// for something else to have gone wrong in) matches Rust's own convention
/// for asserting an invariant provably true by construction.
fn set_state_or_panic(models: &mut HashMap<String, ModelSlot>, id: &str, state: ModelState) {
    match models.get_mut(id) {
        Some(slot) => slot.state = state,
        None => panic!("Supervisor invariant violated: ledger has no entry for {id:?}"),
    }
}

/// The `OpDone`/`ChatDone` counterpart of `set_state_or_panic`: after a
/// real `.await` gap (waiting on a background adapter call), a missing
/// entry is a genuine — if still supposed-to-be-impossible — runtime
/// invariant violation, not a same-step logic contradiction. Returns `Err`
/// instead of panicking (M3, Opus Tier-2 review): panicking here would
/// kill the *entire* actor task over one corrupted entry, taking down
/// every other in-flight and future caller with it, not just the one
/// operation that surfaced the corruption.
fn set_state_or_poison(
    models: &mut HashMap<String, ModelSlot>,
    id: &str,
    state: ModelState,
) -> Result<(), String> {
    match models.get_mut(id) {
        Some(slot) => {
            slot.state = state;
            if state == ModelState::Stopped {
                slot.inherited = false;
            }
            Ok(())
        }
        None => Err(format!("ledger has no entry for {id:?}")),
    }
}

/// Takes and unwraps the current active op's waiters, regardless of
/// whether it was a `Load` or `Unload` — `None` means `active_op` was
/// already empty, which `OpDone`'s caller treats as its own invariant
/// violation (an `OpDone` must always correspond to a real active op).
fn take_waiters(active_op: &mut Option<ActiveOp>) -> Option<Vec<LoadReply>> {
    active_op.take().map(|op| match op.kind {
        ActiveKind::Load { waiters, .. } | ActiveKind::Unload { waiters, .. } => waiters,
    })
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

/// K11/H3: require every target to be absent and enough capacity to remain.
/// Missing/invalid evidence or exhausted polling fails closed.
async fn confirm_memory_released(
    adapter: &Arc<dyn RuntimeAdapter>,
    targets: &[String],
    max_used_gb: f64,
    config: &SupervisorConfig,
) -> Result<BackendStatus, BackendError> {
    for _ in 0..config.release_confirm_max_attempts {
        if let Ok(status) =
            with_adapter_timeout(adapter.status(), config.adapter_call_timeout, "status").await
            && status.used_gb.is_finite()
            && status.used_gb >= 0.0
            && (status.used_gb > 0.0 || status.loaded.is_empty())
            && status.used_gb <= max_used_gb + CAPACITY_EPSILON_GB
            && targets.iter().all(|id| !status.loaded.contains(id))
        {
            return Ok(status);
        }
        tokio::time::sleep(config.release_confirm_interval).await;
    }
    Err(BackendError::internal(
        "memory release could not be confirmed",
    ))
}

/// The pure-IO side of a load: attempts every victim in `evict` (even
/// after an earlier one fails — see the loop below), then, only if all
/// succeeded, calls the adapter and reports the outcome. Any eviction
/// failure means the capacity assumption behind this load no longer
/// holds, so the load itself must not proceed. Every adapter call is
/// timeout-bounded (see [`with_adapter_timeout`]); a panic inside this
/// function is caught by the `tokio::spawn` wrapper in `start_load`, not
/// here — see its doc comment. If the panic lands mid-eviction-loop, this
/// function's local `victim_results` is lost with the panicking stack
/// frame — `start_load`'s `JoinError` arm resolves every chosen victim to
/// `Error` anyway (not distinguishing "already freed" from "never
/// attempted"), so none dangle in `Stopping` forever. Victims can still
/// be explicitly unloaded, but the durable fence blocks new loads until
/// engine termination is established. **Known accepted imprecision**: a victim that
/// actually succeeded right before the panic is still reported `Error`
/// (occupying budget) instead of the more accurate `Stopped` — recovering
/// that needs `catch_unwind`-based partial-state recovery across `.await`
/// points, disproportionate to this already-rare (panics at all) x
/// (specifically mid-loop) edge case.
///
/// **`failure_state` on error** (H1 in the Opus Tier-2 review): an
/// eviction failure or an explicit `adapter.load` rejection means the
/// adapter almost certainly never allocated anything for `id` — settling
/// on `Error` there would occupy the budget forever for a model that was
/// never actually resident, and a *different* future load could then fail
/// with a misleading `eviction_impossible` (no viable plan) when the real
/// problem is a stuck, never-cleaned-up ledger entry. Those two cases
/// settle a fresh load on `Stopped`, or restore a retry's previous slot.
/// A load timeout or `LoadUnconfirmed` may still be executing at the engine,
/// so unload/status cannot prove the original operation will not allocate
/// later; retain its Error slot. `load` returning `Ok` only confirms
/// acceptance, not completion, so a probe failure must retain the same fence.
async fn run_load_flow(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    context: LoadFlowContext,
    id: String,
    policy: LoadPolicy,
    evict: Vec<String>,
    mut previous: Option<PreviousModel>,
) -> OpOutcome {
    let aggregate_target_credit_gb = previous.as_ref().map_or(0.0, |model| model.memory_gb);
    let LoadFlowContext {
        release_limits,
        inherited,
        load_fence,
    } = context;
    // Persist before the first adapter IO (including victim eviction), so a
    // Supervisor restart cannot admit another load while this operation is
    // unresolved. Existing markers fail closed and are never overwritten.
    let load_fence = match load_fence
        .ok_or_else(|| BackendError::internal("load fence is unavailable"))
        .and_then(|fence| {
            fence.begin()?;
            Ok(fence)
        }) {
        Ok(fence) => fence,
        Err(err) => {
            return OpOutcome::Load {
                result: Err(err),
                victim_results: evict
                    .iter()
                    .map(|victim| (victim.clone(), Err(BackendError::eviction_failed(&id))))
                    .collect(),
                aggregate_status: None,
                aggregate_target_credit_gb,
                failure_outcome: previous.map_or(
                    LoadFailureOutcome::Settle(ModelState::Stopped),
                    LoadFailureOutcome::RestorePrevious,
                ),
            };
        }
    };
    // Every chosen victim gets an actual unload attempt, even after an
    // earlier one fails: `OpDone` only resolves ids present in
    // `victim_results`, so stopping early would leave later victims stuck
    // in `Stopping` forever (occupying budget with no in-flight op to ever
    // resolve them) instead of landing on `Stopped`/`Error` like the rest.
    let targets = evict.clone();
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
    let aggregate_status = if !targets.is_empty() {
        match confirm_memory_released(
            &adapter,
            &targets,
            release_limits.after_eviction_gb,
            &config,
        )
        .await
        {
            Ok(status) => Some(status),
            Err(_) => {
                // Keep all selected victims charged until the whole release is confirmed.
                for (_, result) in &mut victim_results {
                    *result = Err(BackendError::eviction_failed(&id));
                }
                eviction_failed = true;
                None
            }
        }
    } else {
        None
    };
    if eviction_failed {
        let err = BackendError::eviction_failed(&id);
        let failure_outcome = resolve_load_failure(
            &adapter,
            &id,
            &err,
            previous,
            release_limits.target_absent_gb,
            &config,
        )
        .await;
        let failure_outcome = if load_fence.clear().is_ok() {
            failure_outcome
        } else {
            LoadFailureOutcome::RetainUnconfirmedLoad(previous)
        };
        return OpOutcome::Load {
            result: Err(err),
            victim_results,
            aggregate_status,
            aggregate_target_credit_gb,
            failure_outcome,
        };
    }

    if inherited {
        let startup_status = with_adapter_timeout(
            adapter.status(),
            config.adapter_call_timeout,
            "startup/inherited-status",
        )
        .await;
        let verified_identity = startup_status.is_ok_and(|status| {
            status.used_gb.is_finite()
                && status.used_gb > 0.0
                && status.used_gb <= release_limits.after_eviction_gb + CAPACITY_EPSILON_GB
                && status.loaded.contains(&id)
        });
        let verified = if verified_identity {
            matches!(
                with_adapter_timeout(adapter.probe_ready(&id), config.adapter_call_timeout, &id)
                    .await,
                Ok(true)
            )
        } else {
            false
        };
        if !verified {
            let failure_outcome = if load_fence.clear().is_ok() {
                previous.map_or(
                    LoadFailureOutcome::Settle(ModelState::Stopped),
                    LoadFailureOutcome::RestorePrevious,
                )
            } else {
                LoadFailureOutcome::RetainUnconfirmedLoad(previous)
            };
            return OpOutcome::Load {
                result: Err(BackendError::model_unavailable(&id)),
                victim_results,
                aggregate_status,
                aggregate_target_credit_gb,
                failure_outcome,
            };
        }
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
        // Never retry an OOM while allocation/release remains unconfirmed.
        let release = confirm_memory_released(
            &adapter,
            std::slice::from_ref(&id),
            release_limits.target_absent_gb,
            &config,
        )
        .await;
        if release.is_err() {
            let cleanup_status =
                best_effort_release(&adapter, &id, release_limits.target_absent_gb, &config).await;
            let failure_outcome = if load_fence.clear().is_ok() {
                match cleanup_status {
                    Some(status) => LoadFailureOutcome::SettleReleased(status),
                    None => LoadFailureOutcome::after_release(ModelState::Error, previous),
                }
            } else {
                LoadFailureOutcome::RetainUnconfirmedLoad(previous)
            };
            return OpOutcome::Load {
                result: attempt,
                victim_results,
                aggregate_status,
                aggregate_target_credit_gb,
                failure_outcome,
            };
        }
        // An inherited request was admitted with a discount for residency
        // already included in the startup aggregate. Once OOM cleanup proves
        // that residency is gone, the same discounted charge cannot safely
        // be used for an immediate retry. End this attempt and let a later
        // request enter normal admission with a fresh estimate.
        if inherited && let Ok(status) = release {
            let failure_outcome = if load_fence.clear().is_ok() {
                LoadFailureOutcome::SettleReleased(status)
            } else {
                LoadFailureOutcome::RetainUnconfirmedLoad(previous)
            };
            return OpOutcome::Load {
                result: attempt,
                victim_results,
                aggregate_status,
                aggregate_target_credit_gb,
                failure_outcome,
            };
        }
        // The old instance is now confirmed absent, so subsequent retry
        // failures must not restore its former Ready ledger entry.
        previous = None;
        tokio::time::sleep(config.oom_retry_backoff).await;
        attempt = with_adapter_timeout(
            adapter.load(&id, Some(&policy)),
            config.adapter_call_timeout,
            &id,
        )
        .await;
    }
    if let Err(err) = attempt {
        let failure_outcome = resolve_load_failure(
            &adapter,
            &id,
            &err,
            previous,
            release_limits.target_absent_gb,
            &config,
        )
        .await;
        let unresolved_load = matches!(
            &err,
            BackendError::AdapterTimedOut { .. } | BackendError::LoadUnconfirmed { .. }
        );
        let fence_resolved = !unresolved_load && load_fence.clear().is_ok();
        let failure_outcome = if unresolved_load || fence_resolved {
            failure_outcome
        } else {
            LoadFailureOutcome::RetainUnconfirmedLoad(previous)
        };
        return OpOutcome::Load {
            result: Err(err),
            victim_results,
            aggregate_status,
            aggregate_target_credit_gb,
            failure_outcome,
        };
    }

    for _ in 0..config.probe_max_attempts {
        match with_adapter_timeout(adapter.probe_ready(&id), config.adapter_call_timeout, &id).await
        {
            Ok(true) => {
                if let Err(err) = load_fence.clear() {
                    return OpOutcome::Load {
                        result: Err(err),
                        victim_results,
                        aggregate_status,
                        aggregate_target_credit_gb,
                        failure_outcome: LoadFailureOutcome::RetainUnconfirmedLoad(previous),
                    };
                }
                return OpOutcome::Load {
                    result: Ok(()),
                    victim_results,
                    aggregate_status,
                    aggregate_target_credit_gb,
                    failure_outcome: LoadFailureOutcome::Settle(ModelState::Error), // unused: `result` is `Ok`
                };
            }
            Ok(false) => tokio::time::sleep(config.probe_interval).await,
            Err(err) => {
                // `load` returning Ok means accepted, not completed. A failed
                // probe provides no operation-termination evidence; cleanup
                // is useful, but cannot release the ledger fence.
                let _ =
                    best_effort_release(&adapter, &id, release_limits.target_absent_gb, &config)
                        .await;
                return OpOutcome::Load {
                    result: Err(err),
                    victim_results,
                    aggregate_status,
                    aggregate_target_credit_gb,
                    failure_outcome: LoadFailureOutcome::RetainUnconfirmedLoad(previous),
                };
            }
        }
    }
    // Exhausted readiness probes do not prove that the accepted load has
    // stopped. Even an empty status after unload cannot prevent a delayed
    // allocation, so keep the reservation pinned.
    let _ = best_effort_release(&adapter, &id, release_limits.target_absent_gb, &config).await;
    OpOutcome::Load {
        result: Err(BackendError::probe_timed_out(id.clone())),
        victim_results,
        aggregate_status,
        aggregate_target_credit_gb,
        failure_outcome: LoadFailureOutcome::RetainUnconfirmedLoad(previous),
    }
}

/// After a `probe_ready` failure/timeout, `id`'s real state is ambiguous —
/// `adapter.load` itself succeeded, so something may genuinely be
/// resident or may still allocate later. Cleanup is attempted, but its
/// acknowledgement and an empty status snapshot cannot establish that the
/// accepted load operation has terminated.
async fn best_effort_release(
    adapter: &Arc<dyn RuntimeAdapter>,
    id: &str,
    max_used_gb: f64,
    config: &SupervisorConfig,
) -> Option<BackendStatus> {
    if with_adapter_timeout(adapter.unload(id), config.adapter_call_timeout, id)
        .await
        .is_ok()
        && let Ok(status) =
            confirm_memory_released(adapter, &[id.to_string()], max_used_gb, config).await
    {
        Some(status)
    } else {
        None
    }
}

/// A rejected retry restores any previous budget-occupying state, including
/// Error (K10/H2). Only a fresh load may treat an explicit rejection as free.
/// A timeout or [`BackendError::LoadUnconfirmed`] still attempts cleanup,
/// but keeps its budget even if [`best_effort_release`] sees an empty engine:
/// the original operation can allocate after that snapshot.
async fn resolve_load_failure(
    adapter: &Arc<dyn RuntimeAdapter>,
    id: &str,
    err: &BackendError,
    previous: Option<PreviousModel>,
    release_limit_gb: f64,
    config: &SupervisorConfig,
) -> LoadFailureOutcome {
    if matches!(
        err,
        BackendError::AdapterTimedOut { .. } | BackendError::LoadUnconfirmed { .. }
    ) {
        // Try to clean up promptly, but this is not proof the original load
        // operation has ended; its delayed allocation can still follow.
        let _ = best_effort_release(adapter, id, release_limit_gb, config).await;
        LoadFailureOutcome::RetainUnconfirmedLoad(previous)
    } else if let Some(previous) = previous {
        LoadFailureOutcome::RestorePrevious(previous)
    } else {
        LoadFailureOutcome::Settle(ModelState::Stopped)
    }
}

/// Bundles what dispatch helpers need but never mutate (clippy arg-count).
struct Env<'a> {
    adapter: &'a Arc<dyn RuntimeAdapter>,
    config: &'a SupervisorConfig,
    self_tx: &'a mpsc::WeakSender<ActorMsg>,
    load_fence: Option<&'a Arc<crate::load_fence::LoadFence>>,
    // Latest aggregate engine residency. `aggregate_covered` records IDs
    // that the same snapshot proves are represented by independent slots.
    reserved_gb: f64,
    aggregate_covered: HashMap<String, f64>,
    startup_error: Option<BackendError>,
}

impl Env<'_> {
    // Keep the raw total so unknown residency can be recovered after a
    // covered slot is later removed. Credits are frozen at snapshot time;
    // they must never grow with a later request estimate.
    fn effective_reserved_gb(&self, models: &HashMap<String, ModelSlot>) -> f64 {
        self.effective_reserved_gb_except(models, None)
    }

    // Excluding a target preserves the aggregate as a release-proof bound;
    // admission always uses the ordinary total, including its coverage.
    fn effective_reserved_gb_except(
        &self,
        models: &HashMap<String, ModelSlot>,
        excluded_id: Option<&str>,
    ) -> f64 {
        let covered_slots_gb: f64 = self
            .aggregate_covered
            .iter()
            .filter(|(id, _)| excluded_id != Some(id.as_str()))
            .filter_map(|(id, credited_gb)| models.get(id).map(|slot| (slot, credited_gb)))
            .filter(|(slot, _)| occupies_budget(slot.state) && !slot.load_unconfirmed)
            .map(|(slot, credited_gb)| slot.memory_gb.min(*credited_gb))
            .sum();
        (self.reserved_gb - covered_slots_gb).max(0.0)
    }

    fn refresh_aggregate(&mut self, models: &HashMap<String, ModelSlot>, status: &BackendStatus) {
        self.refresh_aggregate_except(models, status, None, 0.0);
    }

    fn refresh_aggregate_except(
        &mut self,
        models: &HashMap<String, ModelSlot>,
        status: &BackendStatus,
        excluded_id: Option<&str>,
        excluded_credit_gb: f64,
    ) {
        self.reserved_gb = status.used_gb;
        self.aggregate_covered = status
            .loaded
            .iter()
            .filter(|id| {
                excluded_id != Some(id.as_str())
                    && models
                        .get(*id)
                        .is_some_and(|slot| occupies_budget(slot.state) && !slot.load_unconfirmed)
            })
            .filter_map(|id| models.get(id).map(|slot| (id.clone(), slot.memory_gb)))
            .collect();
        // This snapshot predates the target reload. Only its old charge
        // overlaps the aggregate, even if its new estimate is much larger.
        if let Some(id) = excluded_id
            && excluded_credit_gb > 0.0
            && status.loaded.iter().any(|loaded_id| loaded_id == id)
        {
            self.aggregate_covered
                .insert(id.to_string(), excluded_credit_gb);
        }
    }
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
            pinned: slot.load_unconfirmed
                || slot.inherited
                || matches!(
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

fn ledger_used_except(models: &HashMap<String, ModelSlot>, exclude: &str) -> f64 {
    models
        .iter()
        .filter(|(id, slot)| id.as_str() != exclude && occupies_budget(slot.state))
        .map(|(_, slot)| slot.memory_gb)
        .sum()
}

/// `InsufficientCapacity` is a real, expected admission outcome
/// (`eviction_impossible`); the other two variants mean the Supervisor
/// handed `plan_eviction` a contract-violating input of its own making
/// (`budget_gb` is validated at `spawn`, and `build_snapshot` always
/// excludes `id`) — an invariant violation, not a normal rejection.
fn map_plan_error(err: PlanEvictionError, id: &str) -> BackendError {
    match err {
        PlanEvictionError::InsufficientCapacity { .. } => BackendError::eviction_impossible(id),
        // `handle_load` validates `memory_gb` up front (Low, Opus Tier-2
        // review), so reaching this at all should be structurally
        // impossible — kept as defense-in-depth, mapped the same way a
        // caller-supplied bad value would be rather than as an internal
        // Supervisor bug.
        PlanEvictionError::InvalidCapacity { field, value } => {
            BackendError::invalid_request(format!("invalid capacity for {field}: {value}"))
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
    previous: Option<PreviousModel>,
    models: &mut HashMap<String, ModelSlot>,
    env: &Env<'_>,
) {
    set_state_or_panic(models, &id, ModelState::Loading);
    let previous_memory_gb = previous.map_or(0.0, |model| model.memory_gb);
    let evicted_ids: std::collections::HashSet<&str> = evict.iter().map(String::as_str).collect();
    let release_limit_gb: f64 = models
        .iter()
        .filter(|(model_id, slot)| {
            model_id.as_str() != id
                && !evicted_ids.contains(model_id.as_str())
                && occupies_budget(slot.state)
        })
        .map(|(_, slot)| slot.memory_gb)
        .sum();
    let release_limits = ReleaseLimits {
        target_absent_gb: release_limit_gb + env.effective_reserved_gb(models),
        after_eviction_gb: release_limit_gb
            + previous_memory_gb
            + env.effective_reserved_gb(models),
    };
    let inherited = previous.is_some_and(|model| model.inherited);
    let adapter = env.adapter.clone();
    let config = env.config.clone();
    let load_fence = env.load_fence.cloned();
    let ownership = env.load_fence.cloned();
    let self_tx = env.self_tx.clone();
    let id_for_task = id.clone();
    // Kept alongside `evict` (which is moved into `run_load_flow` below) so
    // the panic-fallback arm can still resolve every chosen victim even
    // though `run_load_flow`'s own `victim_results` was lost with its
    // panicking stack frame (Medium, Opus Tier-2 review — see below).
    let evict_for_panic_fallback = evict.clone();
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
            LoadFlowContext {
                release_limits,
                inherited,
                load_fence,
            },
            id_for_task.clone(),
            policy,
            evict,
            previous,
        ));
        let _ownership = ownership;
        let outcome = match inner.await {
            Ok(outcome) => outcome,
            Err(join_err) => OpOutcome::Load {
                result: Err(BackendError::adapter_panicked(
                    &id_for_task,
                    join_err.to_string(),
                )),
                // Every victim `plan_eviction` chose must still be
                // resolved, not left dangling in `Stopping` forever just
                // because the panic happened to land mid-eviction-loop.
                // Report each as `Err` so `OpDone` resolves the victim;
                // the durable marker continues to block future loads until
                // an operator resets it or the engine is restarted safely.
                victim_results: evict_for_panic_fallback
                    .into_iter()
                    .map(|victim| {
                        let err = BackendError::adapter_panicked(
                            victim.clone(),
                            "load task panicked mid-eviction; this victim's real state is unknown",
                        );
                        (victim, Err(err))
                    })
                    .collect(),
                aggregate_status: None,
                aggregate_target_credit_gb: 0.0,
                // A panic may have happened after the engine accepted the
                // request. Keep its durable marker and ledger fence.
                failure_outcome: LoadFailureOutcome::RetainUnconfirmedLoad(previous),
            },
        };
        // If `upgrade` fails, every `SupervisorHandle` is already gone and
        // the actor itself has exited — nothing is left to deliver this to.
        if let Some(tx) = self_tx.upgrade() {
            let _ = tx
                .send(ActorMsg::OpDone {
                    id: id_for_task,
                    outcome,
                })
                .await;
        }
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
    // Decided before the mutex, same as the already-Ready/already-Stopped
    // checks elsewhere: a malformed request is the caller's mistake
    // regardless of what else is in flight, and must never be silently
    // accepted into an eviction/budget calculation that then produces a
    // meaningless result (Low, Opus Tier-2 review).
    if !memory_gb.is_finite() || memory_gb < 0.0 {
        let _ = reply.send(Err(BackendError::invalid_request(format!(
            "memory_gb must be finite and >= 0, got {memory_gb}"
        ))));
        return;
    }
    if let Some(active) = active_op.as_mut()
        && active.id == id
    {
        if let ActiveKind::Load {
            policy: active_policy,
            memory_gb: active_memory_gb,
            waiters,
        } = &mut active.kind
            && *active_policy == policy
            && *active_memory_gb == memory_gb
        {
            waiters.push(reply);
        } else {
            let _ = reply.send(Err(BackendError::busy(
                "a load for this id with a different policy or memory_gb is already in flight",
                Some(id.clone()),
                None,
            )));
        }
        return;
    }
    // A new attempt cannot supersede an engine-side load that may still run.
    if let Some(slot) = models.get(&id)
        && slot.load_unconfirmed
    {
        let _ = reply.send(Err(BackendError::model_unavailable(&id)));
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
    if models.get(&id).is_some_and(|slot| slot.inflight != 0) {
        let _ = reply.send(Err(BackendError::busy(
            "model has chats in flight",
            Some(id),
            None,
        )));
        return;
    }
    if let Some(active) = active_op.as_ref() {
        let _ = reply.send(Err(BackendError::busy(
            "a different id currently holds the load/evict/unload mutex",
            Some(active.id.clone()),
            None,
        )));
        return;
    }

    let inherited = models.get(&id).is_some_and(|slot| slot.inherited);
    let reservation = env.effective_reserved_gb(models);
    if reservation > env.config.budget_gb {
        let _ = reply.send(Err(BackendError::eviction_impossible(&id)));
        return;
    }
    let request_charge = if inherited {
        memory_gb.max(reservation) - reservation
    } else {
        memory_gb
    };
    let snapshot = build_snapshot(models, env.config.budget_gb - reservation, &id);
    let evict = match plan_eviction(
        &snapshot,
        ModelReq {
            id: id.clone(),
            memory_gb: request_charge,
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
    // Capture every budget-occupying state before overwriting the slot:
    // an Error retry is not a fresh load (K10/H2).
    let previous = models
        .get(&id)
        .filter(|slot| occupies_budget(slot.state))
        .map(|slot| PreviousModel {
            state: slot.state,
            policy: slot.policy,
            memory_gb: slot.memory_gb,
            inherited: slot.inherited,
        });
    models.insert(
        id.clone(),
        ModelSlot {
            memory_gb: if inherited { request_charge } else { memory_gb },
            state: ModelState::Launching,
            last_used_seq,
            policy,
            inflight: 0,
            load_unconfirmed: false,
            inherited,
        },
    );
    *active_op = Some(ActiveOp {
        id: id.clone(),
        kind: ActiveKind::Load {
            policy,
            memory_gb,
            waiters: vec![reply],
        },
    });
    start_load(id, policy, evict, previous, models, env);
}

/// Standalone unloads also confirm release before freeing ledger capacity.
fn start_unload(id: String, max_used_gb: f64, env: &Env<'_>) {
    let adapter = env.adapter.clone();
    let self_tx = env.self_tx.clone();
    let config = env.config.clone();
    let ownership = env.load_fence.cloned();
    tokio::spawn(async move {
        // Same panic-isolation shape as `start_load` — see its doc comment.
        let id_for_inner = id.clone();
        let inner_ownership = ownership.clone();
        let inner = tokio::spawn(async move {
            let _ownership = inner_ownership;
            with_adapter_timeout(
                adapter.unload(&id_for_inner),
                config.adapter_call_timeout,
                &id_for_inner,
            )
            .await?;
            confirm_memory_released(&adapter, &[id_for_inner], max_used_gb, &config).await
        });
        let (result, status) = match inner.await {
            Ok(Ok(status)) => (Ok(()), Some(status)),
            Ok(Err(err)) => (Err(err), None),
            Err(join_err) => (
                Err(BackendError::adapter_panicked(&id, join_err.to_string())),
                None,
            ),
        };
        let _ownership = ownership;
        if let Some(tx) = self_tx.upgrade() {
            let _ = tx
                .send(ActorMsg::OpDone {
                    id,
                    outcome: OpOutcome::Unload { result, status },
                })
                .await;
        }
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
                let _ = reply.send(Err(BackendError::busy(
                    "a load for this id is already in flight",
                    Some(id.clone()),
                    None,
                )));
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
        Some(ModelState::Stopped) if !models.get(&id).is_some_and(|slot| slot.inherited) => {
            let _ = reply.send(Ok(()));
            return;
        }
        Some(_) => {}
    }
    if models.get(&id).is_some_and(|slot| slot.load_unconfirmed) {
        let _ = reply.send(Err(BackendError::model_unavailable(&id)));
        return;
    }
    if let Some(active) = active_op.as_ref() {
        let _ = reply.send(Err(BackendError::busy(
            "a different id currently holds the load/evict/unload mutex",
            Some(active.id.clone()),
            None,
        )));
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
        let max_used_gb =
            ledger_used_except(models, &id) + env.effective_reserved_gb_except(models, Some(&id));
        start_unload(id, max_used_gb, env);
    }
}

async fn run_actor(
    adapter: Arc<dyn RuntimeAdapter>,
    config: SupervisorConfig,
    mut rx: mpsc::Receiver<ActorMsg>,
    call_slots: Arc<Semaphore>,
    self_tx: mpsc::WeakSender<ActorMsg>,
) {
    let mut models: HashMap<String, ModelSlot> = HashMap::new();
    let mut active_op: Option<ActiveOp> = None;
    let mut next_seq: u64 = 0;
    // Once `Some`, the actor refuses all further ledger-touching work
    // rather than risk operating on state whose integrity is no longer
    // trusted (M3, Opus Tier-2 review) — every `Command` gets an immediate
    // `invariant_violation` reply instead of being processed, and any
    // `OpDone` still in flight has its waiters notified the same way. This
    // replaces `panic!`ing on a detected invariant violation, which killed
    // the *entire* actor task (and, with it, every other in-flight and
    // future caller) over a single corrupted entry.
    let mut poisoned: Option<String> = None;
    let load_fence_result = adapter
        .load_fence_path()
        .and_then(crate::load_fence::LoadFence::claim)
        .map(Arc::new);
    let load_fence_error = match &load_fence_result {
        Ok(fence) => fence.check_clear().err(),
        Err(err) => Some(err.clone()),
    };
    let load_fence = load_fence_result.ok();
    // Sample engine residency once, before accepting commands. Spawning the
    // adapter call isolates panic; the timeout bounds startup even when the
    // adapter never returns.
    let startup_adapter = adapter.clone();
    let startup_ownership = load_fence.clone();
    let timeout = config.adapter_call_timeout;
    let startup = tokio::spawn(async move {
        let _ownership = startup_ownership;
        with_adapter_timeout(startup_adapter.status(), timeout, "startup/status").await
    })
    .await
    .map_err(|err| BackendError::adapter_panicked("startup/status", err.to_string()))
    .and_then(|result| result);
    let startup = startup.and_then(|status| {
        if !status.used_gb.is_finite()
            || status.used_gb < 0.0
            || (status.used_gb == 0.0 && !status.loaded.is_empty())
        {
            Err(BackendError::internal(
                "engine residency has no valid memory measurement",
            ))
        } else {
            Ok(status)
        }
    });
    let initial_reserved_gb = startup
        .as_ref()
        .map(|status| status.used_gb)
        .unwrap_or_default();
    if let Ok(status) = &startup {
        for id in &status.loaded {
            models.insert(
                id.clone(),
                ModelSlot {
                    memory_gb: 0.0,
                    state: ModelState::Error,
                    last_used_seq: 0,
                    policy: LoadPolicy {
                        mode: idoris_contracts::load_policy::LoadMode::OnDemand,
                        keepalive: idoris_contracts::load_policy::Keepalive::Pinned {
                            pinned: true,
                        },
                        admission: idoris_contracts::load_policy::Admission::Coexist,
                    },
                    inflight: 0,
                    load_unconfirmed: false,
                    inherited: true,
                },
            );
        }
    }
    let mut env = Env {
        adapter: &adapter,
        config: &config,
        self_tx: &self_tx,
        load_fence: load_fence.as_ref(),
        reserved_gb: initial_reserved_gb,
        aggregate_covered: HashMap::new(),
        startup_error: load_fence_error.or_else(|| startup.err()),
    };

    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMsg::Cmd(cmd) if poisoned.is_some() => {
                let msg = poisoned.clone().unwrap_or_default();
                match cmd {
                    Command::List { reply } => {
                        let _ = reply.send(Err(BackendError::invariant_violation(msg)));
                    }
                    Command::Status { reply } => {
                        let _ = reply.send(Err(BackendError::invariant_violation(msg)));
                    }
                    Command::Chat { reply, .. } => {
                        let _ = reply.send(Err(BackendError::invariant_violation(msg)));
                    }
                    Command::Load { reply, .. } => {
                        let _ = reply.send(Err(BackendError::invariant_violation(msg)));
                    }
                    Command::Unload { reply, .. } => {
                        let _ = reply.send(Err(BackendError::invariant_violation(msg)));
                    }
                }
            }

            ActorMsg::Cmd(Command::List { reply }) => {
                if let Some(err) = &env.startup_error {
                    let _ = reply.send(Err(err.clone()));
                    continue;
                }
                let Ok(permit) = call_slots.clone().try_acquire_owned() else {
                    let _ = reply.send(Err(BackendError::busy(
                        "concurrent adapter-call limit reached",
                        None,
                        None,
                    )));
                    continue;
                };
                let adapter = adapter.clone();
                let timeout = config.adapter_call_timeout;
                let ownership = load_fence.clone();
                tokio::spawn(async move {
                    let _ownership = ownership;
                    let _permit = permit;
                    let _ = reply.send(with_adapter_timeout(adapter.list(), timeout, "list").await);
                });
            }

            ActorMsg::Cmd(Command::Status { reply }) => {
                if let Some(err) = &env.startup_error {
                    let _ = reply.send(Err(err.clone()));
                    continue;
                }
                let used_gb: f64 = models
                    .values()
                    .filter(|slot| occupies_budget(slot.state))
                    .map(|slot| slot.memory_gb)
                    .sum::<f64>()
                    + env.effective_reserved_gb(&models);
                let pressure = if used_gb >= config.budget_gb {
                    Pressure::Hard
                } else if used_gb >= config.budget_gb * 0.8 {
                    Pressure::Soft
                } else {
                    Pressure::Ok
                };
                let loaded: Vec<String> = models
                    .iter()
                    .filter(|(_, slot)| {
                        slot.state == ModelState::Ready
                            || (slot.inherited && slot.state != ModelState::Stopped)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                let _ = reply.send(Ok(BackendStatus {
                    pressure,
                    used_gb,
                    model_memory_max_gb: config.budget_gb,
                    loaded,
                }));
            }

            ActorMsg::Cmd(Command::Chat { req, cancel, reply }) => {
                if let Some(err) = &env.startup_error {
                    let _ = reply.send(Err(err.clone()));
                    continue;
                }
                match models.get(&req.model) {
                    None => {
                        let _ = reply.send(Err(BackendError::model_not_found(&req.model)));
                    }
                    Some(slot) if slot.state != ModelState::Ready => {
                        let _ = reply.send(Err(not_ready_error(&req.model, slot.state)));
                    }
                    Some(_) => {
                        let Ok(permit) = call_slots.clone().try_acquire_owned() else {
                            let _ = reply.send(Err(BackendError::busy(
                                "concurrent adapter-call limit reached",
                                None,
                                None,
                            )));
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
                        let ownership = load_fence.clone();
                        tokio::spawn(async move {
                            let _ownership = ownership;
                            let _permit = permit;
                            let model = req.model.clone();
                            // Same isolation shape as `start_load`/`start_unload`:
                            // a panic in `adapter.chat` must not skip the
                            // `ChatDone` send below, or `inflight` leaks forever
                            // — wedging any pending drain and, with it,
                            // `active_op` (Critical, Opus Tier-2 review, P6).
                            let model_for_chat = model.clone();
                            let inner_ownership = _ownership.clone();
                            let inner = tokio::spawn(async move {
                                let _ownership = inner_ownership;
                                with_adapter_timeout(
                                    adapter.chat(req, cancel),
                                    timeout,
                                    &model_for_chat,
                                )
                                .await
                            });
                            let result = match inner.await {
                                Ok(result) => result,
                                Err(join_err) => Err(BackendError::adapter_panicked(
                                    &model,
                                    join_err.to_string(),
                                )),
                            };
                            let _ = reply.send(result);
                            if let Some(tx) = self_tx.upgrade() {
                                let _ = tx.send(ActorMsg::ChatDone { model }).await;
                            }
                        });
                    }
                }
            }

            ActorMsg::Cmd(Command::Load {
                id,
                memory_gb,
                policy,
                reply,
            }) => {
                if let Some(err) = &env.startup_error {
                    let _ = reply.send(Err(err.clone()));
                    continue;
                }
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
                if let Some(err) = &env.startup_error {
                    let _ = reply.send(Err(err.clone()));
                    continue;
                }
                handle_unload(id, reply, &mut models, &mut active_op, &env);
            }

            ActorMsg::OpDone { id, outcome } if poisoned.is_some() => {
                let msg = poisoned.clone().unwrap_or_default();
                if let Some(waiters) = take_waiters(&mut active_op) {
                    for waiter in waiters {
                        let _ = waiter.send(Err(BackendError::invariant_violation(msg.clone())));
                    }
                }
                let _ = id; // already covered by `msg`; nothing id-specific left to do
                let _ = outcome;
            }

            ActorMsg::OpDone { id, outcome } => {
                let (
                    result,
                    done_state,
                    failure_outcome,
                    victim_results,
                    aggregate_status,
                    aggregate_target_credit_gb,
                    unload_status,
                ) = match outcome {
                    OpOutcome::Load {
                        result,
                        victim_results,
                        aggregate_status,
                        aggregate_target_credit_gb,
                        failure_outcome,
                    } => (
                        result,
                        ModelState::Ready,
                        failure_outcome,
                        victim_results,
                        aggregate_status,
                        aggregate_target_credit_gb,
                        None,
                    ),
                    OpOutcome::Unload { result, status } => (
                        result,
                        ModelState::Stopped,
                        LoadFailureOutcome::Settle(ModelState::Error),
                        Vec::new(),
                        None,
                        0.0,
                        status,
                    ),
                };

                let mut violation: Option<String> = None;
                for (victim_id, victim_result) in victim_results {
                    let victim_state = if victim_result.is_ok() {
                        ModelState::Stopped
                    } else {
                        ModelState::Error
                    };
                    if violation.is_none()
                        && let Err(e) = set_state_or_poison(&mut models, &victim_id, victim_state)
                    {
                        violation = Some(e);
                    }
                }
                if violation.is_none()
                    && let Some(status) = &aggregate_status
                {
                    env.refresh_aggregate_except(
                        &models,
                        status,
                        Some(&id),
                        aggregate_target_credit_gb,
                    );
                }
                if violation.is_none() {
                    match &result {
                        Ok(()) => {
                            next_seq += 1;
                            match models.get_mut(&id) {
                                Some(slot) => {
                                    slot.state = done_state;
                                    if done_state == ModelState::Stopped {
                                        slot.inherited = false;
                                    }
                                    if done_state == ModelState::Ready {
                                        slot.last_used_seq = next_seq;
                                    }
                                }
                                None => violation = Some(format!("ledger lost {id:?} mid-op")),
                            }
                        }
                        Err(_) => match failure_outcome {
                            LoadFailureOutcome::Settle(state) => {
                                if let Err(e) = set_state_or_poison(&mut models, &id, state) {
                                    violation = Some(e);
                                }
                            }
                            LoadFailureOutcome::SettleReleased(status) => {
                                match set_state_or_poison(&mut models, &id, ModelState::Stopped) {
                                    Err(e) => violation = Some(e),
                                    Ok(()) => {
                                        env.refresh_aggregate(&models, &status);
                                    }
                                }
                            }
                            LoadFailureOutcome::RestorePrevious(mut previous)
                            | LoadFailureOutcome::RetainPreviousOnError(mut previous) => {
                                match models.get_mut(&id) {
                                    Some(slot) => {
                                        if matches!(
                                            failure_outcome,
                                            LoadFailureOutcome::RetainPreviousOnError(_)
                                        ) {
                                            previous.state = ModelState::Error;
                                            previous.memory_gb =
                                                slot.memory_gb.max(previous.memory_gb);
                                        }
                                        slot.state = previous.state;
                                        slot.policy = previous.policy;
                                        slot.memory_gb = previous.memory_gb;
                                        slot.inherited = previous.inherited;
                                    }
                                    None => {
                                        violation =
                                            Some(format!("ledger lost {id:?} mid-op (reload)"));
                                    }
                                }
                            }
                            LoadFailureOutcome::RetainUnconfirmedLoad(previous) => {
                                match models.get_mut(&id) {
                                    Some(slot) => {
                                        slot.state = ModelState::Error;
                                        slot.load_unconfirmed = true;
                                        if let Some(previous) = previous {
                                            slot.policy = previous.policy;
                                            slot.memory_gb = slot.memory_gb.max(previous.memory_gb);
                                        }
                                    }
                                    None => {
                                        violation = Some(format!(
                                            "ledger lost {id:?} mid-op (unconfirmed load)"
                                        ))
                                    }
                                }
                            }
                        },
                    }
                }

                if let Some(status) = unload_status {
                    env.refresh_aggregate(&models, &status);
                }

                let waiters = take_waiters(&mut active_op);
                if waiters.is_none() && violation.is_none() {
                    violation = Some(format!("OpDone for {id:?} with no active op"));
                }
                match (violation, waiters) {
                    (Some(msg), Some(waiters)) => {
                        poisoned = Some(msg.clone());
                        for waiter in waiters {
                            let _ =
                                waiter.send(Err(BackendError::invariant_violation(msg.clone())));
                        }
                    }
                    (Some(msg), None) => poisoned = Some(msg),
                    (None, Some(waiters)) => {
                        for waiter in waiters {
                            let _ = waiter.send(result.clone());
                        }
                    }
                    (None, None) => unreachable!("violation is set whenever waiters is None"),
                }
            }

            ActorMsg::ChatDone { model } if poisoned.is_some() => {
                // No reply channel is waiting on a `ChatDone` — nothing
                // more to do than already-being-poisoned covers.
                let _ = model;
            }

            ActorMsg::ChatDone { model } => {
                // Ledger entries are only ever transitioned, never removed,
                // so a `ChatDone` for an id `chat` was actually dispatched
                // to must find an entry.
                match models.get_mut(&model) {
                    Some(slot) => {
                        slot.inflight = slot.inflight.saturating_sub(1);
                        let inflight = slot.inflight;
                        if inflight == 0
                            && let Some(active) = active_op.as_mut()
                            && active.id == model
                            && let ActiveKind::Unload { started, .. } = &mut active.kind
                            && !*started
                        {
                            *started = true;
                            let max_used_gb = ledger_used_except(&models, &model)
                                + env.effective_reserved_gb_except(&models, Some(&model));
                            start_unload(model, max_used_gb, &env);
                        }
                    }
                    None => poisoned = Some(format!("ledger has no entry for {model:?}")),
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

    /// Wraps a call that a regression could make hang forever, so a
    /// reintroduced bug fails *this test* (fast, whether real- or
    /// paused-clock) instead of hanging the whole `cargo test` run — Opus
    /// Tier-2 review, following up on the C1/C2 fixes.
    async fn no_hang<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .expect("must not hang")
    }

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

    /// Low (Opus Tier-2 review): the same singleflight-merge guarantee must
    /// hold under a *real* multi-thread runtime — every other test above
    /// uses the default single-threaded test runtime, whose cooperative,
    /// non-preemptive scheduling could hide a race that only manifests
    /// under true OS-thread parallelism (the actor task and every one of
    /// these 8 callers can each land on a different worker thread here).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn singleflight_merge_holds_under_a_real_multi_thread_runtime() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(50));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let h = handle.clone();
                tokio::spawn(async move { h.load("a", 4.0, on_demand_policy()).await })
            })
            .collect();
        for task in tasks {
            task.await
                .unwrap()
                .expect("every concurrently-merged load should succeed");
        }
        assert_eq!(
            adapter.load_call_count("a"),
            1,
            "singleflight must merge all 8 concurrent loads into exactly one adapter call, even across real OS threads"
        );
    }

    /// M5: when a singleflight-merged load fails, *every* waiter must get
    /// the *same* error (a mutation that only notified the first waiter
    /// must be caught) — and a later, separate `load` call must actually
    /// retry the adapter, not keep replaying the old failure forever.
    #[tokio::test(start_paused = true)]
    async fn singleflight_failure_notifies_every_waiter_identically_and_a_later_load_retries() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(50));
        adapter.set_load_script("a", vec![crate::mock::LoadOutcome::Fail]);
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let h1 = handle.clone();
        let h2 = handle.clone();
        let f1 = tokio::spawn(async move { h1.load("a", 4.0, on_demand_policy()).await });
        let f2 = tokio::spawn(async move { h2.load("a", 4.0, on_demand_policy()).await });
        let err1 = f1
            .await
            .unwrap()
            .expect_err("the scripted Fail outcome must surface as an error");
        let err2 = f2
            .await
            .unwrap()
            .expect_err("the merged waiter must see the same failure, not silently succeed");
        assert_eq!(
            err1, err2,
            "both waiters of a merged, failed load must get an identical error"
        );
        assert_eq!(
            adapter.load_call_count("a"),
            1,
            "the failed attempt must still have been exactly one adapter call"
        );
        // The script queue is now empty (defaults to `Ok`): a later, plain
        // `load` must actually re-invoke the adapter, proving the failure
        // wasn't cached or replayed instead of genuinely retried.
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect("a later load must retry, not replay the earlier failure");
        assert_eq!(adapter.load_call_count("a"), 2);
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

    /// Low (Opus Tier-2 review): the same id, the same policy, but a
    /// *different* `memory_gb` must not be silently merged either — only
    /// exact equality on both fields singleflight-merges.
    #[tokio::test(start_paused = true)]
    async fn concurrent_loads_with_a_different_memory_gb_do_not_merge() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_millis(50));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let h2 = handle.clone();
        let f1 = tokio::spawn(async move { handle.load("a", 4.0, on_demand_policy()).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let err = h2
            .load("a", 8.0, on_demand_policy())
            .await
            .expect_err("a different memory_gb for the same in-flight id must not merge");
        assert_eq!(err.reason_code(), "supervisor_busy");
        f1.await
            .unwrap()
            .expect("the original load should still succeed");
    }

    /// Low (Opus Tier-2 review): a non-finite or negative `memory_gb` is
    /// the caller's mistake, rejected up front — never silently accepted
    /// into an eviction/budget calculation.
    #[tokio::test]
    async fn a_non_finite_memory_gb_is_an_invalid_request() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let err = handle
                .load("a", bad, on_demand_policy())
                .await
                .expect_err("a non-finite or negative memory_gb must be rejected");
            assert_eq!(err.reason_code(), "invalid_request");
        }
    }

    /// Negative contrast: the boundary value `0.0` is finite and
    /// non-negative, so it is accepted — isolates that it was specifically
    /// non-finite/negative values being rejected, not zero itself.
    #[tokio::test]
    async fn a_zero_memory_gb_is_accepted() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        handle
            .load("a", 0.0, on_demand_policy())
            .await
            .expect("a zero memory_gb is a valid (if unusual) request");
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
        // M2 (Opus Tier-2 review): `Busy` must name which id is actually
        // holding the mutex, not just say "busy" with no context.
        match &err {
            BackendError::Busy { active_id, .. } => {
                assert_eq!(active_id.as_deref(), Some("a"));
            }
            other => panic!("expected BackendError::Busy, got {other:?}"),
        }
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

    /// A minimal `RuntimeAdapter` whose `unload` panics for exactly one id
    /// and succeeds for everything else — for exercising a panic that lands
    /// *mid*-eviction-loop, after an earlier victim was already
    /// successfully unloaded.
    struct UnloadPanicsForOneIdAdapter {
        panics_for: &'static str,
        /// Panics exactly once, then behaves normally — a one-shot fault,
        /// not a permanent one, so a later retry of the *same* id (e.g.
        /// evicting it again for a different request) can still succeed.
        already_panicked: std::sync::atomic::AtomicBool,
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for UnloadPanicsForOneIdAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(["a", "b", "c", "d"]
                .into_iter()
                .map(|id| ModelInfo {
                    id: id.to_string(),
                    memory_gb: 10.0,
                })
                .collect())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Ok(())
        }
        async fn unload(&self, id: &str) -> Result<(), BackendError> {
            if id == self.panics_for
                && !self
                    .already_panicked
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                panic!("UnloadPanicsForOneIdAdapter panics once for {id} (test double)");
            }
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

    /// Medium (Opus Tier-2 review, probe P1d): a panic that lands
    /// *mid*-eviction-loop (after "a" was already successfully unloaded,
    /// before "b"'s own unload panics) must still resolve *every* chosen
    /// victim, not just leave the not-yet-attempted ones dangling in
    /// `Stopping` forever.
    #[tokio::test]
    async fn a_panic_mid_eviction_still_resolves_every_victim() {
        let adapter = Arc::new(UnloadPanicsForOneIdAdapter {
            panics_for: "b",
            already_panicked: std::sync::atomic::AtomicBool::new(false),
            fence_path: std::sync::OnceLock::new(),
        });
        let handle = Supervisor::spawn(
            adapter.clone(),
            SupervisorConfig {
                budget_gb: 20.0,
                ..SupervisorConfig::default()
            },
        )
        .expect("spawn should succeed");
        no_hang(handle.load("a", 10.0, on_demand_policy()))
            .await
            .expect("load a should succeed");
        no_hang(handle.load("b", 10.0, on_demand_policy()))
            .await
            .expect("load b should succeed");
        // c(20) requires evicting both a and b (LRU order: a, then b) —
        // a's unload succeeds, b's panics mid-loop. c's own load is never
        // even attempted, so it also settles on `Error` (unknown state).
        let err = no_hang(handle.load("c", 20.0, on_demand_policy()))
            .await
            .expect_err("the panic mid-eviction must surface, not silently succeed");
        assert_eq!(err.reason_code(), "adapter_panicked");
        assert!(
            adapter
                .load_fence_path()
                .and_then(|path| crate::load_fence::LoadFence::new(path).check_clear())
                .is_err()
        );
        for victim in ["a", "b"] {
            no_hang(handle.unload(victim))
                .await
                .expect("each victim must leave Stopping and allow explicit unload");
        }
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
        drop(handle);
        tokio::task::yield_now().await;
        let restarted = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let err = restarted
            .load("d", 15.0, on_demand_policy())
            .await
            .unwrap_err();
        assert_eq!(err.reason_code(), "internal");
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
    struct PanickingAdapter {
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for PanickingAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
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
        let adapter = Arc::new(PanickingAdapter {
            fence_path: std::sync::OnceLock::new(),
        });
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        let err = no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect_err("a panicking adapter call must surface as an error, not hang");
        assert_eq!(err.reason_code(), "adapter_panicked");
        // Negative contrast baked into the same test: if the panic *had*
        // wedged the Supervisor (active_op stuck `Some` forever), this
        // second, unrelated load would also fail with `Busy` — it must not.
        no_hang(handle.load("a", 4.0, on_demand_policy()))
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
        let err = no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect_err("a hanging adapter call must time out, not hang forever");
        assert_eq!(err.reason_code(), "adapter_timed_out");
        // Negative contrast: the Supervisor must still be usable afterward
        // — a stuck `active_op` would make this also fail with `Busy`.
        let models = no_hang(handle.list()).await.expect("list must still work");
        assert_eq!(models, catalog());
    }

    /// A minimal `RuntimeAdapter` whose `unload` always panics — the M13
    /// (Opus Tier-2 review) counterpart of `PanickingAdapter`, isolating
    /// `start_unload`'s own panic-isolation path from an eviction's own
    /// unloads (already covered by `run_load_flow`'s tests).
    struct UnloadAlwaysPanicsAdapter {
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for UnloadAlwaysPanicsAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(two_model_catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Ok(())
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            panic!("UnloadAlwaysPanicsAdapter::unload always panics (test double)");
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

    /// M13 (Opus Tier-2 review): a standalone `unload`'s own panic
    /// isolation (`start_unload`) must not wedge the Supervisor either —
    /// same guarantee C1 gave `load`, verified independently here.
    #[tokio::test]
    async fn a_panicking_standalone_unload_does_not_wedge_the_supervisor() {
        let adapter = Arc::new(UnloadAlwaysPanicsAdapter {
            fence_path: std::sync::OnceLock::new(),
        });
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect("load a should succeed");
        let err = no_hang(handle.unload("a"))
            .await
            .expect_err("a panicking unload must surface as an error, not hang");
        assert_eq!(err.reason_code(), "adapter_panicked");
        // Negative contrast: if `active_op` had stayed stuck on "a", this
        // would report `Busy` instead of succeeding.
        no_hang(handle.load("b", 4.0, on_demand_policy()))
            .await
            .expect("a different id must not be busy after the panicking unload");
    }

    /// M13 (Opus Tier-2 review): a standalone `unload` call that never
    /// returns must not hold `active_op` forever either — bounded by
    /// `adapter_call_timeout`, same as `load`.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_standalone_unload_times_out_and_does_not_wedge_the_supervisor() {
        let adapter = Arc::new(MockAdapter::new(two_model_catalog()));
        let handle = Supervisor::spawn(
            adapter.clone(),
            SupervisorConfig {
                adapter_call_timeout: std::time::Duration::from_millis(50),
                ..SupervisorConfig::default()
            },
        )
        .expect("spawn should succeed");
        no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect("load a should succeed");
        adapter.set_unload_delay("a", std::time::Duration::from_secs(3600));
        let err = no_hang(handle.unload("a"))
            .await
            .expect_err("a hanging unload call must time out, not hang forever");
        assert_eq!(err.reason_code(), "adapter_timed_out");
        no_hang(handle.load("b", 4.0, on_demand_policy()))
            .await
            .expect("a different id must not be busy after the timed-out unload");
    }

    /// A minimal `RuntimeAdapter` whose `chat` always panics (`load`/
    /// `unload`/`probe_ready` all succeed normally) — for exercising the
    /// panic-isolation path in the `Chat` dispatch specifically.
    struct ChatPanicsAdapter {
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for ChatPanicsAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(two_model_catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Ok(())
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
            _req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, BackendError> {
            panic!("ChatPanicsAdapter::chat always panics (test double)");
        }
    }

    /// Critical (Opus Tier-2 review, probe P6): a panicking `chat` call
    /// must not leak `inflight` forever — otherwise a pending drain
    /// (`unload` deferred behind it) never completes, `active_op` stays
    /// stuck, and every *other* id reports `Busy` forever too. This is the
    /// same class of bug as C1, but in the `Chat` dispatch, which C1's
    /// original fix (`start_load`/`start_unload` only) didn't cover.
    #[tokio::test]
    async fn a_panicking_chat_does_not_leak_inflight_unload_completes_and_other_ids_stay_free() {
        let adapter = Arc::new(ChatPanicsAdapter {
            fence_path: std::sync::OnceLock::new(),
        });
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect("load a should succeed");
        let err = no_hang(handle.chat(
            ChatRequest {
                model: "a".to_string(),
                messages: vec![],
            },
            CancellationToken::new(),
        ))
        .await
        .expect_err("a panicking chat call must surface as an error, not hang");
        assert_eq!(err.reason_code(), "adapter_panicked");
        // If `inflight` had leaked, this would hang forever waiting for a
        // drain that can never observe it reach 0.
        no_hang(handle.unload("a"))
            .await
            .expect("unload must complete after the panicking chat, not hang forever");
        // If `active_op` had stayed stuck on "a", this would report `Busy`
        // instead of succeeding.
        no_hang(handle.load("b", 4.0, on_demand_policy()))
            .await
            .expect("a different id must not be busy after the panicking chat");
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
        let err = no_hang(handle.load("b", 20.0, on_demand_policy()))
            .await
            .expect_err("a has an in-flight chat: nothing evictable, b cannot be admitted");
        assert_eq!(err.reason_code(), "eviction_impossible");
        assert_eq!(adapter.unload_call_count("a"), 0);
        no_hang(chat_a)
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
        let err = no_hang(handle.chat(
            ChatRequest {
                model: "a".to_string(),
                messages: vec![],
            },
            CancellationToken::new(),
        ))
        .await
        .expect_err("a new chat during the drain (Stopping) must be rejected");
        assert_eq!(err.reason_code(), "model_unavailable");
        no_hang(chat_a)
            .await
            .unwrap()
            .expect("the original in-flight chat should still complete");
        no_hang(unload_a)
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

    /// K10/H2: a rejected retry must preserve an Error model's old footprint
    /// and keep a later unload from short-circuiting while it is still resident.
    #[tokio::test]
    async fn error_retry_rejection_preserves_old_occupancy_and_real_unload() {
        for recover_via_load in [false, true] {
            let adapter = Arc::new(MockAdapter::new(vec![
                ModelInfo {
                    id: "a".to_string(),
                    memory_gb: 8.0,
                },
                ModelInfo {
                    id: "b".to_string(),
                    memory_gb: 20.0,
                },
            ]));
            let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
            no_hang(handle.load("a", 8.0, resident_policy()))
                .await
                .unwrap();
            adapter.set_unload_script("a", vec![crate::mock::UnloadOutcome::Fail]);
            no_hang(handle.unload("a")).await.unwrap_err();
            assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
            adapter.set_load_script("a", vec![crate::mock::LoadOutcome::Fail]);
            no_hang(handle.load("a", 1.0, on_demand_policy()))
                .await
                .unwrap_err();
            assert_eq!(adapter.status().await.unwrap().used_gb, 8.0);
            assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
            let err = no_hang(handle.chat(
                ChatRequest {
                    model: "a".to_string(),
                    messages: vec![],
                },
                CancellationToken::new(),
            ))
            .await
            .unwrap_err();
            assert_eq!(err.reason_code(), "model_unavailable");
            assert_eq!(adapter.load_call_count("a"), 2);
            let err = no_hang(handle.load("b", 20.0, on_demand_policy()))
                .await
                .unwrap_err();
            assert_eq!(err.reason_code(), "eviction_impossible");
            assert_eq!(adapter.load_call_count("b"), 0);
            assert_eq!(adapter.unload_call_count("a"), 1);
            if recover_via_load {
                adapter.set_load_script("a", vec![crate::mock::LoadOutcome::Ok]);
                no_hang(handle.load("a", 4.0, on_demand_policy()))
                    .await
                    .unwrap();
                let status = handle.status().await.unwrap();
                assert_eq!(status.used_gb, 4.0);
                assert_eq!(status.loaded, vec!["a".to_string()]);
            }
            no_hang(handle.unload("a")).await.unwrap();
            assert_eq!(adapter.unload_call_count("a"), 2);
            assert_eq!(adapter.status().await.unwrap().used_gb, 0.0);
            assert_eq!(handle.status().await.unwrap().used_gb, 0.0);
        }
    }

    /// A load panic retains the durable global fence through retries and
    /// explicit unloads because the engine operation's state is unknown.
    #[tokio::test]
    async fn error_retry_panic_preserves_old_occupancy() {
        let handle = Supervisor::spawn(
            Arc::new(PanickingAdapter {
                fence_path: std::sync::OnceLock::new(),
            }),
            SupervisorConfig::default(),
        )
        .unwrap();
        let err = no_hang(handle.load("a", 8.0, on_demand_policy()))
            .await
            .unwrap_err();
        assert_eq!(err.reason_code(), "adapter_panicked");
        assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
        let err = no_hang(handle.load("a", 1.0, on_demand_policy()))
            .await
            .unwrap_err();
        assert_eq!(err.reason_code(), "model_unavailable");
        assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    }

    /// Medium (follow-up Opus review, prdaemon probe): a reload of an
    /// already-`Ready` model with a *different* policy that then fails
    /// must restore the old `Ready` state (policy + memory_gb), not settle
    /// on `Stopped` — the old instance was never actually torn down, so
    /// `Stopped` would tell the ledger its memory is free when it almost
    /// certainly still isn't (prdaemon-observed: 6 GiB budget, engine
    /// actually still holding 8 GiB).
    #[tokio::test]
    async fn a_failed_reload_restores_the_previous_ready_state_not_stopped() {
        let adapter = Arc::new(MockAdapter::new(evictable_catalog()));
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        no_hang(handle.load("a", 20.0, on_demand_policy()))
            .await
            .expect("initial load of a should succeed");
        adapter.set_load_script("a", vec![crate::mock::LoadOutcome::Fail]);
        no_hang(handle.load("a", 20.0, resident_policy()))
            .await
            .expect_err("the reload with a different (scripted-to-fail) policy must fail");
        // Decisive check: used_gb must still reflect a's original
        // footprint, not have dropped as if a were freed.
        let status = no_hang(handle.status())
            .await
            .expect("status should succeed");
        assert_eq!(
            status.used_gb, 20.0,
            "a failed reload must not silently free a's ledger entry"
        );
        assert_eq!(status.loaded, vec!["a".to_string()]);
        // a must still genuinely be servable, not just accounted-for.
        no_hang(handle.chat(
            ChatRequest {
                model: "a".to_string(),
                messages: vec![],
            },
            CancellationToken::new(),
        ))
        .await
        .expect("a must still be chattable after the failed reload");
        // A second model needing the same space must trigger *real*
        // eviction (adapter.unload("a") actually called), not wrongly
        // succeed as if a's space were already free.
        no_hang(handle.load("b", 20.0, on_demand_policy()))
            .await
            .expect("b should be admitted by evicting the still-genuinely-Ready a");
        assert_eq!(
            adapter.unload_call_count("a"),
            1,
            "a must be genuinely evicted (unload actually called), not treated as already-free"
        );
        let status2 = no_hang(handle.status())
            .await
            .expect("status should succeed");
        assert_eq!(status2.loaded, vec!["b".to_string()]);
    }

    /// Medium (follow-up Opus review): a reload's restored `Ready` state
    /// must not be short-circuited as an already-`Stopped` no-op when
    /// explicitly `unload`ed — that would make the still-genuinely-loaded
    /// instance unrecoverable through the public API.
    #[tokio::test]
    async fn a_restored_ready_model_after_a_failed_reload_can_still_be_unloaded_for_real() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect("initial load of a should succeed");
        adapter.set_load_script("a", vec![crate::mock::LoadOutcome::Fail]);
        no_hang(handle.load("a", 4.0, resident_policy()))
            .await
            .expect_err("the reload must fail");
        no_hang(handle.unload("a"))
            .await
            .expect("unload of the restored a should succeed");
        assert_eq!(
            adapter.unload_call_count("a"),
            1,
            "unload must actually call the adapter for real, not short-circuit as if a were already Stopped"
        );
    }

    /// Medium (follow-up Opus review): a *fresh* (non-reload) load whose
    /// `adapter.load` call times out is genuinely uncertain. A best-effort
    /// unload is still sent, but its acknowledgement cannot clear the slot.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_first_load_stays_charged_after_best_effort_release() {
        let adapter = Arc::new(MockAdapter::new(catalog()));
        adapter.set_load_delay("a", std::time::Duration::from_secs(3600));
        let handle = Supervisor::spawn(
            adapter.clone(),
            SupervisorConfig {
                adapter_call_timeout: std::time::Duration::from_millis(50),
                ..SupervisorConfig::default()
            },
        )
        .expect("spawn should succeed");
        let err = no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect_err("a timed-out first load must surface as an error, not hang");
        assert_eq!(err.reason_code(), "adapter_timed_out");
        assert_eq!(
            adapter.unload_call_count("a"),
            1,
            "a timeout must trigger a best-effort release attempt, not assume nothing happened"
        );
        assert_eq!(handle.status().await.unwrap().used_gb, 4.0);
    }

    /// A minimal `RuntimeAdapter`: `load` always succeeds but
    /// `probe_ready` always fails — exercises H1's "probe fails, but a
    /// best-effort `unload` confirms release" path. `unload_ok` controls
    /// whether that best-effort release itself succeeds.
    struct ProbeAlwaysFailsAdapter {
        unload_ok: bool,
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for ProbeAlwaysFailsAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
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

    /// A `probe_ready` failure remains fenced even when cleanup reports an
    /// empty engine snapshot: the accepted load may still allocate later.
    #[tokio::test]
    async fn probe_failure_with_empty_status_still_occupies_the_ledger() {
        let adapter = Arc::new(ProbeAlwaysFailsAdapter {
            unload_ok: true,
            fence_path: std::sync::OnceLock::new(),
        });
        let handle =
            Supervisor::spawn(adapter, SupervisorConfig::default()).expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("probe_ready always fails, so load must fail too");
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(
            status.used_gb, 4.0,
            "an accepted load remains fenced without operation-termination evidence"
        );
    }

    /// Negative contrast: the same probe failure, but the best-effort
    /// `unload` *also* fails — real state is genuinely unknown, so it must
    /// stay `Error` (occupying budget), never guessed as free.
    #[tokio::test]
    async fn probe_failure_with_failed_release_still_occupies_the_ledger() {
        let adapter = Arc::new(ProbeAlwaysFailsAdapter {
            unload_ok: false,
            fence_path: std::sync::OnceLock::new(),
        });
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

    /// A failed probe fences the accepted operation, so retries cannot
    /// replace its reservation with a smaller or larger estimate.
    #[tokio::test]
    async fn error_retry_probe_failure_preserves_old_occupancy() {
        let adapter = Arc::new(ProbeAlwaysFailsAdapter {
            unload_ok: false,
            fence_path: std::sync::OnceLock::new(),
        });
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        no_hang(handle.load("a", 8.0, on_demand_policy()))
            .await
            .unwrap_err();
        assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
        let err = handle.load("a", 1.0, on_demand_policy()).await.unwrap_err();
        assert_eq!(err.reason_code(), "model_unavailable");
        assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    }

    /// A minimal `RuntimeAdapter` whose `load` reports
    /// `BackendError::LoadUnconfirmed` — the engine accepted the load, a
    /// later step (pin / verify) failed — as a real oMLX two-step load can.
    /// `unload_ok` controls whether the follow-up best-effort release works.
    struct LoadUnconfirmedAdapter {
        unload_ok: bool,
        unload_calls: std::sync::atomic::AtomicU32,
        ready_once: std::sync::atomic::AtomicBool,
        chat_entered: tokio::sync::Notify,
        release_chat: tokio::sync::Notify,
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    impl LoadUnconfirmedAdapter {
        fn new(unload_ok: bool) -> Self {
            Self {
                unload_ok,
                unload_calls: std::sync::atomic::AtomicU32::new(0),
                ready_once: std::sync::atomic::AtomicBool::new(false),
                chat_entered: tokio::sync::Notify::new(),
                release_chat: tokio::sync::Notify::new(),
                fence_path: std::sync::OnceLock::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for LoadUnconfirmedAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(catalog())
        }
        async fn load(&self, id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            if self
                .ready_once
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(());
            }
            Err(BackendError::load_unconfirmed(
                id,
                "status check after POST load failed (test double)",
            ))
        }
        async fn unload(&self, id: &str) -> Result<(), BackendError> {
            self.unload_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.unload_ok {
                Ok(())
            } else {
                Err(BackendError::Upstream {
                    message: format!("LoadUnconfirmedAdapter refuses to unload {id} (test double)"),
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
        async fn probe_ready(&self, _id: &str) -> Result<bool, BackendError> {
            Ok(true)
        }
        async fn chat(
            &self,
            req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, BackendError> {
            self.chat_entered.notify_one();
            self.release_chat.notified().await;
            Ok(ChatResponse {
                model: req.model,
                content: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn ready_reload_cannot_release_a_model_with_inflight_chat() {
        use std::sync::atomic::Ordering::SeqCst;
        let adapter = Arc::new(LoadUnconfirmedAdapter::new(true));
        adapter.ready_once.store(true, SeqCst);
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        no_hang(handle.load("a", 8.0, on_demand_policy()))
            .await
            .unwrap();
        let h = handle.clone();
        let chat = tokio::spawn(async move {
            h.chat(
                ChatRequest {
                    model: "a".into(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
        });
        no_hang(adapter.chat_entered.notified()).await;
        no_hang(handle.load("a", 8.0, on_demand_policy()))
            .await
            .unwrap();
        let err = no_hang(handle.load("a", 1.0, resident_policy()))
            .await
            .unwrap_err();
        assert_eq!(adapter.unload_calls.load(SeqCst), 0);
        assert_eq!(err.reason_code(), "supervisor_busy");
        assert!(!chat.is_finished());
        let status = handle.status().await.unwrap();
        assert_eq!(status.used_gb, 8.0);
        assert_eq!(status.loaded, vec!["a"]);
        adapter.release_chat.notify_one();
        no_hang(chat).await.unwrap().unwrap();
        let err = no_hang(handle.load("a", 1.0, resident_policy()))
            .await
            .unwrap_err();
        assert_eq!(err.reason_code(), "load_unconfirmed");
        assert_eq!(adapter.unload_calls.load(SeqCst), 1);
        assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
    }

    /// An unconfirmed load remains charged because unload/status cannot
    /// prove the original engine operation has stopped.
    #[tokio::test]
    async fn an_unconfirmed_load_stays_charged_after_unload_and_empty_status() {
        let adapter = Arc::new(LoadUnconfirmedAdapter::new(true));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        let err = handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("an unconfirmed load must surface as an error");
        assert_eq!(err.reason_code(), "load_unconfirmed");
        assert_eq!(
            adapter
                .unload_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(status.used_gb, 4.0);
    }

    /// Both successful and failed unload acknowledgements are insufficient
    /// for an unconfirmed load.
    #[tokio::test]
    async fn an_unconfirmed_load_whose_release_fails_still_occupies_the_ledger() {
        let adapter = Arc::new(LoadUnconfirmedAdapter::new(false));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default())
            .expect("spawn should succeed");
        handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("an unconfirmed load must surface as an error");
        let status = handle.status().await.expect("status should succeed");
        assert_eq!(
            status.used_gb, 4.0,
            "memory that could not be confirmed released must stay on the ledger"
        );
    }

    /// The adapter returns an empty snapshot and acknowledges unload while a
    /// detached engine-side load remains pending. Releasing `finish_load`
    /// later simulates allocation after the Supervisor's timeout.
    struct DelayedAllocationAdapter {
        loaded: Arc<std::sync::atomic::AtomicBool>,
        allocation_finished: Arc<tokio::sync::Notify>,
        finish_load: Arc<tokio::sync::Notify>,
        load_started: tokio::sync::Notify,
        load_calls: std::sync::atomic::AtomicU32,
        unload_calls: std::sync::atomic::AtomicU32,
        load_returns_ok: bool,
        probe_hangs: bool,
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for DelayedAllocationAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            self.load_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.load_started.notify_one();
            let loaded = self.loaded.clone();
            let finish = self.finish_load.clone();
            let allocation_finished = self.allocation_finished.clone();
            tokio::spawn(async move {
                finish.notified().await;
                loaded.store(true, std::sync::atomic::Ordering::SeqCst);
                allocation_finished.notify_one();
            });
            if self.load_returns_ok {
                Ok(())
            } else {
                std::future::pending().await
            }
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            self.unload_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            let loaded = self.loaded.load(std::sync::atomic::Ordering::SeqCst);
            Ok(BackendStatus {
                pressure: Pressure::Ok,
                used_gb: if loaded { 8.0 } else { 0.0 },
                model_memory_max_gb: 0.0,
                loaded: if loaded { vec!["a".into()] } else { Vec::new() },
            })
        }
        async fn probe_ready(&self, _id: &str) -> Result<bool, BackendError> {
            if self.probe_hangs {
                std::future::pending().await
            } else {
                Ok(false)
            }
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

    async fn delayed_load_scenario(accepted: bool, probe_hangs: bool, restart: bool) {
        let adapter = Arc::new(DelayedAllocationAdapter {
            loaded: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            allocation_finished: Arc::new(tokio::sync::Notify::new()),
            finish_load: Arc::new(tokio::sync::Notify::new()),
            load_started: tokio::sync::Notify::new(),
            load_calls: std::sync::atomic::AtomicU32::new(0),
            unload_calls: std::sync::atomic::AtomicU32::new(0),
            load_returns_ok: accepted,
            probe_hangs,
            fence_path: std::sync::OnceLock::new(),
        });
        let config = SupervisorConfig {
            budget_gb: 10.0,
            adapter_call_timeout: std::time::Duration::from_millis(30),
            probe_max_attempts: 2,
            ..SupervisorConfig::default()
        };
        let (tx, actor) = spawn_actor(adapter.clone(), config.clone());
        let handle = SupervisorHandle { tx };
        let h = handle.clone();
        let load = tokio::spawn(async move { h.load("a", 8.0, on_demand_policy()).await });
        no_hang(adapter.load_started.notified()).await;
        let err = no_hang(load).await.unwrap().unwrap_err();
        assert_eq!(
            err.reason_code(),
            if accepted && !probe_hangs {
                "probe_timed_out"
            } else {
                "adapter_timed_out"
            }
        );
        let initial = handle.status().await.unwrap();
        assert_eq!(initial.used_gb, 8.0);
        assert!(initial.loaded.is_empty(), "unconfirmed loads are not Ready");
        let empty = adapter.status().await.unwrap();
        assert_eq!(empty.used_gb, 0.0);
        assert!(empty.loaded.is_empty());
        assert_eq!(
            adapter
                .unload_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            handle.unload("a").await.unwrap_err().reason_code(),
            "model_unavailable"
        );
        assert_eq!(
            handle
                .load("a", 1.0, on_demand_policy())
                .await
                .unwrap_err()
                .reason_code(),
            "model_unavailable"
        );
        assert_eq!(
            handle
                .load("b", 5.0, on_demand_policy())
                .await
                .unwrap_err()
                .reason_code(),
            "eviction_impossible"
        );

        let handle = if restart {
            drop(handle);
            no_hang(actor).await.unwrap();
            let restarted = Supervisor::spawn(adapter.clone(), config).unwrap();
            let err = restarted
                .load("b", 5.0, on_demand_policy())
                .await
                .unwrap_err();
            assert_eq!(err.reason_code(), "internal");
            assert!(err.to_string().contains("durable load fence"));
            assert_eq!(
                adapter.load_calls.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            restarted
        } else {
            handle
        };

        adapter.finish_load.notify_one();
        no_hang(adapter.allocation_finished.notified()).await;
        let actual = adapter.status().await.unwrap();
        assert_eq!(actual.used_gb, 8.0);
        assert_eq!(actual.loaded, vec!["a"]);
        if !restart {
            assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
        }
        let err = handle.load("b", 5.0, on_demand_policy()).await.unwrap_err();
        assert_eq!(
            err.reason_code(),
            if restart {
                "internal"
            } else {
                "eviction_impossible"
            }
        );
        assert_eq!(
            adapter.load_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_timed_out_load_stays_charged_and_cannot_be_released_retried_or_evicted() {
        delayed_load_scenario(false, false, false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_load_with_probe_timeout_empty_status_and_delayed_allocation_stays_fenced() {
        for probe_hangs in [false, true] {
            delayed_load_scenario(true, probe_hangs, false).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_load_fence_survives_supervisor_restart_before_delayed_allocation() {
        for (accepted, probe_hangs) in [(false, false), (true, false), (true, true)] {
            delayed_load_scenario(accepted, probe_hangs, true).await;
        }
    }

    /// A minimal `RuntimeAdapter` whose `status().used_gb` only drops after
    /// `calls_until_drop` calls — for proving H2's confirmation actually
    /// polls (multiple `status` calls) rather than trusting the first
    /// reading (or, worse, just trusting `unload()`'s own `Ok`).
    struct LaggingReleaseAdapter {
        status_calls: std::sync::Mutex<u32>,
        calls_until_drop: u32,
        loaded: std::sync::atomic::AtomicBool,
        releasing: std::sync::atomic::AtomicBool,
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for LaggingReleaseAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(evictable_catalog())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            self.loaded.store(true, std::sync::atomic::Ordering::SeqCst);
            self.releasing
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            self.releasing
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            let mut calls = self
                .status_calls
                .lock()
                .expect("test mutex is never poisoned");
            let releasing = self.releasing.load(std::sync::atomic::Ordering::SeqCst);
            *calls += u32::from(releasing);
            let used_gb = if !self.loaded.load(std::sync::atomic::Ordering::SeqCst)
                || (releasing && *calls >= self.calls_until_drop)
            {
                0.0
            } else {
                20.0
            };
            Ok(BackendStatus {
                pressure: Pressure::Ok,
                used_gb,
                model_memory_max_gb: 100.0,
                loaded: if used_gb > 0.0 {
                    vec!["a".into()]
                } else {
                    Vec::new()
                },
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
            loaded: std::sync::atomic::AtomicBool::new(false),
            releasing: std::sync::atomic::AtomicBool::new(false),
            fence_path: std::sync::OnceLock::new(),
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

    /// K11/H3: exhausted confirmation must reject admission and keep victims charged.
    #[tokio::test(start_paused = true)]
    async fn eviction_rejects_after_confirmation_gives_up() {
        let adapter = Arc::new(LaggingReleaseAdapter {
            status_calls: std::sync::Mutex::new(0),
            calls_until_drop: u32::MAX, // never drops
            loaded: std::sync::atomic::AtomicBool::new(false),
            releasing: std::sync::atomic::AtomicBool::new(false),
            fence_path: std::sync::OnceLock::new(),
        });
        let handle = Supervisor::spawn(adapter.clone(), tight_budget_config())
            .expect("spawn should succeed");
        handle
            .load("a", 20.0, on_demand_policy())
            .await
            .expect("load a should succeed");
        let err = handle
            .load("b", 20.0, on_demand_policy())
            .await
            .expect_err("unconfirmed capacity must not admit b");
        assert_eq!(err.reason_code(), "eviction_failed");
        assert_eq!(handle.status().await.unwrap().used_gb, 20.0);
    }

    /// Successful unload replies with deliberately unreliable release evidence.
    struct UnverifiedReleaseAdapter {
        inner: MockAdapter,
        reported: std::sync::Mutex<Option<BackendStatus>>,
        startup: std::sync::atomic::AtomicBool,
        probe_fails: bool,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for UnverifiedReleaseAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            self.inner.load_fence_path()
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            self.inner.list().await
        }
        async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            self.inner.load(id, policy).await
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            Ok(()) // Engine acknowledges without actually freeing anything.
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            if self
                .startup
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return self.inner.status().await;
            }
            self.reported
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| BackendError::internal("status unavailable"))
        }
        async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
            if self.probe_fails {
                Err(BackendError::internal("probe failed"))
            } else {
                self.inner.probe_ready(id).await
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

    #[tokio::test(start_paused = true)]
    async fn release_confirmation_accepts_rounding_but_rejects_invalid_evidence() {
        let ledger_sum = (0.2_f64 + 0.3) + 0.1;
        let engine_sum = (0.1_f64 + 0.2) + 0.3;
        assert_eq!(ledger_sum, 0.6);
        assert_eq!(engine_sum, 0.6000000000000001);

        let config = SupervisorConfig {
            release_confirm_interval: std::time::Duration::ZERO,
            release_confirm_max_attempts: 1,
            ..SupervisorConfig::default()
        };
        let adapter_for = |used_gb, loaded: Vec<String>| -> Arc<dyn RuntimeAdapter> {
            Arc::new(UnverifiedReleaseAdapter {
                startup: std::sync::atomic::AtomicBool::new(false),
                inner: MockAdapter::new(catalog()),
                reported: std::sync::Mutex::new(Some(BackendStatus {
                    pressure: Pressure::Ok,
                    used_gb,
                    model_memory_max_gb: 1.0,
                    loaded,
                })),
                probe_fails: false,
            })
        };

        confirm_memory_released(
            &adapter_for(engine_sum, vec![]),
            &["a".into()],
            ledger_sum,
            &config,
        )
        .await
        .expect("float summation order must not reject a fully freed target");

        for (used_gb, loaded) in [
            (ledger_sum + CAPACITY_EPSILON_GB * 2.0, vec![]),
            (ledger_sum, vec!["a".into()]),
            (f64::NAN, vec![]),
            (f64::INFINITY, vec![]),
            (-1.0, vec![]),
        ] {
            assert!(
                confirm_memory_released(
                    &adapter_for(used_gb, loaded),
                    &["a".into()],
                    ledger_sum,
                    &config,
                )
                .await
                .is_err(),
                "invalid or insufficient release evidence must fail closed"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn eviction_rejects_partial_or_invalid_release_evidence() {
        for evidence in [
            Some((4.0, vec!["b"])), // Some memory freed, one of two victims remains.
            Some((4.0, vec![])),    // All victims absent, still insufficient capacity.
            Some((0.0, vec!["a"])), // Enough capacity, but a victim remains.
            Some((f64::NAN, vec![])),
            Some((-1.0, vec![])),
            None, // Status failure cannot be treated as release confirmation.
        ] {
            let adapter = Arc::new(UnverifiedReleaseAdapter {
                startup: std::sync::atomic::AtomicBool::new(true),
                inner: MockAdapter::new(
                    [("a", 4.0), ("b", 4.0), ("c", 8.0)]
                        .into_iter()
                        .map(|(id, memory_gb)| ModelInfo {
                            id: id.into(),
                            memory_gb,
                        })
                        .collect(),
                ),
                reported: std::sync::Mutex::new(Some(BackendStatus {
                    pressure: Pressure::Ok,
                    used_gb: 8.0,
                    model_memory_max_gb: 8.0,
                    loaded: vec!["a".into(), "b".into()],
                })),
                probe_fails: false,
            });
            let handle = Supervisor::spawn(
                adapter.clone(),
                SupervisorConfig {
                    budget_gb: 8.0,
                    ..SupervisorConfig::default()
                },
            )
            .unwrap();
            handle.load("a", 4.0, on_demand_policy()).await.unwrap();
            handle.load("b", 4.0, on_demand_policy()).await.unwrap();
            *adapter.reported.lock().unwrap() = evidence.map(|(used_gb, ids)| BackendStatus {
                pressure: Pressure::Ok,
                used_gb,
                model_memory_max_gb: 8.0,
                loaded: ids.into_iter().map(String::from).collect(),
            });
            let result = handle.load("c", 8.0, on_demand_policy()).await;
            assert_eq!(result.unwrap_err().reason_code(), "eviction_failed");
            assert_eq!(adapter.inner.load_call_count("c"), 0);
            assert_eq!(handle.status().await.unwrap().used_gb, 8.0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_and_standalone_unload_keep_unconfirmed_occupancy() {
        use crate::mock::LoadOutcome;

        for (probe_fails, oom, loaded) in [
            (false, true, vec!["a".into()]),
            (true, false, vec!["a".into()]),
            (false, false, vec!["a".into()]),
            (false, true, vec![]),
            (true, false, vec![]),
            (false, false, vec![]),
        ] {
            let adapter = Arc::new(UnverifiedReleaseAdapter {
                startup: std::sync::atomic::AtomicBool::new(true),
                inner: MockAdapter::new(two_model_catalog()),
                reported: std::sync::Mutex::new(Some(BackendStatus {
                    pressure: Pressure::Ok,
                    used_gb: 4.0,
                    model_memory_max_gb: 24.0,
                    loaded,
                })),
                probe_fails,
            });
            if oom {
                adapter
                    .inner
                    .set_load_script("a", vec![LoadOutcome::Oom, LoadOutcome::Ok]);
            }
            let handle = Supervisor::spawn(
                adapter.clone(),
                SupervisorConfig {
                    budget_gb: 4.0,
                    ..SupervisorConfig::default()
                },
            )
            .unwrap();
            if probe_fails || oom {
                handle.load("a", 4.0, on_demand_policy()).await.unwrap_err();
            } else {
                handle.load("a", 4.0, on_demand_policy()).await.unwrap();
                handle.unload("a").await.unwrap_err();
            }
            assert_eq!(
                adapter.inner.load_call_count("a"),
                1,
                "unconfirmed OOM must not retry"
            );
            let status = handle.status().await.unwrap();
            assert_eq!(status.used_gb, 4.0);
            assert!(status.loaded.is_empty());
            assert!(handle.load("b", 4.0, on_demand_policy()).await.is_err());
            assert_eq!(adapter.inner.load_call_count("b"), 0);
        }
    }

    /// M1: the actor task itself must exit once every `SupervisorHandle` is
    /// dropped, not run forever. Bypasses `Supervisor::spawn` to capture the
    /// actor's own `JoinHandle` (which the public API doesn't expose, on
    /// purpose — this is an internal detail, not something callers should
    /// depend on).
    #[tokio::test]
    async fn actor_exits_once_every_handle_is_dropped() {
        let adapter: Arc<dyn RuntimeAdapter> = Arc::new(MockAdapter::new(catalog()));
        // Goes through the exact same `spawn_actor` helper `Supervisor::
        // spawn` itself uses (rather than reimplementing its setup here),
        // so this actually exercises the real `downgrade`-not-`clone`
        // choice instead of only the test's own copy of it.
        let (tx, join) = spawn_actor(adapter, SupervisorConfig::default());
        let handle = SupervisorHandle { tx };
        handle
            .list()
            .await
            .expect("the actor should be responsive while a handle exists");
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(1), join)
            .await
            .expect("the actor task must exit shortly after the last handle is dropped")
            .expect("the actor task must not panic on exit");
    }

    /// M3: a detected invariant violation must poison the actor and make it
    /// fail closed for every future call, not `panic!` and take the whole
    /// actor task down with it. Feeds a fabricated `OpDone` for an id the
    /// ledger has never heard of directly into the actor's internal
    /// channel — real code can never produce this (see
    /// `set_state_or_poison`'s doc comment), but it is exactly the class of
    /// corruption M3 is about.
    #[tokio::test]
    async fn an_invariant_violation_poisons_the_actor_and_future_calls_fail_closed() {
        let adapter: Arc<dyn RuntimeAdapter> = Arc::new(MockAdapter::new(catalog()));
        let (tx, _join) = spawn_actor(adapter, SupervisorConfig::default());
        let handle = SupervisorHandle { tx: tx.clone() };

        // No `sleep` needed: `tx` is a single-consumer mpsc channel, so this
        // message is guaranteed processed strictly before the `load` call
        // below, whose own `send` only starts after this `.await` completes.
        tx.send(ActorMsg::OpDone {
            id: "ghost".to_string(),
            outcome: OpOutcome::Load {
                result: Ok(()),
                victim_results: Vec::new(),
                aggregate_status: None,
                aggregate_target_credit_gb: 0.0,
                failure_outcome: LoadFailureOutcome::Settle(ModelState::Error),
            },
        })
        .await
        .expect("the internal channel should accept the message");

        let err = handle
            .load("a", 4.0, on_demand_policy())
            .await
            .expect_err("a poisoned actor must fail closed, not silently keep operating");
        assert_eq!(err.reason_code(), "state_invariant_violated");
        // Negative contrast: `list`/`status` must fail closed too, not just
        // `load` — poisoning is actor-wide, not per-command-type.
        let status_err = handle
            .status()
            .await
            .expect_err("status must also fail closed once poisoned");
        assert_eq!(status_err.reason_code(), "state_invariant_violated");
    }

    /// A minimal `RuntimeAdapter` recording the timestamp of every `load`/
    /// `status` call: `load` OOMs on its first call, succeeds on every
    /// later one — for asserting exactly *when* the OOM retry and its
    /// confirmation actually happen relative to each other (M10/M11, Opus
    /// Tier-2 review).
    struct OomRetryTimingAdapter {
        load_times: std::sync::Mutex<Vec<tokio::time::Instant>>,
        status_times: std::sync::Mutex<Vec<tokio::time::Instant>>,
        fence_path: std::sync::OnceLock<std::path::PathBuf>,
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for OomRetryTimingAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(self
                .fence_path
                .get_or_init(crate::mock::temporary_load_fence_path)
                .clone())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(catalog())
        }
        async fn load(&self, id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            let mut times = self
                .load_times
                .lock()
                .expect("test mutex is never poisoned");
            let is_first = times.is_empty();
            times.push(tokio::time::Instant::now());
            if is_first {
                Err(BackendError::oom(id))
            } else {
                Ok(())
            }
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            Ok(())
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            let mut times = self
                .status_times
                .lock()
                .expect("test mutex is never poisoned");
            let before_load = self.load_times.lock().unwrap().is_empty();
            // Startup is empty; after OOM, one poll precedes release confirmation.
            let used_gb = if !before_load && times.is_empty() {
                100.0
            } else {
                0.0
            };
            if !before_load {
                times.push(tokio::time::Instant::now());
            }
            Ok(BackendStatus {
                pressure: Pressure::Ok,
                used_gb,
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

    /// M10 + M11 (Opus Tier-2 review): the OOM circuit breaker's one retry
    /// must (a) wait at least `oom_retry_backoff` before retrying, and
    /// (b) call `confirm_memory_released` (i.e. poll `status()`) at least
    /// once *between* the OOM'd attempt and the retry — not skip either
    /// step. Uses `tokio::time::Instant` (respects the paused clock) rather
    /// than a real wall-clock measurement.
    #[tokio::test(start_paused = true)]
    async fn oom_retry_confirms_release_and_backs_off_before_retrying() {
        let adapter = Arc::new(OomRetryTimingAdapter {
            load_times: std::sync::Mutex::new(Vec::new()),
            status_times: std::sync::Mutex::new(Vec::new()),
            fence_path: std::sync::OnceLock::new(),
        });
        let backoff = std::time::Duration::from_millis(200);
        let handle = Supervisor::spawn(
            adapter.clone(),
            SupervisorConfig {
                oom_retry_backoff: backoff,
                ..SupervisorConfig::default()
            },
        )
        .expect("spawn should succeed");
        no_hang(handle.load("a", 4.0, on_demand_policy()))
            .await
            .expect("load should self-heal after the OOM retry");

        let load_times = adapter
            .load_times
            .lock()
            .expect("test mutex is never poisoned")
            .clone();
        assert_eq!(load_times.len(), 2, "exactly one OOM retry must happen");
        let gap = load_times[1] - load_times[0];
        assert!(
            gap >= backoff,
            "M10: the retry must wait at least oom_retry_backoff ({backoff:?}), got {gap:?}"
        );

        let status_times = adapter
            .status_times
            .lock()
            .expect("test mutex is never poisoned")
            .clone();
        // The first poll reports high, so confirmation must poll again.
        let calls_in_window = status_times
            .iter()
            .filter(|t| **t >= load_times[0] && **t < load_times[1])
            .count();
        assert!(
            calls_in_window >= 2,
            "M11: confirm_memory_released must poll status() until release is confirmed between the OOM'd attempt and the retry, got {calls_in_window} call(s)"
        );
    }
}
