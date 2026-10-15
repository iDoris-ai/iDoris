#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support {
    pub mod subscription_cli;
}

use std::collections::BTreeMap;
use std::ffi::OsString;

use idoris_upstream::subscription::command::CommandSpec;
use idoris_upstream::subscription::process::{DEFAULT_MAX_OUTPUT_BYTES, run_process};
use support::subscription_cli::FakeSubscriptionCli;

fn env(mode: &str) -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("IDORIS_FAKE_CLI_MODE"), OsString::from(mode)),
    ])
}

fn spec(program: std::path::PathBuf, stdin: Option<String>) -> CommandSpec {
    let leaked: &'static str = Box::leak(program.to_string_lossy().into_owned().into_boxed_str());
    CommandSpec {
        program: leaked,
        args: Vec::new(),
        stdin,
    }
}

#[tokio::test]
async fn echo_preserves_utf8_even_when_input_and_output_cross_chunk_boundaries() {
    let fixture = FakeSubscriptionCli::install();
    let text = "前缀😀".repeat(4097);
    let output = run_process(
        &spec(fixture.program("codex"), Some(text.clone())),
        fixture.bin_dir(),
        &env("echo"),
        DEFAULT_MAX_OUTPUT_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(output.stdout, text);
    assert_eq!(output.exit_code, Some(0));
    assert!(output.process_group > 0);
}

#[tokio::test]
async fn combined_stdout_stderr_limit_counts_bytes_not_characters() {
    let fixture = FakeSubscriptionCli::install();
    let mut environment = env("stderr");
    environment.insert(
        OsString::from("IDORIS_FAKE_CLI_OUTPUT"),
        OsString::from("😀".repeat(3)),
    );
    let err = run_process(
        &spec(fixture.program("claude"), None),
        fixture.bin_dir(),
        &environment,
        11,
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_OUTPUT_LIMIT");
}

#[tokio::test]
async fn combined_limit_is_shared_across_stdout_and_stderr() {
    let fixture = FakeSubscriptionCli::install();
    let err = run_process(
        &spec(fixture.program("claude"), None),
        fixture.bin_dir(),
        &env("split-output"),
        11,
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_OUTPUT_LIMIT");
}

#[tokio::test]
async fn large_stderr_and_large_stdin_are_drained_concurrently_without_deadlock() {
    let fixture = FakeSubscriptionCli::install();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        run_process(
            &spec(fixture.program("codex"), Some("x".repeat(512 * 1024))),
            fixture.bin_dir(),
            &env("pipe-pressure"),
            1024 * 1024,
        ),
    )
    .await
    .expect("parallel pipe handling must not deadlock")
    .unwrap();
    assert_eq!(output.stdout.trim(), "ok");
    assert_eq!(output.stderr.len(), 65_536);
}

#[tokio::test]
async fn spawn_failure_and_closed_stdin_use_fixed_non_leaking_errors() {
    let missing = CommandSpec {
        program: "/definitely/missing/idoris-cli",
        args: vec![],
        stdin: None,
    };
    let err = run_process(&missing, std::path::Path::new("/"), &BTreeMap::new(), 1024)
        .await
        .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_SPAWN_FAILED");

    let fixture = FakeSubscriptionCli::install();
    let err = run_process(
        &spec(fixture.program("codex"), Some("x".repeat(8 * 1024 * 1024))),
        fixture.bin_dir(),
        &env("close-stdin"),
        DEFAULT_MAX_OUTPUT_BYTES,
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason_code(), "RELAY_SPAWN_FAILED");
    assert!(!format!("{err:?}").contains(&"x".repeat(100)));
}
