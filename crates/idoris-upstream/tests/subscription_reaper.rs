#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support {
    pub mod subscription_cli;
}

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::killpg;
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use idoris_upstream::subscription::reaper::{ProcessGroupReaper, await_drain_bounded};
use support::subscription_cli::FakeSubscriptionCli;

fn spawn(
    fixture: &FakeSubscriptionCli,
    mode: &str,
) -> (ProcessGroupReaper, Option<tokio::process::ChildStdout>) {
    let mut command = Command::new(fixture.program("codex"));
    command
        .current_dir(fixture.bin_dir())
        .env_clear()
        .envs(BTreeMap::from([
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (OsString::from("IDORIS_FAKE_CLI_MODE"), OsString::from(mode)),
            (
                OsString::from("IDORIS_FAKE_CLI_MARKER"),
                fixture.marker(mode).into_os_string(),
            ),
        ]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().expect("fixture spawn");
    let stdout = child.stdout.take();
    (ProcessGroupReaper::new(child).unwrap(), stdout)
}

async fn wait_marker(path: &std::path::Path, minimum_lines: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::fs::read_to_string(path)
                .ok()
                .is_some_and(|text| text.lines().count() >= minimum_lines)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture marker");
}

fn group_is_gone(group: i32) -> bool {
    killpg(Pid::from_raw(group), None).is_err()
}

#[tokio::test]
async fn term_reaps_parent_and_grandchild_as_one_group() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("grandchild");
    let (reaper, _) = spawn(&fixture, "grandchild");
    let group = reaper.process_group();
    wait_marker(&marker, 2).await;
    let outcome = reaper.terminate(Duration::from_millis(500)).await.unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.escalated_to_kill);
    assert!(group_is_gone(group));
}

#[tokio::test]
async fn ignored_term_escalates_to_kill_and_reaps_direct_child() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("ignore-term");
    let (reaper, _) = spawn(&fixture, "ignore-term");
    let group = reaper.process_group();
    wait_marker(&marker, 1).await;
    let outcome = reaper.terminate(Duration::from_millis(100)).await.unwrap();
    assert!(outcome.escalated_to_kill);
    assert!(group_is_gone(group));
}

#[tokio::test]
async fn parent_exit_clears_pipe_holding_descendant_and_drain_is_bounded() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("hold-pipe");
    let (reaper, stdout) = spawn(&fixture, "hold-pipe");
    let group = reaper.process_group();
    wait_marker(&marker, 2).await;
    let mut stdout = stdout.expect("stdout pipe");
    let drain = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let outcome = reaper
        .finish_after_parent_exit(Duration::from_millis(300))
        .await
        .unwrap();
    let bytes = await_drain_bounded(drain, Duration::from_millis(300))
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("fake-cli-output"));
    assert!(group_is_gone(group));
    assert!(!outcome.escalated_to_kill);
}

#[tokio::test]
async fn bounded_drain_aborts_a_task_that_never_finishes() {
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let err = await_drain_bounded(task, Duration::from_millis(20))
        .await
        .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_CLEANUP_FAILED");
}
