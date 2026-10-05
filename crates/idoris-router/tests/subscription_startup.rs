//! K04/H8: subscription cards must fail startup before B3, even when enabled.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use idoris_router::subscription::config::{
    RegistrationAction, SANDBOX_PROFILE_ID, SubscriptionConfig, decide_registration,
};
use idoris_router::subscription::runtime::{SubscriptionRuntimeHandle, authorize_subscription};
use tempfile::TempDir;

fn shipped_subscription_card() -> idoris_contracts::ComponentCard {
    serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap()
}

fn config(
    mode: Option<&str>,
    enable: Option<&str>,
    disable: Option<&str>,
    sandbox: Option<&str>,
    cli: Option<&str>,
) -> SubscriptionConfig {
    SubscriptionConfig::snapshot(mode, enable, disable, sandbox, cli)
}

#[test]
fn future_registration_matrix_preserves_org_disable_and_sandbox_precedence() {
    let ok = Some(SANDBOX_PROFILE_ID);
    for (cfg, expected) in [
        (
            config(None, None, None, None, None),
            RegistrationAction::Skip,
        ),
        (
            config(Some("personal"), Some("1"), Some("1"), ok, Some("claude")),
            RegistrationAction::Skip,
        ),
        (
            config(Some("tenant"), Some("1"), Some("1"), ok, Some("claude")),
            RegistrationAction::Refuse,
        ),
        (
            config(Some("unknown"), Some("1"), None, ok, Some("claude")),
            RegistrationAction::Refuse,
        ),
        (
            config(Some("personal"), Some("1"), None, None, Some("claude")),
            RegistrationAction::Refuse,
        ),
        (
            config(Some("personal"), Some("1"), None, ok, Some("unknown-cli")),
            RegistrationAction::Refuse,
        ),
        (
            config(Some("personal"), Some("1"), None, ok, Some("codex")),
            RegistrationAction::Register,
        ),
    ] {
        assert_eq!(decide_registration(&cfg).action, expected, "{cfg:?}");
    }
}

#[tokio::test]
async fn test_build_can_construct_the_authorized_success_path_without_spawning() {
    let card = shipped_subscription_card();
    let authorized = authorize_subscription(
        &config(
            Some("personal"),
            Some("1"),
            None,
            Some(SANDBOX_PROFILE_ID),
            Some("claude"),
        ),
        &card,
    )
    .unwrap()
    .unwrap();
    let handle = SubscriptionRuntimeHandle::build(authorized, &card).unwrap();
    assert_eq!(handle.model_id(), "claude-subscription");
    assert_eq!(handle.service().active_requests().await, 0);
}

#[test]
fn factory_revalidation_rejects_a_form_changed_after_authorization() {
    let card = shipped_subscription_card();
    let authorized = authorize_subscription(
        &config(
            Some("personal"),
            Some("1"),
            None,
            Some(SANDBOX_PROFILE_ID),
            Some("claude"),
        ),
        &card,
    )
    .unwrap()
    .unwrap();
    let mut impostor = card;
    impostor.form = idoris_contracts::component_card::Form::HttpService;
    impostor.endpoint = "http://127.0.0.1:9".into();
    assert!(SubscriptionRuntimeHandle::build(authorized, &impostor).is_err());
}

fn assert_subscription_startup_rejected(form: &str, endpoint: &str, env: &[(&str, &str)]) {
    let dir = TempDir::new().unwrap();
    let card = format!(
        r#"
provider:
  id: subscription
  family: other
  tier: remote
  capabilities: [chat]
  privacy_class: any
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: remote
form: {form}
endpoint: "{endpoint}"
version_pin: "subscription@0.1.0"
privacy_class: any
allowed_egress: [internet]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
"#
    );
    // Prove the fixture is structurally and registration-valid: rejection
    // must come from the unsupported-subscription gate, not malformed YAML.
    let component = serde_yaml::from_str(&card).unwrap();
    idoris_contracts::Contract::validate(&component).unwrap();
    idoris_policy::validate_registration(&[idoris_policy::Card {
        component,
        roles: vec![],
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: Some(0),
        admission_status: idoris_policy::AdmissionStatus::Ready,
    }])
    .unwrap();
    fs::write(dir.path().join("subscription.yaml"), card).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("IDORIS_") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .current_dir(&root)
        .env("IDORIS_COMPONENTS_DIR", dir.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            root.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_PORT", port.to_string())
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        !timed_out,
        "subscription card was accepted; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for text in ["启动失败", "subscription", "订阅中转", "B3", "移除"] {
        assert!(stderr.contains(text), "missing {text:?}: {stderr}");
    }
    assert!(output.stdout.is_empty(), "must reject before listening");
}

#[test]
fn subscription_http_card_is_rejected_without_enable() {
    assert_subscription_startup_rejected("http_service", "http://127.0.0.1:9", &[]);
}

#[test]
fn subscription_http_card_is_rejected_in_tenant_deployment() {
    assert_subscription_startup_rejected(
        "http_service",
        "http://127.0.0.1:9",
        &[("IDORIS_DEPLOY_MODE", "tenant")],
    );
}

#[test]
fn subscription_http_card_is_rejected_even_when_explicitly_enabled() {
    assert_subscription_startup_rejected(
        "http_service",
        "http://127.0.0.1:9",
        &[
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", "configured"),
        ],
    );
}

#[test]
fn subscription_spawn_card_is_rejected_with_kill_switch() {
    assert_subscription_startup_rejected(
        "spawn_cli",
        "spawn://subscription",
        &[("IDORIS_DISABLE_SUBSCRIPTION", "1")],
    );
}

#[test]
fn production_k04_still_rejects_the_shipped_spawn_card_across_env_matrix() {
    for env in [
        vec![],
        vec![("IDORIS_DEPLOY_MODE", "tenant")],
        vec![("IDORIS_DEPLOY_MODE", "unknown")],
        vec![
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
        ],
        vec![
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_DISABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
        ],
        vec![
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
            ("IDORIS_SUBSCRIPTION_CLI", "claude"),
        ],
    ] {
        assert_subscription_startup_rejected("spawn_cli", "spawn://subscription", &env);
    }
}
