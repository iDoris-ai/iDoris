//! Eviction planning as a **pure function** — no IO, no randomness, no
//! clock reads. Per `docs/research/Rust基础选型-2026-09-27.md` §4: "驱逐决策
//! 写成纯函数,副作用只通过 Effects 接口发出" (llama-swap's
//! `internal/router/design.md`, and the root cause of llama-swap Issue
//! #946 — a model permanently stuck because eviction and process-launch
//! logic were entangled with side effects).
//!
//! [`plan_eviction`] takes a [`Snapshot`] of the ledger plus the incoming
//! [`ModelReq`] and returns a decision only; [`crate::supervisor`] is the
//! sole place that turns that decision into actual `unload` calls via the
//! `Effects` boundary.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A model's lifecycle state, tracked by [`crate::supervisor::Supervisor`].
/// `Launching` covers "accepted but not yet actively calling the adapter"
/// (including sitting in the global load/evict queue behind another
/// in-flight operation) — see [`occupies_budget`] for which states count
/// against the memory budget (see [`Snapshot`]) and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Launching,
    Loading,
    Ready,
    Error,
    Stopping,
    Stopped,
}

/// One ledger entry as [`plan_eviction`] sees it. `last_used_seq` is a
/// monotonically increasing counter the Supervisor bumps on every use
/// (load-completed, chat) — using a counter instead of a wall-clock
/// timestamp keeps this struct (and therefore `plan_eviction`) free of any
/// dependency on real time, so tests can construct exact LRU orderings
/// without racing a clock.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub memory_gb: f64,
    pub state: ModelState,
    /// `true` for a `resident`/pinned model — never a candidate for
    /// eviction regardless of how stale its `last_used_seq` is.
    pub pinned: bool,
    pub last_used_seq: u64,
}

/// The ledger `plan_eviction` reasons over. `budget_gb` is the configured
/// global memory ceiling; `models` is every model the Supervisor currently
/// knows about (any state) *except* the one being requested in
/// [`ModelReq`] — see [`plan_eviction`]'s doc comment for why.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub budget_gb: f64,
    pub models: Vec<ModelEntry>,
}

/// The incoming request `plan_eviction` must make room for.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelReq {
    pub id: String,
    pub memory_gb: f64,
}

/// What the caller must do before it may load [`ModelReq`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvictionPlan {
    /// Enough budget is free; load without evicting anything.
    NotNeeded,
    /// Evict exactly these ids, in this order (oldest-used first), then
    /// load. Every id here is `Ready` and unpinned at snapshot time.
    Evict(Vec<String>),
}

/// Per the "不静默" invariant: `plan_eviction` fails explicitly rather than
/// ever guessing past a malformed input or an unsatisfiable request.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PlanEvictionError {
    /// No combination of evictions can free enough budget for `model_id`.
    #[error(
        "cannot admit {model_id}: short {shortfall_gb:.3} GiB (budget {budget_gb:.3} GiB, only {evictable_gb:.3} GiB evictable)"
    )]
    InsufficientCapacity {
        model_id: String,
        /// How much *more* budget is needed beyond what's already free —
        /// **not** `model_id`'s total size. Deliberately distinct from
        /// `evictable_gb` so the message reads as "short by X", not a
        /// confusing restatement of the request size.
        shortfall_gb: f64,
        budget_gb: f64,
        evictable_gb: f64,
    },
    /// A capacity value (`budget_gb`, `need.memory_gb`, or some model's
    /// `memory_gb`) was negative, `NaN`, or infinite. Every comparison in
    /// this module assumes real, finite, non-negative GiB quantities;
    /// silently proceeding with e.g. a `NaN` would make every `<=`/`>=`
    /// comparison `false` and could fabricate an `Evict` plan that doesn't
    /// actually free enough budget (or the reverse — reject an admissible
    /// request). Fail closed instead.
    #[error("invalid capacity value for {field}: {value} (must be finite and >= 0)")]
    InvalidCapacity { field: &'static str, value: f64 },
    /// `Snapshot::models` contained an entry for the model being requested
    /// — a model can't need to evict itself (see [`Snapshot`]'s doc
    /// comment). Kept distinct from [`PlanEvictionError::InvalidCapacity`]:
    /// this is a caller contract violation, not a malformed number, and
    /// conflating the two would misreport a perfectly valid `memory_gb` as
    /// "invalid" in the error message and `reason_code()`.
    #[error("snapshot contained an entry for the requested model {model_id}")]
    SnapshotContainsRequestedModel { model_id: String },
}

impl PlanEvictionError {
    pub fn reason_code(&self) -> &'static str {
        match self {
            PlanEvictionError::InsufficientCapacity { .. } => "eviction_impossible",
            PlanEvictionError::InvalidCapacity { .. } => "invalid_capacity_value",
            PlanEvictionError::SnapshotContainsRequestedModel { .. } => {
                "snapshot_contains_requested_model"
            }
        }
    }
}

/// Two GiB quantities within this of each other are treated as equal for
/// the purposes of an admission decision. Binary floating point can't
/// represent every decimal GiB value exactly (e.g. `0.3 - 0.1` is
/// `0.19999999999999998`, not `0.2`), so an exact `<=`/`>=` comparison at a
/// boundary can reject an admissible request or admit one that doesn't
/// actually fit. `1e-6` GiB (~1 KiB) is far below any real model/engine
/// memory measurement's precision, so it only absorbs float noise, never
/// a real difference.
const CAPACITY_EPSILON_GB: f64 = 1e-6;

fn validate_capacity(field: &'static str, value: f64) -> Result<(), PlanEvictionError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(PlanEvictionError::InvalidCapacity { field, value })
    }
}

/// Which states count against the memory budget. Deliberately
/// conservative — anything whose memory is not *confirmed* free is
/// counted as still occupying it:
/// - `Loading`/`Ready`: obviously occupying memory (§4: "全局内存账本...只有
///   Ready/Loading 状态的模型计入占用" describes the common case).
/// - `Stopping`: an `unload` has been requested but the Supervisor's
///   confirm-released polling (see the crate-level Supervisor docs) has
///   not yet observed the engine actually free it — treating it as
///   already-free here would let a concurrent decision overcommit budget
///   that isn't actually available yet.
/// - `Error`: the model's true engine-side state is unknown (a load could
///   have partially completed before failing, an eviction's `unload` call
///   could have failed leaving the target's real state uncertain) — per
///   "不静默", uncertain must not be treated as "definitely free".
/// - `Launching` (queued, adapter never yet called) and `Stopped`
///   (confirmed released) are the only states that do **not** occupy
///   budget.
fn occupies_budget(state: ModelState) -> bool {
    matches!(
        state,
        ModelState::Loading | ModelState::Ready | ModelState::Stopping | ModelState::Error
    )
}

/// Decide what must be evicted (if anything) to admit `need`. Pure: same
/// inputs always produce the same output, no IO, no RNG. `state.models`
/// must not contain an entry for `need.id` (a model can't need to evict
/// itself — see [`Snapshot`]'s doc comment); a caller that violates this
/// gets a hard [`PlanEvictionError`], not a silent filter, since a pure
/// function should not paper over its caller's contract violations.
pub fn plan_eviction(state: &Snapshot, need: ModelReq) -> Result<EvictionPlan, PlanEvictionError> {
    validate_capacity("budget_gb", state.budget_gb)?;
    validate_capacity("need.memory_gb", need.memory_gb)?;
    for model in &state.models {
        validate_capacity("model.memory_gb", model.memory_gb)?;
    }
    // The documented `Snapshot` contract is "every model *except* the one
    // being requested" — a caller handing back `need.id` inside `models`
    // has violated that contract, and per "不静默" this must be a hard
    // error, not silently filtered out. Silently dropping it would treat
    // whatever memory that entry claims (possibly still-occupied, if its
    // state is `Error`/`Stopping`) as simply not existing, which can
    // overcommit the budget.
    if state.models.iter().any(|m| m.id == need.id) {
        return Err(PlanEvictionError::SnapshotContainsRequestedModel { model_id: need.id });
    }

    let occupied_gb: f64 = state
        .models
        .iter()
        .filter(|m| occupies_budget(m.state))
        .map(|m| m.memory_gb)
        .sum();
    validate_capacity("sum of occupied model.memory_gb", occupied_gb)?;
    let available_gb = state.budget_gb - occupied_gb;

    if need.memory_gb <= available_gb + CAPACITY_EPSILON_GB {
        return Ok(EvictionPlan::NotNeeded);
    }
    let shortfall_gb = need.memory_gb - available_gb;
    // `need.memory_gb` and `available_gb` are each individually finite, but
    // their difference is not guaranteed to be: e.g. a very large `need`
    // combined with a very negative `available_gb` (heavily over-budget
    // already) can overflow this subtraction to `inf`, which would then
    // make every later `>=`/`<` comparison against it meaningless.
    validate_capacity("shortfall_gb", shortfall_gb)?;

    let mut candidates: Vec<&ModelEntry> = state
        .models
        .iter()
        // Zero-capacity entries would be valid to "evict" but pointless:
        // freeing 0 GiB never helps close the shortfall, and unloading one
        // is a real `unload` call that can fail (see the crate-level
        // Supervisor docs) — one that would gain nothing but could still
        // block reaching a candidate that actually matters. Excluding them
        // here, not just relying on them naturally sorting last, keeps the
        // loop below from ever choosing a no-op eviction.
        .filter(|m| m.state == ModelState::Ready && !m.pinned && m.memory_gb > 0.0)
        .collect();
    // LRU: evict the least-recently-used first. `last_used_seq` is a
    // Supervisor-maintained monotonic counter, never wall-clock time (see
    // the struct doc comment), so this ordering is exact and
    // deterministic.
    candidates.sort_by_key(|m| m.last_used_seq);

    let mut freed_gb = 0.0;
    let mut chosen = Vec::new();
    for candidate in &candidates {
        if freed_gb + CAPACITY_EPSILON_GB >= shortfall_gb {
            break;
        }
        freed_gb += candidate.memory_gb;
        chosen.push(candidate.id.clone());
    }
    validate_capacity("cumulative freed_gb", freed_gb)?;

    if freed_gb + CAPACITY_EPSILON_GB < shortfall_gb {
        let evictable_gb: f64 = candidates.iter().map(|m| m.memory_gb).sum();
        validate_capacity("sum of evictable candidate.memory_gb", evictable_gb)?;
        return Err(PlanEvictionError::InsufficientCapacity {
            model_id: need.id,
            shortfall_gb,
            budget_gb: state.budget_gb,
            evictable_gb,
        });
    }

    Ok(EvictionPlan::Evict(chosen))
}
