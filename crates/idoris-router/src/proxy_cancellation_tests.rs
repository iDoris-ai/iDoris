#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use axum::{Json, Router, extract::State, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Notify, Semaphore},
};

struct Upstream {
    calls: AtomicUsize,
    received: Notify,
    release: Semaphore,
}

async fn hold_post(
    State(upstream): State<Arc<Upstream>>,
    Json(_body): Json<Value>,
) -> &'static str {
    upstream.calls.fetch_add(1, Ordering::SeqCst);
    upstream.received.notify_one();
    let permit = upstream.release.acquire().await.unwrap();
    permit.forget();
    "ok"
}

async fn start_upstream() -> (String, Arc<Upstream>, tokio::task::JoinHandle<()>) {
    let upstream = Arc::new(Upstream {
        calls: AtomicUsize::new(0),
        received: Notify::new(),
        release: Semaphore::new(0),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(hold_post))
        .with_state(upstream.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (endpoint, upstream, server)
}

fn options(request_id: &str) -> ForwardOpts<'_> {
    ForwardOpts {
        request_id: Some(request_id),
        tenant_id: Some("tenant-a"),
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
    }
}

fn body(content: &str) -> Value {
    serde_json::json!({"model": "test", "messages": [{"role": "user", "content": content}]})
}

async fn received(upstream: &Upstream) {
    tokio::time::timeout(Duration::from_secs(2), upstream.received.notified())
        .await
        .expect("upstream did not receive the complete POST");
}

async fn forward(proxy: &ChatProxy, endpoint: &str, payload: &Value, id: &str) -> ForwardOutcome {
    tokio::time::timeout(
        Duration::from_secs(2),
        proxy.forward_buffered(endpoint, payload, &options(id)),
    )
    .await
    .expect("forward_buffered did not complete")
}

#[derive(Clone, Copy)]
enum RawFault {
    DropBeforeResponse,
    TruncateResponse,
}

async fn read_complete_post(stream: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0; 2048];
        let size = stream.read(&mut chunk).await.unwrap();
        assert_ne!(size, 0, "connection closed before complete request headers");
        request.extend_from_slice(&chunk[..size]);
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .expect("POST should have a Content-Length header");
    while request.len() < header_end + content_length {
        let mut chunk = [0; 2048];
        let size = stream.read(&mut chunk).await.unwrap();
        assert_ne!(size, 0, "connection closed before complete POST body");
        request.extend_from_slice(&chunk[..size]);
    }
    assert!(request.starts_with(b"POST "));
}

async fn start_raw_fault_upstream(
    fault: RawFault,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_complete_post(&mut stream).await;
            let call = server_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                match fault {
                    RawFault::DropBeforeResponse => {}
                    RawFault::TruncateResponse => {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx")
                            .await
                            .unwrap();
                    }
                }
                // Closing after the complete POST leaves the proxy unable to
                // know whether the upstream executed it.
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            }
        }
    });
    (endpoint, calls, server)
}

async fn assert_uncertain_post_is_remembered(fault: RawFault, id: &str) {
    let (endpoint, calls, server) = start_raw_fault_upstream(fault).await;
    let proxy = ChatProxy::with_config(
        reqwest::Client::new(),
        Duration::from_secs(60),
        vec![Duration::from_millis(1)],
    );
    let original = body("same");
    let first = forward(&proxy, &endpoint, &original, id).await;
    assert_eq!(first.status, 502);
    assert_eq!(first.retries, 0);
    assert!(!first.cached);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let replay = forward(&proxy, &endpoint, &original, id).await;
    assert_eq!(replay.status, 502);
    assert_eq!(replay.retries, 0);
    assert!(!replay.cached);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        forward(&proxy, &endpoint, &body("changed"), id)
            .await
            .status,
        409
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Advance the retained unknown-result entry beyond its configured window
    // without relying on wall-clock sleeps.
    {
        let mut flights = proxy.flights.lock().unwrap();
        let url = format!("{}/v1/chat/completions", endpoint.trim_end_matches('/'));
        let key = cache_key("tenant-a", &url, "provider", id);
        let entry = flights
            .get_mut(&key)
            .expect("uncertain flight should be retained");
        *entry.cancelled_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(61));
    }
    assert_eq!(forward(&proxy, &endpoint, &original, id).await.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn consumed_post_with_lost_headers_is_remembered() {
    assert_uncertain_post_is_remembered(RawFault::DropBeforeResponse, "lost-headers").await;
}

#[tokio::test]
async fn consumed_post_with_truncated_body_is_remembered() {
    assert_uncertain_post_is_remembered(RawFault::TruncateResponse, "truncated-body").await;
}

#[tokio::test]
async fn connection_failure_can_be_retried_with_a_changed_payload() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = format!("http://{addr}");
    drop(listener);
    let proxy = ChatProxy::with_config(
        reqwest::Client::new(),
        Duration::from_secs(60),
        vec![Duration::from_millis(1)],
    );
    let failed = forward(&proxy, &endpoint, &body("first"), "connect-fail").await;
    assert_eq!(failed.status, 502);
    assert_eq!(failed.retries, 1);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let accepted = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_complete_post(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let changed = forward(&proxy, &endpoint, &body("changed"), "connect-fail").await;
    assert_eq!(changed.status, 200);
    assert_eq!(changed.retries, 0);
    accepted.await.unwrap();
}

#[tokio::test]
async fn cancelled_post_is_remembered_until_window_expires() {
    let (endpoint, upstream, server) = start_upstream().await;
    let proxy = Arc::new(ChatProxy::with_config(
        reqwest::Client::new(),
        Duration::from_millis(80),
        vec![],
    ));
    let original = body("same");
    let leader_proxy = proxy.clone();
    let leader_endpoint = endpoint.clone();
    let leader_body = original.clone();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered(&leader_endpoint, &leader_body, &options("request-1"))
            .await
    });
    received(&upstream).await;

    // Poll a second caller until it waits on the active flight's result lock.
    // Drop it with the leader so the test has no surviving waiter.
    let waiter_options = options("request-1");
    let mut waiter = Box::pin(proxy.forward_buffered(&endpoint, &original, &waiter_options));
    let pending = std::future::poll_fn(|cx| match waiter.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(true),
        Poll::Ready(_) => Poll::Ready(false),
    })
    .await;
    assert!(
        pending,
        "same-id waiter should be pending behind the leader"
    );

    // Keep the request active beyond its original start time's idempotency
    // window. Cancellation must start a fresh unknown-result window.
    tokio::time::sleep(Duration::from_millis(100)).await;
    leader.abort();
    let Err(error) = leader.await else {
        panic!("cancelled leader unexpectedly completed");
    };
    assert!(error.is_cancelled());
    drop(waiter);
    // Let the accepted upstream operation finish after the client has gone
    // away; it must not erase the uncertainty record.
    upstream.release.add_permits(2);

    let replay = forward(&proxy, &endpoint, &original, "request-1").await;
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    assert_eq!(replay.status, 502);
    let changed = forward(&proxy, &endpoint, &body("changed"), "request-1").await;
    assert_eq!(changed.status, 409);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let retry_proxy = proxy.clone();
    let retry_endpoint = endpoint.clone();
    let retry_body = original.clone();
    let retry = tokio::spawn(async move {
        retry_proxy
            .forward_buffered(&retry_endpoint, &retry_body, &options("request-1"))
            .await
    });
    received(&upstream).await;
    upstream.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(2), retry)
        .await
        .expect("retry did not complete")
        .unwrap();
    assert_eq!(result.status, 200);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn active_flight_survives_window_expiration_without_duplicate_post() {
    let (endpoint, upstream, server) = start_upstream().await;
    let proxy = Arc::new(ChatProxy::with_config(
        reqwest::Client::new(),
        Duration::from_millis(50),
        vec![],
    ));
    let leader_proxy = proxy.clone();
    let leader_endpoint = endpoint.clone();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered(&leader_endpoint, &body("same"), &options("active"))
            .await
    });
    received(&upstream).await;
    tokio::time::sleep(Duration::from_millis(70)).await;

    let waiter_body = body("same");
    let waiter_options = options("active");
    let mut waiter = Box::pin(proxy.forward_buffered(&endpoint, &waiter_body, &waiter_options));
    std::future::poll_fn(|cx| {
        assert!(waiter.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);

    upstream.release.add_permits(1);
    let leader_out = tokio::time::timeout(Duration::from_secs(2), leader)
        .await
        .expect("leader did not complete")
        .unwrap();
    let waiter_out = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter did not complete");
    assert_eq!((leader_out.status, waiter_out.status), (200, 200));
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn full_flight_capacity_does_not_evict_unknown_execution() {
    let (endpoint, upstream, server) = start_upstream().await;
    let mut proxy = ChatProxy::with_config(reqwest::Client::new(), Duration::from_secs(2), vec![]);
    proxy.max_entries = 1;
    let proxy = Arc::new(proxy);
    let leader_proxy = proxy.clone();
    let leader_endpoint = endpoint.clone();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered(&leader_endpoint, &body("same"), &options("unknown"))
            .await
    });
    received(&upstream).await;
    let active_rejected = forward(&proxy, &endpoint, &body("other"), "new-key").await;
    assert_eq!(active_rejected.status, 503);
    leader.abort();
    let Err(error) = leader.await else {
        panic!("cancelled leader unexpectedly completed");
    };
    assert!(error.is_cancelled());

    let rejected = forward(&proxy, &endpoint, &body("other"), "new-key").await;
    assert_eq!(rejected.status, 503);
    let replay = forward(&proxy, &endpoint, &body("same"), "unknown").await;
    assert_eq!(replay.status, 502);
    let conflict = forward(&proxy, &endpoint, &body("changed"), "unknown").await;
    assert_eq!(conflict.status, 409);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    server.abort();
}
