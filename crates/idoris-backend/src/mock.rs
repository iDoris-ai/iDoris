//! A test-only [`RuntimeAdapter`] implementation with an in-memory catalog
//! and load/unload bookkeeping, so router/tenancy code — and, in a
//! follow-up PR, the Supervisor event loop — can be tested against the
//! trait without a real inference engine. A later PR adds injectable
//! latency/failure/OOM scripting on top of this; this version only carries
//! over the R1 skeleton's behavior onto the renamed [`RuntimeAdapter`]
//! trait.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use idoris_contracts::LoadPolicy;
use tokio_util::sync::CancellationToken;

use crate::RuntimeAdapter;
use crate::error::BackendError;
use crate::types::{BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure};

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

    pub fn set_pressure(&self, pressure: Pressure) -> Result<(), BackendError> {
        self.lock()?.pressure = pressure;
        Ok(())
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
        let mut state = self.lock()?;
        if !state.catalog.iter().any(|m| m.id == id) {
            return Err(BackendError::model_not_found(id));
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
        Ok(())
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        state.loaded.retain(|loaded| loaded != id);
        state.policies.remove(id);
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
