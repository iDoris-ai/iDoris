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
use idoris_upstream::subscription::service::SubscriptionService;
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use tokio_util::sync::CancellationToken;

struct FakeBin {
    dir: tempfile::TempDir,
    markers: tempfile::TempDir,
}

impl FakeBin {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let markers = tempfile::TempDir::new().unwrap();
        let script = r#"#!/bin/sh
set -eu
marker="$TEST_MARKER_DIR/$$"
pgid="$(ps -o pgid= -p $$ | tr -d ' ')"
echo "$pgid" > "$marker"
/bin/sh -c 'trap "exit 0" TERM INT; while :; do sleep 1; done' &
child=$!
cat >/dev/null
trap 'wait "$child" 2>/dev/null || true; exit 0' TERM INT
while :; do sleep 1; done
"#;
        for name in ["claude", "codex"] {
            let path = dir.path().join(name);
            fs::write(&path, script).unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
        Self { dir, markers }
    }

    fn env(&self) -> BTreeMap<OsString, OsString> {
        BTreeMap::from([
            (
                OsString::from("PATH"),
                OsString::from(format!("{}:/usr/bin:/bin", self.dir.path().display())),
            ),
            (
                OsString::from("TEST_MARKER_DIR"),
                self.markers.path().as_os_str().to_os_string(),
            ),
        ])
    }

    async fn groups(&self, count: usize) -> Vec<i32> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut groups = fs::read_dir(self.markers.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter_map(|entry| fs::read_to_string(entry.path()).ok())
                    .filter_map(|value| value.trim().parse().ok())
                    .collect::<Vec<_>>();
                groups.sort_unstable();
                groups.dedup();
                if groups.len() >= count {
                    return groups;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake CLI groups must appear")
    }
}

fn service(fake: &FakeBin) -> SubscriptionService {
    let mut config = SubscriptionRelayConfig::with_environment(
        SandboxProfile::fixed(SubscriptionCli::Claude),
        fake.env(),
    );
    config.process_timeout = Duration::from_secs(30);
    config.termination_grace = Duration::from_millis(100);
    SubscriptionService::new(SubscriptionRelay::new(config), Duration::from_secs(3))
}

fn request(id: &str) -> ChatRequest {
    ChatRequest {
        model: "claude-subscription".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: id.into(),
        }],
    }
}

fn group_is_gone(group: i32) -> bool {
    killpg(Pid::from_raw(group), None).is_err()
}

#[tokio::test]
async fn shutdown_rejects_new_requests_and_waits_for_all_groups_to_disappear() {
    let fake = FakeBin::new();
    let service = service(&fake);
    let a = {
        let service = service.clone();
        tokio::spawn(async move { service.chat(request("a"), CancellationToken::new()).await })
    };
    let b = {
        let service = service.clone();
        tokio::spawn(async move { service.chat(request("b"), CancellationToken::new()).await })
    };

    let groups = fake.groups(2).await;
    assert_eq!(service.active_requests().await, 2);
    service.shutdown().await.unwrap();
    assert!(!service.is_accepting().await);
    assert_eq!(service.active_requests().await, 0);
    for group in groups {
        assert!(group_is_gone(group));
    }
    assert_eq!(
        a.await.unwrap().unwrap_err().reason_code(),
        "RELAY_CANCELLED"
    );
    assert_eq!(
        b.await.unwrap().unwrap_err().reason_code(),
        "RELAY_CANCELLED"
    );
    assert_eq!(
        service
            .chat(request("late"), CancellationToken::new())
            .await
            .unwrap_err()
            .reason_code(),
        "RELAY_CANCELLED"
    );
}

#[tokio::test]
async fn repeated_shutdowns_and_request_finish_race_are_idempotent() {
    let fake = FakeBin::new();
    let service = service(&fake);
    let request_task = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .chat(request("race"), CancellationToken::new())
                .await
        })
    };
    let groups = fake.groups(1).await;
    let (first, second) = tokio::join!(service.shutdown(), service.shutdown());
    first.unwrap();
    second.unwrap();
    assert_eq!(service.active_requests().await, 0);
    assert!(group_is_gone(groups[0]));
    assert_eq!(
        request_task.await.unwrap().unwrap_err().reason_code(),
        "RELAY_CANCELLED"
    );
}
