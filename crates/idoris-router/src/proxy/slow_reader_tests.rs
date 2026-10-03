#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use axum::{Json, Router, response::IntoResponse, routing::post};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time::timeout;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
};

struct TrackedListener {
    inner: TcpListener,
    dropped: Arc<tokio::sync::Notify>,
}

struct TrackedIo {
    inner: TcpStream,
    dropped: Arc<tokio::sync::Notify>,
}

impl Drop for TrackedIo {
    fn drop(&mut self) {
        self.dropped.notify_one();
    }
}

impl AsyncRead for TrackedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TrackedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl axum::serve::Listener for TrackedListener {
    type Io = TrackedIo;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (inner, addr) = self.inner.accept().await.unwrap();
        (
            TrackedIo {
                inner,
                dropped: self.dropped.clone(),
            },
            addr,
        )
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

fn test_proxy(idle: Duration) -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.stream_idle_timeout = idle;
    proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
    proxy
}

async fn read_request(socket: &mut TcpStream) {
    let mut request = Vec::new();
    let mut buf = [0; 4096];
    let header_end = loop {
        let n = socket.read(&mut buf).await.unwrap();
        assert_ne!(n, 0, "client closed before request headers");
        request.extend_from_slice(&buf[..n]);
        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let length = String::from_utf8_lossy(&request[..header_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < header_end + length {
        let n = socket.read(&mut buf).await.unwrap();
        assert_ne!(n, 0, "client closed before request body");
        request.extend_from_slice(&buf[..n]);
    }
}

fn streaming_router(proxy: Arc<ChatProxy>, endpoint: String) -> Router {
    Router::new().route(
        "/",
        post(move |Json(body): Json<Value>| {
            let proxy = proxy.clone();
            let endpoint = endpoint.clone();
            async move {
                match proxy.forward_stream(&endpoint, &body).await {
                    StreamOutcome::Stream { response, .. } => {
                        axum::response::Response::new(response)
                    }
                    _ => axum::http::StatusCode::BAD_GATEWAY.into_response(),
                }
            }
        }),
    )
}

#[tokio::test]
async fn blocked_socket_write_times_out_drops_connection_and_releases_stream_permit() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let (upstream_closed_tx, upstream_closed_rx) = tokio::sync::oneshot::channel();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        let chunk = vec![b'x'; 32 * 1024];
        loop {
            if socket.write_all(b"8000\r\n").await.is_err()
                || socket.write_all(&chunk).await.is_err()
                || socket.write_all(b"\r\n").await.is_err()
            {
                break;
            }
        }
        let _ = upstream_closed_tx.send(());
    });

    let proxy = Arc::new(test_proxy(Duration::from_millis(180)));
    let router = streaming_router(proxy.clone(), format!("http://{upstream_addr}"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dropped = Arc::new(tokio::sync::Notify::new());
    let server_dropped = dropped.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            crate::write_timeout::WriteTimeoutListener::new(
                TrackedListener {
                    inner: listener,
                    dropped: server_dropped,
                },
                Duration::from_secs(2),
            ),
            router,
        )
        .await
        .unwrap();
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    let payload = br#"{"model":"test","messages":[],"stream":true}"#;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    client.write_all(payload).await.unwrap();

    // Never read from client: reclamation must not depend on resumed Body polling.
    timeout(Duration::from_secs(1), upstream_closed_rx).await.expect("producer's idle deadline should close upstream while the downstream write is still blocked").unwrap();
    assert_eq!(
        proxy.permits.available_permits(),
        0,
        "producer timeout must retain its permit while Hyper still owns the body"
    );
    timeout(Duration::from_secs(3), dropped.notified())
        .await
        .expect("write deadline should drop the downstream IO");
    let permit = timeout(
        Duration::from_secs(1),
        proxy.permits.clone().acquire_owned(),
    )
    .await
    .expect("dropping the timed-out response should release the producer permit")
    .unwrap();
    drop(permit);
    drop(client);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(1), upstream_task)
        .await
        .expect("upstream writer should observe its closed peer")
        .unwrap();
}

#[tokio::test]
async fn reading_response_completes_before_write_deadline() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move {
        let (mut socket, _) = upstream.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n")
            .await
            .unwrap();
    });

    let proxy = Arc::new(test_proxy(Duration::from_secs(2)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dropped = Arc::new(tokio::sync::Notify::new());
    let server_dropped = dropped.clone();
    let route_proxy = proxy.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            crate::write_timeout::WriteTimeoutListener::new(
                TrackedListener {
                    inner: listener,
                    dropped: server_dropped,
                },
                Duration::from_secs(2),
            ),
            streaming_router(route_proxy, format!("http://{upstream_addr}")),
        )
        .await
        .unwrap();
    });
    let response = timeout(
        Duration::from_secs(2),
        reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .json(&serde_json::json!({"model":"test","messages":[]}))
            .send(),
    )
    .await
    .expect("response should arrive")
    .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), response.bytes())
            .await
            .unwrap()
            .unwrap()
            .as_ref(),
        b"ok"
    );
    timeout(Duration::from_secs(1), dropped.notified())
        .await
        .expect("normal response should close its downstream IO");
    let permit = timeout(
        Duration::from_secs(1),
        proxy.permits.clone().acquire_owned(),
    )
    .await
    .expect("normal EOF should release the stream permit")
    .unwrap();
    drop(permit);
    timeout(Duration::from_secs(1), upstream_task)
        .await
        .expect("upstream should finish")
        .unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
