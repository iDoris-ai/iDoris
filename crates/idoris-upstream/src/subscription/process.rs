use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::command::CommandSpec;
use super::error::{SubscriptionErrorCode, SubscriptionRelayError};
#[cfg(unix)]
use super::reaper::{CompletionReason, ProcessGroupReaper, await_drain_bounded};

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub process_group: u32,
}

#[cfg(not(unix))]
pub async fn run_process(
    _spec: &CommandSpec,
    _cwd: &Path,
    _env: &BTreeMap<OsString, OsString>,
    _max_output_bytes: usize,
) -> Result<ProcessOutput, SubscriptionRelayError> {
    Err(SubscriptionRelayError::new(
        SubscriptionErrorCode::SpawnFailed,
    ))
}

#[cfg(unix)]
pub async fn run_process(
    spec: &CommandSpec,
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    max_output_bytes: usize,
) -> Result<ProcessOutput, SubscriptionRelayError> {
    run_process_controlled(
        spec,
        cwd,
        env,
        max_output_bytes,
        DEFAULT_PROCESS_TIMEOUT,
        DEFAULT_TERMINATION_GRACE,
        CancellationToken::new(),
    )
    .await
}

#[cfg(unix)]
pub async fn run_process_controlled(
    spec: &CommandSpec,
    cwd: &Path,
    env: &BTreeMap<OsString, OsString>,
    max_output_bytes: usize,
    process_timeout: Duration,
    grace: Duration,
    cancel: CancellationToken,
) -> Result<ProcessOutput, SubscriptionRelayError> {
    use std::process::Stdio;

    if cancel.is_cancelled() {
        return Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::Cancelled,
        ));
    }

    let mut command = Command::new(spec.program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let mut child = command
        .spawn()
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    let mut stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    let reaper = ProcessGroupReaper::new(child)?;
    let process_group = u32::try_from(reaper.process_group())
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CleanupFailed))?;

    let total = Arc::new(AtomicUsize::new(0));
    let exceeded = Arc::new(AtomicBool::new(false));
    let output_limit = CancellationToken::new();
    let stdout_task = tokio::spawn(read_limited(
        stdout,
        Arc::clone(&total),
        Arc::clone(&exceeded),
        max_output_bytes,
        output_limit.clone(),
    ));
    let stderr_task = tokio::spawn(read_limited(
        stderr,
        Arc::clone(&total),
        Arc::clone(&exceeded),
        max_output_bytes,
        output_limit.clone(),
    ));

    let stdin_payload = spec.stdin.clone();
    let stdin_task = tokio::spawn(async move {
        if let (Some(mut stdin), Some(payload)) = (stdin.take(), stdin_payload) {
            stdin.write_all(payload.as_bytes()).await.map_err(|_| ())?;
            stdin.shutdown().await.map_err(|_| ())?;
        }
        Ok::<(), ()>(())
    });

    let completion = reaper
        .run_controlled(grace, process_timeout, cancel, output_limit)
        .await?;
    let terminal_error = match completion.reason {
        CompletionReason::Exited => None,
        CompletionReason::Cancelled => Some(SubscriptionRelayError::new(
            SubscriptionErrorCode::Cancelled,
        )),
        CompletionReason::Timeout => {
            Some(SubscriptionRelayError::new(SubscriptionErrorCode::Timeout))
        }
        CompletionReason::OutputLimit => Some(SubscriptionRelayError::new(
            SubscriptionErrorCode::OutputLimit,
        )),
    };
    if terminal_error.is_some() {
        stdin_task.abort();
        let _ = stdin_task.await;
    } else {
        await_drain_bounded(stdin_task, grace)
            .await?
            .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    }
    let (stdout_bytes, stderr_bytes) =
        finish_output_drains(stdout_task, stderr_task, grace, terminal_error).await?;
    let exit_code = completion.reap.exit_code;

    if exceeded.load(Ordering::Acquire) {
        return Err(SubscriptionRelayError::new(
            SubscriptionErrorCode::OutputLimit,
        ));
    }

    let stdout = String::from_utf8(stdout_bytes)
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    let stderr = String::from_utf8(stderr_bytes)
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    Ok(ProcessOutput {
        stdout,
        stderr,
        exit_code,
        process_group,
    })
}

async fn finish_output_drains(
    stdout_task: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>>,
    stderr_task: tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>>,
    grace: Duration,
    terminal_error: Option<SubscriptionRelayError>,
) -> Result<(Vec<u8>, Vec<u8>), SubscriptionRelayError> {
    let (stdout_result, stderr_result) = tokio::join!(
        await_drain_bounded(stdout_task, grace),
        await_drain_bounded(stderr_task, grace),
    );

    if let Some(error) = terminal_error {
        return Err(error);
    }

    let stdout = stdout_result?
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    let stderr = stderr_result?
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    Ok((stdout, stderr))
}

async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    total: Arc<AtomicUsize>,
    exceeded: Arc<AtomicBool>,
    limit: usize,
    output_limit: CancellationToken,
) -> Result<Vec<u8>, std::io::Error> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let previous = total.fetch_add(read, Ordering::AcqRel);
        if previous >= limit {
            exceeded.store(true, Ordering::Release);
            output_limit.cancel();
            continue;
        }
        let keep = read.min(limit - previous);
        kept.extend_from_slice(&chunk[..keep]);
        if keep < read {
            exceeded.store(true, Ordering::Release);
            output_limit.cancel();
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn pending_reader() -> tokio::task::JoinHandle<Result<Vec<u8>, std::io::Error>> {
        tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(Vec::new())
        })
    }

    #[tokio::test]
    async fn terminal_reason_survives_bounded_drain_abort() {
        let error = finish_output_drains(
            pending_reader(),
            pending_reader(),
            Duration::from_millis(1),
            Some(SubscriptionRelayError::new(
                SubscriptionErrorCode::Cancelled,
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(error.reason_code(), "RELAY_CANCELLED");
    }

    #[tokio::test]
    async fn successful_path_still_reports_drain_cleanup_failure() {
        let error = finish_output_drains(
            pending_reader(),
            pending_reader(),
            Duration::from_millis(1),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.reason_code(), "RELAY_CLEANUP_FAILED");
    }
}
