#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use axum::{Json, Router, extract::State, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use tokio::sync::{Notify, Semaphore};

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
