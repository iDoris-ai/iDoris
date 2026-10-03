//! Per-connection write deadlines for the HTTP server.

use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Sleep, sleep};

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
}

impl<T> WriteTimeoutIo<T> {
    fn new(inner: T, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: None,
            flush_only_deadline: false,
        }
    }

    fn poll_deadline(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(deadline) = self.deadline.as_mut()
            && deadline.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection write timed out",
            )));
        }
        Poll::Pending
    }

    fn arm_deadline(&mut self, cx: &mut Context<'_>, flush: bool) -> Poll<io::Result<()>> {
        if self.deadline.is_none() || (self.flush_only_deadline && !flush) {
            self.deadline = Some(Box::pin(sleep(self.timeout)));
            self.flush_only_deadline = flush;
        }
        self.poll_deadline(cx)
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for WriteTimeoutIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for WriteTimeoutIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Poll::Ready(result) = self.poll_deadline(cx) {
            return Poll::Ready(result.map(|()| 0));
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Pending => match self.arm_deadline(cx, false) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => Poll::Ready(result.map(|()| 0)),
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

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if let Poll::Ready(result) = self.poll_deadline(cx) {
            return Poll::Ready(result.map(|()| 0));
        }
        match Pin::new(&mut self.inner).poll_write_vectored(cx, bufs) {
            Poll::Pending => match self.arm_deadline(cx, false) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => Poll::Ready(result.map(|()| 0)),
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
        if let Poll::Ready(result) = self.poll_deadline(cx) {
            return Poll::Ready(result);
        }
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Pending => self.arm_deadline(cx, true),
            Poll::Ready(Ok(())) => {
                if self.flush_only_deadline {
                    self.deadline = None;
                    self.flush_only_deadline = false;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
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
