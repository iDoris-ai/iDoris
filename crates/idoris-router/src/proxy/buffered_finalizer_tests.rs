#![allow(clippy::unwrap_used)]

use super::*;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

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

#[tokio::test]
async fn failed_finalization_never_publishes_success_or_repeats_the_post() {
    let server = upstream().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let first = proxy
        .forward_buffered_path_finalized(
            &server.uri(),
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            &body(),
            &options("first"),
            |_| Ok(()),
            (
                || async { Ok(()) },
                |outcome| async move {
                    assert_eq!(outcome.status, 200);
                    Err::<(), ()>(())
                },
            ),
        )
        .await;
    assert!(first.is_err());
    assert_eq!(proxy.cache_size_for_test(), 0);
    let replay = proxy
        .forward_buffered(&server.uri(), &body(), &options("retry"))
        .await;
    assert_eq!(replay.status, 503);
    assert!(!replay.cached);
    assert_eq!(replay.execution, ExecutionDisposition::Replay);
    let mut changed = body();
    changed["model"] = serde_json::json!("changed");
    let conflict = proxy
        .forward_buffered(&server.uri(), &changed, &options("conflict"))
        .await;
    assert_eq!(conflict.status, 409);
    server.verify().await;
}

#[tokio::test]
async fn finalization_holds_the_cache_and_waiters_until_it_commits() {
    let server = upstream().await;
    let proxy = Arc::new(ChatProxy::new(reqwest::Client::new()));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let leader_proxy = proxy.clone();
    let leader_started = started.clone();
    let leader_release = release.clone();
    let endpoint = server.uri();
    let leader = tokio::spawn(async move {
        leader_proxy
            .forward_buffered_path_finalized(
                &endpoint,
                BufferedUpstreamPath::CHAT_COMPLETIONS,
                &body(),
                &options("leader"),
                |_| Ok(()),
                (
                    || async { Ok::<(), ()>(()) },
                    move |_| async move {
                        leader_started.notify_one();
                        leader_release.notified().await;
                        Ok(())
                    },
                ),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    assert_eq!(proxy.cache_size_for_test(), 0);
    let endpoint = server.uri();
    let payload = body();
    let opts = options("waiter");
    let waiter = proxy.forward_buffered(&endpoint, &payload, &opts);
    tokio::pin!(waiter);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiter.as_mut())
            .await
            .is_err()
    );
    release.notify_one();
    assert_eq!(leader.await.unwrap().status, 200);
    let replay = waiter.await;
    assert_eq!(replay.status, 200);
    assert!(replay.cached);
    assert_eq!(replay.origin_record_id.as_deref(), Some("leader"));
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
    let leader_proxy = proxy.clone();
    let leader_started = started.clone();
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
                        leader_started.notify_one();
                        std::future::pending::<Result<(), ()>>().await
                    },
                ),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    leader.abort();
    assert!(matches!(leader.await, Err(error) if error.is_cancelled()));
    let replay = proxy
        .forward_buffered(&server.uri(), &body(), &options("retry"))
        .await;
    assert_eq!(replay.status, 503);
    assert_eq!(replay.execution, ExecutionDisposition::Replay);
    assert_eq!(proxy.cache_size_for_test(), 0);
    server.verify().await;
}
