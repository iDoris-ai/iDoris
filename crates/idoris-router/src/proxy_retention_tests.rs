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

async fn forward_live(
    proxy: &ChatProxy,
    endpoint: &str,
    body: &Value,
    request: &ForwardOpts<'_>,
) -> ForwardOutcome {
    tokio::time::resume();
    let outcome = proxy.forward_buffered(endpoint, body, request).await;
    tokio::time::pause();
    outcome
}

fn cache_only_entry(body: &'static [u8]) -> CacheEntry {
    CacheEntry {
        at: Instant::now(),
        fingerprint: [0; 32],
        status: 200,
        body: Bytes::from_static(body),
        record_id: "cache-only".to_string(),
        served_locality: Locality::Loopback,
    }
}

async fn successful_flight_survives_eviction(entry_limit: bool) {
    let response = "success".repeat(80);
    let (endpoint, upstream, server) = upstream(200, response.clone()).await;
    let mut proxy = proxy();
    proxy.window = Duration::from_secs(60);
    proxy.max_cache_bytes = if entry_limit { usize::MAX } else { 1400 };
    proxy.max_entries = 2;
    let first_body = serde_json::json!({"message":"first"});
    let second_body = serde_json::json!({"message":"second"});

    let first = forward_live(&proxy, &endpoint, &first_body, &opts("evicted")).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.body.as_ref(), response.as_bytes());
    drop(first);
    let first_key = cache_key(
        "tenant-a",
        &format!("{endpoint}/v1/chat/completions"),
        "provider",
        "evicted",
    );
    assert!(proxy.cache.lock().unwrap().contains_key(&first_key));
    let first_flight = proxy
        .flights
        .lock()
        .unwrap()
        .get(&first_key)
        .cloned()
        .unwrap();
    assert_eq!(
        first_flight
            .outcome
            .try_lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .status,
        200,
        "the complete successful outcome must have reserved flight capacity"
    );
    assert!(first_flight.retained_bytes.load(Ordering::Relaxed) > 0);
    drop(first_flight);

    // The byte case prunes on the second successful response write.
    let second = forward_live(&proxy, &endpoint, &second_body, &opts("filler")).await;
    assert_eq!(second.status, 200);
    drop(second);
    assert!(cache_retained_bytes(&proxy) <= proxy.max_cache_bytes);
    let second_key = cache_key(
        "tenant-a",
        &format!("{endpoint}/v1/chat/completions"),
        "provider",
        "filler",
    );
    if entry_limit {
        // `max_entries` also bounds flights, so inject cache-only entries to
        // exercise cache entry pruning without consuming flight slots.
        proxy.remember("cache-only-1".into(), cache_only_entry(b"1"));
        proxy.remember("cache-only-2".into(), cache_only_entry(b"2"));
    }
    assert!(!proxy.cache.lock().unwrap().contains_key(&first_key));
    assert_eq!(
        proxy.cache.lock().unwrap().contains_key(&second_key),
        !entry_limit
    );
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    // Retention must survive almost the entire window, not just an immediate replay.
    tokio::time::advance(Duration::from_secs(59)).await;
    let replay = forward_live(&proxy, &endpoint, &first_body, &opts("evicted")).await;
    assert_eq!(
        upstream.calls.load(Ordering::SeqCst),
        2,
        "replay must not POST"
    );
    assert_eq!(replay.status, 200);
    assert_eq!(replay.body.as_ref(), response.as_bytes());
    assert_eq!(replay.origin_record_id.as_deref(), Some("record"));
    assert_eq!(replay.replayed_served_locality, Some(Locality::Loopback));
    assert!(replay.cached);
    drop(replay);
    let conflict = forward_live(
        &proxy,
        &endpoint,
        &serde_json::json!({"message":"changed"}),
        &opts("evicted"),
    )
    .await;
    assert_eq!(conflict.status, 409);
    drop(conflict);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    let blocked = forward_live(
        &proxy,
        &endpoint,
        &serde_json::json!({"message":"new"}),
        &opts("slot-blocked"),
    )
    .await;
    assert_eq!(blocked.status, 503);
    drop(blocked);
    let slot_conflict = forward_live(
        &proxy,
        &endpoint,
        &serde_json::json!({"message":"changed"}),
        &opts("filler"),
    )
    .await;
    assert_eq!(slot_conflict.status, 409);
    drop(slot_conflict);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);

    let bytes_before_expiry = proxy.flight_bytes.load(Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(2)).await;
    let expired = forward_live(
        &proxy,
        &endpoint,
        &serde_json::json!({"message":"after expiry"}),
        &opts("filler"),
    )
    .await;
    assert_eq!(expired.status, 200);
    drop(expired);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 3);
    assert!(proxy.flight_bytes.load(Ordering::Relaxed) < bytes_before_expiry);
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn successful_flight_survives_byte_cache_eviction() {
    successful_flight_survives_eviction(false).await;
}

#[tokio::test(start_paused = true)]
async fn successful_flight_survives_entry_eviction_and_expires() {
    successful_flight_survives_eviction(true).await;
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
    assert_flight_budget(&proxy);

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
    let (endpoint, upstream, server) = upstream(200, "x".repeat(64 * 1024)).await;
    let mut proxy = proxy();
    proxy.max_cache_bytes = 128 * 1024;
    proxy.max_flight_bytes = 2048;
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

#[tokio::test]
async fn concurrent_uncacheable_flight_waiter_replays_success_from_cache() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    use tokio::sync::{Notify, Semaphore};

    async fn hold_response(
        State(state): State<Arc<(Notify, Semaphore, AtomicUsize, String)>>,
        _: Bytes,
    ) -> Response {
        state.2.fetch_add(1, Ordering::SeqCst);
        state.0.notify_one();
        state.1.acquire().await.unwrap().forget();
        Response::builder()
            .status(200)
            .body(Body::from(state.3.clone()))
            .unwrap()
    }

    let response = "x".repeat(64 * 1024);
    let state = Arc::new((
        Notify::new(),
        Semaphore::new(0),
        AtomicUsize::new(0),
        response.clone(),
    ));
    let app = Router::new()
        .route("/v1/chat/completions", post(hold_response))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut proxy = proxy();
    proxy.max_cache_bytes = 128 * 1024;
    proxy.max_flight_bytes = 2 * 1024;
    proxy.window = Duration::from_secs(60);
    let proxy = Arc::new(proxy);
    let request_body = serde_json::json!({"message":"same"});
    let leader_proxy = proxy.clone();
    let leader_endpoint = endpoint.clone();
    let leader_body = request_body.clone();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered(&leader_endpoint, &leader_body, &opts("concurrent"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), state.0.notified())
        .await
        .expect("upstream did not receive the leader POST");

    // The upstream is held, so the leader still owns the outcome mutex. Poll
    // the follower once and prove it reached that lock before releasing it.
    let follower_opts = opts("concurrent");
    let mut follower =
        std::pin::pin!(proxy.forward_buffered(&endpoint, &request_body, &follower_opts));
    let first_poll = poll_fn(|cx| std::task::Poll::Ready(follower.as_mut().poll(cx))).await;
    assert!(matches!(first_poll, Poll::Pending));

    state.1.add_permits(1);
    let leader_result = tokio::time::timeout(Duration::from_secs(2), leader)
        .await
        .expect("leader did not finish")
        .unwrap();
    let follower_result = tokio::time::timeout(Duration::from_secs(2), follower)
        .await
        .expect("follower did not finish");

    assert_eq!(leader_result.status, 200);
    assert_eq!(leader_result.body.as_ref(), response.as_bytes());
    assert_eq!(follower_result.status, 200);
    assert_eq!(follower_result.body.as_ref(), response.as_bytes());
    assert!(follower_result.cached);
    assert_eq!(follower_result.origin_record_id.as_deref(), Some("record"));
    assert_eq!(
        follower_result.replayed_served_locality,
        Some(Locality::Loopback)
    );
    assert_eq!(state.2.load(Ordering::SeqCst), 1);
    assert_flight_budget(&proxy);
    drop(leader_result);
    drop(follower_result);

    proxy.cache.lock().unwrap().clear();
    let fail_closed = proxy
        .forward_buffered(&endpoint, &request_body, &opts("concurrent"))
        .await;
    assert_eq!(fail_closed.status, 502);
    assert!(fail_closed.body.len() < 128);
    assert_eq!(state.2.load(Ordering::SeqCst), 1);
    server.abort();
}

fn assert_flight_budget(proxy: &ChatProxy) -> usize {
    // Measure the actual retained keys and results, independently of the
    // production accounting helpers (including cancellation/fallback records).
    let flights = proxy.flights.lock().unwrap();
    let mut accounted = 0;
    for (key, flight) in flights.iter() {
        let outcome = flight.outcome.try_lock().unwrap();
        let outcome = outcome.as_ref().unwrap();
        let actual = key.capacity()
            + std::mem::size_of::<Flight>()
            + std::mem::size_of::<(String, Arc<Flight>)>()
            + 2 * std::mem::size_of::<usize>()
            + outcome.body.len()
            + outcome.content_type.as_ref().map_or(0, String::capacity)
            + outcome
                .origin_record_id
                .as_ref()
                .map_or(0, String::capacity);
        let charged = flight.retained_bytes.load(Ordering::Relaxed);
        assert!(charged >= actual);
        accounted += charged;
    }
    assert_eq!(proxy.flight_bytes.load(Ordering::Relaxed), accounted);
    assert!(accounted <= proxy.max_flight_bytes);
    accounted
}

fn flight_key(endpoint: &str, id: &str) -> String {
    cache_key(
        "tenant-a",
        &format!("{}/v1/chat/completions", endpoint.trim_end_matches('/')),
        "provider",
        id,
    )
}

#[tokio::test]
async fn long_id_budget_covers_cancelled_marker_and_rejects_new_id() {
    use tokio::sync::{Notify, Semaphore};

    async fn hold(
        State(state): State<Arc<(Notify, Semaphore, AtomicUsize)>>,
        _: Bytes,
    ) -> &'static str {
        state.2.fetch_add(1, Ordering::SeqCst);
        state.0.notify_one();
        state.1.acquire().await.unwrap().forget();
        "ok"
    }

    let state = Arc::new((Notify::new(), Semaphore::new(0), AtomicUsize::new(0)));
    let app = Router::new()
        .route("/v1/chat/completions", post(hold))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let id = "i".repeat(64 * 1024);
    let key = flight_key(&endpoint, &id);
    let mut proxy = proxy();
    proxy.window = Duration::from_secs(60);
    proxy.max_flight_bytes = key.capacity() + 1024;
    let proxy = Arc::new(proxy);
    let leader_proxy = proxy.clone();
    let leader_endpoint = endpoint.clone();
    let leader_id = id.clone();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered(
                &leader_endpoint,
                &serde_json::json!({"m":"same"}),
                &opts(&leader_id),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), state.0.notified())
        .await
        .expect("upstream did not receive the POST");
    // The reservation must already exist while the POST is still active.
    assert!(proxy.flight_bytes.load(Ordering::Relaxed) >= key.capacity());
    leader.abort();
    assert!(matches!(leader.await, Err(error) if error.is_cancelled()));

    let charged = assert_flight_budget(&proxy);
    for i in 0..8 {
        let rejected = proxy
            .forward_buffered(
                &endpoint,
                &serde_json::json!({"m":"other"}),
                &opts(&format!("{i}{}", "n".repeat(64 * 1024))),
            )
            .await;
        assert_eq!(rejected.status, 503);
        assert_eq!(proxy.flights.lock().unwrap().len(), 1);
        assert_eq!(assert_flight_budget(&proxy), charged);
    }
    let replay = proxy
        .forward_buffered(&endpoint, &serde_json::json!({"m":"same"}), &opts(&id))
        .await;
    assert_eq!(replay.status, 502);
    drop(replay);
    let conflict = proxy
        .forward_buffered(&endpoint, &serde_json::json!({"m":"changed"}), &opts(&id))
        .await;
    assert_eq!(conflict.status, 409);
    drop(conflict);
    assert_eq!(state.2.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn oversized_failure_keeps_bounded_marker_and_expiry_releases_budget() {
    let (endpoint, upstream, server) = upstream(503, "x".repeat(64 * 1024)).await;
    let id = "a".repeat(64 * 1024);
    let key = flight_key(&endpoint, &id);
    let mut proxy = proxy();
    proxy.window = Duration::from_secs(60);
    proxy.max_flight_bytes = key.capacity() + 1024;
    let first = proxy
        .forward_buffered(&endpoint, &serde_json::json!({"m":"first"}), &opts(&id))
        .await;
    assert_eq!(first.status, 503);
    assert_eq!(first.body.len(), 64 * 1024);
    drop(first);
    let charged = assert_flight_budget(&proxy);
    let replay = proxy
        .forward_buffered(&endpoint, &serde_json::json!({"m":"first"}), &opts(&id))
        .await;
    assert_eq!(replay.status, 502);
    assert!(replay.body.len() < 128);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    drop(replay);
    {
        let flights = proxy.flights.lock().unwrap();
        let flight = flights.get(&key).unwrap();
        *flight.cancelled_at.lock().unwrap() = Some(Instant::now() - Duration::from_secs(61));
    }
    let replacement = proxy
        .forward_buffered(&endpoint, &serde_json::json!({"m":"second"}), &opts(&id))
        .await;
    assert_eq!(replacement.status, 503);
    drop(replacement);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    assert_eq!(assert_flight_budget(&proxy), charged);
    server.abort();
}
