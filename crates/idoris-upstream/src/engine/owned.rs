//! 自己拥有的引擎：拉起、封印、认回孤儿、停止（设计稿 §2.7.4、§2.7.5）。
//!
//! [`OwnedEngine`] 是「iDoris 拥有的一个本地推理引擎进程」，不论是哪种引擎（引擎相关的部分都在
//! [`super::EngineRecipe`] 里，这里只在 [`OwnedEngine::wait_ready`] 里经它判就绪）。它有两种来历：本进程刚
//! [`OwnedEngine::spawn`] 的，或上一个 iDoris 留下的孤儿经 [`OwnedEngine::recover`] 认回的。外部引擎
//! （用户自己的 oMLX.app、别人的服务）没有对应的类型：本模块里发信号的函数都是私有的，入口只有 `spawn`（自己拉起
//! 的子进程）与 `recover`（核对过身份的记录）。这靠模块私有性，不是类型系统的保证（`signal_group` 在 crate 内可见）。
//!
//! # 所有权记录的两个阶段
//!
//! 引擎的命令行可能要等 `exec` 链和进程改名结束才稳定（例如 oMLX：`omlx-cli` 是 shell 脚本，`exec` 成
//! python，再被 `setproctitle` 改成 `omlx-server`，设计稿附录 A），所以：
//! 1. **拉起后立刻**写一份「未封印」的记录（`command: None`）：`engine_kind`、pid、启动时间、端口、可执行文件、key。
//!    iDoris 在引擎启动的那段时间里被 kill -9，新 iDoris 也找得回它（只能停掉重拉：它还没有加载任何模型）。
//! 2. **就绪后** [`OwnedEngine::seal`]：读命令行、确认监听这个端口的就是这个 pid，写入记录。之后记录才能
//!    被**接管**（[`Recovery::Adopted`]）。
//!
//! # 认回孤儿的身份判据
//!
//! 照 Agent24 `omlx-kept.ts`：pid、启动时间、命令、监听端口四项**全部**吻合才算同一个进程；再加进程组：
//! 引擎是自己那个组的组长（拉起时 `process_group(0)`，组 id 就是 pid，记录里不另存），整组信号只发给这个
//! pid 的组，所以要确认它此刻仍领着这个组。这些判据与引擎种类无关。
//! - 任何一项**不一致**（进程没了、pid 被复用、命令或端口不同）：记录作废，**不发任何信号**。
//! - 任何一项**读不出来**（`ps` / `lsof` 超时、失败）：**不是**不一致：不发信号，记录保留，等下一次。
//!
//! 记录按「引擎种类 → endpoint」存放（规范 §2.7.6，见 [`super::EngineInstance`]）：一个种类的 `recover`
//! 只会读到自己目录里、自己 endpoint 的那一份，别的种类的记录它看不见，更不会对它发信号。文件里的
//! `engine_kind` 或端口与所在位置对不上（拷来的、放错目录的）按作废处理（[`MismatchReason::InvalidRecord`]）。
//!
//! # 停止
//!
//! SIGTERM 整组，宽限期内等整组消失，仍在就 SIGKILL 整组，再等到消失才算完成；完成后删记录。
//! 对自己 spawn 的引擎走 [`ManagedRuntimeProcess::shutdown`]（未回收的直接子进程占着 PGID，发信号时这个组 id 不会已被
//! 复用）；对认回的孤儿，发 SIGTERM 前先过身份闸（pid 存活、启动时间吻合、仍是自己进程组的组长），闸与第一次信号之间有
//! 微秒级的非原子窗口（没有 pidfd），误伤要求这个 pid 恰在窗口内被复用并成为另一个进程组的组长。之后组里还有活成员时，
//! 组 id 不会被重新分配（成员检查与 SIGKILL 之间同样有这个窗口），再发 SIGKILL 前不重新核对 leader（它可能已经先退出了）。
//!
//! # Drop
//!
//! 自己 spawn 的引擎：`stop_on_exit: Owned` 时 Drop 杀整组（B8 的既有行为），`Never` 时什么都不做，
//! 引擎继续跑，记录留给下一个 iDoris。认回的孤儿：Drop 什么都不做；停止要显式调用 [`OwnedEngine::stop`]
//! （设计稿 §2.6 关机顺序第 2 步）。

use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::Signal;

use super::inspect::{PidState, ProcessInspector};
use super::instance::EngineInstance;
use super::key::EngineKey;
use super::launch::{EngineLaunch, StopOnExit};
use super::probe::EngineProbe;
use super::record::{OwnershipRecord, OwnershipStore, RecordError};
use crate::local_runtime_process::{
    EngineSpawn, ManagedRuntimeProcess, group_has_live_members, signal_group,
};

/// 停止引擎的缺省宽限：SIGTERM 之后等整组消失，到点还在就 SIGKILL 整组（规范 §2.7.6「进程组」：8 秒）。
/// [`OwnedEngine::stop`] 的宽限由调用方给；这是调用方（S5 的管理器）该给的缺省值。
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(8);
/// 轮询进程组消失 / 引擎退出的间隔。
const POLL: Duration = Duration::from_millis(25);
/// 拉起后读不到身份时，收拾刚拉起的引擎给的宽限。
const ABORT_GRACE: Duration = Duration::from_millis(500);
/// 拉起后读启动时间的重试次数与间隔（`ps` 偶发失败不该让一次拉起白费）。
const IDENTITY_ATTEMPTS: u32 = 3;
const IDENTITY_RETRY: Duration = Duration::from_millis(50);

/// 记录与进程不一致的原因（不发信号，记录作废）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchReason {
    /// 没有这个进程了。
    ProcessGone,
    /// pid 现在是别的用户的进程。
    OtherUser,
    /// 启动时间不同：pid 被复用了。
    StartTime,
    /// 这个 pid 不再是它自己那个进程组的组长。
    ProcessGroup,
    /// 命令行不同。
    Command,
    /// 这个 pid 没有在记录里的端口上监听。
    Port,
    /// 记录本身不可用（内容坏、种类或端口与所在位置对不上）。
    InvalidRecord,
}

impl MismatchReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ProcessGone => "process_gone",
            Self::OtherUser => "other_user",
            Self::StartTime => "start_time",
            Self::ProcessGroup => "process_group",
            Self::Command => "command",
            Self::Port => "port",
            Self::InvalidRecord => "invalid_record",
        }
    }
}

/// 身份的哪一项读不出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreadableReason {
    StartTime,
    ProcessGroup,
    Command,
    Port,
}

impl UnreadableReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::StartTime => "start_time",
            Self::ProcessGroup => "process_group",
            Self::Command => "command",
            Self::Port => "port",
        }
    }
}

/// [`verify_identity`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityVerdict {
    /// pid、启动时间、命令、监听端口（及进程组）全部吻合：就是记录里的那个进程。
    Match,
    /// 记录还没封印（没有命令）：pid、启动时间、进程组吻合，命令与端口无从核对。
    Unsealed,
    /// 有一项不一致（或进程没了）：不是记录里的那个进程。
    Mismatch(MismatchReason),
    /// 有一项读不出来：既不能说是，也不能说不是。
    Unreadable(UnreadableReason),
}

/// 引擎相关操作的失败。每个都有稳定的 [`EngineError::reason_code`]；错误信息里没有 key、没有记录内容。
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine log is unavailable: {0:?}")]
    Log(std::io::ErrorKind),
    #[error("failed to spawn the engine: {0:?}")]
    Spawn(std::io::ErrorKind),
    #[error("the engine exited before it was ready")]
    ExitedEarly,
    /// 刚拉起的引擎读不出启动时间：写不出一份能认回的记录，宁可不留一个认不回的引擎。
    #[error("the identity of the freshly spawned engine could not be read")]
    IdentityUnavailable,
    #[error("ownership record: {0}")]
    Record(#[from] RecordError),
    /// 这个实例已有记录，而它描述的引擎可能还活着：先 `recover` 再决定，不要覆盖它。
    #[error("an ownership record for this engine describes a process that may still be running")]
    AlreadyOwned,
    #[error("a part of the engine's identity could not be read ({})", .0.code())]
    IdentityUnreadable(UnreadableReason),
    #[error("the engine did not become ready in time (last probe: {last:?})")]
    NotReady { last: Option<EngineProbe> },
    /// 端口上答话的不是这个引擎进程。
    #[error("the port is not held by the engine process")]
    PortNotOwned,
    /// 记录指向本进程或本进程所在的进程组。
    #[error("the ownership record names this process or its own process group")]
    UnsafeRecord,
    /// 只有本进程 spawn 的引擎能封印。
    #[error("only an engine spawned by this process can be sealed")]
    CannotSeal,
    #[error("failed to signal the engine's process group")]
    Signal,
    #[error("the engine's process group did not go away")]
    StopFailed,
    #[error("failed to build the HTTP client")]
    HttpClient,
    /// 查不了子进程的状态（`waitid` 失败）。
    #[error("the engine process state could not be inspected")]
    ProcessState,
}

impl EngineError {
    /// 稳定的原因码，给 `/health.upstreams[].reason` 与日志。
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::Log(_) => "log_unavailable",
            Self::Spawn(_) => "spawn_failed",
            Self::ExitedEarly => "engine_exited",
            Self::IdentityUnavailable => "identity_unavailable",
            Self::Record(_) => "record_failed",
            Self::AlreadyOwned => "engine_already_owned",
            Self::IdentityUnreadable(_) => "identity_unreadable",
            Self::NotReady { .. } => "startup_timeout",
            Self::PortNotOwned => "port_in_use",
            Self::UnsafeRecord => "unsafe_record",
            Self::CannotSeal => "cannot_seal",
            Self::Signal => "signal_failed",
            Self::StopFailed => "stop_failed",
            Self::HttpClient => "http_client",
            Self::ProcessState => "process_state_unavailable",
        }
    }
}

/// 等就绪的节奏（设计稿 §2.7.2：默认 90 秒，轮询 250 毫秒起退避到 1 秒）。
#[derive(Debug, Clone, Copy)]
pub struct ReadyPolicy {
    pub timeout: Duration,
    pub initial_interval: Duration,
    pub max_interval: Duration,
    /// 单次探测的上限。
    pub request_timeout: Duration,
}

impl Default for ReadyPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(90),
            initial_interval: Duration::from_millis(250),
            max_interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
        }
    }
}

/// [`OwnedEngine::recover`] 的结果。
#[derive(Debug)]
pub enum Recovery {
    /// 没有这个实例的记录。
    NoRecord,
    /// 四项身份吻合：认回了上一个 iDoris 留下的引擎，可以直接用（沿用记录里的 key，已加载的模型还在）。
    Adopted(OwnedEngine),
    /// 记录没封印：上一个 iDoris 在引擎启动途中被杀。它还在，但没有就绪过：只能 [`OwnedEngine::stop`] 后重拉。
    Starting(OwnedEngine),
    /// 记录作废（进程没了、pid 被复用、记录坏了）：**没有发任何信号**，记录已删。
    Stale(MismatchReason),
    /// 有一项身份读不出来：**没有发任何信号**，记录保留。
    Unverifiable(UnreadableReason),
}

enum Backing {
    /// 本进程拉起的子进程。
    Spawned(ManagedRuntimeProcess),
    /// 上一个 iDoris 拉起、已被认回的孤儿（不是我们的子进程）。
    Recovered,
}

/// iDoris 拥有的一个本地推理引擎进程。
pub struct OwnedEngine {
    backing: Backing,
    instance: EngineInstance,
    endpoint: String,
    record: OwnershipRecord,
    store: OwnershipStore,
    inspector: Arc<dyn ProcessInspector>,
}

impl std::fmt::Debug for OwnedEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedEngine")
            .field("instance", &self.instance)
            .field("pid", &self.record.pid)
            .field("endpoint", &self.endpoint)
            .field("sealed", &self.is_sealed())
            .field(
                "backing",
                &match self.backing {
                    Backing::Spawned(_) => "spawned",
                    Backing::Recovered => "recovered",
                },
            )
            .finish_non_exhaustive()
    }
}

impl OwnedEngine {
    /// 拉起引擎，并立刻写一份未封印的所有权记录。
    ///
    /// 步骤：（1）这个实例（种类加 endpoint）已有记录而描述的引擎可能还活着 → [`EngineError::AlreadyOwned`]
    /// （陈旧的记录先清掉）；（2）滚动并打开日志；（3）spawn；（4）读启动时间
    /// （重试几次），写记录。（3）与（4）之间的几十毫秒里 iDoris 被杀，会留下一个没有记录的引擎，
    /// 这个窗口无法消除。读不出启动时间或写不了记录：尽力停掉刚拉起的引擎再报错（`abort` 忽略 `shutdown` 的失败，不保证停得掉）。
    ///
    /// `launched_by` 是这个 iDoris 进程的标识（`/health.instance_id`），写进记录，只用于排查。
    pub async fn spawn(
        launch: EngineLaunch,
        inspector: Arc<dyn ProcessInspector>,
        launched_by: &str,
    ) -> Result<Self, EngineError> {
        let instance = launch.instance().clone();
        let store = instance.store();
        let endpoint = launch.endpoint();
        match classify(&store, inspector.as_ref()).await? {
            Classified::NoRecord | Classified::Stale(_) => {}
            Classified::Verified { .. } | Classified::Unverifiable(_) => {
                return Err(EngineError::AlreadyOwned);
            }
        }

        let (stdout, stderr) = launch
            .log()
            .open_for_child()
            .map_err(|error| EngineError::Log(error.kind()))?;
        let mut process = ManagedRuntimeProcess::spawn_engine(EngineSpawn {
            executable: launch.executable().to_owned(),
            args: launch.args().to_vec(),
            env: launch
                .env()
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            current_dir: launch.working_dir().to_owned(),
            stdout,
            stderr,
            kill_group_on_drop: launch.stop_on_exit() == StopOnExit::Owned,
        })
        .map_err(|error| EngineError::Spawn(error.kind()))?;

        let Some(pid) = process.id() else {
            abort(process).await;
            return Err(EngineError::ExitedEarly);
        };
        let mut started_at = None;
        for attempt in 0..IDENTITY_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(IDENTITY_RETRY).await;
            }
            if let Ok(value) = inspector.start_time(pid).await {
                started_at = Some(value);
                break;
            }
        }
        let Some(started_at) = started_at else {
            // 读不出来有两种原因：它已经退出了（最常见：可执行文件立刻失败），或者 `ps` 真的坏了。
            let exited = !process.is_running().unwrap_or(false);
            abort(process).await;
            return Err(if exited {
                EngineError::ExitedEarly
            } else {
                EngineError::IdentityUnavailable
            });
        };
        let record = OwnershipRecord {
            engine_kind: launch.kind().clone(),
            pid,
            port: launch.port(),
            started_at,
            command: None,
            executable: launch.executable().to_owned(),
            api_key: launch.api_key().cloned(),
            launched_by_instance: launched_by.to_owned(),
        };
        if let Err(error) = store.write(&record) {
            abort(process).await;
            return Err(EngineError::Record(error));
        }
        Ok(Self {
            backing: Backing::Spawned(process),
            instance,
            endpoint,
            record,
            store,
            inspector,
        })
    }

    /// 找这个实例（种类加 endpoint）上一个 iDoris 留下的记录，核对身份，认回引擎。
    /// 作废的记录在这里删掉（没有发任何信号）；身份读不出来的原样保留。
    pub async fn recover(
        instance: &EngineInstance,
        inspector: &Arc<dyn ProcessInspector>,
    ) -> Result<Recovery, EngineError> {
        let store = instance.store();
        Ok(match classify(&store, inspector.as_ref()).await? {
            Classified::NoRecord => Recovery::NoRecord,
            Classified::Stale(reason) => Recovery::Stale(reason),
            Classified::Unverifiable(reason) => Recovery::Unverifiable(reason),
            Classified::Verified { record, sealed } => {
                let engine = Self {
                    backing: Backing::Recovered,
                    instance: instance.clone(),
                    endpoint: instance.endpoint(),
                    record,
                    store,
                    inspector: Arc::clone(inspector),
                };
                if sealed {
                    Recovery::Adopted(engine)
                } else {
                    Recovery::Starting(engine)
                }
            }
        })
    }

    pub fn instance(&self) -> &EngineInstance {
        &self.instance
    }

    pub fn pid(&self) -> u32 {
        self.record.pid
    }

    pub fn port(&self) -> u16 {
        self.record.port
    }

    /// `http://127.0.0.1:<port>`。
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 这个引擎的私有 API key（与记录里的是同一个）；没有就是 `None`。
    pub fn api_key(&self) -> Option<&EngineKey> {
        self.record.api_key.as_ref()
    }

    pub fn record(&self) -> &OwnershipRecord {
        &self.record
    }

    /// 记录是否已封印（可以被下一个 iDoris 接管）。
    pub fn is_sealed(&self) -> bool {
        self.record.command.is_some()
    }

    /// 引擎进程还在吗？自己的子进程看 `waitid`（不回收，PGID 锚点保留）；认回的孤儿看身份闸。
    pub async fn is_running(&mut self) -> Result<bool, EngineError> {
        match &mut self.backing {
            Backing::Spawned(process) => {
                process.is_running().map_err(|_| EngineError::ProcessState)
            }
            Backing::Recovered => match gate(self.inspector.as_ref(), &self.record).await {
                Gate::Ours => Ok(true),
                Gate::Gone | Gate::NotOurs(_) => Ok(false),
                Gate::Unreadable(reason) => Err(EngineError::IdentityUnreadable(reason)),
            },
        }
    }

    /// 等引擎就绪：轮询配方的 [`super::EngineRecipe::probe_ready`]（带这个引擎的私有 key），退避，
    /// 直到就绪、进程退出或超时。进程退出立刻返回 [`EngineError::ExitedEarly`]，不等满超时。
    pub async fn wait_ready(&mut self, policy: &ReadyPolicy) -> Result<(), EngineError> {
        let client = crate::http_client().map_err(|_| EngineError::HttpClient)?;
        let deadline = deadline_after(policy.timeout);
        let mut interval = policy.initial_interval;
        loop {
            if !self.is_running().await? {
                return Err(EngineError::ExitedEarly);
            }
            let probe = self
                .instance
                .recipe()
                .probe_ready(
                    &client,
                    &self.endpoint,
                    self.record.api_key.as_ref().map(EngineKey::expose),
                    policy.request_timeout,
                )
                .await;
            if probe == EngineProbe::Ready {
                return Ok(());
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(EngineError::NotReady { last: Some(probe) });
            }
            tokio::time::sleep(interval.min(deadline - now)).await;
            interval = interval.saturating_mul(2).min(policy.max_interval);
        }
    }

    /// 封印记录：引擎就绪之后，读它此刻的命令行、确认监听记录里那个端口的就是这个 pid，写进记录。
    /// 封印之后记录才能被下一个 iDoris 接管。
    ///
    /// 「端口上答话的是不是我们的引擎」在这里才有答案：端口被另一个同类引擎占着、我们拉起的进程绑不上端口
    /// 时，就绪探测会被那个别人的服务答出「就绪」，只有 `lsof` 能说出监听的是哪个 pid
    /// → [`EngineError::PortNotOwned`]，调用方应停掉这个引擎并报 `port_in_use`。
    pub async fn seal(&mut self) -> Result<(), EngineError> {
        if self.is_sealed() {
            return Ok(());
        }
        let Backing::Spawned(process) = &mut self.backing else {
            return Err(EngineError::CannotSeal);
        };
        if !process.is_running().unwrap_or(false) {
            return Err(EngineError::ExitedEarly);
        }
        match gate(self.inspector.as_ref(), &self.record).await {
            Gate::Ours => {}
            Gate::Gone | Gate::NotOurs(_) => return Err(EngineError::ExitedEarly),
            Gate::Unreadable(reason) => return Err(EngineError::IdentityUnreadable(reason)),
        }
        let command = self
            .inspector
            .command(self.record.pid)
            .await
            .map_err(|_| EngineError::IdentityUnreadable(UnreadableReason::Command))?;
        match self
            .inspector
            .listens_on(self.record.pid, self.record.port)
            .await
        {
            Ok(true) => {}
            Ok(false) => return Err(EngineError::PortNotOwned),
            Err(_) => return Err(EngineError::IdentityUnreadable(UnreadableReason::Port)),
        }
        let mut sealed = self.record.clone();
        sealed.command = Some(command);
        self.store.write(&sealed)?;
        self.record = sealed;
        Ok(())
    }

    /// 停止引擎：SIGTERM 整个进程组，`grace` 内等整组消失，仍在就 SIGKILL 整组，再等到消失；成功后删记录。
    ///
    /// 认回的孤儿身份对不上（进程已经没了、pid 被复用）时什么信号都不发，直接 `Ok`：没有我们的东西在跑。
    /// 身份读不出来 → [`EngineError::IdentityUnreadable`]，同样不发信号。
    pub async fn stop(self, grace: Duration) -> Result<(), EngineError> {
        let Self {
            backing,
            record,
            store,
            inspector,
            ..
        } = self;
        match backing {
            Backing::Spawned(process) => {
                if process.shutdown(grace).await.is_err() {
                    // `shutdown` sweeps the group with SIGKILL once; a child forked in the instant
                    // of that sweep is not on its list. Sweep again until nothing is left.
                    let group = i32::try_from(record.process_group())
                        .map_err(|_| EngineError::StopFailed)?;
                    if !sweep_until_gone(group, grace).await? {
                        return Err(EngineError::StopFailed);
                    }
                }
            }
            Backing::Recovered => stop_recovered(inspector.as_ref(), &record, grace).await?,
        }
        // 进程已确认消失。记录删不掉也无妨：下一个 iDoris 读到它会因进程不在而作废。
        let _ = store.remove_if_same(&record);
        Ok(())
    }
}

/// 拉起之后某一步失败：把刚拉起的引擎停掉。
async fn abort(process: ManagedRuntimeProcess) {
    let _ = process.shutdown(ABORT_GRACE).await;
}

enum Gate {
    /// pid 存活、启动时间与进程组都吻合。
    Ours,
    Gone,
    NotOurs(MismatchReason),
    Unreadable(UnreadableReason),
}

/// 发信号之前必须过的闸：记录里的 pid 现在还是那个进程、还领着那个组。
async fn gate(inspector: &dyn ProcessInspector, record: &OwnershipRecord) -> Gate {
    match inspector.pid_state(record.pid) {
        PidState::Gone => return Gate::Gone,
        PidState::OtherUser => return Gate::NotOurs(MismatchReason::OtherUser),
        PidState::Alive => {}
    }
    match inspector.start_time(record.pid).await {
        Err(_) => return Gate::Unreadable(UnreadableReason::StartTime),
        Ok(started) if started != record.started_at => {
            return Gate::NotOurs(MismatchReason::StartTime);
        }
        Ok(_) => {}
    }
    match inspector.process_group(record.pid).await {
        Err(_) => Gate::Unreadable(UnreadableReason::ProcessGroup),
        Ok(group) if group != record.process_group() => Gate::NotOurs(MismatchReason::ProcessGroup),
        Ok(_) => Gate::Ours,
    }
}

/// 逐项核对记录与进程。顺序同 `omlx-kept.ts`：存在 → 启动时间 → （进程组）→ 命令 → 监听端口；
/// 第一个不一致或读不出来的就是结论。与引擎种类无关。
pub async fn verify_identity(
    inspector: &dyn ProcessInspector,
    record: &OwnershipRecord,
) -> IdentityVerdict {
    match gate(inspector, record).await {
        Gate::Ours => {}
        Gate::Gone => return IdentityVerdict::Mismatch(MismatchReason::ProcessGone),
        Gate::NotOurs(reason) => return IdentityVerdict::Mismatch(reason),
        Gate::Unreadable(reason) => return IdentityVerdict::Unreadable(reason),
    }
    let Some(expected) = &record.command else {
        return IdentityVerdict::Unsealed;
    };
    match inspector.command(record.pid).await {
        Err(_) => return IdentityVerdict::Unreadable(UnreadableReason::Command),
        Ok(command) if command != *expected => {
            return IdentityVerdict::Mismatch(MismatchReason::Command);
        }
        Ok(_) => {}
    }
    match inspector.listens_on(record.pid, record.port).await {
        Err(_) => IdentityVerdict::Unreadable(UnreadableReason::Port),
        Ok(false) => IdentityVerdict::Mismatch(MismatchReason::Port),
        Ok(true) => IdentityVerdict::Match,
    }
}

enum Classified {
    NoRecord,
    /// 记录作废，已删。
    Stale(MismatchReason),
    Verified {
        record: OwnershipRecord,
        sealed: bool,
    },
    Unverifiable(UnreadableReason),
}

async fn classify(
    store: &OwnershipStore,
    inspector: &dyn ProcessInspector,
) -> Result<Classified, EngineError> {
    let record = match store.read() {
        Ok(None) => return Ok(Classified::NoRecord),
        Ok(Some(record)) => record,
        // 内容坏了、权限不对、太大、不属于这个实例：认不出任何进程，丢掉它重来是安全的。
        Err(error) if error.is_unusable_record() => {
            store.remove()?;
            return Ok(Classified::Stale(MismatchReason::InvalidRecord));
        }
        Err(error) => return Err(EngineError::Record(error)),
    };
    Ok(match verify_identity(inspector, &record).await {
        IdentityVerdict::Match => Classified::Verified {
            record,
            sealed: true,
        },
        IdentityVerdict::Unsealed => Classified::Verified {
            record,
            sealed: false,
        },
        IdentityVerdict::Mismatch(reason) => {
            store.remove_if_same(&record)?;
            Classified::Stale(reason)
        }
        IdentityVerdict::Unreadable(reason) => Classified::Unverifiable(reason),
    })
}

/// 停认回的孤儿：身份闸 → SIGTERM 整组 → 等整组消失 → SIGKILL 整组 → 等整组消失。
async fn stop_recovered(
    inspector: &dyn ProcessInspector,
    record: &OwnershipRecord,
    grace: Duration,
) -> Result<(), EngineError> {
    match gate(inspector, record).await {
        // 没有我们的东西在跑（或 pid 已经给了别人）：什么都不发。
        Gate::Gone | Gate::NotOurs(_) => return Ok(()),
        Gate::Unreadable(reason) => return Err(EngineError::IdentityUnreadable(reason)),
        Gate::Ours => {}
    }
    let group = signal_target(record)?;
    signal_group(group, Signal::SIGTERM).map_err(|_| EngineError::Signal)?;
    if wait_group_gone(group, grace).await? {
        return Ok(());
    }
    // 组里仍有活成员：组 id 此刻没有被重新分配（成员检查与 SIGKILL 之间有微秒级的非原子窗口），整组 SIGKILL。
    if sweep_until_gone(group, grace).await? {
        Ok(())
    } else {
        Err(EngineError::StopFailed)
    }
}

/// 整组信号的目标进程组。记录指向本进程或本进程所在的组（`killpg` 会杀掉 iDoris 自己与它的整个组），
/// 或指向 pid ≤ 1 / 放不进 `i32` 的组，都不是一个引擎：拒绝。
fn signal_target(record: &OwnershipRecord) -> Result<i32, EngineError> {
    let own_group = rustix::process::getpgrp().as_raw_nonzero().get();
    let group = i32::try_from(record.process_group()).map_err(|_| EngineError::UnsafeRecord)?;
    if group < 2 || group == own_group || record.pid == std::process::id() {
        return Err(EngineError::UnsafeRecord);
    }
    Ok(group)
}

/// `now + after`, saturating: a caller-supplied `Duration::MAX` must not panic inside a primitive.
fn deadline_after(after: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(after)
        .unwrap_or_else(|| now + Duration::from_secs(100 * 365 * 24 * 3600))
}

/// 整组 SIGKILL，直到整组消失（`true`）或 `grace` 用完（`false`）。
///
/// 每一轮都先确认组里还有活成员再发信号：组里有成员时这个组 id 不会被分配给别的进程，组空了就不再发。成员检查与
/// `signal_group` 之间有微秒级的非原子窗口（组恰好清空、pid 又被复用成另一个组的组长，才会误伤），没有办法消除。要一轮
/// 一轮地扫，是因为 SIGKILL 扫过的那一刻刚 fork 出来的子进程不在那一轮的名单里，下一轮才收得掉（引擎正在 fork worker 时被停，会遇到）。
async fn sweep_until_gone(group: i32, grace: Duration) -> Result<bool, EngineError> {
    let deadline = deadline_after(grace);
    loop {
        if !group_has_live_members(group).map_err(|_| EngineError::StopFailed)? {
            return Ok(true);
        }
        // 至少发一次，哪怕宽限已经用完：`stop` 的 SIGTERM 等待可能已经把宽限花光了。
        signal_group(group, Signal::SIGKILL).map_err(|_| EngineError::Signal)?;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(!group_has_live_members(group).map_err(|_| EngineError::StopFailed)?);
        }
        tokio::time::sleep(POLL.min(deadline - now)).await;
    }
}

async fn wait_group_gone(group: i32, grace: Duration) -> Result<bool, EngineError> {
    let deadline = deadline_after(grace);
    loop {
        if !group_has_live_members(group).map_err(|_| EngineError::StopFailed)? {
            return Ok(true);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(POLL.min(deadline - now)).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::engine::test_support::instance;
    use crate::engine::{EngineKind, InspectError};

    /// An inspector whose answers are set per call site, recording what it was asked.
    struct Fake {
        state: PidState,
        start: Result<String, InspectError>,
        group: Result<u32, InspectError>,
        command: Result<String, InspectError>,
        listens: Result<bool, InspectError>,
        asked: Mutex<Vec<&'static str>>,
    }

    impl Fake {
        /// Everything matches `record()`.
        fn matching() -> Self {
            Self {
                state: PidState::Alive,
                start: Ok("Fri Nov 20 08:14:05 2026".into()),
                group: Ok(51234),
                command: Ok("fake-server".into()),
                listens: Ok(true),
                asked: Mutex::new(Vec::new()),
            }
        }
        fn asked(&self) -> Vec<&'static str> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProcessInspector for Fake {
        fn pid_state(&self, _pid: u32) -> PidState {
            self.asked.lock().unwrap().push("state");
            self.state
        }
        async fn start_time(&self, _pid: u32) -> Result<String, InspectError> {
            self.asked.lock().unwrap().push("start");
            self.start.clone()
        }
        async fn command(&self, _pid: u32) -> Result<String, InspectError> {
            self.asked.lock().unwrap().push("command");
            self.command.clone()
        }
        async fn process_group(&self, _pid: u32) -> Result<u32, InspectError> {
            self.asked.lock().unwrap().push("group");
            self.group
        }
        async fn listens_on(&self, _pid: u32, _port: u16) -> Result<bool, InspectError> {
            self.asked.lock().unwrap().push("port");
            self.listens
        }
    }

    fn kind(name: &str) -> EngineKind {
        EngineKind::parse(name).unwrap()
    }

    fn record() -> OwnershipRecord {
        OwnershipRecord {
            engine_kind: kind("fake"),
            pid: 51234,
            port: 19731,
            started_at: "Fri Nov 20 08:14:05 2026".into(),
            command: Some("fake-server".into()),
            executable: "/opt/engines/fake/bin/fake-server".into(),
            api_key: Some(EngineKey::from_hex(&"ab".repeat(32)).unwrap()),
            launched_by_instance: "3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11".into(),
        }
    }

    /// A store (and its instance) for kind `fake` on port 19731 under a fresh state directory.
    fn fixture() -> (tempfile::TempDir, EngineInstance, OwnershipStore) {
        let state = tempfile::tempdir().unwrap();
        let inst = instance(state.path(), "fake", 19731);
        let store = inst.store();
        (state, inst, store)
    }

    #[tokio::test]
    async fn all_four_items_matching_is_a_match() {
        let fake = Fake::matching();
        assert_eq!(
            verify_identity(&fake, &record()).await,
            IdentityVerdict::Match
        );
        assert_eq!(fake.asked(), ["state", "start", "group", "command", "port"]);
    }

    #[tokio::test]
    async fn each_item_alone_differing_is_a_mismatch_and_stops_the_check() {
        type Tweak = fn(&mut Fake);
        let cases: [(&str, Tweak, MismatchReason, &[&str]); 6] = [
            (
                "process gone",
                |f| f.state = PidState::Gone,
                MismatchReason::ProcessGone,
                &["state"],
            ),
            (
                "other user",
                |f| f.state = PidState::OtherUser,
                MismatchReason::OtherUser,
                &["state"],
            ),
            (
                "pid reused: other start time",
                |f| f.start = Ok("Fri Nov 20 09:00:00 2026".into()),
                MismatchReason::StartTime,
                &["state", "start"],
            ),
            (
                "not the group leader any more",
                |f| f.group = Ok(1),
                MismatchReason::ProcessGroup,
                &["state", "start", "group"],
            ),
            (
                "other command",
                |f| f.command = Ok("python3 -m http.server".into()),
                MismatchReason::Command,
                &["state", "start", "group", "command"],
            ),
            (
                "not listening on the port",
                |f| f.listens = Ok(false),
                MismatchReason::Port,
                &["state", "start", "group", "command", "port"],
            ),
        ];
        for (name, tweak, reason, asked) in cases {
            let mut fake = Fake::matching();
            tweak(&mut fake);
            assert_eq!(
                verify_identity(&fake, &record()).await,
                IdentityVerdict::Mismatch(reason),
                "{name}"
            );
            assert_eq!(fake.asked(), asked, "{name}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_item_is_never_a_mismatch_and_never_a_match() {
        type Tweak = fn(&mut Fake);
        let cases: [(Tweak, UnreadableReason); 4] = [
            (
                |f| f.start = Err(InspectError::TimedOut),
                UnreadableReason::StartTime,
            ),
            (
                |f| f.group = Err(InspectError::NoAnswer),
                UnreadableReason::ProcessGroup,
            ),
            (
                |f| f.command = Err(InspectError::ToolMissing),
                UnreadableReason::Command,
            ),
            (
                |f| f.listens = Err(InspectError::TimedOut),
                UnreadableReason::Port,
            ),
        ];
        for (tweak, reason) in cases {
            let mut fake = Fake::matching();
            tweak(&mut fake);
            assert_eq!(
                verify_identity(&fake, &record()).await,
                IdentityVerdict::Unreadable(reason)
            );
        }
    }

    #[tokio::test]
    async fn an_unsealed_record_checks_pid_start_and_group_only() {
        let mut starting = record();
        starting.command = None;
        let fake = Fake::matching();
        assert_eq!(
            verify_identity(&fake, &starting).await,
            IdentityVerdict::Unsealed
        );
        assert_eq!(
            fake.asked(),
            ["state", "start", "group"],
            "no command, no port"
        );

        // A reused pid is still caught on an unsealed record.
        let mut reused = Fake::matching();
        reused.start = Ok("Mon Jan  1 00:00:00 2035".into());
        assert_eq!(
            verify_identity(&reused, &starting).await,
            IdentityVerdict::Mismatch(MismatchReason::StartTime)
        );
    }

    #[tokio::test]
    async fn the_identity_check_does_not_care_what_kind_of_engine_the_record_names() {
        for name in ["omlx", "ollama", "lmstudio", "llama_cpp", "something_new"] {
            let mut r = record();
            r.engine_kind = kind(name);
            assert_eq!(
                verify_identity(&Fake::matching(), &r).await,
                IdentityVerdict::Match,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn the_signal_gate_is_pid_start_and_group_only() {
        let fake = Fake::matching();
        assert!(matches!(gate(&fake, &record()).await, Gate::Ours));
        assert_eq!(fake.asked(), ["state", "start", "group"]);
        let mut reused = Fake::matching();
        reused.start = Ok("other".into());
        assert!(matches!(
            gate(&reused, &record()).await,
            Gate::NotOurs(MismatchReason::StartTime)
        ));
        let mut gone = Fake::matching();
        gone.state = PidState::Gone;
        assert!(matches!(gate(&gone, &record()).await, Gate::Gone));
        let mut blind = Fake::matching();
        blind.start = Err(InspectError::TimedOut);
        assert!(matches!(
            gate(&blind, &record()).await,
            Gate::Unreadable(UnreadableReason::StartTime)
        ));
    }

    #[tokio::test]
    async fn classify_removes_stale_records_but_keeps_unreadable_and_live_ones() {
        let (_state, _inst, store) = fixture();
        let present = || store.read().unwrap().is_some();

        assert!(matches!(
            classify(&store, &Fake::matching()).await.unwrap(),
            Classified::NoRecord
        ));

        store.write(&record()).unwrap();
        let mut reused = Fake::matching();
        reused.start = Ok("other".into());
        assert!(matches!(
            classify(&store, &reused).await.unwrap(),
            Classified::Stale(MismatchReason::StartTime)
        ));
        assert!(!present(), "a stale record is dropped");

        store.write(&record()).unwrap();
        let mut blind = Fake::matching();
        blind.command = Err(InspectError::TimedOut);
        assert!(matches!(
            classify(&store, &blind).await.unwrap(),
            Classified::Unverifiable(UnreadableReason::Command)
        ));
        assert!(present(), "an unreadable identity keeps the record");

        assert!(matches!(
            classify(&store, &Fake::matching()).await.unwrap(),
            Classified::Verified { sealed: true, .. }
        ));
        assert!(present());
    }

    #[tokio::test]
    async fn classify_refuses_records_that_are_not_ours_to_trust() {
        use std::os::unix::fs::PermissionsExt;

        let (state, _inst, store) = fixture();

        // A pid that has stopped leading its own group is not the engine we spawned.
        store.write(&record()).unwrap();
        let mut not_a_leader = Fake::matching();
        not_a_leader.group = Ok(4242);
        assert!(matches!(
            classify(&store, &not_a_leader).await.unwrap(),
            Classified::Stale(MismatchReason::ProcessGroup)
        ));
        assert!(store.read().unwrap().is_none());
        let fake = Fake::matching();

        // Garbage, and a good record in a file others can read, are discarded, not trusted.
        let path = store.path().to_owned();
        std::fs::write(&path, b"{\"schema\":1").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            classify(&store, &fake).await.unwrap(),
            Classified::Stale(MismatchReason::InvalidRecord)
        ));
        assert!(!path.exists());

        store.write(&record()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            classify(&store, &fake).await.unwrap(),
            Classified::Stale(MismatchReason::InvalidRecord)
        ));
        assert!(!path.exists());

        // A record that names another kind, or another endpoint (copied in from somewhere else),
        // is not this engine's: dropped from this location, and the original stays where it is.
        let theirs_store = instance(state.path(), "other_kind", 19731).store();
        let mut theirs = record();
        theirs.engine_kind = kind("other_kind");
        theirs_store.write(&theirs).unwrap();
        std::fs::copy(theirs_store.path(), &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            classify(&store, &fake).await.unwrap(),
            Classified::Stale(MismatchReason::InvalidRecord)
        ));
        assert!(!path.exists());
        assert!(
            theirs_store.read().unwrap().is_some(),
            "the other kind keeps its record"
        );

        let elsewhere = instance(state.path(), "fake", 19732).store();
        let mut moved = record();
        moved.port = 19732;
        elsewhere.write(&moved).unwrap();
        std::fs::copy(elsewhere.path(), &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            classify(&store, &fake).await.unwrap(),
            Classified::Stale(MismatchReason::InvalidRecord)
        ));
        assert!(
            elsewhere.read().unwrap().is_some(),
            "the other endpoint keeps its record"
        );
        assert!(fake.asked().is_empty(), "no process is ever looked at");
    }

    #[tokio::test]
    async fn records_of_another_engine_kind_are_invisible_to_this_kind_and_never_signalled() {
        let (state, inst, store) = fixture(); // kind "fake", port 19731
        let fake = Arc::new(Fake::matching());
        let inspector: Arc<dyn ProcessInspector> = fake.clone();

        // A live engine of another kind on the very same endpoint.
        let theirs_inst = instance(state.path(), "other_kind", 19731);
        let mut theirs = record();
        theirs.engine_kind = kind("other_kind");
        theirs_inst.store().write(&theirs).unwrap();

        let recovery = OwnedEngine::recover(&inst, &inspector).await.unwrap();
        assert!(matches!(recovery, Recovery::NoRecord), "{recovery:?}");
        assert!(
            fake.asked().is_empty(),
            "no process was looked at on behalf of the wrong kind"
        );
        assert_eq!(store.read().unwrap(), None);
        assert_eq!(
            theirs_inst.store().read().unwrap(),
            Some(theirs),
            "the other kind's record is exactly as it was"
        );

        // ...while its own kind finds it.
        let recovery = OwnedEngine::recover(&theirs_inst, &inspector)
            .await
            .unwrap();
        let Recovery::Adopted(engine) = recovery else {
            panic!("expected Adopted, got {recovery:?}");
        };
        assert_eq!(engine.instance().kind().as_str(), "other_kind");

        // A stale record is stale whatever kind it names: identity does not depend on the kind.
        let mut gone = Fake::matching();
        gone.state = PidState::Gone;
        let inspector: Arc<dyn ProcessInspector> = Arc::new(gone);
        let recovery = OwnedEngine::recover(&theirs_inst, &inspector)
            .await
            .unwrap();
        assert!(
            matches!(recovery, Recovery::Stale(MismatchReason::ProcessGone)),
            "{recovery:?}"
        );
        assert_eq!(theirs_inst.store().read().unwrap(), None);
    }

    #[tokio::test]
    async fn a_record_of_this_instance_is_adopted_with_its_kind_endpoint_and_key() {
        let (_state, inst, store) = fixture();
        let inspector: Arc<dyn ProcessInspector> = Arc::new(Fake::matching());
        store.write(&record()).unwrap();
        let Recovery::Adopted(engine) = OwnedEngine::recover(&inst, &inspector).await.unwrap()
        else {
            panic!("expected Adopted");
        };
        assert_eq!(engine.pid(), 51234);
        assert_eq!(engine.port(), 19731);
        assert_eq!(engine.endpoint(), "http://127.0.0.1:19731");
        assert_eq!(engine.instance().kind().as_str(), "fake");
        assert_eq!(engine.record().engine_kind, kind("fake"));
        assert!(engine.api_key().is_some());
        assert!(engine.is_sealed());

        let mut starting = record();
        starting.command = None;
        store.write(&starting).unwrap();
        let recovery = OwnedEngine::recover(&inst, &inspector).await.unwrap();
        assert!(matches!(recovery, Recovery::Starting(_)), "{recovery:?}");
    }

    #[tokio::test]
    async fn a_record_left_under_a_port_nobody_configures_any_more_is_found_and_adopted() {
        use crate::engine::recorded_ports;

        let state = tempfile::tempdir().unwrap();
        let inspector: Arc<dyn ProcessInspector> = Arc::new(Fake::matching());
        // The previous iDoris ran this kind on 19999; the configuration now says 19731.
        let old = instance(state.path(), "fake", 19999);
        let mut leftover = record();
        leftover.port = 19999;
        old.store().write(&leftover).unwrap();

        let configured = instance(state.path(), "fake", 19731);
        let recovery = OwnedEngine::recover(&configured, &inspector).await.unwrap();
        assert!(
            matches!(recovery, Recovery::NoRecord),
            "the configured endpoint has no record: {recovery:?}"
        );

        // Listing the kind's records finds the leftover; a manager can recover and stop it.
        let ports = recorded_ports(state.path(), &kind("fake")).unwrap();
        assert_eq!(ports, [19999]);
        let found = instance(state.path(), "fake", ports[0]);
        let recovery = OwnedEngine::recover(&found, &inspector).await.unwrap();
        let Recovery::Adopted(engine) = recovery else {
            panic!("expected Adopted, got {recovery:?}");
        };
        assert_eq!(engine.port(), 19999);
        assert_eq!(engine.endpoint(), "http://127.0.0.1:19999");
    }

    #[tokio::test]
    async fn stop_recovered_sends_nothing_unless_the_gate_passes() {
        let grace = Duration::from_millis(100);
        // The record names this very process: signalling it would be fatal, so every
        // outcome below must be reached before any signal path.
        let mut mine = record();
        mine.pid = std::process::id();
        for tweak in [
            (|f: &mut Fake| f.start = Ok("other".into())) as fn(&mut Fake),
            |f| f.state = PidState::Gone,
            |f| f.state = PidState::OtherUser,
            |f| f.group = Ok(2),
        ] {
            let mut fake = Fake::matching();
            tweak(&mut fake);
            stop_recovered(&fake, &mine, grace).await.unwrap();
        }
        let mut blind = Fake::matching();
        blind.start = Err(InspectError::TimedOut);
        assert!(matches!(
            stop_recovered(&blind, &mine, grace).await,
            Err(EngineError::IdentityUnreadable(UnreadableReason::StartTime))
        ));
    }

    #[tokio::test]
    async fn sweeping_kills_members_left_in_a_group_whose_leader_is_already_gone() {
        use std::os::unix::process::CommandExt;

        // A leader with two members, in a group of its own (so nothing here can reach the test
        // runner). Killing only the leader leaves the members: exactly what a SIGKILL sweep that
        // missed a freshly forked child leaves behind.
        let mut leader = std::process::Command::new("/bin/sh");
        leader
            .args(["-c", "sleep 600 & sleep 600 & wait"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut leader = leader.spawn().unwrap();
        let group = i32::try_from(leader.id()).unwrap();
        let _cleanup = GroupCleanup(group);
        let members_appear = tokio::time::timeout(Duration::from_secs(30), async {
            while !group_has_live_members(group).unwrap() || count_members(group) < 3 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(members_appear.is_ok(), "the group never filled up");
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert!(
            group_has_live_members(group).unwrap(),
            "the members outlive their leader"
        );

        assert!(
            sweep_until_gone(group, Duration::from_secs(10))
                .await
                .unwrap()
        );
        assert!(!group_has_live_members(group).unwrap());
        // Nothing left: a second sweep is a no-op and signals nobody.
        assert!(
            sweep_until_gone(group, Duration::from_millis(50))
                .await
                .unwrap()
        );
    }

    /// Kills what is left of a test's process group when the test ends, even by panic: a failing
    /// assertion must not leave `sleep 600` processes behind (they would also hold the harness's
    /// output pipe open for ten minutes). Safe against pid reuse: a group id is not handed out
    /// again while the group still has live members, and nothing is sent to an empty group.
    struct GroupCleanup(i32);

    impl Drop for GroupCleanup {
        fn drop(&mut self) {
            if group_has_live_members(self.0).unwrap_or(false) {
                let _ = signal_group(self.0, Signal::SIGKILL);
            }
        }
    }

    /// Live (not zombie) members of `group`, from `ps` (the test's own view, not the code under test).
    fn count_members(group: i32) -> usize {
        let out = std::process::Command::new("/bin/ps")
            .args(["-A", "-o", "pid=,pgid=,stat="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| {
                let mut f = line.split_whitespace();
                let _pid = f.next();
                f.next().and_then(|g| g.parse::<i32>().ok()) == Some(group)
                    && !f.next().unwrap_or("").starts_with('Z')
            })
            .count()
    }

    #[tokio::test]
    async fn a_grace_that_is_already_spent_still_sends_the_one_sigkill() {
        use std::os::unix::process::CommandExt;

        let mut child = std::process::Command::new("/bin/sleep");
        child
            .arg("600")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut child = child.spawn().unwrap();
        let group = i32::try_from(child.id()).unwrap();
        let _cleanup = GroupCleanup(group);
        // The SIGTERM wait of `stop` may have used up the whole grace; the SIGKILL still goes out.
        let _ = sweep_until_gone(group, Duration::ZERO).await.unwrap();
        // The process was killed by that one signal: reaping it returns promptly with a signal status.
        let status = child.wait().unwrap();
        assert!(!status.success(), "{status:?}");
        assert!(!group_has_live_members(group).unwrap());
    }

    #[test]
    fn the_signal_target_is_never_this_process_its_group_or_a_system_pid() {
        let own_pid = std::process::id();
        let own_group = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap();
        let with = |pid: u32| {
            let mut r = record();
            r.pid = pid;
            r
        };
        for refused in [
            with(own_pid),
            with(own_group), // the leader of the group this test runs in: signalling it reaches us
            with(1),
            with(0),
            with(u32::MAX),
            with(i32::MAX as u32 + 1),
        ] {
            assert!(
                matches!(signal_target(&refused), Err(EngineError::UnsafeRecord)),
                "{refused:?}"
            );
        }
        assert_eq!(signal_target(&with(51234)).unwrap(), 51234);
    }

    /// Compile-time: the manager (S5) will hold an engine in a spawned task.
    #[allow(dead_code)]
    async fn the_engine_and_its_futures_are_send(
        mut engine: OwnedEngine,
        inst: EngineInstance,
        inspector: Arc<dyn ProcessInspector>,
        launch: EngineLaunch,
    ) {
        fn is_send<T: Send>(_: T) {}
        is_send(OwnedEngine::spawn(launch, inspector.clone(), "x"));
        is_send(OwnedEngine::recover(&inst, &inspector));
        is_send(engine.wait_ready(&ReadyPolicy::default()));
        is_send(engine.seal());
        is_send(engine.is_running());
        is_send(engine.stop(Duration::from_secs(1)));
    }

    #[test]
    fn the_default_stop_grace_is_the_specs_eight_seconds() {
        assert_eq!(DEFAULT_STOP_GRACE, Duration::from_secs(8));
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(EngineError::PortNotOwned.reason_code(), "port_in_use");
        assert_eq!(
            EngineError::NotReady { last: None }.reason_code(),
            "startup_timeout"
        );
        assert_eq!(EngineError::ExitedEarly.reason_code(), "engine_exited");
        assert_eq!(MismatchReason::StartTime.code(), "start_time");
        let shown = EngineError::IdentityUnreadable(UnreadableReason::Port).to_string();
        assert!(shown.contains("port"));
    }
}
