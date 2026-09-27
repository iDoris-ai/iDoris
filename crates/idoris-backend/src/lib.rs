// TODO(R1-redesign): 按 docs/research/Rust基础选型 §4 拆为运行时适配器 + Supervisor
//! Engine-agnostic model backend trait — 1:1 with
//! `packages/adapters/src/backend.ts`'s `ModelBackend` interface. See
//! `backend.rs` for the trait itself and `mock.rs` for [`MockBackend`], a
//! test double used by anything that depends on this crate.

pub mod backend;
pub mod error;
pub mod mock;
pub mod types;

pub use backend::ModelBackend;
pub use error::BackendError;
pub use mock::MockBackend;
pub use types::{
    Admission, BackendStatus, ChatMessage, ChatRequest, ChatResponse, ModelInfo, Pressure,
};

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
        let backend = MockBackend::new(catalog());
        let models = backend.list().await.expect("list should succeed");
        assert_eq!(models, catalog());
    }

    #[tokio::test]
    async fn load_unknown_model_is_a_backend_error_with_a_reason_code() {
        let backend = MockBackend::new(catalog());
        let err = backend
            .load("does-not-exist", None)
            .await
            .expect_err("loading an unknown model must fail");
        assert_eq!(err.reason_code(), "model_not_found");
    }

    #[tokio::test]
    async fn load_then_status_reports_the_model_as_loaded() {
        let backend = MockBackend::new(catalog());
        backend
            .load("qwen3-8b", None)
            .await
            .expect("load should succeed");
        let status = backend.status().await.expect("status should succeed");
        assert_eq!(status.loaded, vec!["qwen3-8b".to_string()]);
        assert_eq!(status.pressure, Pressure::Ok);
    }

    #[tokio::test]
    async fn unload_removes_the_model_from_status() {
        let backend = MockBackend::new(catalog());
        backend
            .load("qwen3-8b", None)
            .await
            .expect("load should succeed");
        backend
            .unload("qwen3-8b")
            .await
            .expect("unload should succeed");
        let status = backend.status().await.expect("status should succeed");
        assert!(status.loaded.is_empty());
    }

    #[tokio::test]
    async fn cancelled_token_short_circuits_chat() {
        let backend = MockBackend::new(catalog());
        let token = CancellationToken::new();
        token.cancel();
        let err = backend
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
        let backend = MockBackend::new(catalog());
        let response = backend
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
