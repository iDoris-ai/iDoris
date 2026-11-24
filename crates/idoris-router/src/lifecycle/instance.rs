//! 一个托管引擎实例的任务：启动决策、等就绪、监管、失败的退避与熔断、命令（接口规范 §2.7.6）。
//!
//! 整体是一台「步骤」状态机：[`Step::Attempt`]（走一遍启动决策）、[`Step::Supervise`]（引擎就绪，周期探测）、
//! [`Step::Idle`]（处于终态，等命令或半开重试）、[`Step::Exit`]。每个等待（退避、等就绪、监管周期、空闲）都可以被命令打断，
//! 命令暂存在 [`Actor::pending`]，主循环在下一个安全点处理它。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use idoris_backend::load_fence::LoadFence;
use idoris_upstream::engine::{
    DiscoveryError, EngineError, EngineInstance, EngineKey, EngineLaunch, EngineLaunchSpec,
    EngineProbe, EngineRecipe, ExternalListenerGuard, LaunchError, ListenerIdentity,
    ListenerVerdict, ListenerVerifier, MANAGED_DIAL, OwnedEngine, OwnedListenerGuard, Recovery,
    fresh_connection_client,
};
use idoris_upstream::omlx::key::EndpointGuard;
use tokio::sync::{mpsc, oneshot};

use super::backoff::{CrashWindow, restart_delay};
use super::manager::{CommandError, EngineFailure, ManagedPlan, Shared, Status};
use crate::readiness::{UpstreamReason, UpstreamState};

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
    /// 校验通过的监听者（pid 加启动时间）与据此建的守卫：发往这个端点的每个请求，发出之前都让它重新确认监听者还是它们。
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
        self.shared.publish(
            &self.id,
            Status {
                state: UpstreamState::Failed,
                reason: None,
                owned: false,
                terminal: true,
            },
        );
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
    /// 等待期间收到、还没有处理的命令。
    pending: Option<Request>,
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
            pending: None,
        }
    }

    pub(super) async fn run(mut self) {
        let _panic_report = PanicReport {
            shared: self.shared.clone(),
            id: self.plan.id.clone(),
        };
        let mut next = Step::Attempt;
        loop {
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

    fn timings(&self) -> &super::backoff::Timings {
        &self.shared.config.timings
    }

    fn log(&self, text: &str) {
        self.shared
            .log(&format!("[idoris] 引擎 {}：{text}", self.plan.id));
    }

    /// 发布状态（还在往前走，不是终局）。只在状态或原因变化时写一行日志（不刷屏）。
    fn publish(&mut self, state: UpstreamState, reason: Option<UpstreamReason>) {
        self.emit(state, reason, false);
    }

    /// 发布一个终局的状态：这个实例不会自己再往就绪走了，门控把它算作「落定」。
    fn publish_terminal(&mut self, state: UpstreamState, reason: Option<UpstreamReason>) {
        self.emit(state, reason, true);
    }

    /// 门控是在这个实例没有就绪的时候降级放行的：它冻结，不再半开重试，不再接受 `start`（要重启 iDoris）。
    fn frozen(&self) -> bool {
        self.shared.is_frozen(&self.plan.id)
    }

    fn emit(&mut self, state: UpstreamState, reason: Option<UpstreamReason>, terminal: bool) {
        let changed = self.state != state || self.reason != reason;
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
            },
        );
        if changed {
            match reason {
                Some(reason) => self.log(&format!("{}（{}）", state.as_str(), reason.as_str())),
                None => self.log(state.as_str()),
            }
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
                        });
                        return true;
                    };
                    match self.immediate_answer(&request.command) {
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
    fn immediate_answer(&self, command: &Command) -> Option<Result<(), CommandError>> {
        match command {
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
            // 降级放行之后，这个上游不再启动：它的 Supervisor 已经在放行时看过一个失败的引擎（要重启 iDoris）。
            Command::Start | Command::Restart if self.frozen() => {
                Some(Err(CommandError::RestartRequired))
            }
            // 已经停下了：幂等。
            Command::Stop if self.engine.is_none() && self.state == UpstreamState::Stopped => {
                Some(Ok(()))
            }
            // 不是自己拥有的引擎（外部引擎、还在启动、没有引擎）：iDoris 不给别人的进程发信号。
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

    /// 停掉并收尾一个引擎（宽限之后 SIGKILL，删记录），没有期限（照 `stop_grace`）。失败只记日志：认不出的进程不发信号。iDoris 在停时
    /// 的停止不走这里：那是 `Command::Shutdown` 带着期限交给 [`Self::stop_engine_by`]（等就绪被它打断时，引擎也是放回 `self.engine`
    /// 交给那条命令，不在 [`Self::abandon`] 里先停，所以这里不会再遇到「已经有带期限的 `Shutdown` 在等」）。
    async fn stop_engine(&self, owned: Owned) {
        self.stop_engine_by(owned, None).await;
    }

    /// 在 `deadline` 之前停掉并收尾一个引擎：把**绝对期限**交给引擎（`OwnedEngine::stop_until`），宽限与确认窗口由它在真正发 SIGTERM 的那一刻
    /// 按期限里剩下的时间算（[`super::backoff::stop_windows`]：不到 2 秒就 SIGTERM 之后立刻 SIGKILL）；没有期限就照 `stop_grace`。
    /// 认回的孤儿要先过身份闸（读启动时间、进程组，可能很慢）：闸花掉的时间从同一个期限里扣，不是在闸之前就换算成固定时长（H3：那样
    /// 闸慢的时候宽限还是原来那么长，总时间超过期限）。确认不了进程已经消失时所有权记录保留，下一个 iDoris 认回它再收尾
    /// （`OwnedEngine` 的停止只在确认消失之后才删记录）。
    async fn stop_engine_by(&self, owned: Owned, deadline: Option<tokio::time::Instant>) {
        self.plan.backend.block(&self.plan.endpoint);
        let grace = self.timings().stop_grace;
        if let Err(error) = owned.engine.stop_until(grace, deadline).await {
            self.log(&format!("停止引擎失败（{}）", error.reason_code()));
        }
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
        let Request { command, reply } = request;
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
            Command::Start | Command::Restart if self.frozen() => {
                answer(Err(CommandError::RestartRequired));
                current
            }
            Command::Start => {
                answer(Ok(()));
                match self.state {
                    // 已经好了、或正在启动：幂等，什么也不改。
                    UpstreamState::Ready | UpstreamState::Starting | UpstreamState::Restarting => {
                        current
                    }
                    UpstreamState::Stopped
                    | UpstreamState::Failed
                    | UpstreamState::Absent
                    | UpstreamState::Unavailable => {
                        // 操作员显式要求：之前的失败不再计数。
                        self.window.clear();
                        Step::Attempt
                    }
                }
            }
            Command::Stop => {
                if let Some(owned) = self.engine.take() {
                    answer(Ok(()));
                    self.stop_engine(owned).await;
                    self.publish_terminal(UpstreamState::Stopped, None);
                    Step::Idle(Idle::Stopped)
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
                if let Some(owned) = self.engine.take() {
                    answer(Ok(()));
                    self.stop_engine(owned).await;
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
        self.external = None;
        self.plan.backend.block(&self.plan.endpoint);
        // `stop_engine` 为假：让它继续跑（`StopOnExit::Never` 拉起的，句柄丢弃时不杀进程组），所有权记录留给下一个 iDoris。
        if let Some(owned) = self.engine.take()
            && stop_engine
        {
            self.stop_engine_by(owned, deadline).await;
        }
    }

    // -----------------------------------------------------------------------------------------
    // 步骤
    // -----------------------------------------------------------------------------------------

    async fn attempt_cycle(&mut self) -> Step {
        self.publish(UpstreamState::Starting, None);
        match self.attempt().await {
            Attempt::Ready => {
                self.publish(UpstreamState::Ready, None);
                Step::Supervise
            }
            Attempt::Terminal { state, reason } => {
                self.publish_terminal(state, reason);
                Step::Idle(Idle::Terminal)
            }
            Attempt::Failure { reason } => self.fail(reason).await,
            Attempt::Interrupted => Step::Attempt,
        }
    }

    /// 一次运行期失败：记入熔断窗口；超过上限就是 `failed` + `crash_loop`（之后半开重试），否则 `restarting`，
    /// 退避（1、2、4 秒起翻倍，封顶 30 秒，±20%）之后再试。
    async fn fail(&mut self, reason: Option<UpstreamReason>) -> Step {
        let now = Instant::now();
        let failures = self.window.record(now);
        if self.window.tripped(now) {
            self.publish_terminal(UpstreamState::Failed, Some(UpstreamReason::CrashLoop));
            return Step::Idle(Idle::CrashLoop);
        }
        self.publish(UpstreamState::Restarting, reason);
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
        self.engine = None;
        self.external = None;
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
                Ok(Recovery::NoRecord | Recovery::Stale(_)) => break None,
                Ok(Recovery::Adopted(engine)) => {
                    if self.fence_is_present() {
                        // 引擎可能正处于加载中，我们对它无从确认：它是我们的，所以杀掉整组、确认消失、清围栏、重拉。
                        self.log("接管到自己遗留的引擎，但有加载围栏：停掉重拉");
                        if let Err(error) = engine.stop(self.timings().stop_grace).await {
                            self.log(&format!("停止遗留的引擎失败（{}）", error.reason_code()));
                            return Attempt::Failure { reason: None };
                        }
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
                    if let Err(error) = engine.stop(self.timings().stop_grace).await {
                        self.log(&format!("停止遗留的引擎失败（{}）", error.reason_code()));
                        return Attempt::Failure { reason: None };
                    }
                    break None;
                }
                Ok(Recovery::Exiting(engine)) => {
                    // 上一代引擎正在退出：pid、启动时间、进程组、命令都吻合，只是端口上没有它在监听（它收到 SIGTERM 之后关了监听、
                    // 还在等在途的生成结束，而上一个 iDoris 在这期间被 launcher SIGKILL 了）。它是我们的：有界地停掉
                    // （SIGTERM、宽限、SIGKILL），确认消失之后才删记录，再重拉。不能当作记录作废丢掉：那会留下一个没人管的引擎。
                    self.log(
                        "遗留的引擎正在退出（端口上已没有它在监听）：有界地停掉，确认消失之后重拉",
                    );
                    if let Err(error) = engine.stop(self.timings().stop_grace).await {
                        self.log(&format!("停止遗留的引擎失败（{}）", error.reason_code()));
                        return Attempt::Failure { reason: None };
                    }
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

        // 接管了自己遗留的引擎：用记录里的 key（先确认监听端口的是它）等就绪。
        if let Some(mut owned) = adopted {
            return match self.wait_owned_ready(&mut owned).await {
                Ok(()) => self.finish_owned(owned).await,
                Err(failure) => self.abandon(owned, failure).await,
            };
        }

        // 2. 探测 endpoint。
        let probe_recipe = self.plan.backend.probe_recipe();
        match self.probe(&probe_recipe, None).await {
            EngineProbe::ConnectionRefused => {}
            first @ (EngineProbe::Ready | EngineProbe::AuthRejected(_)) => {
                return self.adopt_external(&probe_recipe, first).await;
            }
            EngineProbe::NotThisEngine => {
                self.log("endpoint 上有别的服务（响应不像这种引擎）");
                return Attempt::Terminal {
                    state: UpstreamState::Failed,
                    reason: Some(UpstreamReason::PortInUse),
                };
            }
            EngineProbe::Unavailable => {
                // 有东西在监听但还答不上来（外部引擎正在启动或很忙，或别的什么）：等它，不拉起第二个。
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
                // 探测与校验之间监听者没了：再来一次。
                return Attempt::Failure { reason: None };
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
                    Some(key) => match self.probe(recipe, Some(&key)).await {
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
                        EngineProbe::ConnectionRefused | EngineProbe::Unavailable => false,
                    },
                }
            }
        };
        if !adopted {
            return Attempt::Failure { reason: None };
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
                // 监听者没了：回到决策的开头（可能要拉起）。
                EngineProbe::ConnectionRefused => return Attempt::Failure { reason: None },
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
        // 先构造一次，把配置性的失败（额度、base、模型目录）分清楚，再拉起。
        if let Err(error) = launch_with() {
            self.log(&format!("引擎的启动命令不合规（{}）", error.reason_code()));
            return Attempt::Terminal {
                state: UpstreamState::Failed,
                reason: launch_reason(&error),
            };
        }
        // 一个新拉起的引擎什么都没加载：之前的围栏说的是已经不存在的上一代。清不掉就不拉起（围栏会让每次加载都 fail-closed）。
        if self.clear_fence().is_err() {
            self.log("无法清除加载围栏");
            return Attempt::Terminal {
                state: UpstreamState::Failed,
                reason: Some(UpstreamReason::FencePending),
            };
        }
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
        match self.wait_owned_ready(&mut owned).await {
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
        let mut listener_confirmed = false;
        let mut rejections = 0_u32;
        loop {
            // 读不出来（`Err`）不当作退出，下一轮再看。
            if let Ok(false) = owned.engine.is_running().await {
                return Err(WaitFailure::Exited);
            }
            if !listener_confirmed {
                listener_confirmed = matches!(
                    inspector
                        .listens_on(owned.engine.pid(), owned.engine.port())
                        .await,
                    Ok(true)
                );
            }
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
        match owned.engine.seal().await {
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
        if matches!(failure, WaitFailure::Interrupted) && self.pending_is_shutdown() {
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

    /// 自有引擎的进程退出了。门控完成之前：计入熔断窗口，退避重启。门控完成之后：Supervisor 的账本已经过期，
    /// 进程内恢复不了（v0.6.1），通知 `bin` 以退出码 5 优雅停止。
    async fn owned_engine_exited(&mut self) -> Step {
        if let Some(owned) = self.engine.take() {
            // 已经死了：`stop` 只是收尾（删所有权记录），不会向别的进程发信号。
            self.stop_engine(owned).await;
        }
        // 先发布「不再就绪」，再读 `gate_open`：与 `open_gate`（同一把锁里检查）两者之一必然看到对方。
        self.publish(UpstreamState::Failed, None);
        if self.shared.gate_open.load(Ordering::SeqCst) {
            self.log("自有引擎已退出，进程内无法恢复（热恢复是 v0.6.1）：iDoris 将以退出码 5 停止，由 agent24d 重启");
            self.publish_terminal(UpstreamState::Failed, None);
            self.shared.signal_failure(EngineFailure {
                upstream_id: self.plan.id.clone(),
            });
            return Step::Idle(Idle::Terminal);
        }
        self.log("自有引擎已退出");
        self.fail(None).await
    }

    /// 一个周期的外部引擎监管：探测失败只报 `unavailable`，iDoris 永远不重启它，也不给它发信号。
    async fn supervise_external(&mut self) {
        let Some(external) = &self.external else {
            return;
        };
        let guard = external.guard.clone();
        let backend = self.plan.backend.clone();
        // 每个周期先确认监听者还是校验过的那个（按 pid 加启动时间缓存，进程没换就是两次便宜的读）：之后才可能发 key。
        match external.verifier.check(self.plan.port).await {
            ListenerVerdict::Verified { .. } => {
                backend.govern_external(&self.plan.endpoint, guard);
            }
            ListenerVerdict::NoListener => {
                self.publish(UpstreamState::Unavailable, None);
                return;
            }
            ListenerVerdict::Rejected(_) | ListenerVerdict::Unverifiable(_) => {
                backend.block(&self.plan.endpoint);
                self.publish(UpstreamState::Failed, Some(UpstreamReason::PortInUse));
                return;
            }
        }
        let recipe = backend.probe_recipe();
        let key = backend.current_key(&self.plan.endpoint);
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
}

/// 刚写好的可执行文件偶发 `ETXTBSY` 时 `spawn` 的重试次数与间隔（总共至多约半秒）。
const BUSY_RETRIES: u32 = 50;
const BUSY_PAUSE: Duration = Duration::from_millis(10);

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
    use super::*;

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
