#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream;
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

const PARTIAL: &[u8] = b"data: partial\n\n";
const COMPLETE: &[u8] = b"data: [DONE]\n\n";

#[derive(Clone)]
struct UpstreamState {
    releases: mpsc::Sender<oneshot::Sender<()>>,
    requests: Arc<AtomicUsize>,
}

async fn upstream_chat(State(state): State<UpstreamState>, Json(_): Json<Value>) -> Response {
    let index = state.requests.fetch_add(1, Ordering::SeqCst);
    let (release, wait_for_release) = oneshot::channel();
    state.releases.send(release).await.unwrap();
    let include_done = index == 1;
    let body = stream::unfold(
        (false, Some(wait_for_release), include_done),
        |(sent_prefix, release, include_done)| async move {
            if !sent_prefix {
                Some((
                    Ok::<_, Infallible>(Bytes::from_static(PARTIAL)),
                    (true, release, include_done),
                ))
            } else if let Some(release) = release {
                let _ = release.await;
                if include_done {
                    Some((Ok(Bytes::from_static(COMPLETE)), (true, None, false)))
                } else {
                    None
                }
            } else {
                None
            }
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

fn resident_card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.provider.id = "sse-test".to_string();
    card.endpoint = endpoint.to_string();
    card.load_policy = Some(idoris_contracts::load_policy::LoadPolicy {
        mode: idoris_contracts::load_policy::LoadMode::Resident,
        keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
        admission: idoris_contracts::load_policy::Admission::Coexist,
    });
    card
}

async fn post_stream(client: &reqwest::Client, endpoint: &str) -> reqwest::Response {
    client
        .post(endpoint)
        .json(&serde_json::json!({
            "model": "idoris/daily",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap()
}

async fn read_exact_prefix(response: &mut reqwest::Response, expected: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    while bytes.len() < expected.len() {
        let next = response
            .chunk()
            .await
            .unwrap()
            .expect("SSE ended before its partial event");
        bytes.extend_from_slice(&next);
    }
    assert_eq!(bytes, expected);
    bytes
}

#[tokio::test]
async fn sse_http_client_errors_on_normal_upstream_eof_without_done_and_accepts_done() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (release_tx, mut release_rx) = mpsc::channel(2);
        let upstream_state = UpstreamState {
            releases: release_tx,
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let upstream_app = Router::new()
            .route("/v1/chat/completions", post(upstream_chat))
            .with_state(upstream_state);
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let app = build_app(AppState {
            cards: vec![resident_card(&format!("http://{upstream_addr}"))],
            ..AppState::default()
        });
        let router_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let router_addr = router_listener.local_addr().unwrap();
        let router_task = tokio::spawn(async move {
            axum::serve(router_listener, app).await.unwrap();
        });
        let endpoint = format!("http://{router_addr}/v1/chat/completions");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let mut incomplete = post_stream(&client, &endpoint).await;
        assert_eq!(incomplete.status(), StatusCode::OK);
        // Do not let the upstream finish until the real client has received bytes.
        read_exact_prefix(&mut incomplete, PARTIAL).await;
        let release = release_rx.recv().await.unwrap();
        release.send(()).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(2), incomplete.chunk())
            .await
            .expect("unterminated SSE read timed out");
        let err = next.unwrap_err();
        assert!(
            err.is_body() || err.is_decode(),
            "expected a streamed body/decode error, got {err:?}"
        );
        assert!(!err.is_timeout(), "unexpected timeout error: {err:?}");

        let mut complete = post_stream(&client, &endpoint).await;
        assert_eq!(complete.status(), StatusCode::OK);
        let mut complete_bytes = read_exact_prefix(&mut complete, PARTIAL).await;
        let release = release_rx.recv().await.unwrap();
        release.send(()).unwrap();
        // Collect through EOF without assuming TCP preserves the DONE chunk boundary.
        let done = complete.bytes().await.unwrap();
        complete_bytes.extend_from_slice(&done);
        assert_eq!(done, COMPLETE);
        assert_eq!(complete_bytes, [PARTIAL, COMPLETE].concat());

        router_task.abort();
        upstream_task.abort();
        let _ = router_task.await;
        let _ = upstream_task.await;
    })
    .await
    .expect("HTTP SSE termination test exceeded its timeout");
}
