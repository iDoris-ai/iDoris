//! A test-only [`ModelBackend`] implementation with an in-memory catalog and
//! load/unload bookkeeping, so router/tenancy code can be tested against the
//! trait without a real inference engine — the Rust equivalent of hand
//! rolling a fake in a TS test file, but shared as one crate-level type.

use std::sync::Mutex;

use async_trait::async_trait;
use idoris_contracts::LoadPolicy;
use tokio_util::sync::CancellationToken;

use crate::ModelBackend;
use crate::error::BackendError;
use crate::types::{Admission, BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure};

struct MockState {
    catalog: Vec<ModelInfo>,
    loaded: Vec<String>,
    pressure: Pressure,
    model_memory_max_gb: f64,
}

/// Configurable in-memory [`ModelBackend`]. `catalog` fixes what
/// [`ModelBackend::list`]/`load`/`admission` will recognize;
/// [`MockBackend::set_pressure`] lets a test drive [`ModelBackend::status`]'s
/// `pressure` field without needing a real memory-pressure signal.
pub struct MockBackend {
    state: Mutex<MockState>,
}

impl MockBackend {
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
        self.state.lock().map_err(|_| BackendError::Internal {
            message: "MockBackend's internal lock was poisoned by a panicking test".to_string(),
            reason_code: "internal_lock_poisoned".to_string(),
        })
    }

    pub fn set_pressure(&self, pressure: Pressure) -> Result<(), BackendError> {
        self.lock()?.pressure = pressure;
        Ok(())
    }
}

#[async_trait]
impl ModelBackend for MockBackend {
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

    async fn admission(&self, id: &str) -> Result<Admission, BackendError> {
        let state = self.lock()?;
        if !state.catalog.iter().any(|m| m.id == id) {
            return Err(BackendError::model_not_found(id));
        }
        Ok(Admission::Coexist)
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
