#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use axum::serve::Listener;
use axum::{Router, http::StatusCode, response::IntoResponse, routing::any};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn opts() -> ForwardOpts<'static> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
    }
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut byte = [0; 1];
    loop {
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return bytes;
        }
    }
}

async fn slow_upstream(
    listener: tokio::net::TcpListener,
    closed: tokio::sync::oneshot::Sender<()>,
) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let head = read_head(&mut socket).await;
    let content_length = String::from_utf8_lossy(&head)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; content_length];
    if socket.read_exact(&mut body).await.is_err() {
        let _ = closed.send(());
        return;
    }
    if socket
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\nconnection: keep-alive\r\n\r\n")
        .await
        .is_err()
    {
        let _ = closed.send(());
        return;
    }
    let chunk = vec![b'x'; 64 * 1024];
    let mut wire = format!("{:x}\r\n", chunk.len()).into_bytes();
    wire.extend_from_slice(&chunk);
    wire.extend_from_slice(b"\r\n");
    // At most 64 MiB; the downstream writer should close while this is still
    // producing data, leaving the upstream response without its terminal chunk.
    for _ in 0..1024 {
        if socket.write_all(&wire).await.is_err() {
            let _ = closed.send(());
            return;
        }
    }
    // Do not report success merely because the 64 MiB cap was reached. Wait
    // until the proxy closes its upstream response after downstream timeout.
    let mut probe = [0; 1];
    assert!(!matches!(socket.read(&mut probe).await, Ok(1..)));
    let _ = closed.send(());
}

#[tokio::test]
async fn slow_reader_hits_write_deadline_and_releases_proxy_permit() {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_url = format!("http://{}", upstream.local_addr().unwrap());
    let quick = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&quick)
        .await;
    let (closed_tx, mut closed_rx) = tokio::sync::oneshot::channel();
    let upstream_task = tokio::spawn(slow_upstream(upstream, closed_tx));

    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
    proxy.stream_idle_timeout = Duration::from_secs(5);
    let proxy = Arc::new(proxy);
    let handler_proxy = proxy.clone();
    let app = Router::new().route(
        "/",
        any(move || {
            let proxy = handler_proxy.clone();
            let url = slow_url.clone();
            async move {
                match proxy.forward_stream(&url, &serde_json::json!({})).await {
                    StreamOutcome::Stream { response, .. } => response.into_response(),
                    StreamOutcome::Buffered { status, body, .. } => {
                        (StatusCode::from_u16(status).unwrap(), body).into_response()
                    }
                }
            }
        }),
    );
    let listener = crate::write_timeout::WriteTimeoutListener::new(
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Duration::from_millis(220),
    );
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);

    let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
    stalled
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();
    let head = tokio::time::timeout_at(deadline, read_head(&mut stalled))
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));
    assert!(
        String::from_utf8_lossy(&head)
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
    );
    assert_eq!(proxy.permits.available_permits(), 0);

    let result = proxy
        .forward_buffered(&quick.uri(), &serde_json::json!({}), &opts())
        .await;
    assert_eq!(result.status, 503);

    // A real socket that never reads must hit the listener write deadline.
    let permit = tokio::time::timeout_at(deadline, proxy.permits.clone().acquire_owned())
        .await
        .unwrap()
        .unwrap();
    drop(permit);
    let result = tokio::time::timeout_at(
        deadline,
        proxy.forward_buffered(&quick.uri(), &serde_json::json!({}), &opts()),
    )
    .await
    .unwrap();
    assert_eq!(
        (result.status, result.body.as_ref()),
        (200, b"ok".as_slice())
    );
    assert_eq!(proxy.permits.available_permits(), 1);

    // Draining after the deadline observes EOF/reset before any terminal chunk.
    let mut remainder = Vec::new();
    let drained = tokio::time::timeout_at(deadline, stalled.read_to_end(&mut remainder))
        .await
        .unwrap();
    // Both orderly EOF and TCP reset demonstrate that the server closed it.
    if let Err(error) = drained {
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }
    assert!(!remainder.windows(5).any(|window| window == b"0\r\n\r\n"));
    assert!(
        tokio::time::timeout_at(deadline, &mut closed_rx)
            .await
            .unwrap()
            .is_ok(),
        "downstream closure should cancel the upstream stream"
    );

    server.abort();
    upstream_task.abort();
    let _ = server.await;
    let _ = upstream_task.await;
    quick.verify().await;
}

#[tokio::test]
async fn progressing_stream_and_idle_keep_alive_outlive_write_deadline() {
    let app = Router::new().route(
        "/",
        any(|| async {
            Body::from_stream(stream::unfold(0, |n| async move {
                if n == 5 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
                Some((Ok::<_, std::io::Error>(Bytes::from_static(b"x")), n + 1))
            }))
        }),
    );
    let listener = crate::write_timeout::WriteTimeoutListener::new(
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Duration::from_millis(220),
    );
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        assert!(read_head(&mut client).await.starts_with(b"HTTP/1.1 200"));
        let mut body = Vec::new();
        client.read_to_end(&mut body).await.unwrap();
        assert_eq!(body.iter().filter(|&&b| b == b'x').count(), 5);
        assert!(body.ends_with(b"0\r\n\r\n"));
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;
}
