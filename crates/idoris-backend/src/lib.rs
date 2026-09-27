//! Model-management layer split into a **runtime adapter** (one concrete
//! inference engine) and a **Supervisor** (single-writer event loop that
//! owns lifecycle state across all models of that engine), per
//! `docs/research/Rust基础选型-2026-09-27.md` §4.
//!
//! This PR lands the first half: [`adapter`] ([`RuntimeAdapter`], the
//! engine-agnostic trait a real integration implements) and [`mock`]
//! ([`MockAdapter`], a test double). `eviction`'s pure `plan_eviction`
//! function and the `supervisor` event loop itself land in follow-up PRs.
//!
//! - [`error`] — [`BackendError`], with a stable `reason_code()`.
//! - [`types`] — wire/value types mirroring `packages/adapters/src/backend.ts`,
//!   minus `Admission` (moved to a Supervisor-level `plan_eviction`, so the
//!   two sides are no longer 1:1 on that one type).

pub mod adapter;
pub mod error;
pub mod mock;
pub mod types;

pub use adapter::RuntimeAdapter;
pub use error::BackendError;
pub use mock::MockAdapter;
pub use types::{BackendStatus, ChatMessage, ChatRequest, ChatResponse, ModelInfo, Pressure};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use tokio_util::sync::CancellationToken;

    use super::*;

    fn catalog() -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "qwen3-8b".to_string(),
            memory_gb: 8.5,
        }]
    }

    #[tokio::test]
    async fn list_returns_the_configured_catalog() {
        let adapter = MockAdapter::new(catalog());
        let models = adapter.list().await.expect("list should succeed");
        assert_eq!(models, catalog());
    }

    #[tokio::test]
    async fn load_unknown_model_is_a_backend_error_with_a_reason_code() {
        let adapter = MockAdapter::new(catalog());
        let err = adapter
            .load("does-not-exist", None)
            .await
            .expect_err("loading an unknown model must fail");
        assert_eq!(err.reason_code(), "model_not_found");
    }

    #[tokio::test]
    async fn load_then_status_reports_the_model_as_loaded() {
        let adapter = MockAdapter::new(catalog());
        adapter
            .load("qwen3-8b", None)
            .await
            .expect("load should succeed");
        let status = adapter.status().await.expect("status should succeed");
        assert_eq!(status.loaded, vec!["qwen3-8b".to_string()]);
        assert_eq!(status.pressure, Pressure::Ok);
    }

    #[tokio::test]
    async fn unload_removes_the_model_from_status() {
        let adapter = MockAdapter::new(catalog());
        adapter
            .load("qwen3-8b", None)
            .await
            .expect("load should succeed");
        adapter
            .unload("qwen3-8b")
            .await
            .expect("unload should succeed");
        let status = adapter.status().await.expect("status should succeed");
        assert!(status.loaded.is_empty());
    }

    #[tokio::test]
    async fn probe_ready_is_false_until_loaded_then_true() {
        let adapter = MockAdapter::new(catalog());
        assert_eq!(adapter.probe_ready("qwen3-8b").await, Ok(false));
        adapter
            .load("qwen3-8b", None)
            .await
            .expect("load should succeed");
        assert_eq!(adapter.probe_ready("qwen3-8b").await, Ok(true));
    }

    /// Negative contrast: `probe_ready` on an id the catalog has never
    /// heard of must fail loudly (`model_not_found`), not `Ok(false)` — the
    /// Supervisor treats `Ok(false)` as "still loading, keep polling",
    /// which for an id that will never exist would eventually surface as a
    /// misleading `ProbeTimedOut` instead of the real problem.
    #[tokio::test]
    async fn probe_ready_on_an_unknown_model_is_not_found_not_false() {
        let adapter = MockAdapter::new(catalog());
        let err = adapter
            .probe_ready("does-not-exist")
            .await
            .expect_err("an unknown model must fail, not silently report not-ready");
        assert_eq!(err.reason_code(), "model_not_found");
    }

    #[tokio::test]
    async fn cancelled_token_short_circuits_chat() {
        let adapter = MockAdapter::new(catalog());
        let token = CancellationToken::new();
        token.cancel();
        let err = adapter
            .chat(
                ChatRequest {
                    model: "qwen3-8b".to_string(),
                    messages: vec![ChatMessage {
                        role: "user".to_string(),
                        content: "hi".to_string(),
                    }],
                },
                token,
            )
            .await
            .expect_err("a pre-cancelled token must short-circuit chat");
        assert_eq!(err.reason_code(), "cancelled");
    }

    #[tokio::test]
    async fn chat_echoes_the_last_message() {
        let adapter = MockAdapter::new(catalog());
        let response = adapter
            .chat(
                ChatRequest {
                    model: "qwen3-8b".to_string(),
                    messages: vec![ChatMessage {
                        role: "user".to_string(),
                        content: "hello there".to_string(),
                    }],
                },
                CancellationToken::new(),
            )
            .await
            .expect("chat should succeed");
        assert_eq!(response.model, "qwen3-8b");
        assert!(response.content.contains("hello there"));
    }

    #[test]
    fn unknown_pressure_is_conservative() {
        assert!(!Pressure::Ok.at_least_soft());
        for tier in [
            Pressure::Soft,
            Pressure::Hard,
            Pressure::Ceiling,
            Pressure::Unknown,
        ] {
            assert!(
                tier.at_least_soft(),
                "{tier:?} must be treated as at least soft"
            );
        }
    }

    #[test]
    fn pressure_serializes_to_the_omlx_shaped_strings() {
        for (tier, expected) in [
            (Pressure::Ok, "\"ok\""),
            (Pressure::Soft, "\"soft\""),
            (Pressure::Hard, "\"hard\""),
            (Pressure::Ceiling, "\"ceiling\""),
            (Pressure::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&tier).unwrap(), expected);
        }
    }
}
