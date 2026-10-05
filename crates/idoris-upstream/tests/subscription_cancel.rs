#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support {
    pub mod subscription_cli;
}

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::time::Duration;

use idoris_upstream::subscription::command::CommandSpec;
use idoris_upstream::subscription::process::{
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_PROCESS_TIMEOUT, DEFAULT_TERMINATION_GRACE,
    run_process_controlled,
};
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use support::subscription_cli::FakeSubscriptionCli;
use tokio_util::sync::CancellationToken;

// CI macOS runners can take materially longer than local machines to reap
// process groups; keep this comfortably above the observed scheduling jitter.
const TEST_TERMINATION_GRACE: Duration = Duration::from_millis(500);

fn spec(program: std::path::PathBuf) -> CommandSpec {
    let leaked: &'static str = Box::leak(program.to_string_lossy().into_owned().into_boxed_str());
    CommandSpec {
        program: leaked,
        args: Vec::new(),
        stdin: None,
    }
}

fn env(fixture: &FakeSubscriptionCli, mode: &str, marker: &str) -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("IDORIS_FAKE_CLI_MODE"), OsString::from(mode)),
        (
            OsString::from("IDORIS_FAKE_CLI_MARKER"),
            fixture.marker(marker).into_os_string(),
        ),
    ])
}

async fn wait_group(marker: &std::path::Path) -> i32 {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(marker)
                && let Some(line) = text.lines().find(|line| line.starts_with("parent:"))
                && let Some(group) = line.split(':').nth(2)
                && let Ok(group) = group.parse()
            {
                return group;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture must publish process group")
}

async fn wait_gone(group: i32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while killpg(Pid::from_raw(group), None).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned process group must disappear");
}

#[test]
fn defaults_are_the_documented_120s_and_5s() {
    assert_eq!(DEFAULT_PROCESS_TIMEOUT, Duration::from_secs(120));
    assert_eq!(DEFAULT_TERMINATION_GRACE, Duration::from_secs(5));
}

#[tokio::test]
async fn pre_cancelled_request_never_spawns() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("pre-cancel");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = run_process_controlled(
        &spec(fixture.program("codex")),
        fixture.bin_dir(),
        &env(&fixture, "pid-marker", "pre-cancel"),
        DEFAULT_MAX_OUTPUT_BYTES,
        Duration::from_millis(100),
        TEST_TERMINATION_GRACE,
        cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_CANCELLED");
    assert!(!marker.exists());
}

#[tokio::test]
async fn timeout_reaps_even_a_term_ignoring_group() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("timeout");
    let err = run_process_controlled(
        &spec(fixture.program("claude")),
        fixture.bin_dir(),
        &env(&fixture, "ignore-term", "timeout"),
        DEFAULT_MAX_OUTPUT_BYTES,
        Duration::from_secs(1),
        TEST_TERMINATION_GRACE,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_TIMEOUT");
    let group = wait_group(&marker).await;
    wait_gone(group).await;
}

#[tokio::test]
async fn output_limit_reaps_a_process_that_would_otherwise_hang() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("limit");
    let err = run_process_controlled(
        &spec(fixture.program("codex")),
        fixture.bin_dir(),
        &env(&fixture, "limit-hang", "limit"),
        11,
        Duration::from_secs(5),
        TEST_TERMINATION_GRACE,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_OUTPUT_LIMIT");
    let group = wait_group(&marker).await;
    wait_gone(group).await;
}

#[tokio::test]
async fn cancellation_reaps_the_owned_group() {
    let fixture = FakeSubscriptionCli::install();
    let marker = fixture.marker("cancel");
    let cancel = CancellationToken::new();
    let cancel_signal = cancel.clone();
    let coordinator = async {
        let group = wait_group(&marker).await;
        cancel_signal.cancel();
        group
    };
    let run_spec = spec(fixture.program("codex"));
    let run_env = env(&fixture, "hang", "cancel");
    let run = run_process_controlled(
        &run_spec,
        fixture.bin_dir(),
        &run_env,
        DEFAULT_MAX_OUTPUT_BYTES,
        Duration::from_secs(5),
        TEST_TERMINATION_GRACE,
        cancel,
    );
    let (result, group) = tokio::join!(run, coordinator);
    assert_eq!(result.unwrap_err().reason_code(), "RELAY_CANCELLED");
    wait_gone(group).await;
}

#[tokio::test]
async fn cancelling_one_request_does_not_kill_another_group() {
    let fixture = FakeSubscriptionCli::install();
    let marker_a = fixture.marker("a");
    let marker_b = fixture.marker("b");
    let cancel_a = CancellationToken::new();
    let cancel_b = CancellationToken::new();
    let coordinator = {
        let cancel_a = cancel_a.clone();
        let cancel_b = cancel_b.clone();
        async move {
            let group_a = wait_group(&marker_a).await;
            let group_b = wait_group(&marker_b).await;
            cancel_a.cancel();
            wait_gone(group_a).await;
            assert!(killpg(Pid::from_raw(group_b), None).is_ok());
            cancel_b.cancel();
            group_b
        }
    };
    let env_a = env(&fixture, "hang", "a");
    let env_b = env(&fixture, "hang", "b");
    let spec_a = spec(fixture.program("codex"));
    let spec_b = spec(fixture.program("claude"));
    let run_a = run_process_controlled(
        &spec_a,
        fixture.bin_dir(),
        &env_a,
        DEFAULT_MAX_OUTPUT_BYTES,
        Duration::from_secs(5),
        TEST_TERMINATION_GRACE,
        cancel_a,
    );
    let run_b = run_process_controlled(
        &spec_b,
        fixture.bin_dir(),
        &env_b,
        DEFAULT_MAX_OUTPUT_BYTES,
        Duration::from_secs(5),
        TEST_TERMINATION_GRACE,
        cancel_b,
    );
    let (result_a, result_b, group_b) = tokio::join!(run_a, run_b, coordinator);
    assert_eq!(result_a.unwrap_err().reason_code(), "RELAY_CANCELLED");
    assert_eq!(result_b.unwrap_err().reason_code(), "RELAY_CANCELLED");
    wait_gone(group_b).await;
}
