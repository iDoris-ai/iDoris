#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn opts(id: Option<&str>) -> ForwardOpts<'_> {
    ForwardOpts {
        request_id: id,
        tenant_id: Some("breaker-test"),
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
        require_openai_usage: false,
    }
}
fn body() -> Value {
    serde_json::json!({"model":"test","messages":[]})
}
fn proxy() -> ChatProxy {
    let mut p = ChatProxy::new(reqwest::Client::new());
    p.test_clock_ms.store(0, Ordering::SeqCst);
    p.retry_delays.clear();
    p
}
fn clock(p: &ChatProxy, ms: u64) {
    p.test_clock_ms.store(ms, Ordering::SeqCst);
}
async fn server(status: u16, header: Option<&str>) -> MockServer {
    let s = MockServer::start().await;
    let mut response = ResponseTemplate::new(status).set_body_json(serde_json::json!({}));
    if let Some(h) = header {
        response = response.insert_header("retry-after", h);
    }
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&s)
        .await;
    s
}
async fn send(p: &ChatProxy, endpoint: &str, dispatches: &AtomicUsize) -> ForwardOutcome {
    p.forward_buffered_observed(endpoint, &body(), &opts(None), || async {
        dispatches.fetch_add(1, Ordering::SeqCst);
        Ok::<(), ()>(())
    })
    .await
    .unwrap()
}
fn trip(p: &ChatProxy, url: &str) {
    for _ in 0..credential_class(url).failure_threshold() {
        p.admit_provider("provider", url).unwrap().finish(500, None);
    }
}
#[tokio::test]
async fn wire_status_matrix_single_outcome_and_full_local_probe_window() {
    for status in [408, 429, 500, 502, 503, 504, 599] {
        let s = server(status, None).await;
        let p = proxy();
        let n = AtomicUsize::new(0);
        assert_eq!(send(&p, &s.uri(), &n).await.status, status);
        assert_eq!(send(&p, &s.uri(), &n).await.status, status);
        let blocked = send(&p, &s.uri(), &n).await;
        assert_eq!(blocked.status, 503);
        assert_eq!(blocked.execution, ExecutionDisposition::NotExecuted);
        assert_eq!(n.load(Ordering::SeqCst), 2);
        assert_eq!(s.received_requests().await.unwrap().len(), 2);
        clock(&p, 14_999);
        assert_eq!(send(&p, &s.uri(), &n).await.status, 503);
        clock(&p, 15_000);
        assert_eq!(send(&p, &s.uri(), &n).await.status, status);
        clock(&p, 29_999);
        assert_eq!(send(&p, &s.uri(), &n).await.status, 503);
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }
}
#[tokio::test]
async fn wire_header_cap_and_exact_boundaries() {
    for (header, ms) in [
        ("0", 0),
        ("1", 1000),
        ("299", 299_000),
        ("300", 300_000),
        ("301", 300_000),
        ("18446744073709552", 300_000),
        ("18446744073709551616", 300_000),
        ("9999999999999999999999999999999999999", 300_000),
    ] {
        for status in [429, 503] {
            let s = server(status, Some(header)).await;
            let p = proxy();
            let n = AtomicUsize::new(0);
            assert_eq!(send(&p, &s.uri(), &n).await.status, status);
            if ms > 0 {
                clock(&p, ms - 1);
                assert_eq!(send(&p, &s.uri(), &n).await.status, 503);
                assert_eq!(n.load(Ordering::SeqCst), 1);
            }
            clock(&p, ms);
            assert_eq!(send(&p, &s.uri(), &n).await.status, status);
            assert_eq!(n.load(Ordering::SeqCst), 2);
            assert_eq!(s.received_requests().await.unwrap().len(), 2);
        }
    }
}
#[tokio::test]
async fn wire_invalid_headers_do_not_open_before_threshold_and_only_429_503_use_headers() {
    for header in ["+7", "+0", "+0007", "bad", "-1", "1.5"] {
        let s = server(429, Some(header)).await;
        let p = proxy();
        let n = AtomicUsize::new(0);
        assert_eq!(send(&p, &s.uri(), &n).await.status, 429);
        assert_eq!(send(&p, &s.uri(), &n).await.status, 429);
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }
    for status in [200, 400, 408, 500, 502, 504] {
        let s = server(status, Some("300")).await;
        let p = proxy();
        let n = AtomicUsize::new(0);
        assert_eq!(send(&p, &s.uri(), &n).await.status, status);
        assert_eq!(send(&p, &s.uri(), &n).await.status, status);
        assert_eq!(s.received_requests().await.unwrap().len(), 2);
    }
}
#[tokio::test]
async fn threshold_short_header_counts_failure_before_header() {
    let s = server(503, Some("1")).await;
    let p = proxy();
    let n = AtomicUsize::new(0);
    p.admit_provider("provider", &s.uri())
        .unwrap()
        .finish(500, None);
    assert_eq!(send(&p, &s.uri(), &n).await.status, 503);
    clock(&p, 1000);
    assert_eq!(send(&p, &s.uri(), &n).await.status, 503);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    clock(&p, 15_000);
    assert_eq!(send(&p, &s.uri(), &n).await.status, 503);
    assert_eq!(n.load(Ordering::SeqCst), 2);
}
#[test]
fn dialed_url_classes_are_separate_and_apikey_uses_five_and_thirty_seconds() {
    let p = proxy();
    let local = "http://127.0.0.1:8080";
    let api = "https://api.example.invalid";
    assert_eq!(credential_class(local), CredentialClass::Local);
    assert_eq!(credential_class(api), CredentialClass::ApiKey);
    trip(&p, local);
    for _ in 0..4 {
        p.admit_provider("provider", api).unwrap().finish(500, None);
    }
    p.admit_provider("provider", api).unwrap().finish(500, None);
    clock(&p, 15_000);
    p.admit_provider("provider", local)
        .unwrap()
        .finish(200, None);
    assert!(p.admit_provider("provider", api).is_none());
    clock(&p, 30_000);
    p.admit_provider("provider", api).unwrap().finish(200, None);
}
#[tokio::test]
async fn observer_cancellation_has_one_probe_and_reopens_at_drop_time() {
    let s = server(200, None).await;
    let p = proxy();
    trip(&p, &s.uri());
    clock(&p, 15_000);
    let entered = tokio::sync::Notify::new();
    let request_body = body();
    let request_opts = opts(None);
    let endpoint = s.uri();
    let mut future =
        Box::pin(
            p.forward_buffered_observed(&endpoint, &request_body, &request_opts, || async {
                entered.notify_one();
                std::future::pending::<Result<(), ()>>().await
            }),
        );
    tokio::select! { _ = entered.notified() => {}, _ = &mut future => panic!("observer completed") }
    assert!(p.admit_provider("provider", &s.uri()).is_none());
    clock(&p, 20_000);
    drop(future);
    clock(&p, 34_999);
    assert!(p.admit_provider("provider", &s.uri()).is_none());
    clock(&p, 35_000);
    p.admit_provider("provider", &s.uri())
        .unwrap()
        .finish(200, None);
    assert!(s.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn observer_error_does_not_count_ordinary_failure_but_recovers_probe() {
    let s = server(200, None).await;
    let p = proxy();
    for _ in 0..3 {
        assert!(
            p.forward_buffered_observed(&s.uri(), &body(), &opts(None), || async {
                Err::<(), _>(())
            })
            .await
            .is_err()
        );
    }
    p.admit_provider("provider", &s.uri())
        .unwrap()
        .finish(500, None);
    assert!(p.admit_provider("provider", &s.uri()).is_some());
    // The ordinary cancelled claim above did not count as another failure.
    p.admit_provider("provider", &s.uri())
        .unwrap()
        .finish(500, None);
    clock(&p, 15_000);
    assert!(
        p.forward_buffered_observed(&s.uri(), &body(), &opts(None), || async {
            Err::<(), _>(())
        })
        .await
        .is_err()
    );
    clock(&p, 29_999);
    assert!(p.admit_provider("provider", &s.uri()).is_none());
    clock(&p, 30_000);
    p.admit_provider("provider", &s.uri())
        .unwrap()
        .finish(200, None);
    assert!(s.received_requests().await.unwrap().is_empty());
}
#[test]
fn toggles_preserve_captured_ownership_and_poisoned_settlement_recovers_probe() {
    let p = proxy();
    let url = "http://localhost";
    trip(&p, url);
    clock(&p, 15_000);
    let probe = p.admit_provider("provider", url).unwrap();
    p.set_breakers_enabled(false);
    probe.finish(200, None);
    p.set_breakers_enabled(true);
    assert!(p.admit_provider("provider", url).is_some());
    p.set_breakers_enabled(false);
    let untracked = p.admit_provider("provider", url).unwrap();
    p.set_breakers_enabled(true);
    untracked.finish(503, Some(300_000));
    assert!(p.admit_provider("provider", url).is_some());
    trip(&p, url);
    clock(&p, 30_000);
    let probe = p.admit_provider("provider", url).unwrap();
    p.set_breakers_enabled(false);
    let _ = std::panic::catch_unwind(|| {
        let _lock = p.breakers.lock().unwrap();
        panic!("poison");
    });
    clock(&p, 40_000);
    drop(probe);
    p.set_breakers_enabled(true);
    assert!(p.admit_provider("provider", url).is_none());
    let mut book = p.breakers.lock().unwrap_err().into_inner();
    assert_eq!(
        book.admit(&provider_key("provider"), CredentialClass::Local, 54_999)
            .admission,
        Admission::Rejected { retry_after_ms: 1 }
    );
    let probe = book.admit(&provider_key("provider"), CredentialClass::Local, 55_000);
    assert_eq!(probe.admission, Admission::Probe);
}
#[test]
fn stale_completion_and_existing_long_deadline_survive_short_network_header() {
    let p = proxy();
    let url = "http://localhost";
    let stale = p.admit_provider("provider", url).unwrap();
    trip(&p, url);
    clock(&p, 15_000);
    p.admit_provider("provider", url).unwrap().finish(200, None);
    stale.finish(503, Some(300_000));
    assert!(p.admit_provider("provider", url).is_some());
    let late = p.admit_provider("provider", url).unwrap();
    let t = p
        .breakers
        .lock()
        .unwrap()
        .admit(&provider_key("provider"), CredentialClass::Local, 15_000)
        .ticket
        .unwrap();
    p.breakers.lock().unwrap().on_retry_after(
        &provider_key("provider"),
        CredentialClass::Local,
        t,
        15_000,
        900_000,
    );
    late.finish(503, Some(300_000));
    clock(&p, 315_000);
    assert!(p.admit_provider("provider", url).is_none());
    clock(&p, 915_000);
    assert!(p.admit_provider("provider", url).is_some());
}
#[tokio::test]
async fn cache_and_preflight_bypass_admission_when_open() {
    let s = server(200, None).await;
    let p = proxy();
    let n = AtomicUsize::new(0);
    assert_eq!(
        p.forward_buffered(&s.uri(), &body(), &opts(Some("cache")))
            .await
            .status,
        200
    );
    trip(&p, &s.uri());
    let cached = p
        .forward_buffered_observed(&s.uri(), &body(), &opts(Some("cache")), || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert_eq!(cached.status, 200);
    let request = opts(Some("cache"));
    let different_body = serde_json::json!({"model":"different","messages":[]});
    let rejected = p
        .forward_buffered_observed(&s.uri(), &different_body, &request, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert_eq!(rejected.status, 409);
    assert_eq!(rejected.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(n.load(Ordering::SeqCst), 0);
    assert_eq!(s.received_requests().await.unwrap().len(), 1);
}

async fn blocked_body_server() -> (
    String,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<()>,
) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post({
            let entered = entered.clone();
            let release = release.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    let stream = futures_util::stream::once(async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok::<_, std::io::Error>(Bytes::from_static(b"{}"))
                    });
                    axum::response::Response::builder()
                        .status(200)
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, entered, release, server)
}
#[tokio::test]
async fn body_cancellation_reopens_probe_without_changing_uncertain_execution() {
    let (url, entered, _release, server) = blocked_body_server().await;
    let p = proxy();
    trip(&p, &url);
    clock(&p, 15_000);
    let n = AtomicUsize::new(0);
    let mut future = Box::pin(send(&p, &url, &n));
    tokio::select! { _ = entered.notified() => {}, _ = &mut future => panic!("body completed") }
    assert!(p.admit_provider("provider", &url).is_none());
    p.set_breakers_enabled(false);
    clock(&p, 17_000);
    drop(future);
    p.set_breakers_enabled(true);
    clock(&p, 31_999);
    assert!(p.admit_provider("provider", &url).is_none());
    clock(&p, 32_000);
    p.admit_provider("provider", &url)
        .unwrap()
        .finish(200, None);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn inflight_toggle_both_directions_settles_only_captured_ticket() {
    for initially_enabled in [false, true] {
        let (url, entered, release, server) = blocked_body_server().await;
        let p = proxy();
        trip(&p, &url);
        clock(&p, 15_000);
        p.set_breakers_enabled(initially_enabled);
        let n = AtomicUsize::new(0);
        let mut future = Box::pin(send(&p, &url, &n));
        tokio::select! { _ = entered.notified() => {}, _ = &mut future => panic!("body completed") }
        p.set_breakers_enabled(!initially_enabled);
        release.notify_one();
        assert_eq!(future.await.status, 200);
        p.set_breakers_enabled(true);
        let mut book = p.breakers.lock().unwrap();
        let admission = book
            .admit(&provider_key("provider"), CredentialClass::Local, 15_000)
            .admission;
        assert_eq!(
            admission,
            if initially_enabled {
                Admission::Allowed
            } else {
                Admission::Probe
            }
        );
        assert_eq!(n.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
#[tokio::test]
async fn singleflight_waiter_has_no_additional_claim_or_dispatch() {
    let (url, entered, release, server) = blocked_body_server().await;
    let p = proxy();
    trip(&p, &url);
    clock(&p, 15_000);
    let n = AtomicUsize::new(0);
    let request_body = body();
    let request_opts = opts(Some("same"));
    let mut leader =
        Box::pin(
            p.forward_buffered_observed(&url, &request_body, &request_opts, || async {
                n.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            }),
        );
    tokio::select! { _ = entered.notified() => {}, _ = &mut leader => panic!("body completed") }
    let mut waiter =
        Box::pin(
            p.forward_buffered_observed(&url, &request_body, &request_opts, || async {
                n.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            }),
        );
    assert!(futures_util::poll!(&mut waiter).is_pending());
    assert_eq!(n.load(Ordering::SeqCst), 1);
    release.notify_one();
    let (leader, waiter) = tokio::join!(leader, waiter);
    assert_eq!(leader.unwrap().status, 200);
    assert_eq!(waiter.unwrap().status, 200);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    server.abort();
}

#[test]
fn poisoned_finish_settles_owned_probe_while_admission_stays_closed() {
    let p = proxy();
    let url = "http://localhost";
    trip(&p, url);
    clock(&p, 15_000);
    let probe = p.admit_provider("provider", url).unwrap();
    let _ = std::panic::catch_unwind(|| {
        let _lock = p.breakers.lock().unwrap();
        panic!("poison");
    });
    probe.finish(200, None);
    assert!(p.admit_provider("provider", url).is_none());
    let mut book = p.breakers.lock().unwrap_err().into_inner();
    assert_eq!(
        book.admit(&provider_key("provider"), CredentialClass::Local, 15_000)
            .admission,
        Admission::Allowed
    );
}
