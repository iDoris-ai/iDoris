#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Notify, Semaphore},
};

struct Gate {
    calls: AtomicUsize,
    received: Notify,
    release: Semaphore,
    status: u16,
}

async fn gated(
    State(g): State<Arc<Gate>>,
    Json(_): Json<Value>,
) -> (StatusCode, [(&'static str, &'static str); 1], &'static str) {
    g.calls.fetch_add(1, Ordering::SeqCst);
    g.received.notify_one();
    g.release.acquire().await.unwrap().forget();
    if g.status == 503 {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/problem+json")],
            "overloaded",
        )
    } else {
        (StatusCode::OK, [("content-type", "application/json")], "ok")
    }
}

async fn see_other(State(calls): State<Arc<AtomicUsize>>, Json(_): Json<Value>) -> StatusCode {
    calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::SEE_OTHER
}

async fn upstream(status: u16) -> (String, Arc<Gate>, tokio::task::JoinHandle<()>) {
    let gate = Arc::new(Gate {
        calls: AtomicUsize::new(0),
        received: Notify::new(),
        release: Semaphore::new(0),
        status,
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(gated))
        .with_state(gate.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, gate, task)
}

async fn read_complete_post(stream: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    loop {
        let mut buf = [0; 1024];
        let size = stream.read(&mut buf).await.unwrap();
        assert_ne!(size, 0);
        request.extend_from_slice(&buf[..size]);
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head_end = end + 4;
        let headers = String::from_utf8_lossy(&request[..head_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while request.len() < head_end + length {
            let mut buf = [0; 1024];
            let size = stream.read(&mut buf).await.unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&buf[..size]);
        }
        assert!(request.starts_with(b"POST "));
        return;
    }
}

fn opts(id: &str) -> ForwardOpts<'_> {
    ForwardOpts {
        request_id: Some(id),
        tenant_id: Some("tenant-a"),
        record_id: "record",
        provider_id: "provider",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::Any,
        require_openai_usage: false,
    }
}

fn proxy(slots: usize) -> ChatProxy {
    let mut p = ChatProxy::new(reqwest::Client::new());
    p.permits = Arc::new(Semaphore::new(slots));
    p.header_timeout = Duration::from_secs(2);
    p.body_timeout = Duration::from_secs(2);
    p.retry_delays.clear();
    p
}

#[tokio::test]
async fn singleflight_outputs_each_own_a_permit_and_retained_state_owns_none() {
    let (url, gate, server) = upstream(200).await;
    let p = Arc::new(proxy(2));
    let body = serde_json::json!({"messages": []});
    let leader = {
        let p = p.clone();
        let url = url.clone();
        let body = body.clone();
        tokio::spawn(async move { p.forward_buffered(&url, &body, &opts("same")).await })
    };
    gate.received.notified().await;
    let options = opts("same");
    let mut waiter = Box::pin(p.forward_buffered(&url, &body, &options));
    assert!(std::future::poll_fn(|cx| Poll::Ready(waiter.as_mut().poll(cx).is_pending())).await);
    assert_eq!(p.permits.available_permits(), 0);
    gate.release.add_permits(1);
    let first = leader.await.unwrap();
    let second = waiter.await;
    assert_eq!((first.status, second.status), (200, 200));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    drop(first);
    let replay = p.forward_buffered(&url, &body, &opts("same")).await;
    assert!(replay.cached);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(p.permits.available_permits(), 0);
    assert_eq!(
        p.forward_buffered(&url, &body, &opts("same")).await.status,
        503
    );
    drop(second);
    drop(replay);
    assert_eq!(
        p.permits.available_permits(),
        2,
        "flight and cache must not retain permits"
    );
    server.abort();
}

#[tokio::test]
async fn complete_503_replay_takes_and_releases_a_fresh_permit() {
    let (url, gate, server) = upstream(503).await;
    let p = proxy(1);
    gate.release.add_permits(1);
    let body = serde_json::json!({"messages": []});
    let first = p.forward_buffered(&url, &body, &opts("503")).await;
    assert_eq!(first.status, 503);
    assert_eq!(
        first.content_type.as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(first.body.as_ref(), b"overloaded");
    assert_eq!(p.permits.available_permits(), 0);
    drop(first);
    assert_eq!(p.permits.available_permits(), 1);
    let replay = p.forward_buffered(&url, &body, &opts("503")).await;
    assert_eq!(replay.status, 503);
    assert_eq!(
        replay.content_type.as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(replay.body.as_ref(), b"overloaded");
    assert_eq!(p.permits.available_permits(), 0);
    drop(replay);
    assert_eq!(p.permits.available_permits(), 1);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn see_other_replay_does_not_repeat_post_and_rejects_changed_payload() {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(see_other))
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let p = ChatProxy::new(client);
    let body = serde_json::json!({"messages": [{"content": "first"}]});

    assert_eq!(
        p.forward_buffered(&url, &body, &opts("redirect"))
            .await
            .status,
        303
    );
    assert_eq!(
        p.forward_buffered(&url, &body, &opts("redirect"))
            .await
            .status,
        303
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let changed = serde_json::json!({"messages": [{"content": "changed"}]});
    assert_eq!(
        p.forward_buffered(&url, &changed, &opts("redirect"))
            .await
            .status,
        409
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn deadline_failures_remain_singleflight_conflicts_for_both_phases() {
    for body_phase in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let received = Arc::new(Notify::new());
        let server_received = received.clone();
        let body_sent = Arc::new(Notify::new());
        let server_body_sent = body_sent.clone();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_complete_post(&mut stream).await;
            server_calls.fetch_add(1, Ordering::SeqCst);
            server_received.notify_one();
            if body_phase {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                    .await
                    .unwrap();
                server_body_sent.notify_one();
            }
            std::future::pending::<()>().await;
        });
        let mut p = proxy(1);
        p.window = Duration::from_secs(120);
        p.header_timeout = Duration::from_secs(60);
        p.body_timeout = Duration::from_millis(30);
        let p = Arc::new(p);
        let body = serde_json::json!({"messages": []});
        let leader = tokio::spawn({
            let p = p.clone();
            let url = url.clone();
            let body = body.clone();
            async move { p.forward_buffered(&url, &body, &opts("deadline")).await }
        });
        received.notified().await;
        if body_phase {
            body_sent.notified().await;
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(61)).await;
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), leader)
                .await
                .unwrap()
                .unwrap()
                .status,
            504
        );
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                p.forward_buffered(&url, &body, &opts("deadline"))
            )
            .await
            .unwrap()
            .status,
            504
        );
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                p.forward_buffered(
                    &url,
                    &serde_json::json!({"changed": true}),
                    &opts("deadline")
                )
            )
            .await
            .unwrap()
            .status,
            409
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
        if !body_phase {
            tokio::time::resume();
        }
    }
}
