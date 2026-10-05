#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use idoris_backend::{ChatMessage, ChatRequest};
use idoris_upstream::subscription::profile::{SandboxProfile, SubscriptionCli};
use idoris_upstream::subscription::relay::{SubscriptionRelay, SubscriptionRelayConfig};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn real_child_sees_safe_argv_scrubbed_secrets_and_read_only_cwd() {
    let root = tempfile::TempDir::new().unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = root.path().join("spawned");
    let script = format!(
        r#"#!/bin/sh
set -eu
printf spawned > "{}"
[ -z "${{ANTHROPIC_API_KEY+x}}" ] || exit 70
[ -z "${{OPENAI_API_KEY+x}}" ] || exit 71
[ -z "${{IDORIS_SECRET_CONFIG+x}}" ] || exit 72
if touch "$PWD/should-not-write" 2>/dev/null; then exit 73; fi
args="$*"
case "$args" in
  *--tools*--restricted*--strict-mcp-config*--no-session-persistence*--permission-prompts*) ;;
  *) exit 74 ;;
esac
cat
"#,
        marker.display()
    );
    let claude = bin.join("claude");
    fs::write(&claude, script).unwrap();
    let mut permissions = fs::metadata(&claude).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&claude, permissions).unwrap();

    let environment = BTreeMap::from([
        (
            OsString::from("PATH"),
            OsString::from(format!("{}:/usr/bin:/bin", bin.display())),
        ),
        (
            OsString::from("ANTHROPIC_API_KEY"),
            OsString::from("SECRET_ANTHROPIC_SENTINEL"),
        ),
        (
            OsString::from("OPENAI_API_KEY"),
            OsString::from("SECRET_OPENAI_SENTINEL"),
        ),
        (
            OsString::from("IDORIS_SECRET_CONFIG"),
            OsString::from("SECRET_CONFIG_SENTINEL"),
        ),
    ]);
    let relay = SubscriptionRelay::new(SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        environment,
    ));
    let response = relay
        .chat(
            ChatRequest {
                model: "claude-subscription".into(),
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: "SAFE_RESPONSE".into(),
                }],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(response.content, "SAFE_RESPONSE");
    assert_eq!(fs::read_to_string(marker).unwrap(), "spawned");
    assert!(!response.content.contains("SECRET_ANTHROPIC_SENTINEL"));
    assert!(!response.content.contains("SECRET_OPENAI_SENTINEL"));
    assert!(!response.content.contains("SECRET_CONFIG_SENTINEL"));
}
