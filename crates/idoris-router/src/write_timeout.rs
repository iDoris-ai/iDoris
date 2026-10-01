use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use axum::serve::Listener;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    time::{self, Sleep},
};

/// A TCP listener that applies a deadline to each stalled write operation.
pub struct WriteTimeoutListener {
    listener: TcpListener,
    timeout: Duration,
}

impl WriteTimeoutListener {
    pub fn new(listener: TcpListener, timeout: Duration) -> Self {
        Self { listener, timeout }
    }
}

impl Listener for WriteTimeoutListener {
    type Io = WriteTimeoutStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, addr) = Listener::accept(&mut self.listener).await;
        (WriteTimeoutStream::new(stream, self.timeout), addr)
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[doc(hidden)]
pub struct WriteTimeoutStream {
    stream: TcpStream,
    timeout: Duration,
    timer: Option<Pin<Box<Sleep>>>,
    timed_out: bool,
}

impl WriteTimeoutStream {
    fn new(stream: TcpStream, timeout: Duration) -> Self {
        Self {
            stream,
            timeout,
            timer: None,
            timed_out: false,
        }
    }
    fn check_timeout(&mut self, cx: &mut Context<'_>) -> bool {
        if self.timed_out
            || self
                .timer
                .as_mut()
                .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
        {
            self.timed_out = true;
            return true;
        }
        false
    }

    fn pending(&mut self, cx: &mut Context<'_>) -> bool {
        self.timer
            .get_or_insert_with(|| Box::pin(time::sleep(self.timeout)));
        self.check_timeout(cx)
    }
    fn timeout_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "socket write stalled past its deadline",
        )
    }
    fn clear_timer(&mut self) {
        self.timer = None;
    }

    fn poll_control(
        &mut self,
        cx: &mut Context<'_>,
        poll: impl FnOnce(Pin<&mut TcpStream>, &mut Context<'_>) -> Poll<io::Result<()>>,
    ) -> Poll<io::Result<()>> {
        if self.check_timeout(cx) {
            return Poll::Ready(Err(Self::timeout_error()));
        }
        match poll(Pin::new(&mut self.stream), cx) {
            Poll::Pending if self.pending(cx) => Poll::Ready(Err(Self::timeout_error())),
            result => result,
        }
    }
}

impl AsyncRead for WriteTimeoutStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for WriteTimeoutStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.check_timeout(cx) {
            return Poll::Ready(Err(Self::timeout_error()));
        }
        match Pin::new(&mut self.stream).poll_write(cx, buf) {
            Poll::Pending if self.pending(cx) => Poll::Ready(Err(Self::timeout_error())),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(n)) if n > 0 => {
                self.clear_timer();
                Poll::Ready(Ok(n))
            }
            Poll::Ready(result) => Poll::Ready(result),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_control(cx, |stream, cx| stream.poll_flush(cx))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_control(cx, |stream, cx| stream.poll_shutdown(cx))
    }
}
