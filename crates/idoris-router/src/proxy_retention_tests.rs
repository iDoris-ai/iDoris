#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use axum::{Router, body::Body, extract::State, response::Response, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Upstream {
    calls: AtomicUsize,
    status: u16,
    response_body: String,
}

async fn handle(State(state): State<Arc<Upstream>>, _: Bytes) -> Response {
    state.calls.fetch_add(1, Ordering::SeqCst);
    Response::builder()
        .status(state.status)
        .body(Body::from(state.response_body.clone()))
        .unwrap()
}

async fn upstream(
    status: u16,
    response_body: String,
) -> (String, Arc<Upstream>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Upstream {
        calls: AtomicUsize::new(0),
        status,
        response_body,
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(handle))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, state, task)
}

fn opts<'a>(id: &'a str) -> ForwardOpts<'a> {
    ForwardOpts {
        request_id: Some(id),
        tenant_id: Some("tenant-a"),
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
    }
}

fn proxy() -> ChatProxy {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.header_timeout = Duration::from_secs(2);
    proxy.body_timeout = Duration::from_secs(2);
    proxy.retry_delays.clear();
    proxy
}

fn cache_retained_bytes(proxy: &ChatProxy) -> usize {
    proxy
        .cache
        .lock()
        .unwrap()
        .iter()
        .map(|(key, entry)| ChatProxy::entry_bytes(key, entry))
        .sum()
}

#[tokio::test]
async fn flight_retention_is_byte_bounded_and_expires_without_reposting_ids() {
    let response_size = 64 * 1024;
    let (endpoint, upstream, server) = upstream(503, "x".repeat(response_size)).await;
    let mut proxy = proxy();
    proxy.max_flight_bytes = 96 * 1024;
    proxy.window = Duration::from_secs(60);
    proxy.permits = Arc::new(tokio::sync::Semaphore::new(1));

    for i in 0..8 {
        let id = format!("large-{i}");
        let result = proxy
            .forward_buffered(&endpoint, &serde_json::json!({"m": i}), &opts(&id))
            .await;
        assert_eq!(result.status, 503);
        assert_eq!(
            result.body.len(),
            response_size,
            "initial caller gets full response"
        );
        drop(result);
        assert_eq!(proxy.permits.available_permits(), 1);
    }
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 8);
    assert!(proxy.flight_bytes.load(Ordering::Relaxed) <= proxy.max_flight_bytes);
    let actual_outcomes_bytes: usize = {
        let flights = proxy.flights.lock().unwrap();
        flights
            .values()
            .map(|flight| {
                let outcome = flight.outcome.try_lock().unwrap();
                let outcome = outcome.as_ref().unwrap();
                outcome.body.len()
                    + outcome.content_type.as_ref().map_or(0, String::len)
                    + outcome.origin_record_id.as_ref().map_or(0, String::len)
            })
            .sum()
    };
    assert!(actual_outcomes_bytes <= proxy.max_flight_bytes);

    for i in 0..8 {
        let id = format!("large-{i}");
        let replay = proxy
            .forward_buffered(&endpoint, &serde_json::json!({"m": i}), &opts(&id))
            .await;
        if i == 0 {
            assert_eq!(replay.status, 503);
            assert_eq!(replay.body.len(), response_size);
        } else {
            assert_eq!(replay.status, 502);
            assert!(replay.body.len() < 128);
        }
        drop(replay);
    }
    let conflict = proxy
        .forward_buffered(
            &endpoint,
            &serde_json::json!({"m":"changed"}),
            &opts("large-7"),
        )
        .await;
    assert_eq!(conflict.status, 409);
    drop(conflict);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 8);

    let second_body = serde_json::json!({"m": 1});

    // Expiring uncertainty entries returns their accounted bytes, allowing a
    // fresh execution and full response retention for the same request id.
    {
        let flights = proxy.flights.lock().unwrap();
        for flight in flights.values() {
            *flight.cancelled_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(61));
        }
    }
    proxy.window = Duration::from_secs(1);
    let expired = proxy
        .forward_buffered(&endpoint, &second_body, &opts("large-1"))
        .await;
    assert_eq!(expired.status, 503);
    assert_eq!(expired.body.len(), response_size);
    drop(expired);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 9);
    assert!(proxy.flight_bytes.load(Ordering::Relaxed) <= proxy.max_flight_bytes);
    let retained_again = proxy
        .forward_buffered(&endpoint, &second_body, &opts("large-1"))
        .await;
    assert_eq!(retained_again.status, 503);
    assert_eq!(retained_again.body.len(), response_size);
    drop(retained_again);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 9);
    server.abort();
}

#[tokio::test]
async fn large_payload_fingerprints_are_fixed_size_normalized_and_context_bound() {
    let (endpoint, upstream, server) = upstream(200, "ok".to_string()).await;
    let mut proxy = proxy();
    proxy.max_cache_bytes = 16 * 1024;
    let large = "p".repeat(1024 * 1024);
    let body: Value = serde_json::from_value(serde_json::json!({
        "model": "test",
        "messages": [{"content": large, "role": "user"}],
        "stream": true
    }))
    .unwrap();
    let first = proxy
        .forward_buffered(&endpoint, &body, &opts("large"))
        .await;
    assert_eq!(first.status, 200);
    drop(first);
    assert!(cache_retained_bytes(&proxy) <= proxy.max_cache_bytes);
    {
        let cache = proxy.cache.lock().unwrap();
        let entry = cache.values().next().unwrap();
        assert!(std::mem::size_of_val(&entry.fingerprint) <= 32);
        assert!(serde_json::to_vec(&entry.fingerprint).unwrap().len() <= 129);
        assert!(ChatProxy::entry_bytes(cache.keys().next().unwrap(), entry) < 512);
    }
    {
        let flights = proxy.flights.lock().unwrap();
        let flight = flights.values().next().unwrap();
        assert!(std::mem::size_of_val(&flight.fingerprint) <= 32);
        assert!(serde_json::to_vec(&flight.fingerprint).unwrap().len() <= 129);
    }

    let reordered: Value = serde_json::from_str(&format!(
        r#"{{"stream":false,"messages":[{{"role":"user","content":{:?}}}],"model":"test"}}"#,
        large
    ))
    .unwrap();
    let replay = proxy
        .forward_buffered(&endpoint, &reordered, &opts("large"))
        .await;
    assert!(replay.cached);
    drop(replay);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);

    for id in ["large-2", "large-3"] {
        let result = proxy.forward_buffered(&endpoint, &body, &opts(id)).await;
        assert_eq!(result.status, 200);
        drop(result);
    }
    assert_eq!(proxy.cache_size_for_test(), 3);
    assert!(cache_retained_bytes(&proxy) <= proxy.max_cache_bytes);
    {
        let cache = proxy.cache.lock().unwrap();
        assert!(cache.values().all(|entry| {
            std::mem::size_of_val(&entry.fingerprint) <= 32
                && serde_json::to_vec(&entry.fingerprint).unwrap().len() <= 129
        }));
    }

    let mut changed = reordered.clone();
    changed["messages"][0]["content"] = Value::String("different".into());
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &changed, &opts("large"))
            .await
            .status,
        409
    );
    let mut privacy_changed = opts("large");
    privacy_changed.privacy = PrivacyClass::LocalOnly;
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &reordered, &privacy_changed)
            .await
            .status,
        409
    );
    let mut locality_changed = opts("large");
    locality_changed.served_locality = Locality::Remote;
    assert_eq!(
        proxy
            .forward_buffered(&endpoint, &reordered, &locality_changed)
            .await
            .status,
        409
    );
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn unretained_success_stays_fail_closed_after_cache_eviction() {
    let (endpoint, upstream, server) = upstream(200, "ok".to_string()).await;
    let mut proxy = proxy();
    proxy.max_cache_bytes = 1024;
    proxy.max_flight_bytes = 1;
    let body = serde_json::json!({"message":"executed"});

    let first = proxy
        .forward_buffered(&endpoint, &body, &opts("success"))
        .await;
    assert_eq!(first.status, 200);
    drop(first);
    assert_eq!(proxy.cache_size_for_test(), 1);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);

    // The successful response is evicted. The flight's small uncertainty
    // marker must still prevent an identical POST from being sent again.
    proxy.cache.lock().unwrap().clear();
    let replay = proxy
        .forward_buffered(&endpoint, &body, &opts("success"))
        .await;
    assert_eq!(replay.status, 502);
    assert!(replay.body.len() < 128);
    drop(replay);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    server.abort();
}
