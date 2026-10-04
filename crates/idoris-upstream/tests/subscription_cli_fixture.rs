#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support {
    pub mod subscription_cli;
}

use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use support::subscription_cli::FakeSubscriptionCli;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

fn marker_pids(contents: &str) -> Vec<(i32, i32)> {
    contents
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(':');
            let _kind = parts.next()?;
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect()
}

#[tokio::test]
async fn fixture_basic_modes_are_real() {
    let fixture = FakeSubscriptionCli::install();

    let stderr = Command::new(fixture.program("claude"))
        .env("IDORIS_FAKE_CLI_MODE", "stderr")
        .env("IDORIS_FAKE_CLI_OUTPUT", "stderr-sentinel")
        .output()
        .await
        .expect("stderr fixture runs");
    assert_eq!(
        String::from_utf8_lossy(&stderr.stderr).trim(),
        "stderr-sentinel"
    );

    let nonzero = Command::new(fixture.program("codex"))
        .env("IDORIS_FAKE_CLI_MODE", "nonzero")
        .status()
        .await
        .expect("nonzero fixture runs");
    assert_eq!(nonzero.code(), Some(23));

    let empty = Command::new(fixture.program("codex"))
        .env("IDORIS_FAKE_CLI_MODE", "empty")
        .output()
        .await
        .expect("empty fixture runs");
    assert!(empty.stdout.is_empty());

    let output_path = fixture.marker("result.txt");
    let output = Command::new(fixture.program("codex"))
        .env("IDORIS_FAKE_CLI_MODE", "output-file")
        .env("IDORIS_FAKE_CLI_OUTPUT", "file-result")
        .arg("-o")
        .arg(&output_path)
        .status()
        .await
        .expect("output-file fixture runs");
    assert!(output.success());
    assert_eq!(std::fs::read_to_string(output_path).unwrap(), "file-result");
}

#[tokio::test]
async fn fixture_creates_parent_and_grandchild_in_one_group_and_can_be_reaped() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("pids.txt");
    let mut command = Command::new(fixture.program("codex"));
    command
        .env("IDORIS_FAKE_CLI_MODE", "grandchild")
        .env("IDORIS_FAKE_CLI_MARKER", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().expect("grandchild fixture spawns");
    let group = child.id().expect("fixture pid") as i32;

    let entries = timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(&marker) {
                let entries = marker_pids(&contents);
                if entries.len() >= 2 {
                    break entries;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("parent and grandchild markers must appear");

    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|(_, pgid)| *pgid == group));
    assert!(entries.iter().any(|(pid, _)| *pid == group));
    assert!(entries.iter().any(|(pid, _)| *pid != group));

    killpg(Pid::from_raw(group), Signal::SIGTERM).expect("group TERM succeeds");
    timeout(Duration::from_secs(2), child.wait())
        .await
        .expect("parent exits after group TERM")
        .expect("wait parent");
    assert!(
        killpg(Pid::from_raw(group), None).is_err(),
        "group must be gone"
    );
}

#[tokio::test]
async fn fixture_exposes_hang_ignore_term_hold_pipe_and_pid_marker_modes() {
    let fixture = FakeSubscriptionCli::install();
    for mode in ["hang", "ignore-term", "hold-pipe"] {
        let marker = fixture.marker(&format!("{mode}.txt"));
        let mut command = Command::new(fixture.program("claude"));
        command
            .env("IDORIS_FAKE_CLI_MODE", mode)
            .env("IDORIS_FAKE_CLI_MARKER", &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("fixture mode spawns");
        let group = child.id().unwrap() as i32;
        timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("mode marker appears");
        killpg(Pid::from_raw(group), Signal::SIGKILL).expect("group KILL succeeds");
        let _ = timeout(Duration::from_secs(2), child.wait()).await;
    }

    let marker = fixture.marker("pid-marker.txt");
    let status = Command::new(fixture.program("claude"))
        .env("IDORIS_FAKE_CLI_MODE", "pid-marker")
        .env("IDORIS_FAKE_CLI_MARKER", &marker)
        .status()
        .await
        .expect("pid-marker fixture runs");
    assert!(status.success());
    let entries = marker_pids(&std::fs::read_to_string(marker).unwrap());
    assert_eq!(entries.len(), 1);
}
