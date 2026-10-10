use super::*;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

struct Gate {
    calls: AtomicUsize,
    received: Notify,
    release: Semaphore,
}

async fn gated(State(gate): State<Arc<Gate>>, Json(_): Json<Value>) -> (StatusCode, &'static str) {
    gate.calls.fetch_add(1, Ordering::SeqCst);
    gate.received.notify_one();
    let Ok(permit) = gate.release.acquire().await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "closed");
    };
    permit.forget();
    (StatusCode::OK, "ok")
}

async fn counted(
    State(calls): State<Arc<AtomicUsize>>,
    Json(_): Json<Value>,
) -> (StatusCode, &'static str) {
    calls.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, "ok")
}

async fn spawn_gate() -> (String, Arc<Gate>, tokio::task::JoinHandle<()>) {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        received: Notify::new(),
        release: Semaphore::new(0),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(gated))
        .with_state(gate.clone());
    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        panic!("test listener must bind");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("test listener must have an address");
    };
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), gate, task)
}

async fn spawn_counter() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(counted))
        .with_state(calls.clone());
    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        panic!("test listener must bind");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("test listener must have an address");
    };
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), calls, task)
}

fn opts(request_id: Option<&str>) -> ForwardOpts<'_> {
    ForwardOpts {
        request_id,
        tenant_id: Some("tenant-a"),
        record_id: "record-a",
        provider_id: "provider-a",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
        require_openai_usage: false,
    }
}

fn body(marker: &str) -> Value {
    serde_json::json!({"messages": [{"role": "user", "content": marker}]})
}

#[tokio::test]
async fn buffered_leader_observes_once_waiter_cache_and_conflict_do_not() {
    let (url, gate, server) = spawn_gate().await;
    let proxy = Arc::new(ChatProxy::new(reqwest::Client::new()));
    let observed = Arc::new(AtomicUsize::new(0));
    let request = body("same");

    let leader = {
        let proxy = proxy.clone();
        let observed = observed.clone();
        let url = url.clone();
        let request = request.clone();
        tokio::spawn(async move {
            proxy
                .forward_buffered_observed(&url, &request, &opts(Some("same")), || async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), ()>(())
                })
                .await
        })
    };
    gate.received.notified().await;

    let waiter = {
        let proxy = proxy.clone();
        let observed = observed.clone();
        let url = url.clone();
        let request = request.clone();
        tokio::spawn(async move {
            proxy
                .forward_buffered_observed(&url, &request, &opts(Some("same")), || async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), ()>(())
                })
                .await
        })
    };
    gate.release.add_permits(1);

    let Ok(Ok(first)) = leader.await else {
        panic!("leader must succeed");
    };
    let Ok(Ok(second)) = waiter.await else {
        panic!("waiter must replay");
    };
    assert_eq!(first.status, 200);
    assert_eq!(second.execution, ExecutionDisposition::Replay);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    drop((first, second));

    let cache = proxy
        .forward_buffered_observed(&url, &request, &opts(Some("same")), || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            }
        })
        .await;
    let Ok(cache) = cache else {
        panic!("cache replay must succeed");
    };
    assert!(cache.cached);
    assert_eq!(observed.load(Ordering::SeqCst), 1);

    let conflict = proxy
        .forward_buffered_observed(&url, &body("changed"), &opts(Some("same")), || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            }
        })
        .await;
    let Ok(conflict) = conflict else {
        panic!("conflict is a normal proxy outcome");
    };
    assert_eq!(conflict.status, 409);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn buffered_capacity_preflight_does_not_observe() {
    let mut proxy = ChatProxy::new(reqwest::Client::new());
    proxy.max_entries = 0;
    let observed = AtomicUsize::new(0);
    let result = proxy
        .forward_buffered_observed(
            "http://127.0.0.1:1",
            &body("capacity"),
            &opts(Some("capacity")),
            || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            },
        )
        .await;
    let Ok(result) = result else {
        panic!("capacity failure is a normal proxy outcome");
    };
    assert_eq!(result.status, 503);
    assert_eq!(result.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(observed.load(Ordering::SeqCst), 0);

    let mut no_permit = ChatProxy::new(reqwest::Client::new());
    no_permit.permits = Arc::new(Semaphore::new(0));
    let result = no_permit
        .forward_buffered_observed(
            "http://127.0.0.1:1",
            &body("permit"),
            &opts(None),
            || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            },
        )
        .await;
    let Ok(result) = result else {
        panic!("permit failure is a normal proxy outcome");
    };
    assert_eq!(result.status, 503);
    assert_eq!(result.execution, ExecutionDisposition::NotExecuted);
    assert_eq!(observed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn buffered_observer_failure_sends_nothing_and_does_not_poison_flight() {
    let (url, calls, server) = spawn_counter().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let request = body("observer-fail");
    let result = proxy
        .forward_buffered_observed(&url, &request, &opts(Some("retryable")), || async {
            Err::<(), &'static str>("observer failed")
        })
        .await;
    assert!(matches!(result, Err("observer failed")));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let retry = proxy
        .forward_buffered(&url, &request, &opts(Some("retryable")))
        .await;
    assert_eq!(retry.status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn buffered_observer_runs_once_across_connect_retries() {
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        panic!("temporary port must bind");
    };
    let Ok(addr) = listener.local_addr() else {
        panic!("temporary port must have an address");
    };
    drop(listener);
    let proxy = ChatProxy::with_config(
        reqwest::Client::new(),
        Duration::from_secs(60),
        vec![Duration::ZERO, Duration::ZERO],
    );
    let observed = AtomicUsize::new(0);
    let result = proxy
        .forward_buffered_observed(
            &format!("http://{addr}"),
            &body("retry"),
            &opts(None),
            || async {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<(), ()>(())
            },
        )
        .await;
    let Ok(result) = result else {
        panic!("transport failure is a normal proxy outcome");
    };
    assert_eq!(result.status, 502);
    assert_eq!(result.retries, 2);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_observer_runs_once_and_failure_sends_nothing() {
    let (url, calls, server) = spawn_counter().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let observed = AtomicUsize::new(0);
    let success = proxy
        .forward_stream_observed(&url, &body("stream"), || async {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await;
    let Ok(success) = success else {
        panic!("stream observer must succeed");
    };
    match success {
        StreamOutcome::Stream {
            status, response, ..
        } => {
            assert_eq!(status, 200);
            drop(response);
        }
        StreamOutcome::Buffered { status, .. } => panic!("unexpected buffered status {status}"),
    }
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let failed = proxy
        .forward_stream_observed(&url, &body("blocked"), || async {
            Err::<(), &'static str>("observer failed")
        })
        .await;
    assert!(matches!(failed, Err("observer failed")));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let mut no_permit = ChatProxy::new(reqwest::Client::new());
    no_permit.permits = Arc::new(Semaphore::new(0));
    let skipped = AtomicUsize::new(0);
    let preflight = no_permit
        .forward_stream_observed(&url, &body("no-permit"), || async {
            skipped.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        })
        .await;
    let Ok(StreamOutcome::Buffered { status, .. }) = preflight else {
        panic!("stream permit failure must be buffered");
    };
    assert_eq!(status, 503);
    assert_eq!(skipped.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn legacy_public_forwarding_apis_remain_unchanged() {
    let (url, calls, server) = spawn_counter().await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let buffered = proxy
        .forward_buffered(&url, &body("public-buffered"), &opts(None))
        .await;
    assert_eq!(buffered.status, 200);

    match proxy.forward_stream(&url, &body("public-stream")).await {
        StreamOutcome::Stream {
            status, response, ..
        } => {
            assert_eq!(status, 200);
            drop(response);
        }
        StreamOutcome::Buffered { status, .. } => panic!("unexpected buffered status {status}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}
