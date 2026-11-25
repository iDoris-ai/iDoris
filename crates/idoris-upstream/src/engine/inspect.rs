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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use idoris_sockprobe as native;
pub use idoris_sockprobe::{Bind, ExecutableIdentity};
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

/// 托管的 oMLX 引擎被拨的地址：`127.0.0.1`（引擎按配方只监听它，托管卡的 endpoint 只能写它，见 `plan_managed`）。授权结果带着它
/// （[`crate::omlx::key::Authorization::dial_url`]）：发送端一律拨这个地址，不拨卡里写的别名（`localhost`、`[::1]`、`127.0.0.2`），
/// 而守卫验证的就是「登记时认的进程握着一个**这个地址**连得到的 LISTEN 套接字」（Codex 专项评审 H1）。
pub const MANAGED_DIAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// 每个请求的确认（IDORIS-CORE S6）：对**一个**进程的一个端口，问「它此刻还握着这个端口上的一个 LISTEN 套接字吗」。
/// 引擎管理器登记引擎时建（[`ProcessInspector::watch_listener`]），进程的身份在建的那一刻钉死；必须便宜：不起子进程，不扫别的进程。
#[async_trait]
pub trait ListenerWatch: Send + Sync + std::fmt::Debug {
    /// Kernel start time captured when the watch was established.
    fn generation(&self) -> Option<&str> {
        None
    }

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

    /// 监听 TCP `port` 的每个套接字：进程号、它绑的**地址族与地址**（[`Bind`]）。外部引擎的属主校验用它区分「能接这个连接的监听者」
    /// 与「绑在别的地址上、这个连接到不了的监听者」（Codex 专项评审 M13）。缺省由 [`Self::listeners_on`] 的 pid 推出来，每个当作绑在
    /// IPv4 通配地址上（连得到任何 IPv4 地址）：不会读地址的假检查器照旧「端口上的每个 pid 都得合格」。
    async fn listening_sockets(&self, port: u16) -> Result<Vec<ListeningSocket>, InspectError> {
        Ok(self
            .listeners_on(port)
            .await?
            .into_iter()
            .map(ListeningSocket::any)
            .collect())
    }

    /// 进程的有效 uid（`ps -o uid=`）。缺省「读不出来」。
    async fn uid_of(&self, _pid: u32) -> Result<u32, InspectError> {
        Err(InspectError::NoAnswer)
    }

    /// 为**每个请求**的那一次确认建一个原生监视（IDORIS-CORE S6）：引擎管理器登记一个引擎时，守卫（[`super::OwnedListenerGuard`]、
    /// [`super::ExternalListenerGuard`]）对它的进程建一个，这一刻就钉死进程的身份（启动时间；只读这一次，不扫描），之后每个发往这个
    /// 端点的请求只问 [`ListenerWatch::holds`]：「**这个进程**此刻还握着一个**`dial` 这个地址连得到的** LISTEN 套接字吗」。它不起
    /// 子进程、不扫别的进程；「端口上是不是只有它一个监听者」那种贵的问法（`lsof`，[`Self::listens_on`]）留给管理器的监督节拍和每次
    /// 状态变化。
    ///
    /// 缺省没有（`None`）：守卫每个请求问那个贵的问题（测试用的假检查器就这样，它们的回答是编排好的、不花钱）。[`SystemInspector`]
    /// 返回 macOS 的 libproc、Linux 的 `/proc` 做的监视；别的平台、读不了那个进程（进程没了、不是我们的用户）是 `None`。
    fn watch_listener(&self, _pid: u32, _dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
        None
    }

    /// 守卫在没有原生监视（或它这一次读不出来）时，那个贵的问题的**肯定**答案可以重用多久。缺省 0：每个请求都问（测试里编排好的假
    /// 检查器）；[`SystemInspector`] 是 [`FALLBACK_TTL`]。
    fn fallback_ttl(&self) -> Duration {
        Duration::ZERO
    }

    /// 进程的「代」标识：平台给的、比 `ps -o lstart=` 的秒更细的启动时刻（Linux：boot_id 加 `/proc/<pid>/stat` 的
    /// starttime 节拍数；macOS：内核记的启动时刻，到微秒），同一个进程每次读到的值相同，pid 被另一个进程复用后读到另一个值。
    /// 防的是「同一秒内 pid 被同一用户的另一个进程复用、又碰巧同命令同组长」。缺省与不支持的平台「读不出来」：记录里就不写，
    /// 那样的记录只有秒级身份。
    async fn generation(&self, _pid: u32) -> Result<String, InspectError> {
        Err(InspectError::NoAnswer)
    }

    /// Fine-grained start time and executable facts for external listener verification.
    /// An inspector with native identity support must return an error on a failed read.
    fn listener_generation(&self, _pid: u32) -> Result<Option<String>, InspectError> {
        Ok(None)
    }

    fn executable_identity(&self, _pid: u32) -> Result<Option<ExecutableIdentity>, InspectError> {
        Ok(None)
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

    /// 监听 TCP `port` 的每个套接字（`lsof -nP -iTCP:<port> -sTCP:LISTEN -Fpnt`）：进程号、地址族（`t` 行）与它绑的地址。没有监听者是
    /// 空列表；读不出来（工具不存在、超时、出错、看不懂的地址）是 [`InspectError`]。
    async fn lsof_sockets(&self, port: u16) -> Result<Vec<ListeningSocket>, InspectError> {
        let lsof = self.lsof_tool()?;
        let args = ["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpnt"].map(OsString::from);
        let output = run_tool(lsof, &args, self.lsof_timeout).await?;
        match output.code {
            Some(0) => parse_lsof_sockets(&output.stdout()),
            // lsof 自己的「没有匹配」：退出码 1，stdout 与 stderr 都是空的（出错同样退出 1，但会在 stderr 说话）。
            Some(1) if output.stdout().trim().is_empty() && output.stderr.trim().is_empty() => {
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
        match native::process_generation(pid) {
            Some(generation) => Ok(generation),
            None => read_generation(pid, self.ps_timeout).await,
        }
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
        let sockets = self.lsof_sockets(port).await?;
        Ok(is_sole_listener(&sockets, pid))
    }

    async fn listening_sockets(&self, port: u16) -> Result<Vec<ListeningSocket>, InspectError> {
        self.lsof_sockets(port).await
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

    fn watch_listener(&self, pid: u32, dial: SocketAddr) -> Option<Arc<dyn ListenerWatch>> {
        if !self.native_probe {
            return None;
        }
        let watch = native::ListenerWatch::establish(pid, dial.port(), dial.ip())?;
        let generation = watch.generation()?;
        let mut watch = NativeWatch::new(Arc::new(watch));
        watch.generation = Some(generation);
        Some(Arc::new(watch))
    }

    fn fallback_ttl(&self) -> Duration {
        FALLBACK_TTL
    }

    fn listener_generation(&self, pid: u32) -> Result<Option<String>, InspectError> {
        if !self.native_probe {
            return Ok(None);
        }
        native::process_generation(pid)
            .map(Some)
            .ok_or(InspectError::NoAnswer)
    }

    fn executable_identity(&self, pid: u32) -> Result<Option<ExecutableIdentity>, InspectError> {
        if !self.native_probe {
            return Ok(None);
        }
        native::executable_identity(pid)
            .map(Some)
            .ok_or(InspectError::NoAnswer)
    }

    async fn signing_team(&self, pid: u32) -> Result<Option<String>, InspectError> {
        // The kernel's own record of the running image first (macOS): a valid signature and a team identifier, read **without the file on
        // disk**. An engine whose executable was updated, replaced or deleted after it started (and is still running fine) is judged by
        // the code it runs, not refused because `codesign` cannot find the file any more. Where the kernel names no team, the slower
        // path-based tool below judges exactly as before.
        if self.native_probe
            && let Some(team) = native::kernel_signing_team(pid)
        {
            return Ok(Some(team));
        }
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

/// 原生探针（`idoris-sockprobe`）的两个动作。抽成 trait 只为了能用一个**会慢**的假探针测：扫描不占异步执行器的线程、同一个监视的
/// 并发扫描合并成一个（Codex 专项评审 M9）。
trait Native: Send + Sync + std::fmt::Debug + 'static {
    /// 便宜的那一问：几个系统调用，不扫描；要扫描才答得出来是 `None`。
    fn check_cached(&self) -> Option<native::Probe>;
    /// 完整的一问：必要时**扫描**（进程的描述符表，Linux 上还有整张 TCP 表）：阻塞，规模随机器而定。
    fn check(&self) -> native::Probe;
}

impl Native for native::ListenerWatch {
    fn check_cached(&self) -> Option<native::Probe> {
        native::ListenerWatch::check_cached(self)
    }

    fn check(&self) -> native::Probe {
        native::ListenerWatch::check(self)
    }
}

/// 整个进程里同时最多这么多个扫描（它们占阻塞线程池，各读一张随机器规模增长的表）。
const MAX_SCANS: usize = 4;
static SCANS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_SCANS);

/// 原生的监视：便宜的那一问（几个系统调用，没有子进程，直接在任务里做）；要重新定位套接字（还没找到过、描述符换了号或关了）才需要
/// 扫描，那一步在**阻塞线程池**里做（`spawn_blocking`），**同一个监视同一时刻只有一个**扫描（等着的请求在轮到它们时先再问一次便宜
/// 的那一问：前一个扫描已经把答案放进去了），整个进程里同时最多 [`MAX_SCANS`] 个（Codex 专项评审 M9）。原生读不了这个进程（权限、
/// 意外的错误、短读）是 `Err`：守卫这一次问那个贵的问题。
#[derive(Debug)]
struct NativeWatch {
    watch: Arc<dyn Native>,
    generation: Option<String>,
    scanning: tokio::sync::Mutex<()>,
}

impl NativeWatch {
    fn new(watch: Arc<dyn Native>) -> Self {
        Self {
            watch,
            generation: None,
            scanning: tokio::sync::Mutex::new(()),
        }
    }
}

#[async_trait]
impl ListenerWatch for NativeWatch {
    fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }

    async fn holds(&self) -> Result<bool, InspectError> {
        if let Some(probe) = self.watch.check_cached() {
            return answer_of(probe);
        }
        // 套接字要重新定位：一次一个，轮到的时候先看前一个扫描有没有已经找到。
        let _only_one = self.scanning.lock().await;
        if let Some(probe) = self.watch.check_cached() {
            return answer_of(probe);
        }
        // 许可进到阻塞任务里：请求被取消时扫描还在跑，许可跟着它，不会因为取消而超出上限。
        let permit = SCANS.acquire().await.map_err(|_| InspectError::NoAnswer)?;
        let watch = self.watch.clone();
        let probe = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            watch.check()
        })
        .await
        .map_err(|_| InspectError::NoAnswer)?;
        answer_of(probe)
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

/// 一个监听中的 TCP 套接字：属于哪个进程、绑在哪个地址族与地址上（[`Bind`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListeningSocket {
    pub pid: u32,
    pub bind: Bind,
}

impl ListeningSocket {
    /// 绑在 IPv4 通配地址上的套接字：连得到任何 IPv4 地址。不会读地址的检查器（`listeners_on` 只给 pid）用它。
    pub fn any(pid: u32) -> Self {
        Self {
            pid,
            bind: Bind::V4(Ipv4Addr::UNSPECIFIED),
        }
    }

    /// 绑在 `address` 这个 IPv4 地址上的套接字。
    pub fn v4(pid: u32, address: Ipv4Addr) -> Self {
        Self {
            pid,
            bind: Bind::V4(address),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}

/// `lsof -Fpnt` 的输出：`p<pid>` 开头一个进程，其后每个套接字一个文件集：`f<fd>`、`t<IPv4|IPv6>`、`n<地址>:<端口>`。`*`（通配）只有
/// 跟着 `t` 行才知道是哪个地址族；`lsof` 看不出 IPv6 套接字是不是 `IPV6_V6ONLY`，所以 IPv6 通配当作没有双栈的证据（连不到 IPv4 地址）。
/// `p` 行不是纯数字、`n` 行出现在任何 `p` 行之前、没有 `:<端口>` 的地址、看不懂的地址、没有地址族的 `*`，都是读不出来：宁可不认也不猜。
pub(crate) fn parse_lsof_sockets(text: &str) -> Result<Vec<ListeningSocket>, InspectError> {
    let mut sockets = Vec::new();
    let mut current: Option<u32> = None;
    let mut family: Option<Family> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            current = Some(rest.parse::<u32>().map_err(|_| InspectError::NoAnswer)?);
            family = None;
        } else if line.starts_with('f') {
            // 新的文件集：上一个的地址族不带过来。
            family = None;
        } else if let Some(kind) = line.strip_prefix('t') {
            family = match kind {
                "IPv4" => Some(Family::V4),
                "IPv6" => Some(Family::V6),
                _ => None,
            };
        } else if let Some(name) = line.strip_prefix('n') {
            let pid = current.ok_or(InspectError::NoAnswer)?;
            sockets.push(ListeningSocket {
                pid,
                bind: bind_of_lsof_name(name, family)?,
            });
        }
    }
    Ok(sockets)
}

/// `lsof -n` 的地址写法：`127.0.0.1:80`、`*:80`、`[::1]:80`（IPv6 可能带 `%区域`）。
fn bind_of_lsof_name(name: &str, family: Option<Family>) -> Result<Bind, InspectError> {
    let (host, _port) = name.rsplit_once(':').ok_or(InspectError::NoAnswer)?;
    if host.is_empty() {
        return Err(InspectError::NoAnswer);
    }
    if host == "*" {
        return match family {
            Some(Family::V4) => Ok(Bind::V4(Ipv4Addr::UNSPECIFIED)),
            // 没有证据说它也接 IPv4（`IPV6_V6ONLY` 没设）：只算 IPv6 通配。
            Some(Family::V6) => Ok(Bind::v6(Ipv6Addr::UNSPECIFIED, false)),
            None => Err(InspectError::NoAnswer),
        };
    }
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        let address = inner.split('%').next().unwrap_or(inner);
        let address = address
            .parse::<Ipv6Addr>()
            .map_err(|_| InspectError::NoAnswer)?;
        return Ok(Bind::v6(address, false));
    }
    host.parse::<Ipv4Addr>()
        .map(Bind::V4)
        .map_err(|_| InspectError::NoAnswer)
}

/// `pid` 是不是这些监听套接字的**唯一**主人，并且至少有一个套接字是托管的拨号地址（[`MANAGED_DIAL`]）连得到的：绑在 `127.0.0.1` 或
/// IPv4 通配上的（IPv6 套接字只在有双栈证据时算，`lsof` 给不出）。
pub(crate) fn is_sole_listener(sockets: &[ListeningSocket], pid: u32) -> bool {
    !sockets.is_empty()
        && sockets.iter().all(|socket| socket.pid == pid)
        && sockets
            .iter()
            .any(|socket| socket.bind.covers(MANAGED_DIAL))
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

    /// The address a managed engine is dialed at, on `port`.
    fn dial(port: u16) -> SocketAddr {
        SocketAddr::new(MANAGED_DIAL, port)
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
            "p42\\nf3\\ntIPv4\\nn127.0.0.1:80\\n",
            "p42\\nf3\\ntIPv4\\nn*:80\\n",
            "p42\\nf3\\ntIPv4\\nn127.0.0.1:80\\nf4\\ntIPv4\\nn*:80\\n",
        ] {
            assert_eq!(
                ask("sole", output).listens_on(42, 80).await,
                Ok(true),
                "{output}"
            );
        }
        // Someone else listens too: on the same address, on the wildcard while we are on loopback, on another family.
        for output in [
            "p42\\nf3\\ntIPv4\\nn127.0.0.1:80\\np43\\nf3\\ntIPv4\\nn127.0.0.1:80\\n",
            "p42\\nf3\\ntIPv4\\nn*:80\\np43\\nf3\\ntIPv4\\nn127.0.0.1:80\\n",
            "p42\\nf3\\ntIPv4\\nn127.0.0.1:80\\np43\\nf3\\ntIPv4\\nn*:80\\n",
            "p42\\nf3\\ntIPv4\\nn127.0.0.1:80\\np43\\nf3\\ntIPv6\\nn[::1]:80\\n",
        ] {
            assert_eq!(
                ask("shared", output).listens_on(42, 80).await,
                Ok(false),
                "{output}"
            );
        }
        // Ours, but not on the target address (IPv6 loopback only): a connection to 127.0.0.1 does not reach it.
        assert_eq!(
            ask("elsewhere", "p42\\nf3\\ntIPv6\\nn[::1]:80\\n")
                .listens_on(42, 80)
                .await,
            Ok(false)
        );
        // Not ours at all, and nobody at all.
        assert_eq!(
            ask("other", "p43\\nf3\\ntIPv4\\nn127.0.0.1:80\\n")
                .listens_on(42, 80)
                .await,
            Ok(false)
        );
    }

    /// Codex focused review H2 (the `lsof` fallback keeps the address family): an IPv6 wildcard (`*:80` on a `tIPv6` line) is **not** a
    /// socket a connection to `127.0.0.1` can reach: `lsof` cannot see `IPV6_V6ONLY`, so there is no evidence of dual-stack. The old
    /// parser dropped the `t` line and read every `*` as "any address", so an IPv6-only listener on `::` passed for an IPv4 one.
    #[tokio::test]
    async fn an_ipv6_wildcard_listener_does_not_pass_for_an_ipv4_one_in_the_lsof_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let ask = |name: &str, output: &str| {
            let lsof = script(dir.path(), name, &format!("printf '{output}'"));
            with_fake_tools(&ps, lsof)
        };
        // IPv6 wildcard alone: the managed dial address (127.0.0.1) is not reachable through it as far as lsof can tell.
        assert_eq!(
            ask("v6-only", "p42\\nf3\\ntIPv6\\nn*:80\\n")
                .listens_on(42, 80)
                .await,
            Ok(false)
        );
        // ... but an IPv4 socket of the same pid next to it is the target: the pid is the sole owner and one socket covers the dial address.
        assert_eq!(
            ask(
                "v6-and-v4",
                "p42\\nf3\\ntIPv6\\nn*:80\\nf4\\ntIPv4\\nn127.0.0.1:80\\n"
            )
            .listens_on(42, 80)
            .await,
            Ok(true)
        );
        // The same wildcard text with no family line is unreadable, never guessed (the family decides what it covers).
        assert_eq!(
            ask("no-family", "p42\\nf3\\nn*:80\\n")
                .listens_on(42, 80)
                .await,
            Err(InspectError::NoAnswer)
        );
        // The verifier's view of the sockets keeps the family too.
        let sockets = ask(
            "both",
            "p42\\nf3\\ntIPv6\\nn*:80\\np43\\nf4\\ntIPv4\\nn*:80\\n",
        )
        .listening_sockets(80)
        .await
        .unwrap();
        assert_eq!(
            sockets,
            vec![
                ListeningSocket {
                    pid: 42,
                    bind: Bind::v6(Ipv6Addr::UNSPECIFIED, false)
                },
                ListeningSocket {
                    pid: 43,
                    bind: Bind::V4(Ipv4Addr::UNSPECIFIED)
                },
            ]
        );
        assert!(!sockets[0].bind.covers(MANAGED_DIAL));
        assert!(sockets[1].bind.covers(MANAGED_DIAL));
    }

    #[test]
    fn the_lsof_socket_listing_is_parsed_strictly() {
        assert_eq!(
            parse_lsof_sockets(
                "p42\nf3\ntIPv4\nn127.0.0.1:80\nf4\ntIPv6\nn[::1]:80\np43\nf5\ntIPv4\nn*:80\nf6\ntIPv6\nn*:80\n"
            ),
            Ok(vec![
                ListeningSocket::v4(42, Ipv4Addr::LOCALHOST),
                ListeningSocket {
                    pid: 42,
                    bind: Bind::v6(Ipv6Addr::LOCALHOST, false)
                },
                ListeningSocket::any(43),
                ListeningSocket {
                    pid: 43,
                    bind: Bind::v6(Ipv6Addr::UNSPECIFIED, false)
                },
            ])
        );
        // The address of a specific socket is read even without a family line; an IPv6 zone id is dropped; an IPv4-mapped IPv6 address is the
        // IPv4 one it maps to (the socket is reachable at that IPv4 address).
        assert_eq!(
            parse_lsof_sockets("p42\nf3\nn[fe80::1%lo0]:80\nf4\nn[::ffff:127.0.0.1]:80\n"),
            Ok(vec![
                ListeningSocket {
                    pid: 42,
                    bind: Bind::v6("fe80::1".parse().unwrap(), false)
                },
                ListeningSocket::v4(42, Ipv4Addr::LOCALHOST),
            ])
        );
        assert_eq!(parse_lsof_sockets(""), Ok(Vec::new()));
        // A name before any process, a pid that is not a number, an address without a port, an empty host, a host that is no address, a
        // wildcard with no (or an unknown) family: unreadable, never guessed.
        for junk in [
            "n127.0.0.1:80\n",
            "pabc\nn127.0.0.1:80\n",
            "p42\nn127.0.0.1\n",
            "p42\nn:80\n",
            "p42\nn*:80\n",
            "p42\nf3\ntunix\nn*:80\n",
            "p42\nn[not-an-address]:80\n",
            "p42\nnexample.com:80\n",
        ] {
            assert_eq!(
                parse_lsof_sockets(junk),
                Err(InspectError::NoAnswer),
                "{junk:?}"
            );
        }
        // The family of one file set does not leak into the next: `*` after a new `f` line has no family any more.
        assert_eq!(
            parse_lsof_sockets("p42\nf3\ntIPv4\nn127.0.0.1:80\nf4\nn*:80\n"),
            Err(InspectError::NoAnswer)
        );
    }

    #[tokio::test]
    async fn the_socket_listing_query_asks_for_the_family_and_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let ps = script(dir.path(), "ps", "exit 1");
        let seen = dir.path().join("args");
        let lsof = script(
            dir.path(),
            "lsof",
            &format!(
                "echo \"$@\" > '{}'; printf 'p4242\\nf3\\ntIPv4\\nn127.0.0.1:19731\\n'",
                seen.display()
            ),
        );
        let inspector = with_fake_tools(&ps, lsof);
        assert_eq!(
            inspector.listening_sockets(19731).await,
            Ok(vec![ListeningSocket::v4(4242, Ipv4Addr::LOCALHOST)])
        );
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap().trim(),
            "-nP -iTCP:19731 -sTCP:LISTEN -Fpnt"
        );
    }

    /// The trait's default `listening_sockets` (an inspector that only knows pids) reads every pid as an IPv4 wildcard listener: the old
    /// "every pid on the port must qualify" behaviour for the fakes that cannot read addresses.
    #[tokio::test]
    async fn the_default_socket_listing_reads_every_pid_as_an_ipv4_wildcard_listener() {
        struct Pids;
        #[async_trait]
        impl ProcessInspector for Pids {
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
                Err(InspectError::NoAnswer)
            }
            async fn listeners_on(&self, _port: u16) -> Result<Vec<u32>, InspectError> {
                Ok(vec![7, 9])
            }
        }
        assert_eq!(
            Pids.listening_sockets(80).await,
            Ok(vec![ListeningSocket::any(7), ListeningSocket::any(9)])
        );
        assert!(ListeningSocket::any(7).bind.covers(MANAGED_DIAL));
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
        let watch = inspector
            .watch_listener(std::process::id(), dial(port))
            .unwrap();
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
        let ps = script(dir.path(), "ps", "echo start");
        let inspector = Arc::new(with_fake_tools(ps, lsof).with_native_probe(false));
        assert!(inspector.watch_listener(4242, dial(19999)).is_none());
        assert_eq!(inspector.fallback_ttl(), FALLBACK_TTL);
        // The window of a yes starts when the check **starts**: warm the tool up first (the first `exec` of a freshly written file is slow
        // on macOS, slower than the whole window), then count from zero.
        assert_eq!(inspector.listens_on(4242, 19999).await, Ok(true));
        assert_eq!(inspector.start_time(4242).await, Ok("start".to_owned()));
        std::fs::remove_file(&log).unwrap();
        let guard = OwnedListenerGuard::new(inspector, 4242, dial(19999), "start".into(), None);
        // Every request also re-confirms the start time (`ps`, a subprocess here: the guard has no generation), which takes a few
        // milliseconds each on a busy machine: send requests only while well inside the window, not a fixed twenty.
        let window_start = std::time::Instant::now();
        let mut sent = 0;
        while sent < 20 && window_start.elapsed() < FALLBACK_TTL / 2 {
            assert_eq!(guard.verify().await, Verification::Verified);
            sent += 1;
        }
        assert!(sent >= 2, "too slow a machine to test a window: {sent}");
        assert_eq!(calls(&log), 1, "{sent} requests inside the window");
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
                .watch_listener(std::process::id(), dial(1))
                .is_none()
        );
        let on = SystemInspector::new();
        assert!(on.watch_listener(std::process::id(), dial(1)).is_some());
        assert!(on.watch_listener(0, dial(1)).is_none());
        assert!(on.watch_listener(1, dial(1)).is_none());
        assert!(on.watch_listener(u32::MAX, dial(1)).is_none());
        assert!(
            SystemInspector::new()
                .with_native_probe(false)
                .watch_listener(std::process::id(), dial(1))
                .is_none()
        );
        // A process that has exited.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(on.watch_listener(pid, dial(1)).is_some());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(on.watch_listener(pid, dial(1)).is_none());
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
        assert!(Bare.watch_listener(1234, dial(80)).is_none());
        assert_eq!(Bare.fallback_ttl(), Duration::ZERO);
    }

    // ----- the native watch off the executor (Codex focused review M9) ---------------------------------------------------------------

    /// A native probe whose scan **blocks** (a stand-in for a big descriptor table) until the test lets it go, and counts how many scans
    /// run, how many are running at once, and how often the cheap question is asked.
    #[derive(Debug)]
    struct SlowNative {
        /// What the cheap question answers until a scan has run, then what the scan found.
        cached: std::sync::Mutex<Option<native::Probe>>,
        scans: std::sync::atomic::AtomicUsize,
        running: std::sync::atomic::AtomicUsize,
        most_running: std::sync::atomic::AtomicUsize,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        entered: std::sync::mpsc::SyncSender<()>,
        answer: native::Probe,
    }

    impl Native for SlowNative {
        fn check_cached(&self) -> Option<native::Probe> {
            *self.cached.lock().unwrap()
        }

        fn check(&self) -> native::Probe {
            use std::sync::atomic::Ordering::SeqCst;
            self.scans.fetch_add(1, SeqCst);
            let now = self.running.fetch_add(1, SeqCst) + 1;
            self.most_running.fetch_max(now, SeqCst);
            let _ = self.entered.send(());
            // Wait for the test to let this scan finish.
            let _ = self.release.lock().unwrap().recv();
            *self.cached.lock().unwrap() = Some(self.answer);
            self.running.fetch_sub(1, SeqCst);
            self.answer
        }
    }

    /// Lets the blocked scans go. Dropping it releases every scan still waiting, so a test that fails half way does not leave a thread of
    /// the blocking pool hanging the shutdown of its runtime.
    struct Releaser(std::sync::mpsc::Sender<()>);

    impl Releaser {
        fn one(&self) {
            let _ = self.0.send(());
        }

        fn sender(&self) -> std::sync::mpsc::Sender<()> {
            self.0.clone()
        }
    }

    impl Drop for Releaser {
        fn drop(&mut self) {
            for _ in 0..64 {
                let _ = self.0.send(());
            }
        }
    }

    fn slow_native(
        answer: native::Probe,
    ) -> (Arc<SlowNative>, Releaser, std::sync::mpsc::Receiver<()>) {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(64);
        (
            Arc::new(SlowNative {
                cached: std::sync::Mutex::new(None),
                scans: std::sync::atomic::AtomicUsize::new(0),
                running: std::sync::atomic::AtomicUsize::new(0),
                most_running: std::sync::atomic::AtomicUsize::new(0),
                release: std::sync::Mutex::new(release_rx),
                entered: entered_tx,
                answer,
            }),
            Releaser(release_tx),
            entered_rx,
        )
    }

    /// **A scan runs off the executor.** While a request's scan is blocked, the single worker thread of the runtime keeps serving other
    /// tasks (a timer fires, another task makes progress); the old code scanned inside the request's own task and would have stalled it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_scan_does_not_block_the_executor() {
        let (native, release, entered) = slow_native(native::Probe::Holds);
        // A safety net: a scan that blocks the executor would hang this test; let it go after a while so the test fails instead.
        let safety = release.sender();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            let _ = safety.send(());
        });
        let watch = Arc::new(NativeWatch::new(native.clone()));
        let asked = {
            let watch = watch.clone();
            tokio::spawn(async move { watch.holds().await })
        };
        // The scan is running (blocked) on the blocking pool ...
        tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        // ... and this single-threaded runtime is still alive: a timer fires and another task completes while the scan is blocked.
        let side = tokio::spawn(async { 7_u32 });
        let started = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the runtime was blocked by the scan for {:?}",
            started.elapsed()
        );
        assert_eq!(side.await.unwrap(), 7);
        assert!(
            !asked.is_finished(),
            "the request is still waiting for its scan"
        );
        release.one();
        assert_eq!(asked.await.unwrap(), Ok(true));
    }

    /// **One scan per watch at a time.** Many concurrent requests that find the socket needs locating share one scan: the ones that wait
    /// find the answer the first scan left behind (the cheap question answers from then on) and never scan themselves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_on_one_watch_share_one_scan() {
        let (native, release, entered) = slow_native(native::Probe::Holds);
        // A safety net: scans that are not merged would wait for releases that never come; let them all go after a while so the assertion
        // below fails instead of the test hanging.
        let safety = release.sender();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            for _ in 0..64 {
                let _ = safety.send(());
            }
        });
        let watch = Arc::new(NativeWatch::new(native.clone()));
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let watch = watch.clone();
                tokio::spawn(async move { watch.holds().await })
            })
            .collect();
        tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        // Give the other fifteen time to queue up behind the scan.
        tokio::time::sleep(Duration::from_millis(200)).await;
        release.one();
        for task in tasks {
            assert_eq!(task.await.unwrap(), Ok(true));
        }
        assert_eq!(
            native.scans.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "sixteen requests, one scan"
        );
    }

    /// **The cheap hit does not queue behind a scan.** A request whose cheap question can be answered returns at once even while another
    /// request's scan of the same watch is blocked (the lock that merges scans is only for the requests that have to scan).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cheap_hit_does_not_wait_behind_a_scan_of_the_same_watch() {
        let (native, release, entered) = slow_native(native::Probe::Holds);
        let watch = Arc::new(NativeWatch::new(native.clone()));
        let scanning = {
            let watch = watch.clone();
            tokio::spawn(async move { watch.holds().await })
        };
        tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)).unwrap())
            .await
            .unwrap();
        // the scan is blocked inside the system calls; meanwhile the cheap question can answer
        *native.cached.lock().unwrap() = Some(native::Probe::Holds);
        let answer = tokio::time::timeout(Duration::from_secs(2), watch.holds()).await;
        assert_eq!(
            answer.unwrap(),
            Ok(true),
            "the cheap hit waited for the scan that was in flight"
        );
        release.one();
        assert_eq!(scanning.await.unwrap(), Ok(true));
    }

    /// **The scans of the whole process are bounded** ([`MAX_SCANS`]): more watches than the bound scanning at once leave the rest waiting
    /// for a permit; none of them gives up or answers wrongly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_scans_of_all_watches_are_bounded_process_wide() {
        let total = MAX_SCANS + 3;
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release = Releaser(release_tx);
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        /// Many watches, one shared gate: they all block in `check` until the test sends a release for each.
        #[derive(Debug)]
        struct Gate {
            release: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<()>>>,
            running: Arc<std::sync::atomic::AtomicUsize>,
            most: Arc<std::sync::atomic::AtomicUsize>,
            cached: std::sync::Mutex<Option<native::Probe>>,
        }
        impl Native for Gate {
            fn check_cached(&self) -> Option<native::Probe> {
                *self.cached.lock().unwrap()
            }
            fn check(&self) -> native::Probe {
                use std::sync::atomic::Ordering::SeqCst;
                let now = self.running.fetch_add(1, SeqCst) + 1;
                self.most.fetch_max(now, SeqCst);
                let _ = self.release.lock().unwrap().recv();
                self.running.fetch_sub(1, SeqCst);
                *self.cached.lock().unwrap() = Some(native::Probe::Holds);
                native::Probe::Holds
            }
        }

        let watches: Vec<_> = (0..total)
            .map(|_| {
                Arc::new(NativeWatch::new(Arc::new(Gate {
                    release: release_rx.clone(),
                    running: running.clone(),
                    most: most.clone(),
                    cached: std::sync::Mutex::new(None),
                })))
            })
            .collect();
        let tasks: Vec<_> = watches
            .iter()
            .map(|watch| {
                let watch = watch.clone();
                tokio::spawn(async move { watch.holds().await })
            })
            .collect();
        // Let the scans pile up against the bound.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            most.load(std::sync::atomic::Ordering::SeqCst) <= MAX_SCANS,
            "{} scans ran at once, the bound is {MAX_SCANS}",
            most.load(std::sync::atomic::Ordering::SeqCst)
        );
        // Let them all through, one by one; every request is answered.
        for _ in 0..total {
            release.one();
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), Ok(true));
        }
        assert!(most.load(std::sync::atomic::Ordering::SeqCst) <= MAX_SCANS);
    }

    /// A scan that finds the process gone or the socket closed is a **no**; one that could not read the process is "cannot answer"
    /// (the guard asks the expensive question), never a yes and never a refusal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scan_that_finds_nothing_is_a_no_and_one_that_cannot_read_is_no_answer() {
        for (found, expected) in [
            (native::Probe::NotHolding, Ok(false)),
            (native::Probe::Unavailable, Err(InspectError::NoAnswer)),
            (native::Probe::Holds, Ok(true)),
        ] {
            let (native, release, _entered) = slow_native(found);
            release.one();
            let watch = NativeWatch::new(native);
            assert_eq!(watch.holds().await, expected);
        }
    }
}
