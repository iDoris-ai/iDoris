//! Upstream inference clients for iDoris: this crate is the one place a
//! local runtime engine (oMLX, implementing an
//! `idoris_backend::RuntimeAdapter`) and a remote HTTP provider client
//! (`RemoteChat`, OpenAI/Anthropic-compatible endpoints) both live, behind
//! stable, non-spoofable error reason codes ([`error::UpstreamError`]).
//!
//! This PR lands only [`error::UpstreamError`] — the `RemoteChat` trait
//! R2-D depends on, and the concrete oMLX/remote-client implementations,
//! follow in the next PRs on this branch stack, built on top of this
//! error type.

pub mod error;

pub use error::UpstreamError;
