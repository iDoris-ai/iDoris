#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use futures_util::FutureExt;
use http_body_util::BodyExt;
use tokio::sync::Notify;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn proxy() -> ChatProxy {
    let mut p = ChatProxy::new(reqwest::Client::new());
    clock(&p, 0);
    p.retry_delays = vec![Duration::ZERO; 2];
    p
}
fn clock(p: &ChatProxy, ms: u64) {
    p.test_clock_ms.store(ms, Ordering::SeqCst);
}
fn body() -> Value {
    serde_json::json!({"model":"test","messages":[]})
}
fn opts() -> ForwardOpts<'static> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "stream-test",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
        require_openai_usage: false,
    }
}
fn ready(p: &ChatProxy, key: &BreakerKey, now: u64) -> Admission {
    p.breakers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .readiness(key, CredentialClass::Local, now)
}
fn open(p: &ChatProxy, key: &BreakerKey, until: u64) {
    let mut book = p.breakers.lock().unwrap();
    let ticket = book.admit(key, CredentialClass::Local, 0).ticket.unwrap();
    book.on_retry_after(key, CredentialClass::Local, ticket, 0, until);
}
fn keys(url: &str) -> [BreakerKey; 2] {
    [provider_key("provider"), connection_key(url).unwrap()]
}
fn pair(p: &ChatProxy, url: &str, now: u64, want: Admission) {
    for key in keys(url) {
        assert_eq!(ready(p, &key, now), want, "{key:?}");
    }
}
fn probes(p: &ChatProxy, url: &str) {
    for key in keys(url) {
        open(p, &key, 1000);
    }
    clock(p, 1000);
}
fn buffered(outcome: StreamOutcome, status: u16, execution: ExecutionDisposition) -> Bytes {
    let StreamOutcome::Buffered {
        status: actual,
        execution: disposition,
        body,
        ..
    } = outcome
    else {
        panic!("expected buffered {status}");
    };
    assert_eq!(actual, status);
    assert_eq!(disposition, execution);
    body
}
async fn send(p: &ChatProxy, url: &str) -> StreamOutcome {
    p.forward_stream_path(
        url,
        StreamingUpstreamPath::CHAT_COMPLETIONS,
        &body(),
        "provider",
    )
    .await
}
async fn server(status: u16, header: Option<&str>) -> MockServer {
    let s = MockServer::start().await;
    let mut response = ResponseTemplate::new(status).set_body_string("data: [DONE]\n\n");
    if let Some(header) = header {
        response = response.insert_header("retry-after", header);
    }
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&s)
        .await;
    s
}

struct HeldServer {
    url: String,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HeldServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn held_server(status: u16, headers_first: bool) -> HeldServer {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post({
            let entered = entered.clone();
            let release = release.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    if !headers_first {
                        entered.notify_one();
                        release.notified().await;
                    }
                    let stream = futures_util::stream::once(async move {
                        if headers_first {
                            entered.notify_one();
                            release.notified().await;
                        }
                        Ok::<_, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"))
                    });
                    axum::response::Response::builder()
                        .status(status)
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    HeldServer {
        url,
        entered,
        release,
        task,
    }
}

#[tokio::test]
async fn header_cooldown_is_shared_across_modes_with_other_origin_positive() {
    for status in [429, 503] {
        let s = server(status, Some("300")).await;
        let other = server(200, None).await;
        let p = proxy();
        buffered(
            send(&p, &s.uri()).await,
            status,
            ExecutionDisposition::Executed,
        );
        assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
        let observed = AtomicUsize::new(0);
        let denied = p
            .forward_stream_observed(&s.uri(), &body(), "other-provider", || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            })
            .await
            .unwrap();
        let error = buffered(denied, 503, ExecutionDisposition::NotExecuted);
        assert_eq!(
            serde_json::from_slice::<Value>(&error).unwrap()["error"]["type"],
            "provider_circuit_open"
        );
        let denied = p
            .forward_buffered_observed(&s.uri(), &body(), &opts(), || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            })
            .await
            .unwrap();
        assert_eq!(denied.status, 503);
        assert_eq!(denied.execution, ExecutionDisposition::NotExecuted);
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
        let StreamOutcome::Stream { response, .. } = send(&p, &other.uri()).await else {
            panic!("other origin blocked");
        };
        response.collect().await.unwrap();
        assert_eq!(other.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn public_endpoint_fallback_and_streams_never_retry_or_cache() {
    let s = server(500, None).await;
    let p = proxy();
    for _ in 0..2 {
        buffered(
            p.forward_stream(&s.uri(), &body()).await,
            500,
            ExecutionDisposition::Executed,
        );
    }
    buffered(
        p.forward_stream(&s.uri(), &body()).await,
        503,
        ExecutionDisposition::NotExecuted,
    );
    assert_eq!(s.received_requests().await.unwrap().len(), 2);
    assert_eq!(
        ready(&p, &provider_key(&s.uri()), 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
    assert!(p.cache.lock().unwrap().is_empty());
    assert!(p.flights.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stream_refusal_preserves_other_probe_in_both_orders_without_observation() {
    let s = server(200, None).await;
    for blocked in [0, 1] {
        let p = proxy();
        let gates = keys(&s.uri());
        open(&p, &gates[blocked], 2000);
        open(&p, &gates[1 - blocked], 1000);
        clock(&p, 1000);
        let observed = AtomicUsize::new(0);
        let outcome = p
            .forward_stream_observed(&s.uri(), &body(), "provider", || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            })
            .await
            .unwrap();
        buffered(outcome, 503, ExecutionDisposition::NotExecuted);
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        assert_eq!(ready(&p, &gates[1 - blocked], 1000), Admission::Probe);
    }
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn stream_and_buffered_race_claim_before_observer_with_one_complete_winner() {
    for stream_wins in [true, false] {
        let s = server(200, None).await;
        let url = s.uri();
        let p = proxy();
        probes(&p, &url);
        let entered = Notify::new();
        let release = Notify::new();
        let observed = AtomicUsize::new(0);
        let payload = body();
        let options = opts();
        let observer = || async {
            observed.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            release.notified().await;
            Ok::<(), ()>(())
        };
        let mut winner = Box::pin(async {
            if stream_wins {
                let outcome = p
                    .forward_stream_observed(&url, &payload, "provider", observer)
                    .await
                    .unwrap();
                let StreamOutcome::Stream { response, .. } = outcome else {
                    panic!("winner denied");
                };
                response.collect().await.unwrap();
            } else {
                assert_eq!(
                    p.forward_buffered_observed(&url, &payload, &options, observer)
                        .await
                        .unwrap()
                        .status,
                    200
                );
            }
        });
        tokio::select! { _ = entered.notified() => {}, _ = &mut winner => panic!("winner returned before release") }
        pair(&p, &url, 1000, Admission::Rejected { retry_after_ms: 0 });
        let loser_observer = || async {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        };
        if stream_wins {
            let loser = p
                .forward_buffered_observed(&url, &payload, &options, loser_observer)
                .await
                .unwrap();
            assert_eq!(loser.status, 503);
            assert_eq!(loser.execution, ExecutionDisposition::NotExecuted);
        } else {
            buffered(
                p.forward_stream_observed(&url, &payload, "provider", loser_observer)
                    .await
                    .unwrap(),
                503,
                ExecutionDisposition::NotExecuted,
            );
        }
        assert_eq!(observed.load(Ordering::SeqCst), 1);
        assert!(s.received_requests().await.unwrap().is_empty());
        release.notify_one();
        winner.await;
        pair(&p, &url, 1000, Admission::Allowed);
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn observer_error_cancel_and_unwind_preserve_normal_and_drop_time_probe_semantics() {
    for probing in [false, true] {
        for mode in [0, 1, 2] {
            let s = server(200, None).await;
            let p = proxy();
            let url = s.uri();
            if probing {
                probes(&p, &url);
            }
            if mode == 0 {
                let result = p
                    .forward_stream_observed(&url, &body(), "provider", || async {
                        clock(&p, 7000);
                        Err::<(), ()>(())
                    })
                    .await;
                assert!(result.is_err());
            } else if mode == 1 {
                let entered = Notify::new();
                let payload = body();
                let mut future =
                    Box::pin(
                        p.forward_stream_observed(&url, &payload, "provider", || async {
                            entered.notify_one();
                            std::future::pending::<Result<(), ()>>().await
                        }),
                    );
                tokio::select! { _ = entered.notified() => {}, _ = &mut future => panic!("observer returned") }
                clock(&p, 7000);
                drop(future);
            } else {
                let result = std::panic::AssertUnwindSafe(p.forward_stream_observed(
                    &url,
                    &body(),
                    "provider",
                    || async {
                        clock(&p, 7000);
                        panic!("observer unwind");
                        #[allow(unreachable_code)]
                        Ok::<(), ()>(())
                    },
                ))
                .catch_unwind()
                .await;
                assert!(result.is_err());
            }
            pair(
                &p,
                &url,
                7000,
                if probing {
                    Admission::Rejected {
                        retry_after_ms: 15_000,
                    }
                } else {
                    Admission::Allowed
                },
            );
            if probing {
                pair(&p, &url, 22_000, Admission::Probe);
            }
            assert!(s.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn cancellation_while_headers_pending_reopens_probes_at_drop_clock() {
    for probing in [false, true] {
        let s = held_server(200, false).await;
        let p = proxy();
        if probing {
            probes(&p, &s.url);
        }
        let mut future = Box::pin(send(&p, &s.url));
        tokio::select! { _ = s.entered.notified() => {}, _ = &mut future => panic!("headers unexpectedly arrived") }
        clock(&p, 9000);
        drop(future);
        pair(
            &p,
            &s.url,
            9000,
            if probing {
                Admission::Rejected {
                    retry_after_ms: 15_000,
                }
            } else {
                Admission::Allowed
            },
        );
        assert_eq!(p.permits.available_permits(), 32);
    }
}

#[tokio::test]
async fn header_timeout_and_connect_fault_settle_once_without_stream_retries() {
    let s = held_server(200, false).await;
    let mut p = proxy();
    p.header_timeout = Duration::from_millis(100);
    let mut future = Box::pin(send(&p, &s.url));
    tokio::select! { _ = s.entered.notified() => {}, _ = &mut future => panic!("request did not reach upstream") }
    buffered(future.await, 504, ExecutionDisposition::Uncertain);
    assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
    // A second owned ordinary fault reaches the Local threshold: first counted once.
    p.claim_send("provider", &s.url).unwrap().finish(500, None);
    assert_eq!(
        ready(&p, &provider_key("provider"), 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let p = proxy();
    buffered(send(&p, &url).await, 502, ExecutionDisposition::NotExecuted);
    assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
    assert_eq!(
        ready(&p, &connection_key(&url).unwrap(), 0),
        Admission::Allowed
    );
    // The connect fault is one ordinary provider failure and never populates cache.
    p.claim_send("provider", &url).unwrap().finish(500, None);
    assert_eq!(
        ready(&p, &provider_key("provider"), 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
    assert!(p.cache.lock().unwrap().is_empty());
}

#[tokio::test]
async fn non_success_body_timeout_accounts_initial_headers_only() {
    for status in [400, 503] {
        let s = held_server(status, true).await;
        let mut p = proxy();
        p.body_timeout = Duration::from_millis(100);
        let mut future = Box::pin(send(&p, &s.url));
        tokio::select! { _ = s.entered.notified() => {}, _ = &mut future => panic!("body was not pending") }
        // Settlement precedes the blocked non2xx body read, not its eventual timeout.
        assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
        buffered(future.await, 504, ExecutionDisposition::Uncertain);
        assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
        p.claim_send("provider", &s.url).unwrap().finish(500, None);
        assert_eq!(
            ready(&p, &provider_key("provider"), 0),
            if status == 503 {
                Admission::Rejected {
                    retry_after_ms: 15_000,
                }
            } else {
                Admission::Allowed
            }
        );
    }
}

#[tokio::test]
async fn successful_headers_settle_probes_before_body_drop_idle_or_eof() {
    for mode in [0, 1, 2] {
        let s = held_server(200, true).await;
        let mut p = proxy();
        p.stream_idle_timeout = Duration::from_millis(100);
        probes(&p, &s.url);
        let StreamOutcome::Stream { response, .. } = send(&p, &s.url).await else {
            panic!("expected SSE");
        };
        s.entered.notified().await;
        pair(&p, &s.url, 1000, Admission::Allowed);
        match mode {
            0 => drop(response),
            1 => assert!(response.collect().await.is_err()),
            _ => {
                s.release.notify_one();
                response.collect().await.unwrap();
            }
        }
        pair(&p, &s.url, 1000, Admission::Allowed);
        // A following fault still needs another failure: no hidden post-header failure.
        p.claim_send("provider", &s.url).unwrap().finish(500, None);
        assert_eq!(
            ready(&p, &provider_key("provider"), 1000),
            Admission::Allowed
        );
    }
}

#[tokio::test]
async fn flag_changes_settle_only_captured_stream_ownership() {
    let s = server(200, None).await;
    for initially_enabled in [true, false] {
        let p = proxy();
        probes(&p, &s.uri());
        p.set_breakers_enabled(initially_enabled);
        let outcome = p
            .forward_stream_observed(&s.uri(), &body(), "provider", || async {
                p.set_breakers_enabled(!initially_enabled);
                Ok::<(), ()>(())
            })
            .await
            .unwrap();
        let StreamOutcome::Stream { response, .. } = outcome else {
            panic!("expected success");
        };
        response.collect().await.unwrap();
        pair(
            &p,
            &s.uri(),
            1000,
            if initially_enabled {
                Admission::Allowed
            } else {
                Admission::Probe
            },
        );
    }
}

#[tokio::test]
async fn poison_after_capture_settles_owned_stream_probe_but_future_admission_stays_closed() {
    for cancel in [false, true] {
        let s = server(200, None).await;
        let p = proxy();
        probes(&p, &s.uri());
        let outcome = p
            .forward_stream_observed(&s.uri(), &body(), "provider", || async {
                let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _book = p.breakers.lock().unwrap();
                    panic!("poison");
                }));
                assert!(poisoned.is_err());
                if cancel {
                    clock(&p, 6000);
                    Err(())
                } else {
                    Ok(())
                }
            })
            .await;
        if cancel {
            assert!(outcome.is_err());
            pair(
                &p,
                &s.uri(),
                6000,
                Admission::Rejected {
                    retry_after_ms: 15_000,
                },
            );
        } else {
            let StreamOutcome::Stream { response, .. } = outcome.unwrap() else {
                panic!("owned success lost");
            };
            response.collect().await.unwrap();
            pair(&p, &s.uri(), 1000, Admission::Allowed);
        }
        assert!(p.breakers.is_poisoned());
        let observed = AtomicUsize::new(0);
        let denied = p
            .forward_stream_observed(&s.uri(), &body(), "provider", || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            })
            .await
            .unwrap();
        buffered(denied, 503, ExecutionDisposition::NotExecuted);
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        assert_eq!(
            s.received_requests().await.unwrap().len(),
            usize::from(!cancel)
        );
    }
}

#[tokio::test]
async fn stream_headers_keep_strict_parser_status_applicability_and_network_cap() {
    for (status, header, cooldown) in [
        (429, "184467440737095516160000", 300_000),
        (503, "1", 1000),
        (429, "+1", 0),
        (429, "-1", 0),
        (429, "Wed, 21 Oct 2015 07:28:00 GMT", 0),
        (500, "300", 0),
        (400, "300", 0),
        (200, "300", 0),
    ] {
        let s = server(status, Some(header)).await;
        let p = proxy();
        let outcome = send(&p, &s.uri()).await;
        if status == 200 {
            let StreamOutcome::Stream { response, .. } = outcome else {
                panic!("expected stream");
            };
            response.collect().await.unwrap();
        } else {
            buffered(outcome, status, ExecutionDisposition::Executed);
        }
        assert_eq!(
            ready(&p, &connection_key(&s.uri()).unwrap(), 0),
            if cooldown > 0 {
                Admission::Rejected {
                    retry_after_ms: cooldown,
                }
            } else {
                Admission::Allowed
            }
        );
        assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
        if cooldown > 0 {
            assert_eq!(
                ready(&p, &connection_key(&s.uri()).unwrap(), cooldown - 1),
                Admission::Rejected { retry_after_ms: 1 }
            );
            assert_eq!(
                ready(&p, &connection_key(&s.uri()).unwrap(), cooldown),
                Admission::Probe
            );
        }
        assert_eq!(s.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn transport_loss_before_headers_is_uncertain_and_settles_once() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        count.fetch_add(1, Ordering::SeqCst);
        let mut bytes = [0u8; 4096];
        assert!(socket.read(&mut bytes).await.unwrap() > 0);
        // Dropping after reading the POST cannot prove it was not executed.
    });
    let p = proxy();
    buffered(send(&p, &url).await, 502, ExecutionDisposition::Uncertain);
    server.await.unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(ready(&p, &provider_key("provider"), 0), Admission::Allowed);
    p.claim_send("provider", &url).unwrap().finish(500, None);
    assert_eq!(
        ready(&p, &provider_key("provider"), 0),
        Admission::Rejected {
            retry_after_ms: 15_000
        }
    );
}

#[tokio::test]
async fn truncated_body_after_successful_headers_does_not_fail_provider() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0u8; 4096];
        assert!(socket.read(&mut bytes).await.unwrap() > 0);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 500\r\nConnection: close\r\n\r\ndata: partial\n\n").await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let p = proxy();
    probes(&p, &url);
    let StreamOutcome::Stream { response, .. } = send(&p, &url).await else {
        panic!("initial headers lost");
    };
    assert!(response.collect().await.is_err());
    server.await.unwrap();
    pair(&p, &url, 1000, Admission::Allowed);
    p.claim_send("provider", &url).unwrap().finish(500, None);
    assert_eq!(
        ready(&p, &provider_key("provider"), 1000),
        Admission::Allowed
    );
}

#[tokio::test]
async fn short_stream_header_never_shortens_failed_pair_probe_floors() {
    let s = server(503, Some("1")).await;
    let p = proxy();
    probes(&p, &s.uri());
    buffered(
        send(&p, &s.uri()).await,
        503,
        ExecutionDisposition::Executed,
    );
    pair(
        &p,
        &s.uri(),
        1000,
        Admission::Rejected {
            retry_after_ms: 15_000,
        },
    );
    pair(
        &p,
        &s.uri(),
        15_999,
        Admission::Rejected { retry_after_ms: 1 },
    );
    pair(&p, &s.uri(), 16_000, Admission::Probe);
}
