#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use http_body_util::BodyExt;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

fn test_proxy() -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));
    proxy.header_timeout = Duration::from_millis(300);
    proxy.body_timeout = Duration::from_millis(300);
    proxy.stream_idle_timeout = Duration::from_millis(60);
    proxy.retry_delays.clear();
    proxy
}

fn endpoint(listener: &TcpListener) -> String {
    format!("http://{}", listener.local_addr().unwrap())
}

fn read_request(stream: &mut std::net::TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut bytes = [0; 4096];
    let _ = stream.read(&mut bytes);
}

fn one_response(response: &'static [u8]) -> String {
    one_response_for(response, Duration::ZERO)
}

fn one_response_for(response: &'static [u8], hold_open: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&listener);
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        let _ = stream.write_all(response);
        let _ = stream.flush();
        std::thread::sleep(hold_open);
    });
    url
}

fn opts<'a>() -> ForwardOpts<'a> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
    }
}

#[tokio::test]
async fn live_stream_uses_the_shared_permit_until_body_completion() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw("hello", "text/plain"))
        .expect(1)
        .mount(&server)
        .await;
    let url = server.uri();
    let proxy = test_proxy();
    let body = match proxy.forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Stream { response, .. } => response,
        StreamOutcome::Buffered { status, .. } => panic!("expected stream, got {status}"),
    };

    assert_eq!(proxy.permits.available_permits(), 0);
    let buffered = proxy
        .forward_buffered(&url, &serde_json::json!({}), &opts())
        .await;
    assert_eq!(buffered.status, 503);
    assert!(matches!(
        proxy.forward_stream(&url, &serde_json::json!({})).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "rejected calls must not go upstream"
    );

    let bytes = body.collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), b"hello");
    assert_eq!(proxy.permits.available_permits(), 1);
    server.verify().await;
}

#[tokio::test]
async fn stream_errors_and_unpolled_or_polled_body_drops_release_the_permit() {
    // A body idle timeout is emitted as an explicit error and releases the permit.
    let url = one_response_for(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\nx\r\n",
        Duration::from_millis(180),
    );
    let proxy = test_proxy();
    let mut body = match proxy.forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Stream { response, .. } => response,
        StreamOutcome::Buffered { status, .. } => panic!("expected stream, got {status}"),
    };
    assert!(body.frame().await.unwrap().unwrap().into_data().is_ok());
    assert!(
        tokio::time::timeout(Duration::from_millis(250), body.frame())
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(proxy.permits.available_permits(), 1);

    // A truncated upstream body yields a read error, then releases its permit.
    let url = one_response(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nx");
    let proxy = test_proxy();
    let mut body = match proxy.forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Stream { response, .. } => response,
        StreamOutcome::Buffered { status, .. } => panic!("expected stream, got {status}"),
    };
    assert!(body.frame().await.unwrap().unwrap().into_data().is_ok());
    assert!(body.frame().await.unwrap().is_err());
    assert_eq!(proxy.permits.available_permits(), 1);

    // Dropping before the first poll still drops the body-owned permit.
    let url = one_response(b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\nx");
    let proxy = test_proxy();
    let body = match proxy.forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Stream { response, .. } => response,
        StreamOutcome::Buffered { status, .. } => panic!("expected stream, got {status}"),
    };
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(body);
    assert_eq!(proxy.permits.available_permits(), 1);

    // Dropping after one chunk has been polled also releases it.
    let url = one_response(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n1\r\ny\r\n1\r\nz\r\n0\r\n\r\n",
    );
    let proxy = test_proxy();
    let mut body = match proxy.forward_stream(&url, &serde_json::json!({})).await {
        StreamOutcome::Stream { response, .. } => response,
        StreamOutcome::Buffered { status, .. } => panic!("expected stream, got {status}"),
    };
    assert!(body.frame().await.unwrap().unwrap().into_data().is_ok());
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(body);
    assert_eq!(proxy.permits.available_permits(), 1);
}

#[tokio::test]
async fn buffered_complete_failure_and_cancel_release_the_shared_permit() {
    let url = one_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
    let proxy = test_proxy();
    assert_eq!(
        proxy
            .forward_buffered(&url, &serde_json::json!({}), &opts())
            .await
            .status,
        200
    );
    assert_eq!(proxy.permits.available_permits(), 1);

    let url = one_response_for(
        b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nx",
        Duration::from_millis(180),
    );
    let mut proxy = test_proxy();
    proxy.body_timeout = Duration::from_millis(70);
    assert_eq!(
        proxy
            .forward_buffered(&url, &serde_json::json!({}), &opts())
            .await
            .status,
        504
    );
    assert_eq!(proxy.permits.available_permits(), 1);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = endpoint(&listener);
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nx");
        let _ = stream.flush();
        accepted_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
    });
    let mut in_flight_proxy = test_proxy();
    in_flight_proxy.header_timeout = Duration::from_secs(2);
    in_flight_proxy.body_timeout = Duration::from_secs(2);
    let proxy = Arc::new(in_flight_proxy);
    let in_flight = {
        let proxy = proxy.clone();
        let url = url.clone();
        tokio::spawn(async move {
            proxy
                .forward_buffered(&url, &serde_json::json!({}), &opts())
                .await
        })
    };
    tokio::task::spawn_blocking(move || accepted_rx.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(proxy.permits.available_permits(), 0);
    assert!(matches!(
        proxy.forward_stream(&url, &serde_json::json!({})).await,
        StreamOutcome::Buffered { status: 503, .. }
    ));
    in_flight.abort();
    let _ = in_flight.await;
    assert_eq!(proxy.permits.available_permits(), 1);
}

#[tokio::test]
async fn cached_buffered_bytes_keep_the_permit_until_the_last_clone_is_dropped() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"marker": "cached"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let proxy = test_proxy();
    let request = serde_json::json!({});
    let mut request_opts = opts();
    request_opts.request_id = Some("req-cache-permit");
    let first = proxy
        .forward_buffered(&server.uri(), &request, &request_opts)
        .await;
    assert_eq!(first.status, 200);
    assert!(!first.cached);
    let expected = first.body.to_vec();
    let cloned = first.body.clone();
    let slice = cloned.slice(1..);
    drop(cloned);
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(first);

    let blocked = proxy
        .forward_buffered(&server.uri(), &request, &request_opts)
        .await;
    assert_eq!(blocked.status, 503);
    assert_eq!(proxy.permits.available_permits(), 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    drop(slice);

    // The cache owns unbound bytes and therefore does not consume a permit.
    assert_eq!(proxy.permits.available_permits(), 1);
    let replay = proxy
        .forward_buffered(&server.uri(), &request, &request_opts)
        .await;
    assert!(replay.cached);
    assert_eq!(replay.body, expected);
    assert_eq!(proxy.permits.available_permits(), 0);
    drop(replay);
    assert_eq!(proxy.permits.available_permits(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}
