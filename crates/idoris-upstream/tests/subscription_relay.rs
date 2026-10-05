#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use idoris_backend::{ChatMessage, ChatRequest};
use idoris_upstream::subscription::profile::{SandboxProfile, SubscriptionCli};
use idoris_upstream::subscription::relay::{SubscriptionRelay, SubscriptionRelayConfig};
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use tokio_util::sync::CancellationToken;

struct FakeBin {
    dir: tempfile::TempDir,
}

impl FakeBin {
    fn new(script: &str) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        for name in ["claude", "codex"] {
            let path = dir.path().join(name);
            fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        Self { dir }
    }

    fn env(&self) -> BTreeMap<OsString, OsString> {
        BTreeMap::from([(
            OsString::from("PATH"),
            OsString::from(format!("{}:/usr/bin:/bin", self.dir.path().display())),
        )])
    }
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "hello".into(),
        }],
    }
}

fn relay(cli: SubscriptionCli, fake: &FakeBin) -> SubscriptionRelay {
    SubscriptionRelay::new(SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(cli),
        fake.env(),
    ))
}

#[tokio::test]
async fn list_and_claude_chat_return_subscription_model_and_content() {
    let fake = FakeBin::new("cat");
    let relay = relay(SubscriptionCli::Claude, &fake);
    assert_eq!(relay.list()[0].id, "claude-subscription");
    let response = relay
        .chat(request("claude-subscription"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.model, "claude-subscription");
    assert_eq!(response.content, "hello");
}

#[tokio::test]
async fn codex_prefers_private_result_file_over_stdout() {
    let fake = FakeBin::new(
        r#"out=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then out="$arg"; break; fi
  prev="$arg"
done
cat >/dev/null
printf 'from-file' > "$out"
printf 'from-stdout'
"#,
    );
    let response = relay(SubscriptionCli::Codex, &fake)
        .chat(request("codex-subscription"), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.content, "from-file");
}

#[tokio::test]
async fn nonzero_empty_timeout_cancel_and_output_limit_are_fixed_errors() {
    let nonzero = FakeBin::new("cat >/dev/null; printf 'SECRET' >&2; exit 23");
    let err = relay(SubscriptionCli::Claude, &nonzero)
        .chat(request("claude-subscription"), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_CLI_FAILED");
    assert_eq!(err.diagnostics().unwrap().exit_code, Some(23));
    assert!(!format!("{err:?}").contains("SECRET"));

    let empty = FakeBin::new("cat >/dev/null");
    assert_eq!(
        relay(SubscriptionCli::Claude, &empty)
            .chat(request("claude-subscription"), CancellationToken::new())
            .await
            .unwrap_err()
            .reason_code(),
        "RELAY_EMPTY_OUTPUT"
    );

    let hanging = FakeBin::new("cat >/dev/null; while :; do sleep 1; done");
    let mut config = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        hanging.env(),
    );
    config.process_timeout = Duration::from_millis(100);
    config.termination_grace = Duration::from_millis(50);
    assert_eq!(
        SubscriptionRelay::new(config)
            .chat(request("claude-subscription"), CancellationToken::new())
            .await
            .unwrap_err()
            .reason_code(),
        "RELAY_TIMEOUT"
    );

    let cancelled = FakeBin::new("cat >/dev/null; while :; do sleep 1; done");
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        relay(SubscriptionCli::Claude, &cancelled)
            .chat(request("claude-subscription"), cancel)
            .await
            .unwrap_err()
            .reason_code(),
        "RELAY_CANCELLED"
    );

    let noisy = FakeBin::new(
        "cat >/dev/null; i=0; while [ $i -lt 100 ]; do printf 0123456789; i=$((i+1)); done; while :; do sleep 1; done",
    );
    let mut config = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        noisy.env(),
    );
    config.max_output_bytes = 64;
    config.termination_grace = Duration::from_millis(50);
    assert_eq!(
        SubscriptionRelay::new(config)
            .chat(request("claude-subscription"), CancellationToken::new())
            .await
            .unwrap_err()
            .reason_code(),
        "RELAY_OUTPUT_LIMIT"
    );
}

#[tokio::test]
async fn normal_exit_leaves_no_process_group() {
    let fake = FakeBin::new(
        r#"printf '%s\n' "$$:$(ps -o pgid= -p $$ | tr -d ' ')" > "$TEST_MARKER"
cat
"#,
    );
    let marker = fake.dir.path().join("marker");
    let mut environment = fake.env();
    environment.insert(
        OsString::from("TEST_MARKER"),
        marker.as_os_str().to_os_string(),
    );
    let relay = SubscriptionRelay::new(SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        environment,
    ));
    relay
        .chat(request("claude-subscription"), CancellationToken::new())
        .await
        .unwrap();
    let group: i32 = fs::read_to_string(marker)
        .unwrap()
        .trim()
        .split(':')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(killpg(Pid::from_raw(group), None).is_err());
}
