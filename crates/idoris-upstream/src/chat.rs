//! [`RemoteChat`] — the boundary a remote (network) chat provider client
//! implements, e.g. an OpenAI/Anthropic-compatible HTTP endpoint.
//!
//! A **separate** trait from [`idoris_backend::RuntimeAdapter`], not a
//! generalization of it: `RuntimeAdapter` is a stateful local engine whose
//! lifecycle (`load`/`unload`/`status`) the Supervisor owns; `RemoteChat`
//! is a stateless request/response (or request/stream) call to a provider
//! this process does not manage. Folding both into one trait would force
//! every remote client to fake `load`/`unload`/`status` for a "model" a
//! remote API neither loads nor unloads on our command.
//!
//! `RemoteChat` takes a `deadline: Instant` rather than a
//! `CancellationToken` (`RuntimeAdapter::chat`'s mechanism): a remote call
//! has no local process to cancel, only a point in time to stop waiting
//! at. `Instant` (absolute), not `Duration`, so a caller chaining several
//! calls against one overall budget hands every leg the same instant
//! instead of re-deriving "how much is left" each time.

use std::pin::Pin;
use std::time::Instant;

use async_trait::async_trait;
use futures_core::Stream;

pub use idoris_backend::{ChatMessage, ChatRequest, ChatResponse};

use crate::error::UpstreamError;

/// One incremental piece of a streamed chat response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatChunk {
    /// Incremental text for this chunk. May be empty for a marker/heartbeat
    /// chunk that carries no new text (e.g. a `done`-only final chunk).
    pub delta: String,
    /// `true` on the final chunk of the stream — no further items follow.
    pub done: bool,
}

/// A boxed stream of chat chunks. Boxed (not an associated type) because
/// `RemoteChat` is meant to be used as a trait object (`dyn RemoteChat`) by
/// callers that select a provider client at runtime rather than at compile
/// time.
pub type ChatChunkStream = Pin<Box<dyn Stream<Item = Result<ChatChunk, UpstreamError>> + Send>>;

#[async_trait]
pub trait RemoteChat: Send + Sync {
    /// Send `req` and wait for the full response, or fail with
    /// [`UpstreamError::Timeout`] once `deadline` passes.
    async fn chat(
        &self,
        req: ChatRequest,
        deadline: Instant,
    ) -> Result<ChatResponse, UpstreamError>;

    /// Streaming variant of [`Self::chat`]. `deadline` bounds the *whole*
    /// call, not each individual chunk: a slow-but-still-progressing
    /// stream that runs past `deadline` must still fail rather than being
    /// allowed to stream forever just because *something* keeps arriving.
    ///
    /// The outer `Result` covers failures known before any chunk is
    /// produced (e.g. the request was rejected outright); once the stream
    /// itself starts yielding items, a later failure (including the
    /// deadline elapsing mid-stream) is reported as an `Err` item on the
    /// stream instead, not by dropping it silently.
    async fn chat_stream(
        &self,
        req: ChatRequest,
        deadline: Instant,
    ) -> Result<ChatChunkStream, UpstreamError>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;

    // `futures-core` alone has no `stream::once`/combinators (those live in
    // `futures-util`, which this crate does not depend on yet) — this tiny
    // hand-rolled single-item stream is enough for the test double below.
    struct OnceReady<T>(Option<T>);

    impl<T: Unpin> futures_core::Stream for OnceReady<T> {
        type Item = T;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<T>> {
            Poll::Ready(self.get_mut().0.take())
        }
    }

    /// A minimal `RemoteChat` double, just enough to prove the trait's
    /// shape is usable both as a concrete type and as `dyn RemoteChat`,
    /// and that a deadline already in the past is something an
    /// implementation can observe and act on (a real client's timeout
    /// enforcement is exercised in its own tests, not here).
    struct AlwaysTimesOutOncePastDeadline {
        calls: Mutex<u32>,
    }

    #[async_trait]
    impl RemoteChat for AlwaysTimesOutOncePastDeadline {
        async fn chat(
            &self,
            _req: ChatRequest,
            deadline: Instant,
        ) -> Result<ChatResponse, UpstreamError> {
            *self.calls.lock().expect("mutex poisoned") += 1;
            if Instant::now() >= deadline {
                return Err(UpstreamError::timeout());
            }
            Ok(ChatResponse {
                model: "test-model".to_string(),
                content: "ok".to_string(),
            })
        }

        async fn chat_stream(
            &self,
            req: ChatRequest,
            deadline: Instant,
        ) -> Result<ChatChunkStream, UpstreamError> {
            let resp = self.chat(req, deadline).await?;
            let chunk = ChatChunk {
                delta: resp.content,
                done: true,
            };
            Ok(Box::pin(OnceReady(Some(Ok(chunk)))))
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn chat_succeeds_before_the_deadline() {
        let client = AlwaysTimesOutOncePastDeadline {
            calls: Mutex::new(0),
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        let resp = client
            .chat(request(), deadline)
            .await
            .expect("call before the deadline must succeed");
        assert_eq!(resp.content, "ok");
    }

    #[tokio::test]
    async fn chat_fails_with_timeout_reason_code_past_the_deadline() {
        let client = AlwaysTimesOutOncePastDeadline {
            calls: Mutex::new(0),
        };
        // Already in the past.
        let deadline = Instant::now() - Duration::from_secs(1);
        let err = client
            .chat(request(), deadline)
            .await
            .expect_err("a deadline already in the past must fail");
        assert_eq!(err.reason_code(), "timeout");
    }

    #[tokio::test]
    async fn dyn_remote_chat_is_object_safe_and_usable() {
        let client: Box<dyn RemoteChat> = Box::new(AlwaysTimesOutOncePastDeadline {
            calls: Mutex::new(0),
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut stream = client
            .chat_stream(request(), deadline)
            .await
            .expect("stream setup must succeed before the deadline");
        let mut items = Vec::new();
        while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            items.push(item.expect("chunk must be Ok"));
        }
        assert_eq!(items.len(), 1);
        assert!(items[0].done);
        assert_eq!(items[0].delta, "ok");
    }
}
