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
//!
//! ## Stream termination contract
//!
//! A well-formed [`ChatChunkStream`] must yield **exactly one terminal
//! item as its last item**, then end (`None`) — never end for any other
//! reason:
//! - an `Ok(ChatChunk { done: true, .. })`, meaning the response
//!   completed normally, or
//! - an `Err(_)`, meaning it failed.
//!
//! No further items may follow an `Err`, and no further items may follow
//! the `done: true` chunk. **A bare `None` that was not preceded by one of
//! these two is truncation, not success** — a connection that drops
//! mid-stream looks, from the caller's side, identical to one that simply
//! finished, *unless* every implementation and every caller agree on this
//! rule. A caller must never treat a bare `None` as "the response
//! completed" on its own; [`ensure_terminated`] is the enforcement point
//! that turns an implementation's possible mistake here into a guaranteed
//! `Err` instead of a silent truncation a caller has to remember to check
//! for itself.

use std::pin::Pin;
use std::task::{Context, Poll};
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
    ///
    /// The returned stream must follow this module's doc comment's
    /// "stream termination contract" — end with exactly one `done: true`
    /// `Ok` or one `Err`, never a bare `None`. Implementations that cannot
    /// yet guarantee this themselves should return
    /// `ensure_terminated(the_real_stream)` rather than the raw stream.
    async fn chat_stream(
        &self,
        req: ChatRequest,
        deadline: Instant,
    ) -> Result<ChatChunkStream, UpstreamError>;
}

/// Wraps `stream` so that ending (`None`) without first having produced a
/// terminal item (an `Ok` chunk with `done: true`, or an `Err`) is itself
/// turned into an `Err(UpstreamError::network())` item instead of a silent
/// `None` — see this module's "stream termination contract". Mapped to
/// `network_error` (not a dedicated "truncated" code) deliberately: an
/// upstream stream ending abnormally without a clean `done`/error signal
/// is, in practice, almost always a dropped connection — the same
/// dependency-failure family `network_error` already names, not a new
/// failure class a caller needs to distinguish from it.
///
/// Once a stream is known-good (guarantees the contract on its own), this
/// wrapper is redundant but harmless — it only ever forwards items
/// unchanged and adds exactly one synthetic item in the one case the
/// underlying stream got wrong.
pub fn ensure_terminated(stream: ChatChunkStream) -> ChatChunkStream {
    Box::pin(EnsureTerminated {
        inner: stream,
        terminated: false,
    })
}

struct EnsureTerminated {
    inner: ChatChunkStream,
    /// Set once a terminal item (`done: true` or `Err`) has been observed
    /// — once terminated, every later poll returns `None` unconditionally,
    /// so this wrapper itself never violates the contract it enforces.
    terminated: bool,
}

impl Stream for EnsureTerminated {
    type Item = Result<ChatChunk, UpstreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            // The contract violation this wrapper exists to catch: the
            // inner stream ended without ever producing a terminal item.
            Poll::Ready(None) => {
                self.terminated = true;
                Poll::Ready(Some(Err(UpstreamError::network())))
            }
            Poll::Ready(Some(Err(err))) => {
                self.terminated = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(chunk))) => {
                if chunk.done {
                    self.terminated = true;
                }
                Poll::Ready(Some(Ok(chunk)))
            }
        }
    }
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

    /// Yields each item of a fixed `Vec` in order (via `VecDeque::pop_front`),
    /// then `None` — unlike `OnceReady`, lets a test stream end with a bare
    /// `None` (no terminal item), exactly the case `ensure_terminated`
    /// exists to catch.
    struct FromVec<T>(std::collections::VecDeque<T>);

    impl<T: Unpin> futures_core::Stream for FromVec<T> {
        type Item = T;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<T>> {
            Poll::Ready(self.get_mut().0.pop_front())
        }
    }

    async fn drain(mut stream: ChatChunkStream) -> Vec<Result<ChatChunk, UpstreamError>> {
        let mut items = Vec::new();
        while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            items.push(item);
        }
        items
    }

    #[tokio::test]
    async fn ensure_terminated_passes_through_a_well_formed_done_stream_unchanged() {
        let chunk = ChatChunk {
            delta: "hi".to_string(),
            done: true,
        };
        let inner: ChatChunkStream = Box::pin(FromVec(std::collections::VecDeque::from([Ok(
            chunk.clone(),
        )])));
        let items = drain(ensure_terminated(inner)).await;
        assert_eq!(items, vec![Ok(chunk)]);
    }

    #[tokio::test]
    async fn ensure_terminated_passes_through_an_err_unchanged_and_stops_there() {
        let inner: ChatChunkStream = Box::pin(FromVec(std::collections::VecDeque::from([Err(
            UpstreamError::timeout(),
        )])));
        let items = drain(ensure_terminated(inner)).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].as_ref().unwrap_err().reason_code(), "timeout");
    }

    /// The regression this whole mechanism exists to prevent: a stream that
    /// ends (`None`) without ever producing a `done: true` chunk or an
    /// `Err` must not be silently treated as a successful, complete
    /// response — `ensure_terminated` must turn that bare `None` into an
    /// explicit `Err`.
    #[tokio::test]
    async fn ensure_terminated_turns_a_bare_none_into_a_network_error() {
        let not_done = ChatChunk {
            delta: "partial".to_string(),
            done: false,
        };
        let inner: ChatChunkStream = Box::pin(FromVec(std::collections::VecDeque::from([Ok(
            not_done.clone(),
        )])));
        let items = drain(ensure_terminated(inner)).await;
        assert_eq!(
            items.len(),
            2,
            "the partial chunk, then the synthesized error"
        );
        assert_eq!(items[0], Ok(not_done));
        assert_eq!(
            items[1].as_ref().unwrap_err().reason_code(),
            "network_error"
        );
    }
}
