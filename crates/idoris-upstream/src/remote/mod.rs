//! Remote (network) provider clients implementing [`crate::chat::RemoteChat`]
//! against OpenAI/Anthropic-compatible HTTP endpoints. Split across
//! submodules landing across several PRs on this branch stack:
//! - [`credentials`] (this PR) — [`credentials::CredentialSource`], how a
//!   client obtains its API key.
//! - the `genai`-backed client itself (follow-up PR).

pub mod credentials;

pub use credentials::{CredentialSource, EnvCredentialSource};
