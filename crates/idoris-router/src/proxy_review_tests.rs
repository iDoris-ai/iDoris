#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use axum::{Router, body::Body, extract::State, response::Response, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

struct Upstream {
    calls: AtomicUsize,
    block_call: usize,
    received: Notify,
    release: Semaphore,
}

async fn handle(State(state): State<Arc<Upstream>>, _: Bytes) -> Response {
    let call = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if call == state.block_call {
        state.received.notify_one();
        state.release.acquire().await.unwrap().forget();
    }
    let (status, body) = if call == 2 {
        (503, "busy")
    } else {
        (200, "origin-body")
    };
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .unwrap()
}

async fn setup(block_call: usize) -> (String, Arc<Upstream>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Upstream {
        calls: AtomicUsize::new(0),
        block_call,
        received: Notify::new(),
        release: Semaphore::new(0),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, state, server)
}

fn opts<'a>(id: &'a str, record_id: &'a str) -> ForwardOpts<'a> {
    ForwardOpts {
        request_id: Some(id),
        tenant_id: Some("tenant-a"),
        record_id,
        provider_id: "provider-a",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
    }
}

fn body(content: &str) -> Value {
    serde_json::json!({"model":"test", "messages":[{"role":"user","content":content}]})
}

fn proxy() -> ChatProxy {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    let mut proxy = ChatProxy::with_config(client, Duration::from_secs(60), vec![]);
    // Successful calls now retain their fingerprint through the idempotency
    // window, so the seeded success occupies one flight slot alongside the
    // later complete 503 or active flight under test.
    proxy.max_entries = 2;
    proxy
}

async fn seed(proxy: &ChatProxy, endpoint: &str) {
    let result = proxy
        .forward_buffered(endpoint, &body("A"), &opts("a", "record-a"))
        .await;
    assert_eq!(result.status, 200);
    assert_eq!(result.body.as_ref(), b"origin-body");
}

async fn assert_replay(proxy: &ChatProxy, endpoint: &str, calls: &AtomicUsize) {
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        proxy.forward_buffered(endpoint, &body("A"), &opts("a", "record-replay")),
    )
    .await
    .expect("cached replay did not complete");
    assert_eq!(result.status, 200);
    assert_eq!(result.body.as_ref(), b"origin-body");
    assert!(result.cached);
    assert_eq!(result.origin_record_id.as_deref(), Some("record-a"));
    assert_eq!(result.replayed_served_locality, Some(Locality::Loopback));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cached_replay_survives_full_complete_503_flight() {
    let (endpoint, upstream, server) = setup(usize::MAX).await;
    let proxy = proxy();
    seed(&proxy, &endpoint).await;
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &body("B"), &opts("b", "record-b"))
            .await
            .status,
        503
    );
    assert_eq!(proxy.flights.lock().unwrap().len(), 2);
    assert_replay(&proxy, &endpoint, &upstream.calls).await;
    let mut changed = opts("a", "record-changed");
    changed.privacy = PrivacyClass::LocalOnly;
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &body("A"), &changed)
            .await
            .status,
        409
    );
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &body("changed"), &opts("a", "record-x"))
            .await
            .status,
        409
    );
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &body("new"), &opts("new", "record-new"))
            .await
            .status,
        503
    );
    {
        let mut cache = proxy.cache.lock().unwrap();
        let key = cache_key(
            "tenant-a",
            &format!("{endpoint}/v1/chat/completions"),
            "provider-a",
            "a",
        );
        cache.get_mut(&key).unwrap().at = Instant::now() - Duration::from_secs(61);
    }
    let expired_cache_replay = proxy
        .forward_buffered(&endpoint, &body("A"), &opts("a", "record-expired"))
        .await;
    assert_eq!(expired_cache_replay.status, 200);
    assert_eq!(expired_cache_replay.body.as_ref(), b"origin-body");
    assert!(expired_cache_replay.cached);
    assert_eq!(
        expired_cache_replay.origin_record_id.as_deref(),
        Some("record-a")
    );
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn cached_replay_survives_another_complete_post_pending() {
    let (endpoint, upstream, server) = setup(2).await;
    let proxy = Arc::new(proxy());
    seed(&proxy, &endpoint).await;
    let pending_proxy = proxy.clone();
    let pending_endpoint = endpoint.clone();
    let pending = tokio::spawn(async move {
        pending_proxy
            .forward_buffered(&pending_endpoint, &body("B"), &opts("b", "record-b"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), upstream.received.notified())
        .await
        .expect("B POST did not reach upstream");
    assert_eq!(proxy.flights.lock().unwrap().len(), 2);
    assert_replay(&proxy, &endpoint, &upstream.calls).await;
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &body("new"), &opts("new", "record-new"))
            .await
            .status,
        503
    );
    upstream.release.add_permits(1);
    assert_eq!(pending.await.unwrap().status, 503);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    server.abort();
}
