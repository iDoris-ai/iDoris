//! Wire/value types 1:1 with `packages/adapters/src/backend.ts`.

use serde::{Deserialize, Serialize};

/// A model entry a backend can serve (capacity is part of the interface,
/// 06 §10.8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub memory_gb: f64,
}

/// Pressure tier, shaped like oMLX's `ok/soft/hard/ceiling`
/// (06 §10.3) plus `unknown` (added post-FU-16/FU-18 retest against oMLX
/// 0.6.4 — see PR #44).
///
/// `Unknown` means the backend didn't report a pressure state (field
/// missing, or the backend doesn't support it) — **not** the same as `Ok`.
/// A backend that doesn't know its own pressure can't be treated as
/// "pressure is fine". Any consumer (admission/eviction decisions) must
/// treat `Unknown` conservatively, i.e. as at least `Soft`. [`Pressure::at_least_soft`]
/// makes that rule enforceable in code instead of only living in a doc
/// comment (which is as far as the TS side has gotten as of PR #44/FU-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    Ok,
    Soft,
    Hard,
    Ceiling,
    Unknown,
}

impl Pressure {
    /// `true` for every tier a conservative consumer must treat as "at least
    /// some pressure", i.e. everything except `Ok`. Named for the FU-18 rule
    /// ("`unknown` must be treated as at least `soft`") but also covers the
    /// already-unambiguous `Soft`/`Hard`/`Ceiling` tiers.
    pub fn at_least_soft(self) -> bool {
        !matches!(self, Pressure::Ok)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub pressure: Pressure,
    pub used_gb: f64,
    pub model_memory_max_gb: f64,
    pub loaded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// No `signal`/`AbortSignal` field here, unlike the TS `ChatRequest` —
/// cancellation is a first-class parameter on [`crate::RuntimeAdapter::chat`]
/// via `tokio_util::sync::CancellationToken` instead of living on the
/// request value. A `CancellationToken` isn't `Serialize`/meaningful as
/// wire data, and Rust's ownership makes "the caller dropped its handle" a
/// viable cancellation signal too — spawn-type backends can additionally key
/// off the token being cancelled to kill a whole process group, mirroring
/// the TS comment on `signal`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub model: String,
    pub content: String,
}
