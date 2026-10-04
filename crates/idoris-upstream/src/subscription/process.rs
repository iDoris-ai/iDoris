use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::command::CommandSpec;
use super::error::{SubscriptionErrorCode, SubscriptionRelayError};

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

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
    use std::process::Stdio;

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
    let process_group = child
        .id()
        .ok_or_else(|| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    let mut stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;

    let total = Arc::new(AtomicUsize::new(0));
    let exceeded = Arc::new(AtomicBool::new(false));
    let stdout_task = tokio::spawn(read_limited(
        stdout,
        Arc::clone(&total),
        Arc::clone(&exceeded),
        max_output_bytes,
    ));
    let stderr_task = tokio::spawn(read_limited(
        stderr,
        Arc::clone(&total),
        Arc::clone(&exceeded),
        max_output_bytes,
    ));

    let stdin_payload = spec.stdin.clone();
    let stdin_task = tokio::spawn(async move {
        if let (Some(mut stdin), Some(payload)) = (stdin.take(), stdin_payload) {
            stdin.write_all(payload.as_bytes()).await.map_err(|_| ())?;
            stdin.shutdown().await.map_err(|_| ())?;
        }
        Ok::<(), ()>(())
    });

    let (status, stdin_result, stdout_result, stderr_result) =
        tokio::join!(child.wait(), stdin_task, stdout_task, stderr_task,);
    let status =
        status.map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    stdin_result
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::SpawnFailed))?;
    let stdout_bytes = stdout_result
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;
    let stderr_bytes = stderr_result
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?
        .map_err(|_| SubscriptionRelayError::new(SubscriptionErrorCode::CliFailed))?;

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
        exit_code: status.code(),
        process_group,
    })
}

async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    total: Arc<AtomicUsize>,
    exceeded: Arc<AtomicBool>,
    limit: usize,
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
            continue;
        }
        let keep = read.min(limit - previous);
        kept.extend_from_slice(&chunk[..keep]);
        if keep < read {
            exceeded.store(true, Ordering::Release);
        }
    }
    Ok(kept)
}
