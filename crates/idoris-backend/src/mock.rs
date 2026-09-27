//! A test-only [`RuntimeAdapter`] implementation with an in-memory catalog
//! and load/unload bookkeeping, so router/tenancy code — and, in a
//! follow-up PR, the Supervisor event loop — can be tested against the
//! trait without a real inference engine. A later PR adds injectable
//! latency/failure/OOM scripting on top of this; this version only carries
//! over the R1 skeleton's behavior onto the renamed [`RuntimeAdapter`]
//! trait.

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
}

#[async_trait]
impl RuntimeAdapter for MockAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(self.lock()?.catalog.clone())
    }

    async fn load(&self, id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        let mut state = self.lock()?;
        if !state.catalog.iter().any(|m| m.id == id) {
            return Err(BackendError::model_not_found(id));
        }
        if !state.loaded.iter().any(|loaded| loaded == id) {
            state.loaded.push(id.to_string());
        }
        Ok(())
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.lock()?.loaded.retain(|loaded| loaded != id);
        Ok(())
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let state = self.lock()?;
        Ok(BackendStatus {
            pressure: state.pressure,
            used_gb: 0.0,
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
