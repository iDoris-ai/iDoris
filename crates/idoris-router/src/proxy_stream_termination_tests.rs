#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use http_body_util::BodyExt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use tokio::time::timeout;

fn request_body() -> Value {
    serde_json::json!({"model":"test","messages":[]})
}

fn test_proxy(idle: Duration) -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.stream_idle_timeout = idle;
    proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
    proxy
}

fn tcp_server() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    (endpoint, listener)
}

fn accept_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        assert_ne!(n, 0, "client closed before sending request");
        request.extend_from_slice(&buf[..n]);
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < end + 4 + length {
            let n = stream.read(&mut buf).unwrap();
            assert_ne!(n, 0, "client closed before request body");
            request.extend_from_slice(&buf[..n]);
        }
        return;
    }
}

fn terminated_body(response: axum::body::Body) -> axum::body::Body {
    crate::terminated_proxy_body(response)
}

#[tokio::test]
async fn missing_done_reports_unexpected_eof_and_keeps_permit_with_retained_chunk() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .unwrap();
        let data = b"data: partial\n\n";
        write!(socket, "{:x}\r\n", data.len()).unwrap();
        socket.write_all(data).unwrap();
        socket.write_all(b"\r\n").unwrap();
        socket.write_all(b"0\r\n\r\n").unwrap();
    });
    let proxy = test_proxy(Duration::from_secs(2));
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut body = terminated_body(response);

    let chunk = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("SSE payload should arrive")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(proxy.permits.available_permits(), 0);
    assert!(matches!(
        proxy.forward_stream(&endpoint, &request_body()).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));

    let error = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("missing terminal marker should fail")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "unterminated upstream SSE");
    assert!(
        body.frame().await.is_none(),
        "only one terminal error is emitted"
    );
    // The terminal error ends the Body stream, but an already emitted chunk
    // still owns the permit until the downstream releases those bytes.
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(chunk);
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn missing_done_keeps_permit_after_payload_drop_until_terminal_error_is_read() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .unwrap();
        let data = b"data: partial\n\n";
        write!(socket, "{:x}\r\n", data.len()).unwrap();
        socket.write_all(data).unwrap();
        socket.write_all(b"\r\n0\r\n\r\n").unwrap();
    });
    let mut proxy = test_proxy(Duration::from_secs(2));
    let eof_observed = Arc::new(tokio::sync::Notify::new());
    proxy.stream_eof_observed = Some(eof_observed.clone());
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut body = terminated_body(response);

    let payload = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("SSE payload should arrive")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    drop(payload);
    timeout(Duration::from_secs(2), eof_observed.notified())
        .await
        .expect("producer should observe natural upstream EOF");
    assert_eq!(proxy.permits.available_permits(), 0);

    let error = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("missing terminal marker should fail")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.to_string(), "unterminated upstream SSE");
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn dropping_unfinished_body_after_natural_eof_releases_permit() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .unwrap();
        let data = b"data: partial\n\n";
        write!(socket, "{:x}\r\n", data.len()).unwrap();
        socket.write_all(data).unwrap();
        socket.write_all(b"\r\n0\r\n\r\n").unwrap();
    });
    let mut proxy = test_proxy(Duration::from_secs(5));
    let eof_observed = Arc::new(tokio::sync::Notify::new());
    proxy.stream_eof_observed = Some(eof_observed.clone());
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let body = terminated_body(response);
    timeout(Duration::from_secs(2), eof_observed.notified())
        .await
        .expect("producer should observe natural upstream EOF");
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(body);
    let released = timeout(
        Duration::from_millis(500),
        proxy.permits.clone().acquire_owned(),
    )
    .await
    .expect("dropping unfinished body should release permit")
    .expect("semaphore remains open");
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(released);
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn done_marker_completes_without_error() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .unwrap();
        let data = b"data: [DONE]\n\n";
        write!(socket, "{:x}\r\n", data.len()).unwrap();
        socket.write_all(data).unwrap();
        socket.write_all(b"\r\n").unwrap();
        socket.write_all(b"0\r\n\r\n").unwrap();
    });
    let proxy = test_proxy(Duration::from_secs(2));
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut body = terminated_body(response);
    let mut chunks = Vec::new();
    while let Some(frame) = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("terminated stream should complete")
    {
        chunks.push(frame.unwrap().into_data().unwrap());
    }
    let mut joined = Vec::new();
    for chunk in &chunks {
        joined.extend_from_slice(chunk);
    }
    assert_eq!(joined, b"data: [DONE]\n\n");
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(chunks);
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn upstream_idle_error_is_not_replaced_by_missing_done_error() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .unwrap();
        let data = b"data: partial\n\n";
        write!(socket, "{:x}\r\n", data.len()).unwrap();
        socket.write_all(data).unwrap();
        socket.write_all(b"\r\n").unwrap();
        socket.flush().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut closed = [0u8; 1];
        let _ = socket.read(&mut closed);
    });
    let proxy = test_proxy(Duration::from_millis(80));
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut body = terminated_body(response);
    let chunk = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("SSE payload should arrive")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let error = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("idle timeout should reach body")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().to_lowercase().contains("idle"));
    assert!(
        body.frame().await.is_none(),
        "idle failure must be reported once"
    );
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(chunk);
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}
