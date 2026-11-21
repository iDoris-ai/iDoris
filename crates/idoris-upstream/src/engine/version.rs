//! 取引擎版本的通用部分。
//!
//! 版本怎么取是引擎相关的（多数是 `<exe> --version`，也可能是别的参数或别的办法）；
//! 默认实现 [`super::EngineRecipe::version`] 用这里的 [`run_version_command`]，需要别的参数的引擎也可以直接调它。
//! 失败不是致命错误：调用方只是少一个字段（`/health` 的 `engine.version`）。

use std::path::Path;
use std::time::Duration;

use super::launch::ENGINE_PATH;

/// 版本输出最多读这么多字节。
const MAX_OUTPUT_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("the engine executable could not be started")]
    Spawn,
    #[error("the engine executable did not answer in time")]
    TimedOut,
    #[error("the version command exited with an error")]
    Failed,
    #[error("the version command printed something that is not a version")]
    Unparseable,
}

/// 跑 `<executable> <args…>`（环境清空、固定 PATH、限时、输出最多读 4 KiB），取第一行里的版本号。
pub async fn run_version_command(
    executable: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, VersionError> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .env_clear()
        .env("PATH", ENGINE_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // 自己的进程组：超时时整组杀掉（包装脚本可能 fork）。
    command.as_std_mut().process_group(0);
    let mut child = super::inspect::retry_exec_busy(|| command.spawn())
        .await
        .map_err(|_| VersionError::Spawn)?;
    let group = child.id().and_then(|pid| i32::try_from(pid).ok());
    let Some(mut stdout) = child.stdout.take() else {
        return Err(VersionError::Failed);
    };

    let run = async {
        let mut buffer = Vec::new();
        (&mut stdout)
            .take(MAX_OUTPUT_BYTES)
            .read_to_end(&mut buffer)
            .await
            .map_err(|_| VersionError::Failed)?;
        // 关掉读端：输出超过上限的进程下一次写会吃 SIGPIPE，不会卡在满了的管道上等我们。
        drop(stdout);
        let status = child.wait().await.map_err(|_| VersionError::Failed)?;
        if !status.success() {
            return Err(VersionError::Failed);
        }
        Ok(buffer)
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(buffer)) => parse_version(&String::from_utf8_lossy(&buffer)),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            if let Some(group) = group {
                // 我们刚拉起的子进程，pid 即 pgid。
                let _ = crate::local_runtime_process::signal_group(
                    group,
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            Err(VersionError::TimedOut)
        }
    }
}

/// 第一行非空文本里的版本号：`0.6.4`、`omlx 0.6.4`、`v0.6.4` 都取 `0.6.4`。
pub fn parse_version(output: &str) -> Result<String, VersionError> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or(VersionError::Unparseable)?;
    let token = line.rsplit(char::is_whitespace).next().unwrap_or(line);
    let token = token.strip_prefix('v').unwrap_or(token);
    let valid = token.len() <= 64
        && token.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_'));
    if valid {
        Ok(token.to_owned())
    } else {
        Err(VersionError::Unparseable)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    /// Generous: macOS serializes the first exec of every freshly written file machine-wide
    /// (about 0.2 s each), so a tight bound would make these tests flaky under load.
    const PATIENT: Duration = Duration::from_secs(60);

    fn script(dir: &Path, body: &str, mode: u32) -> PathBuf {
        let path = dir.join("engine-cli");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[tokio::test]
    async fn the_version_is_the_first_line_of_the_command_output() {
        let dir = tempfile::tempdir().unwrap();
        for (output, expected) in [
            ("0.6.4", "0.6.4"),
            ("omlx 0.6.4", "0.6.4"),
            ("ollama version is 0.13.5", "0.13.5"),
            ("v0.7.0-dev+1", "0.7.0-dev+1"),
            ("\n  0.6.4  \nmore", "0.6.4"),
        ] {
            let cli = script(
                dir.path(),
                &format!("[ \"$1\" = --version ] && printf '%s\\n' '{output}' || exit 9"),
                0o755,
            );
            assert_eq!(
                run_version_command(&cli, &["--version"], PATIENT)
                    .await
                    .as_deref(),
                Ok(expected),
                "{output:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_arguments_are_the_callers_to_choose() {
        let dir = tempfile::tempdir().unwrap();
        let cli = script(
            dir.path(),
            "[ \"$1 $2\" = 'version -s' ] && echo 2.1.0 || exit 9",
            0o755,
        );
        assert_eq!(
            run_version_command(&cli, &["version", "-s"], PATIENT)
                .await
                .as_deref(),
            Ok("2.1.0")
        );
        assert_eq!(
            run_version_command(&cli, &["--version"], PATIENT).await,
            Err(VersionError::Failed)
        );
    }

    #[tokio::test]
    async fn failures_are_errors_the_caller_can_ignore() {
        let dir = tempfile::tempdir().unwrap();
        let args = ["--version"];
        let cli = script(dir.path(), "exit 2", 0o755);
        assert_eq!(
            run_version_command(&cli, &args, PATIENT).await,
            Err(VersionError::Failed)
        );
        let cli = script(dir.path(), "echo 'Usage: engine'", 0o755);
        assert_eq!(
            run_version_command(&cli, &args, PATIENT).await,
            Err(VersionError::Unparseable)
        );
        let cli = script(dir.path(), "true", 0o755);
        assert_eq!(
            run_version_command(&cli, &args, PATIENT).await,
            Err(VersionError::Unparseable)
        );
        let cli = script(dir.path(), "echo 0.6.4", 0o644);
        assert_eq!(
            run_version_command(&cli, &args, PATIENT).await,
            Err(VersionError::Spawn)
        );
        assert_eq!(
            run_version_command(&dir.path().join("missing"), &args, PATIENT).await,
            Err(VersionError::Spawn)
        );
        let cli = script(dir.path(), "sleep 30", 0o755);
        let started = std::time::Instant::now();
        assert_eq!(
            run_version_command(&cli, &args, Duration::from_millis(200)).await,
            Err(VersionError::TimedOut)
        );
        // The script sleeps for 30 s; the probe came back long before that.
        assert!(started.elapsed() < Duration::from_secs(20));
        // Endless output is read only up to the cap and does not hang the probe: the reader
        // closes the pipe, `yes` dies of SIGPIPE, and a signalled exit is a failed version command.
        let cli = script(dir.path(), "yes", 0o755);
        assert_eq!(
            run_version_command(&cli, &args, PATIENT).await,
            Err(VersionError::Failed)
        );
    }

    #[tokio::test]
    async fn the_command_runs_with_a_cleaned_environment() {
        let dir = tempfile::tempdir().unwrap();
        let cli = script(
            dir.path(),
            "echo \"0.0.$(env | grep -c '^IDORIS_\\|^CARGO\\|^HOME=')\"",
            0o755,
        );
        assert_eq!(
            run_version_command(&cli, &["--version"], PATIENT)
                .await
                .as_deref(),
            Ok("0.0.0")
        );
    }
}
