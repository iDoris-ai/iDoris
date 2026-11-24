//! 一个托管引擎实例的任务：启动决策、等就绪、监管、失败的退避与熔断、命令（接口规范 §2.7.6）。
//!
//! 整体是一台「步骤」状态机：[`Step::Attempt`]（走一遍启动决策）、[`Step::Supervise`]（引擎就绪，周期探测）、
//! [`Step::Idle`]（处于终态，等命令或半开重试）、[`Step::Exit`]。每个等待（退避、等就绪、监管周期、空闲）都可以被命令打断，
//! 命令暂存在 [`Actor::pending`]，主循环在下一个安全点处理它。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use idoris_backend::BackendError;
use idoris_backend::load_fence::LoadFence;
use idoris_upstream::engine::{
    DiscoveryError, EngineError, EngineInstance, EngineKey, EngineLaunch, EngineLaunchSpec,
    EngineProbe, EngineRecipe, ExternalListenerGuard, LaunchError, ListenerIdentity,
    ListenerVerdict, ListenerVerifier, MANAGED_DIAL, OwnedEngine, OwnedListenerGuard,
    OwnershipRecord, Recovery, fresh_connection_client, probe_authorized,
};
use idoris_upstream::omlx::key::{EndpointGuard, seal_endpoint_flights_if_idle};
use tokio::sync::{mpsc, oneshot};

use super::backoff::{CrashWindow, restart_delay};
use super::manager::{
    CommandError, EngineFailure, ManagedPlan, Shared, ShutdownOrder, Status, counts_as_failure,
};
use crate::readiness::{UpstreamReason, UpstreamState, unix_millis_now};

/// 对实例的命令。
#[derive(Debug)]
pub(super) enum Command {
    Start,
    Stop,
    Restart,
    /// 门控等的时间到了（启动超时），这个实例还没落定：放弃。停掉半拉起的引擎，报 `failed` + `startup_timeout`，
    /// 不再重试（[`EngineManager::give_up_unsettled`]）。已经落定的实例当场回答，什么也不改。
    GiveUp,
    /// iDoris 停止：收尾并结束任务。`stop_engine` 为假时让自有引擎继续跑。`deadline` 是整个停止必须完成的期限
    /// （信号停止：launcher 在 SIGTERM 之后 17 秒 SIGKILL iDoris，见 `crate::stop::STOP_DEADLINE`）：停引擎的宽限只拿期限里剩下的
    /// （[`super::backoff::stop_windows`]）；`None` 是没有期限，宽限照 [`super::backoff::Timings::stop_grace`]。
    Shutdown {
        stop_engine: bool,
        deadline: Option<tokio::time::Instant>,
    },
}

pub(super) struct Request {
    pub command: Command,
    /// 命令被接受（或被拒绝）的回答；`Shutdown` 在收尾完成之后才回答。
    pub reply: Option<oneshot::Sender<Result<(), CommandError>>>,
    /// 入队时盖的章（[`Shared::next_seq`]，从 1 起；`GiveUp`、`Shutdown` 是 0）：见 [`Actor::restart_cover`]。
    pub seq: u64,
    /// 命令的绝对截止时间（`Timings::command_timeout`，只有 `start`、`stop`、`restart` 有；IDORIS-CORE S6，Codex 终审 M5）：到点还没被
    /// 收下就当它没有发生过（回答 `Closed`、不执行）。调用方在同一个截止时间放弃，所以一个超时的、有副作用的命令不会在它放弃之后才被执行。
    pub deadline: Option<Instant>,
}

impl Request {
    fn expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// 操作员发起、还没做完的一次操作。从受理一直到引擎就绪（或不会再往前走）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `start`（`stopped`、`failed`、`absent`、`unavailable` 之后重新走启动决策）。
    Start,
    /// `restart`（一个自有引擎退役、换一个新的）：期间状态一直是 `restarting`。
    Restart,
}

/// 发布一个状态时怎么记失败次数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    /// 进入 `restarting`、`failed` 才算（[`counts_as_failure`]）。
    Edge,
    /// 不算：操作员要的重启不是失败。
    Skip,
    /// 算：操作员的重启进行期间，一次真的失败（状态本来就是 `restarting`，没有「进入」这回事）。
    Force,
}

/// 空闲时在等什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Idle {
    /// 终态（`absent`、配置性的 `failed`、`unavailable`、被放弃）：只等命令。
    Terminal,
    /// 熔断（`failed` + `crash_loop`）：每隔半开间隔重试一次，也等命令；门控降级放行之后不再重试（冻结）。
    CrashLoop,
    /// 被 `stop` 停下：只等命令。
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Attempt,
    Supervise,
    Idle(Idle),
    Exit,
}

/// 一次启动决策的结果。
enum Attempt {
    /// 引擎就绪（`Actor::engine` 或 `Actor::external` 已设）。
    Ready,
    /// 终态：不会自己好，等 `start`（门控降级放行时没就绪的实例冻结，连 `start` 也不接受，见 `Actor::frozen`）。
    Terminal {
        state: UpstreamState,
        reason: Option<UpstreamReason>,
    },
    /// 一次运行期失败：计入熔断窗口，退避之后重试。
    Failure { reason: Option<UpstreamReason> },
    /// 被命令打断（命令在 `Actor::pending`）；半拉起的引擎已经收拾掉。
    Interrupted,
    /// 启动决策的前提在判断期间变了（刚才有东西在、答不上来，现在端口上什么都没有了；探测与校验之间监听者没了）：回到决策的开头
    /// 重来。**这不是引擎失败**：没有哪个我们拉起的进程退出或超时，所以不记入熔断窗口、不退避、不计 `failure_count`
    /// （IDORIS-CORE S6，Codex 预审 6）。整串重来受一个启动超时约束（[`Actor::redecide`]）。
    Redecide,
}

/// [`Actor::retire_engine`] 的结果。
enum Retired {
    /// 进程确认消失（或本来就没有引擎）。
    Confirmed,
    /// 没能确认进程消失：它可能还活着。调用方不能清围栏、不能当它停了（[`Actor::condemned`]）。
    Unconfirmed,
    /// iDoris 在停（[`Actor::shutdown_order`]）：退役在中途放手，引擎还握在 `Actor::engine`，由收尾按指令处置。
    Shutdown,
}

/// 等就绪失败的原因。
enum WaitFailure {
    Exited,
    Timeout,
    AuthRejected,
    Interrupted,
}

/// 我们拥有的引擎（自己拉起的，或接管了上一个 iDoris 留下的）。
struct Owned {
    engine: OwnedEngine,
    key: Option<EngineKey>,
    /// 登记私有 key 时一起交给登记表的校验守卫：发往这个端点的每个请求，发出之前都让它重新确认监听这个端口的还是这个引擎的进程
    /// （Codex 终审 H1）。**整个引擎的一生用同一个对象**：重新登记同一个口径时守卫是同一个，登记表的代际不动。
    guard: Arc<dyn EndpointGuard>,
}

impl Owned {
    fn new(
        engine: OwnedEngine,
        key: Option<EngineKey>,
        inspector: Arc<dyn idoris_upstream::engine::ProcessInspector>,
    ) -> Self {
        // 托管的引擎只监听 `127.0.0.1`，也只被拨 `127.0.0.1`：守卫验证的、授权结果带给发送端的，是这个地址（Codex 专项评审 H1）。
        let guard: Arc<dyn EndpointGuard> = Arc::new(OwnedListenerGuard::new(
            inspector,
            engine.pid(),
            std::net::SocketAddr::new(MANAGED_DIAL, engine.port()),
        ));
        Self { engine, key, guard }
    }
}

/// 接管的外部引擎：监听者通过了属主校验。
struct External {
    verifier: ListenerVerifier,
    /// 据校验通过的监听者（pid 加启动时间）建的守卫：发往这个端点的每个请求，发出之前都让它重新确认监听者还是它们。
    guard: Arc<dyn EndpointGuard>,
}

impl External {
    fn new(verifier: ListenerVerifier, port: u16, listeners: &[ListenerIdentity]) -> Self {
        // `listeners` 是校验时**能接 `127.0.0.1:<port>` 这个连接的**监听者（绑在别的地址上的不在里面）；守卫验证、授权带出去的拨号
        // 地址是 `127.0.0.1:<port>`。
        let guard: Arc<dyn EndpointGuard> = Arc::new(ExternalListenerGuard::new(
            verifier.clone(),
            std::net::SocketAddr::new(MANAGED_DIAL, port),
            listeners.to_vec(),
        ));
        Self { verifier, guard }
    }
}

/// 一个引擎**进程**的身份：Supervisor 的账本是对着它建的（放行时采样一次；之后每次换代复位时再采样）。身份变了就是换了一个
/// 进程，账本过期，要复位；没变（`start` 重新认回了同一个引擎）就什么都不用做（IDORIS-CORE S6）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum EngineIdentity {
    /// 自己拉起（或认回）的引擎：pid 加 `ps` 的启动时间（pid 被复用也分得开）。
    Owned { pid: u32, started_at: String },
    /// 外部引擎：端口上通过了属主校验的监听者，每个是 pid 加启动时间（升序）：只比 pid 分不出「还是它」与「pid 被复用、换了
    /// 一个进程」。
    External { listeners: Vec<ListenerIdentity> },
}

/// [`Actor::sync_supervisor`] 的结果。
enum Synced {
    /// 账本跟上了，或者不需要跟（还没放行、没有 Supervisor、同一个进程）。
    Done,
    /// 等待期间失败了，一会儿再试（引擎已经停掉）。
    Retry,
    /// Supervisor 没有了（actor 已退出，或者检测到自己的账本损坏而中毒）：不会自己好，要重启 iDoris（引擎已经停掉）。
    Permanent,
    /// iDoris 在停：等待放手了，引擎还握在手里，由主循环按停止指令收尾（Codex 预审 H3）。
    Shutdown,
}

/// 实例任务 panic（一个 bug）时，不能让它无声地冻结在最后一个状态上（门控永远完不成，或者一个 `ready` 的谎话一直挂着）：
/// 守卫在展开中被丢弃时把它当成引擎失效报出去，`bin` 以退出码 5 停止，由启动器重启。
struct PanicReport {
    shared: Arc<Shared>,
    id: String,
}

impl Drop for PanicReport {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        self.shared.log(&format!(
            "[idoris] 引擎 {}：实例任务 panic，按引擎失效处理（iDoris 将以退出码 5 停止）",
            self.id
        ));
        self.shared.publish_panicked(&self.id);
        self.shared.signal_failure(EngineFailure {
            upstream_id: self.id.clone(),
        });
    }
}

pub(super) struct Actor {
    plan: ManagedPlan,
    shared: Arc<Shared>,
    commands: mpsc::Receiver<Request>,
    /// 不保留空闲连接的客户端：带 key 的请求只走新开的连接（#619）。
    client: Option<reqwest::Client>,
    window: CrashWindow,
    engine: Option<Owned>,
    external: Option<External>,
    /// 当前发布的状态。
    state: UpstreamState,
    reason: Option<UpstreamReason>,
    /// 当前发布的状态是不是终局（见 [`Status::terminal`]）。
    terminal: bool,
    /// 状态进入 `restarting` 或 `failed` 的次数与最近一次的墙钟时间（Admin 的只读细节，见 [`Status::failures`]）。
    failures: u32,
    last_failure_at_ms: Option<u64>,
    /// 最近一次发布 `ready` 时引擎进程的身份：Supervisor 的账本最后一次是对着它建（或复位）的。`None` = 还没有，或者引擎已经
    /// 停了（下一个就绪的引擎一定要复位）。
    identity: Option<EngineIdentity>,
    /// 这一轮启动决策接管的外部引擎的监听者（校验通过时记下，就绪时变成 [`EngineIdentity::External`]）。
    adopted_listeners: Option<Vec<ListenerIdentity>>,
    /// 操作员发起的、还没做完的 `start`、`restart`。
    op: Option<Op>,
    /// 序号不大于它的 `restart` 命令，都是在上一次 `start`、`restart` 操作进行期间（或之前）入队的：它们并进那次操作的结果，
    /// 不再换第二次引擎。命令在**入队时**盖章（[`Shared::next_seq`]），操作完成时记下此刻盖出的最大序号；这样即使命令
    /// 在 actor 忙着排空、停进程、复位、不去收命令的时候排在队里，它也被认出是「同一次操作」的并发（IDORIS-CORE S6，
    /// Codex 预审 5）。
    restart_cover: u64,
    /// 启动决策「前提变了、重来」（[`Attempt::Redecide`]）的这一串从什么时候开始。
    redecide_since: Option<Instant>,
    /// 外部引擎的监听者没了（或不合格）：我们封了端点、关了 Supervisor 的门。同一个进程回来时要开回去。
    external_gap: bool,
    /// 自有引擎活着，但此刻监听这个端口的不是它（放掉了端口，或别的进程占了）：我们收回了它的私有 key（端点封上）。它又拥有
    /// 端口时要把私有 key 重新登记回去（IDORIS-CORE S6，Codex 审计 H3）。
    owned_key_revoked: bool,
    /// 等待期间收到、还没有处理的命令。
    pending: Option<Request>,
    /// 一个自有的子进程正在启动（拉起了、或认回了，等它就绪，还没封印）：它此刻在 [`Actor::wait_owned_ready`] 的局部变量里，不在
    /// `engine`。`stop`、`restart` 要看见它（Codex 预审 M3）：命令打断等待之后，半拉起的引擎交给命令处置（[`Actor::abandon`]）。
    starting_owned: bool,
    /// 半拉起的引擎被命令打断后放回了 `engine`（[`Actor::abandon`]），命令还没被收下。命令在收下之前过期了的，继续等**同一个**引擎就绪，不退役它
    /// （Codex 复审 M7）。
    handed_off: bool,
    /// 这一轮启动决策走到了外部监听者那条路（端口上有东西答话或在听）：不是在拉起自有引擎。每轮 [`Actor::attempt`] 开头清零。
    external_path: bool,
    /// 退役中的引擎进程的**完整所有权记录**（pid、启动时间、「代」标识、进程组、命令；Codex 复审四 H）：停它之前记下，**只有确认它消失才清掉**（Codex 预审 M4）。没能确认时它可能还活着、
    /// 还握着端口；之后 `start` 或重启的启动决策又认回同一个进程时，不能把它当作新引擎放行，要先把它停掉。
    condemned: Option<OwnershipRecord>,
    /// 操作员的 `stop`、`restart` 已经被受理、退役还没确认的那个引擎（pid）：之后 iDoris 停止时（哪怕 `stop_on_exit:"never"` 的正常退出）这个引擎也要停掉，
    /// 不能把一个已经受理了的停止放手成一个在跑的引擎（Codex 复审三 M）。绑在引擎的身份上：那个引擎的退役确认了就清掉，之后的替身不受它影响（复审四 M）。
    retire_accepted: Option<u32>,
}

impl Actor {
    pub(super) fn new(
        plan: ManagedPlan,
        shared: Arc<Shared>,
        commands: mpsc::Receiver<Request>,
    ) -> Self {
        let timings = &shared.config.timings;
        let window = CrashWindow::new(timings.crash_window, timings.crash_limit);
        Self {
            plan,
            shared,
            commands,
            client: fresh_connection_client().ok(),
            window,
            engine: None,
            external: None,
            state: UpstreamState::Starting,
            reason: None,
            terminal: false,
            failures: 0,
            last_failure_at_ms: None,
            identity: None,
            adopted_listeners: None,
            op: None,
            restart_cover: 0,
            redecide_since: None,
            external_gap: false,
            owned_key_revoked: false,
            pending: None,
            starting_owned: false,
            handed_off: false,
            external_path: false,
            condemned: None,
            retire_accepted: None,
        }
    }

    pub(super) async fn run(mut self) {
        let _panic_report = PanicReport {
            shared: self.shared.clone(),
            id: self.plan.id.clone(),
        };
        let mut next = Step::Attempt;
        loop {
            // iDoris 在停：任何步骤之间都先看一眼（长的等待自己也看，见 [`Self::unless_shutdown`]）。命令邮箱里排在后面的 `Shutdown` 不必再等。
            if let Some(order) = self.shutdown_order() {
                self.shutdown(order.stop_engine, order.deadline).await;
                return;
            }
            // 等待期间收到的命令先处理：它可能改变下一步。
            if let Some(request) = self.pending.take() {
                next = self.on_command(request, next).await;
                continue;
            }
            next = match next {
                Step::Attempt => self.attempt_cycle().await,
                Step::Supervise => self.supervise().await,
                Step::Idle(idle) => self.idle(idle).await,
                Step::Exit => return,
            };
        }
    }

    // -----------------------------------------------------------------------------------------
    // 小工具
    // -----------------------------------------------------------------------------------------

    /// iDoris 在停的指令（[`EngineManager::shutdown_by`] 先公告、后送命令）。
    fn shutdown_order(&self) -> Option<ShutdownOrder> {
        *self.shared.shutdown.borrow()
    }

    /// 跑 `work`，iDoris 一旦在停就放手（`None`）：退役里的长等待（关门、排空、复位）不能挡住收尾。放手的 future 被丢弃，所以只能
    /// 包不持有「丢了就泄漏」的东西的等待；握着引擎的停止走 [`Self::stop_retiring`]。
    async fn unless_shutdown<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        let mut order = self.shared.shutdown.subscribe();
        tokio::select! {
            biased;
            () = async { let _ = order.wait_for(Option::is_some).await; } => None,
            output = work => Some(output),
        }
    }

    fn timings(&self) -> &super::backoff::Timings {
        &self.shared.config.timings
    }

    fn log(&self, text: &str) {
        self.shared
            .log(&format!("[idoris] 引擎 {}：{text}", self.plan.id));
    }

    /// 发布状态（还在往前走，不是终局）。只在状态或原因变化时写一行日志（不刷屏）。
    fn publish(&mut self, state: UpstreamState, reason: Option<UpstreamReason>) {
        self.emit(state, reason, false, Count::Edge);
    }

    /// 发布一个状态，不记失败：操作员要的 `restart` 不是失败（[`Count::Skip`]）。
    fn publish_uncounted(&mut self, state: UpstreamState, reason: Option<UpstreamReason>) {
        self.emit(state, reason, false, Count::Skip);
    }

    /// 发布一个终局的状态：这个实例不会自己再往就绪走了，门控把它算作「落定」。
    fn publish_terminal(&mut self, state: UpstreamState, reason: Option<UpstreamReason>) {
        self.emit(state, reason, true, Count::Edge);
    }

    /// 门控是在这个实例没有就绪的时候降级放行的：它冻结，不再半开重试，不再接受 `start`（要重启 iDoris）。
    fn frozen(&self) -> bool {
        self.shared.is_frozen(&self.plan.id)
    }

    fn emit(
        &mut self,
        state: UpstreamState,
        reason: Option<UpstreamReason>,
        terminal: bool,
        count: Count,
    ) {
        self.emit_logged(state, reason, terminal, count, true);
    }

    /// [`Self::emit`]，日志可以先不写（`log` 假）：引擎失效的发布路径要在写任何日志之前先把失效锁存（日志的出口可能阻塞，Codex 复审 M7）。
    fn emit_logged(
        &mut self,
        state: UpstreamState,
        reason: Option<UpstreamReason>,
        terminal: bool,
        count: Count,
        log: bool,
    ) {
        let changed = self.state != state || self.reason != reason;
        let counted = match count {
            Count::Edge => counts_as_failure(self.state, state),
            Count::Skip => false,
            Count::Force => true,
        };
        if counted {
            self.failures = self.failures.saturating_add(1);
            self.last_failure_at_ms = Some(unix_millis_now());
        }
        self.state = state;
        self.reason = reason;
        self.terminal = terminal;
        self.shared.publish(
            &self.plan.id,
            Status {
                state,
                reason,
                owned: self.engine.is_some(),
                terminal,
                pid: self.engine.as_ref().map(|owned| owned.engine.pid()),
                failures: self.failures,
                last_failure_at_ms: self.last_failure_at_ms,
            },
        );
        if changed && log {
            match reason {
                Some(reason) => self.log(&format!("{}（{}）", state.as_str(), reason.as_str())),
                None => self.log(state.as_str()),
            }
        }
    }

    fn state_line(state: UpstreamState, reason: Option<UpstreamReason>) -> String {
        match reason {
            Some(reason) => format!("{}（{}）", state.as_str(), reason.as_str()),
            None => state.as_str().to_owned(),
        }
    }

    /// 可被命令打断的等待，返回是否被打断。已经有暂存的命令时立刻返回打断。
    ///
    /// 在当前状态下什么也改变不了的命令（正在启动时又 `start`、没有自有引擎时 `stop`/`restart`）当场回答，不打断等待：
    /// 否则一个多余的 `start` 会把进行到一半的启动白白废掉。
    async fn wait(&mut self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.pending.is_some() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            tokio::select! {
                () = tokio::time::sleep(remaining) => return false,
                request = self.commands.recv() => {
                    let Some(request) = request else {
                        // 管理器没有走 `shutdown` 就丢了：当作不停引擎的收尾，让 `OwnedEngine` 的 `Drop` 按自己的规则处置。
                        self.pending = Some(Request {
                            command: Command::Shutdown {
                                stop_engine: false,
                                deadline: None,
                            },
                            reply: None,
                            seq: 0,
                            deadline: None,
                        });
                        return true;
                    };
                    // 调用方已经在这个截止时间放弃了：当作没有发生过（Codex 终审 M5）。
                    if request.expired() {
                        if let Some(reply) = request.reply {
                            let _ = reply.send(Err(CommandError::Closed));
                        }
                        continue;
                    }
                    match self.immediate_answer(&request) {
                        Some(result) => {
                            if let Some(reply) = request.reply {
                                let _ = reply.send(result);
                            }
                        }
                        None => {
                            self.pending = Some(request);
                            return true;
                        }
                    }
                }
            }
        }
    }

    /// 在当前状态下什么也改变不了的命令的回答（`None` = 这个命令要真的处理）。
    fn immediate_answer(&self, request: &Request) -> Option<Result<(), CommandError>> {
        let covered = request.seq != 0 && request.seq <= self.restart_cover;
        match &request.command {
            // 已经好了、或正在启动：幂等。
            Command::Start
                if matches!(
                    self.state,
                    UpstreamState::Ready | UpstreamState::Starting | UpstreamState::Restarting
                ) =>
            {
                Some(Ok(()))
            }
            // 已经落定（就绪，或终局）：门控等的时间到了也不用再放弃它。
            Command::GiveUp if self.state == UpstreamState::Ready || self.terminal => Some(Ok(())),
            // 降级放行之后，这个上游不再启动：放行时它没就绪，完整的 Router 里根本没有它的 Supervisor（要重启 iDoris）。放行时
            // 就绪的上游不冻结：它们的 Supervisor 在引擎换代之后复位（[`Self::sync_supervisor`]）。
            Command::Start | Command::Restart if self.frozen() => {
                Some(Err(CommandError::RestartRequired))
            }
            // 入队时落在一次 `start`、`restart` 操作之内（或之前）的 `restart`：并进那次操作的结果，不换第二次引擎（Admin 也回 202）。
            Command::Restart if covered => Some(Ok(())),
            // 已经停下了：幂等。
            Command::Stop if self.engine.is_none() && self.state == UpstreamState::Stopped => {
                Some(Ok(()))
            }
            // 操作员的 `restart` 正在进行、此刻没有握着的自有引擎（旧的已退役，新的在启动或在退避）：并发的第二个并进它，不换第二次引擎，也不去
            // 杀刚拉起的替身。但启动决策已经走到了外部监听者那条路（等外部引擎答话、接管它）就不是了：那里没有自有引擎可重启
            // （Codex 复审 M5）。
            Command::Restart
                if self.engine.is_none() && self.op == Some(Op::Restart) && !self.external_path =>
            {
                Some(Ok(()))
            }
            // 正在启动一个自有的子进程（拉起了、等它就绪）：`stop`、`restart` 要处理它，不是「没有自有引擎」——打断等待，
            // 半拉起的引擎交给命令（[`Self::abandon`]；Codex 预审 M3）。
            Command::Stop | Command::Restart if self.starting_owned => None,
            // 不是自己拥有的引擎（外部引擎、没有引擎）：iDoris 不给别人的进程发信号。
            Command::Stop | Command::Restart if self.engine.is_none() => {
                Some(Err(CommandError::NotOwned))
            }
            _ => None,
        }
    }

    /// 一次探测：`key` 有值才带 `Authorization`。
    async fn probe(&self, recipe: &Arc<dyn EngineRecipe>, key: Option<&str>) -> EngineProbe {
        let Some(client) = &self.client else {
            return EngineProbe::Unavailable;
        };
        recipe
            .probe_ready(
                client,
                &self.plan.endpoint,
                key,
                self.timings().probe_timeout,
            )
            .await
    }

    /// 带 key 的探测，走登记表的授权路径（[`idoris_upstream::engine::probe_authorized`]；Codex 复审 L2 的 M2'）：key 由调用方**先**取好，授权负责监听者的重新确认、
    /// 代际与封上、核验过的拨号地址、不保留空闲连接的客户端、发出前的最后一眼与端点上的在途登记。外部引擎的接管与监督用它，引擎换代之后的探测也该用它。
    /// 授权被拒绝（监听者变了、读不出来、端点封上、代际变了、托管客户端建不出来）：这一轮没有结论（`Unavailable`），key 一个字节也没发出去，记一行日志。
    async fn probe_authorized(&self, recipe: &Arc<dyn EngineRecipe>, key: &str) -> EngineProbe {
        let Some(client) = &self.client else {
            return EngineProbe::Unavailable;
        };
        match probe_authorized(
            recipe.as_ref(),
            &self.plan.endpoint,
            client,
            Some(key),
            self.timings().probe_timeout,
        )
        .await
        {
            Ok(probe) => probe,
            Err(refusal) => {
                self.log(&format!("外部引擎的带凭证探测没有发出（{refusal}）"));
                EngineProbe::Unavailable
            }
        }
    }

    fn probe_instance(&self) -> Option<EngineInstance> {
        EngineInstance::new(
            self.plan.backend.probe_recipe(),
            &self.shared.config.state_dir,
            self.plan.port,
        )
        .ok()
    }

    fn fence_path(&self) -> Option<PathBuf> {
        self.plan
            .backend
            .fence_path(&self.shared.config.state_dir, &self.plan.endpoint)
    }

    /// 围栏还在，或者读不出它在不在（保守：当作在）。
    fn fence_is_present(&self) -> bool {
        self.fence_path()
            .is_some_and(|path| LoadFence::new(path).check_clear().is_err())
    }

    /// 清围栏：一个刚拉起（或杀掉重拉）的引擎什么都没加载，之前的围栏说的是已经不存在的上一代。
    fn clear_fence(&self) -> Result<(), ()> {
        match self.fence_path() {
            Some(path) => LoadFence::new(path).clear().map_err(|_| ()),
            None => Ok(()),
        }
    }

    /// 停掉并收尾一个引擎（SIGTERM 整组，宽限之后 SIGKILL，删记录），没有期限（照 `stop_grace`），返回是不是**确认**它已经消失
    /// （认不出的进程不发信号，那时没有确认，只记日志）。停止、重启、对一个还活着却不答的引擎 `start`（[`Self::retire_engine`]）走这里；
    /// iDoris 在停时的停止不走这里：那是 `Command::Shutdown` 带着期限交给 [`Self::stop_engine_by`]（等就绪被它打断时，引擎也是放回
    /// `self.engine` 交给那条命令，不在 [`Self::abandon`] 里先停，所以这里不会再遇到「已经有带期限的 `Shutdown` 在等」）。
    async fn stop_engine(&self, owned: Owned) -> bool {
        // 走 [`Self::drive_stop`]：iDoris 在停时它的 SIGTERM 等待可被剪短，不会在升级到 SIGKILL 之前被管理器中止（Codex 复审三 H1）。
        let confirmed = self.drive_stop(owned.engine).await;
        self.plan.backend.block(&self.plan.endpoint);
        confirmed
    }

    /// 在 `deadline` 之前停掉并收尾一个引擎，返回是不是**确认**它已经消失：把**绝对期限**交给引擎（`OwnedEngine::stop_until`），宽限与确认窗口
    /// 由它在真正发 SIGTERM 的那一刻按期限里剩下的时间算（[`super::backoff::stop_windows`]：不到 2 秒就 SIGTERM 之后立刻 SIGKILL）；没有期限就照
    /// `stop_grace`。认回的孤儿要先过身份闸（读启动时间、进程组，可能很慢）：闸花掉的时间从同一个期限里扣，不是在闸之前就换算成固定时长
    /// （H3：那样闸慢的时候宽限还是原来那么长，总时间超过期限）。确认不了进程已经消失时所有权记录保留，下一个 iDoris 认回它再收尾
    /// （`OwnedEngine` 的停止只在确认消失之后才删记录）。
    ///
    /// 私有 key 的登记要等进程确认消失**之后**才换掉（G2）：停止的这几秒里引擎还活着、还在监听，发往它的请求仍然只用它的私有
    /// key。之后——确认消失也好，没能确认也好——端点**封上**（[`EngineBackend::block`]：什么凭证都不发），**不是撤销登记**：
    /// 撤销会让回退链（环境变量、`settings.json`）重新适用于这个端点，把用户的 key 发给之后占着这个端口、还没通过校验的别的东西
    /// （Codex 预审 3）。封上一直保持到下一个监听者通过校验、被登记成私有或外部的口径。
    async fn stop_engine_by(&self, owned: Owned, deadline: Option<tokio::time::Instant>) -> bool {
        let grace = self.timings().stop_grace;
        let confirmed = match owned.engine.stop_until(grace, deadline).await {
            Ok(()) => true,
            Err(error) => {
                self.log(&format!("停止引擎失败（{}）", error.reason_code()));
                false
            }
        };
        self.plan.backend.block(&self.plan.endpoint);
        confirmed
    }

    /// 关 Supervisor 的门：新的加载、卸载、聊天、列表不再准入，已经准入的做到完。引擎被停止、替换之前的第一步（次序：关门、
    /// 排空、停进程、封上、复位、开门；IDORIS-CORE S6，Codex 预审 1、2）。没有 Supervisor（门控还没放行、resident 的托管卡）什么都
    /// 不用做；关门失败（actor 没了、中毒）只记日志：引擎照样要停。幂等。
    async fn close_admission(&self) {
        let Some(bound) = self.supervisor() else {
            return;
        };
        // iDoris 在停时不再等：Supervisor 的邮箱可能被一次长的复位占着（Codex 预审 H1、H3）。
        if let Some(Err(error)) = self.unless_shutdown(bound.close_admission()).await {
            self.log(&format!(
                "关 Supervisor 的门失败（{}）",
                error.reason_code()
            ));
        }
    }

    /// 门已经关了：等已经准入的操作做完，至多 [`super::backoff::Timings::drain_timeout`]。引擎此刻还活着，它们拿着它们准入时的凭证照常
    /// 跑完。等两样（Codex 终审 M1）：Supervisor 的在途（加载、卸载、聊天、**列表**），和这个端点上登记的在途操作（`endpoint_flights`：
    /// resident 的转发、后台预热、模型列表的读取、仍在输出的流式响应体……凡是经 `authorize_omlx_request` 发出去的请求，没有
    /// Supervisor 的托管卡也一样）。`true` = 排空了（Supervisor 读不出来也算：没有什么可等的）；`false` = 到点还有在途的。
    ///
    /// **排空成功的那一刻，端点的在途登记同时封上**（[`seal_endpoint_flights_if_idle`]：「在途数为零」与「封上」是同一个原子操作；
    /// IDORIS-CORE S6，Codex 预审 L3 的 M5）。分开读与封的话，一个早先抓到这张托管卡的请求（模型列表逐卡走，前面的慢卡让它停在门外）
    /// 会在零与停进程之间挤进来，在进程的宽限期里登记新的在途、发往还在监听的引擎，随后被 SIGTERM 打断。封上之后到达的请求在授权处被
    /// 拒绝（`KeyRefusal::Draining`），一个字节也不发；封上到进程确认消失、端点封上（代际前进，同时解开这一代的封）之间，已经登记的
    /// 操作为零，所以没有谁需要「凭证继续有效」。到点还有在途的（`false`）走的是另一条路：调用方封上端点、切断旧一代的凭证。
    ///
    /// **一个绝对的截止时间管每一次读**（Codex 终审 M5）：Supervisor 忙着的时候一次 `is_quiescent` 的 RPC 本身就可能等，不能到它返回之后才
    /// 发现已经过了截止时间。
    async fn drain(&self) -> bool {
        // iDoris 在停：不再等在途的操作（它们随引擎一起被收尾），按「没排空」返回，调用方先看 `shutdown_order` 再决定（Codex 预审 H3）。
        self.unless_shutdown(self.drain_inner())
            .await
            .unwrap_or(false)
    }

    async fn drain_inner(&self) -> bool {
        let deadline = Instant::now() + self.timings().drain_timeout;
        let pause = self.timings().ready_initial_interval;
        loop {
            let supervisor_quiet = match self.supervisor() {
                None => true,
                Some(bound) => {
                    match tokio::time::timeout_at(
                        tokio::time::Instant::from_std(deadline),
                        bound.is_quiescent(),
                    )
                    .await
                    {
                        Ok(Ok(quiet)) => quiet,
                        // Supervisor 没了（读不出来）：没有什么可等的。
                        Ok(Err(_)) => true,
                        // 读它的这一次 RPC 本身就超过了截止时间。
                        Err(_elapsed) => return false,
                    }
                }
            };
            if supervisor_quiet && seal_endpoint_flights_if_idle(&self.plan.endpoint) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep(pause.min(deadline - now)).await;
        }
    }

    /// 把还握在手里的自有引擎退役：关门、排空（有界）、停进程（确认它消失）、封上端点。没有引擎握在手里时什么都不做（`Confirmed`）。
    ///
    /// 排空到点还有在途的操作时：先切断旧一代的凭证（端点封上，代际前进），旧的操作之后发的每个请求都被适配器拒绝，再停进程。
    /// 没能确认进程消失时返回 [`Retired::Unconfirmed`]：调用方不能清围栏、不能拉起新的，进程记在 [`Actor::condemned`]，之后的启动决策认回
    /// 它时先把它停掉，不放行（Codex 预审 M4）。
    ///
    /// **iDoris 在停时放手**（Codex 预审 H3）：关门与排空的等待立刻结束，引擎仍握在 `self.engine`，返回 [`Retired::Shutdown`]，由主循环按
    /// 停止指令收尾（期限、`stop_on_exit`）；已经在停进程的，见 [`Self::stop_retiring`]。
    async fn retire_engine(&mut self) -> Retired {
        if self.engine.is_none() {
            return Retired::Confirmed;
        }
        self.close_admission().await;
        if self.shutdown_order().is_some() {
            return Retired::Shutdown;
        }
        let drained = self.drain().await;
        if self.shutdown_order().is_some() {
            return Retired::Shutdown;
        }
        if !drained {
            self.log("排空的上限到了、还有在途的操作：切断旧一代的凭证，不再等它们");
            self.plan.backend.block(&self.plan.endpoint);
        }
        match self.engine.take() {
            Some(owned) => self.stop_retiring(owned).await,
            None => Retired::Confirmed,
        }
    }

    /// 退役里的停进程：把引擎交给 [`Self::drive_stop`]；确认消失才清 [`Actor::condemned`]，端点封上在进程确认消失**之后**（G2，同
    /// [`Self::stop_engine_by`]）。
    async fn stop_retiring(&mut self, owned: Owned) -> Retired {
        self.condemned = Some(owned.engine.record().clone());
        let confirmed = self.drive_stop(owned.engine).await;
        self.plan.backend.block(&self.plan.endpoint);
        if confirmed {
            self.clear_retirement();
            Retired::Confirmed
        } else {
            Retired::Unconfirmed
        }
    }

    /// 停一个引擎，返回是不是**确认**它消失。停止在一个独立的任务里做（任务拿着引擎句柄，直到确认进程消失或任务被中止），所以这里等的时候
    /// iDoris 来了停止指令，放手的只是「等」，不是正在进行的 SIGTERM、宽限、SIGKILL（Codex 预审 H3）：
    ///
    /// - 指令在开始停之前就到了：停止直接带着它的期限（宽限只拿剩下的），等到期限加一个确认窗口，到点中止任务。
    /// - 指令在停止进行中到达（任务的宽限是按**没有期限**算的，可能比剩下的时间长）：到期限之前留出升级的时间，那之后还没停完就**剪短** SIGTERM
    ///   的等待（`stop_until_cut`：同一个停止直接进入 SIGKILL，句柄与身份闸不变）。不取消：`Owned` 的句柄丢弃时杀整组，`stop_on_exit:"never"`
    ///   拉起的、认回的不杀（Codex 复审 H1、H2）。
    ///
    /// 到点还没确认：所有权记录保留，下一个 iDoris 认回并收尾。
    async fn drive_stop(&self, engine: OwnedEngine) -> bool {
        let grace = self.timings().stop_grace;
        let bounded_from_start = self.shutdown_order().and_then(|order| order.deadline);
        let (cut, cut_rx) = tokio::sync::watch::channel(false);
        let mut stopping = tokio::spawn(async move {
            engine
                .stop_until_cut(grace, bounded_from_start, cut_rx)
                .await
        });
        let mut order = self.shared.shutdown.subscribe();
        let joined = tokio::select! {
            biased;
            joined = &mut stopping => joined,
            () = async { let _ = order.wait_for(Option::is_some).await; } => {
                self.log("退役中的引擎还在停：iDoris 在停，等它做完，不超过停止期限");
                let deadline = self.shutdown_order().and_then(|order| order.deadline);
                // 停止一开始就带着期限（`bounded_from_start`）的，宽限已经按它算；进行中到达的期限，宽限是按没有期限算的，可能比剩下的时间长：
                // 在期限之前留出升级的时间，那之后还没停完就**剪短** SIGTERM 的等待（同一个停止直接进入 SIGKILL，句柄、身份闸都不变；不取消它：
                // 丢弃 `never`、认回的句柄不杀进程），再多等一个确认窗口。
                let escalate_at = match (deadline, bounded_from_start) {
                    (Some(deadline), None) => Some(
                        deadline
                            .checked_sub(super::backoff::MIN_TERM_GRACE)
                            .unwrap_or(deadline),
                    ),
                    _ => None,
                };
                let mut waited = None;
                if let Some(at) = escalate_at {
                    waited = tokio::time::timeout_at(at, &mut stopping).await.ok();
                    if waited.is_none() {
                        self.log("停止引擎来不及在停止期限里自己做完：剪短 SIGTERM 的等待，直接 SIGKILL");
                        let _ = cut.send(true);
                    }
                }
                if waited.is_none() {
                    waited = match deadline {
                        Some(deadline) => tokio::time::timeout_at(
                            deadline + super::backoff::MIN_CONFIRM,
                            &mut stopping,
                        )
                        .await
                        .ok(),
                        None => Some((&mut stopping).await),
                    };
                }
                match waited {
                    Some(joined) => joined,
                    None => {
                        stopping.abort();
                        self.log("停止引擎超过停止期限：不再等待（所有权记录保留，下一个 iDoris 会认回并收尾）");
                        return false;
                    }
                }
            }
        };
        match joined {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                self.log(&format!("停止引擎失败（{}）", error.reason_code()));
                false
            }
            Err(_) => {
                self.log("停止引擎的任务异常结束");
                false
            }
        }
    }

    /// 退役没能确认的进程，在期限里再停一次：用**留着的完整记录**重建句柄（不读记录文件：文件没了、作废了，进程也可能还在），过身份闸
    /// （进程没了、pid 给了别人就什么信号也不发，算已经消失；读不出来算没能确认），带 `deadline` 停。没有 condemned 就是没有要停的（`true`）。
    async fn finish_stop_by_recovery(&self, deadline: Option<tokio::time::Instant>) -> bool {
        let Some(record) = self.condemned.clone() else {
            return true;
        };
        let Some(instance) = self.probe_instance() else {
            return false;
        };
        let inspector = self.shared.config.inspector.clone();
        let grace = self.timings().stop_grace;
        match OwnedEngine::from_record(&instance, record, &inspector)
            .stop_until(grace, deadline)
            .await
        {
            Ok(()) => true,
            Err(error) => {
                self.log(&format!("停止引擎失败（{}）", error.reason_code()));
                false
            }
        }
    }

    /// 退役确认了（进程确认消失）：退役中的记录与受理的停止意图一起放下。
    fn clear_retirement(&mut self) {
        self.condemned = None;
        self.retire_accepted = None;
    }

    /// 启动决策里停遗留引擎成了：它要是就是退役没能确认的那个，现在确认了，放下并放掉它占着的共享容量（Codex 复审三 M）。
    async fn recovery_stopped(&mut self, pid: u32) {
        if self
            .condemned
            .as_ref()
            .is_some_and(|condemned| condemned.pid == pid)
        {
            self.clear_retirement();
            self.forget_engine().await;
        }
    }

    /// 启动决策里停遗留引擎没成：iDoris 在停就放手（主循环收尾），否则按一次失败退避重试。drive_stop 已经写过原因日志。
    fn recovery_stop_failed(&self) -> Attempt {
        if self.shutdown_order().is_some() {
            Attempt::Interrupted
        } else {
            Attempt::Failure { reason: None }
        }
    }

    /// 封印该被放弃的那一刻：停止指令到了而且要停引擎（非零退出、被要求停止）就是现在；要留引擎（`stop_on_exit:"never"` 的正常退出）就等到停止期限
    /// （没有期限就等一个停止宽限）。没有停止指令就永远不到。
    async fn seal_cancel_point(&self) {
        let mut order = self.shared.shutdown.subscribe();
        let ordered = loop {
            if let Some(ordered) = *order.borrow_and_update() {
                break ordered;
            }
            if order.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        if ordered.stop_engine {
            return;
        }
        match ordered.deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => tokio::time::sleep(self.timings().stop_grace).await,
        }
    }

    /// 暂存的命令是 `stop` 或 `restart` 吗。
    fn pending_is_stop_or_restart(&self) -> bool {
        matches!(
            &self.pending,
            Some(Request {
                command: Command::Stop | Command::Restart,
                ..
            })
        )
    }

    /// 暂存的命令是 `Shutdown`（iDoris 在停）吗。
    fn pending_is_shutdown(&self) -> bool {
        matches!(
            &self.pending,
            Some(Request {
                command: Command::Shutdown { .. },
                ..
            })
        )
    }

    // -----------------------------------------------------------------------------------------
    // 命令
    // -----------------------------------------------------------------------------------------

    /// 在 `current`（被打断的步骤）这个上下文里处理一个命令，返回下一步。命令不改变任何东西时返回 `current`。
    async fn on_command(&mut self, request: Request, current: Step) -> Step {
        // 在邮箱里排队排到过期的命令不执行（它的调用方已经放弃了）：回答 `Closed`，什么也不改（Codex 终审 M5）。
        if request.expired() {
            if let Some(reply) = request.reply {
                let _ = reply.send(Err(CommandError::Closed));
            }
            return current;
        }
        // 命令被收下了：交出来的半拉起的引擎归它处置。
        self.handed_off = false;
        let Request {
            command,
            reply,
            seq,
            deadline: _,
        } = request;
        // 入队时落在一次 `start`、`restart` 操作之内（或之前）：见 [`Actor::restart_cover`]。
        let covered = seq != 0 && seq <= self.restart_cover;
        let answer = move |result: Result<(), CommandError>| {
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        };
        match command {
            Command::Shutdown {
                stop_engine,
                deadline,
            } => {
                self.shutdown(stop_engine, deadline).await;
                answer(Ok(()));
                Step::Exit
            }
            Command::GiveUp => {
                // 在启动的等待里被打断：半拉起的引擎已经被 `abandon` 收拾掉了。在监管里被打断（就绪过、之后活着却不答的
                // 自有引擎）：它还在跑，放弃就是停掉它，不留一个没人管的引擎进程、状态却说 `failed`。
                if let Some(owned) = self.engine.take() {
                    self.stop_engine(owned).await;
                }
                self.log("门控等的时间到了而引擎还没就绪：放弃，不再重试");
                self.publish_terminal(UpstreamState::Failed, Some(UpstreamReason::StartupTimeout));
                answer(Ok(()));
                Step::Idle(Idle::Terminal)
            }
            // 命令在等待期间收到、门控在这之后才放行：现在按放行之后的规矩回答（`immediate_answer` 只看收到那一刻）。
            Command::Start | Command::Restart if self.frozen() => {
                answer(Err(CommandError::RestartRequired));
                current
            }
            // 并发的 `restart`：它入队的时候一次 `start`、`restart` 操作正在进行（actor 忙着排空、停进程、复位，没有去收命令）或者
            // 刚做完，所以并进那次操作的结果，**不换第二次引擎**。
            Command::Restart if covered => {
                answer(Ok(()));
                current
            }
            Command::Start => match self.state {
                // 已经好了、或正在启动：幂等，什么也不改。
                UpstreamState::Ready | UpstreamState::Starting | UpstreamState::Restarting => {
                    answer(Ok(()));
                    current
                }
                UpstreamState::Stopped
                | UpstreamState::Failed
                | UpstreamState::Absent
                | UpstreamState::Unavailable => {
                    // 操作员显式要求：之前的失败不再计数。一个还活着的自有引擎（`unavailable`：活着却不答）由 `attempt()` 开头
                    // 走完整的退役路径（关门、排空、停进程、封上端点），再重新走启动决策（G3）。
                    self.window.clear();
                    self.op = Some(Op::Start);
                    // 先发布 `starting` 再回答：Admin 的 202 体是受理之后的元素，不能还说 `stopped`（数据面也马上对它 503）。
                    self.publish(UpstreamState::Starting, None);
                    answer(Ok(()));
                    Step::Attempt
                }
            },
            Command::Stop => {
                // 一个 `restart` 的并发合并不跨过 `stop`（Codex 预审 M2）：在它之后入队的 `restart` 是对 `stop` 之后那个状态的请求，不并进
                // 它之前那次操作的结果，按当时的状态回答（已停下，没有自有引擎：`NotOwned`）。
                if seq != 0 {
                    self.restart_cover = self.restart_cover.min(seq);
                }
                // `stop` 结束正在进行的 `start`、`restart` 操作（被它打断的启动不会再走到「就绪」）：之后的 `restart` 不并进一个已经不存在的操作。
                self.op = None;
                if self.engine.is_some() {
                    // 次序（IDORIS-CORE S6，Codex 预审 2）：先关 Supervisor 的门（新的加载、聊天不再准入），再发布「不可服务」的
                    // 状态（数据面立刻 503，不靠请求入口那一次性的状态检查）、再回答，最后才排空、停进程。引擎在这几步里还握在手里，
                    // 所以 `owned` 与 `pid` 还在：`stopped` 加 `owned: true` 是「已经不服务、进程在退出」，进程没了才是 `owned: false`。
                    self.close_admission().await;
                    self.publish_terminal(UpstreamState::Stopped, None);
                    answer(Ok(()));
                    self.retire_accepted = self.engine.as_ref().map(|owned| owned.engine.pid());
                    let retired = self.retire_engine().await;
                    if matches!(retired, Retired::Shutdown) {
                        // iDoris 在停：主循环按停止指令收尾（引擎还握在手里）。
                        return current;
                    }
                    if matches!(retired, Retired::Confirmed) {
                        // 进程确认消失：账本里的模型都不在了，让 Supervisor 与共享容量账本也知道（不问引擎，它已经没有了）。
                        // 门保持关着：下一次 `start` 就绪之后复位、开门。
                        self.forget_engine().await;
                        self.publish_terminal(UpstreamState::Stopped, None);
                        Step::Idle(Idle::Stopped)
                    } else {
                        // 没能确认进程已经消失：不能说它停了。端点保持封上、围栏不动。
                        self.log("停止引擎没能确认进程已经消失：端点保持封上，围栏不动");
                        self.publish_terminal(
                            UpstreamState::Unavailable,
                            Some(UpstreamReason::PortInUse),
                        );
                        Step::Idle(Idle::Terminal)
                    }
                } else if self.state == UpstreamState::Stopped {
                    // 目标状态已经达成：幂等。
                    answer(Ok(()));
                    Step::Idle(Idle::Stopped)
                } else {
                    answer(Err(CommandError::NotOwned));
                    current
                }
            }
            Command::Restart => {
                if self.engine.is_some() {
                    // 次序同 `stop`：先关门，再发布，再回答，最后才排空、停旧引擎。状态是 `restarting`，从受理一直到新引擎就绪
                    // （Agent24 只对 `restarting` 有界等待）；这是操作员要的重启，不记失败。
                    self.op = Some(Op::Restart);
                    self.close_admission().await;
                    self.publish_uncounted(UpstreamState::Restarting, None);
                    answer(Ok(()));
                    self.retire_accepted = self.engine.as_ref().map(|owned| owned.engine.pid());
                    // 没能确认旧进程消失时也往下走：`attempt()` 开头的孤儿接管按既有的规矩处理（记录还在，围栏没清）。
                    // 旧进程确认消失了：它的模型占着的共享容量现在就放掉（Codex 终审 M2）。新引擎要是起不来（找不到可执行文件、
                    // 起不来），账本若还留着旧进程的占用，别的上游申请同一份容量就一直被它挡着；门保持关着，新进程就绪之后再单独采样。
                    match self.retire_engine().await {
                        Retired::Confirmed => self.forget_engine().await,
                        Retired::Unconfirmed => {}
                        Retired::Shutdown => return current,
                    }
                    self.window.clear();
                    Step::Attempt
                } else {
                    answer(Err(CommandError::NotOwned));
                    current
                }
            }
        }
    }

    /// iDoris 停止：收尾。`stop_engine` 为真才停自己拥有的引擎，在 `deadline` 之前（见 [`Command::Shutdown`]）。
    async fn shutdown(&mut self, stop_engine: bool, deadline: Option<tokio::time::Instant>) {
        // 受理了的 `stop`、`restart` 的那个引擎（还握在手里的，或退役没能确认的）要停，不管 `stop_on_exit`；换上来的替身不算。
        let intent = self.retire_accepted.is_some_and(|pid| match &self.engine {
            Some(owned) => owned.engine.pid() == pid,
            None => self
                .condemned
                .as_ref()
                .is_some_and(|condemned| condemned.pid == pid),
        });
        let stop_engine = stop_engine || intent;
        // 退役没能确认的进程（没有握在手里，所有权记录还在）：要停就在期限里再认回一次、带身份闸地停（Codex 复审 M4）。
        if stop_engine
            && self.engine.is_none()
            && self.condemned.is_some()
            && self.finish_stop_by_recovery(deadline).await
        {
            self.clear_retirement();
        }
        self.external = None;
        match self.engine.take() {
            // 停掉自己拥有的引擎，在 `deadline` 之前：先等进程退出，之后才封上端点（G2，见 `stop_engine_by`）。
            Some(owned) if stop_engine => {
                self.stop_engine_by(owned, deadline).await;
            }
            // `stop_engine` 为假：让它继续跑（`StopOnExit::Never` 拉起的，句柄丢弃时不杀进程组），所有权记录留给下一个
            // iDoris；这个 iDoris 不再给它发 key。没有自有引擎（外部的、没起来的）：同样封上。
            _ => self.plan.backend.block(&self.plan.endpoint),
        }
    }

    // -----------------------------------------------------------------------------------------
    // 步骤
    // -----------------------------------------------------------------------------------------

    async fn attempt_cycle(&mut self) -> Step {
        // 操作员的 `restart` 从受理到就绪一直是 `restarting`（Agent24 对它有界等待），不记失败；别的（冷启动、`start`、半开重试）
        // 是 `starting`。
        if self.op == Some(Op::Restart) {
            self.publish_uncounted(UpstreamState::Restarting, None);
        } else {
            self.publish(UpstreamState::Starting, None);
        }
        let outcome = self.attempt().await;
        // 默认拒绝（Codex 终审 H2）：启动决策不论怎么走出来（终局的失败、重判耗尽、放弃、被命令打断、一次运行期失败），只要没有落到
        // 「引擎就绪、有持续的监督」上，端点就封上。接管外部引擎时，校验通过的监听者已经被登记成「外部」（为了带 key 探测它），带 key 的
        // 探测没有结论、被打断、要退避重来……这些出口之后都没有监督去重新校验端口，那条登记不能留着。
        if !matches!(outcome, Attempt::Ready) && self.engine.is_none() && self.external.is_none() {
            self.plan.backend.block(&self.plan.endpoint);
        }
        match outcome {
            Attempt::Ready => {
                self.redecide_since = None;
                match self.sync_supervisor().await {
                    Synced::Done => {
                        self.publish(UpstreamState::Ready, None);
                        self.finish_op();
                        Step::Supervise
                    }
                    // Supervisor 的账本没能跟上：这个引擎不能放行，停掉它（`sync_supervisor` 已经停了），按一次失败算，退避之后重来。
                    Synced::Retry => {
                        self.revoke_external_authorization();
                        self.fail(Some(UpstreamReason::StartupLatched)).await
                    }
                    // iDoris 在停：引擎还握在手里，主循环马上收尾。
                    Synced::Shutdown => Step::Attempt,
                    // Supervisor 没有了：不会自己好，要重启 iDoris。
                    Synced::Permanent => {
                        self.revoke_external_authorization();
                        self.publish_terminal(
                            UpstreamState::Failed,
                            Some(UpstreamReason::StartupLatched),
                        );
                        self.finish_op();
                        Step::Idle(Idle::Terminal)
                    }
                }
            }
            Attempt::Terminal { state, reason } => {
                self.redecide_since = None;
                // 终局的失败之后端点保持封上（Codex 审计 H1）：上面那条对每一条非 `Ready` 的出口统一做了。
                self.publish_terminal(state, reason);
                self.finish_op();
                Step::Idle(Idle::Terminal)
            }
            Attempt::Failure { reason } => {
                self.redecide_since = None;
                self.fail(reason).await
            }
            Attempt::Interrupted => Step::Attempt,
            Attempt::Redecide => self.redecide().await,
        }
    }

    /// 一次 `start`、`restart` 操作做完了（就绪，或者不会再往前走）：之前入队的 `restart` 都并进它的结果（[`Self::restart_cover`]）。
    fn finish_op(&mut self) {
        if self.op.take().is_some() {
            // 这一轮走的是外部监听者那条路：结果是接管了一个外部引擎，不是我们换了一个自有引擎，之前入队的 `restart` 没有可并入的结果
            // （Codex 复审 M6）：不更新截断点，它们按各自到达时的状态回答。
            if !self.external_path {
                self.restart_cover = self.shared.seq_now(&self.plan.id);
            }
        }
    }

    /// 启动决策的前提在判断期间变了（[`Attempt::Redecide`]）：回到决策的开头重来。**不是引擎失败**：不记入熔断窗口、不退避、不计
    /// `failure_count`。这一串重来整体受一个启动超时约束，到点按「有东西占着端口答不上来」收尾（同 `wait_for_answer`）。
    async fn redecide(&mut self) -> Step {
        let started = *self.redecide_since.get_or_insert_with(Instant::now);
        if started.elapsed() >= self.timings().startup_timeout {
            self.redecide_since = None;
            // 重判耗尽直接发布终态，不经 `Attempt::Terminal`；端点已经在 `attempt_cycle` 里随这一轮的出口封上了（Codex 终审 H2）。
            self.publish_terminal(
                UpstreamState::Unavailable,
                Some(UpstreamReason::StartupTimeout),
            );
            self.finish_op();
            return Step::Idle(Idle::Terminal);
        }
        self.log("端口上的情况在判断期间变了：重新判断（不算失败）");
        let pause = self.timings().ready_initial_interval;
        let _ = self.wait(pause).await;
        Step::Attempt
    }

    /// 一次运行期失败：记入熔断窗口；超过上限就是 `failed` + `crash_loop`（之后半开重试），否则 `restarting`，
    /// 退避（1、2、4 秒起翻倍，封顶 30 秒，±20%）之后再试。
    async fn fail(&mut self, reason: Option<UpstreamReason>) -> Step {
        let now = Instant::now();
        let failures = self.window.record(now);
        // 操作员的 `restart` 已经把状态发布成 `restarting`（没有「进入」这回事）：这一次是真的失败，按次计数。
        let count = if self.op == Some(Op::Restart) && self.state == UpstreamState::Restarting {
            Count::Force
        } else {
            Count::Edge
        };
        if self.window.tripped(now) {
            self.emit(
                UpstreamState::Failed,
                Some(UpstreamReason::CrashLoop),
                true,
                count,
            );
            self.finish_op();
            return Step::Idle(Idle::CrashLoop);
        }
        self.emit(UpstreamState::Restarting, reason, false, count);
        let delay = restart_delay(failures, self.timings());
        let _ = self.wait(delay).await;
        Step::Attempt
    }

    async fn idle(&mut self, idle: Idle) -> Step {
        let delay = match idle {
            Idle::CrashLoop => self.timings().half_open_interval,
            // 没有计时器：只等命令。给一个足够长的上限，`wait` 返回「没被打断」时重新等。
            Idle::Terminal | Idle::Stopped => Duration::from_secs(24 * 3600),
        };
        if self.wait(delay).await {
            return Step::Idle(idle);
        }
        match idle {
            // 门控已经在这个实例没就绪的时候降级放行了：不再半开重试（重试好了也读不到：Supervisor 的快照是放行时
            // 创建的，要重启 iDoris）。
            Idle::CrashLoop if self.frozen() => Step::Idle(idle),
            // 半开：试一次。
            Idle::CrashLoop => Step::Attempt,
            Idle::Terminal | Idle::Stopped => Step::Idle(idle),
        }
    }

    // -----------------------------------------------------------------------------------------
    // 启动决策
    // -----------------------------------------------------------------------------------------

    async fn attempt(&mut self) -> Attempt {
        // 不丢弃一个还活着的引擎句柄：丢弃就是 SIGKILL，绕过 `stop_engine`（优雅停止、封上端点），也没有人知道引擎换了。
        // 对一个还活着却不答的自有引擎 `start`（`unavailable`，G3）走到这里时它还在：走完整的退役路径（关门、排空、停进程、封上
        // 端点），再重新走启动决策。
        // 半拉起的引擎被一个后来过期的命令打断：命令没有被收下，继续等同一个引擎就绪（Codex 复审 M7）。
        if self.handed_off
            && let Some(mut owned) = self.engine.take()
        {
            self.handed_off = false;
            self.starting_owned = true;
            let waited = self.wait_owned_ready(&mut owned).await;
            self.starting_owned = false;
            return match waited {
                Ok(()) => self.finish_owned(owned).await,
                Err(failure) => self.abandon(owned, failure).await,
            };
        }
        if self.engine.is_some() {
            match self.retire_engine().await {
                // 同 `restart`：旧进程确认消失，它占着的共享容量现在就放掉（Codex 终审 M2）。
                Retired::Confirmed => self.forget_engine().await,
                Retired::Unconfirmed => {}
                // iDoris 在停：主循环按停止指令收尾，这一轮不再往下走。
                Retired::Shutdown => return Attempt::Interrupted,
            }
        }
        self.external = None;
        self.adopted_listeners = None;
        self.external_path = false;
        let inspector = self.shared.config.inspector.clone();

        // 1. 所有权记录，先于探测。
        let Some(probe_instance) = self.probe_instance() else {
            self.log("无法为这个实例建立引擎描述");
            return Attempt::Terminal {
                state: UpstreamState::Failed,
                reason: None,
            };
        };
        let mut unreadable = 0;
        let adopted = loop {
            match OwnedEngine::recover(&probe_instance, &inspector).await {
                Ok(Recovery::NoRecord | Recovery::Stale(_)) => {
                    // 记录没了或作废了：之前没能确认的退役进程不一定不在了（记录作废也可能是命令行变了、而同一个进程还在听，Codex 复审 M6）。
                    // 用留着的完整记录重建句柄、过身份闸再停一次（进程没了、pid 给了别人就什么也不发，算消失；读不出来就按一次失败退避，复审四 H）。
                    if let Some(record) = self.condemned.clone() {
                        let engine = OwnedEngine::from_record(&probe_instance, record, &inspector);
                        if !self.drive_stop(engine).await {
                            return if self.shutdown_order().is_some() {
                                Attempt::Interrupted
                            } else {
                                Attempt::Failure {
                                    reason: Some(UpstreamReason::PortInUse),
                                }
                            };
                        }
                        self.clear_retirement();
                        self.forget_engine().await;
                    }
                    break None;
                }
                Ok(Recovery::Adopted(engine)) => {
                    // 刚才退役时没能确认消失的那个进程（它还活着、认得出来）：不是新引擎，不能放行（Codex 预审 M4）。先把它停掉，
                    // 确认消失才往下拉起新的；停不掉就按一次失败退避重试。
                    let record = engine.record();
                    if self.condemned.as_ref().is_some_and(|condemned| {
                        condemned.pid == record.pid && condemned.started_at == record.started_at
                    }) {
                        self.log("认回的是之前没能确认停掉的那个进程：先把它停掉，不放行");
                        // 走和退役同一条有期限、可被停止指令打断的停止（Codex 复审 H2）。
                        if !self.drive_stop(engine).await {
                            return if self.shutdown_order().is_some() {
                                Attempt::Interrupted
                            } else {
                                Attempt::Failure {
                                    reason: Some(UpstreamReason::PortInUse),
                                }
                            };
                        }
                        self.clear_retirement();
                        // 退役终于确认了：旧进程占着的共享容量现在放掉（Codex 复审 M5），不等新引擎起来。
                        self.forget_engine().await;
                        break None;
                    }
                    if self.fence_is_present() {
                        // 引擎可能正处于加载中，我们对它无从确认：它是我们的，所以杀掉整组、确认消失、清围栏、重拉。
                        self.log("接管到自己遗留的引擎，但有加载围栏：停掉重拉");
                        let pid = engine.pid();
                        if !self.drive_stop(engine).await {
                            return self.recovery_stop_failed();
                        }
                        self.recovery_stopped(pid).await;
                        break None;
                    }
                    let key = engine.api_key().cloned();
                    break Some(Owned::new(
                        engine,
                        key,
                        self.shared.config.inspector.clone(),
                    ));
                }
                Ok(Recovery::Starting(engine)) => {
                    // 上一个 iDoris 在引擎启动途中被杀：它没有就绪过，什么都没加载，停掉重拉。
                    self.log("遗留的引擎没有封印（启动途中被杀）：停掉重拉");
                    let pid = engine.pid();
                    if !self.drive_stop(engine).await {
                        return self.recovery_stop_failed();
                    }
                    self.recovery_stopped(pid).await;
                    break None;
                }
                Ok(Recovery::Exiting(engine)) => {
                    // 上一代引擎正在退出：pid、启动时间、进程组、命令都吻合，只是端口上没有它在监听（它收到 SIGTERM 之后关了监听、
                    // 还在等在途的生成结束，而上一个 iDoris 在这期间被 launcher SIGKILL 了）。它是我们的：有界地停掉
                    // （SIGTERM、宽限、SIGKILL），确认消失之后才删记录，再重拉。不能当作记录作废丢掉：那会留下一个没人管的引擎。
                    self.log(
                        "遗留的引擎正在退出（端口上已没有它在监听）：有界地停掉，确认消失之后重拉",
                    );
                    let pid = engine.pid();
                    if !self.drive_stop(engine).await {
                        return self.recovery_stop_failed();
                    }
                    self.recovery_stopped(pid).await;
                    break None;
                }
                Ok(Recovery::Unverifiable(_)) => {
                    // 记录在，进程在，但身份读不出来：既不能认回也不能停，端口很可能还被它占着。重读几次。
                    unreadable += 1;
                    if unreadable > self.timings().unverifiable_retries {
                        self.log("遗留的所有权记录对应的进程身份读不出来：不发信号，保留记录");
                        return Attempt::Failure {
                            reason: Some(UpstreamReason::PortInUse),
                        };
                    }
                    let pause = self.timings().unverifiable_pause;
                    if self.wait(pause).await {
                        return Attempt::Interrupted;
                    }
                }
                Err(error) => {
                    self.log(&format!("读所有权记录失败（{}）", error.reason_code()));
                    return Attempt::Failure { reason: None };
                }
            }
        };

        // 退役时没能确认消失的进程还没有落定（记录没了、作废了，进程却还在）：它可能还握着端口、还在加载。没有它已经消失的证据，不探测、不清围栏、
        // 不拉起替身（Codex 复审 H3），端点保持封上，按一次失败退避再认。
        if adopted.is_none() && self.condemned.is_some() {
            self.log("之前没能确认停掉的进程还在：不拉起新的，端点保持封上，稍后再认");
            return Attempt::Failure {
                reason: Some(UpstreamReason::PortInUse),
            };
        }
        // iDoris 在停：不再往下认回、拉起（Codex 复审 H2）。认回的引擎放回手里，由主循环按停止指令收尾。
        if self.shutdown_order().is_some() {
            if let Some(owned) = adopted {
                self.engine = Some(owned);
            }
            return Attempt::Interrupted;
        }
        // 接管了自己遗留的引擎：用记录里的 key（先确认监听端口的是它）等就绪。
        if let Some(mut owned) = adopted {
            self.starting_owned = true;
            let waited = self.wait_owned_ready(&mut owned).await;
            self.starting_owned = false;
            return match waited {
                Ok(()) => self.finish_owned(owned).await,
                Err(failure) => self.abandon(owned, failure).await,
            };
        }

        // 2. 探测 endpoint。
        let probe_recipe = self.plan.backend.probe_recipe();
        match self.probe(&probe_recipe, None).await {
            EngineProbe::ConnectionRefused => {}
            first @ (EngineProbe::Ready | EngineProbe::AuthRejected(_)) => {
                self.external_path = true;
                return self.adopt_external(&probe_recipe, first).await;
            }
            EngineProbe::NotThisEngine => {
                self.external_path = true;
                self.log("endpoint 上有别的服务（响应不像这种引擎）");
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: Some(UpstreamReason::PortInUse),
                };
            }
            EngineProbe::Unavailable => {
                // 有东西在监听但还答不上来（外部引擎正在启动或很忙，或别的什么）：等它，不拉起第二个。
                self.external_path = true;
                return self.wait_for_answer(&probe_recipe).await;
            }
        }

        // 3. 可执行文件。
        let explicit = self
            .shared
            .config
            .executables
            .get(self.plan.backend.kind().as_str())
            .cloned();
        let found = match probe_recipe.discover(explicit.as_deref()).await {
            Ok(found) => found,
            Err(DiscoveryError::NotFound) => {
                return Attempt::Terminal {
                    state: UpstreamState::Absent,
                    reason: Some(UpstreamReason::EngineNotFound),
                };
            }
            Err(DiscoveryError::Rejected(rejections)) => {
                let detail: Vec<String> = rejections
                    .iter()
                    .map(|r| format!("{}：{}", r.path.display(), r.reason.code()))
                    .collect();
                self.log(&format!(
                    "找到的可执行文件没有通过校验（{}）",
                    detail.join("；")
                ));
                return Attempt::Terminal {
                    state: UpstreamState::Absent,
                    reason: Some(UpstreamReason::EngineNotFound),
                };
            }
        };

        // 4. 拉起。
        self.spawn_and_wait(found.path).await
    }

    /// 外部引擎：先过端口属主校验（#619），之后才可能发 key；不符就不发 key、不接管。
    async fn adopt_external(
        &mut self,
        recipe: &Arc<dyn EngineRecipe>,
        first: EngineProbe,
    ) -> Attempt {
        let backend = self.plan.backend.clone();
        let Some(policy) = backend.listener_policy() else {
            self.log("这种引擎没法校验端口属主：不接管外部引擎");
            return Attempt::Terminal {
                state: UpstreamState::Failed,
                reason: Some(UpstreamReason::PortInUse),
            };
        };
        let verifier = ListenerVerifier::new(self.shared.config.inspector.clone(), policy);
        let listeners = match verifier.check(self.plan.port).await {
            ListenerVerdict::Verified { listeners } => listeners,
            ListenerVerdict::NoListener => {
                // 探测与校验之间监听者没了：回到决策的开头重来（不是引擎失败：没有哪个我们拉起的进程出了问题）。
                return Attempt::Redecide;
            }
            ListenerVerdict::Rejected(rejection) => {
                self.log(&format!(
                    "endpoint 上的监听者不是这种引擎（{}）：不发 key，不接管",
                    rejection.code()
                ));
                backend.block(&self.plan.endpoint);
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: Some(UpstreamReason::PortInUse),
                };
            }
            ListenerVerdict::Unverifiable(_) => {
                self.log("读不出 endpoint 上监听者的身份：不发 key，不接管");
                backend.block(&self.plan.endpoint);
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: Some(UpstreamReason::PortInUse),
                };
            }
        };
        // 退役时没能确认消失的自有进程，不能以「外部引擎」的身份回来（Codex 复审 M6）：它还握着端口，却已经被我们判了退役。
        if let Some(condemned) = &self.condemned
            && listeners
                .iter()
                .any(|listener| listener.pid == condemned.pid)
        {
            self.log("端口上监听的是之前没能确认停掉的那个进程：不当作外部引擎接管");
            backend.block(&self.plan.endpoint);
            return Attempt::Failure {
                reason: Some(UpstreamReason::PortInUse),
            };
        }
        self.adopted_listeners = Some(listeners.clone());
        let external = External::new(verifier.clone(), self.plan.port, &listeners);
        // 通过：`settings.json` 的 key 现在可以发给它了（环境变量的 key 一直可以）。每个发往它的请求发出之前，守卫都重新确认监听者
        // 还是刚才校验通过的这些（Codex 终审 H1）。
        backend.govern_external(&self.plan.endpoint, external.guard.clone());
        let adopted = match first {
            EngineProbe::Ready => true,
            _ => {
                let key = backend.current_key(&self.plan.endpoint);
                match key {
                    None => {
                        return Attempt::Terminal {
                            state: UpstreamState::Unavailable,
                            reason: Some(UpstreamReason::AuthRequired),
                        };
                    }
                    // key 先取好（上面），再经授权路径探测：守卫重新确认监听者、只拨核验过的地址、走新开的连接（Codex 复审 L2 的 M2'）。
                    // 授权被拒绝（校验通过之后、发出之前监听者变了或确认不了）是 `Unavailable`：什么都没发，退避之后从头再来。
                    Some(key) => match self.probe_authorized(recipe, &key).await {
                        EngineProbe::Ready => true,
                        EngineProbe::AuthRejected(_) => {
                            return Attempt::Terminal {
                                state: UpstreamState::Unavailable,
                                reason: Some(UpstreamReason::AuthFailed),
                            };
                        }
                        EngineProbe::NotThisEngine => {
                            return Attempt::Terminal {
                                state: UpstreamState::Failed,
                                reason: Some(UpstreamReason::PortInUse),
                            };
                        }
                        // 连接被拒：端口上已经没有监听者了（确定地消失了）。
                        EngineProbe::ConnectionRefused => false,
                        // 有东西在监听、带 key 却答不上来：是不是还是刚才校验通过的那个，下面再确认一次。
                        EngineProbe::Unavailable => {
                            return self
                                .after_inconclusive_keyed_probe(&verifier, &listeners)
                                .await;
                        }
                    },
                }
            }
        };
        if !adopted {
            // 带 key 的探测被拒绝连接：监听者确定地没了，回到决策的开头重来，不是引擎失败。
            return Attempt::Redecide;
        }
        // 外部引擎的围栏不是我们的：没有证据证明它的加载已经停止，原样留着，要人工处理。但 Supervisor 在围栏存在时会把启动
        // 错误粘滞住，数据面之后一直 503：所以不放行，把原因说清楚（`fence_pending`），而不是放行之后报 `startup_latched`。
        if self.fence_is_present() {
            self.log("外部引擎的加载围栏还在（上一个 iDoris 在加载中被停）：不放行，需要人工确认后删除围栏");
            return Attempt::Terminal {
                state: UpstreamState::Unavailable,
                reason: Some(UpstreamReason::FencePending),
            };
        }
        self.external = Some(external);
        Attempt::Ready
    }

    /// 接管外部引擎时，带 key 的探测没有结论（有东西在监听，却答不上来）。**只有确认监听者消失、或者换了一个身份时才回到决策开头重来
    /// （不是引擎失败）**；一个稳定的、还是刚才校验通过的监听者一直这样，就是它的毛病：按一次失败算，退避、熔断照旧（S5 的语义；Codex
    /// 终审 M3）。读不出来（`Unverifiable`）既不能说它走了也不能说它换了，不免计。
    async fn after_inconclusive_keyed_probe(
        &mut self,
        verifier: &ListenerVerifier,
        verified: &[ListenerIdentity],
    ) -> Attempt {
        match verifier.check(self.plan.port).await {
            ListenerVerdict::Verified { listeners } if listeners == verified => {
                self.log("外部引擎的监听者没有变，但带 key 的探测一直答不上来：按失败算");
                Attempt::Failure { reason: None }
            }
            ListenerVerdict::Unverifiable(_) => Attempt::Failure { reason: None },
            // 没有监听者了，或者监听者换了、不合格：不是这个引擎的毛病。
            ListenerVerdict::NoListener
            | ListenerVerdict::Rejected(_)
            | ListenerVerdict::Verified { .. } => Attempt::Redecide,
        }
    }

    /// 端点上有服务在监听，但探测还答不上来：每个探测间隔再试，直到有结论或启动超时。
    async fn wait_for_answer(&mut self, recipe: &Arc<dyn EngineRecipe>) -> Attempt {
        let t = self.timings().clone();
        let deadline = Instant::now() + t.startup_timeout;
        let mut interval = t.ready_initial_interval;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Attempt::Terminal {
                    state: UpstreamState::Unavailable,
                    reason: Some(UpstreamReason::StartupTimeout),
                };
            }
            if self.wait(interval.min(deadline - now)).await {
                return Attempt::Interrupted;
            }
            interval = interval.saturating_mul(2).min(t.ready_max_interval);
            match self.probe(recipe, None).await {
                EngineProbe::Unavailable => {}
                // 监听者没了：回到决策的开头（可能要拉起）。这不是引擎失败：刚才答不上来的是别的东西（或者一个正在退出的
                // 进程，连接被它接受了又被重置），现在端口上什么都没有，**还没有哪个我们拉起的进程出过问题**，所以不记入熔断窗口、
                // 不退避（Codex 预审 6）。
                EngineProbe::ConnectionRefused => return Attempt::Redecide,
                first @ (EngineProbe::Ready | EngineProbe::AuthRejected(_)) => {
                    return self.adopt_external(recipe, first).await;
                }
                EngineProbe::NotThisEngine => {
                    return Attempt::Terminal {
                        state: UpstreamState::Failed,
                        reason: Some(UpstreamReason::PortInUse),
                    };
                }
            }
        }
    }

    /// 准备、拉起、等就绪、封印。
    async fn spawn_and_wait(&mut self, executable: PathBuf) -> Attempt {
        // 探测与找可执行文件期间 iDoris 开始停了：不再清围栏、拉起（`stop_on_exit:"never"` 的子进程会被留下，Codex 复审三 M）。
        if self.shutdown_order().is_some() {
            return Attempt::Interrupted;
        }
        let backend = self.plan.backend.clone();
        let state_dir = self.shared.config.state_dir.clone();
        let budget = self.shared.config.allocated_budget_bytes;
        // 额度为 0：什么都装不下，不拉起（不足 1 GiB 的额度由配方拒绝，同一个原因码）。
        if budget == Some(0) {
            return Attempt::Terminal {
                state: UpstreamState::Failed,
                reason: Some(UpstreamReason::BudgetZero),
            };
        }
        // 每次拉起都重新准备：iDoris 自己的 base、最新的模型目录（S4 的 `managed_models_dir()`）。
        let recipe = match backend.launch_recipe(&state_dir) {
            Ok(recipe) => recipe,
            Err(failure) => {
                self.log(&failure.message);
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: failure.reason,
                };
            }
        };
        let instance = match EngineInstance::new(recipe, &state_dir, self.plan.port) {
            Ok(instance) => instance,
            Err(_) => {
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: None,
                };
            }
        };
        let key = if backend.supports_private_key() {
            match EngineKey::generate() {
                Ok(key) => Some(key),
                Err(_) => {
                    self.log("无法生成引擎的私有 key");
                    return Attempt::Failure { reason: None };
                }
            }
        } else {
            None
        };
        let stop_on_exit = self.shared.config.stop_on_exit;
        let launch_with = || {
            EngineLaunch::new(
                &instance,
                EngineLaunchSpec {
                    executable: executable.clone(),
                    allocated_budget_bytes: budget,
                    api_key: key.clone(),
                    stop_on_exit,
                },
                std::env::vars_os(),
            )
        };
        // 先构造一次，把配置性的失败（额度、base、模型目录）分清楚，再拉起；这一次的摘要写成一行日志（下面，拉起之前）。
        let summary = match launch_with() {
            Ok(launch) => backend.launch_summary(&launch),
            Err(error) => {
                self.log(&format!("引擎的启动命令不合规（{}）", error.reason_code()));
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: launch_reason(&error),
                };
            }
        };
        // 一个新拉起的引擎什么都没加载：之前的围栏说的是已经不存在的上一代。清不掉就不拉起（围栏会让每次加载都 fail-closed）。
        if self.clear_fence().is_err() {
            self.log("无法清除加载围栏");
            return Attempt::Terminal {
                state: UpstreamState::Failed,
                reason: Some(UpstreamReason::FencePending),
            };
        }
        // 引擎会改写自己的进程标题（oMLX 就是），`ps` 里看不到端口、模型目录、内存护栏、base 目录：排查靠这一行。没有 key。
        self.log(&format!("拉起引擎：{summary}"));
        let inspector = self.shared.config.inspector.clone();
        let launched_by = self.shared.config.launched_by.clone();
        let mut spawned = None;
        for attempt in 0..BUSY_RETRIES {
            let launch = match launch_with() {
                Ok(launch) => launch,
                Err(_) => break,
            };
            match OwnedEngine::spawn(launch, inspector.clone(), &launched_by).await {
                Ok(engine) => {
                    spawned = Some(Ok(engine));
                    break;
                }
                // 刚写好的可执行文件偶尔「text file busy」（别的线程 fork 时写端还没关）：短暂重试。
                Err(EngineError::Spawn(std::io::ErrorKind::ExecutableFileBusy))
                    if attempt + 1 < BUSY_RETRIES =>
                {
                    tokio::time::sleep(BUSY_PAUSE).await;
                }
                Err(error) => {
                    spawned = Some(Err(error));
                    break;
                }
            }
        }
        let engine = match spawned {
            Some(Ok(engine)) => engine,
            Some(Err(error)) => {
                self.log(&format!("拉起引擎失败（{}）", error.reason_code()));
                return Attempt::Failure {
                    // 这个实例已有一份可能还活着的记录：端口多半还被它占着。
                    reason: matches!(error, EngineError::AlreadyOwned)
                        .then_some(UpstreamReason::PortInUse),
                };
            }
            None => return Attempt::Failure { reason: None },
        };
        let mut owned = Owned::new(engine, key, self.shared.config.inspector.clone());
        self.starting_owned = true;
        let waited = self.wait_owned_ready(&mut owned).await;
        self.starting_owned = false;
        match waited {
            Ok(()) => self.finish_owned(owned).await,
            Err(failure) => self.abandon(owned, failure).await,
        }
    }

    /// 等自己拥有的引擎就绪。**发任何带私有 key 的请求之前**，先确认监听这个端口的就是这个引擎进程（#619）：
    /// 端口上别的东西答话时，不会收到 key。
    async fn wait_owned_ready(&mut self, owned: &mut Owned) -> Result<(), WaitFailure> {
        let t = self.timings().clone();
        let inspector = self.shared.config.inspector.clone();
        let recipe = owned.engine.instance().recipe().clone();
        let deadline = Instant::now() + t.startup_timeout;
        let mut interval = t.ready_initial_interval;
        let mut rejections = 0_u32;
        loop {
            // 读不出来（`Err`）不当作退出，下一轮再看。
            if let Ok(false) = owned.engine.is_running().await {
                return Err(WaitFailure::Exited);
            }
            // **每一次**带私有 key 的探测之前都重新确认监听这个端口的是这个进程，不缓存（Codex 审计 H3）：进程活着不等于端口还是
            // 它的，引擎可以放掉端口而不退出，别的东西接着绑上来；之前确认过一次，不能让之后每一次探测都带着 key 出去。
            let Some(asked) = self
                .unless_shutdown(inspector.listens_on(owned.engine.pid(), owned.engine.port()))
                .await
            else {
                return Err(WaitFailure::Interrupted);
            };
            let listener_confirmed = matches!(asked, Ok(true));
            if listener_confirmed {
                let key = owned.key.as_ref().map(EngineKey::expose);
                match self.probe(&recipe, key).await {
                    EngineProbe::Ready => return Ok(()),
                    // 自己的引擎拒绝自己的私有 key：连续三次就是真的（不是启动中的偶发）。
                    EngineProbe::AuthRejected(_) => {
                        rejections += 1;
                        if rejections >= 3 {
                            return Err(WaitFailure::AuthRejected);
                        }
                    }
                    EngineProbe::ConnectionRefused
                    | EngineProbe::NotThisEngine
                    | EngineProbe::Unavailable => {}
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(WaitFailure::Timeout);
            }
            if self.wait(interval.min(deadline - now)).await {
                return Err(WaitFailure::Interrupted);
            }
            interval = interval.saturating_mul(2).min(t.ready_max_interval);
        }
    }

    /// 引擎就绪之后：封印所有权记录（读它此刻的命令行、确认监听端口的就是这个 pid），登记私有 key。
    async fn finish_owned(&mut self, mut owned: Owned) -> Attempt {
        // 封印要再问一次 `lsof`，也会占着停止的预算：iDoris 要**停**这个引擎时放手，引擎放回手里按停止指令收尾（Codex 复审五 M）。要**留**它
        // （`stop_on_exit:"never"` 的正常退出）时让封印做完，至多到停止期限：取消封印会留下一条没封印的记录，下一个 iDoris 把它当作「启动途中被杀」停掉重拉，
        // 暖着的引擎白白丢了（Codex 复审六 M）。
        let sealed = {
            let seal = owned.engine.seal();
            tokio::select! {
                biased;
                sealed = seal => Some(sealed),
                () = self.seal_cancel_point() => None,
            }
        };
        let Some(sealed) = sealed else {
            self.engine = Some(owned);
            return Attempt::Interrupted;
        };
        match sealed {
            Ok(()) => {}
            Err(error) => {
                let reason = match error {
                    EngineError::PortNotOwned => Some(UpstreamReason::PortInUse),
                    _ => None,
                };
                self.log(&format!("封印所有权记录失败（{}）", error.reason_code()));
                self.stop_engine(owned).await;
                return Attempt::Failure { reason };
            }
        }
        if let Some(key) = &owned.key {
            self.plan
                .backend
                .govern_private(&self.plan.endpoint, key, owned.guard.clone());
        }
        self.owned_key_revoked = false;
        self.engine = Some(owned);
        Attempt::Ready
    }

    /// 等就绪失败：按原因处置这个引擎（失败的原因决定它的去留，不能一概停掉），回报。
    ///
    /// **被 `Shutdown` 打断时不在这里停**（H4）：引擎是不是要停是那条命令说了算（`stop_on_exit:"never"` 的信号停止、以 0 退出：
    /// 引擎留给下一个 iDoris，原来这里一概先停，认回来的、正在探测就绪的引擎就被无条件杀掉了，所有权记录也跟着删）。所有权交给
    /// 暂存的命令：引擎放回 `self.engine`，回报「被打断」，主循环马上处理那条命令（[`Self::shutdown`] 按它的 `stop_engine` 与期限
    /// 处置：停就停，不停就放手、记录原样保留）。启动失败（超时、被拒、退出）与别的命令（门控放弃）照旧停掉半拉起的引擎。
    async fn abandon(&mut self, owned: Owned, failure: WaitFailure) -> Attempt {
        // 被 `Shutdown`、`stop`、`restart` 打断：半拉起的引擎交给那条命令（Codex 预审 M3）。`stop`、`restart` 在 `engine` 里看到它，走
        // 完整的退役（关门、排空、停进程、确认），不是在这里先停了、命令却说「没有自有引擎」。
        if matches!(failure, WaitFailure::Interrupted)
            && (self.pending_is_shutdown()
                || self.pending_is_stop_or_restart()
                || self.shutdown_order().is_some())
        {
            self.handed_off = true;
            self.engine = Some(owned);
            return Attempt::Interrupted;
        }
        let inspector = self.shared.config.inspector.clone();
        let port = self.plan.port;
        self.stop_engine(owned).await;
        match failure {
            WaitFailure::Interrupted => Attempt::Interrupted,
            WaitFailure::Timeout => Attempt::Failure {
                reason: Some(UpstreamReason::StartupTimeout),
            },
            WaitFailure::AuthRejected => Attempt::Failure {
                reason: Some(UpstreamReason::AuthFailed),
            },
            WaitFailure::Exited => {
                // 引擎一起来就退出：最常见的原因是端口已经被别的东西占了（它绑不上）。
                let held =
                    matches!(inspector.listeners_on(port).await, Ok(pids) if !pids.is_empty());
                Attempt::Failure {
                    reason: held.then_some(UpstreamReason::PortInUse),
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Supervisor 的账本跟着引擎换代（IDORIS-CORE S6）
    // -----------------------------------------------------------------------------------------

    /// 此刻引擎进程的身份（就绪之后才有）：自有引擎是 pid 加启动时间，外部引擎是校验通过的监听者的 pid。
    fn current_identity(&self) -> Option<EngineIdentity> {
        match &self.engine {
            Some(owned) => {
                let record = owned.engine.record();
                Some(EngineIdentity::Owned {
                    pid: record.pid,
                    started_at: record.started_at.clone(),
                })
            }
            None => self
                .adopted_listeners
                .clone()
                .map(|listeners| EngineIdentity::External { listeners }),
        }
    }

    /// 完整的 Router 里这个上游的 Supervisor（门控放行之后才有；resident 的托管卡没有 Supervisor）。
    fn supervisor(&self) -> Option<crate::dispatch::BoundSupervisor> {
        if !self.shared.gate_open.load(Ordering::SeqCst) {
            return None;
        }
        self.shared.runtimes.get()?.get(&self.plan.id).cloned()
    }

    /// 让 Supervisor 复位，忙就等，至多等一个启动超时（[`reset_when_quiet`]）。
    async fn reset_supervisor(
        &self,
        bound: &crate::dispatch::BoundSupervisor,
        sample: bool,
    ) -> Result<(), BackendError> {
        let deadline = Instant::now() + self.timings().startup_timeout;
        // iDoris 在停时不再等一个忙的 Supervisor 静下来（复位最长等一个启动超时，比停止期限长得多；Codex 预审 H3）：放手，按「忙」返回。
        self.unless_shutdown(reset_when_quiet(
            bound,
            sample,
            deadline,
            self.timings().ready_initial_interval,
        ))
        .await
        .unwrap_or_else(|| {
            Err(BackendError::busy(
                "iDoris is stopping: the reset was given up",
                None,
                None,
            ))
        })
    }

    /// 引擎换了一个进程（门控放行之后）：让 Supervisor 的账本对着新进程重建，**在发布 `ready` 之前**（之前这个上游对数据面
    /// 是 503，所以没有请求会带着过期的账本进来）。账本是对着旧进程建的：以为已加载的模型已经不在，适配器抓着的私有 key 也换了。
    ///
    /// 什么都不用做：门控还没放行（Supervisor 还不存在，放行时对此刻的引擎采样）、没有 Supervisor（resident 的托管卡）、
    /// 还是同一个进程（`start` 重新认回了同一个引擎：账本没有过期，也不能清掉它对未确认的加载的记忆）。
    async fn sync_supervisor(&mut self) -> Synced {
        let identity = self.current_identity();
        let Some(bound) = self.supervisor() else {
            self.identity = identity;
            return Synced::Done;
        };
        if identity == self.identity {
            // 还是同一个进程：账本没有过期，也不能清掉它对未确认的加载的记忆。这一轮若关过门（外部监听者没了又回来了），
            // 开回来。
            return self.reopen_admission(&bound).await;
        }
        // 引擎换了一个进程：账本要复位。门此刻应该已经关了（`stop`、`restart`、外部监听者换了都先关门，再排空），再关一次不吃亏（幂等）。
        let _ = self.unless_shutdown(bound.close_admission()).await;
        match self.reset_supervisor(&bound, true).await {
            Ok(()) => {
                self.identity = identity;
                // 复位之后才开门：这之前没有任何请求能进来，所以没有请求带着过期的账本进来。
                self.reopen_admission(&bound).await
            }
            Err(_) if self.shutdown_order().is_some() => {
                // iDoris 在停：复位放手了。账本的状态不明，但引擎不在这里停（没有期限）：留在手里，主循环按停止指令带着期限收尾。
                self.identity = None;
                Synced::Shutdown
            }
            Err(error) => {
                // 引擎不能放行：账本的状态不明（粘滞着采样的错误，或者根本没动），也没有人确认它对得上这个进程。停掉引擎；
                // 下一个就绪的引擎一定要再复位。
                self.log(&format!(
                    "引擎换代之后复位 Supervisor 失败（{}）",
                    error.reason_code()
                ));
                self.identity = None;
                if let Some(owned) = self.engine.take() {
                    self.stop_engine(owned).await;
                }
                // 接管的外部引擎：这个实例不再监督它，已验证的登记不能留着（Codex 终审 H2）。
                self.revoke_external_authorization();
                classify_reset_failure(&error)
            }
        }
    }

    /// 复位之后开门（新进程的账本已经对上了）。Supervisor 没了（actor 退出、中毒）是终局。
    async fn reopen_admission(&self, bound: &crate::dispatch::BoundSupervisor) -> Synced {
        match bound.reopen_admission().await {
            Ok(()) => Synced::Done,
            Err(error) => {
                self.log(&format!(
                    "开 Supervisor 的门失败（{}）",
                    error.reason_code()
                ));
                classify_reset_failure(&error)
            }
        }
    }

    /// 自有引擎被停下了（进程确认消失）：Supervisor 账本里的模型都不在了。清空它，不问引擎（它已经没有了），共享容量账本里的
    /// 占用也跟着放掉。失败只记日志：`stopped` 的上游不接受请求，下一次 `start` 之后的就绪会再复位。
    async fn forget_engine(&mut self) {
        self.identity = None;
        let Some(bound) = self.supervisor() else {
            return;
        };
        if let Err(error) = self.reset_supervisor(&bound, false).await {
            self.log(&format!(
                "引擎停止之后清空 Supervisor 的账本失败（{}）",
                error.reason_code()
            ));
        }
    }

    // -----------------------------------------------------------------------------------------
    // 监管
    // -----------------------------------------------------------------------------------------

    async fn supervise(&mut self) -> Step {
        // 两个节拍：进程还在不在（便宜，每 `liveness_interval`）与探测（每 `probe_interval`）。
        let mut last_probe = Instant::now();
        loop {
            let tick = self
                .timings()
                .liveness_interval
                .min(self.timings().probe_interval);
            if self.wait(tick).await {
                return Step::Supervise;
            }
            let probe_due = last_probe.elapsed() >= self.timings().probe_interval;
            if probe_due {
                last_probe = Instant::now();
            }
            if self.engine.is_some() {
                if let Some(step) = self.supervise_owned(probe_due).await {
                    return step;
                }
            } else if self.external.is_some() {
                if probe_due {
                    self.supervise_external().await;
                }
            } else {
                return Step::Attempt;
            }
        }
    }

    /// 门控完成之后，Supervisor 的启动错误粘滞要反映到状态上：引擎本身答得上来也不能报 `ready`（聊天实际是 503）。
    async fn supervisor_latched(&self) -> bool {
        if !self.shared.gate_open.load(Ordering::SeqCst) {
            return false;
        }
        let Some(runtimes) = self.shared.runtimes.get() else {
            return false;
        };
        let Some(bound) = runtimes.get(&self.plan.id) else {
            return false;
        };
        // 超时（Supervisor 还在忙）没有新信息：不当作粘滞。
        matches!(
            tokio::time::timeout(crate::readiness::SUPERVISOR_STATUS_TIMEOUT, bound.status()).await,
            Ok(Err(_))
        )
    }

    /// 一个节拍的自有引擎监管：进程退出了就处理；`probe_due` 时再探测一次。`Some(step)` 表示离开监管。
    async fn supervise_owned(&mut self, probe_due: bool) -> Option<Step> {
        let running = match self.engine.as_mut() {
            Some(owned) => owned.engine.is_running().await,
            None => return Some(Step::Attempt),
        };
        if let Ok(false) = running {
            return Some(self.owned_engine_exited().await);
        }
        // 进程活着不等于端口还是它的（Codex 审计 H3）：引擎可以放掉端口而不退出，别的进程接着绑上来。**每个节拍**重新确认一次
        // 监听这个端口的此刻还是我们的进程（不缓存）；确认不了，先收回私有 key（端点封上）、发布 `unavailable`（`port_in_use`，数据
        // 面立刻 503），这一轮不发探测：发往那个端口的请求不再带私有 key。又确认了，把私有 key 登记回去，接着往下探测。
        // 检查（`lsof`，读不出来时还要按 `unverifiable_retries` 重读、每次隔一段）可能占掉很多秒：iDoris 在停时放手，引擎还握在手里，
        // 主循环马上带着期限收尾（Codex 复审四 H）。
        let confirmed = self
            .unless_shutdown(self.owned_listener_confirmed())
            .await?;
        if !confirmed {
            self.revoke_owned_key();
            self.publish(UpstreamState::Unavailable, Some(UpstreamReason::PortInUse));
            return None;
        }
        if self.owned_key_revoked {
            if let Some(owned) = self.engine.as_ref()
                && let Some(key) = owned.key.as_ref()
            {
                self.plan
                    .backend
                    .govern_private(&self.plan.endpoint, key, owned.guard.clone());
            }
            self.owned_key_revoked = false;
        }
        if !probe_due {
            return None;
        }
        // 进程活着：探测只决定状态，不杀它（oMLX 加载大模型时 `/v1/models` 可能很慢，那不是失效）。
        let (recipe, key) = match self.engine.as_ref() {
            Some(owned) => (
                owned.engine.instance().recipe().clone(),
                owned.key.as_ref().map(|key| key.expose().to_owned()),
            ),
            None => return Some(Step::Attempt),
        };
        let (state, reason) = match self.probe(&recipe, key.as_deref()).await {
            EngineProbe::Ready => {
                if self.supervisor_latched().await {
                    (
                        UpstreamState::Unavailable,
                        Some(UpstreamReason::StartupLatched),
                    )
                } else {
                    (UpstreamState::Ready, None)
                }
            }
            EngineProbe::AuthRejected(_) => {
                (UpstreamState::Unavailable, Some(UpstreamReason::AuthFailed))
            }
            EngineProbe::ConnectionRefused
            | EngineProbe::NotThisEngine
            | EngineProbe::Unavailable => (UpstreamState::Unavailable, None),
        };
        self.publish(state, reason);
        None
    }

    /// 自有引擎此刻还拥有它的端口吗：监听这个端口的就是它的进程（进程本身还在，由调用方先问过 `is_running`；认回的孤儿那一问还
    /// 核对了身份）。**每个监督节拍、等就绪时每一次带私有 key 的探测之前**都要重新问，不缓存（数据面的每个请求只问守卫里原生的那一半：
    /// 登记时认的进程还握着套接字，见 `engine::guard`；这一问是贵的那一半，`lsof`）。检查器读不出来（`lsof` 没答上来）按
    /// `unverifiable_retries` 重读，仍读不出来就当作没有确认：宁可不发。
    async fn owned_listener_confirmed(&self) -> bool {
        let Some(owned) = self.engine.as_ref() else {
            return false;
        };
        let (pid, port) = (owned.engine.pid(), owned.engine.port());
        let inspector = self.shared.config.inspector.clone();
        let mut unreadable = 0;
        loop {
            match inspector.listens_on(pid, port).await {
                Ok(listening) => return listening,
                Err(_) if unreadable < self.timings().unverifiable_retries => {
                    unreadable += 1;
                    tokio::time::sleep(self.timings().unverifiable_pause).await;
                }
                Err(_) => return false,
            }
        }
    }

    /// 不再监督接管的外部引擎（放弃、复位失败、重来）：忘掉它、封上端点，登记表里那条「已验证的外部」不能留着
    /// （Codex 终审 H2）：留着，端口上之后出现的东西，凭那条登记（再加上守卫恰好通过）仍能拿到 `settings.json` 的 key。
    fn revoke_external_authorization(&mut self) {
        if self.external.take().is_some() {
            self.plan.backend.block(&self.plan.endpoint);
        }
    }

    /// 收回自有引擎的私有 key：端点封上，之后什么凭证都不发（回退链不适用），旧一代还在飞的操作也拿不到任何东西。
    fn revoke_owned_key(&mut self) {
        if !self.owned_key_revoked {
            self.log("自有引擎还活着，但此刻监听这个端口的不是它：收回私有 key，端点封上");
        }
        self.plan.backend.block(&self.plan.endpoint);
        self.owned_key_revoked = true;
    }

    /// 自有引擎的进程退出了。门控完成之前：计入熔断窗口，退避重启。门控完成之后：Supervisor 的账本已经过期，
    /// 进程内恢复不了（v0.6.1），通知 `bin` 以退出码 5 优雅停止。
    async fn owned_engine_exited(&mut self) -> Step {
        let dead = self.engine.take();
        // 先发布「不再就绪」，再读 `gate_open`：与 `open_gate`（同一把锁里检查）两者之一必然看到对方。发布时先不写日志：失效锁存之前不碰日志的
        // 出口（它可能阻塞；Codex 复审 M7）。
        self.emit_logged(UpstreamState::Failed, None, false, Count::Edge, false);
        if self.shared.gate_open.load(Ordering::SeqCst) {
            self.emit_logged(UpstreamState::Failed, None, true, Count::Edge, false);
            // **先报失效、后收拾**（Codex 预审 H1）：失效一旦发布就一直成立，停止流程里「引擎失效（5）」排在信号前面；收拾（给死进程收尾、
            // 关 Supervisor 的门：邮箱忙时要等）要是排在报告前面，这段时间里到的 SIGTERM 就把退出码盖成 0。
            self.shared.signal_failure(EngineFailure {
                upstream_id: self.plan.id.clone(),
            });
            self.log(Self::state_line(UpstreamState::Failed, None).as_str());
            self.log("自有引擎已退出，进程内无法恢复（热恢复是 v0.6.1）：iDoris 将以退出码 5 停止，由 agent24d 重启");
            self.cleanup_dead(dead).await;
            return Step::Idle(Idle::Terminal);
        }
        self.log(Self::state_line(UpstreamState::Failed, None).as_str());
        self.cleanup_dead(dead).await;
        self.log("自有引擎已退出");
        self.fail(None).await
    }

    /// 退出的自有引擎的收拾：已经死了，`stop` 只是收尾（删所有权记录，不会向别的进程发信号）；Supervisor 也不再准入新的请求
    /// （放行之前没有 Supervisor，这是空操作）。
    async fn cleanup_dead(&self, dead: Option<Owned>) {
        if let Some(owned) = dead {
            self.stop_engine(owned).await;
        }
        self.close_admission().await;
    }

    /// 一个周期的外部引擎监管：探测失败只报 `unavailable`，iDoris 永远不重启它，也不给它发信号。
    ///
    /// 每个周期先确认监听者还是校验过的那个**进程**（pid 加启动时间都比：只比 pid，pid 被复用就分不出来了）。换了进程，
    /// Supervisor 的账本就是对着旧进程建的，不能直接回 `ready`：先封上端点、关门、排空、复位、开门，之后才发布 `ready`
    /// （[`Self::external_replaced`]，Codex 预审 4）。
    async fn supervise_external(&mut self) {
        let Some(external) = &self.external else {
            return;
        };
        let verifier = external.verifier.clone();
        let guard = external.guard.clone();
        let backend = self.plan.backend.clone();
        match verifier.check(self.plan.port).await {
            ListenerVerdict::Verified { listeners } => {
                let identity = EngineIdentity::External { listeners };
                if self.identity.as_ref() == Some(&identity) {
                    // 还是校验过的那个进程：`settings.json` 的 key 现在可以发给它了（同一个守卫：登记表的代际不动）。
                    backend.govern_external(&self.plan.endpoint, guard);
                    if self.external_gap {
                        // 刚才的「没有监听者」是一次误读，或者它自己回来了：同一个进程，账本没有过期，开回来。
                        if let Some(bound) = self.supervisor()
                            && !matches!(self.reopen_admission(&bound).await, Synced::Done)
                        {
                            return;
                        }
                        self.external_gap = false;
                    }
                } else if !self.external_replaced(identity).await {
                    return;
                }
            }
            // 没有监听者了：这一刻起什么凭证都不能再发给端口上之后出现的东西（它还没有通过校验），不等下一个周期的校验。
            ListenerVerdict::NoListener => {
                // 没有监听者了：这一刻起什么凭证都不能再发给端口上之后出现的东西（它还没有通过校验），不等下一个周期的校验
                // （G2）；Supervisor 的门也关上，之后新的请求立刻 503。
                self.external_unavailable().await;
                self.publish(UpstreamState::Unavailable, None);
                return;
            }
            ListenerVerdict::Rejected(_) | ListenerVerdict::Unverifiable(_) => {
                self.external_unavailable().await;
                self.publish(UpstreamState::Failed, Some(UpstreamReason::PortInUse));
                return;
            }
        }
        let recipe = backend.probe_recipe();
        // key 先取好，再经授权路径探测（守卫重新确认监听者、只拨核验过的地址、走新开的连接；Codex 复审 L2 的 M2'）。授权不过（监听者变了、读不出来）
        // 就这一轮没有结论（`Unavailable`，记一行日志）：下一个周期的校验会把监听者的变化分清楚。
        let key = backend.current_key(&self.plan.endpoint);
        // 带 key 的周期探测走授权（监听者重新确认、代际、核验过的拨号地址与新开的连接），不是对着端口直接发一个裸的 bearer：换主人的窗口里
        // 它不会把 key 发给端口上的另一个进程（Codex 复审四 H）。授权不过（监听者变了、读不出来）就这一轮没有结论，下一个周期重新核对。
        let probed = match key.as_deref() {
            Some(key) => self.probe_authorized(&recipe, key).await,
            None => self.probe(&recipe, None).await,
        };
        let (state, reason) = match probed {
            EngineProbe::Ready => {
                if self.supervisor_latched().await {
                    (
                        UpstreamState::Unavailable,
                        Some(UpstreamReason::StartupLatched),
                    )
                } else {
                    (UpstreamState::Ready, None)
                }
            }
            EngineProbe::AuthRejected(_) => (
                UpstreamState::Unavailable,
                Some(if key.is_some() {
                    UpstreamReason::AuthFailed
                } else {
                    UpstreamReason::AuthRequired
                }),
            ),
            EngineProbe::NotThisEngine => (UpstreamState::Failed, Some(UpstreamReason::PortInUse)),
            EngineProbe::ConnectionRefused | EngineProbe::Unavailable => {
                (UpstreamState::Unavailable, None)
            }
        };
        self.publish(state, reason);
    }

    /// 端口上没有（通过校验的）监听者：封上端点（什么凭证都不发），关 Supervisor 的门。同一个进程回来时开回去
    /// （[`Self::external_gap`]）；换了进程则走 [`Self::external_replaced`]。
    async fn external_unavailable(&mut self) {
        self.plan.backend.block(&self.plan.endpoint);
        self.close_admission().await;
        self.external_gap = true;
    }

    /// 通过校验的监听者换了一个进程：Supervisor 的账本是对着旧进程建的。次序：封上端点（旧一代拿不到凭证）、关门、发布**明确不可服务**的
    /// `restarting`（不记失败）、排空（有界；旧进程已经没了，在途的很快失败）、**重新确认监听者**、登记新监听者的凭证与守卫、复位账本、
    /// 开门；这期间状态不是 `ready`。返回这个周期能不能接着探测、发布状态：复位没成功、或者排空期间监听者又变了就不能（端点保持封上、
    /// 门保持关着，下一个周期再来）。
    ///
    /// Codex 终审 H6：之前这里发布的是没有原因码的 `unavailable`，而数据面的状态门把它当成「只是慢、照常服务」，伴生卡能在账本复位完成
    /// 之前通过健康门；并且新监听者是在排空**之前**验的，排空花了一段时间，之后直接恢复凭证、用排空前的结论复位采样。现在换代期间是
    /// `restarting`（`upstream_serves` 为假，同操作员的 `restart`），排空之后重新取一次监听者、重新核对身份，仍是预期的那个进程才登记、
    /// 采样、开放服务；它在排空期间离开了、或又换成了第三个，不放行。
    async fn external_replaced(&mut self, identity: EngineIdentity) -> bool {
        let EngineIdentity::External {
            listeners: expected,
        } = &identity
        else {
            return false;
        };
        let expected = expected.clone();
        let backend = self.plan.backend.clone();
        self.log("外部引擎换了一个进程：Supervisor 的账本对着新进程重建");
        backend.block(&self.plan.endpoint);
        self.external_gap = true;
        let bound = self.supervisor();
        if let Some(bound) = &bound {
            let _ = self.unless_shutdown(bound.close_admission()).await;
        }
        self.publish_uncounted(UpstreamState::Restarting, None);
        let _ = self.drain().await;
        if self.shutdown_order().is_some() {
            return false;
        }
        // 排空花了一段时间：上面验过的进程可能已经离开，或者换成了第三个。重新取一次监听者、重新核对。
        let Some(verifier) = self
            .external
            .as_ref()
            .map(|external| external.verifier.clone())
        else {
            return false;
        };
        match verifier.check(self.plan.port).await {
            ListenerVerdict::Verified { listeners } if listeners == expected => {}
            ListenerVerdict::Verified { .. } => {
                self.log("外部引擎在排空期间又换了一个进程：不放行，下一个周期重新来");
                return false;
            }
            ListenerVerdict::NoListener => {
                self.external_unavailable().await;
                self.publish(UpstreamState::Unavailable, None);
                return false;
            }
            ListenerVerdict::Rejected(_) | ListenerVerdict::Unverifiable(_) => {
                self.external_unavailable().await;
                self.publish(UpstreamState::Failed, Some(UpstreamReason::PortInUse));
                return false;
            }
        }
        // 还是校验过的那个进程：登记它的凭证与新的守卫（登记表的代际前进），再复位账本。复位的采样要用这份凭证，所以登记在前；但**身份与守卫
        // 在复位成功之后才一起发布**（`self.external`、`self.identity`，Codex 预审 H5）：复位没成功时旧的守卫、旧的身份原样留着，之后旧进程
        // 回来也不会拿到新进程的守卫。
        let external = External::new(verifier.clone(), self.plan.port, &expected);
        backend.govern_external(&self.plan.endpoint, external.guard.clone());
        let Some(bound) = bound else {
            // 没有 Supervisor（门控还没放行、resident 的托管卡）：没有账本要复位。
            self.external = Some(external);
            self.identity = Some(identity);
            self.external_gap = false;
            return true;
        };
        match self.reset_supervisor(&bound, true).await {
            // 复位（采样）花了一段时间，期间端口可能又换了主人（B 走了、C 占了）：开门之前**重新核对**监听者还是复位对着的那个。不是就不开门，
            // 端点保持封上，账本身份作废，下一个周期对着新的监听者重新来（Codex 复审四 H）。
            Ok(())
                if !matches!(
                    verifier.check(self.plan.port).await,
                    ListenerVerdict::Verified { listeners } if listeners == expected
                ) =>
            {
                self.log("外部引擎复位期间监听者又变了：不开门，端点保持封上，下一个周期重新来");
                backend.block(&self.plan.endpoint);
                self.identity = None;
                false
            }
            Ok(()) => match self.reopen_admission(&bound).await {
                Synced::Done => {
                    self.external = Some(external);
                    self.identity = Some(identity);
                    self.external_gap = false;
                    true
                }
                _ => {
                    self.identity = None;
                    backend.block(&self.plan.endpoint);
                    false
                }
            },
            Err(error) => {
                self.log(&format!(
                    "外部引擎换进程之后复位 Supervisor 失败（{}）：端点保持封上，门保持关着",
                    error.reason_code()
                ));
                backend.block(&self.plan.endpoint);
                // 结果不确定（采样超时、截止时间过了、没收到回答：账本可能已经提交了这个进程的内容，也可能没动）：账本跟哪个进程对得上，没人能
                // 担保。清掉身份：之后不论哪个进程回来（含旧的那个）都重走一遍换代，复位对着它重建，不拿旧身份直接开门（Codex 预审 H5）。
                self.identity = None;
                if self.shutdown_order().is_none() {
                    self.publish(
                        UpstreamState::Unavailable,
                        Some(UpstreamReason::StartupLatched),
                    );
                }
                false
            }
        }
    }
}

/// 刚写好的可执行文件偶发 `ETXTBSY` 时 `spawn` 的重试次数与间隔（总共至多约半秒）。
const BUSY_RETRIES: u32 = 50;
const BUSY_PAUSE: Duration = Duration::from_millis(10);

/// Supervisor 复位失败之后的出路：它没有了（actor 已退出，或者检测到自己的账本损坏而中毒：中毒的 actor 不被复位，每次都是同一个
/// 回答）不会自己好，要重启 iDoris；别的（采样不到新进程、围栏还在、一直忙）一会儿再试。
fn classify_reset_failure(error: &BackendError) -> Synced {
    match error {
        BackendError::SupervisorUnavailable | BackendError::InvariantViolation { .. } => {
            Synced::Permanent
        }
        _ => Synced::Retry,
    }
}

/// 让 Supervisor 复位，忙就等（`pause` 一次），到 `deadline` 还忙就把 `Busy` 交出去：账本只在静止时才复位（有进行中的加载或
/// 聊天它回 `Busy`，什么也不改）。引擎已经换了，旧引擎上没做完的调用很快就会失败，所以这个等待很短。
async fn reset_when_quiet(
    bound: &crate::dispatch::BoundSupervisor,
    sample: bool,
    deadline: Instant,
    pause: Duration,
) -> Result<(), BackendError> {
    loop {
        // 一个绝对的截止时间管入队、等安静、回答（Codex 终审 M5）：到点还没开始的复位被 Supervisor 丢弃，不会在调用方放弃之后才执行。
        match bound.engine_replaced_before(sample, deadline).await {
            Err(BackendError::Busy { .. }) if Instant::now() < deadline => {
                tokio::time::sleep(pause.min(deadline.saturating_duration_since(Instant::now())))
                    .await;
            }
            other => return other,
        }
    }
}

/// 启动命令不合规的稳定原因码：额度、base 目录有（规范 §2.11.1），其余没有合适的码，只记日志。
fn launch_reason(error: &LaunchError) -> Option<UpstreamReason> {
    match error {
        LaunchError::BudgetZero => Some(UpstreamReason::BudgetZero),
        LaunchError::Invalid("base_path") => Some(UpstreamReason::BaseUnavailable),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_dead_or_poisoned_supervisor_is_final_and_everything_else_is_retried() {
        for error in [
            BackendError::supervisor_unavailable(),
            BackendError::invariant_violation("ledger lost an entry"),
        ] {
            assert!(
                matches!(classify_reset_failure(&error), Synced::Permanent),
                "{error}"
            );
        }
        for error in [
            BackendError::busy("still in flight", None, None),
            BackendError::Upstream {
                message: "the engine is not answering".into(),
            },
            BackendError::internal("durable load fence is present"),
            BackendError::adapter_timed_out("reset/status"),
        ] {
            assert!(
                matches!(classify_reset_failure(&error), Synced::Retry),
                "{error}"
            );
        }
    }

    /// A Supervisor with a chat in flight answers `Busy` to a reset; the wait retries until it is quiet, and a
    /// deadline in the past hands the `Busy` back instead of waiting.
    #[tokio::test]
    async fn the_reset_waits_for_a_supervisor_that_is_busy_and_gives_up_at_its_deadline() {
        use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
        use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};

        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "a".into(),
            memory_gb: 1.0,
        }]));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let card: idoris_contracts::ComponentCard =
            serde_yaml::from_str(include_str!("../../../../config/components/omlx.yaml")).unwrap();
        let bound = crate::dispatch::BoundSupervisor::new(&card, handle.clone());
        let policy = LoadPolicy {
            mode: LoadMode::OnDemand,
            keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
            admission: Admission::Coexist,
        };
        handle.load("a", 1.0, policy).await.unwrap();
        adapter.set_chat_delay("a", Duration::from_millis(600));
        let chat = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .chat(
                        idoris_backend::ChatRequest {
                            native: None,
                            model: "a".into(),
                            messages: Vec::new(),
                        },
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
            })
        };
        // The chat takes 600 ms; 100 ms in, it is certainly in flight.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No patience: the deadline has passed, so the answer is the `Busy` itself.
        let impatient =
            reset_when_quiet(&bound, true, Instant::now(), Duration::from_millis(10)).await;
        assert!(
            matches!(impatient, Err(BackendError::Busy { .. })),
            "{impatient:?}"
        );
        // With patience it waits out the chat and the reset goes through.
        let patient = reset_when_quiet(
            &bound,
            true,
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
        assert!(patient.is_ok(), "{patient:?}");
        chat.await.unwrap().unwrap();
    }

    #[test]
    fn a_refused_launch_command_maps_to_the_stable_reason_codes_the_spec_has() {
        assert_eq!(
            launch_reason(&LaunchError::BudgetZero),
            Some(UpstreamReason::BudgetZero)
        );
        assert_eq!(
            launch_reason(&LaunchError::Invalid("base_path")),
            Some(UpstreamReason::BaseUnavailable)
        );
        // The model dir has no stable code in the contract: the log carries it.
        for error in [
            LaunchError::Invalid("model_dir"),
            LaunchError::ForbiddenEnv,
            LaunchError::KeyInArgv,
            LaunchError::ExecutableNotAbsolute,
        ] {
            assert_eq!(launch_reason(&error), None, "{error}");
        }
    }
}
