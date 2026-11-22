//! 读别的进程的身份（设计稿 §2.7.5、风险 R5）。
//!
//! workspace `unsafe_code = "forbid"`，取别的进程的启动时间、命令行、监听端口，不能裸调 libproc /
//! `proc_pidinfo`，也没有已有依赖的安全封装；设计稿 R5 选的办法是用系统工具，与 Agent24
//! `omlx-kept.ts` 同一套：`kill(pid, 0)` 判存在，`ps -o lstart=` / `command=` / `pgid=`，
//! `lsof` 判某个 pid 是否监听某个端口。不加新依赖。**这一套仍是所有「问一次」的问题的办法**（管理器的状态变化与监督节拍）。
//!
//! 唯一的例外是**每个请求**的那一次确认（IDORIS-CORE S6，Codex 终审 H1 之后的要求：p99 < 5 毫秒、请求路径上不起子进程）：一次
//! `lsof` 要花约 100 毫秒和一个子进程，每个请求都来一次不可接受。那一问换成原生的监视（[`ProcessInspector::watch_listener`]，
//! 在 `idoris-sockprobe` 里：macOS 的 libproc、Linux 的 `/proc`）；libproc 在 `libc` 之外没有安全的封装，所以那个 crate 是 workspace
//! 里**唯一**带 `unsafe` 的地方（一个文件，窄的 `allow`，见它的 crate 文档），别的 crate 一律保持 `forbid`。
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
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use idoris_sockprobe as native;
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

/// 没有原生监视的平台、或原生读不了某个进程时，每个请求的确认退回到那个贵的问题（自有引擎 `lsof`，外部引擎 [`super::ListenerVerifier`]）；
/// 它的**肯定**答案至多重用这么久（IDORIS-CORE S6）：每个请求一次 `lsof` 要花约 100 毫秒和一个子进程，重用把它压成每秒至多四次，
/// 代价是「监听者换了」最多晚这么久才被请求路径看到（管理器的监督节拍照常每个节拍问一次，不重用）。
pub const FALLBACK_TTL: Duration = Duration::from_millis(250);

/// 每个请求的确认（IDORIS-CORE S6）：对**一个**进程的一个端口，问「它此刻还握着这个端口上的一个 LISTEN 套接字吗」。
/// 引擎管理器登记引擎时建（[`ProcessInspector::watch_listener`]），进程的身份在建的那一刻钉死；必须便宜：不起子进程，不扫别的进程。
#[async_trait]
pub trait ListenerWatch: Send + Sync + std::fmt::Debug {
    /// `Ok(true)`：还握着，绑在客户端连得到的地址上（回环地址或通配）。`Ok(false)`：不再握着（进程没了、pid 被回收成了另一个进程、
    /// 或者套接字关了）。`Err`：这一次读不出来（调用方问那个贵的问题）。
    async fn holds(&self) -> Result<bool, InspectError>;
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

    /// 为**每个请求**的那一次确认建一个原生监视（IDORIS-CORE S6）：引擎管理器登记一个引擎时，守卫（[`super::OwnedListenerGuard`]、
    /// [`super::ExternalListenerGuard`]）对它的进程建一个，这一刻就钉死进程的身份（启动时间），之后每个发往这个端点的请求只问
    /// [`ListenerWatch::holds`]：「**这个进程**此刻还握着这个端口上的一个 LISTEN 套接字吗」。它不起子进程、不扫别的进程；「端口上
    /// 是不是只有它一个监听者」那种贵的问法（`lsof`，[`Self::listens_on`]）留给管理器的监督节拍和每次状态变化。
    ///
    /// 缺省没有（`None`）：守卫每个请求问那个贵的问题（测试用的假检查器就这样，它们的回答是编排好的、不花钱）。[`SystemInspector`]
    /// 返回 macOS 的 libproc、Linux 的 `/proc` 做的监视；别的平台、读不了那个进程（进程没了、不是我们的用户）是 `None`。
    fn watch_listener(&self, _pid: u32, _port: u16) -> Option<Arc<dyn ListenerWatch>> {
        None
    }

    /// 守卫在没有原生监视（或它这一次读不出来）时，那个贵的问题的**肯定**答案可以重用多久。缺省 0：每个请求都问（测试里编排好的假
    /// 检查器）；[`SystemInspector`] 是 [`FALLBACK_TTL`]。
    fn fallback_ttl(&self) -> Duration {
        Duration::ZERO
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
    /// 用原生的监视（libproc、`/proc`）做每个请求的确认；`false` 时一律走那个贵的问题，肯定的答案重用至多 [`FALLBACK_TTL`]。
    native_probe: bool,
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
            native_probe: true,
        }
    }

    /// 换掉工具路径（测试用：假 `ps` / `lsof` 脚本）。换了工具就是要用工具：原生的监视关掉（要的话再 [`Self::with_native_probe`]）。
    pub fn with_tools(ps: impl Into<PathBuf>, lsof: impl Into<PathBuf>) -> Self {
        Self {
            ps: ps.into(),
            lsof: vec![lsof.into()],
            native_probe: false,
            ..Self::new()
        }
    }

    /// 开、关每个请求的确认用的原生监视（见 [`ProcessInspector::watch_listener`]）。关掉就是没有原生监视的平台上的样子：每个请求问那个
    /// 贵的问题（自有引擎 `lsof`），肯定的答案重用至多 [`FALLBACK_TTL`]。
    pub fn with_native_probe(mut self, native_probe: bool) -> Self {
        self.native_probe = native_probe;
        self
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

    /// 监听 TCP `port` 的每个套接字（`lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpn`）：进程号与它绑的地址。没有监听者是空列表；读不出来
    /// （工具不存在、超时、出错）是 [`InspectError`]。
    async fn listening_sockets(&self, port: u16) -> Result<Vec<ListeningSocket>, InspectError> {
        let lsof = self.lsof_tool()?;
        let args = ["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpn"].map(OsString::from);
        let output = run_tool(lsof, &args, self.lsof_timeout).await?;
        match output.code {
            Some(0) => parse_lsof_sockets(&output.stdout),
            // lsof 自己的「没有匹配」：退出码 1，stdout 与 stderr 都是空的（出错同样退出 1，但会在 stderr 说话）。
            Some(1) if output.stdout.trim().is_empty() && output.stderr.trim().is_empty() => {
                Ok(Vec::new())
            }
            _ => Err(InspectError::NoAnswer),
        }
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

    /// `pid` 是不是这个端口上**唯一**的监听者（IDORIS-CORE S6，Codex 终审 H1；owner 2026-11-22 拍板）：`lsof` 能看到的、监听这个
    /// TCP 端口的每个套接字（任何地址）都必须是 `pid` 的，并且至少有一个落在目标地址上（托管的端点是 `127.0.0.1`：绑在
    /// `127.0.0.1` 或 IPv4 通配地址上的）。别的进程也在这个端口上监听（哪怕在另一个地址上）、或者目标地址上的那个不是它，都是「不是」：
    /// 之前只问「这个 pid 在某个地址上监听这个端口」，一个绑在 `127.0.0.1:<port>` 上的别的进程与绑在 `*:<port>` 上的引擎并存时，
    /// 连到 `127.0.0.1` 的请求会落在更具体的那一个（别的进程）上。
    ///
    /// `lsof` 只看得到同一个用户的进程：别的用户的进程在这个端口上的监听它看不见，这是这个校验够不着的（规范勘误十，已知限制）。
    async fn listens_on(&self, pid: u32, port: u16) -> Result<bool, InspectError> {
        let sockets = self.listening_sockets(port).await?;
        Ok(is_sole_listener(&sockets, pid))
    }

    async fn listeners_on(&self, port: u16) -> Result<Vec<u32>, InspectError> {
        let lsof = self.lsof_tool()?;
        let args = ["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"].map(OsString::from);
        let output = run_tool(lsof, &args, self.lsof_timeout).await?;
        match output.code {
            Some(0) => parse_lsof_pids(&output.stdout),
            // lsof 自己的「没有匹配」：退出码 1，stdout 与 stderr 都是空的（出错同样退出 1，但会在 stderr 说话）。
            Some(1) if output.stdout.trim().is_empty() && output.stderr.trim().is_empty() => {
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

    fn watch_listener(&self, pid: u32, port: u16) -> Option<Arc<dyn ListenerWatch>> {
        if !self.native_probe {
            return None;
        }
        let watch = native::ListenerWatch::establish(pid, port)?;
        Some(Arc::new(NativeWatch { watch }))
    }

    fn fallback_ttl(&self) -> Duration {
        FALLBACK_TTL
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
            .or_else(|| super::discovery::team_identifier(&described.stdout)))
    }
}

/// 原生的监视：两三次系统调用，没有子进程（macOS 的 libproc、Linux 的 `/proc`）。原生读不了这个进程（权限、意外的错误）是 `Err`：
/// 守卫这一次问那个贵的问题。系统调用是同步的、微秒级，直接在任务里做（不值得 `spawn_blocking`）。
#[derive(Debug)]
struct NativeWatch {
    watch: native::ListenerWatch,
}

#[async_trait]
impl ListenerWatch for NativeWatch {
    async fn holds(&self) -> Result<bool, InspectError> {
        answer_of(self.watch.check())
    }
}

/// 探针的答案到守卫的答案：读不了（`Unavailable`）**不是**「是」，也不是「不是」，是「读不出来」（守卫这一次问那个贵的问题）。
fn answer_of(probe: native::Probe) -> Result<bool, InspectError> {
    match probe {
        native::Probe::Holds => Ok(true),
        native::Probe::NotHolding => Ok(false),
        native::Probe::Unavailable => Err(InspectError::NoAnswer),
    }
}

/// 一个监听中的 TCP 套接字：属于哪个进程、绑在哪个地址上（`lsof -n` 的写法：`127.0.0.1`、`*`（通配）、`[::1]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListeningSocket {
    pub(crate) pid: u32,
    pub(crate) host: String,
}

/// `lsof -Fpn` 的输出：`p<pid>` 开头一个进程，其后每个套接字一行 `n<地址>:<端口>`（夹着 `f<fd>` 行，忽略）。`p` 行不是纯数字、
/// `n` 行出现在任何 `p` 行之前、或者没有 `:<端口>` 的地址，都是读不出来：宁可不认也不猜。
pub(crate) fn parse_lsof_sockets(text: &str) -> Result<Vec<ListeningSocket>, InspectError> {
    let mut sockets = Vec::new();
    let mut current: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current = Some(rest.parse::<u32>().map_err(|_| InspectError::NoAnswer)?);
        } else if let Some(name) = line.strip_prefix('n') {
            let pid = current.ok_or(InspectError::NoAnswer)?;
            let (host, _port) = name.rsplit_once(':').ok_or(InspectError::NoAnswer)?;
            if host.is_empty() {
                return Err(InspectError::NoAnswer);
            }
            sockets.push(ListeningSocket {
                pid,
                host: host.to_owned(),
            });
        }
    }
    Ok(sockets)
}

/// `pid` 是不是这些监听套接字的**唯一**主人，并且至少有一个套接字落在目标地址上（`127.0.0.1` 或 IPv4 通配 `*`）。
pub(crate) fn is_sole_listener(sockets: &[ListeningSocket], pid: u32) -> bool {
    !sockets.is_empty()
        && sockets.iter().all(|socket| socket.pid == pid)
        && sockets
            .iter()
            .any(|socket| matches!(socket.host.as_str(), "127.0.0.1" | "*"))
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
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// Runs a system tool with a cleared environment (`PATH`, `TZ=UTC`, `LC_ALL=C` only), no stdin,
/// piped output (read in full, then rejected if over the cap; the timeout bounds the memory), and a hard timeout (the tool is killed when it expires).
pub(super) async fn run_tool(
    program: &Path,
    args: &[OsString],
    timeout: Duration,
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

        let found = script(
            dir.path(),
            "lsof-found",
            "printf 'p42\\nf3\\nn127.0.0.1:80\\n'",
        );
        let inspector = with_fake_tools(&ps, &found);
        assert_eq!(inspector.listens_on(42, 80).await, Ok(true));
        let someone_else = script(
            dir.path(),
            "lsof-other",
            "printf 'p43\\nf3\\nn127.0.0.1:80\\n'",
        );
        let inspector = with_fake_tools(&ps, &someone_else);
        assert_eq!(inspector.listens_on(42, 80).await, Ok(false));
    }

    /// IDORIS-CORE S6, Codex final review H1 (owner decision): a pid is "the listener" only when it is the **only** one on
    /// the port, and one of its sockets is on the target address. The old question (does this pid listen on that port,
    /// at any address) said yes while another process held `127.0.0.1:<port>` and the engine held `*:<port>`: a
    /// connection to `127.0.0.1` lands on the more specific socket, the other process's.
    #[tokio::test]
    async fn a_pid_is_the_listener_only_when_it_is_the_only_one_and_one_socket_is_on_the_target_address()
     {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let ask = |name: &str, output: &str| {
            let lsof = script(dir.path(), name, &format!("printf '{output}'"));
            with_fake_tools(&ps, lsof)
        };
        // Ours alone: on the loopback address, on the IPv4 wildcard, on both.
        for output in [
            "p42\\nf3\\nn127.0.0.1:80\\n",
            "p42\\nf3\\nn*:80\\n",
            "p42\\nf3\\nn127.0.0.1:80\\nf4\\nn*:80\\n",
        ] {
            assert_eq!(
                ask("sole", output).listens_on(42, 80).await,
                Ok(true),
                "{output}"
            );
        }
        // Someone else listens too: on the same address, on the wildcard while we are on loopback, on another family.
        for output in [
            "p42\\nf3\\nn127.0.0.1:80\\np43\\nf3\\nn127.0.0.1:80\\n",
            "p42\\nf3\\nn*:80\\np43\\nf3\\nn127.0.0.1:80\\n",
            "p42\\nf3\\nn127.0.0.1:80\\np43\\nf3\\nn*:80\\n",
            "p42\\nf3\\nn127.0.0.1:80\\np43\\nf3\\nn[::1]:80\\n",
        ] {
            assert_eq!(
                ask("shared", output).listens_on(42, 80).await,
                Ok(false),
                "{output}"
            );
        }
        // Ours, but not on the target address (IPv6 loopback only): a connection to 127.0.0.1 does not reach it.
        assert_eq!(
            ask("elsewhere", "p42\\nf3\\nn[::1]:80\\n")
                .listens_on(42, 80)
                .await,
            Ok(false)
        );
        // Not ours at all, and nobody at all.
        assert_eq!(
            ask("other", "p43\\nf3\\nn127.0.0.1:80\\n")
                .listens_on(42, 80)
                .await,
            Ok(false)
        );
    }

    #[test]
    fn the_lsof_socket_listing_is_parsed_strictly() {
        assert_eq!(
            parse_lsof_sockets("p42\nf3\nn127.0.0.1:80\nf4\nn[::1]:80\np43\nf5\nn*:80\n"),
            Ok(vec![
                ListeningSocket {
                    pid: 42,
                    host: "127.0.0.1".into()
                },
                ListeningSocket {
                    pid: 42,
                    host: "[::1]".into()
                },
                ListeningSocket {
                    pid: 43,
                    host: "*".into()
                },
            ])
        );
        assert_eq!(parse_lsof_sockets(""), Ok(Vec::new()));
        // A name before any process, a pid that is not a number, an address without a port: unreadable, never guessed.
        for junk in [
            "n127.0.0.1:80\n",
            "pabc\nn127.0.0.1:80\n",
            "p42\nn127.0.0.1\n",
            "p42\nn:80\n",
        ] {
            assert_eq!(
                parse_lsof_sockets(junk),
                Err(InspectError::NoAnswer),
                "{junk:?}"
            );
        }
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

    /// A fake `lsof` that records every invocation (one line per call in `calls.log`) and answers "pid 4242 listens on 127.0.0.1".
    fn counting_lsof(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("calls.log");
        let lsof = script(
            dir,
            "lsof-counting",
            &format!(
                "echo called >> '{}'\nprintf 'p4242\\nn127.0.0.1:19999\\n'",
                log.display()
            ),
        );
        (lsof, log)
    }

    fn calls(log: &Path) -> usize {
        std::fs::read_to_string(log).map_or(0, |text| text.lines().count())
    }

    /// IDORIS-CORE S6 (main session's follow-up): the per-request check starts **no child process**. The inspector is built with a fake
    /// `lsof` that counts its invocations; a watch on a real listener of this process answers every request natively (held while the
    /// listener is open, not held on the very next call once it is closed) and the fake tool is never run.
    #[tokio::test]
    async fn the_per_request_watch_spawns_no_process() {
        let dir = tempfile::tempdir().unwrap();
        let (lsof, log) = counting_lsof(dir.path());
        let ps = script(dir.path(), "ps", "exit 1");
        let inspector = with_fake_tools(ps, lsof).with_native_probe(true);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let watch = inspector.watch_listener(std::process::id(), port).unwrap();
        for _ in 0..50 {
            assert_eq!(watch.holds().await, Ok(true));
        }
        drop(listener);
        assert_eq!(
            watch.holds().await,
            Ok(false),
            "the very next request sees the released port"
        );
        assert_eq!(watch.holds().await, Ok(false));
        assert_eq!(
            calls(&log),
            0,
            "a child process was started on the request path"
        );
    }

    /// With the native probe off (a platform without one) the guard asks the expensive question, `lsof`, and reuses a **yes** for at most
    /// [`FALLBACK_TTL`]: many requests inside the window cost one `lsof`, a request after it asks again, and an `lsof` that cannot answer is
    /// neither reused nor mistaken for a yes.
    #[tokio::test]
    async fn without_the_native_probe_the_guard_asks_lsof_at_most_once_per_window() {
        use crate::engine::OwnedListenerGuard;
        use crate::omlx::key::{EndpointGuard, Verification};

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let answer = dir.path().join("answer");
        std::fs::write(&answer, "p4242\nn127.0.0.1:19999\n").unwrap();
        let lsof = script(
            dir.path(),
            "lsof-scripted",
            &format!(
                "echo called >> '{}'\nif [ -f '{}' ]; then cat '{}'; else exit 2; fi",
                log.display(),
                answer.display(),
                answer.display()
            ),
        );
        let ps = script(dir.path(), "ps", "exit 1");
        let inspector = Arc::new(with_fake_tools(ps, lsof).with_native_probe(false));
        assert!(inspector.watch_listener(4242, 19999).is_none());
        assert_eq!(inspector.fallback_ttl(), FALLBACK_TTL);
        let guard = OwnedListenerGuard::new(inspector, 4242, 19999);
        for _ in 0..20 {
            assert_eq!(guard.verify().await, Verification::Verified);
        }
        assert_eq!(calls(&log), 1, "twenty requests inside the window");
        tokio::time::sleep(FALLBACK_TTL + Duration::from_millis(100)).await;
        assert_eq!(guard.verify().await, Verification::Verified);
        assert_eq!(calls(&log), 2, "a request after the window asks again");
        // The tool stops answering: not reused past its window, not a yes; asked twice (the one retry), and again on the next request.
        std::fs::remove_file(&answer).unwrap();
        tokio::time::sleep(FALLBACK_TTL + Duration::from_millis(100)).await;
        assert_eq!(guard.verify().await, Verification::Unverifiable);
        assert_eq!(calls(&log), 4);
        assert_eq!(guard.verify().await, Verification::Unverifiable);
        assert_eq!(calls(&log), 6);
    }

    /// The probe's three answers and what the guard makes of them: only "holds" is a yes, only "not holding" is a no, and a probe that could
    /// not read the process is neither (the guard asks the expensive question instead).
    #[test]
    fn the_probes_answers_map_to_yes_no_and_unreadable() {
        assert_eq!(answer_of(native::Probe::Holds), Ok(true));
        assert_eq!(answer_of(native::Probe::NotHolding), Ok(false));
        assert_eq!(
            answer_of(native::Probe::Unavailable),
            Err(InspectError::NoAnswer)
        );
    }

    /// The reuse window of the fallback is short: at most a quarter of a second (spec errata ten (8) writes this number down).
    #[test]
    fn the_fallback_reuse_window_is_at_most_a_quarter_of_a_second() {
        assert!(FALLBACK_TTL <= Duration::from_millis(250));
        assert!(FALLBACK_TTL > Duration::ZERO);
    }

    /// No watch is offered for what cannot be watched: the native probe switched off, a pid that is never an engine, a process that is not
    /// there; and the trait's own default (the fakes of the tests) offers none and reuses nothing.
    #[tokio::test]
    async fn no_watch_is_offered_for_what_cannot_be_watched() {
        // Fake tools mean the tools are what is asked: no native watch unless it is asked for.
        assert!(
            SystemInspector::with_tools("/bin/ps", "/usr/sbin/lsof")
                .watch_listener(std::process::id(), 1)
                .is_none()
        );
        let on = SystemInspector::new();
        assert!(on.watch_listener(std::process::id(), 1).is_some());
        assert!(on.watch_listener(0, 1).is_none());
        assert!(on.watch_listener(1, 1).is_none());
        assert!(on.watch_listener(u32::MAX, 1).is_none());
        assert!(
            SystemInspector::new()
                .with_native_probe(false)
                .watch_listener(std::process::id(), 1)
                .is_none()
        );
        // A process that has exited.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(on.watch_listener(pid, 1).is_some());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(on.watch_listener(pid, 1).is_none());
    }

    /// An inspector that does not offer a watch (the default; the fakes of the tests) leaves the guards on the per-request `listens_on`.
    #[tokio::test]
    async fn an_inspector_without_a_watch_offers_none() {
        struct Bare;
        #[async_trait]
        impl ProcessInspector for Bare {
            fn pid_state(&self, _pid: u32) -> PidState {
                PidState::Alive
            }
            async fn start_time(&self, _pid: u32) -> Result<String, InspectError> {
                Err(InspectError::NoAnswer)
            }
            async fn command(&self, _pid: u32) -> Result<String, InspectError> {
                Err(InspectError::NoAnswer)
            }
            async fn process_group(&self, _pid: u32) -> Result<u32, InspectError> {
                Err(InspectError::NoAnswer)
            }
            async fn listens_on(&self, _pid: u32, _port: u16) -> Result<bool, InspectError> {
                Ok(true)
            }
        }
        assert!(Bare.watch_listener(1234, 80).is_none());
        assert_eq!(Bare.fallback_ttl(), Duration::ZERO);
    }
}
