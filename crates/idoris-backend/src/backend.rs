//! The engine-agnostic backend trait itself — 1:1 with the `ModelBackend`
//! interface in `packages/adapters/src/backend.ts`. `LoadPolicy`'s abstract
//! semantics (resident/on_demand/evict_to_load, pinned/idle_ttl, admission)
//! are the implementation's job to map onto a concrete engine; the router
//! only ever sees this trait — **no engine name appears here**.

use async_trait::async_trait;
use idoris_contracts::LoadPolicy;
use tokio_util::sync::CancellationToken;

use crate::error::BackendError;
use crate::types::{Admission, BackendStatus, ChatRequest, ChatResponse, ModelInfo};

#[async_trait]
pub trait ModelBackend: Send + Sync {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError>;

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError>;

    async fn unload(&self, id: &str) -> Result<(), BackendError>;

    async fn admission(&self, id: &str) -> Result<Admission, BackendError>;

    async fn status(&self) -> Result<BackendStatus, BackendError>;

    /// `cancel` fires when the caller wants to abort — the request has
    /// already ended when this returns `Err(BackendError::Cancelled { .. })`.
    /// A spawn-type backend implementation should additionally treat
    /// `cancel` firing as a signal to kill its whole process group, not just
    /// stop reading its output (mirrors the TS `signal` doc comment).
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError>;
}
