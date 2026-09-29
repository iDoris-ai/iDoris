//! Remote (network) provider clients implementing [`crate::chat::RemoteChat`]
//! against OpenAI/Anthropic-compatible HTTP endpoints. Split across
//! submodules:
//! - [`credentials`] — [`credentials::CredentialSource`], how a client
//!   obtains its API key.
//! - [`client`] — [`client::RemoteClient`], the `genai`-backed
//!   implementation (non-streaming `chat` this PR; `chat_stream` in a
//!   follow-up PR on this branch stack).

pub mod client;
pub mod credentials;

pub use client::{RemoteClient, RemoteClientConfig, RemoteProviderKind};
pub use credentials::{CredentialSource, EnvCredentialSource};
