//! 读别的进程的身份（设计稿 §2.7.5、风险 R5）。
//!
//! workspace `unsafe_code = "forbid"`，取别的进程的启动时间、命令行、监听端口，不能裸调 libproc /
//! `proc_pidinfo`，也没有已有依赖的安全封装；设计稿 R5 选的办法是用系统工具，与 Agent24
//! `omlx-kept.ts` 同一套：`kill(pid, 0)` 判存在，`ps -o lstart=` / `command=` / `pgid=`，
//! `lsof` 判某个 pid 是否监听某个端口。不加新依赖。
//!
//! 约定（都照 `omlx-kept.ts`）：
//! - 工具走绝对路径，环境清空，只给 `PATH`、`TZ=UTC`、`LC_ALL=C`：换时区、换语言，同一个进程读出同一段文本；
//!   工具读到什么都不受调用方环境影响。
//! - 读不出来（工具不存在、超时、没有答案）是 [`InspectError`]，**不是**「不一致」：调用方不能把它当成
//!   进程不是我们的而去清记录，也不能当成进程是我们的而去发信号。
//! - 工具有超时，超时即杀；不会把调用方卡住。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

/// 工具的 `PATH`（只用于工具自己找它的辅助程序）。
const TOOL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// 工具输出的上限；`ps` / `lsof -t` 的答案只有一行。
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// `kill(pid, 0)` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidState {
    /// 进程存在，且是这个用户的。
    Alive,
    /// 没有这个进程（ESRCH）。
    Gone,
    /// 存在，但不是这个用户的（EPERM）：不可能是我们拉起的引擎。
    OtherUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InspectError {
    /// 系统工具不存在。
    #[error("process inspection tool is not available")]
    ToolMissing,
    /// 工具没有在期限内答复。
    #[error("process inspection tool timed out")]
    TimedOut,
    /// 工具失败，或没有可用的答案。
    #[error("process inspection tool gave no usable answer")]
    NoAnswer,
}

/// 读一个进程的身份。生产用 [`SystemInspector`]；测试可以换成假的，来表达「读不出来」「读出另一个值」。
#[async_trait]
pub trait ProcessInspector: Send + Sync {
    /// `kill(pid, 0)`。
    fn pid_state(&self, pid: u32) -> PidState;
    /// `ps -o lstart= -p <pid>`（UTC、C locale）的文本，已 trim。
    async fn start_time(&self, pid: u32) -> Result<String, InspectError>;
    /// `ps -o command= -p <pid>` 的文本，已 trim。
    async fn command(&self, pid: u32) -> Result<String, InspectError>;
    /// `ps -o pgid= -p <pid>`。
    async fn process_group(&self, pid: u32) -> Result<u32, InspectError>;
    /// `pid` 是否在 TCP `port` 上监听。
    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError>;
}

/// 经 `/bin/ps` 与 `lsof` 读。
#[derive(Debug, Clone)]
pub struct SystemInspector {
    ps: PathBuf,
    /// 按顺序找第一个存在的：macOS 在 `/usr/sbin/lsof`，常见 Linux 在 `/usr/bin/lsof`。
    lsof: Vec<PathBuf>,
    ps_timeout: Duration,
    lsof_timeout: Duration,
}

impl Default for SystemInspector {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemInspector {
    pub fn new() -> Self {
        Self {
            ps: PathBuf::from("/bin/ps"),
            lsof: vec![
                PathBuf::from("/usr/sbin/lsof"),
                PathBuf::from("/usr/bin/lsof"),
            ],
            ps_timeout: Duration::from_secs(2),
            lsof_timeout: Duration::from_secs(3),
        }
    }

    /// 换掉工具路径（测试用：假 `ps` / `lsof` 脚本）。
    pub fn with_tools(ps: impl Into<PathBuf>, lsof: impl Into<PathBuf>) -> Self {
        Self {
            ps: ps.into(),
            lsof: vec![lsof.into()],
            ..Self::new()
        }
    }

    pub fn with_timeouts(mut self, ps: Duration, lsof: Duration) -> Self {
        self.ps_timeout = ps;
        self.lsof_timeout = lsof;
        self
    }

    async fn ps_value(&self, pid: u32, column: &str) -> Result<String, InspectError> {
        let args = vec![
            OsString::from("-o"),
            OsString::from(format!("{column}=")),
            OsString::from("-p"),
            OsString::from(pid.to_string()),
        ];
        let output = run_tool(&self.ps, &args, self.ps_timeout).await?;
        // 调用方只会在 `pid_state` 说进程在之后才问；这时空答案或失败不是「进程没了」，是读不出来。
        let text = output.stdout.trim();
        if output.code != Some(0) || text.is_empty() || text.contains('\n') {
            return Err(InspectError::NoAnswer);
        }
        Ok(text.to_owned())
    }
}

#[async_trait]
impl ProcessInspector for SystemInspector {
    fn pid_state(&self, pid: u32) -> PidState {
        // pid 0 / 1 / 越界在记录校验处已经被拒；这里再守一次，`kill(0, 0)` 会探测整个进程组。
        let Some(raw) = i32::try_from(pid).ok().filter(|raw| *raw >= 2) else {
            return PidState::Gone;
        };
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None) {
            Ok(()) => PidState::Alive,
            Err(nix::errno::Errno::ESRCH) => PidState::Gone,
            Err(_) => PidState::OtherUser,
        }
    }

    async fn start_time(&self, pid: u32) -> Result<String, InspectError> {
        self.ps_value(pid, "lstart").await
    }

    async fn command(&self, pid: u32) -> Result<String, InspectError> {
        self.ps_value(pid, "command").await
    }

    async fn process_group(&self, pid: u32) -> Result<u32, InspectError> {
        self.ps_value(pid, "pgid")
            .await?
            .parse()
            .map_err(|_| InspectError::NoAnswer)
    }

    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
        let lsof = self
            .lsof
            .iter()
            .find(|path| path.is_file())
            .ok_or(InspectError::ToolMissing)?;
        let args = [
            "-nP",
            "-a",
            "-p",
            &pid.to_string(),
            &format!("-iTCP:{port}"),
            "-sTCP:LISTEN",
            "-t",
        ]
        .map(OsString::from);
        let output = run_tool(lsof, &args, self.lsof_timeout).await?;
        match output.code {
            Some(0) => {
                let wanted = pid.to_string();
                Ok(output.stdout.lines().any(|line| line.trim() == wanted))
            }
            // lsof 自己的「没有匹配」：退出码 1，stdout 与 stderr 都是空的。出错同样退出 1，但会在 stderr 说话。
            Some(1) if output.stdout.trim().is_empty() && output.stderr.trim().is_empty() => {
                Ok(false)
            }
            _ => Err(InspectError::NoAnswer),
        }
    }
}

pub(super) struct ToolOutput {
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// Runs a system tool with a cleared environment (`PATH`, `TZ=UTC`, `LC_ALL=C` only), no stdin,
/// piped and size-capped output, and a hard timeout (the tool is killed when it expires).
pub(super) async fn run_tool(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
) -> Result<ToolOutput, InspectError> {
    let child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", TOOL_PATH)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 超时丢弃 future 时一并杀掉工具。
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                InspectError::ToolMissing
            } else {
                InspectError::NoAnswer
            }
        })?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| InspectError::TimedOut)?
        .map_err(|_| InspectError::NoAnswer)?;
    if output.stdout.len() > MAX_TOOL_OUTPUT_BYTES || output.stderr.len() > MAX_TOOL_OUTPUT_BYTES {
        return Err(InspectError::NoAnswer);
    }
    Ok(ToolOutput {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Fake tools, with timeouts far above anything but a hang. macOS serializes the first
    /// exec of every freshly written file machine-wide (about 0.2 s each), so a tight timeout
    /// would make these tests flaky under load; only the test that is about timeouts shrinks it.
    fn with_fake_tools(ps: impl Into<PathBuf>, lsof: impl Into<PathBuf>) -> SystemInspector {
        SystemInspector::with_tools(ps, lsof)
            .with_timeouts(Duration::from_secs(60), Duration::from_secs(60))
    }

    #[tokio::test]
    async fn reads_this_process_the_way_the_ownership_record_will() {
        let inspector = SystemInspector::new();
        let me = std::process::id();
        assert_eq!(inspector.pid_state(me), PidState::Alive);

        let first = inspector.start_time(me).await.unwrap();
        assert_eq!(first, inspector.start_time(me).await.unwrap(), "stable");
        assert!(first.is_ascii() && first.len() > 10, "{first:?}");
        assert!(!inspector.command(me).await.unwrap().is_empty());
        let own_group = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap();
        assert_eq!(inspector.process_group(me).await.unwrap(), own_group);
    }

    #[tokio::test]
    async fn a_finished_process_is_gone_and_pid_zero_and_one_are_never_probed() {
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let inspector = SystemInspector::new();
        assert_eq!(inspector.pid_state(pid), PidState::Gone);
        for pid in [0, 1, u32::MAX] {
            assert_eq!(inspector.pid_state(pid), PidState::Gone, "{pid}");
        }
    }

    #[tokio::test]
    async fn another_users_process_is_reported_as_such() {
        // Any live process of another user (root's, typically) must read as `OtherUser`
        // (EPERM): it can never be an engine this user spawned. There is no portable
        // pid to name, so look one up; with none visible there is nothing to check.
        let Ok(out) = std::process::Command::new("/bin/ps")
            .args(["-A", "-o", "pid=,uid="])
            .output()
        else {
            return;
        };
        let me = rustix::process::geteuid().as_raw();
        let foreign = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let pid = parts.next()?.parse::<u32>().ok()?;
                let uid = parts.next()?.parse::<u32>().ok()?;
                (uid != me && pid > 1).then_some(pid)
            })
            .next();
        if let Some(pid) = foreign {
            assert_eq!(SystemInspector::new().pid_state(pid), PidState::OtherUser);
        }
    }

    #[tokio::test]
    async fn listens_on_tells_this_process_apart_from_a_port_it_does_not_hold() {
        let inspector = SystemInspector::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        match inspector.listens_on(me, port).await {
            Ok(held) => assert!(held, "this process listens on {port}"),
            Err(InspectError::ToolMissing) => return, // no lsof on this machine
            Err(error) => panic!("{error}"),
        }
        let spare = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let spare_port = spare.local_addr().unwrap().port();
        drop(spare);
        assert!(!inspector.listens_on(me, spare_port).await.unwrap());
        // Another pid on the same port is not a match.
        let parent = std::os::unix::process::parent_id();
        if parent > 1 && parent != me {
            assert!(!inspector.listens_on(parent, port).await.unwrap());
        }
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_an_error_never_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let ok_lsof = script(dir.path(), "lsof", "exit 1");

        let failing_ps = script(dir.path(), "ps-fail", "exit 1");
        let inspector = with_fake_tools(&failing_ps, &ok_lsof);
        assert_eq!(inspector.start_time(2).await, Err(InspectError::NoAnswer));
        assert_eq!(
            inspector.process_group(2).await,
            Err(InspectError::NoAnswer)
        );

        let empty_ps = script(dir.path(), "ps-empty", "exit 0");
        let inspector = with_fake_tools(&empty_ps, &ok_lsof);
        assert_eq!(inspector.command(2).await, Err(InspectError::NoAnswer));

        let junk_ps = script(dir.path(), "ps-junk", "echo not-a-number");
        let inspector = with_fake_tools(&junk_ps, &ok_lsof);
        assert_eq!(
            inspector.process_group(2).await,
            Err(InspectError::NoAnswer)
        );

        let slow_ps = script(dir.path(), "ps-slow", "sleep 30");
        let inspector = with_fake_tools(&slow_ps, &ok_lsof)
            .with_timeouts(Duration::from_millis(150), Duration::from_millis(150));
        assert_eq!(inspector.start_time(2).await, Err(InspectError::TimedOut));

        let inspector = with_fake_tools(dir.path().join("no-such-ps"), &ok_lsof);
        assert_eq!(
            inspector.start_time(2).await,
            Err(InspectError::ToolMissing)
        );
        let inspector = with_fake_tools(&failing_ps, dir.path().join("no-such-lsof"));
        assert_eq!(
            inspector.listens_on(2, 80).await,
            Err(InspectError::ToolMissing)
        );
    }

    #[tokio::test]
    async fn lsof_nothing_found_is_false_but_an_lsof_error_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let nothing = script(dir.path(), "lsof-nothing", "exit 1");
        let inspector = with_fake_tools(&ps, &nothing);
        assert_eq!(inspector.listens_on(42, 80).await, Ok(false));

        let broken = script(
            dir.path(),
            "lsof-broken",
            "echo 'lsof: WARNING' >&2; exit 1",
        );
        let inspector = with_fake_tools(&ps, &broken);
        assert_eq!(
            inspector.listens_on(42, 80).await,
            Err(InspectError::NoAnswer)
        );

        let found = script(dir.path(), "lsof-found", "echo 42");
        let inspector = with_fake_tools(&ps, &found);
        assert_eq!(inspector.listens_on(42, 80).await, Ok(true));
        let someone_else = script(dir.path(), "lsof-other", "echo 43");
        let inspector = with_fake_tools(&ps, &someone_else);
        assert_eq!(inspector.listens_on(42, 80).await, Ok(false));
    }

    #[tokio::test]
    async fn the_tools_run_with_a_clean_fixed_environment() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(
            dir.path(),
            "ps",
            "echo \"$TZ|$LC_ALL|$PATH|$(env | wc -l | tr -d ' ')\"",
        );
        let inspector = with_fake_tools(&ps, dir.path().join("lsof"));
        let seen = inspector.start_time(2).await.unwrap();
        assert!(
            seen.starts_with("UTC|C|/usr/bin:/bin:/usr/sbin:/sbin|"),
            "{seen}"
        );
    }
}
