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
    /// Number of `chat` calls currently dispatched to this model. Nonzero
    /// means real, in-progress work would be aborted mid-flight — never a
    /// candidate for eviction, same as `pinned`, regardless of how stale
    /// `last_used_seq` is.
    pub inflight: u32,
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
    /// load. Every id here is `Ready` or `Error`, unpinned, and had no
    /// in-flight `chat` calls at snapshot time.
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
pub(crate) fn occupies_budget(state: ModelState) -> bool {
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
        .filter(|m| {
            // `Error` is included (Opus Tier-2 review, H1): a model stuck
            // in `Error` is doing no useful work and, unlike `Ready`, was
            // never a candidate before — meaning it could occupy budget
            // forever with no automatic way to reclaim it, surfacing as a
            // misleading `EvictionImpossible` for some unrelated future
            // load instead of the real problem (a stuck, uncleaned entry).
            (m.state == ModelState::Ready || m.state == ModelState::Error)
                && !m.pinned
                && m.memory_gb > 0.0
                && m.inflight == 0
        })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn entry(id: &str, memory_gb: f64, state: ModelState, pinned: bool, seq: u64) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            memory_gb,
            state,
            pinned,
            last_used_seq: seq,
            inflight: 0,
        }
    }

    fn req(id: &str, memory_gb: f64) -> ModelReq {
        ModelReq {
            id: id.to_string(),
            memory_gb,
        }
    }

    #[test]
    fn sufficient_capacity_needs_no_eviction() {
        let snap = Snapshot {
            budget_gb: 24.0,
            models: vec![entry("a", 8.0, ModelState::Ready, false, 1)],
        };
        assert_eq!(
            plan_eviction(&snap, req("b", 8.0)),
            Ok(EvictionPlan::NotNeeded)
        );
    }

    /// Negative contrast: same layout, but the request no longer fits.
    #[test]
    fn insufficient_capacity_triggers_a_plan() {
        let snap = Snapshot {
            budget_gb: 24.0,
            models: vec![entry("a", 20.0, ModelState::Ready, false, 1)],
        };
        assert_eq!(
            plan_eviction(&snap, req("b", 8.0)),
            Ok(EvictionPlan::Evict(vec!["a".to_string()]))
        );
    }

    #[test]
    fn lru_evicts_the_oldest_used_model_first() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![
                entry("a", 4.0, ModelState::Ready, false, 1),
                entry("b", 4.0, ModelState::Ready, false, 2),
            ],
        };
        // Only one of the two needs to go to fit "need"; LRU picks the
        // lower `last_used_seq`, "a".
        let plan = plan_eviction(&snap, req("need", 5.0)).expect("plan should succeed");
        assert_eq!(plan, EvictionPlan::Evict(vec!["a".to_string()]));
    }

    /// Negative contrast: same ids/vec order, swapped seqs — the choice
    /// must flip too, ruling out sorting by id/vec order instead of seq.
    #[test]
    fn lru_follows_last_used_seq_not_id_or_vec_order() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![
                entry("a", 4.0, ModelState::Ready, false, 2),
                entry("b", 4.0, ModelState::Ready, false, 1),
            ],
        };
        let plan = plan_eviction(&snap, req("need", 5.0)).expect("plan should succeed");
        assert_eq!(plan, EvictionPlan::Evict(vec!["b".to_string()]));
    }

    /// Must pick as many LRU candidates as needed and stop once covered.
    #[test]
    fn lru_evicts_multiple_oldest_models_until_the_shortfall_is_covered() {
        let snap = Snapshot {
            budget_gb: 12.0,
            models: vec![
                entry("oldest", 4.0, ModelState::Ready, false, 1),
                entry("middle", 4.0, ModelState::Ready, false, 2),
                entry("newest", 4.0, ModelState::Ready, false, 3),
            ],
        };
        // occupied=12, available=0, need=7 => shortfall=7: "oldest" alone
        // (4.0) isn't enough, "oldest"+"middle" (8.0) is — "newest" must
        // not be touched.
        let plan = plan_eviction(&snap, req("need", 7.0)).expect("plan should succeed");
        assert_eq!(
            plan,
            EvictionPlan::Evict(vec!["oldest".to_string(), "middle".to_string()])
        );
    }

    #[test]
    fn pinned_models_are_never_evicted_even_when_oldest() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![
                entry("pinned-old", 4.0, ModelState::Ready, true, 1),
                entry("unpinned-newer", 4.0, ModelState::Ready, false, 2),
            ],
        };
        let plan = plan_eviction(&snap, req("need", 5.0)).expect("plan should succeed");
        assert_eq!(
            plan,
            EvictionPlan::Evict(vec!["unpinned-newer".to_string()])
        );
    }

    /// Negative contrast: un-pin it and it becomes the LRU choice again.
    #[test]
    fn unpinning_the_oldest_model_makes_it_evictable_again() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![
                entry("formerly-pinned-old", 4.0, ModelState::Ready, false, 1),
                entry("unpinned-newer", 4.0, ModelState::Ready, false, 2),
            ],
        };
        let plan = plan_eviction(&snap, req("need", 5.0)).expect("plan should succeed");
        assert_eq!(
            plan,
            EvictionPlan::Evict(vec!["formerly-pinned-old".to_string()])
        );
    }

    #[test]
    fn no_evictable_candidates_is_an_explicit_error() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("pinned", 8.0, ModelState::Ready, true, 1)],
        };
        let err = plan_eviction(&snap, req("need", 5.0)).expect_err("must fail, not guess");
        assert_eq!(err.reason_code(), "eviction_impossible");
        assert_eq!(
            err,
            PlanEvictionError::InsufficientCapacity {
                model_id: "need".to_string(),
                shortfall_gb: 3.0,
                budget_gb: 10.0,
                evictable_gb: 0.0,
            }
        );
    }

    /// Negative contrast: same pinned model, but enough spare budget that
    /// no eviction is needed at all.
    #[test]
    fn sufficient_budget_needs_no_eviction_even_with_a_pinned_model_present() {
        let snap = Snapshot {
            budget_gb: 20.0,
            models: vec![entry("pinned", 8.0, ModelState::Ready, true, 1)],
        };
        assert_eq!(
            plan_eviction(&snap, req("need", 5.0)),
            Ok(EvictionPlan::NotNeeded)
        );
    }

    #[test]
    fn launching_models_do_not_count_against_the_budget() {
        // A model queued behind another op ("Launching") hasn't started
        // consuming memory yet, so it must not block admission.
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("queued", 9.0, ModelState::Launching, false, 1)],
        };
        assert_eq!(
            plan_eviction(&snap, req("need", 5.0)),
            Ok(EvictionPlan::NotNeeded)
        );
    }

    #[test]
    fn loading_models_do_count_against_the_budget() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("in-flight", 9.0, ModelState::Loading, false, 1)],
        };
        let err = plan_eviction(&snap, req("need", 5.0)).expect_err("must fail, not guess");
        assert_eq!(err.reason_code(), "eviction_impossible");
    }

    /// `Stopping` (not yet confirmed released) must still count.
    #[test]
    fn stopping_models_still_count_against_the_budget() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("mid-unload", 9.0, ModelState::Stopping, false, 1)],
        };
        let err = plan_eviction(&snap, req("need", 5.0)).expect_err("must fail, not guess");
        assert_eq!(err.reason_code(), "eviction_impossible");
    }

    /// Negative contrast: once `Stopped`, it no longer counts.
    #[test]
    fn stopped_models_do_not_count_against_the_budget() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("released", 9.0, ModelState::Stopped, false, 1)],
        };
        assert_eq!(
            plan_eviction(&snap, req("need", 5.0)),
            Ok(EvictionPlan::NotNeeded)
        );
    }

    /// `Error` state is uncertain — conservatively, it must still count.
    #[test]
    fn error_state_models_still_count_against_the_budget() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("uncertain", 9.0, ModelState::Error, false, 1)],
        };
        // If `Error` didn't occupy budget, all 10 GiB would be free and
        // this would need no eviction at all (`NotNeeded`). Because `Error`
        // still counts (`occupies_budget`'s documented conservatism), only
        // 1 GiB is free, forcing eviction — and per H1 (Opus Tier-2
        // review), an `Error` entry is itself now a valid candidate rather
        // than occupying budget forever with no way to reclaim it.
        let plan = plan_eviction(&snap, req("need", 5.0)).expect("uncertain is now evictable");
        assert_eq!(plan, EvictionPlan::Evict(vec!["uncertain".to_string()]));
    }

    #[test]
    fn zero_capacity_candidates_are_never_chosen() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![
                entry("zero", 0.0, ModelState::Ready, false, 1),
                entry("nonzero", 6.0, ModelState::Ready, false, 2),
            ],
        };
        // "zero" is older (lower seq), so a buggy implementation that
        // doesn't exclude it would pick it *first*, producing
        // `Evict(["zero", "nonzero"])` (freeing 0 from "zero" never
        // satisfies the shortfall on its own, so it would fall through to
        // "nonzero" too). Excluding "zero" up front means only "nonzero" is
        // ever a candidate.
        let plan = plan_eviction(&snap, req("need", 5.0)).expect("plan should succeed");
        assert_eq!(plan, EvictionPlan::Evict(vec!["nonzero".to_string()]));
    }

    /// Negative contrast: give it real capacity — now a valid candidate.
    #[test]
    fn a_formerly_zero_capacity_model_becomes_evictable_once_it_has_real_capacity() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![
                entry("was-zero", 6.0, ModelState::Ready, false, 1),
                entry("nonzero", 6.0, ModelState::Ready, false, 2),
            ],
        };
        let plan = plan_eviction(&snap, req("need", 3.0)).expect("plan should succeed");
        assert_eq!(plan, EvictionPlan::Evict(vec!["was-zero".to_string()]));
    }

    #[test]
    fn nan_budget_is_rejected_not_silently_miscomputed() {
        let snap = Snapshot {
            budget_gb: f64::NAN,
            models: vec![],
        };
        let err = plan_eviction(&snap, req("need", 5.0)).expect_err("NaN must be rejected");
        assert_eq!(err.reason_code(), "invalid_capacity_value");
    }

    #[test]
    fn negative_model_memory_is_rejected() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("bogus", -1.0, ModelState::Ready, false, 1)],
        };
        let err = plan_eviction(&snap, req("need", 5.0)).expect_err("negative must be rejected");
        assert_eq!(err.reason_code(), "invalid_capacity_value");
    }

    // (Negative contrast — valid values succeeding — is already covered by
    // every other test in this module, e.g. `sufficient_capacity_needs_no_eviction`.)

    /// Negative contrast: every other test above omits `need.id`.
    #[test]
    fn snapshot_containing_the_requested_model_is_a_hard_error() {
        let snap = Snapshot {
            budget_gb: 10.0,
            models: vec![entry("need", 5.0, ModelState::Ready, false, 1)],
        };
        let err =
            plan_eviction(&snap, req("need", 5.0)).expect_err("must fail, not silently filter");
        assert_eq!(err.reason_code(), "snapshot_contains_requested_model");
    }
}
