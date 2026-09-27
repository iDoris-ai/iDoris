//! A test-only [`RuntimeAdapter`] implementation with an in-memory catalog,
//! load/unload bookkeeping, and **injectable latency/OOM/failure
//! scripting**, so the [`crate::supervisor`] event loop can be tested
//! against realistic adapter misbehavior without a real inference engine.
//! Configure with the `set_*` methods before sharing the adapter
//! (typically via `Arc`) — the scripts and counters are interior-mutable
//! so they keep working once shared.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use idoris_contracts::LoadPolicy;
use tokio_util::sync::CancellationToken;

use crate::RuntimeAdapter;
use crate::error::BackendError;
use crate::types::{BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure};

/// One scripted result for a `load()` call. Consumed in order from the
/// per-model queue set by [`MockAdapter::set_load_script`]; once the queue
/// is empty every further call defaults to `Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    Ok,
    /// Simulates the engine reporting an out-of-memory condition —
    /// [`BackendError::oom`], which the Supervisor's load flow is allowed
    /// to retry exactly once.
    Oom,
    /// A generic, non-retryable failure.
    Fail,
}

#[derive(Default)]
struct ModelScript {
    load_delay: Duration,
    load_outcomes: VecDeque<LoadOutcome>,
    unload_delay: Duration,
}

struct MockState {
    catalog: Vec<ModelInfo>,
    loaded: Vec<String>,
    /// The `LoadPolicy` most recently applied to each *currently loaded*
    /// model — tracked (and always overwritten on `load`, cleared on
    /// `unload`) so this test double actually honors the policy-scoped
    /// idempotency contract `adapter.rs`'s `load()` documents, instead of
    /// silently treating every repeat `load` as a no-op regardless of
    /// whether the requested policy changed.
    policies: HashMap<String, LoadPolicy>,
    scripts: HashMap<String, ModelScript>,
    load_calls: HashMap<String, u32>,
    unload_calls: HashMap<String, u32>,
    /// Ordered `"{op}:{id}:{phase}"` records (e.g. `"load:a:start"`,
    /// `"load:a:end"`) — lets concurrency tests assert two operations'
    /// effects never interleave, without depending on wall-clock timing.
    event_log: Vec<String>,
    pressure: Pressure,
    model_memory_max_gb: f64,
}

/// Configurable in-memory [`RuntimeAdapter`]. `catalog` fixes what
/// [`RuntimeAdapter::list`]/`load` will recognize;
/// [`MockAdapter::set_pressure`] lets a test drive [`RuntimeAdapter::status`]'s
/// `pressure` field without needing a real memory-pressure signal.
pub struct MockAdapter {
    state: Mutex<MockState>,
}

impl MockAdapter {
    pub fn new(catalog: Vec<ModelInfo>) -> Self {
        Self {
            state: Mutex::new(MockState {
                catalog,
                loaded: Vec::new(),
                policies: HashMap::new(),
                scripts: HashMap::new(),
                load_calls: HashMap::new(),
                unload_calls: HashMap::new(),
                event_log: Vec::new(),
                pressure: Pressure::Ok,
                model_memory_max_gb: 24.0,
            }),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MockState>, BackendError> {
        self.state
            .lock()
            .map_err(|_| BackendError::lock_poisoned("MockAdapter's lock was poisoned by a panic"))
    }

    fn script_mut<'a>(state: &'a mut MockState, id: &str) -> &'a mut ModelScript {
        state.scripts.entry(id.to_string()).or_default()
    }

    pub fn set_pressure(&self, pressure: Pressure) -> Result<(), BackendError> {
        self.lock()?.pressure = pressure;
        Ok(())
    }

    /// These `set_*`/`*_count`/`event_log` helpers are test-only
    /// configuration/inspection points, not part of [`RuntimeAdapter`]. A
    /// poisoned lock here only happens after some other assertion already
    /// panicked mid-test, so they degrade to a no-op/default rather than
    /// unwrapping — never introducing a *second*, confusing panic.
    pub fn set_load_delay(&self, id: &str, delay: Duration) {
        if let Ok(mut state) = self.state.lock() {
            Self::script_mut(&mut state, id).load_delay = delay;
        }
    }

    pub fn set_load_script(&self, id: &str, outcomes: Vec<LoadOutcome>) {
        if let Ok(mut state) = self.state.lock() {
            Self::script_mut(&mut state, id).load_outcomes = outcomes.into();
        }
    }

    pub fn set_unload_delay(&self, id: &str, delay: Duration) {
        if let Ok(mut state) = self.state.lock() {
            Self::script_mut(&mut state, id).unload_delay = delay;
        }
    }

    pub fn load_call_count(&self, id: &str) -> u32 {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.load_calls.get(id).copied())
            .unwrap_or(0)
    }

    pub fn unload_call_count(&self, id: &str) -> u32 {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.unload_calls.get(id).copied())
            .unwrap_or(0)
    }

    pub fn event_log(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.event_log.clone())
            .unwrap_or_default()
    }

    /// The policy most recently applied to `id` via `load`, or `None` if it
    /// isn't currently loaded (or was loaded with `policy: None`). Lets a
    /// test assert that a repeat `load` with a different policy actually
    /// took effect, rather than being silently swallowed as a no-op.
    ///
    /// Returns `Result`, not a bare `Option` collapsing a poisoned lock into
    /// "no policy recorded" — this crate's "不静默" invariant applies to
    /// test-inspection helpers too: a poisoned lock is a real failure a
    /// test should see, not be misled by into thinking a policy was never
    /// applied.
    pub fn effective_policy(&self, id: &str) -> Result<Option<LoadPolicy>, BackendError> {
        Ok(self.lock()?.policies.get(id).copied())
    }
}

#[async_trait]
impl RuntimeAdapter for MockAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(self.lock()?.catalog.clone())
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        let (delay, outcome) = {
            let mut state = self.lock()?;
            if !state.catalog.iter().any(|m| m.id == id) {
                return Err(BackendError::model_not_found(id));
            }
            *state.load_calls.entry(id.to_string()).or_insert(0) += 1;
            state.event_log.push(format!("load:{id}:start"));
            let script = Self::script_mut(&mut state, id);
            let delay = script.load_delay;
            let outcome = script.load_outcomes.pop_front().unwrap_or(LoadOutcome::Ok);
            (delay, outcome)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        let mut state = self.lock()?;
        match outcome {
            LoadOutcome::Oom => {
                state.event_log.push(format!("load:{id}:oom"));
                return Err(BackendError::oom(id));
            }
            LoadOutcome::Fail => {
                state.event_log.push(format!("load:{id}:fail"));
                return Err(BackendError::Upstream {
                    message: format!("mock adapter scripted failure loading {id}"),
                });
            }
            LoadOutcome::Ok => {}
        }
        if !state.loaded.iter().any(|loaded| loaded == id) {
            state.loaded.push(id.to_string());
        }
        // Always overwrite, never conditionally skip on "already loaded" —
        // per `adapter.rs`'s contract a repeat `load` with a *different*
        // policy (e.g. upgrading to `resident`/pinned) must take effect,
        // not be treated as a blanket no-op.
        match policy {
            Some(policy) => {
                state.policies.insert(id.to_string(), *policy);
            }
            None => {
                state.policies.remove(id);
            }
        }
        state.event_log.push(format!("load:{id}:end"));
        Ok(())
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        let delay = {
            let mut state = self.lock()?;
            *state.unload_calls.entry(id.to_string()).or_insert(0) += 1;
            state.event_log.push(format!("unload:{id}:start"));
            Self::script_mut(&mut state, id).unload_delay
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        let mut state = self.lock()?;
        state.loaded.retain(|loaded| loaded != id);
        state.policies.remove(id);
        state.event_log.push(format!("unload:{id}:end"));
        Ok(())
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let state = self.lock()?;
        // Computed from the loaded set's own `memory_gb`, not hardcoded —
        // the Supervisor's eviction-confirmation polling and any future
        // memory-accounting test needs this to actually track reality, not
        // silently report zero regardless of what's loaded.
        let used_gb = state
            .loaded
            .iter()
            .filter_map(|id| state.catalog.iter().find(|m| &m.id == id))
            .map(|m| m.memory_gb)
            .sum();
        Ok(BackendStatus {
            pressure: state.pressure,
            used_gb,
            model_memory_max_gb: state.model_memory_max_gb,
            loaded: state.loaded.clone(),
        })
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        let state = self.lock()?;
        if !state.catalog.iter().any(|m| m.id == id) {
            // An unknown id must fail loudly, not `Ok(false)` — the
            // Supervisor treats `Ok(false)` as "keep polling", which for a
            // model that will never exist would eventually surface as a
            // misleading `ProbeTimedOut` instead of the real problem.
            return Err(BackendError::model_not_found(id));
        }
        Ok(state.loaded.iter().any(|loaded| loaded == id))
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        if cancel.is_cancelled() {
            return Err(BackendError::cancelled());
        }
        {
            let state = self.lock()?;
            if !state.catalog.iter().any(|m| m.id == req.model) {
                return Err(BackendError::model_not_found(&req.model));
            }
            // Mirrors what a real engine would do: it only knows about
            // whatever it has actually loaded, independent of the
            // Supervisor's own ledger. A test double that served `chat` for
            // any catalog id regardless of `loaded` would mask a Supervisor
            // bug that forwards requests to a `Loading`/`Stopped` model.
            if !state.loaded.iter().any(|loaded| loaded == &req.model) {
                return Err(BackendError::model_unavailable(&req.model));
            }
        }
        let last_user_message = req
            .messages
            .last()
            .map(|m| m.content.as_str())
            .unwrap_or("");
        Ok(ChatResponse {
            model: req.model,
            content: format!("mock reply to: {last_user_message}"),
        })
    }
}
