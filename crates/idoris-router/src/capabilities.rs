//! `GET /capabilities` transport boundary (B1 task32).
//!
//! This task intentionally stops at the injectable provider surface. The live
//! recommender/backend-status aggregation belongs to task33, so production must
//! never fabricate a static capacity snapshot when no provider is configured.

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Ready,
    RequiresEviction,
    Blocked,
}

/// Current TS `/capabilities` entry shape. The newer Agent24 role-oriented
/// contract is a separate interface evolution; task32 mirrors the reference
/// implementation that B1 is porting.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapabilityEntry {
    pub id: String,
    pub capability: String,
    pub resident: bool,
    pub estimated_memory_gb: f64,
    pub ctx_limit: u64,
    pub queue_depth: u64,
    pub admission_status: AdmissionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilitiesError {
    message: String,
}

impl CapabilitiesError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CapabilitiesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CapabilitiesError {}

pub trait CapabilitiesProvider: Send + Sync {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CapabilityEntry>, CapabilitiesError>> + Send + '_>>;
}
