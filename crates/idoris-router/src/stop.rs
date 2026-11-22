//! 优雅停止的「为什么停」与「怎么收尾」（接口规范 §2.7.5、§2.10.4、§2.10.8；勘误十二）。
//!
//! # 停止有五个来源，同一轮里同时成立时按优先级取一个
//!
//! | 来源 | 退出码 | 性质 |
//! |---|---|---|
//! | 自有引擎失效（门控完成之后进程退出） | 5 | 发布出来的状态：发布之后一直成立 |
//! | 孤儿超时 | 4 | 同上（登记客户端发布，[`crate::register::Registration::wants_stop`]） |
//! | agent24d 的 `instance_conflict` | 3 | 同上 |
//! | 门控完成时创建完整服务体失败（启动失败） | 1 | 同上（[`crate::lifecycle::gate::GateTask::failed`]） |
//! | 信号（SIGTERM、SIGINT） | 0 | 一次性事件 |
//!
//! 前四者发布之后一直成立，谁来读都读得到；信号只有一次。它们在同一轮里同时成立（登记任务已经发布了 `instance_conflict`，
//! 这时 SIGTERM 也到了，而停止流程还没轮到被 poll）时，无 `biased` 的 `tokio::select!` 是**随机**挑一个：已经成立的退出原因
//! 会被信号盖成退出码 0，`stop_on_exit:"never"` 时本该停的引擎被留下，退出码 5 丢失（launcher 的崩溃计数少一次），
//! `bye` 的策略也跟着变（H4）。所以优先级写死在 [`StopSources`] 的字段顺序里（[`StopSources::first`] 按这个顺序、`biased` 地轮询），
//! 已经成立的永远排在信号前面：
//!
//! **5（引擎失效）> 4（孤儿超时）> 3（`instance_conflict`）> 启动失败（1）> 信号（0）**
//!
//! 4 与 3 来自同一个登记客户端，它发布一个原因就结束，所以不会同时有两个；它们之间的先后只在文字上写明。
//! 启动失败排在 3、4、5 之后、信号之前：门控没能创建完整服务体是一个真实的故障，不能被一个顺手的 SIGTERM 盖成「干净退出」，
//! 但 3、4、5 比它更具体地告诉 launcher 该怎么办（3 不要重启，4 按预期停止，5 计入崩溃熔断并走全新的启动门控）。
//!
//! # 只锁存一次，同一个结果驱动一切
//!
//! 决定（[`StopSources::first`]）之后，结果放进 [`StopLatch`]（先到先得的一次性槽：之后到的原因一概不改它）。
//! 退出码、`bye`、对自有引擎的处置都从这**一个**[`StopVerdict`] 推出（[`StopPlan::new`]、[`engines_stop_after_drain`]），
//! 没有第二处各自再判断一次。
//!
//! # 引擎的处置（H5）
//!
//! `stop_on_exit:"never"` 只对退出码 0 有效（§2.7.5）。信号一到就放下所有权句柄、之后排空失败以 1 退出，会留下一个没人管的引擎。
//! 所以 `never` 的信号停止（[`EnginePlan::DecideAfterDrain`]）在排空期间**保留所有权句柄**，排空结束、退出结果出来之后才定：
//! 以 0 退出就移交（引擎继续跑，所有权记录留给下一个 iDoris），否则停止并回收。其余情形（`owned`、退出码 3、4、5、启动失败）
//! 原因一确定就与排空并行地停（[`EnginePlan::StopNow`]），总时长仍在 §2.7.5 的预算内。
//!
//! # 总期限（M1、H2）
//!
//! 排空失败之后才停引擎的那条路（`never` + 信号 + 排空失败）是**串行**的：排空最多 10 秒，之后引擎 SIGTERM 加 8 秒宽限，共 18 秒，
//! 而 launcher 在 SIGTERM 之后 17 秒 SIGKILL iDoris：引擎可能停到一半（收到了 SIGTERM、还在退出）iDoris 就被杀了。
//! 所以整个收尾有一个**从停止原因锁存那一刻起算的期限**（[`STOP_DEADLINE`]，16 秒，给确认与退出留 1 秒），**所有**后续各段共用它，
//! 不是各管各的：引擎收尾、登记收尾、删发现文件、拆运行时。每一段有一个绝对的结束时刻（[`StopBudgets`]，从总期限倒着留出后面几段的
//! 预留），前面的段提前结束，后面的段就多得时间；前面的段超了，后面的段至少还有自己的预留。
//!
//! - 引擎：[`finish_engines`] 的等待、引擎管理器的等待与 `OwnedEngine` 的宽限都只拿期限里剩下的（`min(8 秒, 剩下的)`，不到 2 秒就
//!   SIGTERM 之后立刻 SIGKILL，[`crate::lifecycle::stop_windows`]）。外面这层等待只比里面多一个确认窗口的余量（[`WAIT_SLACK`]），
//!   不再多留 1.5 秒：那 1.5 秒正是后面几段的预留。仍然没能确认消失的引擎，所有权记录保留，下一个 iDoris 认回它
//!   （`Recovery::Exiting`：身份吻合、只是端口上没有它在监听）再有界地收尾，不会悄悄丢掉记录。
//! - 登记：[`crate::register::Registration::finish_by`]；自愈任务在等旁路锁（最多 5 秒）时不会再把整个退出拖过期限。
//! - 发现文件：[`crate::discovery::DiscoveryTarget::remove_if_owner_within`]；预算用完还没删成就留着（崩溃残留语义：下一个拿到
//!   实例锁的实例覆盖它），写一行日志。
//! - 运行时：[`ExitDeadline`]；`main` 在最后拆运行时只等期限里剩下的。
//!
//! # 启动期间收到信号（H1）
//!
//! 启动期间（读组件目录、等 `idoris setup` 放开目录锁、等 launcher 的启动帧、等 Admin token）收到的 SIGTERM 不能等到这些等待自己
//! 结束：等组件目录锁最长 30 秒（可配到 10 分钟）、等帧 10 秒，而 launcher 17 秒就 SIGKILL。[`StopSignal`] 是个可以在同步代码里读的
//! 标志（[`StopSignal::is_set`]）：这些等待每一小段（至多 50 毫秒）看一眼，看到了就放弃等待，`run` 以退出码 0 干净地退出
//! （还没有拉起任何引擎、没有发布发现文件、没有登记）。拉起引擎之前再看一次：已经到过的信号一定在引擎拉起之前处理。
//!
//! # 信号处理器启动时就装好（L6）
//!
//! [`StopSignal::install`] 在 `run` 一开始就注册 SIGINT/SIGTERM，不是等停止流程第一次被 poll：`select!` 在更靠前的来源
//! 已经就绪时根本不 poll 信号那个分支，旧写法里处理器那时还没装，之后到的 SIGTERM 按默认动作直接杀死进程，不走优雅停止。

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::launcher_frame::StopOnExit;
use crate::lifecycle::EngineManager;
use crate::register::{Registration, StopCause};

/// 引擎收尾的总期限，从停止原因锁存那一刻起算。launcher 在 SIGTERM 之后 17 秒 SIGKILL iDoris；HTTP 排空最多 10 秒加引擎 8 秒宽限
/// 放不下（M1），所以引擎的等待与宽限都从这个期限里扣。留 1 秒给 SIGKILL 之后的确认、登记收尾与删发现文件。
pub const STOP_DEADLINE: Duration = Duration::from_secs(16);
/// 没有期限时（服务自己倒了：没有信号在催）等引擎收尾的上限：SIGTERM 之后 8 秒宽限，加上 SIGKILL 与收尾。
pub const OPEN_ENDED_STOP_WAIT: Duration = Duration::from_secs(14);
/// 外面这层等待比里面的期限多留的余量：里面的停止（宽限加确认窗口）先自己结束并如实报告，外面这层只兜底。里面的确认窗口至少
/// [`crate::lifecycle::MIN_CONFIRM`]（期限已到也给这么多：SIGKILL 之后几毫秒就够，这是给卡在内核里的进程的），所以余量就是它加一点调度的
/// 误差。再大就是在吃后面几段的预留（H2：此前这里是 1.5 秒，加上期限本身就压过了 launcher 的 17 秒）。
pub const WAIT_SLACK: Duration = Duration::from_millis(600);
/// 登记收尾（等自愈任务与登记客户端结束；`bye` 已在排空期间并行发出）为后面的段留的时间。
pub const REGISTRATION_RESERVE: Duration = Duration::from_millis(700);
/// 删发现文件（旁路锁至多等这么久，剩下的是读、比对、unlink）为后面的段留的时间。
pub const DISCOVERY_RESERVE: Duration = Duration::from_secs(1);
/// 拆运行时（等阻塞任务、丢弃所有任务）的预留：总期限的最后这一段。
pub const RUNTIME_RESERVE: Duration = Duration::from_millis(500);

/// 进程为什么停。只记一次（[`StopLatch`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopVerdict {
    /// 预期的停止：信号、登记客户端要求停止（`instance_conflict`、孤儿超时）、自有引擎失效。
    Stop(StopCause),
    /// 启动失败：门控完成时创建完整的服务体失败，带原因文本。不是任何一种预期的停止：退出码 1，不发 `bye`，停引擎。
    StartupFailed(String),
}

/// 停止原因的一次性槽：先到先得，之后到的原因一概不改它。同时记下锁存的时刻，收尾的期限（[`STOP_DEADLINE`]）从这一刻起算。
#[derive(Debug, Default)]
pub struct StopLatch {
    slot: OnceLock<Latched>,
}

#[derive(Debug)]
struct Latched {
    verdict: StopVerdict,
    at: Instant,
}

impl StopLatch {
    /// 锁存 `verdict`，返回**生效的**那一个：槽是空的就是 `verdict`，已经有了就是先到的那个（`verdict` 被丢掉）。
    pub fn latch(&self, verdict: StopVerdict) -> &StopVerdict {
        &self
            .slot
            .get_or_init(|| Latched {
                verdict,
                at: Instant::now(),
            })
            .verdict
    }

    /// 已经锁存的停止原因；还没有时是 `None`（停止流程没有走到决定这一步：例如服务自己倒了）。
    pub fn get(&self) -> Option<&StopVerdict> {
        self.slot.get().map(|latched| &latched.verdict)
    }

    /// 整个收尾必须完成的期限：锁存那一刻加 [`STOP_DEADLINE`]；还没有锁存时是 `None`（没有信号在催）。
    pub fn deadline(&self) -> Option<Instant> {
        self.slot.get().map(|latched| latched.at + STOP_DEADLINE)
    }
}

/// 收尾各段的绝对结束时刻（H2）：从总期限 `exit` 倒着数，每一段给它后面的几段留出预留，所有段共用这**一个**期限。
///
/// ```text
/// engines (+WAIT_SLACK) ─ REGISTRATION_RESERVE ─ registration ─ DISCOVERY_RESERVE ─ discovery ─ RUNTIME_RESERVE ─ exit
/// ```
///
/// 每一段只等到自己的结束时刻（用的是绝对时刻，不是各自的固定时长）：前面的段早结束，后面的段自然多得时间；前面的段用满了
/// （引擎的确认窗口可以超出它自己的期限 [`WAIT_SLACK`]，所以引擎的期限已经提前了那么多），后面的段至少还有自己的预留，整个
/// 收尾仍在 `exit` 之前结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopBudgets {
    /// 引擎收尾的期限：[`finish_engines`]、引擎管理器与引擎的宽限都按它算。
    pub engines: Instant,
    /// 登记收尾必须完成的时刻。
    pub registration: Instant,
    /// 删发现文件必须完成的时刻：到了还没删成就留着，由下一个实例覆盖。
    pub discovery: Instant,
    /// 整个进程必须退出的时刻（含拆运行时）。
    pub exit: Instant,
}

impl StopBudgets {
    /// 从整个进程必须退出的时刻 `exit` 倒推各段。
    pub fn from_exit(exit: Instant) -> Self {
        // 倒推到平台时钟的起点之前（刚开机十几秒内）不可能发生；真发生了就退回 `exit` 本身：每一段都立刻到期，不 panic。
        let before = |at: Instant, by: Duration| at.checked_sub(by).unwrap_or(at);
        let discovery = before(exit, RUNTIME_RESERVE);
        let registration = before(discovery, DISCOVERY_RESERVE);
        let engines = before(registration, REGISTRATION_RESERVE + WAIT_SLACK);
        Self {
            engines,
            registration,
            discovery,
            exit,
        }
    }

    /// 停止流程锁存了原因时从它起算 [`STOP_DEADLINE`]（`latch.deadline()`）；没有锁存（服务自己倒了，没有信号在催）就从现在起算
    /// 同样长的期限：收尾照样各段有界，不因为没人催就可以无限等。
    pub fn for_latch(latch: &StopLatch) -> Self {
        Self::from_exit(
            latch
                .deadline()
                .unwrap_or_else(|| Instant::now() + STOP_DEADLINE),
        )
    }
}

/// 整个进程必须退出的时刻，由 `run` 在算出收尾预算时记下（[`StopBudgets::exit`]），`main` 在最后拆运行时之前读：拆运行时只等期限里
/// 剩下的，不是固定的 5 秒。
#[derive(Debug, Default)]
pub struct ExitDeadline(OnceLock<std::time::Instant>);

impl ExitDeadline {
    pub fn set(&self, exit: Instant) {
        let _ = self.0.set(exit.into_std());
    }

    /// 拆运行时（等阻塞任务结束）至多等多久：`cap` 与期限里剩下的取小；期限已过就是零（任务照样被丢弃，只是不再等阻塞线程）；
    /// 没有记下期限（没走收尾，例如启动失败）就是 `cap`。
    pub fn runtime_grace(&self, cap: Duration) -> Duration {
        match self.0.get() {
            Some(exit) => exit
                .saturating_duration_since(std::time::Instant::now())
                .min(cap),
            None => cap,
        }
    }
}

/// 停止的各个来源。字段按**优先级从高到低**排列（[`Self::first`] 就按这个顺序轮询），写成命名字段是为了不会把位置参数的
/// 顺序传错（信号与引擎失效的 `Output` 都是 `()`，传反了编译器看不出来）。
///
/// 每个来源都是一个 future：**已经发布的状态在第一次被 poll 时就就绪**（`watch` 里的当前值、已经发出的 oneshot），
/// 没有的来源传 `std::future::pending()`。
pub struct StopSources<E, R, G, S> {
    /// 自有引擎失效（退出码 5）。
    pub engine_failed: E,
    /// 登记客户端要求停止：孤儿超时（4）或 `instance_conflict`（3）。
    pub registration: R,
    /// 门控创建完整服务体失败（启动失败，退出码 1）。
    pub gate_failed: G,
    /// SIGTERM、SIGINT（退出码 0）。
    pub signal: S,
}

impl<E, R, G, S> StopSources<E, R, G, S>
where
    E: Future<Output = ()>,
    R: Future<Output = StopCause>,
    G: Future<Output = String>,
    S: Future<Output = ()>,
{
    /// 等到第一个来源成立，同一轮里有几个同时成立就取优先级最高的那个。
    pub async fn first(self) -> StopVerdict {
        let Self {
            engine_failed,
            registration,
            gate_failed,
            signal,
        } = self;
        // `biased`：同一次 poll 里按书写顺序取第一个就绪的，不随机；已经发布的状态一被 poll 就就绪，所以排在信号前面的永远赢信号。
        tokio::select! {
            biased;
            () = engine_failed => StopVerdict::Stop(StopCause::EngineFailed),
            cause = registration => StopVerdict::Stop(cause),
            message = gate_failed => StopVerdict::StartupFailed(message),
            () = signal => StopVerdict::Stop(StopCause::Signal),
        }
    }
}

/// 引擎在停止流程里的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePlan {
    /// 原因一确定就开始停止自己拥有的引擎，与 HTTP 排空并行（§2.7.5 第 2、3 步）。
    StopNow,
    /// 排空期间保留所有权句柄（引擎照常跑，所有权记录不动）；排空结束、退出结果出来之后才定（[`engines_stop_after_drain`]）。
    /// 只有 `stop_on_exit:"never"` 的信号停止走这条（[`StopCause::stops_owned_engines`] 为假的唯一情形）。
    DecideAfterDrain,
}

/// 怎么通知登记客户端与发现文件自愈。**两种都立刻让自愈停下**（之后不再重写发现文件）；区别只在发不发 `bye`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationAction {
    /// 预期的停止：[`StopCause::says_bye`] 决定发不发 `bye`。
    Stop(StopCause),
    /// 启动失败：不是预期的停止，不发 `bye`（没有通知登记客户端的原因），但登记与自愈也要在排空期间就停：
    /// 否则一个已经在退出的进程还在往发现文件里写「我是活的」（L2）。
    Silent,
}

/// 由**一个** [`StopVerdict`] 推出的收尾安排：通知登记客户端（决定发不发 `bye`）与引擎的处置都从这里来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPlan {
    pub registration: RegistrationAction,
    pub engines: EnginePlan,
}

impl StopPlan {
    pub fn new(verdict: &StopVerdict, stop_on_exit: StopOnExit) -> Self {
        match verdict {
            StopVerdict::Stop(cause) => Self {
                registration: RegistrationAction::Stop(*cause),
                engines: if cause.stops_owned_engines(stop_on_exit) {
                    EnginePlan::StopNow
                } else {
                    EnginePlan::DecideAfterDrain
                },
            },
            // 启动失败：没有对外服务过，留着引擎没有意义，一律停。
            StopVerdict::StartupFailed(_) => Self {
                registration: RegistrationAction::Silent,
                engines: EnginePlan::StopNow,
            },
        }
    }
}

/// [`EnginePlan::DecideAfterDrain`] 的引擎在排空结束之后怎么办：**只有以退出码 0 退出才留给下一个 iDoris**（§2.7.5：保留引擎只限
/// 退出码 0），退出结果是 1（排空超时、订阅收尾失败）就停止并回收，否则会留下一个没人管的引擎。
pub fn engines_stop_after_drain(exit: &Result<u8, String>) -> bool {
    !matches!(exit, Ok(0))
}

/// 引擎管理器对停止流程暴露的两个动作；测试里换成会记录调用的假货。
#[async_trait]
pub trait EngineControl: Send + Sync + 'static {
    /// 自有引擎失效（门控完成之后进程退出）已经发布时立即返回；之前、或永远没有时不返回。
    async fn failed(&self);
    /// 收尾：`stop_engines` 为真就停掉自己拥有的引擎并回收所有权记录；为假就让它继续跑，所有权记录留给下一个 iDoris。
    /// `deadline` 是整个收尾必须完成的期限（[`STOP_DEADLINE`]）：宽限只拿期限里剩下的；`None` 是没有期限。
    async fn shutdown(&self, stop_engines: bool, deadline: Option<Instant>);
}

#[async_trait]
impl EngineControl for EngineManager {
    async fn failed(&self) {
        let _ = self.engine_failed().await;
    }

    async fn shutdown(&self, stop_engines: bool, deadline: Option<Instant>) {
        EngineManager::shutdown_by(self, stop_engines, deadline).await;
    }
}

/// 登记客户端（与发现文件自愈）对停止流程暴露的动作；测试里换成会记录调用的假货。
#[async_trait]
pub trait RegistrationControl: Send + Sync + 'static {
    /// 登记客户端要求停止（孤儿超时、`instance_conflict`）时返回原因；没有时不返回。已经发布的原因第一次被 poll 就就绪。
    async fn wants_stop(&self) -> StopCause;
    /// 预期的停止：自愈立刻停；`cause.says_bye()` 决定发不发 `bye`。立即返回。
    fn begin_stop(&self, cause: StopCause);
    /// 启动失败：自愈与登记客户端立刻停，不发 `bye`。立即返回。
    fn begin_silent_stop(&self);
}

#[async_trait]
impl RegistrationControl for Registration {
    async fn wants_stop(&self) -> StopCause {
        Registration::wants_stop(self).await
    }

    fn begin_stop(&self, cause: StopCause) {
        Registration::begin_stop(self, cause);
    }

    fn begin_silent_stop(&self) {
        Registration::begin_silent_stop(self);
    }
}

/// 停止流程的决定与开始：等到第一个来源成立（[`StopSources::first`]），**先锁存**，再从**锁存下来的那一个**推出收尾安排
/// （L1：不用刚读到的局部值，免得先到的另一个结果与这里的安排各说各的），然后立刻开始：通知登记客户端（发不发 `bye`、自愈停下），
/// 开始停引擎（[`begin_engines`]，句柄放进 `stopping` 交给 [`finish_engines`]）。`bin` 里的停止 future 就是这一个函数，
/// 所以「哪个来源接哪个 future」「`bye` 与引擎处置从哪个结果推出」都能在这里用假的登记客户端与引擎管理器测（L5）。
///
/// `engines`、`registration` 为 `None` 的来源永不成立。
pub async fn decide_and_begin<C, R, G, S>(
    engines: Option<&Arc<C>>,
    registration: Option<&R>,
    gate_failed: G,
    signal: S,
    stop_on_exit: StopOnExit,
    latch: &StopLatch,
    stopping: &Mutex<Option<JoinHandle<()>>>,
) where
    C: EngineControl,
    R: RegistrationControl,
    G: Future<Output = String>,
    S: Future<Output = ()>,
{
    // 引擎失效通常只在门控完成之后才会有（之前走重启与熔断；实例任务自己 panic 是例外，也报在这里）。没有引擎管理器时永不成立。
    let engine_failed = async {
        match engines {
            Some(engines) => engines.failed().await,
            None => std::future::pending::<()>().await,
        }
    };
    let registration_stop = async {
        match registration {
            Some(registration) => registration.wants_stop().await,
            None => std::future::pending::<StopCause>().await,
        }
    };
    // 同一轮里几个来源同时成立：已经发布的 5、4、3 与启动失败排在信号前面，不随机挑（`StopSources::first` 是 `biased` 的）。
    let verdict = StopSources {
        engine_failed,
        registration: registration_stop,
        gate_failed,
        signal,
    }
    .first()
    .await;
    // 先锁存，再从生效的那一个推出 `bye` 与引擎处置：同一个结果驱动一切。
    let verdict = latch.latch(verdict).clone();
    let plan = StopPlan::new(&verdict, stop_on_exit);
    if let Some(registration) = registration {
        match plan.registration {
            RegistrationAction::Stop(cause) => registration.begin_stop(cause),
            RegistrationAction::Silent => registration.begin_silent_stop(),
        }
    }
    // 引擎的停止与 HTTP 排空并行：`stop_on_exit:"never"` 只对退出码 0 有效，退出码 4、5 与 3 一律停（接口规范 §2.7.5）；
    // 启动失败当作要停（没有对外服务过，留着引擎没有意义）。`never` 的信号停止此刻不动，保留所有权句柄。
    if let Some(engines) = engines {
        *stopping.lock().unwrap_or_else(PoisonError::into_inner) = begin_engines(
            engines,
            plan.engines,
            Some(StopBudgets::for_latch(latch).engines),
        );
    }
}

/// 停止原因确定时调用：[`EnginePlan::StopNow`] 就在后台开始停（返回的句柄交给 [`finish_engines`]）；
/// [`EnginePlan::DecideAfterDrain`] 什么也不做，所有权句柄保留。`deadline` 见 [`EngineControl::shutdown`]。
pub fn begin_engines<E: EngineControl>(
    engines: &Arc<E>,
    plan: EnginePlan,
    deadline: Option<Instant>,
) -> Option<JoinHandle<()>> {
    match plan {
        EnginePlan::StopNow => {
            let engines = engines.clone();
            Some(tokio::spawn(async move {
                engines.shutdown(true, deadline).await;
            }))
        }
        EnginePlan::DecideAfterDrain => None,
    }
}

/// 外面这层等待的上限：期限里剩下的加 [`WAIT_SLACK`]（里面确认窗口的余量，不是另一段独立的时间）；没有期限就是 [`OPEN_ENDED_STOP_WAIT`]。
fn wait_budget(deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(deadline) => deadline.saturating_duration_since(Instant::now()) + WAIT_SLACK,
        None => OPEN_ENDED_STOP_WAIT,
    }
}

/// 排空结束、退出结果 `exit` 已经出来之后收尾引擎，至多等到 `deadline`（再加一点余量）。`started` 是 [`begin_engines`] 返回的句柄；
/// `plan` 是当时锁存的停止原因推出的安排（`None` = 没有锁存任何原因：服务自己倒了，没有走停止流程，现在才停，没有期限）。
///
/// 等不到（超时）或后台的停止任务异常结束（panic）时各写一行 `log`（不含任何秘密），所有权记录保留，下一个 iDoris 认回并收尾
/// （L7：不能悄悄吞掉）。
pub async fn finish_engines<E: EngineControl>(
    engines: &E,
    started: Option<JoinHandle<()>>,
    plan: Option<EnginePlan>,
    exit: &Result<u8, String>,
    deadline: Option<Instant>,
    log: &dyn Fn(&str),
) {
    let wait = wait_budget(deadline);
    let timed_out = || {
        log(&format!(
            "[idoris] 停止自有引擎超过 {:.1} 秒仍未完成，不再等待（所有权记录保留，下一个 iDoris 会认回并收尾）",
            wait.as_secs_f64()
        ));
    };
    match (started, plan) {
        // 已经在后台停了（与排空并行）：等它做完。
        (Some(handle), _) => match tokio::time::timeout(wait, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => log(
                "[idoris] 停止自有引擎的任务异常结束（panic）：所有权记录保留，下一个 iDoris 会认回并收尾",
            ),
            Err(_) => timed_out(),
        },
        // `never` 的信号停止：现在退出结果出来了，以 0 退出才放手，否则停止并回收。
        (None, Some(EnginePlan::DecideAfterDrain)) => {
            let stop = engines_stop_after_drain(exit);
            if tokio::time::timeout(wait, engines.shutdown(stop, deadline))
                .await
                .is_err()
            {
                timed_out();
            }
        }
        // 没有锁存原因（服务自己倒了），或 `StopNow` 却没有句柄（不应该发生）：现在才停。
        (None, _) => {
            if tokio::time::timeout(wait, engines.shutdown(true, deadline))
                .await
                .is_err()
            {
                timed_out();
            }
        }
    }
}

/// SIGINT 与（unix）SIGTERM，**启动一开始就注册处理器**（[`Self::install`]），三种读法：同步（[`Self::is_set`]，给阻塞的等待每一小段看一眼）、
/// 异步（[`Self::wait`]，给停止流程）、克隆之后随便传。
///
/// 旧写法是一个 `async fn`，处理器在第一次被 poll 时才注册；而 `select!` 在更靠前的来源已经就绪时不 poll 信号那个分支，
/// 启动期间（注册之前）到的 SIGTERM 按默认动作直接杀死进程，来不及走优雅停止。现在处理器注册之后到的信号由一个后台任务读到、
/// 置位；之后任何一种读法都读得到。**只记「到过」**，不记次数：之后的信号一概是同一次停止。
#[derive(Clone)]
pub struct StopSignal {
    state: Arc<SignalState>,
}

#[derive(Default)]
struct SignalState {
    set: AtomicBool,
    wake: Notify,
}

impl SignalState {
    fn raise(&self) {
        self.set.store(true, Ordering::SeqCst);
        // `notify_waiters` 不留许可：已经在等的被唤醒，之后才来等的由 `wait` 里的二次检查接住。
        self.wake.notify_waiters();
    }
}

impl StopSignal {
    /// 没有挂任何操作系统信号的实例：[`Self::raise`] 才置位。单元测试与诊断用。
    pub fn manual() -> Self {
        Self {
            state: Arc::new(SignalState::default()),
        }
    }

    /// 置位，效果与收到信号相同。
    pub fn raise(&self) {
        self.state.raise();
    }

    /// 立刻注册 SIGINT 与 SIGTERM 的处理器（必须在 tokio 运行时里调用）。注册完成之后到的信号不会再按默认动作杀死进程。
    #[cfg(unix)]
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let this = Self::manual();
        let state = this.state.clone();
        tokio::spawn(async move {
            let arrived = tokio::select! {
                arrived = interrupt.recv() => arrived,
                arrived = terminate.recv() => arrived,
            };
            // `None`：信号驱动已经关了（运行时在收摊），不会再有信号。
            if arrived.is_some() {
                state.raise();
            }
        });
        Ok(this)
    }

    #[cfg(not(unix))]
    pub fn install() -> std::io::Result<Self> {
        let this = Self::manual();
        let state = this.state.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                state.raise();
            }
        });
        Ok(this)
    }

    /// 信号到过了吗。便宜、不阻塞，阻塞的等待（等目录锁、等 stdin）每一小段调一次。
    pub fn is_set(&self) -> bool {
        self.state.set.load(Ordering::SeqCst)
    }

    /// 等到信号到过（已经到过就立即返回）。可以被取消，可以有多个等待者。
    pub async fn wait(&self) {
        loop {
            let notified = self.state.wake.notified();
            tokio::pin!(notified);
            // 先登记、再检查：置位发生在两者之间也不会丢。
            notified.as_mut().enable();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::future::pending;

    use tokio::sync::Notify;

    use super::*;

    /// 每个来源的就绪与否：`true` 就绪（已经发布的状态），`false` 永不就绪。
    fn sources(
        engine: bool,
        registration: Option<StopCause>,
        gate: bool,
        signal: bool,
    ) -> StopSources<
        impl Future<Output = ()>,
        impl Future<Output = StopCause>,
        impl Future<Output = String>,
        impl Future<Output = ()>,
    > {
        StopSources {
            engine_failed: async move {
                if !engine {
                    pending::<()>().await;
                }
            },
            registration: async move {
                match registration {
                    Some(cause) => cause,
                    None => pending::<StopCause>().await,
                }
            },
            gate_failed: async move {
                if gate {
                    "gate".to_string()
                } else {
                    pending::<String>().await
                }
            },
            signal: async move {
                if !signal {
                    pending::<()>().await;
                }
            },
        }
    }

    /// 参照排序：与实现无关地写出规范里的优先级。
    fn rank(verdict: &StopVerdict) -> u8 {
        match verdict {
            StopVerdict::Stop(StopCause::EngineFailed) => 4,
            StopVerdict::Stop(StopCause::OrphanTimeout) => 3,
            StopVerdict::Stop(StopCause::InstanceConflict) => 2,
            StopVerdict::StartupFailed(_) => 1,
            StopVerdict::Stop(StopCause::Signal) => 0,
        }
    }

    /// 同一轮里任意几个来源同时就绪：结果永远是其中优先级最高的（5 > 4 > 3 > 启动失败 > 信号）。每种组合重复多次：
    /// 去掉 `biased` 之后 `select!` 是随机挑，单次碰巧挑对的概率不小，重复几十次才压得住。
    #[tokio::test]
    async fn the_highest_priority_among_simultaneously_ready_sources_always_wins() {
        let mut checked = 0;
        for engine in [false, true] {
            for registration in [
                None,
                Some(StopCause::InstanceConflict),
                Some(StopCause::OrphanTimeout),
            ] {
                for gate in [false, true] {
                    for signal in [false, true] {
                        if !engine && registration.is_none() && !gate && !signal {
                            continue;
                        }
                        let mut expected = Vec::new();
                        if engine {
                            expected.push(StopVerdict::Stop(StopCause::EngineFailed));
                        }
                        if let Some(cause) = registration {
                            expected.push(StopVerdict::Stop(cause));
                        }
                        if gate {
                            expected.push(StopVerdict::StartupFailed("gate".to_string()));
                        }
                        if signal {
                            expected.push(StopVerdict::Stop(StopCause::Signal));
                        }
                        let expected = expected.into_iter().max_by_key(rank).unwrap();
                        for round in 0..64 {
                            let got = sources(engine, registration, gate, signal).first().await;
                            assert_eq!(
                                got, expected,
                                "round {round}: engine={engine} registration={registration:?} gate={gate} signal={signal}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(
            checked, 23,
            "2 x 3 x 2 x 2 combinations minus the empty one"
        );
    }

    /// 单独一个来源成立就是它自己（信号也是）；全不成立时不返回。
    #[tokio::test]
    async fn a_lone_source_is_the_verdict_and_nothing_ready_never_returns() {
        assert_eq!(
            sources(false, None, false, true).first().await,
            StopVerdict::Stop(StopCause::Signal)
        );
        assert_eq!(
            sources(true, None, false, false).first().await,
            StopVerdict::Stop(StopCause::EngineFailed)
        );
        assert_eq!(
            sources(false, Some(StopCause::OrphanTimeout), false, false)
                .first()
                .await,
            StopVerdict::Stop(StopCause::OrphanTimeout)
        );
        assert_eq!(
            sources(false, None, true, false).first().await,
            StopVerdict::StartupFailed("gate".to_string())
        );
        let waiting = tokio::time::timeout(
            Duration::from_millis(50),
            sources(false, None, false, false).first(),
        )
        .await;
        assert!(waiting.is_err(), "no source is ready: keep waiting");
    }

    /// 来源先后到达（不是同一轮）：先到的先得。信号先到并被锁存之后，再来的 5、4、3 不改结果。
    #[test]
    fn the_latch_keeps_the_first_cause_and_ignores_later_ones() {
        let latch = StopLatch::default();
        assert_eq!(latch.get(), None);
        assert_eq!(
            latch.latch(StopVerdict::Stop(StopCause::Signal)),
            &StopVerdict::Stop(StopCause::Signal)
        );
        for later in [
            StopVerdict::Stop(StopCause::EngineFailed),
            StopVerdict::Stop(StopCause::OrphanTimeout),
            StopVerdict::StartupFailed("late".to_string()),
        ] {
            assert_eq!(
                latch.latch(later),
                &StopVerdict::Stop(StopCause::Signal),
                "first come, first served"
            );
        }
        assert_eq!(latch.get(), Some(&StopVerdict::Stop(StopCause::Signal)));
    }

    /// 真并发：几个线程同时锁存各不相同的原因，所有线程读到的是同一个赢家（先到先得，不是「检查再写」两步，中间没有缝）。
    #[test]
    fn concurrent_latches_agree_on_one_winner() {
        let causes = [
            StopCause::Signal,
            StopCause::InstanceConflict,
            StopCause::OrphanTimeout,
            StopCause::EngineFailed,
        ];
        for round in 0..200 {
            let latch = Arc::new(StopLatch::default());
            let barrier = Arc::new(std::sync::Barrier::new(causes.len()));
            let workers: Vec<_> = causes
                .into_iter()
                .map(|cause| {
                    let (latch, barrier) = (latch.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        latch.latch(StopVerdict::Stop(cause)).clone()
                    })
                })
                .collect();
            let seen: Vec<StopVerdict> = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            let winner = latch.get().unwrap().clone();
            assert!(
                seen.iter().all(|verdict| *verdict == winner),
                "round {round}: {seen:?} vs {winner:?}"
            );
        }
    }

    fn cause_of(plan: &StopPlan) -> StopCause {
        match plan.registration {
            RegistrationAction::Stop(cause) => cause,
            RegistrationAction::Silent => panic!("no cause in a silent stop: {plan:?}"),
        }
    }

    /// `bye` 与引擎处置从同一个结果推出：同一轮里信号与别的原因同时成立，退出码、`bye`、引擎处置都是优先级最高的那个的。
    #[tokio::test]
    async fn bye_and_the_engine_plan_follow_the_one_verdict_even_when_the_signal_was_ready_too() {
        // 引擎失效（5）加信号，`never`：5 一律停引擎（即使 `never`），并且发 `bye`；不是信号那条「保留」的路。
        let verdict = sources(true, None, false, true).first().await;
        assert_eq!(verdict, StopVerdict::Stop(StopCause::EngineFailed));
        let plan = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(
            plan,
            StopPlan {
                registration: RegistrationAction::Stop(StopCause::EngineFailed),
                engines: EnginePlan::StopNow
            }
        );
        assert!(cause_of(&plan).says_bye());
        assert_eq!(cause_of(&plan).exit_code(), 5);

        // `instance_conflict`（3）加信号，`never`：3 停引擎、不发 `bye`、退出码 3。
        let verdict = sources(false, Some(StopCause::InstanceConflict), false, true)
            .first()
            .await;
        let plan = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(plan.engines, EnginePlan::StopNow);
        assert!(!cause_of(&plan).says_bye());
        assert_eq!(cause_of(&plan).exit_code(), 3);

        // 孤儿超时（4）加信号：同理，退出码 4，不发 `bye`。
        let verdict = sources(false, Some(StopCause::OrphanTimeout), false, true)
            .first()
            .await;
        let plan = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(plan.engines, EnginePlan::StopNow);
        assert!(!cause_of(&plan).says_bye());
        assert_eq!(cause_of(&plan).exit_code(), 4);

        // 只有信号：`owned` 立刻停，`never` 保留到排空之后；都发 `bye`。
        let verdict = sources(false, None, false, true).first().await;
        let owned = StopPlan::new(&verdict, StopOnExit::Owned);
        assert_eq!(owned.engines, EnginePlan::StopNow);
        let never = StopPlan::new(&verdict, StopOnExit::Never);
        assert_eq!(never.engines, EnginePlan::DecideAfterDrain);
        assert!(cause_of(&never).says_bye());

        // 启动失败：不是预期的停止，不发 `bye`，但登记与自愈也立刻停下（静默停止，L2）；一律立刻停引擎，与 `stop_on_exit` 无关。
        for mode in [StopOnExit::Owned, StopOnExit::Never] {
            assert_eq!(
                StopPlan::new(&StopVerdict::StartupFailed("x".into()), mode),
                StopPlan {
                    registration: RegistrationAction::Silent,
                    engines: EnginePlan::StopNow
                }
            );
        }
    }

    #[test]
    fn engines_are_kept_only_for_a_clean_exit_zero() {
        assert!(!engines_stop_after_drain(&Ok(0)));
        // 退出码 1（排空失败的 `Err`）以及任何别的退出码：停止并回收。
        assert!(engines_stop_after_drain(&Err(
            "优雅停机超过 10s".to_string()
        )));
        for code in [1, 3, 4, 5] {
            assert!(engines_stop_after_drain(&Ok(code)), "{code}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // 假的引擎管理器与登记客户端
    // -----------------------------------------------------------------------------------------

    /// 假引擎管理器的 `shutdown` 怎么表现。
    #[derive(Clone, Copy, Default)]
    enum ShutdownBehavior {
        /// 立刻做完。
        #[default]
        Return,
        /// 永远不返回（引擎卡住了）。
        Hang,
        /// panic（实例任务里的 bug）。
        Panic,
    }

    /// 记录调用的假引擎管理器。`log` 里除了 `shutdown(..)`，测试还往里放「排空」的标记，以便断言先后。
    #[derive(Default)]
    struct FakeEngines {
        log: Mutex<Vec<String>>,
        /// 每次 `shutdown` 收到的期限。
        deadlines: Mutex<Vec<Option<Instant>>>,
        /// `shutdown` 一进来就通知：测试等的是这个事件（后台任务确实开始停了），不是让出几轮调度碰运气。
        entered: Notify,
        /// `failed()` 是否已经成立（自有引擎失效已经发布）。
        failed: bool,
        behavior: ShutdownBehavior,
    }

    impl FakeEngines {
        fn failing() -> Self {
            Self {
                failed: true,
                ..Self::default()
            }
        }

        fn with(behavior: ShutdownBehavior) -> Self {
            Self {
                behavior,
                ..Self::default()
            }
        }

        fn note(&self, line: &str) {
            self.log.lock().unwrap().push(line.to_string());
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn deadlines(&self) -> Vec<Option<Instant>> {
            self.deadlines.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EngineControl for FakeEngines {
        async fn failed(&self) {
            if !self.failed {
                pending::<()>().await;
            }
        }

        async fn shutdown(&self, stop_engines: bool, deadline: Option<Instant>) {
            self.note(&format!("shutdown({stop_engines})"));
            self.deadlines.lock().unwrap().push(deadline);
            self.entered.notify_one();
            match self.behavior {
                ShutdownBehavior::Return => {}
                ShutdownBehavior::Hang => pending::<()>().await,
                ShutdownBehavior::Panic => panic!("a bug in an instance task"),
            }
        }
    }

    /// 记录调用的假登记客户端：`wants` 是 `wants_stop()` 已经发布的原因。
    #[derive(Default)]
    struct FakeRegistration {
        wants: Option<StopCause>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRegistration {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RegistrationControl for FakeRegistration {
        async fn wants_stop(&self) -> StopCause {
            match self.wants {
                Some(cause) => cause,
                None => pending::<StopCause>().await,
            }
        }

        fn begin_stop(&self, cause: StopCause) {
            self.calls.lock().unwrap().push(format!("stop({cause:?})"));
        }

        fn begin_silent_stop(&self) {
            self.calls.lock().unwrap().push("silent".to_string());
        }
    }

    fn quiet(_: &str) {}

    /// 一轮决定的输入：哪几个来源已经成立，`stop_on_exit`。
    struct Case {
        name: &'static str,
        engine_failed: bool,
        registration: Option<StopCause>,
        gate: bool,
        signal: bool,
        mode: StopOnExit,
        /// 期望锁存下来的结果。
        verdict: StopVerdict,
        /// 期望对登记客户端做的那一个动作。
        registration_call: &'static str,
        /// 期望原因一确定就开始停引擎（`Some`：那一次 `shutdown(..)` 调用）；`None`：此刻不动（保留所有权句柄）。
        engines_now: Option<&'static str>,
    }

    fn cases() -> Vec<Case> {
        let stop = |cause| StopVerdict::Stop(cause);
        vec![
            Case {
                name: "signal alone, owned",
                engine_failed: false,
                registration: None,
                gate: false,
                signal: true,
                mode: StopOnExit::Owned,
                verdict: stop(StopCause::Signal),
                registration_call: "stop(Signal)",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "signal alone, never: the handle is kept through the drain",
                engine_failed: false,
                registration: None,
                gate: false,
                signal: true,
                mode: StopOnExit::Never,
                verdict: stop(StopCause::Signal),
                registration_call: "stop(Signal)",
                engines_now: None,
            },
            // 只有一个来源成立：错接线（引擎失效接成信号、信号接成引擎失效……）在这里就红，两者同时成立的行分不出来。
            Case {
                name: "engine failure alone, never: stopped anyway",
                engine_failed: true,
                registration: None,
                gate: false,
                signal: false,
                mode: StopOnExit::Never,
                verdict: stop(StopCause::EngineFailed),
                registration_call: "stop(EngineFailed)",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "orphan timeout alone, never: stopped anyway",
                engine_failed: false,
                registration: Some(StopCause::OrphanTimeout),
                gate: false,
                signal: false,
                mode: StopOnExit::Never,
                verdict: stop(StopCause::OrphanTimeout),
                registration_call: "stop(OrphanTimeout)",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "instance conflict alone, never: stopped anyway",
                engine_failed: false,
                registration: Some(StopCause::InstanceConflict),
                gate: false,
                signal: false,
                mode: StopOnExit::Never,
                verdict: stop(StopCause::InstanceConflict),
                registration_call: "stop(InstanceConflict)",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "gate failure alone: silent registration stop, engines stopped",
                engine_failed: false,
                registration: None,
                gate: true,
                signal: false,
                mode: StopOnExit::Never,
                verdict: StopVerdict::StartupFailed("gate".to_string()),
                registration_call: "silent",
                engines_now: Some("shutdown(true)"),
            },
            // 同一轮里几个同时成立：优先级最高的赢，`bye` 与引擎处置都是它的。
            Case {
                name: "engine failure beats the signal that was ready too",
                engine_failed: true,
                registration: None,
                gate: false,
                signal: true,
                mode: StopOnExit::Never,
                verdict: stop(StopCause::EngineFailed),
                registration_call: "stop(EngineFailed)",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "instance conflict beats the signal that was ready too",
                engine_failed: false,
                registration: Some(StopCause::InstanceConflict),
                gate: false,
                signal: true,
                mode: StopOnExit::Never,
                verdict: stop(StopCause::InstanceConflict),
                registration_call: "stop(InstanceConflict)",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "gate failure beats the signal that was ready too",
                engine_failed: false,
                registration: None,
                gate: true,
                signal: true,
                mode: StopOnExit::Never,
                verdict: StopVerdict::StartupFailed("gate".to_string()),
                registration_call: "silent",
                engines_now: Some("shutdown(true)"),
            },
            Case {
                name: "everything ready at once: 5 wins",
                engine_failed: true,
                registration: Some(StopCause::OrphanTimeout),
                gate: true,
                signal: true,
                mode: StopOnExit::Owned,
                verdict: stop(StopCause::EngineFailed),
                registration_call: "stop(EngineFailed)",
                engines_now: Some("shutdown(true)"),
            },
        ]
    }

    /// L5：从「哪个来源接哪个 future」到「`bye` 与引擎处置」的整条接线，用假的登记客户端与引擎管理器测。
    #[tokio::test]
    async fn the_decision_wiring_picks_the_verdict_and_starts_exactly_the_stop_that_verdict_calls_for()
     {
        for case in cases() {
            for round in 0..16 {
                let latch = StopLatch::default();
                let registration = FakeRegistration {
                    wants: case.registration,
                    ..FakeRegistration::default()
                };
                let engines = Arc::new(if case.engine_failed {
                    FakeEngines::failing()
                } else {
                    FakeEngines::default()
                });
                let stopping = Mutex::new(None);
                decide_and_begin(
                    Some(&engines),
                    Some(&registration),
                    async {
                        if case.gate {
                            "gate".to_string()
                        } else {
                            pending::<String>().await
                        }
                    },
                    async {
                        if !case.signal {
                            pending::<()>().await;
                        }
                    },
                    case.mode,
                    &latch,
                    &stopping,
                )
                .await;
                let label = format!("{} (round {round})", case.name);
                assert_eq!(latch.get(), Some(&case.verdict), "{label}");
                assert_eq!(registration.calls(), [case.registration_call], "{label}");
                let started = stopping.lock().unwrap().take();
                match case.engines_now {
                    Some(call) => {
                        started
                            .unwrap_or_else(|| panic!("{label}: the stop did not start"))
                            .await
                            .unwrap();
                        assert_eq!(engines.log(), [call], "{label}");
                        // 引擎拿到的就是锁存下来的那个期限里属于它的那一段（H2：总期限往回数掉后面几段的预留）。
                        assert_eq!(
                            engines.deadlines(),
                            [Some(StopBudgets::for_latch(&latch).engines)],
                            "{label}"
                        );
                    }
                    None => {
                        assert!(started.is_none(), "{label}: nothing runs during the drain");
                        assert_eq!(engines.log(), Vec::<String>::new(), "{label}");
                    }
                }
            }
        }
    }

    /// L1：`bye` 与引擎处置从**锁存下来的**结果推出，不是这一次刚读到的局部值。先到的是孤儿超时（4）：这一次读到的信号输了，
    /// 不发 `bye`、引擎立刻停（即使 `never`），用局部值会照信号（发 `bye`、`never` 保留引擎）走。
    #[tokio::test]
    async fn the_plan_follows_the_latched_verdict_not_the_one_this_call_happened_to_read() {
        let latch = StopLatch::default();
        latch.latch(StopVerdict::Stop(StopCause::OrphanTimeout));
        let registration = FakeRegistration::default();
        let engines = Arc::new(FakeEngines::default());
        let stopping = Mutex::new(None);
        decide_and_begin(
            Some(&engines),
            Some(&registration),
            pending::<String>(),
            async {},
            StopOnExit::Never,
            &latch,
            &stopping,
        )
        .await;
        assert_eq!(
            latch.get(),
            Some(&StopVerdict::Stop(StopCause::OrphanTimeout)),
            "the first latched cause stands"
        );
        assert_eq!(registration.calls(), ["stop(OrphanTimeout)"]);
        let started = stopping.lock().unwrap().take();
        started
            .expect("an orphan timeout stops the engines at once, whatever stop_on_exit says")
            .await
            .unwrap();
        assert_eq!(engines.log(), ["shutdown(true)"]);
    }

    /// 没有引擎管理器、没有登记客户端（没有托管卡、没有发现文件）：信号照样决定，没有任何东西可通知。
    #[tokio::test]
    async fn without_engines_or_registration_the_signal_still_decides() {
        let latch = StopLatch::default();
        let stopping = Mutex::new(None);
        decide_and_begin(
            None::<&Arc<FakeEngines>>,
            None::<&FakeRegistration>,
            pending::<String>(),
            async {},
            StopOnExit::Owned,
            &latch,
            &stopping,
        )
        .await;
        assert_eq!(latch.get(), Some(&StopVerdict::Stop(StopCause::Signal)));
        assert!(stopping.lock().unwrap().is_none());
    }

    // -----------------------------------------------------------------------------------------
    // 引擎的收尾（H5）
    // -----------------------------------------------------------------------------------------

    const WAIT: Duration = Duration::from_secs(5);

    /// H5：`never` 的信号停止，排空期间引擎一个动作也没有（所有权句柄保留）；排空成功（以 0 退出）才移交（不停）。
    #[tokio::test]
    async fn never_keeps_the_handle_during_the_drain_and_hands_over_only_after_a_clean_drain() {
        let engines = Arc::new(FakeEngines::default());
        let plan = StopPlan::new(&StopVerdict::Stop(StopCause::Signal), StopOnExit::Never);
        let started = begin_engines(&engines, plan.engines, None);
        assert!(started.is_none(), "nothing runs in parallel with the drain");
        engines.note("drain");
        // 没有后台任务，所以没有什么可等的：此刻引擎一个动作也没有，是结构上的，不是让出几轮调度碰运气。
        assert_eq!(
            engines.log(),
            ["drain"],
            "no stop before the drain result is known"
        );
        engines.note("drain-ok");
        finish_engines(
            engines.as_ref(),
            started,
            Some(plan.engines),
            &Ok(0),
            None,
            &quiet,
        )
        .await;
        assert_eq!(engines.log(), ["drain", "drain-ok", "shutdown(false)"]);
    }

    /// H5：同一条路径，排空失败（以退出码 1 退出）：停止并回收，不留一个没人管的引擎。
    #[tokio::test]
    async fn never_stops_and_reclaims_the_engine_when_the_drain_fails() {
        let engines = Arc::new(FakeEngines::default());
        let plan = StopPlan::new(&StopVerdict::Stop(StopCause::Signal), StopOnExit::Never);
        let started = begin_engines(&engines, plan.engines, None);
        assert!(started.is_none());
        engines.note("drain-failed");
        finish_engines(
            engines.as_ref(),
            started,
            Some(plan.engines),
            &Err("优雅停机超过 10s，强制退出".to_string()),
            None,
            &quiet,
        )
        .await;
        assert_eq!(engines.log(), ["drain-failed", "shutdown(true)"]);
    }

    /// 其余情形原因一确定就停（与排空并行）：`finish_engines` 只等那个已经在跑的任务，不再发第二次 `shutdown`。
    #[tokio::test]
    async fn every_other_cause_stops_engines_at_once_and_finish_only_waits_for_that() {
        for (verdict, mode) in [
            (StopVerdict::Stop(StopCause::Signal), StopOnExit::Owned),
            (
                StopVerdict::Stop(StopCause::EngineFailed),
                StopOnExit::Never,
            ),
            (
                StopVerdict::Stop(StopCause::OrphanTimeout),
                StopOnExit::Never,
            ),
            (
                StopVerdict::Stop(StopCause::InstanceConflict),
                StopOnExit::Never,
            ),
            (StopVerdict::StartupFailed("x".into()), StopOnExit::Never),
        ] {
            let engines = Arc::new(FakeEngines::default());
            let plan = StopPlan::new(&verdict, mode);
            let started = begin_engines(&engines, plan.engines, None);
            assert!(started.is_some(), "{verdict:?} {mode:?}: stops in parallel");
            // 与排空并行：停止在「排空结束」之前就已经开始。等的是事件（后台任务进了 `shutdown`），之后才记下「排空结束」，
            // 所以日志里的先后是因果保证的；从不开始的实现会在这里超时，而不是靠让出几轮调度碰运气。
            tokio::time::timeout(WAIT, engines.entered.notified())
                .await
                .unwrap_or_else(|_| {
                    panic!("{verdict:?} {mode:?}: the stop never began on its own")
                });
            engines.note("drain-finished");
            finish_engines(
                engines.as_ref(),
                started,
                Some(plan.engines),
                &Ok(match plan.registration {
                    RegistrationAction::Stop(cause) => cause.exit_code(),
                    RegistrationAction::Silent => 1,
                }),
                None,
                &quiet,
            )
            .await;
            assert_eq!(
                engines.log(),
                ["shutdown(true)", "drain-finished"],
                "{verdict:?} {mode:?}: one stop, started before the drain finished"
            );
        }
    }

    /// 停止流程没走到决定这一步（服务自己倒了）：没有原因、没有句柄，现在才停。
    #[tokio::test]
    async fn a_server_that_died_on_its_own_still_gets_its_engines_stopped() {
        let engines = FakeEngines::default();
        finish_engines(
            &engines,
            None,
            None,
            &Err("data server exited".to_string()),
            None,
            &quiet,
        )
        .await;
        assert_eq!(engines.log(), ["shutdown(true)"]);
    }

    // -----------------------------------------------------------------------------------------
    // 总期限（M1）与收尾的日志（L7）：用暂停的时钟，不真等
    // -----------------------------------------------------------------------------------------

    /// 期限从**第一次**锁存起算（之后到的原因不挪它），常量是 launcher 的 17 秒减去 1 秒的余量；引擎拿到的是各段预算里**引擎那一段**的期限
    /// （总期限往回数掉后面几段的预留，H2），不是总期限本身。
    #[tokio::test(start_paused = true)]
    async fn the_deadline_counts_from_the_first_latch_and_the_engines_are_given_their_share_of_it()
    {
        assert_eq!(STOP_DEADLINE, Duration::from_secs(16));
        assert_eq!(OPEN_ENDED_STOP_WAIT, Duration::from_secs(14));
        let latch = StopLatch::default();
        assert_eq!(latch.deadline(), None, "nothing latched, nothing urges");
        let t0 = Instant::now();
        let engines = Arc::new(FakeEngines::default());
        let stopping = Mutex::new(None);
        decide_and_begin(
            Some(&engines),
            None::<&FakeRegistration>,
            pending::<String>(),
            async {},
            StopOnExit::Owned,
            &latch,
            &stopping,
        )
        .await;
        assert_eq!(latch.deadline(), Some(t0 + STOP_DEADLINE));
        let started = stopping.lock().unwrap().take();
        started.unwrap().await.unwrap();
        assert_eq!(
            engines.deadlines(),
            [Some(StopBudgets::from_exit(t0 + STOP_DEADLINE).engines)]
        );
        // 之后到的原因不挪期限。
        tokio::time::advance(Duration::from_secs(5)).await;
        latch.latch(StopVerdict::Stop(StopCause::EngineFailed));
        assert_eq!(latch.deadline(), Some(t0 + STOP_DEADLINE));
    }

    /// 引擎卡住了：`finish_engines` 只等期限里剩下的加一点余量，不是固定的 14 秒；等不到就写一行日志（每条路径各一行）。
    #[tokio::test(start_paused = true)]
    async fn finish_waits_only_for_the_time_left_before_the_deadline_and_says_so_when_it_gives_up()
    {
        for branch in ["background stop", "decided after the drain", "no verdict"] {
            let engines = Arc::new(FakeEngines::with(ShutdownBehavior::Hang));
            let lines = Mutex::new(Vec::<String>::new());
            let sink = |line: &str| lines.lock().unwrap().push(line.to_string());
            let t0 = Instant::now();
            // 排空占掉了 10 秒，期限里还剩 6 秒。
            let deadline = t0 + Duration::from_secs(6);
            let (started, plan) = match branch {
                "background stop" => (
                    begin_engines(&engines, EnginePlan::StopNow, Some(deadline)),
                    Some(EnginePlan::StopNow),
                ),
                "decided after the drain" => (None, Some(EnginePlan::DecideAfterDrain)),
                _ => (None, None),
            };
            finish_engines(
                engines.as_ref(),
                started,
                plan,
                &Err("drain failed".to_string()),
                Some(deadline),
                &sink,
            )
            .await;
            assert_eq!(
                t0.elapsed(),
                Duration::from_millis(6600),
                "{branch}: 6 s left plus the one confirmation window of slack, not a fixed 14 s and not 1.5 s past the deadline"
            );
            let lines = lines.lock().unwrap().clone();
            assert_eq!(lines.len(), 1, "{branch}: exactly one line: {lines:?}");
            assert!(
                lines[0].starts_with("[idoris] 停止自有引擎超过 6.6 秒"),
                "{lines:?}"
            );
            assert!(lines[0].contains("所有权记录保留"), "{lines:?}");
            // 引擎拿到的就是这个期限（不是自己另算一个）。
            assert_eq!(engines.deadlines(), [Some(deadline)], "{branch}");
        }
    }

    /// 没有期限（服务自己倒了，没有信号在催）：照原来的 14 秒上限；期限已经过了：只剩余量。
    #[tokio::test(start_paused = true)]
    async fn without_a_deadline_the_wait_is_the_open_ended_bound_and_a_spent_deadline_leaves_only_the_slack()
     {
        let engines = FakeEngines::with(ShutdownBehavior::Hang);
        let t0 = Instant::now();
        finish_engines(&engines, None, None, &Err("died".to_string()), None, &quiet).await;
        assert_eq!(t0.elapsed(), OPEN_ENDED_STOP_WAIT);
        let t1 = Instant::now();
        let spent = t1 - Duration::from_secs(3);
        finish_engines(
            &engines,
            None,
            None,
            &Err("died".to_string()),
            Some(spent),
            &quiet,
        )
        .await;
        assert_eq!(t1.elapsed(), WAIT_SLACK);
    }

    /// L7：后台的停止任务 panic（实例任务里的 bug）也要写一行，不能悄悄吞掉；一切正常时一行也不写。
    #[tokio::test]
    async fn a_panicking_background_stop_is_logged_and_a_clean_finish_logs_nothing() {
        let engines = Arc::new(FakeEngines::with(ShutdownBehavior::Panic));
        let started = begin_engines(&engines, EnginePlan::StopNow, None);
        let lines = Mutex::new(Vec::<String>::new());
        let sink = |line: &str| lines.lock().unwrap().push(line.to_string());
        finish_engines(
            engines.as_ref(),
            started,
            Some(EnginePlan::StopNow),
            &Ok(0),
            None,
            &sink,
        )
        .await;
        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("panic"), "{lines:?}");
        assert!(lines[0].contains("所有权记录保留"), "{lines:?}");

        // 正常收尾：一行也不写（三条路径）。
        for plan in [
            Some(EnginePlan::StopNow),
            Some(EnginePlan::DecideAfterDrain),
            None,
        ] {
            let engines = Arc::new(FakeEngines::default());
            let started = match plan {
                Some(EnginePlan::StopNow) => begin_engines(&engines, EnginePlan::StopNow, None),
                _ => None,
            };
            let lines = Mutex::new(Vec::<String>::new());
            let sink = |line: &str| lines.lock().unwrap().push(line.to_string());
            finish_engines(engines.as_ref(), started, plan, &Ok(0), None, &sink).await;
            assert_eq!(
                lines.lock().unwrap().clone(),
                Vec::<String>::new(),
                "{plan:?}"
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // 总期限的各段预算（H2）
    // -----------------------------------------------------------------------------------------

    /// 各段结束时刻是从总期限倒着数的，常量写死在这里：引擎的期限比总期限早 [`REGISTRATION_RESERVE`] + [`WAIT_SLACK`] + [`DISCOVERY_RESERVE`] +
    /// [`RUNTIME_RESERVE`]，引擎的确认窗口超出它自己期限的 [`WAIT_SLACK`] 之后，登记、发现文件、运行时三段各自仍有自己的预留。
    #[tokio::test(start_paused = true)]
    async fn the_stages_share_one_deadline_and_each_keeps_the_reserve_of_the_stages_after_it() {
        let t0 = Instant::now();
        let budgets = StopBudgets::from_exit(t0 + STOP_DEADLINE);
        assert_eq!(budgets.exit, t0 + Duration::from_secs(16));
        assert_eq!(budgets.discovery, t0 + Duration::from_millis(15_500));
        assert_eq!(budgets.registration, t0 + Duration::from_millis(14_500));
        assert_eq!(budgets.engines, t0 + Duration::from_millis(13_200));
        // 引擎最慢也是：期限加上确认窗口的余量；这时登记还剩它自己的预留。
        assert_eq!(
            budgets.registration - (budgets.engines + WAIT_SLACK),
            REGISTRATION_RESERVE
        );
        assert_eq!(budgets.discovery - budgets.registration, DISCOVERY_RESERVE);
        assert_eq!(budgets.exit - budgets.discovery, RUNTIME_RESERVE);
        // launcher 在 SIGTERM 之后 17 秒 SIGKILL：最后一段也在它之前。
        assert!(budgets.exit < t0 + Duration::from_secs(17));
    }

    /// 没有锁存原因（服务自己倒了）：从现在起算同样的期限；锁存了就从锁存那一刻起算，之后再过多久都不挪。
    #[tokio::test(start_paused = true)]
    async fn the_budgets_count_from_the_latch_or_from_now_when_nothing_was_latched() {
        let latch = StopLatch::default();
        let t0 = Instant::now();
        assert_eq!(
            StopBudgets::for_latch(&latch),
            StopBudgets::from_exit(t0 + STOP_DEADLINE)
        );
        latch.latch(StopVerdict::Stop(StopCause::Signal));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            StopBudgets::for_latch(&latch),
            StopBudgets::from_exit(t0 + STOP_DEADLINE),
            "the latch moment, not the moment of asking"
        );
    }

    /// 倒推到时钟起点之前不 panic：每一段立刻到期。
    #[test]
    fn a_deadline_too_close_to_the_clock_origin_does_not_panic() {
        // `Instant` 的最小值无从构造；用一个肯定倒推不动的极小间隔检查 `checked_sub` 的退路：期限就在现在，各段都不晚于它。
        let now = Instant::now();
        let budgets = StopBudgets::from_exit(now);
        assert!(budgets.engines <= budgets.registration);
        assert!(budgets.registration <= budgets.discovery);
        assert!(budgets.discovery <= budgets.exit);
    }

    /// 拆运行时只等期限里剩下的，封顶 `cap`；期限已过是零；没有期限是 `cap`。
    #[test]
    fn the_runtime_teardown_only_waits_for_what_is_left_of_the_exit_deadline() {
        let cap = Duration::from_secs(5);
        let none = ExitDeadline::default();
        assert_eq!(none.runtime_grace(cap), cap, "no stop flow: the usual cap");
        let far = ExitDeadline::default();
        far.set(Instant::now() + Duration::from_secs(60));
        assert_eq!(far.runtime_grace(cap), cap, "plenty left: capped");
        let near = ExitDeadline::default();
        near.set(Instant::now() + Duration::from_secs(2));
        let grace = near.runtime_grace(cap);
        assert!(
            grace <= Duration::from_secs(2) && grace > Duration::from_millis(1500),
            "{grace:?}"
        );
        let spent = ExitDeadline::default();
        spent.set(Instant::now());
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(spent.runtime_grace(cap), Duration::ZERO);
        // 只记第一次。
        near.set(Instant::now() + Duration::from_secs(60));
        assert!(near.runtime_grace(cap) <= Duration::from_secs(2));
    }

    // -----------------------------------------------------------------------------------------
    // 信号：处理器启动时就装好（L6），同步可读、异步可等（H1）
    // -----------------------------------------------------------------------------------------

    /// 没有信号时既不置位也不返回（不是一个一直就绪的 future）；置位之后已经在等的与之后才来等的都立即返回。
    #[tokio::test]
    async fn a_raised_signal_wakes_every_waiter_and_stays_set() {
        let signal = StopSignal::manual();
        assert!(!signal.is_set());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), signal.wait())
                .await
                .is_err()
        );
        let early = {
            let signal = signal.clone();
            tokio::spawn(async move { signal.wait().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.raise();
        assert!(signal.is_set());
        tokio::time::timeout(WAIT, early).await.unwrap().unwrap();
        tokio::time::timeout(WAIT, signal.wait())
            .await
            .expect("a waiter that comes after the signal returns at once");
        // 克隆共用同一个标志。
        assert!(signal.clone().is_set());
    }

    /// 真并发（多线程运行时加另一个操作系统线程）：`raise` 与一个刚好在登记的等待者同时发生，反复很多轮，等待者每一轮都必须返回。
    /// `wait` 在检查标志**之前**就建好 `Notified` 并登记（置位发生在两者之间也会唤醒它，不会丢）；这条测试只能抽样那个纳秒级的窗口，
    /// 不能逼出它，确定性的两头（先等后置位、先置位后等）由 `a_raised_signal_wakes_every_waiter_and_stays_set` 钉住。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_raise_racing_with_a_waiter_that_is_just_registering_is_never_lost() {
        for round in 0..2000 {
            let signal = StopSignal::manual();
            let waiter = {
                let signal = signal.clone();
                tokio::spawn(async move { signal.wait().await })
            };
            let raiser = {
                let signal = signal.clone();
                std::thread::spawn(move || signal.raise())
            };
            tokio::time::timeout(WAIT, waiter)
                .await
                .unwrap_or_else(|_| {
                    panic!("round {round}: a raise that raced with the waiter was lost")
                })
                .unwrap();
            raiser.join().unwrap();
        }
    }

    /// 信号在**没有任何人读之前**就到了：处理器是 `install` 时注册的，所以信号被后台任务记下，同步读（`is_set`）与异步等（`wait`）都
    /// 读得到。处理器若是等第一次 poll 才注册（旧写法），这两次 `raise` 按默认动作直接杀死整个进程（这个测试进程），根本走不到断言。
    /// SIGINT 与 SIGTERM 各来一次。
    #[cfg(unix)]
    #[tokio::test]
    async fn signals_that_arrive_before_anybody_reads_are_recorded_because_the_handlers_are_installed_up_front()
     {
        use nix::sys::signal::{Signal, raise};

        for which in [Signal::SIGTERM, Signal::SIGINT] {
            let signal = StopSignal::install().unwrap();
            assert!(!signal.is_set(), "{which}: nothing yet");
            raise(which).unwrap();
            let deadline = std::time::Instant::now() + WAIT;
            while !signal.is_set() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{which} raised before anybody read is recorded"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::timeout(WAIT, signal.wait())
                .await
                .expect("wait returns at once once the signal has arrived");
        }
    }
}
