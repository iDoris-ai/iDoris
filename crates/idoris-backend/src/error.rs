//! Backend errors, each carrying a `reason_code` — the router's unified
//! error envelope (interface spec §3.11: `{error: {type, rule_id,
//! reason_code, evidence, remediation}}`) needs a stable machine-readable
//! code independent of the (human, free-text) `Display` message.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("model not found: {model_id}")]
    ModelNotFound {
        model_id: String,
        reason_code: String,
    },

    #[error("admission denied for {model_id}")]
    AdmissionDenied {
        model_id: String,
        reason_code: String,
    },

    #[error("upstream error: {message}")]
    Upstream {
        message: String,
        reason_code: String,
    },

    /// The caller's `CancellationToken` fired (or was already cancelled)
    /// before/while the request was in flight.
    #[error("request cancelled")]
    Cancelled { reason_code: String },

    #[error("internal backend error: {message}")]
    Internal {
        message: String,
        reason_code: String,
    },
}

impl BackendError {
    /// Stable machine-readable code for the router's error envelope. Kept
    /// distinct from `Display`'s message, which is free text for logs/humans.
    pub fn reason_code(&self) -> &str {
        match self {
            BackendError::ModelNotFound { reason_code, .. }
            | BackendError::AdmissionDenied { reason_code, .. }
            | BackendError::Upstream { reason_code, .. }
            | BackendError::Cancelled { reason_code, .. }
            | BackendError::Internal { reason_code, .. } => reason_code,
        }
    }

    pub fn model_not_found(model_id: impl Into<String>) -> Self {
        Self::ModelNotFound {
            model_id: model_id.into(),
            reason_code: "model_not_found".to_string(),
        }
    }

    pub fn cancelled() -> Self {
        Self::Cancelled {
            reason_code: "cancelled".to_string(),
        }
    }
}
