//! Upstream inference clients for iDoris: this crate is the one place a
//! local runtime engine (oMLX, implementing an
//! [`idoris_backend::RuntimeAdapter`]) and a remote HTTP provider client
//! ([`chat::RemoteChat`], OpenAI/Anthropic-compatible endpoints) both live,
//! behind stable, non-spoofable error reason codes ([`error::UpstreamError`]).
//!
//! - [`chat`] — [`chat::RemoteChat`], the remote-provider boundary R2-D
//!   (routing) depends on. Landed in this PR specifically so R2-D can build
//!   against it without waiting for the concrete oMLX/remote-client
//!   implementations that follow in later PRs on this branch stack.
//! - [`error`] — [`error::UpstreamError`] (landed in the previous PR on
//!   this stack), with a stable `reason_code()` that keeps authentication
//!   failure and dependency failure from ever collapsing into the same
//!   code.
//! - [`omlx`] — the oMLX `RuntimeAdapter` implementation (landing across
//!   several PRs on this stack; this PR has `list`/`status` only).

pub mod chat;
pub mod error;
pub mod omlx;

pub use chat::{
    ChatChunk, ChatChunkStream, ChatMessage, ChatRequest, ChatResponse, RemoteChat,
    ensure_terminated,
};
pub use error::UpstreamError;
