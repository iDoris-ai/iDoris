#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use http_body_util::BodyExt;
use std::io::{Read, Write};
use std::net::TcpListener;

fn proxy() -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.header_timeout = Duration::from_millis(300);
    proxy.body_timeout = Duration::from_millis(80);
    proxy.stream_idle_timeout = Duration::from_millis(50);
    proxy.max_body_bytes = 4;
    proxy.retry_delays.clear();
    proxy
}

fn endpoint(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

fn receive_request(socket: &mut std::net::TcpStream) {
    let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buf = [0; 4096];
    let _ = socket.read(&mut buf);
}

fn server(response: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&listener);
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        receive_request(&mut socket);
        let _ = socket.write_all(response);
    });
    url
}

#[tokio::test]
async fn stream_headers_timeout_is_504_without_retry() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_millis(150)))
        .expect(1)
        .mount(&server)
        .await;
    let mut p = proxy();
    p.header_timeout = Duration::from_millis(40);
    p.retry_delays = vec![Duration::from_millis(1), Duration::from_millis(1)];
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        p.forward_stream(&server.uri(), &serde_json::json!({})),
    )
    .await
    .expect("header timeout must bound a response that never arrives");
    match result {
        StreamOutcome::Buffered { status, .. } => assert_eq!(status, 504),
        StreamOutcome::Stream { .. } => panic!("header timeout must be buffered 504"),
    }
    server.verify().await;
}

#[tokio::test]
async fn stream_idle_timeout_emits_error_after_first_chunk() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&listener);
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        receive_request(&mut socket);
        let _ =
            socket.write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n");
        std::thread::sleep(Duration::from_millis(150));
    });
    let mut body = match proxy().forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Stream { response, .. } => response,
        StreamOutcome::Buffered { status, .. } => panic!("unexpected buffered {status}"),
    };
    let first = tokio::time::timeout(Duration::from_millis(200), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.into_data().unwrap().as_ref(), b"x");
    let next = tokio::time::timeout(Duration::from_millis(200), body.frame())
        .await
        .unwrap();
    assert!(
        next.is_some(),
        "idle timeout must be an explicit body error, not EOF"
    );
    let error = next.unwrap().unwrap_err();
    assert!(error.to_string().contains("idle timeout"));
}

#[tokio::test]
async fn streaming_error_body_limits_declared_and_chunked_lengths() {
    for bytes in [
        &b"HTTP/1.1 400 Bad Request\r\ncontent-length: 5\r\n\r\n12345"[..],
        &b"HTTP/1.1 400 Bad Request\r\ntransfer-encoding: chunked\r\n\r\n5\r\n12345\r\n0\r\n\r\n"[..],
    ] {
        match proxy()
            .forward_stream(&server(bytes), &serde_json::json!({}))
            .await
        {
            StreamOutcome::Buffered { status, .. } => assert_eq!(status, 502),
            StreamOutcome::Stream { .. } => panic!("error response must be buffered"),
        }
    }
}

#[tokio::test]
async fn streaming_error_body_stall_is_504_and_truncation_is_502() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&listener);
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        receive_request(&mut socket);
        let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 4\r\n\r\na");
        std::thread::sleep(Duration::from_millis(160));
    });
    match proxy().forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Buffered { status, .. } => assert_eq!(status, 504),
        StreamOutcome::Stream { .. } => panic!("error response must be buffered"),
    }

    let url = server(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 4\r\n\r\na");
    match proxy().forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Buffered { status, .. } => assert_eq!(status, 502),
        StreamOutcome::Stream { .. } => panic!("error response must be buffered"),
    }
}

#[tokio::test]
async fn buffered_limit_is_cumulative_and_exact_limit_succeeds() {
    // Separate HTTP chunk frames force multiple smaller body chunks through
    // the client, so this detects a per-chunk check in place of cumulative accounting.
    let over = server(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\nab\r\n2\r\ncd\r\n1\r\ne\r\n0\r\n\r\n",
    );
    let p = proxy();
    let out = p
        .forward_buffered(&over, &serde_json::json!({}), &buffered_opts())
        .await;
    assert_eq!(out.status, 502);
    assert_eq!(p.cache_size_for_test(), 0);

    let exact = server(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n",
    );
    let out = proxy()
        .forward_buffered(&exact, &serde_json::json!({}), &buffered_opts())
        .await;
    assert_eq!((out.status, out.body.as_ref()), (200, b"abcd".as_slice()));
}

fn buffered_opts<'a>() -> ForwardOpts<'a> {
    ForwardOpts {
        request_id: Some("limit-check"),
        tenant_id: None,
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
    }
}
