//! [`RuntimeAdapter`] — the engine-agnostic boundary a single concrete
//! inference runtime implements. Formerly `ModelBackend` (see the R1
//! skeleton and `packages/adapters/src/backend.ts`); renamed and narrowed
//! for R2-A per `docs/research/Rust基础选型-2026-09-27.md` §4
//! ("原来的 `ModelBackend` trait 下沉为『运行时适配器』，上面加一层 Runtime
//! Supervisor"). `admission` is deliberately **not** part of this trait any
//! more — that decision now lives one layer up, in the pure
//! [`crate::eviction::plan_eviction`] function, so every load path
//! (manual, auto, API, CLI) funnels through the same decision point instead
//! of letting each adapter reimplement its own admission heuristic (LM
//! Studio #2051).
//!
//! The oMLX implementation lives in `idoris-upstream`; this crate supplies
//! [`crate::mock::MockAdapter`] for tests. oMLX 0.6.4 details (see PR #44/#45,
//! `packages/adapters/omlx/omlx-backend.ts`, `spike/u0/U0-LOG.md`):
//! - `GET /api/status`'s loaded-model list field is `loaded_models`, not
//!   `loaded` (0.4.3's assumed name) — a real `status()` impl must read the
//!   new field and only fall back to `loaded` for older engines.
//! - Pinning (`resident` keepalive) needs `PUT
//!   /admin/api/models/{id}/settings`, and as of 0.6.4 that endpoint demands
//!   a *separate admin session* — the plain inference API key gets a bare
//!   401. `idoris-upstream` logs in with the main API key, caches the admin
//!   cookie and reauthenticates once on 401, then verifies the pin via
//!   `/v1/models/status`. Sub keys cannot establish an admin session.
//! - Pressure prefers `/admin/api/activity`'s `active_models.memory_pressure`;
//!   disabled enforcement means `Ok`, unknown tiers mean `Unknown`. If admin
//!   access fails, the legacy `/api/status` pressure behavior is preserved.
//! - `mlx_lm.server` (and therefore oMLX) exposes **no `/health` endpoint**.
//!   [`RuntimeAdapter::probe_ready`] exists specifically so the Supervisor
//!   never has to know that — a real adapter's `probe_ready` must poll an
//!   endpoint that reports **per-model loaded state** (`GET
//!   /v1/models/status`, readable with just the inference key — see
//!   `spike/u0/U0-LOG.md`), not the bare `GET /v1/models` catalog: that
//!   endpoint lists every *routable* model whether or not it is loaded (see
//!   [`RuntimeAdapter::list`]'s doc comment), so treating "id present in
//!   `/v1/models`" as readiness would report every known model `Ready`
//!   immediately, before the engine has actually finished loading it.

use async_trait::async_trait;
use idoris_contracts::LoadPolicy;
use tokio_util::sync::CancellationToken;

use crate::error::BackendError;
use crate::types::{BackendStatus, ChatRequest, ChatResponse, ModelInfo};

#[async_trait]
pub trait RuntimeAdapter: Send + Sync {
    /// Every model this engine instance *could* route to, whether or not it
    /// is currently loaded (mirrors `/v1/models`'s "list routable models"
    /// semantics per `docs/research/Rust基础选型-2026-09-27.md` §4).
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError>;

    /// Start loading `id`. Must return once the *load request* has been
    /// accepted/rejected by the engine — it does **not** have to wait for
    /// the model to become servable; that is what [`Self::probe_ready`] is
    /// for. A `policy` of `None` means "use the engine's default"; the
    /// Supervisor always passes `Some` in practice.
    ///
    /// **Error contract.** A plain `Err` means "nothing was allocated" (or
    /// it was already released): the Supervisor settles the slot on
    /// `Stopped` and frees its budget. An implementation whose load can fail
    /// *after* the engine may already hold the model (e.g. a two-step
    /// load-then-pin/verify) must return [`BackendError::LoadUnconfirmed`]
    /// for that case instead, so the Supervisor confirms release with a
    /// real `unload` rather than forgetting memory still in use.
    ///
    /// **Concurrency contract** (owned by the Supervisor, not this trait):
    /// the Supervisor never issues two `load`/`unload` calls for the *same*
    /// `id` concurrently, and never overlaps a `load`/`unload` for `id`
    /// with another `load`/`unload` for `id` — the global load/evict mutex
    /// serializes all of that (see the crate-level Supervisor docs). An
    /// implementation may therefore assume calls for a given `id` arrive
    /// strictly one at a time and never needs to defend against concurrent
    /// duplicate calls for the same `id`.
    ///
    /// **Idempotency is policy-scoped, not blanket, and "equivalent" means
    /// exactly equal.** Two concurrent requests to load the *same
    /// not-yet-loaded* `id` are singleflight-merged into one `load` call —
    /// and both callers get that call's outcome — **only if their
    /// [`LoadPolicy`]s are `==`**. Merging on some coarser notion of
    /// "close enough" (e.g. "same pinned-ness, ignore `idle_ttl_s`/
    /// `admission`") is explicitly wrong: this engine-agnostic layer has no
    /// way to know which policy fields a given engine actually treats as
    /// consequential, so any two policies that differ *at all* must be
    /// treated as genuinely different. Concurrent requests for the same
    /// `id` whose policies are not `==` are **not** merged — but there is
    /// no wait queue yet (a deliberately deferred simplification, see the
    /// crate-level Supervisor docs), so the second caller does not get
    /// serialized behind the first: it fails fast with
    /// [`BackendError::Busy`], the same as a request for a wholly
    /// *different* id would while the mutex is held. The caller is
    /// responsible for retrying with their own exact policy once the
    /// first `load` completes; a policy is never silently dropped or
    /// merged into someone else's, but it is also never queued for the
    /// Supervisor to apply automatically.
    ///
    /// The same exact-equality rule defines the no-op case: a repeat
    /// `load(id, policy)` call for an `id` that is already loaded is a
    /// no-op **only when `policy` is `==`** to what is already in effect.
    /// It is **not** a no-op when the policy differs at all — e.g. an `id`
    /// currently loaded `on_demand`/unpinned, re-requested as
    /// `resident`/pinned, must
    /// actually update the engine's pin state (mirroring
    /// `packages/adapters/omlx/omlx-backend.ts`'s own `load()`, which
    /// always hits the load endpoint and then conditionally (un)pins).
    /// Silently treating every repeat call as a no-op would leave a model
    /// the caller explicitly asked to pin still evictable.
    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError>;

    /// See [`Self::load`]'s concurrency contract — the same non-overlap
    /// guarantee applies here.
    async fn unload(&self, id: &str) -> Result<(), BackendError>;

    /// Current engine-observed status: which models are loaded and the
    /// engine's own memory/pressure signal. The Supervisor also polls this
    /// after an eviction's `unload` to confirm memory was actually
    /// released (see the crate-level Supervisor docs) before trusting its
    /// own estimate.
    async fn status(&self) -> Result<BackendStatus, BackendError>;

    /// Health probe used while a model is transitioning
    /// `Loading -> Ready`. Polled on an interval by the Supervisor;
    /// `Ok(false)` means "not ready yet, keep polling", `Ok(true)` means
    /// ready.
    ///
    /// `Err` means **terminal, not retryable** — the Supervisor aborts the
    /// load immediately rather than treating it as one more "not ready
    /// yet". Because of that, an implementation — not the (engine-agnostic)
    /// Supervisor — owns the judgment call of what counts as terminal: a
    /// transient network blip (timeout, connection reset, a 5xx from the
    /// engine's own HTTP surface) must be absorbed internally and reported
    /// as `Ok(false)`, reserving `Err` for conditions polling again cannot
    /// fix (the id doesn't exist, the engine reports the load itself
    /// failed, auth rejected, ...). Getting this wrong in the transient
    /// direction — surfacing a network blip as `Err` — fails a load that
    /// was actually still in progress.
    ///
    /// This call is not guaranteed a bounded amount of time by the
    /// Supervisor's own retry budget alone: `probe_max_attempts` bounds how
    /// many *calls* are made, not how long any single call may take. An
    /// implementation must therefore apply its own timeout to whatever I/O
    /// it does (e.g. a bounded HTTP client timeout) — a call that never
    /// returns stalls the load, and eventually the whole Supervisor
    /// instance, since load/eviction is single-flighted through one active
    /// operation at a time (see the crate-level Supervisor docs).
    ///
    /// A real adapter's implementation is engine-specific — see the module
    /// doc comment for why oMLX in particular cannot use a dedicated
    /// health endpoint.
    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError>;

    /// `cancel` fires when the caller wants to abort — the request has
    /// already ended when this returns `Err(BackendError::Cancelled)`.
    /// A spawn-type adapter implementation should additionally treat
    /// `cancel` firing as a signal to kill its whole process group, not just
    /// stop reading its output (mirrors the TS `signal` doc comment).
    ///
    /// The Supervisor only routes `chat` to a model it believes is `Ready`,
    /// but an implementation must not simply trust that: it should still
    /// report an unknown/not-actually-loaded model as an error rather than
    /// serving the request anyway, exactly as a real engine would reject a
    /// request for a model it hasn't loaded. A test double that served
    /// `chat` unconditionally would mask a Supervisor bug that forwards
    /// requests to a `Loading`/`Stopped` model.
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError>;
}
