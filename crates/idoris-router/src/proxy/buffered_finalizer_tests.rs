#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const BARRIER_TIMEOUT: Duration = Duration::from_secs(2);

fn body() -> Value {
    serde_json::json!({"model":"m","messages":[{"role":"user","content":"hello"}]})
}

fn options(record_id: &str) -> ForwardOpts<'_> {
    ForwardOpts {
        request_id: Some("finalized-request"),
        tenant_id: Some("tenant-a"),
        record_id,
        provider_id: "p",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::LocalOnly,
        require_openai_usage: false,
    }
}

async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
        .expect(1)
        .mount(&server)
        .await;
    server
}

async fn wait_for_barrier(barrier: &tokio::sync::Notify) {
    tokio::time::timeout(BARRIER_TIMEOUT, barrier.notified())
        .await
        .expect("finalizer callback did not reach its barrier in time");
}

#[tokio::test]
async fn failed_finalization_never_publishes_success_or_repeats_the_post() {
    let server = upstream().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&callback_count);
    let first = proxy
        .forward_buffered_path_finalized(
            &server.uri(),
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            &options("first"),
            |_| Ok(()),
            (
                || async { Ok(()) },
                move |outcome| async move {
                    assert_eq!(outcome.status, 200);
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    Err::<(), ()>(())
                },
            ),
        )
        .await;
    assert!(first.is_err());
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
    assert_eq!(proxy.cache_size_for_test(), 0);

    let replay_calls = Arc::clone(&callback_count);
    let replay = tokio::time::timeout(
        BARRIER_TIMEOUT,
        proxy.forward_buffered_path_finalized(
            &server.uri(),
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            &options("retry"),
            |_| Ok(()),
            (
                || async { Ok::<(), ()>(()) },
                move |_| async move {
                    replay_calls.fetch_add(1, Ordering::SeqCst);
                    panic!("retained failure replay invoked completion callback");
                },
            ),
        ),
    )
    .await
    .expect("retained failure replay did not finish in time");
    assert_eq!(replay.status, 503);
    assert!(!replay.cached);
    assert_eq!(replay.execution, ExecutionDisposition::Replay);
    assert_eq!(proxy.cache_size_for_test(), 0);

    let mut changed = body();
    changed["model"] = serde_json::json!("changed");
    let conflict = proxy
        .forward_buffered(&server.uri(), &changed, &options("conflict"))
        .await;
    assert_eq!(conflict.status, 409);
    assert_eq!(proxy.cache_size_for_test(), 0);
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
    server.verify().await;
}

#[tokio::test]
async fn finalization_holds_the_cache_and_waiters_until_it_commits() {
    let server = upstream().await;
    let proxy = Arc::new(ChatProxy::new(reqwest::Client::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let callback_count = Arc::new(AtomicUsize::new(0));
    let leader_proxy = Arc::clone(&proxy);
    let leader_started = Arc::clone(&started);
    let leader_release = Arc::clone(&release);
    let leader_callback_calls = Arc::clone(&callback_count);
    let endpoint = server.uri();
    let mut leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered_path_finalized(
                &endpoint,
                BufferedUpstreamPath::CHAT_COMPLETIONS,
                &body(),
                &options("leader"),
                |_| Ok(()),
                (
                    || async { Ok::<(), ()>(()) },
                    move |outcome| async move {
                        assert_eq!(outcome.status, 200);
                        leader_callback_calls.fetch_add(1, Ordering::SeqCst);
                        leader_started.notify_one();
                        leader_release.notified().await;
                        Ok(())
                    },
                ),
            )
            .await
            .unwrap()
    });

    wait_for_barrier(&started).await;
    assert_eq!(proxy.cache_size_for_test(), 0);
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
    let endpoint = server.uri();
    let payload = body();
    let opts = options("waiter");
    let waiter_calls = Arc::clone(&callback_count);
    let waiter = proxy.forward_buffered_path_finalized(
        &endpoint,
        BufferedUpstreamPath::CHAT_COMPLETIONS,
        &payload,
        &opts,
        |_| Ok(()),
        (
            || async { Ok::<(), ()>(()) },
            move |_| async move {
                waiter_calls.fetch_add(1, Ordering::SeqCst);
                panic!("same-key waiter invoked completion callback");
            },
        ),
    );
    tokio::pin!(waiter);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiter.as_mut())
            .await
            .is_err(),
        "same-key waiter completed while async finalization was still pending"
    );
    assert_eq!(proxy.cache_size_for_test(), 0);

    release.notify_one();
    let leader_outcome = tokio::time::timeout(BARRIER_TIMEOUT, &mut leader)
        .await
        .expect("leader did not finish after finalizer release")
        .unwrap();
    assert_eq!(leader_outcome.status, 200);
    let replay = tokio::time::timeout(BARRIER_TIMEOUT, waiter)
        .await
        .expect("waiter did not receive the finalized result");
    assert_eq!(replay.status, 200);
    assert!(replay.cached);
    assert_eq!(replay.execution, ExecutionDisposition::Replay);
    assert_eq!(replay.origin_record_id.as_deref(), Some("leader"));
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);

    let cache_hit_calls = Arc::clone(&callback_count);
    let cache_hit = tokio::time::timeout(
        BARRIER_TIMEOUT,
        proxy.forward_buffered_path_finalized(
            &server.uri(),
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            &options("cache-hit"),
            |_| Ok(()),
            (
                || async { Ok::<(), ()>(()) },
                move |_| async move {
                    cache_hit_calls.fetch_add(1, Ordering::SeqCst);
                    panic!("successful cache hit invoked completion callback");
                },
            ),
        ),
    )
    .await
    .expect("successful cache hit did not finish in time");
    assert_eq!(cache_hit.status, 200);
    assert!(cache_hit.cached);
    assert_eq!(cache_hit.execution, ExecutionDisposition::Replay);
    assert_eq!(cache_hit.origin_record_id.as_deref(), Some("leader"));
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
    server.verify().await;
}

#[tokio::test]
async fn non_idempotent_calls_also_require_finalization_before_returning_success() {
    let server = upstream().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let mut opts = options("no-id");
    opts.request_id = None;
    let outcome = proxy
        .forward_buffered_path_finalized(
            &server.uri(),
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            &opts,
            |_| Ok(()),
            (|| async { Ok(()) }, |_| async { Err::<(), ()>(()) }),
        )
        .await;
    assert!(outcome.is_err());
    assert_eq!(proxy.cache_size_for_test(), 0);
    server.verify().await;
}

#[tokio::test]
async fn cancellation_during_finalization_retains_the_executed_request_marker() {
    let server = upstream().await;
    let proxy = Arc::new(ChatProxy::new(reqwest::Client::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let callback_count = Arc::new(AtomicUsize::new(0));
    let leader_proxy = Arc::clone(&proxy);
    let leader_started = Arc::clone(&started);
    let leader_callback_calls = Arc::clone(&callback_count);
    let endpoint = server.uri();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered_path_finalized(
                &endpoint,
                BufferedUpstreamPath::CHAT_COMPLETIONS,
                &body(),
                &options("cancelled"),
                |_| Ok(()),
                (
                    || async { Ok::<(), ()>(()) },
                    move |_| async move {
                        leader_callback_calls.fetch_add(1, Ordering::SeqCst);
                        leader_started.notify_one();
                        std::future::pending::<Result<(), ()>>().await
                    },
                ),
            )
            .await
    });

    wait_for_barrier(&started).await;
    assert_eq!(proxy.cache_size_for_test(), 0);
    leader.abort();
    assert!(matches!(leader.await, Err(error) if error.is_cancelled()));
    assert_eq!(proxy.cache_size_for_test(), 0);

    let replay_calls = Arc::clone(&callback_count);
    let replay = tokio::time::timeout(
        BARRIER_TIMEOUT,
        proxy.forward_buffered_path_finalized(
            &server.uri(),
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            &options("retry"),
            |_| Ok(()),
            (
                || async { Ok::<(), ()>(()) },
                move |_| async move {
                    replay_calls.fetch_add(1, Ordering::SeqCst);
                    panic!("cancelled-operation replay invoked completion callback");
                },
            ),
        ),
    )
    .await
    .expect("cancelled-operation replay did not finish in time");
    assert_eq!(replay.status, 502);
    assert!(!replay.cached);
    assert_eq!(replay.execution, ExecutionDisposition::Replay);
    assert_eq!(proxy.cache_size_for_test(), 0);
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);

    let mut changed = body();
    changed["model"] = serde_json::json!("changed");
    let conflict = proxy
        .forward_buffered(&server.uri(), &changed, &options("conflict"))
        .await;
    assert_eq!(conflict.status, 409);
    assert_eq!(proxy.cache_size_for_test(), 0);
    server.verify().await;
}
