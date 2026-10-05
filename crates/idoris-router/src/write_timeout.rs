//! Per-connection write deadlines for the HTTP server.

use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Sleep, sleep};

use crate::connection::{ConnectionInfo, ConnectionListener, ConnectionTagged};

/// Production limit for a connection that cannot accept response bytes.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// An Axum listener that gives each accepted connection a write deadline.
pub struct WriteTimeoutListener<L> {
    inner: L,
    timeout: Duration,
}

impl<L> WriteTimeoutListener<L> {
    /// Wraps a listener, timing out writes that remain blocked for `timeout`.
    pub fn new(inner: L, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

impl<L: Listener> Listener for WriteTimeoutListener<L> {
    type Io = WriteTimeoutIo<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, addr) = self.inner.accept().await;
        (WriteTimeoutIo::new(io, self.timeout), addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// An IO stream whose pending writes and flushes have a deadline.
pub struct WriteTimeoutIo<T> {
    inner: T,
    timeout: Duration,
    deadline: Option<Pin<Box<Sleep>>>,
    flush_only_deadline: bool,
    timed_out: bool,
}

impl<T: ConnectionTagged> ConnectionTagged for WriteTimeoutIo<T> {
    fn connection_token(&self) -> tokio_util::sync::CancellationToken {
        self.inner.connection_token()
    }
}

impl<'a> Connected<IncomingStream<'a, WriteTimeoutListener<ConnectionListener>>>
    for ConnectionInfo
{
    fn connect_info(stream: IncomingStream<'a, WriteTimeoutListener<ConnectionListener>>) -> Self {
        ConnectionInfo::from_parts(*stream.remote_addr(), stream.io().connection_token())
    }
}

impl<T> WriteTimeoutIo<T> {
    fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: None,
            flush_only_deadline: false,
            timed_out: false,
        }
    }

    fn poll_deadline(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.timed_out
            || self
                .deadline
                .as_mut()
                .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready())
        {
            self.timed_out = true;
            return Poll::Ready(Err(Self::timeout_error()));
        }
        Poll::Pending
    }

    fn timeout_error() -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, "connection write timed out")
    }

    fn arm_deadline(&mut self, cx: &mut Context<'_>, flush: bool) -> Poll<io::Result<()>> {
        if self.deadline.is_none() || (self.flush_only_deadline && !flush) {
            self.deadline = Some(Box::pin(sleep(self.timeout)));
            self.flush_only_deadline = flush;
        }
        self.poll_deadline(cx)
    }

    fn poll_control(
        &mut self,
        cx: &mut Context<'_>,
        flush: bool,
        poll: impl FnOnce(Pin<&mut T>, &mut Context<'_>) -> Poll<io::Result<()>>,
    ) -> Poll<io::Result<()>>
    where
        T: AsyncWrite + Unpin,
    {
        if let Poll::Ready(error) = self.poll_deadline(cx) {
            return Poll::Ready(error);
        }
        match poll(Pin::new(&mut self.inner), cx) {
            Poll::Pending => self.arm_deadline(cx, flush),
            Poll::Ready(Ok(())) => {
                if self.flush_only_deadline && flush {
                    self.deadline = None;
                    self.flush_only_deadline = false;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteTimeoutIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Reads remain fully owned by the HTTP server. In particular, this
        // wrapper never pre-reads or consumes a request body to enforce a
        // response write deadline.
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteTimeoutIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Poll::Ready(error) = self.poll_deadline(cx) {
            return Poll::Ready(error.map(|()| 0));
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => match self.arm_deadline(cx, false) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(error) => Poll::Ready(error.map(|()| 0)),
            },
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.deadline = None;
                    self.flush_only_deadline = false;
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }

    fn is_write_vectored(&self) -> bool {
        // Keep hyper's queued Bytes owners alive through writes; its fallback
        // flattens whole bodies into an unguarded buffer and drops their permits.
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if let Poll::Ready(error) = self.poll_deadline(cx) {
            return Poll::Ready(error.map(|()| 0));
        }
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Pending => match self.arm_deadline(cx, false) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(error) => Poll::Ready(error.map(|()| 0)),
            },
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.deadline = None;
                    self.flush_only_deadline = false;
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_control(cx, true, |inner, cx| inner.poll_flush(cx))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_control(cx, false, |inner, cx| inner.poll_shutdown(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::poll_fn;

    use tokio::io::AsyncWrite;

    use super::{Context, Duration, Pin, Poll, WriteTimeoutIo, io};

    struct StalledWriter;

    impl AsyncWrite for StalledWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn ready_flush_does_not_clear_a_stalled_write_deadline() {
        let mut io = WriteTimeoutIo::new(StalledWriter, Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            poll_fn(|cx| {
                match Pin::new(&mut io).poll_write(cx, b"response") {
                    Poll::Ready(result) => return Poll::Ready(result),
                    Poll::Pending => {}
                }
                assert!(Pin::new(&mut io).poll_flush(cx).is_ready());
                Poll::Pending
            }),
        )
        .await;

        assert!(matches!(result, Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut));
    }

    struct FlushPendingOnce(Cell<bool>);

    impl AsyncWrite for FlushPendingOnce {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.0.replace(false) {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct ShutdownStallsButWrites;

    impl AsyncWrite for ShutdownStallsButWrites {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_shutdown_times_out_and_latches_for_later_writes() {
        let mut io = WriteTimeoutIo::new(ShutdownStallsButWrites, Duration::from_secs(5));
        let shutdown_pending =
            poll_fn(|cx| Poll::Ready(matches!(Pin::new(&mut io).poll_shutdown(cx), Poll::Pending)))
                .await;
        assert!(shutdown_pending);

        tokio::time::advance(Duration::from_secs(5)).await;
        let shutdown = poll_fn(|cx| Pin::new(&mut io).poll_shutdown(cx)).await;
        assert!(matches!(shutdown, Err(error) if error.kind() == io::ErrorKind::TimedOut));

        let write = poll_fn(|cx| Pin::new(&mut io).poll_write(cx, b"ready")).await;
        assert!(matches!(write, Err(error) if error.kind() == io::ErrorKind::TimedOut));
    }

    #[tokio::test]
    async fn successful_flush_clears_its_own_deadline() {
        let mut io =
            WriteTimeoutIo::new(FlushPendingOnce(Cell::new(true)), Duration::from_millis(20));
        let first_flush =
            poll_fn(|cx| Poll::Ready(matches!(Pin::new(&mut io).poll_flush(cx), Poll::Pending)))
                .await;
        assert!(first_flush);
        assert!(matches!(
            poll_fn(|cx| Pin::new(&mut io).poll_flush(cx)).await,
            Ok(())
        ));

        tokio::time::sleep(Duration::from_millis(30)).await;
        let write = poll_fn(|cx| Pin::new(&mut io).poll_write(cx, b"ok")).await;
        assert!(matches!(write, Ok(2)));
    }
}
