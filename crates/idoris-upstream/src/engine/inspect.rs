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
/// 工具输出的上限，超限按「没有答案」处理；`ps` 的答案是一行（`ps_value` 要求不含换行），`lsof -t` 每个 pid 一行（`listens_on` 逐行比对）。
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

    /// 正在 TCP `port` 上监听（任一地址：回环、通配、IPv6）的全部 pid，升序去重；没有人监听是空列表。
    /// 端口属主校验（[`super::ListenerVerifier`]，#619）用它回答「这个端口上现在是谁」。缺省「读不出来」：
    /// 不实现它的检查器永远通不过属主校验，也就永远不会有 key 发给它面对的端口。
    async fn listeners_on(&self, _port: u16) -> Result<Vec<u32>, InspectError> {
        Err(InspectError::NoAnswer)
    }

    /// 进程的有效 uid（`ps -o uid=`）。缺省「读不出来」。
    async fn uid_of(&self, _pid: u32) -> Result<u32, InspectError> {
        Err(InspectError::NoAnswer)
    }

    /// 进程的「代」标识：平台给的、比 `ps -o lstart=` 的秒更细的启动时刻（Linux：boot_id 加 `/proc/<pid>/stat` 的
    /// starttime 节拍数；macOS：内核记的启动时刻，到微秒），同一个进程每次读到的值相同，pid 被另一个进程复用后读到另一个值。
    /// 防的是「同一秒内 pid 被同一用户的另一个进程复用、又碰巧同命令同组长」。缺省与不支持的平台「读不出来」：记录里就不写，
    /// 那样的记录只有秒级身份。
    async fn generation(&self, _pid: u32) -> Result<String, InspectError> {
        Err(InspectError::NoAnswer)
    }

    /// 进程**正在运行的代码**的签名团队：`codesign --verify <pid>` 通过、再读 `codesign -d --verbose=2 <pid>` 的
    /// `TeamIdentifier`。签名无效、没有签名、ad-hoc 签名（没有团队）都是 `Ok(None)`：它们不是任何团队的代码。
    /// 读 pid 而不读可执行文件路径，是因为进程可以改自己的进程名（oMLX 把自己改名为 `omlx-server`，`ps` 的 `comm`
    /// 因此不是路径），而 `codesign` 给出的是内核认定的、这个进程实际在跑的那份代码。缺省「读不出来」。
    async fn signing_team(&self, _pid: u32) -> Result<Option<String>, InspectError> {
        Err(InspectError::NoAnswer)
    }
}

/// 经 `/bin/ps` 与 `lsof` 读。
#[derive(Debug, Clone)]
pub struct SystemInspector {
    ps: PathBuf,
    /// 按顺序找第一个存在的：macOS 在 `/usr/sbin/lsof`，常见 Linux 在 `/usr/bin/lsof`。
    lsof: Vec<PathBuf>,
    codesign: PathBuf,
    ps_timeout: Duration,
    lsof_timeout: Duration,
    codesign_timeout: Duration,
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
            codesign: PathBuf::from(super::CODESIGN_TOOL),
            ps_timeout: Duration::from_secs(2),
            lsof_timeout: Duration::from_secs(3),
            codesign_timeout: Duration::from_secs(10),
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

    /// 换掉 `codesign` 的路径与时限（测试用：假 `codesign` 脚本）。
    pub fn with_codesign(mut self, tool: impl Into<PathBuf>, timeout: Duration) -> Self {
        self.codesign = tool.into();
        self.codesign_timeout = timeout;
        self
    }

    /// `lsof` 的位置：按顺序找第一个存在的。
    fn lsof_tool(&self) -> Result<&PathBuf, InspectError> {
        self.lsof
            .iter()
            .find(|path| path.is_file())
            .ok_or(InspectError::ToolMissing)
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
        let stdout = output.stdout();
        let text = stdout.trim();
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

    async fn generation(&self, pid: u32) -> Result<String, InspectError> {
        read_generation(pid, self.ps_timeout).await
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
        let lsof = self.lsof_tool()?;
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
                Ok(output.stdout().lines().any(|line| line.trim() == wanted))
            }
            // lsof 自己的「没有匹配」：退出码 1，stdout 与 stderr 都是空的。出错同样退出 1，但会在 stderr 说话。
            Some(1) if output.stdout().trim().is_empty() && output.stderr.trim().is_empty() => {
                Ok(false)
            }
            _ => Err(InspectError::NoAnswer),
        }
    }

    async fn listeners_on(&self, port: u16) -> Result<Vec<u32>, InspectError> {
        let lsof = self.lsof_tool()?;
        let args = ["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"].map(OsString::from);
        let output = run_tool(lsof, &args, self.lsof_timeout).await?;
        match output.code {
            Some(0) => parse_lsof_pids(&output.stdout()),
            // lsof 自己的「没有匹配」：退出码 1，stdout 与 stderr 都是空的（出错同样退出 1，但会在 stderr 说话）。
            Some(1) if output.stdout().trim().is_empty() && output.stderr.trim().is_empty() => {
                Ok(Vec::new())
            }
            _ => Err(InspectError::NoAnswer),
        }
    }

    async fn uid_of(&self, pid: u32) -> Result<u32, InspectError> {
        self.ps_value(pid, "uid")
            .await?
            .parse()
            .map_err(|_| InspectError::NoAnswer)
    }

    async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
        let pid = OsString::from(pid.to_string());
        let verified = run_tool(
            Path::new(&self.codesign),
            &[OsString::from("--verify"), pid.clone()],
            self.codesign_timeout,
        )
        .await?;
        if verified.code != Some(0) {
            // 签名对这个进程不成立（没签名、被改过）：不是任何团队的代码。
            return Ok(None);
        }
        // `codesign -d` 把说明写到 stderr。
        let described = run_tool(
            Path::new(&self.codesign),
            &[OsString::from("-d"), OsString::from("--verbose=2"), pid],
            self.codesign_timeout,
        )
        .await?;
        if described.code != Some(0) {
            return Err(InspectError::NoAnswer);
        }
        Ok(super::discovery::team_identifier(&described.stderr)
            .or_else(|| super::discovery::team_identifier(&described.stdout())))
    }
}

/// `lsof -Fp` 的输出：每个进程一行 `p<pid>`，其余字段行（`f…`、`n…`）忽略；`p` 行不是纯数字就是读不出来。
fn parse_lsof_pids(text: &str) -> Result<Vec<u32>, InspectError> {
    let mut pids = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pids.push(rest.parse::<u32>().map_err(|_| InspectError::NoAnswer)?);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// Upper bound on [`retry_exec_busy`] attempts; with [`EXEC_BUSY_PAUSE`] this caps the wait at ~0.5 s.
const EXEC_BUSY_ATTEMPTS: u32 = 50;
const EXEC_BUSY_PAUSE: Duration = Duration::from_millis(10);

/// Runs `spawn`, retrying only on `ETXTBSY` (`ExecutableFileBusy`) a bounded number of times.
/// A just-written executable can transiently be "text file busy" when another thread forks while
/// the write fd is still open; every other error is returned at once.
pub(super) async fn retry_exec_busy<T>(
    mut spawn: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut attempt = 1;
    loop {
        match spawn() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt < EXEC_BUSY_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(EXEC_BUSY_PAUSE).await;
            }
            result => return result,
        }
    }
}

pub(super) struct ToolOutput {
    pub(super) code: Option<i32>,
    /// 标准输出的原始字节：`sysctl -b kern.proc` 输出的是二进制进程表（macOS 读进程「代」标识用），别的工具输出的是文本，用
    /// [`Self::stdout`] 读。只留这一份，文本是从它派生出来的：每个平台上的每一次读 stdout 都读到它，不靠平台相关的 `cfg`
    /// 去躲「字段从未被读」（Linux 不读进程表，之前这个字段在 Linux 上就是死的，clippy 的 `dead_code` 拦下了）。
    pub(super) stdout_bytes: Vec<u8>,
    pub(super) stderr: String,
}

impl ToolOutput {
    /// 标准输出当文本读：不是合法 UTF-8 的字节按 U+FFFD 替换（与原来的 `String::from_utf8_lossy` 相同）。
    pub(super) fn stdout(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout_bytes)
    }
}

/// 平台给的进程「代」标识（见 [`ProcessInspector::generation`]）。
#[allow(clippy::unused_async)]
async fn read_generation(pid: u32, timeout: Duration) -> Result<String, InspectError> {
    #[cfg(target_os = "linux")]
    {
        let _ = timeout;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| InspectError::NoAnswer)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| InspectError::NoAnswer)?;
        let ticks = parse_proc_stat_starttime(&stat).ok_or(InspectError::NoAnswer)?;
        Ok(format!("linux:{}:{ticks}", boot.trim()))
    }
    #[cfg(target_os = "macos")]
    {
        // `sysctl` 命令行读不了「某个 pid 的 kinfo_proc」（`kern.proc.pid.<pid>` 不是一个能按名字解析的 OID：`unknown oid`），
        // 但 `kern.proc` 本身按名字读得出来：整张进程表，每个进程一条 `struct kinfo_proc`。在里面按 pid 找（没有 unsafe，没有 FFI）。
        let args = [OsString::from("-b"), OsString::from("kern.proc")];
        let output = run_tool_capped(
            Path::new("/usr/sbin/sysctl"),
            &args,
            timeout,
            MAX_KINFO_TABLE_BYTES,
        )
        .await?;
        if output.code != Some(0) {
            return Err(InspectError::NoAnswer);
        }
        parse_kinfo_proc_start(&output.stdout_bytes, pid).ok_or(InspectError::NoAnswer)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, timeout);
        Err(InspectError::NoAnswer)
    }
}

/// `/proc/<pid>/stat` 的第 22 个字段（starttime，开机后的时钟节拍数）。`comm` 里可以有空格与括号，所以从最后一个 `)` 之后数：
/// 之后的字段从第 3 个（state）开始，第 22 个是它的第 20 个。
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat_starttime(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(") ")?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// 进程表的上限：一条 `kinfo_proc` 648 字节，两万个进程也只有十几 MB；超过按「没有答案」处理。
#[cfg(target_os = "macos")]
const MAX_KINFO_TABLE_BYTES: usize = 32 * 1024 * 1024;
/// `struct kinfo_proc` 的大小（macOS 的 x86_64 与 arm64 都是 648 字节）。
#[cfg(any(target_os = "macos", test))]
const KINFO_PROC_SIZE: usize = 648;
/// `p_pid` 在 `kinfo_proc` 里的偏移：`extern_proc` 开头是 16 字节的 `p_un`（里面的 `p_starttime`）、两个 8 字节指针、4 字节 `p_flag`、
/// 1 字节 `p_stat`，对齐到 4 字节之后就是 `p_pid`。
#[cfg(any(target_os = "macos", test))]
const KINFO_PID_OFFSET: usize = 40;

/// `sysctl -b kern.proc` 输出的是整张进程表：一条接一条的 `struct kinfo_proc`（每条 [`KINFO_PROC_SIZE`] 字节，小端）。在里面找 `pid` 那一条，
/// 取它开头的 `p_starttime`（`struct timeval`：8 字节秒加 4 字节微秒）。长度不是整条的倍数、找不到这个 pid（进程已经没了）、值不合理（读错了），
/// 都是「没有答案」。
#[cfg(any(target_os = "macos", test))]
fn parse_kinfo_proc_start(table: &[u8], pid: u32) -> Option<String> {
    if table.is_empty() || !table.len().is_multiple_of(KINFO_PROC_SIZE) {
        return None;
    }
    for record in table.chunks_exact(KINFO_PROC_SIZE) {
        let found = i32::from_le_bytes(
            record
                .get(KINFO_PID_OFFSET..KINFO_PID_OFFSET + 4)?
                .try_into()
                .ok()?,
        );
        if u32::try_from(found).ok() != Some(pid) {
            continue;
        }
        let seconds = i64::from_le_bytes(record.get(0..8)?.try_into().ok()?);
        let micros = i32::from_le_bytes(record.get(8..12)?.try_into().ok()?);
        if !(1_000_000_000..=100_000_000_000).contains(&seconds)
            || !(0..1_000_000).contains(&micros)
        {
            return None;
        }
        return Some(format!("darwin:{seconds}.{micros:06}"));
    }
    None
}

/// Runs a system tool with a cleared environment (`PATH`, `TZ=UTC`, `LC_ALL=C` only), no stdin,
/// piped output (read in full, then rejected if over the cap; the timeout bounds the memory), and a hard timeout (the tool is killed when it expires).
pub(super) async fn run_tool(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
) -> Result<ToolOutput, InspectError> {
    run_tool_capped(program, args, timeout, MAX_TOOL_OUTPUT_BYTES).await
}

/// [`run_tool`] with the output cap given by the caller (the process table of `sysctl -b kern.proc` is far bigger than any one-line answer).
pub(super) async fn run_tool_capped(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
    max_output: usize,
) -> Result<ToolOutput, InspectError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("PATH", TOOL_PATH)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 超时丢弃 future 时一并杀掉工具。
        .kill_on_drop(true);
    let child = retry_exec_busy(|| command.spawn()).await.map_err(|error| {
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
    if output.stdout.len() > max_output || output.stderr.len() > MAX_TOOL_OUTPUT_BYTES {
        return Err(InspectError::NoAnswer);
    }
    Ok(ToolOutput {
        code: output.status.code(),
        stdout_bytes: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The 22nd field of `/proc/<pid>/stat` counted after the last `)`, whatever spaces and parentheses `comm` has.
    #[test]
    fn the_proc_stat_starttime_is_the_twenty_second_field_whatever_comm_looks_like() {
        let stat = "4242 (my (odd) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 3 0 987654321 1000 2 18446744073709551615 1";
        assert_eq!(parse_proc_stat_starttime(stat), Some(987_654_321));
        assert_eq!(parse_proc_stat_starttime("4242 (x) S 1"), None);
        assert_eq!(parse_proc_stat_starttime("garbage"), None);
    }

    /// One 648-byte `kinfo_proc` record: `p_starttime` (8-byte little-endian seconds, 4-byte microseconds) at the start, `p_pid` at offset 40.
    fn kinfo_record(pid: i32, seconds: i64, micros: i32) -> Vec<u8> {
        let mut record = vec![0_u8; KINFO_PROC_SIZE];
        record[0..8].copy_from_slice(&seconds.to_le_bytes());
        record[8..12].copy_from_slice(&micros.to_le_bytes());
        record[KINFO_PID_OFFSET..KINFO_PID_OFFSET + 4].copy_from_slice(&pid.to_le_bytes());
        record
    }

    /// `sysctl -b kern.proc` is a table of `struct kinfo_proc`: the record whose `p_pid` is the asked pid gives the start time with microseconds,
    /// and only that record (another record's start time is never read for this pid). Anything that is not a whole table, no such pid, or an
    /// absurd value is "no answer".
    #[test]
    fn the_kinfo_proc_start_is_found_by_pid_in_the_process_table_with_microseconds() {
        let table = [
            kinfo_record(100, 1_700_000_100, 100_100),
            kinfo_record(200, 1_700_000_200, 123_456),
            kinfo_record(300, 1_700_000_300, 300_300),
        ]
        .concat();
        assert_eq!(
            parse_kinfo_proc_start(&table, 200).as_deref(),
            Some("darwin:1700000200.123456")
        );
        assert_eq!(
            parse_kinfo_proc_start(&table, 100).as_deref(),
            Some("darwin:1700000100.100100")
        );
        assert_eq!(
            parse_kinfo_proc_start(&table, 300).as_deref(),
            Some("darwin:1700000300.300300")
        );
        assert_eq!(parse_kinfo_proc_start(&table, 999), None, "no such pid");
        assert_eq!(parse_kinfo_proc_start(&[], 100), None, "empty table");
        assert_eq!(
            parse_kinfo_proc_start(&table[..table.len() - 1], 100),
            None,
            "not a whole number of records"
        );
        let absurd = kinfo_record(7, 1_700_000_000, 2_000_000);
        assert_eq!(
            parse_kinfo_proc_start(&absurd, 7),
            None,
            "microseconds out of range"
        );
        assert_eq!(
            parse_kinfo_proc_start(&kinfo_record(7, 0, 0), 7),
            None,
            "second 0 is no process"
        );
    }

    /// On a platform that can read it, a live process's generation is stable between reads and another process has another one.
    #[tokio::test]
    async fn the_generation_of_a_live_process_is_stable_and_differs_between_processes() {
        let inspector = SystemInspector::new();
        let me = std::process::id();
        let first = match inspector.generation(me).await {
            Ok(first) => first,
            Err(error) => {
                // macOS and Linux MUST be able to read it (a silently disabled reader would turn the strong identity off): only other
                // platforms may skip.
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                panic!("this platform must be able to read a process generation: {error:?}");
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                {
                    let _ = error;
                    eprintln!(
                        "SKIPPED the_generation_of_a_live_process_is_stable_and_differs_between_processes: this platform has no generation reader"
                    );
                    return;
                }
            }
        };
        assert_eq!(inspector.generation(me).await.unwrap(), first);
        let mut other = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let theirs = inspector.generation(other.id()).await.unwrap();
        let _ = other.kill();
        let _ = other.wait();
        assert_ne!(theirs, first);
    }

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

    fn busy() -> std::io::Error {
        std::io::ErrorKind::ExecutableFileBusy.into()
    }

    #[tokio::test]
    async fn retry_exec_busy_recovers_after_transient_busy() {
        let mut calls = 0;
        let result = retry_exec_busy(|| {
            calls += 1;
            if calls <= 3 { Err(busy()) } else { Ok(calls) }
        })
        .await;
        assert_eq!(result.unwrap(), 4);
    }

    #[tokio::test]
    async fn retry_exec_busy_does_not_retry_other_errors() {
        let mut calls = 0;
        let result: std::io::Result<()> = retry_exec_busy(|| {
            calls += 1;
            Err(std::io::ErrorKind::NotFound.into())
        })
        .await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retry_exec_busy_gives_up_when_always_busy() {
        let mut calls = 0;
        let result: std::io::Result<()> = retry_exec_busy(|| {
            calls += 1;
            Err(busy())
        })
        .await;
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ExecutableFileBusy
        );
        assert_eq!(calls, EXEC_BUSY_ATTEMPTS);
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
    async fn listeners_on_names_every_process_holding_the_port_and_nobody_for_a_free_one() {
        let inspector = SystemInspector::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        match inspector.listeners_on(port).await {
            Ok(pids) => assert_eq!(
                pids,
                [std::process::id()],
                "this process is the one listener"
            ),
            Err(InspectError::ToolMissing) => return, // no lsof on this machine
            Err(error) => panic!("{error}"),
        }
        drop(listener);
        assert_eq!(inspector.listeners_on(port).await, Ok(Vec::new()));
    }

    #[tokio::test]
    async fn the_lsof_listing_is_parsed_strictly() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let listing = |name: &str, body: &str| {
            let lsof = script(dir.path(), name, body);
            with_fake_tools(&ps, lsof)
        };
        // `-Fp`: a `p` line per process, field lines (`f…`, `n…`) in between; sorted, deduplicated.
        let inspector = listing(
            "lsof-two",
            "printf 'p43\\nf3\\nn*:80\\np42\\nf4\\np43\\nf9\\n'",
        );
        assert_eq!(inspector.listeners_on(80).await, Ok(vec![42, 43]));
        // lsof's own "nothing matches": exit 1, no output. An lsof error also exits 1, but it says why.
        let inspector = listing("lsof-nothing", "exit 1");
        assert_eq!(inspector.listeners_on(80).await, Ok(Vec::new()));
        let inspector = listing("lsof-broken", "echo 'lsof: WARNING' >&2; exit 1");
        assert_eq!(
            inspector.listeners_on(80).await,
            Err(InspectError::NoAnswer)
        );
        // A `p` line that is not a number is unreadable, not a pid.
        let inspector = listing("lsof-junk", "printf 'p42\\npabc\\n'");
        assert_eq!(
            inspector.listeners_on(80).await,
            Err(InspectError::NoAnswer)
        );
        // A non-zero status other than the "nothing" shape is unreadable even when it printed a pid.
        let inspector = listing("lsof-two-exit", "printf 'p42\\n'; exit 2");
        assert_eq!(
            inspector.listeners_on(80).await,
            Err(InspectError::NoAnswer)
        );
        let missing = with_fake_tools(&ps, dir.path().join("no-such-lsof"));
        assert_eq!(
            missing.listeners_on(80).await,
            Err(InspectError::ToolMissing)
        );
    }

    #[tokio::test]
    async fn the_lsof_query_names_the_port_and_asks_for_listeners_only() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let seen = dir.path().join("args");
        let lsof = script(
            dir.path(),
            "lsof",
            &format!("echo \"$@\" > '{}'; echo p4242", seen.display()),
        );
        let inspector = with_fake_tools(&ps, lsof);
        assert_eq!(inspector.listeners_on(19731).await, Ok(vec![4242]));
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap().trim(),
            "-nP -iTCP:19731 -sTCP:LISTEN -Fp"
        );
    }

    #[tokio::test]
    async fn uid_of_reads_the_effective_uid_of_a_process_and_refuses_junk() {
        let me = std::process::id();
        let uid = SystemInspector::new().uid_of(me).await.unwrap();
        assert_eq!(uid, rustix::process::geteuid().as_raw());

        let dir = tempfile::tempdir().unwrap();
        let lsof = dir.path().join("lsof");
        for (body, expected) in [
            ("echo 501", Ok(501)),
            ("echo '  0'", Ok(0)),
            ("echo not-a-number", Err(InspectError::NoAnswer)),
            ("echo -1", Err(InspectError::NoAnswer)),
            ("exit 1", Err(InspectError::NoAnswer)),
        ] {
            let ps = script(dir.path(), "ps-uid", body);
            assert_eq!(
                with_fake_tools(&ps, &lsof).uid_of(2).await,
                expected,
                "{body}"
            );
        }
    }

    /// A fake `codesign` that answers `--verify` with `verify_code` and `-d` with `describe` on stderr (as the real one does).
    fn fake_codesign(dir: &Path, verify_code: i32, describe_code: i32, describe: &str) -> PathBuf {
        script(
            dir,
            "codesign",
            &format!(
                "case \"$1\" in\n  --verify) exit {verify_code};;\n  -d) printf '%s\\n' '{describe}' >&2; exit {describe_code};;\nesac\nexit 99"
            ),
        )
    }

    #[tokio::test]
    async fn signing_team_is_the_team_of_the_running_code_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let lsof = dir.path().join("lsof");
        let timeout = Duration::from_secs(60);
        let inspector =
            |codesign: PathBuf| with_fake_tools(&ps, &lsof).with_codesign(codesign, timeout);
        let describe = "Executable=/Applications/oMLX.app/Contents/Resources/Python/bin/python3.11\nIdentifier=python3\nTeamIdentifier=PSK5Q5T46L";
        assert_eq!(
            inspector(fake_codesign(dir.path(), 0, 0, describe))
                .signing_team(42)
                .await,
            Ok(Some("PSK5Q5T46L".to_owned()))
        );
        // The signature does not hold for the running code: it is no team's code, whatever `-d` would say.
        assert_eq!(
            inspector(fake_codesign(dir.path(), 3, 0, describe))
                .signing_team(42)
                .await,
            Ok(None)
        );
        // Signed ad hoc, or without any team line.
        for describe in ["TeamIdentifier=not set", "Identifier=python3"] {
            assert_eq!(
                inspector(fake_codesign(dir.path(), 0, 0, describe))
                    .signing_team(42)
                    .await,
                Ok(None),
                "{describe}"
            );
        }
        // A `-d` that fails is unreadable: not a pass, not a rejection.
        assert_eq!(
            inspector(fake_codesign(dir.path(), 0, 1, describe))
                .signing_team(42)
                .await,
            Err(InspectError::NoAnswer)
        );
        assert_eq!(
            inspector(dir.path().join("no-such-codesign"))
                .signing_team(42)
                .await,
            Err(InspectError::ToolMissing)
        );
        let slow = script(dir.path(), "codesign-slow", "sleep 30");
        assert_eq!(
            with_fake_tools(&ps, &lsof)
                .with_codesign(slow, Duration::from_millis(150))
                .signing_team(42)
                .await,
            Err(InspectError::TimedOut)
        );
    }

    /// The real `codesign` on a running process. This test binary is linker-signed (ad hoc) on macOS, so
    /// it has no team: `Ok(None)`, never a team name. Other platforms have no `codesign`.
    #[tokio::test]
    async fn the_real_codesign_reads_a_running_process_and_an_ad_hoc_one_has_no_team() {
        match SystemInspector::new()
            .signing_team(std::process::id())
            .await
        {
            Ok(team) => assert_eq!(team, None, "a test binary is not signed by any team"),
            // No `codesign` here (not macOS): nothing to read.
            Err(InspectError::ToolMissing) => {}
            // codesign could not describe it on this machine: unreadable is a legitimate answer.
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn codesign_is_asked_about_the_pid_not_about_a_path() {
        // The question is "who is this process", so the argument is the pid itself.
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let seen = dir.path().join("seen");
        let codesign = script(
            dir.path(),
            "codesign",
            &format!(
                "echo \"$@\" >> '{}'\ncase \"$1\" in --verify) exit 0;; *) echo 'TeamIdentifier=ABCDE12345' >&2; exit 0;; esac",
                seen.display()
            ),
        );
        let inspector = with_fake_tools(&ps, dir.path().join("lsof"))
            .with_codesign(codesign, Duration::from_secs(60));
        assert_eq!(
            inspector.signing_team(31337).await,
            Ok(Some("ABCDE12345".to_owned()))
        );
        let lines: Vec<String> = std::fs::read_to_string(&seen)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines, ["--verify 31337", "-d --verbose=2 31337"]);
    }

    /// The raw bytes are the one stored copy of a tool's stdout and the text is read from them (so the field is read on every platform, not
    /// only where a binary table is parsed): non-UTF-8 bytes come out as U+FFFD, exactly as the old `String::from_utf8_lossy` copy did, and the
    /// bytes themselves are untouched for the binary readers.
    #[tokio::test]
    async fn a_tools_stdout_is_kept_as_raw_bytes_and_read_as_lossy_text() {
        let output = run_tool(
            Path::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("printf 'ab\\377cd'")],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(output.code, Some(0));
        assert_eq!(output.stdout_bytes, b"ab\xffcd");
        assert_eq!(output.stdout(), "ab\u{fffd}cd");
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
