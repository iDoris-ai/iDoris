#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use idoris_contracts::ComponentCard;
use idoris_router::dispatch::BoundSupervisor;
use idoris_router::{AppState, build_app};
use tower::ServiceExt;
use wiremock::MockServer;

fn card(id: &str, endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.provider.id = id.to_string();
    card.endpoint = endpoint.to_string();
    card
}

#[test]
fn k03_two_lifecycle_cards_exit_nonzero_before_listening() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["a", "b"] {
        std::fs::write(
            dir.path().join(format!("{id}.yaml")),
            serde_yaml::to_string(&card(id, "http://127.0.0.1:8088")).unwrap(),
        )
        .unwrap();
    }
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_idoris"))
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .env("IDORIS_COMPONENTS_DIR", dir.path())
        .env("IDORIS_ROUTING_POLICY", "config/routing-policy.yaml")
        .env("IDORIS_PORT", "19873")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let timed_out = child.try_wait().unwrap().is_none();
    if timed_out {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !timed_out,
        "two lifecycle cards were accepted and server kept running"
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("多个 lifecycle"), "{stderr}");
    assert!(stderr.contains("a") && stderr.contains("b"), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("listening"));
}

async fn mismatch_is_blocked(same_provider: bool, same_endpoint: bool) {
    let upstream = MockServer::start().await;
    let mut bound_card = card("a", &upstream.uri());
    if same_endpoint {
        bound_card.provider.locality = idoris_contracts::provider::Locality::Remote;
    }
    let selected = card(
        if same_provider { "a" } else { "b" },
        if same_endpoint || !same_provider {
            &bound_card.endpoint
        } else {
            "http://127.0.0.1:9"
        },
    );
    let supervisor = BoundSupervisor::spawn_omlx(&bound_card).unwrap();
    let app = build_app(AppState {
        cards: vec![selected],
        supervisor: Some(supervisor),
        ..AppState::default()
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("X-iDoris-Privacy", "local_only")
                .body(Body::from(
                    r#"{"model":"idoris/daily","messages":[{"role":"user","content":"private"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "mismatched Supervisor contacted upstream"
    );
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn k03_provider_mismatch_is_503_with_zero_upstream_requests() {
    mismatch_is_blocked(false, false).await;
}

#[tokio::test]
async fn k03_endpoint_mismatch_is_503_with_zero_upstream_requests() {
    mismatch_is_blocked(true, false).await;
}

#[tokio::test]
async fn k03_locality_mismatch_is_503_with_zero_upstream_requests() {
    mismatch_is_blocked(true, true).await;
}
