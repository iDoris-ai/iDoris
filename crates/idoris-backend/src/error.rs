//! Backend/Supervisor errors, each carrying a `reason_code` — the router's
//! unified error envelope (interface spec §3.11: `{error: {type, rule_id,
//! reason_code, evidence, remediation}}`) needs a stable machine-readable
//! code independent of the (human, free-text) `Display` message.
//!
//! Repo-wide invariant "不静默" (never fail silently): every state
//! inconsistency the Supervisor can observe must surface as one of these
//! variants rather than being swallowed or guessed at.
//!
//! **Design note** (raised in Codex review, accepted as a deliberate,
//! documented trade-off rather than fixed here): this single enum carries
//! both "errors a backend/adapter implementation returns"
//! (`ModelNotFound`/`Upstream`/`Oom`/`Cancelled`/...) and
//! "errors only the Supervisor's own event loop produces"
//! (`SupervisorUnavailable`/`InvariantViolation`). A stricter design would
//! split these into two types (`AdapterError` + a `SupervisorError` that
//! wraps it) so the type system — not just doc comments — stops an adapter
//! implementation from fabricating `SupervisorUnavailable`. We are not
//! doing that split yet: today there is exactly one implementation of
//! [`crate::adapter::RuntimeAdapter`] in this crate ([`crate::mock::MockAdapter`],
//! a test double) and no external consumer of `BackendError`, so the
//! split's benefit is
//! currently theoretical while its cost (touching every signature in the
//! adapter trait, its implementations, and the Supervisor) is not. Revisit
//! this once a second real adapter (e.g. oMLX) or an external consumer
//! shows up — until then, the contract is enforced by convention: **a
//! backend/adapter implementation must never construct
//! `SupervisorUnavailable` or `InvariantViolation`.**

use thiserror::Error;

// `Clone` matters here beyond convenience: singleflight-merged waiters on
// the same in-flight load/unload all need the *same* outcome delivered to
// each of their independent oneshot channels. `PartialEq` lets tests
// `assert_eq!` a `Result<_, BackendError>` directly instead of matching on
// `reason_code()` alone.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum BackendError {
    #[error("model not found: {model_id}")]
    ModelNotFound { model_id: String },

    /// The model exists in the Supervisor's ledger but is still
    /// `Launching`/`Loading` — a transient state. Distinct from
    /// [`BackendError::ModelUnavailable`] (a terminal/conflicting state) so
    /// a caller can tell "wait and retry" apart from "give up, this needs a
    /// fresh `load`".
    #[error("model still loading: {model_id}")]
    ModelLoading { model_id: String },

    /// The model exists in the Supervisor's ledger but is `Error`,
    /// `Stopping`, or `Stopped` — a terminal or conflicting state a caller
    /// must not simply wait out. See [`BackendError::ModelLoading`] for the
    /// transient counterpart.
    #[error("model unavailable: {model_id}")]
    ModelUnavailable { model_id: String },

    /// [`crate::eviction::plan_eviction`] determined that even evicting
    /// every evictable (non-pinned, `Ready`) model still would not free
    /// enough budget for `model_id` — a *planning*-time rejection: no
    /// viable plan exists at all. Construct exclusively via
    /// [`BackendError::eviction_impossible`]. Distinct from
    /// [`BackendError::EvictionFailed`], the *execution*-time counterpart
    /// (a plan existed, but carrying it out failed).
    #[error("cannot admit {model_id}: no viable eviction plan")]
    EvictionImpossible { model_id: String },

    /// A viable plan existed and was chosen, but a chosen victim's
    /// `unload` itself failed while making room for `model_id`. Kept
    /// distinct from [`BackendError::EvictionImpossible`] on purpose: this
    /// is an adapter/IO-layer failure a caller should treat as transient
    /// and worth retrying, not "give up, there is no capacity" (retrying
    /// an `EvictionImpossible` is pointless until the situation changes;
    /// retrying this might just work).
    #[error("eviction failed while admitting {model_id}: a chosen victim's unload failed")]
    EvictionFailed { model_id: String },

    /// The adapter reported an out-of-memory condition while loading
    /// `model_id`. The Supervisor self-heals with exactly one retry
    /// (circuit breaker) — see `supervisor::load_flow`.
    #[error("out of memory while loading {model_id}")]
    Oom { model_id: String },

    /// A `RuntimeAdapter::probe_ready` polling loop exhausted its attempt
    /// budget without observing the model become ready.
    #[error("timed out waiting for {model_id} to become ready")]
    ProbeTimedOut { model_id: String },

    /// A single `RuntimeAdapter` call (`load`/`unload`/`probe_ready`) did
    /// not finish within `SupervisorConfig::adapter_call_timeout`. Distinct
    /// from [`BackendError::ProbeTimedOut`]: that means "polled N times,
    /// never observed `Ready`"; this means "one specific call itself hung."
    /// Existing to close llama-swap Issue #946's failure mode: without a
    /// bound here, a hanging adapter call would hold the Supervisor's
    /// global load/evict mutex forever.
    #[error("adapter call for {model_id} timed out")]
    AdapterTimedOut { model_id: String },

    /// A `RuntimeAdapter` call's background task panicked instead of
    /// returning normally. Kept distinct from `AdapterTimedOut`: a panic is
    /// an adapter implementation bug (actionable, should be reported/fixed)
    /// where a timeout is more often transient latency (retry-worthy).
    #[error("adapter call for {model_id} panicked: {message}")]
    AdapterPanicked { model_id: String, message: String },

    #[error("upstream error: {message}")]
    Upstream { message: String },

    /// The caller's `CancellationToken` fired (or was already cancelled)
    /// before/while the request was in flight.
    #[error("request cancelled")]
    Cancelled,

    /// The Supervisor's event loop task is gone (panicked, or the handle
    /// outlived it) — the command channel is closed. Distinct from
    /// `Internal` because it means *no* command can be serviced, not that
    /// one particular command failed. **Supervisor-only** — see the module
    /// doc comment's design note; a `RuntimeAdapter` implementation must
    /// never construct this.
    #[error("supervisor is not running")]
    SupervisorUnavailable,

    /// The Supervisor's bounded concurrency limit for in-flight adapter
    /// calls (`SupervisorConfig::max_concurrent_adapter_calls`) is
    /// currently exhausted. Distinct from `SupervisorUnavailable`: the
    /// Supervisor *is* running, it's just momentarily saturated — retrying
    /// shortly is the right response, not treating it as down.
    /// **Supervisor-only** — see the module doc comment's design note; a
    /// backend/adapter implementation must never construct this.
    #[error("supervisor is at its concurrent-call limit")]
    Busy,

    /// A state inconsistency the Supervisor's single-writer loop detected
    /// in itself — e.g. a completion message referencing a model id the
    /// ledger no longer has an entry for. Per the "不静默" invariant this
    /// must always be raised, never guessed past. **Supervisor-only** — see
    /// the module doc comment's design note; a backend/adapter
    /// implementation must never construct this.
    #[error("internal invariant violated: {message}")]
    InvariantViolation { message: String },

    /// A backend/adapter implementation's own internal synchronization
    /// state (e.g. a `Mutex`) was poisoned by an earlier panic while held.
    /// Kept as its own variant — rather than folded into the generic
    /// `Internal` below — specifically so `reason_code()` stays
    /// `"internal_lock_poisoned"` and doesn't collapse into the same code
    /// as every other unexpected internal failure.
    #[error("internal lock poisoned: {message}")]
    LockPoisoned { message: String },

    #[error("internal backend error: {message}")]
    Internal { message: String },
}

impl BackendError {
    /// Stable machine-readable code for the router's error envelope. Kept
    /// distinct from `Display`'s message, which is free text for logs/humans.
    ///
    /// Deliberately derived from the variant itself (a fixed `&'static
    /// str`), not stored as a field on each variant: an earlier draft stored
    /// `reason_code` as a free-form `String` field, which let (buggy) code
    /// construct e.g. `Upstream { reason_code: "oom" }` — a value whose
    /// `reason_code()` and whose `is_oom()` would then disagree. Deriving it
    /// here makes that class of bug unrepresentable.
    pub fn reason_code(&self) -> &'static str {
        match self {
            BackendError::ModelNotFound { .. } => "model_not_found",
            BackendError::ModelLoading { .. } => "model_loading",
            BackendError::ModelUnavailable { .. } => "model_unavailable",
            BackendError::EvictionImpossible { .. } => "eviction_impossible",
            BackendError::EvictionFailed { .. } => "eviction_failed",
            BackendError::Oom { .. } => "oom",
            BackendError::ProbeTimedOut { .. } => "probe_timed_out",
            BackendError::AdapterTimedOut { .. } => "adapter_timed_out",
            BackendError::AdapterPanicked { .. } => "adapter_panicked",
            BackendError::Upstream { .. } => "upstream_error",
            BackendError::Cancelled => "cancelled",
            BackendError::SupervisorUnavailable => "supervisor_unavailable",
            BackendError::Busy => "supervisor_busy",
            BackendError::InvariantViolation { .. } => "state_invariant_violated",
            BackendError::LockPoisoned { .. } => "internal_lock_poisoned",
            BackendError::Internal { .. } => "internal",
        }
    }

    /// `true` iff this is an out-of-memory error. This is a plain type
    /// check, **not** a "may I retry" answer — it does not know how many
    /// times a caller has already retried. The Supervisor's load flow is
    /// the sole place that turns this into "retry exactly once": it tracks
    /// the attempt count itself and must never call `adapter.load` a third
    /// time for the same request just because `is_oom()` is still `true`.
    pub fn is_oom(&self) -> bool {
        matches!(self, BackendError::Oom { .. })
    }

    pub fn model_not_found(model_id: impl Into<String>) -> Self {
        Self::ModelNotFound {
            model_id: model_id.into(),
        }
    }

    pub fn model_loading(model_id: impl Into<String>) -> Self {
        Self::ModelLoading {
            model_id: model_id.into(),
        }
    }

    pub fn model_unavailable(model_id: impl Into<String>) -> Self {
        Self::ModelUnavailable {
            model_id: model_id.into(),
        }
    }

    /// See [`BackendError::EvictionImpossible`]'s doc comment: this is the
    /// one and only intended call site pattern (mapping
    /// `plan_eviction`'s "no viable plan" result), not a general "admission
    /// denied" helper.
    pub fn eviction_impossible(model_id: impl Into<String>) -> Self {
        Self::EvictionImpossible {
            model_id: model_id.into(),
        }
    }

    pub fn eviction_failed(model_id: impl Into<String>) -> Self {
        Self::EvictionFailed {
            model_id: model_id.into(),
        }
    }

    pub fn oom(model_id: impl Into<String>) -> Self {
        Self::Oom {
            model_id: model_id.into(),
        }
    }

    pub fn probe_timed_out(model_id: impl Into<String>) -> Self {
        Self::ProbeTimedOut {
            model_id: model_id.into(),
        }
    }

    pub fn adapter_timed_out(model_id: impl Into<String>) -> Self {
        Self::AdapterTimedOut {
            model_id: model_id.into(),
        }
    }

    pub fn adapter_panicked(model_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self::AdapterPanicked {
            model_id: model_id.into(),
            message: message.into(),
        }
    }

    pub fn cancelled() -> Self {
        Self::Cancelled
    }

    pub fn supervisor_unavailable() -> Self {
        Self::SupervisorUnavailable
    }

    pub fn busy() -> Self {
        Self::Busy
    }

    pub fn invariant_violation(message: impl Into<String>) -> Self {
        Self::InvariantViolation {
            message: message.into(),
        }
    }

    pub fn lock_poisoned(message: impl Into<String>) -> Self {
        Self::LockPoisoned {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}
