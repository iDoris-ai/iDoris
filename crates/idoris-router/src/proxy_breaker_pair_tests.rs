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

fn ready(p: &ChatProxy, key: &BreakerKey, class: CredentialClass, now: u64) -> Admission {
    p.breakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .readiness(key, class, now)
}
fn open(p: &ChatProxy, key: &BreakerKey, class: CredentialClass, now: u64, ms: u64) {
    let mut book = p.breakers.lock().unwrap();
    let t = book.admit(key, class, now).ticket.unwrap();
    book.on_retry_after(key, class, t, now, ms);
}
fn both_open(p: &ChatProxy, url: &str) {
    for key in [provider_key("provider"), connection_key(url).unwrap()] {
        open(p, &key, credential_class(url), 0, 15_000);
    }
}
fn assert_pair(p: &ChatProxy, url: &str, now: u64, want: Admission) {
    for key in [provider_key("provider"), connection_key(url).unwrap()] {
        assert_eq!(ready(p, &key, credential_class(url), now), want);
    }
}

#[test]
fn typed_canonical_http_origins_and_class_namespace_have_positive_controls() {
    let origin = connection_key("http://LOCALHOST/a?q=1").unwrap();
    for url in [
        "http://localhost:80/b",
        "http://user:pass@LOCALHOST:80/x?y=2#f",
    ] {
        assert_eq!(connection_key(url).unwrap(), origin);
    }
    assert_eq!(
        connection_key("https://EXAMPLE.com"),
        connection_key("https://example.com:443/a")
    );
    assert_eq!(
        connection_key("http://[0:0:0:0:0:0:0:1]"),
        connection_key("http://[::1]:80/a")
    );
    for url in [
        "https://localhost",
        "http://localhost:81",
        "http://127.0.0.1",
    ] {
        assert_ne!(connection_key(url).unwrap(), origin);
    }
    for url in [
        "not a URL",
        "file:///tmp/a",
        "data:text/plain,x",
        "ftp://localhost/a",
    ] {
        assert!(connection_key(url).is_none());
    }
    let p = proxy();
    open(&p, &origin, CredentialClass::Local, 0, 1000);
    let collision = "connection:http://localhost:80";
    assert_ne!(provider_key(collision), origin);
    p.claim_send(collision, "http://localhost:81")
        .unwrap()
        .finish(200, None);
    assert_eq!(
        ready(&p, &origin, CredentialClass::ApiKey, 0),
        Admission::Allowed
    );
    assert!(p.claim_send("other", "http://localhost/b").is_none());
    let invalid = p.claim_send("invalid", "not a URL").unwrap();
    assert_eq!(invalid.gates.len(), 1);
    invalid.finish(200, None);
}

#[test]
fn either_refusal_order_leaves_other_expired_probe_unclaimed() {
    let url = "http://localhost";
    let keys = [provider_key("provider"), connection_key(url).unwrap()];
    for blocked in [0, 1] {
        let p = proxy();
        open(&p, &keys[blocked], CredentialClass::Local, 0, 20_000);
        open(&p, &keys[1 - blocked], CredentialClass::Local, 0, 10_000);
        clock(&p, 10_000);
        assert!(p.claim_send("provider", url).is_none());
        assert_eq!(
            ready(&p, &keys[1 - blocked], CredentialClass::Local, 10_000),
            Admission::Probe
        );
        clock(&p, 20_000);
        let claim = p.claim_send("provider", url).unwrap();
        assert!(claim.gates.iter().all(|gate| gate.probe));
        claim.finish(200, None);
        assert_pair(&p, url, 20_000, Admission::Allowed);
    }
}

#[tokio::test]
async fn parallel_expired_pair_has_one_observed_winner_and_no_network_on_refusal() {
    let s = server(200, None).await;
    let p = proxy();
    both_open(&p, &s.uri());
    clock(&p, 15_000);
    let entered = tokio::sync::Notify::new();
    let release = tokio::sync::Notify::new();
    let n = AtomicUsize::new(0);
    let b = body();
    let o = opts(None);
    let url = s.uri();
    let mut winner = Box::pin(p.forward_buffered_observed(&url, &b, &o, || async {
        n.fetch_add(1, Ordering::SeqCst);
        entered.notify_one();
        release.notified().await;
        Ok::<(), ()>(())
    }));
    tokio::select! { _ = entered.notified() => {}, _ = &mut winner => panic!("early completion") }
    let loser = p
        .forward_buffered_observed(&url, &b, &o, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert_eq!(loser.status, 503);
    assert_eq!(loser.execution, ExecutionDisposition::NotExecuted);
    assert!(String::from_utf8_lossy(&loser.body).contains("provider_circuit_open"));
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert!(s.received_requests().await.unwrap().is_empty());
    assert_pair(&p, &url, 15_000, Admission::Rejected { retry_after_ms: 0 });
    release.notify_one();
    assert_eq!(winner.await.unwrap().status, 200);
    assert_eq!(s.received_requests().await.unwrap().len(), 1);
    assert_pair(&p, &url, 15_000, Admission::Allowed);
}

#[tokio::test]
async fn header_transition_shares_origin_but_keeps_same_provider_other_origin_usable() {
    let a = server(429, Some("300")).await;
    let b = server(200, None).await;
    let p = proxy();
    let n = AtomicUsize::new(0);
    let response = p
        .forward_buffered_observed(&a.uri(), &body(), &opts(None), || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert_eq!(response.status, 429);
    assert_eq!(
        ready(&p, &provider_key("provider"), CredentialClass::Local, 0),
        Admission::Allowed
    );
    let mut other = opts(None);
    other.provider_id = "other";
    let denied = p
        .forward_buffered_observed(&a.uri(), &body(), &other, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert_eq!(denied.status, 503);
    assert_eq!(denied.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(
        p.forward_buffered(&b.uri(), &body(), &opts(None))
            .await
            .status,
        200
    );
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert_eq!(a.received_requests().await.unwrap().len(), 1);
    assert_eq!(b.received_requests().await.unwrap().len(), 1);
    // The first 429 counted once: another failure, without an intervening success,
    // still reaches the local provider threshold even at a different origin.
    let q = proxy();
    q.claim_send("provider", &a.uri())
        .unwrap()
        .finish(429, Some(300_000));
    q.claim_send("provider", &b.uri())
        .unwrap()
        .finish(500, None);
    assert_eq!(
        ready(&q, &provider_key("provider"), CredentialClass::Local, 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
}

#[test]
fn normal_connection_faults_never_accumulate_but_provider_thresholds_do() {
    for (url, threshold, floor) in [
        ("http://localhost", 2, 15_000),
        ("https://api.invalid", 5, 30_000),
    ] {
        let p = proxy();
        for i in 0..10 {
            p.claim_send(&format!("p{i}"), url)
                .unwrap()
                .finish(500, Some(300_000));
        }
        assert_eq!(
            ready(&p, &connection_key(url).unwrap(), credential_class(url), 0),
            Admission::Allowed
        );
        for _ in 0..threshold {
            p.claim_send("provider", url).unwrap().finish(500, None);
        }
        assert_eq!(
            ready(
                &p,
                &provider_key("provider"),
                credential_class(url),
                floor - 1
            ),
            Admission::Rejected { retry_after_ms: 1 }
        );
        assert_eq!(
            ready(&p, &provider_key("provider"), credential_class(url), floor),
            Admission::Probe
        );
    }
}

#[test]
fn pair_cohort_headers_extend_without_shortening_and_stale_finish_cannot_reopen() {
    let p = proxy();
    let url = "http://localhost";
    let first = p.claim_send("provider", url).unwrap();
    let longer = p.claim_send("provider", url).unwrap();
    let shorter = p.claim_send("provider", url).unwrap();
    let stale = p.claim_send("provider", url).unwrap();
    first.finish(429, Some(1000));
    longer.finish(503, Some(40_000));
    shorter.finish(503, Some(10));
    assert_eq!(
        ready(
            &p,
            &connection_key(url).unwrap(),
            CredentialClass::Local,
            39_999
        ),
        Admission::Rejected { retry_after_ms: 1 }
    );
    clock(&p, 40_000);
    p.claim_send("provider", url).unwrap().finish(200, None);
    stale.finish(503, Some(300_000));
    assert_pair(&p, url, 40_000, Admission::Allowed);
}

#[test]
fn short_headers_never_shorten_provider_or_failed_connection_probe_floors() {
    let p = proxy();
    let url = "http://localhost";
    let claim = p.claim_send("provider", url).unwrap();
    open(
        &p,
        &provider_key("provider"),
        CredentialClass::Local,
        0,
        900_000,
    );
    claim.finish(503, Some(1000));
    assert_eq!(
        ready(
            &p,
            &provider_key("provider"),
            CredentialClass::Local,
            899_999
        ),
        Admission::Rejected { retry_after_ms: 1 }
    );
    assert_eq!(
        ready(
            &p,
            &connection_key(url).unwrap(),
            CredentialClass::Local,
            999
        ),
        Admission::Rejected { retry_after_ms: 1 }
    );
    let c = proxy();
    let late = c.claim_send("provider", url).unwrap();
    open(
        &c,
        &connection_key(url).unwrap(),
        CredentialClass::Local,
        0,
        900_000,
    );
    late.finish(503, Some(1000));
    assert_eq!(
        ready(
            &c,
            &connection_key(url).unwrap(),
            CredentialClass::Local,
            899_999
        ),
        Admission::Rejected { retry_after_ms: 1 }
    );
    let q = proxy();
    both_open(&q, url);
    clock(&q, 15_000);
    q.claim_send("provider", url).unwrap().finish(503, Some(1));
    assert_pair(&q, url, 29_999, Admission::Rejected { retry_after_ms: 1 });
    clock(&q, 30_000);
    q.claim_send("provider", url).unwrap().finish(200, None);
    assert_pair(&q, url, 30_000, Admission::Allowed);
}

#[tokio::test]
async fn captured_keys_and_classes_precede_observer_and_survive_mutable_inputs() {
    let p = proxy();
    let url = "http://LOCALHOST:80/a?q=1";
    both_open(&p, url);
    clock(&p, 15_000);
    let mut original_provider = "provider".to_string();
    let mut original_url = url.to_string();
    let claim = p.claim_send(&original_provider, &original_url).unwrap();
    original_provider.replace_range(.., "changed");
    original_url.replace_range(.., "https://api.invalid");
    assert_eq!(claim.gates.len(), 2);
    assert_eq!(claim.gates[0].key, provider_key("provider"));
    assert_eq!(claim.gates[1].key, connection_key(url).unwrap());
    assert!(
        claim
            .gates
            .iter()
            .all(|g| g.class == CredentialClass::Local && g.probe)
    );
    p.set_breakers_enabled(false);
    claim.finish(200, None);
    assert_pair(&p, url, 15_000, Admission::Allowed);
    // A separate dialed class and provider remain independent.
    assert_eq!(
        ready(
            &p,
            &provider_key("provider"),
            CredentialClass::ApiKey,
            15_000
        ),
        Admission::Allowed
    );
    assert_eq!(
        ready(&p, &provider_key("changed"), CredentialClass::Local, 15_000),
        Admission::Allowed
    );
}

#[tokio::test]
async fn observer_error_drop_panic_and_body_cancel_reopen_both_from_drop_clock() {
    for mode in [0, 1, 2] {
        let s = server(200, None).await;
        let p = proxy();
        both_open(&p, &s.uri());
        clock(&p, 15_000);
        if mode == 0 {
            let result = p
                .forward_buffered_observed(&s.uri(), &body(), &opts(None), || async {
                    clock(&p, 20_000);
                    Err::<(), ()>(())
                })
                .await;
            assert!(result.is_err());
        } else if mode == 1 {
            let entered = tokio::sync::Notify::new();
            let b = body();
            let o = opts(None);
            let url = s.uri();
            let mut f = Box::pin(p.forward_buffered_observed(&url, &b, &o, || async {
                entered.notify_one();
                std::future::pending::<Result<(), ()>>().await
            }));
            tokio::select! { _ = entered.notified() => {}, _ = &mut f => panic!("early") }
            assert_pair(&p, &url, 15_000, Admission::Rejected { retry_after_ms: 0 });
            clock(&p, 20_000);
            drop(f);
        } else {
            use futures_util::FutureExt;
            let result = std::panic::AssertUnwindSafe(p.forward_buffered_observed(
                &s.uri(),
                &body(),
                &opts(None),
                || async {
                    clock(&p, 20_000);
                    panic!("observer panic");
                    #[allow(unreachable_code)]
                    Ok::<(), ()>(())
                },
            ))
            .catch_unwind()
            .await;
            assert!(result.is_err());
        }
        assert_pair(
            &p,
            &s.uri(),
            34_999,
            Admission::Rejected { retry_after_ms: 1 },
        );
        clock(&p, 35_000);
        p.claim_send("provider", &s.uri())
            .unwrap()
            .finish(200, None);
        assert_pair(&p, &s.uri(), 35_000, Admission::Allowed);
        assert!(s.received_requests().await.unwrap().is_empty());
    }
    let (url, entered, _release, task) = blocked_body_server().await;
    let p = proxy();
    both_open(&p, &url);
    clock(&p, 15_000);
    let b = body();
    let o = opts(None);
    let mut f = Box::pin(p.forward_buffered(&url, &b, &o));
    tokio::select! { _ = entered.notified() => {}, _ = &mut f => panic!("early") }
    assert_pair(&p, &url, 15_000, Admission::Rejected { retry_after_ms: 0 });
    clock(&p, 22_000);
    drop(f);
    assert_pair(&p, &url, 36_999, Admission::Rejected { retry_after_ms: 1 });
    clock(&p, 37_000);
    p.claim_send("provider", &url).unwrap().finish(200, None);
    task.abort();
}

#[test]
fn normal_drop_ignored_headers_toggles_and_poison_keep_owned_pair_semantics() {
    let url = "http://localhost";
    let p = proxy();
    for _ in 0..10 {
        drop(p.claim_send("provider", url).unwrap());
    }
    for status in [200, 400, 408, 500, 502, 504] {
        let q = proxy();
        q.claim_send("provider", url)
            .unwrap()
            .finish(status, Some(300_000));
        assert_pair(&q, url, 0, Admission::Allowed);
    }
    p.set_breakers_enabled(false);
    let empty = p.claim_send("provider", url).unwrap();
    assert!(empty.gates.is_empty());
    p.set_breakers_enabled(true);
    empty.finish(503, Some(300_000));
    assert_pair(&p, url, 0, Admission::Allowed);
    for settle in [false, true] {
        let q = proxy();
        both_open(&q, url);
        clock(&q, 15_000);
        let claim = q.claim_send("provider", url).unwrap();
        q.set_breakers_enabled(false);
        let _ = std::panic::catch_unwind(|| {
            let _lock = q.breakers.lock().unwrap();
            panic!("poison");
        });
        clock(&q, 20_000);
        if settle {
            claim.finish(200, None);
        } else {
            drop(claim);
        }
        q.set_breakers_enabled(true);
        assert!(q.claim_send("provider", url).is_none());
        assert!(q.breakers.is_poisoned());
        assert_pair(
            &q,
            url,
            34_999,
            if settle {
                Admission::Allowed
            } else {
                Admission::Rejected { retry_after_ms: 1 }
            },
        );
    }
}

#[tokio::test]
async fn retries_hold_one_pair_and_count_one_final_transport_failure() {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    drop(socket);
    let mut p = proxy();
    p.retry_delays = vec![Duration::ZERO, Duration::ZERO];
    both_open(&p, &url);
    clock(&p, 15_000);
    let n = AtomicUsize::new(0);
    let b = body();
    let o = opts(None);
    let entered = tokio::sync::Notify::new();
    let release = tokio::sync::Notify::new();
    let mut future = Box::pin(p.forward_buffered_observed(&url, &b, &o, || async {
        n.fetch_add(1, Ordering::SeqCst);
        entered.notify_one();
        release.notified().await;
        Ok::<(), ()>(())
    }));
    tokio::select! { _ = entered.notified() => {}, _ = &mut future => panic!("early") }
    assert_pair(&p, &url, 15_000, Admission::Rejected { retry_after_ms: 0 });
    release.notify_one();
    let outcome = future.await.unwrap();
    assert_eq!(outcome.status, 502);
    assert_eq!(outcome.retries, 2);
    assert_eq!(outcome.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert_pair(&p, &url, 29_999, Admission::Rejected { retry_after_ms: 1 });
    clock(&p, 30_000);
    p.claim_send("provider", &url).unwrap().finish(200, None);
    // Three actual connection attempts produce one ordinary failure, not three.
    let q = ChatProxy {
        retry_delays: vec![Duration::ZERO, Duration::ZERO],
        ..proxy()
    };
    let outcome = q.forward_buffered(&url, &b, &o).await;
    assert_eq!(outcome.retries, 2);
    q.claim_send("provider", &url).unwrap().finish(500, None);
    assert_eq!(
        ready(&q, &provider_key("provider"), CredentialClass::Local, 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    assert_eq!(
        ready(
            &q,
            &connection_key(&url).unwrap(),
            CredentialClass::Local,
            0
        ),
        Admission::Allowed
    );
}

#[tokio::test]
async fn body_timeout_after_send_counts_provider_fault_once_and_releases_pair() {
    let (url, entered, _release, task) = blocked_body_server().await;
    let mut p = proxy();
    p.body_timeout = Duration::from_millis(30);
    let b = body();
    let o = opts(None);
    let mut f = Box::pin(p.forward_buffered(&url, &b, &o));
    tokio::select! { _ = entered.notified() => {}, _ = &mut f => panic!("early") }
    let outcome = f.await;
    assert_eq!(outcome.status, 504);
    assert_eq!(outcome.execution, ExecutionDisposition::Uncertain);
    assert_eq!(
        ready(
            &p,
            &connection_key(&url).unwrap(),
            CredentialClass::Local,
            0
        ),
        Admission::Allowed
    );
    p.claim_send("provider", &url).unwrap().finish(500, None);
    assert_eq!(
        ready(&p, &provider_key("provider"), CredentialClass::Local, 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    task.abort();
}

#[tokio::test]
async fn poisoned_admission_has_no_observer_or_network_but_disabled_capture_is_empty() {
    let s = server(200, None).await;
    let p = proxy();
    let _ = std::panic::catch_unwind(|| {
        let _lock = p.breakers.lock().unwrap();
        panic!("poison");
    });
    let n = AtomicUsize::new(0);
    let denied = p
        .forward_buffered_observed(&s.uri(), &body(), &opts(None), || async {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
    assert_eq!(denied.status, 503);
    assert_eq!(denied.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(n.load(Ordering::SeqCst), 0);
    assert!(s.received_requests().await.unwrap().is_empty());
    p.set_breakers_enabled(false);
    assert!(p.claim_send("provider", &s.uri()).unwrap().gates.is_empty());
    assert_eq!(
        p.forward_buffered(&s.uri(), &body(), &opts(None))
            .await
            .status,
        200
    );
    assert_eq!(s.received_requests().await.unwrap().len(), 1);
    assert!(p.breakers.is_poisoned());
}

#[test]
fn stale_pair_success_does_not_erase_new_header_window_and_success_resets_both() {
    let p = proxy();
    let url = "http://localhost";
    let success = p.claim_send("provider", url).unwrap();
    p.claim_send("provider", url)
        .unwrap()
        .finish(429, Some(300_000));
    success.finish(200, None);
    assert_eq!(
        ready(
            &p,
            &connection_key(url).unwrap(),
            CredentialClass::Local,
            299_999
        ),
        Admission::Rejected { retry_after_ms: 1 }
    );
    clock(&p, 300_000);
    p.claim_send("provider", url).unwrap().finish(200, None);
    assert_pair(&p, url, 300_000, Admission::Allowed);
    p.claim_send("provider", url).unwrap().finish(500, None);
    assert!(p.claim_send("provider", url).is_some());
}

#[tokio::test]
async fn invalid_url_keeps_provider_only_error_path_without_null_connection_key() {
    let p = proxy();
    let response = p.forward_buffered("not a url", &body(), &opts(None)).await;
    assert_eq!(response.status, 502);
    assert_eq!(response.execution, ExecutionDisposition::Uncertain);
    assert_eq!(
        p.claim_send("other", "file:///tmp/a").unwrap().gates.len(),
        1
    );
    assert!(p.claim_send("other", "http://localhost").is_some());
}

#[test]
fn concurrent_threads_at_same_expiry_claim_exactly_one_complete_pair() {
    let p = proxy();
    let url = "http://localhost";
    both_open(&p, url);
    clock(&p, 15_000);
    let start = std::sync::Barrier::new(8);
    let claimed = std::sync::Barrier::new(8);
    let winners = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                start.wait();
                let claim = p.claim_send("provider", url);
                if let Some(ref c) = claim {
                    assert_eq!(c.gates.len(), 2);
                    assert!(c.gates.iter().all(|g| g.probe));
                    winners.fetch_add(1, Ordering::SeqCst);
                }
                // Keep the winning pair in flight until every admission ran.
                claimed.wait();
                if let Some(c) = claim {
                    c.finish(200, None);
                }
            });
        }
    });
    assert_eq!(winners.load(Ordering::SeqCst), 1);
    assert_pair(&p, url, 15_000, Admission::Allowed);
    p.claim_send("provider", url).unwrap().finish(200, None);
}
