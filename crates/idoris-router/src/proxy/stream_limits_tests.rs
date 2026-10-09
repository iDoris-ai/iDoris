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

fn forward_opts() -> ForwardOpts<'static> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "eof-held",
        provider_id: "omlx",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
        require_openai_usage: false,
    }
}

fn accept_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).unwrap();
        assert_ne!(n, 0, "client closed before sending request");
        request.extend_from_slice(&buf[..n]);
        if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < header_end + length {
        let n = stream.read(&mut buf).unwrap();
        assert_ne!(n, 0, "client closed before request body");
        request.extend_from_slice(&buf[..n]);
    }
}

fn test_proxy(idle: Duration) -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.stream_idle_timeout = idle;
    proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
    proxy
}

async fn take_permit(proxy: &ChatProxy) {
    let permit = timeout(
        Duration::from_secs(3),
        proxy.permits.clone().acquire_owned(),
    )
    .await
    .expect("stream producer should release its permit")
    .expect("semaphore remains open");
    drop(permit);
}

#[tokio::test]
async fn normal_upstream_eof_remains_normal_eof() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\na\r\n1\r\nb\r\n1\r\nc\r\n0\r\n\r\n",
            )
            .unwrap();
    });
    let proxy = test_proxy(Duration::from_secs(1));
    let StreamOutcome::Stream { mut response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let body = timeout(Duration::from_secs(2), async {
        let mut body = Vec::new();
        while let Some(frame) = response.frame().await {
            body.extend_from_slice(&frame.unwrap().into_data().unwrap());
        }
        body
    })
    .await
    .expect("normal upstream EOF should complete");
    assert_eq!(body, b"abc");
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn unconsumed_eof_keeps_permit_past_idle_deadline_until_error_consumed() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n\r\n")
            .unwrap();
    });
    let mut proxy = test_proxy(Duration::from_millis(200));
    let eof_observed = Arc::new(tokio::sync::Notify::new());
    proxy.stream_eof_observed = Some(eof_observed.clone());
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut response = crate::terminated_proxy_body(response);

    // The hook fires only after the producer's next resp.chunk() returned
    // None. The queued byte remains deliberately unconsumed by the caller.
    timeout(Duration::from_secs(2), eof_observed.notified())
        .await
        .expect("producer should observe upstream EOF");
    assert_eq!(proxy.permits.available_permits(), 0);
    assert!(matches!(
        proxy.forward_stream(&endpoint, &request_body()).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &request_body(), &forward_opts())
            .await
            .status,
        503
    );

    // The producer's idle deadline closes upstream, but the unread body still
    // owns its permit until the terminal error is consumed or the body drops.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(proxy.permits.available_permits(), 0);
    assert!(matches!(
        proxy.forward_stream(&endpoint, &request_body()).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));
    assert_eq!(
        response
            .frame()
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        "x"
    );
    let error = timeout(Duration::from_secs(1), response.frame())
        .await
        .expect("stalled body should deliver its terminal error")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().to_lowercase().contains("idle"));
    assert_eq!(proxy.permits.available_permits(), 1);
    drop(response);
    server.join().unwrap();
}

#[tokio::test]
async fn dropping_body_closes_upstream_and_releases_permit_before_idle_deadline() {
    let (endpoint, listener) = tcp_server();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n")
            .unwrap();
        socket.flush().unwrap();
        let _ = closed_tx.send(wait_for_peer_close(&mut socket));
    });

    let mut proxy = test_proxy(Duration::from_secs(5));
    let read_waiting = Arc::new(tokio::sync::Notify::new());
    proxy.stream_read_waiting = Some(read_waiting.clone());
    let StreamOutcome::Stream { mut response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let first = timeout(Duration::from_secs(2), response.frame())
        .await
        .expect("first chunk should arrive")
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap(), "x");
    // The server keeps the response open after this chunk, so the producer
    // has no upstream EOF to finish on. Wait for its read hook before drop.
    timeout(Duration::from_secs(1), read_waiting.notified())
        .await
        .expect("producer should return to waiting for the next upstream chunk");
    drop(response);

    timeout(Duration::from_millis(500), async {
        assert!(closed_rx.await.unwrap());
        take_permit(&proxy).await;
    })
    .await
    .expect("drop should close upstream and release permit before idle timeout");
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

#[tokio::test]
async fn dropping_body_after_upstream_eof_releases_permit_without_idle_wait() {
    let (endpoint, listener) = tcp_server();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n")
            .unwrap();
    });

    let mut proxy = test_proxy(Duration::from_secs(5));
    let eof_observed = Arc::new(tokio::sync::Notify::new());
    proxy.stream_eof_observed = Some(eof_observed.clone());
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    timeout(Duration::from_secs(2), eof_observed.notified())
        .await
        .expect("producer should observe upstream EOF");
    drop(response);

    timeout(Duration::from_millis(500), async {
        let permit = proxy.permits.clone().acquire_owned().await.unwrap();
        drop(permit);
    })
    .await
    .expect("dropping an EOF body should release its permit immediately");
    assert_eq!(proxy.permits.available_permits(), 1);
    server.join().unwrap();
}

fn tcp_server() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    (address, listener)
}

fn wait_for_peer_close(socket: &mut TcpStream) -> bool {
    let mut byte = [0; 1];
    loop {
        match socket.read(&mut byte) {
            Ok(0) => return true,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return true,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return false;
            }
            Err(e) => panic!("TCP peer observation failed: {e}"),
            Ok(_) => {}
        }
    }
}

#[tokio::test]
async fn idle_upstream_expires_while_response_is_not_polled() {
    let (endpoint, listener) = tcp_server();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n")
            .unwrap();
        socket.flush().unwrap();
        let _ = closed_tx.send(wait_for_peer_close(&mut socket));
    });

    let proxy = test_proxy(Duration::from_millis(150));
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut response = crate::terminated_proxy_body(response);
    let first = timeout(Duration::from_secs(2), response.frame())
        .await
        .expect("first chunk should arrive")
        .unwrap()
        .unwrap();
    assert!(!first.into_data().unwrap().is_empty());

    // Keep `response` alive and deliberately stop polling it.
    assert!(
        timeout(Duration::from_secs(3), closed_rx)
            .await
            .expect("idle timeout should close the upstream TCP connection")
            .unwrap()
    );
    assert_eq!(proxy.permits.available_permits(), 0);
    assert!(matches!(
        proxy.forward_stream(&endpoint, &request_body()).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));
    let error = timeout(Duration::from_secs(1), response.frame())
        .await
        .expect("idle terminal error should be delivered")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().to_lowercase().contains("idle"));
    assert_eq!(proxy.permits.available_permits(), 1);
    drop(response);
    server.join().unwrap();
}

#[tokio::test]
async fn backpressured_stream_times_out_and_reports_error_when_resumed() {
    let (endpoint, listener) = tcp_server();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        accept_request(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
            .unwrap();
        socket.flush().unwrap();
        let payload = vec![b'a'; 16 * 1024];
        let chunk_head = format!("{:x}\r\n", payload.len());
        for _ in 0..512 {
            if socket.write_all(chunk_head.as_bytes()).is_err()
                || socket.write_all(&payload).is_err()
                || socket.write_all(b"\r\n").is_err()
            {
                break;
            }
            if socket.flush().is_err() {
                break;
            }
        }
        let _ = closed_tx.send(wait_for_peer_close(&mut socket));
    });

    let proxy = test_proxy(Duration::from_millis(180));
    let StreamOutcome::Stream { response, .. } =
        proxy.forward_stream(&endpoint, &request_body()).await
    else {
        panic!("expected streaming response");
    };
    let mut response = crate::terminated_proxy_body(response);
    let first = timeout(Duration::from_secs(2), response.frame())
        .await
        .expect("first chunk should arrive")
        .unwrap()
        .unwrap();
    assert!(!first.into_data().unwrap().is_empty());

    // The upstream writes enough data to fill the producer queue and socket
    // buffers. While the caller holds, but does not poll, the body, send-side
    // backpressure must have the same idle deadline as an upstream read.
    assert!(
        timeout(Duration::from_secs(3), closed_rx)
            .await
            .expect("backpressure timeout should close upstream TCP")
            .unwrap()
    );
    assert_eq!(proxy.permits.available_permits(), 0);
    assert!(matches!(
        proxy.forward_stream(&endpoint, &request_body()).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));

    // Allow the finite queue to drain, then require a body error instead of
    // clean EOF so downstream callers can detect the truncated response.
    let error = timeout(Duration::from_secs(3), async {
        loop {
            match response.frame().await {
                Some(Ok(_)) => continue,
                Some(Err(error)) => break error,
                None => panic!("timed out upstream must not become normal EOF"),
            }
        }
    })
    .await
    .expect("terminal stream error should arrive after queued chunks drain");
    assert!(error.to_string().to_lowercase().contains("backpressure"));
    assert_eq!(proxy.permits.available_permits(), 1);
    assert!(
        timeout(Duration::from_secs(1), response.frame())
            .await
            .expect("body should end after its terminal error")
            .is_none()
    );
    assert_eq!(proxy.permits.available_permits(), 1);
    drop(response);
    server.join().unwrap();
}
