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

fn run_production_startup(
    card_yaml: &str,
    env: &[(&str, &str)],
) -> (bool, std::process::Output, bool) {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;

    let components = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    fs::write(components.path().join("subscription.yaml"), card_yaml).unwrap();

    let fake_bin = state.path().join("bin");
    fs::create_dir(&fake_bin).unwrap();
    let spawn_marker = state.path().join("spawned");
    for name in ["claude", "codex"] {
        let path = fake_bin.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf spawned > '{}'\nexit 91\n",
                spawn_marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let admin_port = loop {
        let candidate = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if candidate != port {
            break candidate;
        }
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_idoris"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("IDORIS_") {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .current_dir(&root)
        .env("IDORIS_COMPONENTS_DIR", components.path())
        .env(
            "IDORIS_ROUTING_POLICY",
            root.join("config/routing-policy.yaml"),
        )
        .env("IDORIS_CATALOG", root.join("config/catalog.yaml"))
        .env("IDORIS_DB_PATH", state.path().join("state.sqlite3"))
        .env("IDORIS_PORT", port.to_string())
        .env("IDORIS_ADMIN_PORT", admin_port.to_string())
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), existing_path.to_string_lossy()),
        )
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut started = false;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port))
            && stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .is_ok()
        {
            let mut response = String::new();
            if stream.read_to_string(&mut response).is_ok() {
                assert!(response.starts_with("HTTP/1.1 200"), "{response}");
                started = true;
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    (started, output, spawn_marker.exists())
}

fn shipped_card_text() -> &'static str {
    include_str!("../../../config/components/subscription.yaml")
}

#[test]
fn production_default_off_and_kill_switch_start_without_registering_or_spawning() {
    for env in [
        vec![],
        vec![
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_DISABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
            ("IDORIS_SUBSCRIPTION_CLI", "claude"),
        ],
    ] {
        let (started, output, spawned) = run_production_startup(shipped_card_text(), &env);
        assert!(
            started,
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!spawned, "startup must never invoke the subscription CLI");
    }
}

#[test]
fn production_enabled_valid_profile_starts_without_eager_cli_spawn() {
    let env = [
        ("IDORIS_DEPLOY_MODE", "personal"),
        ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
        ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
        ("IDORIS_SUBSCRIPTION_CLI", "claude"),
    ];
    let (started, output, spawned) = run_production_startup(shipped_card_text(), &env);
    assert!(
        started,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!spawned, "runtime construction must be spawn-free");
}

#[test]
fn production_org_unknown_sandbox_and_cli_fail_closed_before_listen() {
    for env in [
        vec![
            ("IDORIS_DEPLOY_MODE", "tenant"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
            ("IDORIS_SUBSCRIPTION_CLI", "claude"),
        ],
        vec![
            ("IDORIS_DEPLOY_MODE", "unknown"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
            ("IDORIS_SUBSCRIPTION_CLI", "claude"),
        ],
        vec![
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", "wrong"),
            ("IDORIS_SUBSCRIPTION_CLI", "claude"),
        ],
        vec![
            ("IDORIS_DEPLOY_MODE", "personal"),
            ("IDORIS_ENABLE_SUBSCRIPTION", "1"),
            ("IDORIS_SUBSCRIPTION_SANDBOX", SANDBOX_PROFILE_ID),
            ("IDORIS_SUBSCRIPTION_CLI", "unknown-cli"),
        ],
    ] {
        let (started, output, spawned) = run_production_startup(shipped_card_text(), &env);
        assert!(
            !started,
            "invalid subscription configuration unexpectedly listened"
        );
        assert!(!output.status.success());
        assert!(!spawned, "rejected configuration must have zero CLI spawns");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("subscription registration failed"),
            "{stderr}"
        );
    }
}

#[test]
fn production_wrong_subscription_form_and_spawn_cli_impersonation_fail_structurally() {
    let wrong_form = shipped_card_text()
        .replace("form: spawn_cli", "form: http_service")
        .replace("spawn://subscription", "http://127.0.0.1:9");
    let (started, output, spawned) = run_production_startup(&wrong_form, &[]);
    assert!(!started);
    assert!(!spawned);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fixed card boundary"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let impostor = shipped_card_text().replace("id: subscription", "id: impostor");
    let (started, output, spawned) = run_production_startup(&impostor, &[]);
    assert!(!started);
    assert!(!spawned);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fixed card boundary"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
