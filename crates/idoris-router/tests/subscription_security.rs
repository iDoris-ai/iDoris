#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use idoris_backend::ChatMessage;
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::Locality;
use idoris_policy::{Decision, SUBSCRIPTION_PROVIDER_ID};
use idoris_router::dispatch::Selected;
use idoris_router::subscription::config::{SANDBOX_PROFILE_ID, SubscriptionConfig};
use idoris_router::subscription::dispatch::dispatch_selected;
use idoris_router::subscription::runtime::{
    SubscriptionRuntimeHandle, SubscriptionRuntimeRegistry, authorize_subscription,
};
use idoris_router::{AppState, build_app};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const SANDBOX_PROFILE: &str = "idoris-subscription-no-tools-readonly-v1";

fn fake_cli_bin(root: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = root.path().join("spawned");
    for name in ["claude", "codex"] {
        let path = bin.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf spawned > '{}'\nexit 91\n",
                marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }
    (bin, marker)
}

fn assert_production_gate_never_spawns(env: &[(&str, &str)]) {
    let components = TempDir::new().unwrap();
    fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/components/subscription.yaml"),
        components.path().join("subscription.yaml"),
    )
    .unwrap();
    let fake = TempDir::new().unwrap();
    let (bin, marker) = fake_cli_bin(&fake);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("IDORIS_") {
            command.env_remove(key);
        }
    }
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let mut child = command
        .current_dir(&root)
        .env("IDORIS_COMPONENTS_DIR", components.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            root.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_PORT", port.to_string())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), existing_path.to_string_lossy()),
        )
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("production subscription gate unexpectedly kept serving");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        !marker.exists(),
        "subscription CLI must not spawn before task24"
    );
}

#[test]
fn default_disabled_and_non_personal_modes_have_zero_subscription_spawns() {
    assert_production_gate_never_spawns(&[]);
    assert_production_gate_never_spawns(&[
        ("IDORIS_DEPLOY_MODE", "personal"),
        ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
        ("IDORIS_DISABLE_SUBSCRIPTION", "1"),
        ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE),
        ("IDORIS_SUBSCRIPTION_CLI", "claude"),
    ]);
    assert_production_gate_never_spawns(&[
        ("IDORIS_DEPLOY_MODE", "tenant"),
        ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
        ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE),
        ("IDORIS_SUBSCRIPTION_CLI", "claude"),
    ]);
}

#[tokio::test]
async fn ordinary_free_loopback_http_model_still_returns_200() {
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"marker": "ordinary-free-http"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let card: idoris_contracts::ComponentCard = serde_yaml::from_str(&format!(
        r#"
provider:
  id: free-http
  family: other
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{}"
version_pin: "test@1"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
"#,
        upstream.uri()
    ))
    .unwrap();
    let app = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["marker"], "ordinary-free-http");
    upstream.verify().await;
}

#[tokio::test]
async fn local_only_rejects_before_subscription_service_starts_a_request() {
    let card: idoris_contracts::ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap();
    let config = SubscriptionConfig::snapshot(
        Some("personal"),
        Some("1"),
        None,
        Some(SANDBOX_PROFILE_ID),
        Some("claude"),
    );
    let authorized = authorize_subscription(&config, &card).unwrap().unwrap();
    let handle = SubscriptionRuntimeHandle::build(authorized, &card).unwrap();
    let service = handle.service().clone();
    let mut registry = SubscriptionRuntimeRegistry::default();
    registry.insert(handle).unwrap();
    let selected = Selected {
        decision: Decision {
            chosen_id: SUBSCRIPTION_PROVIDER_ID.into(),
            reason_codes: Vec::new(),
            degradations: Vec::new(),
        },
        served_locality: Locality::Remote,
        card,
        load_policy: LoadPolicy {
            mode: LoadMode::OnDemand,
            keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
            admission: Admission::Coexist,
        },
        estimated_cost_minor: 0,
    };

    let error = dispatch_selected(
        &selected,
        PrivacyClass::LocalOnly,
        Some("127.0.0.1:1234".parse().unwrap()),
        &registry,
        vec![ChatMessage {
            role: "user".into(),
            content: "must-not-spawn".into(),
        }],
        CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.reason_code(), "SUBSCRIPTION_PRIVACY_FORBIDDEN");
    assert_eq!(service.active_requests().await, 0);
}
