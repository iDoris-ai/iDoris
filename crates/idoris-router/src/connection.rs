//! Real TCP connection lifetime -> request cancellation.
//!
//! Axum calls the make-service once per accepted connection and reuses the
//! returned service for all keep-alive requests on that connection. The IO
//! wrapper below owns one connection token; request middleware derives a
//! fresh child token for every request.

use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use axum::extract::Request;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::middleware::Next;
use axum::response::Response;
use axum::serve::{IncomingStream, Listener};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::poll::{PollFd, PollFlags, poll};
#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::sync::mpsc;

#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    peer: SocketAddr,
    cancellation: CancellationToken,
}

impl ConnectionInfo {
    pub(crate) fn from_parts(peer: SocketAddr, cancellation: CancellationToken) -> Self {
        Self { peer, cancellation }
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn request_token(&self) -> CancellationToken {
        self.cancellation.child_token()
    }
}

#[derive(Clone, Debug)]
pub struct RequestLifecycle {
    peer: Option<SocketAddr>,
    cancellation: CancellationToken,
}

impl RequestLifecycle {
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    fn detached() -> Self {
        Self {
            peer: None,
            cancellation: CancellationToken::new(),
        }
    }
}

pub async fn request_lifecycle_middleware(mut request: Request, next: Next) -> Response {
    let lifecycle = request
        .extensions()
        .get::<ConnectInfo<ConnectionInfo>>()
        .map(|ConnectInfo(connection)| RequestLifecycle {
            peer: Some(connection.peer()),
            cancellation: connection.request_token(),
        })
        .unwrap_or_else(RequestLifecycle::detached);
    request.extensions_mut().insert(lifecycle);
    next.run(request).await
}

pub trait ConnectionTagged {
    fn connection_token(&self) -> CancellationToken;
}

pub struct ConnectionListener {
    inner: TcpListener,
    shutdown: CancellationToken,
    monitor: DisconnectMonitor,
}

impl ConnectionListener {
    pub fn new(inner: TcpListener, shutdown: CancellationToken) -> io::Result<Self> {
        Ok(Self {
            inner,
            shutdown,
            monitor: DisconnectMonitor::new()?,
        })
    }
}

impl Listener for ConnectionListener {
    type Io = ConnectionIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, peer) = match self.inner.accept().await {
                Ok(value) => value,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };
            match ConnectionIo::new(stream, self.shutdown.child_token(), &self.monitor) {
                Ok(io) => return (io, peer),
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl<'a> Connected<IncomingStream<'a, ConnectionListener>> for ConnectionInfo {
    fn connect_info(stream: IncomingStream<'a, ConnectionListener>) -> Self {
        Self {
            peer: *stream.remote_addr(),
            cancellation: stream.io().connection_token(),
        }
    }
}

pub struct ConnectionIo {
    inner: TcpStream,
    cancellation: CancellationToken,
    _registration: MonitorRegistration,
}

impl ConnectionIo {
    fn new(
        stream: TcpStream,
        cancellation: CancellationToken,
        monitor: &DisconnectMonitor,
    ) -> io::Result<Self> {
        let std_stream = stream.into_std()?;
        let monitor_stream = std_stream.try_clone()?;
        std_stream.set_nonblocking(true)?;
        monitor_stream.set_nonblocking(true)?;
        let inner = TcpStream::from_std(std_stream)?;
        let registration = monitor.register(monitor_stream, cancellation.clone())?;
        Ok(Self {
            inner,
            cancellation,
            _registration: registration,
        })
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[cfg(unix)]
struct DisconnectMonitor {
    tx: mpsc::Sender<MonitorCommand>,
    next_id: AtomicU64,
}

#[cfg(not(unix))]
struct DisconnectMonitor;

#[cfg(unix)]
enum MonitorCommand {
    Add {
        id: u64,
        stream: std::net::TcpStream,
        cancellation: CancellationToken,
    },
    Remove(u64),
}

#[cfg(unix)]
struct MonitoredConnection {
    stream: std::net::TcpStream,
    cancellation: CancellationToken,
}

#[cfg(unix)]
impl DisconnectMonitor {
    fn new() -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("idoris-connection-monitor".into())
            .spawn(move || monitor_connections(rx))
            .map_err(|error| io::Error::other(format!("connection monitor: {error}")))?;
        Ok(Self {
            tx,
            next_id: AtomicU64::new(1),
        })
    }

    fn register(
        &self,
        stream: std::net::TcpStream,
        cancellation: CancellationToken,
    ) -> io::Result<MonitorRegistration> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.tx
            .send(MonitorCommand::Add {
                id,
                stream,
                cancellation,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection monitor stopped"))?;
        Ok(MonitorRegistration {
            id,
            tx: self.tx.clone(),
        })
    }
}

#[cfg(unix)]
fn monitor_connections(rx: mpsc::Receiver<MonitorCommand>) {
    let mut connections = BTreeMap::<u64, MonitoredConnection>::new();
    loop {
        while let Ok(command) = rx.try_recv() {
            apply_monitor_command(command, &mut connections);
        }
        if connections.is_empty() {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(command) => {
                    apply_monitor_command(command, &mut connections);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let ids = connections.keys().copied().collect::<Vec<_>>();
        let mut fds = ids
            .iter()
            .filter_map(|id| connections.get(id))
            .map(|connection| PollFd::new(connection.stream.as_fd(), PollFlags::POLLIN))
            .collect::<Vec<_>>();
        let poll_result = poll(&mut fds, 50_u16);
        let closed = match poll_result {
            Err(Errno::EINTR) => Vec::new(),
            Err(_) => ids.clone(),
            Ok(_) => fds
                .iter()
                .zip(ids.iter().copied())
                .filter_map(|(fd, id)| {
                    let events = fd.revents().unwrap_or_else(PollFlags::empty);
                    let hard_close = events
                        .intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL);
                    let eof = events.contains(PollFlags::POLLIN)
                        && connections.get(&id).is_some_and(|connection| {
                            let mut byte = [0_u8; 1];
                            matches!(connection.stream.peek(&mut byte), Ok(0))
                        });
                    (hard_close || eof).then_some(id)
                })
                .collect::<Vec<_>>(),
        };
        drop(fds);
        for id in closed {
            if let Some(connection) = connections.remove(&id) {
                connection.cancellation.cancel();
            }
        }
        if !connections.is_empty() {
            // A duplicate fd can remain level-triggered POLLIN while Hyper
            // drains request bytes. Avoid spinning the monitor thread; this
            // never consumes request bytes.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(unix)]
fn apply_monitor_command(
    command: MonitorCommand,
    connections: &mut BTreeMap<u64, MonitoredConnection>,
) {
    match command {
        MonitorCommand::Add {
            id,
            stream,
            cancellation,
        } => {
            connections.insert(
                id,
                MonitoredConnection {
                    stream,
                    cancellation,
                },
            );
        }
        MonitorCommand::Remove(id) => {
            connections.remove(&id);
        }
    }
}

#[cfg(not(unix))]
impl DisconnectMonitor {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    fn register(
        &self,
        _stream: std::net::TcpStream,
        _cancellation: CancellationToken,
    ) -> io::Result<MonitorRegistration> {
        Ok(MonitorRegistration)
    }
}

#[cfg(unix)]
struct MonitorRegistration {
    id: u64,
    tx: mpsc::Sender<MonitorCommand>,
}

#[cfg(unix)]
impl Drop for MonitorRegistration {
    fn drop(&mut self) {
        let _ = self.tx.send(MonitorCommand::Remove(self.id));
    }
}

#[cfg(not(unix))]
struct MonitorRegistration;

impl ConnectionTagged for ConnectionIo {
    fn connection_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Drop for ConnectionIo {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl AsyncRead for ConnectionIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        let remaining = buffer.remaining();
        match Pin::new(&mut self.inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) => {
                if remaining > 0 && buffer.filled().len() == before {
                    self.cancel();
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => {
                self.cancel();
                Poll::Ready(Err(error))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for ConnectionIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, bytes) {
            Poll::Ready(Err(error)) => {
                self.cancel();
                Poll::Ready(Err(error))
            }
            other => other,
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write_vectored(cx, buffers) {
            Poll::Ready(Err(error)) => {
                self.cancel();
                Poll::Ready(Err(error))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Ready(Err(error)) => {
                self.cancel();
                Poll::Ready(Err(error))
            }
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.inner).poll_shutdown(cx) {
            Poll::Ready(result) => {
                self.cancel();
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{ConnectInfo, Extension, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;
    use tower::ServiceExt;

    use super::*;

    struct Probe {
        entered: Notify,
        token: Mutex<Option<CancellationToken>>,
    }

    async fn wait_for_disconnect(
        ConnectInfo(connection): ConnectInfo<ConnectionInfo>,
        State(probe): State<Arc<Probe>>,
        _body: Bytes,
    ) -> StatusCode {
        *probe.token.lock().unwrap() = Some(connection.request_token());
        probe.entered.notify_one();
        std::future::pending::<()>().await;
        StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn raw_connection_monitor_cancels_when_peer_closes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(address);
        let accept = listener.accept();
        let (client, accepted) = tokio::join!(client, accept);
        let client = client.unwrap();
        let (server, _) = accepted.unwrap();
        let token = CancellationToken::new();
        let monitor = DisconnectMonitor::new().unwrap();
        let io = ConnectionIo::new(server, token.clone(), &monitor).unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), token.cancelled())
            .await
            .expect("raw connection monitor must observe peer close");
        drop(io);
    }

    #[tokio::test]
    async fn completed_request_body_then_tcp_close_cancels_connection_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let probe = Arc::new(Probe {
            entered: Notify::new(),
            token: Mutex::new(None),
        });
        let app = Router::new()
            .route("/probe", post(wait_for_disconnect))
            .with_state(probe.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                ConnectionListener::new(listener, shutdown.clone()).unwrap(),
                app.into_make_service_with_connect_info::<ConnectionInfo>(),
            )
            .await
            .unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"POST /probe HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), probe.entered.notified())
            .await
            .unwrap();
        let token = probe
            .token
            .lock()
            .unwrap()
            .clone()
            .expect("handler must publish its connection token");
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), token.cancelled())
            .await
            .expect("server must observe disconnect after the complete request");
        server.abort();
    }

    #[derive(Clone)]
    struct RequestProbe(Arc<AtomicUsize>);

    async fn request_token_is_fresh(
        Extension(lifecycle): Extension<RequestLifecycle>,
        State(probe): State<RequestProbe>,
    ) -> StatusCode {
        let index = probe.0.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            lifecycle.cancellation.cancel();
        } else {
            assert!(!lifecycle.cancellation.is_cancelled());
        }
        StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn middleware_derives_a_fresh_child_token_per_request() {
        let connection = ConnectionInfo {
            peer: "127.0.0.1:1234".parse().unwrap(),
            cancellation: CancellationToken::new(),
        };
        let probe = RequestProbe(Arc::new(AtomicUsize::new(0)));
        let app = Router::new()
            .route("/", get(request_token_is_fresh))
            .layer(axum::middleware::from_fn(request_lifecycle_middleware))
            .with_state(probe.clone());

        for _ in 0..2 {
            let mut request = axum::http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(connection.clone()));
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::NO_CONTENT
            );
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
    }

    async fn read_response_head(stream: &mut tokio::net::TcpStream) {
        let mut response = Vec::new();
        loop {
            if response.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "connection closed before response headers");
            response.extend_from_slice(&chunk[..read]);
        }
    }

    #[tokio::test]
    async fn real_keep_alive_requests_receive_fresh_child_tokens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let probe = RequestProbe(Arc::new(AtomicUsize::new(0)));
        let app = Router::new()
            .route("/", get(request_token_is_fresh))
            .layer(axum::middleware::from_fn(request_lifecycle_middleware))
            .with_state(probe.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                ConnectionListener::new(listener, shutdown).unwrap(),
                app.into_make_service_with_connect_info::<ConnectionInfo>(),
            )
            .await
            .unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        for _ in 0..2 {
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            read_response_head(&mut client).await;
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn server_shutdown_cancels_existing_connection_tokens() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let shutdown_for_listener = shutdown.clone();
        let probe = Arc::new(Probe {
            entered: Notify::new(),
            token: Mutex::new(None),
        });
        let app = Router::new()
            .route("/probe", post(wait_for_disconnect))
            .with_state(probe.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                ConnectionListener::new(listener, shutdown_for_listener).unwrap(),
                app.into_make_service_with_connect_info::<ConnectionInfo>(),
            )
            .await
            .unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"POST /probe HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), probe.entered.notified())
            .await
            .unwrap();
        let token = probe.token.lock().unwrap().clone().unwrap();
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), token.cancelled())
            .await
            .expect("server shutdown must cancel the connection token");
        drop(client);
        server.abort();
    }
}
