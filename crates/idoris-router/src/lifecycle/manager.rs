//! 引擎管理器：每个托管卡一个实例任务（`instance.rs`），这里是它们共用的东西、对外的接口，以及从组件卡到实例计划的转换。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use idoris_upstream::engine::{ProcessInspector, StopOnExit};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::backend::{EngineBackend, OmlxBackend};
use super::backoff::Timings;
use super::instance::{Actor, Command, Request};
use crate::readiness::{
    HealthState, ManagedFailure, UpstreamPlan, UpstreamReason, UpstreamState, unix_millis_now,
};
use crate::register::LogSink;
use crate::runtime::RuntimeRegistry;

/// 管理器的配置。
pub struct ManagerConfig {
    /// iDoris 的状态目录（绝对路径）：所有权记录、引擎日志、oMLX 自己的 base、加载围栏都在它下面。
    pub state_dir: PathBuf,
    /// `IDORIS_ALLOCATED_BUDGET_BYTES`；`None` = 没设。
    pub allocated_budget_bytes: Option<u64>,
    /// launcher 帧 `engine.stop_on_exit`。
    pub stop_on_exit: StopOnExit,
    /// launcher 帧 `engine.executables`：引擎种类到可执行文件（launcher 已校验，视为可信）。
    pub executables: BTreeMap<String, PathBuf>,
    /// 这个 iDoris 进程的标识（`/health.instance_id`），写进所有权记录，只用于排查。
    pub launched_by: String,
    pub timings: Timings,
    /// 读进程身份的检查器：生产是 `SystemInspector`，测试可以换假的。
    pub inspector: Arc<dyn ProcessInspector>,
    pub log: LogSink,
}

/// 一个托管卡对应的实例：哪张卡、哪个端口、哪种引擎。
#[derive(Clone)]
pub struct ManagedPlan {
    /// 卡的 `provider.id`，也是 `/health.upstreams[].id`。
    pub id: String,
    /// `http://127.0.0.1:<port>`。
    pub endpoint: String,
    pub port: u16,
    pub backend: Arc<dyn EngineBackend>,
}

impl std::fmt::Debug for ManagedPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedPlan")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// 卡不能交给管理器（启动失败：卡校验没过）。文案只含卡 id 与引擎种类名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// 托管卡的 endpoint 必须是 `http://127.0.0.1:<端口>`：引擎只监听 127.0.0.1，端口取自 endpoint。
    EndpointNotManageable { id: String },
    /// 本版本没有这种引擎的管理器。
    NoBackend { id: String, engine_kind: String },
    /// 两张托管卡用了同一个端口。
    PortShared { id: String, other: String },
    /// 同一种引擎有两张托管卡：它们会共用同一个私有 base 目录（oMLX 把命令行设置落盘到 base，共用会互相覆盖，还共用缓存与日志）。
    /// v0.6.0 每种引擎只托管一个实例（同种类的多个实例是预留，规范 §2.12.2）。
    SecondOfKind {
        id: String,
        other: String,
        engine_kind: String,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointNotManageable { id } => write!(
                f,
                "托管卡 \"{id}\" 的 endpoint 必须是 http://127.0.0.1:<端口>（不含路径、凭据、查询与片段）：引擎只监听 127.0.0.1，端口取自 endpoint"
            ),
            Self::NoBackend { id, engine_kind } => write!(
                f,
                "托管卡 \"{id}\" 的引擎种类 \"{engine_kind}\" 在本版本没有引擎管理器"
            ),
            Self::PortShared { id, other } => {
                write!(
                    f,
                    "托管卡 \"{id}\" 与 \"{other}\" 的 endpoint 用了同一个端口"
                )
            }
            Self::SecondOfKind {
                id,
                other,
                engine_kind,
            } => write!(
                f,
                "托管卡 \"{id}\" 与 \"{other}\" 都是 {engine_kind} 引擎：本版本每种引擎只托管一个实例（它们会共用同一个私有目录）"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// 从启动时读到的上游计划里取出托管的那些，生产的引擎种类到 [`EngineBackend`] 的对应（v0.6.0 只有 oMLX）。
pub fn plan_managed(upstreams: &[UpstreamPlan]) -> Result<Vec<ManagedPlan>, PlanError> {
    plan_managed_with(upstreams, |kind| match kind {
        "omlx" => Some(Arc::new(OmlxBackend::from_env()) as Arc<dyn EngineBackend>),
        _ => None,
    })
}

/// 同上，引擎种类到 backend 的对应由调用方给（测试里换假的 backend）。
pub fn plan_managed_with(
    upstreams: &[UpstreamPlan],
    backend_for: impl Fn(&str) -> Option<Arc<dyn EngineBackend>>,
) -> Result<Vec<ManagedPlan>, PlanError> {
    let mut plans: Vec<ManagedPlan> = Vec::new();
    for upstream in upstreams.iter().filter(|plan| plan.identity.managed) {
        let id = upstream.id.clone();
        let port = managed_port(&upstream.card().endpoint)
            .ok_or_else(|| PlanError::EndpointNotManageable { id: id.clone() })?;
        let backend =
            backend_for(&upstream.identity.engine_kind).ok_or_else(|| PlanError::NoBackend {
                id: id.clone(),
                engine_kind: upstream.identity.engine_kind.clone(),
            })?;
        if let Some(other) = plans.iter().find(|plan| plan.port == port) {
            return Err(PlanError::PortShared {
                id,
                other: other.id.clone(),
            });
        }
        if let Some(other) = plans
            .iter()
            .find(|plan| plan.backend.kind() == backend.kind())
        {
            return Err(PlanError::SecondOfKind {
                id,
                other: other.id.clone(),
                engine_kind: upstream.identity.engine_kind.clone(),
            });
        }
        plans.push(ManagedPlan {
            id,
            endpoint: format!("http://127.0.0.1:{port}"),
            port,
            backend,
        });
    }
    Ok(plans)
}

/// `http://127.0.0.1:<port>`（端口显式给出、非 0；路径只能是空或 `/`；没有凭据、查询、片段）里的端口。
fn managed_port(endpoint: &str) -> Option<u16> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    let manageable = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    manageable
        .then(|| url.port())
        .flatten()
        .filter(|port| *port != 0)
}

/// 门控完成之后，一个自有引擎失效、进程内无法恢复（v0.6.0：热恢复是 v0.6.1）。iDoris 因此优雅停止，退出码 5。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFailure {
    /// 失效的引擎所属的上游 id。
    pub upstream_id: String,
}

/// 对一个上游下命令的结果（Admin 的 `start`/`stop`/`restart` 用它，S6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// 没有这个上游。
    NotFound,
    /// 不是 iDoris 自己拥有的引擎：iDoris 不给别人的进程发信号。
    NotOwned,
    /// 实例任务已经结束（管理器在停止）。
    Closed,
    /// 这个上游在门控放行时没有就绪（降级放行）：完整的 Router 里根本没有它的 Supervisor，之后即使引擎好了也读不到（给它建
    /// Supervisor 要重建 Router，那是运行期引擎换代之外的事，S5 起就是已知限制）。放行时就绪的上游不冻结：引擎换代（`restart`、
    /// `stop` 之后的 `start`）时它们的 Supervisor 复位（[`super::instance`] 的 `sync_supervisor`）。要重启 iDoris。
    RestartRequired,
}

impl CommandError {
    /// 稳定的短名字，给日志与测试用（Admin 的错误码另在 `admin` 里定）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotOwned => "not_owned",
            Self::Closed => "closed",
            Self::RestartRequired => "restart_required",
        }
    }
}

/// 状态从 `previous` 变到 `next` 是不是一次新的失败：进入 `restarting` 或 `failed` 才算，而且是从不是这两个之一的状态进入（
/// `restarting` 之后又 `failed`、`failed` 之后又发布一次终局的 `failed`，是同一次失败）。`unavailable` 不算：它可能是引擎忙，
/// 探测恢复就回到 `ready`。
pub(super) fn counts_as_failure(previous: UpstreamState, next: UpstreamState) -> bool {
    let failing = |state| matches!(state, UpstreamState::Restarting | UpstreamState::Failed);
    failing(next) && !failing(previous)
}

/// Admin 看到的一个托管实例：只读快照，**不含任何 key、token、nonce**（`GET /admin/api/v1/upstreams`，S6）。
/// `state`、`reason`、`owned` 与 `/health.upstreams[]` 里同一个实例的值同源：管理器发布状态时在同一把锁里同时写这里和
/// `/health` 的快照，所以这个快照自己是一致的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedView {
    pub state: UpstreamState,
    pub reason: Option<UpstreamReason>,
    pub owned: bool,
    /// 卡的 endpoint 里的 loopback 端口。
    pub port: u16,
    /// 自有引擎的进程号（`owned` 为真才有）。
    pub pid: Option<u32>,
    /// 只有重启 iDoris 才能让这个上游回来：它在门控放行时没就绪（降级放行，冻结），完整的 Router 里没有它的 Supervisor
    /// （[`CommandError::RestartRequired`]）。
    pub restart_required: bool,
    /// iDoris 启动以来这个实例失败过几次：状态进入 `restarting` 或 `failed` 的次数。
    pub failure_count: u32,
    /// 最近一次失败的墙钟时间（Unix 毫秒）；没失败过是 `None`。
    pub last_failure_at_ms: Option<u64>,
}

/// 门控放行的结果：放行时没有就绪的托管上游（降级放行）。空 = 全部就绪。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateOutcome {
    /// 上游 id → 放行时的状态与原因。
    pub degraded: BTreeMap<String, ManagedFailure>,
}

/// 一个实例当前的状态（管理器内部，也是门控判据的来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Status {
    pub state: UpstreamState,
    pub reason: Option<UpstreamReason>,
    pub owned: bool,
    /// 终局：这个实例不会自己再往就绪走了（`Idle::Terminal`、熔断、被 `stop`、被放弃）。和 `state` 分开记：
    /// `owned_engine_exited` 在重启之前会短暂发布一次 `failed`，那不是终局，门控不能把它当成「落定」。
    pub terminal: bool,
    /// 自有引擎的进程号（`owned` 为真才有）：Admin 的只读细节（[`ManagedView::pid`]）。
    pub pid: Option<u32>,
    /// 状态进入 `restarting` 或 `failed` 的次数（[`ManagedView::failure_count`]）。
    pub failures: u32,
    /// 最近一次失败的墙钟时间（Unix 毫秒）。
    pub last_failure_at_ms: Option<u64>,
}

impl Status {
    /// 落定：就绪，或终局。门控等所有托管实例都落定才放行（接口规范 §2.7.6，勘误六）。
    pub fn is_settled(&self) -> bool {
        self.state == UpstreamState::Ready || self.terminal
    }
}

/// 各实例任务共用的东西。
pub(super) struct Shared {
    pub config: ManagerConfig,
    pub health: HealthState,
    /// 完整的 Router 已经按「所有引擎落定」的判据放行（[`EngineManager::open_gate`]）：之后（放行时已经就绪的）自有引擎的进程退出
    /// 无法在进程内恢复，是退出码 5。
    pub gate_open: AtomicBool,
    statuses: Mutex<BTreeMap<String, Status>>,
    /// 上游 id → 引擎种类（`GateOutcome` 要带）。
    kinds: BTreeMap<String, String>,
    /// 上游 id → 卡的 endpoint 里的 loopback 端口（`ManagedView` 要带）。
    ports: BTreeMap<String, u16>,
    /// 上游 id → 已经入队的命令的序号（从 1 起）：[`EngineManager::command`] 在**入队时**盖章，`Actor::restart_cover` 据此把
    /// 一次 `restart` 进行期间到达的 `restart` 并进它的结果里（IDORIS-CORE S6，Codex 预审 5）。
    enqueued: BTreeMap<String, AtomicU64>,
    /// 门控放行时没有就绪的实例，只在 [`EngineManager::open_gate`] 里写一次（持有 `statuses` 的锁时写，所以与状态一致）。
    /// 这些实例「冻结」：不再半开重试，不再接受 `start`（见 [`CommandError::RestartRequired`]）。
    degraded: Mutex<BTreeMap<String, ManagedFailure>>,
    /// 管理器启动的时刻：门控最晚等到这之后一个启动超时（[`EngineManager::gate_deadline`]）。
    started: Instant,
    changed: watch::Sender<u64>,
    failure: watch::Sender<Option<EngineFailure>>,
    /// 完整的 Router 创建之后挂上来的 Supervisor 表：门控完成之后，Supervisor 的启动错误粘滞要反映到 `/health`。
    pub runtimes: OnceLock<RuntimeRegistry>,
}

impl Shared {
    pub fn log(&self, line: &str) {
        (self.config.log)(line);
    }

    /// 发布一个实例的状态：管理器自己的表（门控判据）与 `/health` 的快照一起更新，在同一把锁里，所以
    /// [`EngineManager::open_gate`] 看到的与 `/health` 说的一致。
    pub fn publish(&self, id: &str, status: Status) {
        self.publish_with(id, |_| status);
    }

    /// 同 [`Self::publish`]，新的状态由旧的（没有就是 `None`）算出来，读旧值与写新值在同一把锁里。
    pub fn publish_with(&self, id: &str, next: impl FnOnce(Option<&Status>) -> Status) {
        let mut statuses = self.statuses.lock().unwrap_or_else(PoisonError::into_inner);
        let status = next(statuses.get(id));
        statuses.insert(id.to_owned(), status);
        self.health
            .set_managed(id, status.state, status.reason, status.owned);
        drop(statuses);
        self.changed.send_modify(|version| *version += 1);
    }

    /// 实例任务 panic：按引擎失效发布成终局的 `failed`（失败计数沿用，再记一次）。
    pub fn publish_panicked(&self, id: &str) {
        self.publish_with(id, |previous| {
            let (failures, last_failure_at_ms) = previous.map_or((0, None), |status| {
                (status.failures, status.last_failure_at_ms)
            });
            let counted = previous
                .is_none_or(|status| counts_as_failure(status.state, UpstreamState::Failed));
            Status {
                state: UpstreamState::Failed,
                reason: None,
                owned: false,
                terminal: true,
                pid: None,
                failures: if counted {
                    failures.saturating_add(1)
                } else {
                    failures
                },
                last_failure_at_ms: if counted {
                    Some(unix_millis_now())
                } else {
                    last_failure_at_ms
                },
            }
        });
    }

    /// 每个实例都落定了（就绪，或终局）。
    fn all_settled(statuses: &BTreeMap<String, Status>) -> bool {
        statuses.values().all(Status::is_settled)
    }

    /// 这个实例在门控放行时没有就绪：冻结（不再半开重试、不再接受 `start`）。
    pub fn is_frozen(&self, id: &str) -> bool {
        self.degraded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(id)
    }

    /// 给下一个入队的命令盖章：这个上游已入队命令的序号加一。
    pub fn next_seq(&self, id: &str) -> u64 {
        self.enqueued
            .get(id)
            .map_or(0, |counter| counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// 此刻已盖出的最大序号：序号不大于它的命令都是在这一刻之前入队的。
    pub fn seq_now(&self, id: &str) -> u64 {
        self.enqueued
            .get(id)
            .map_or(0, |counter| counter.load(Ordering::SeqCst))
    }

    /// 自有引擎失效：通知 `bin` 去走退出码 5 的停止路径。只记第一个。
    pub fn signal_failure(&self, failure: EngineFailure) {
        self.failure.send_if_modified(|current| {
            let first = current.is_none();
            if first {
                *current = Some(failure);
            }
            first
        });
    }
}

/// 引擎管理器。
pub struct EngineManager {
    shared: Arc<Shared>,
    senders: BTreeMap<String, mpsc::Sender<Request>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl EngineManager {
    /// 启动每个托管卡的实例任务（必须在 tokio 运行时里调用）：`/health` 的门控设为 [`GateState::Pending`]，各实例开始走
    /// 启动决策。`plans` 为空时没有任何任务，门控仍视为 [`GateState::NotGated`]（调用方不该在没有托管卡时创建管理器）。
    ///
    /// [`GateState::Pending`]: crate::readiness::GateState::Pending
    /// [`GateState::NotGated`]: crate::readiness::GateState::NotGated
    pub fn start(config: ManagerConfig, plans: Vec<ManagedPlan>, health: HealthState) -> Self {
        let (changed, _) = watch::channel(0_u64);
        let (failure, _) = watch::channel(None);
        let kinds = plans
            .iter()
            .map(|plan| (plan.id.clone(), plan.backend.kind().as_str().to_owned()))
            .collect();
        let ports = plans
            .iter()
            .map(|plan| (plan.id.clone(), plan.port))
            .collect();
        let enqueued = plans
            .iter()
            .map(|plan| (plan.id.clone(), AtomicU64::new(0)))
            .collect();
        let shared = Arc::new(Shared {
            config,
            health,
            gate_open: AtomicBool::new(false),
            statuses: Mutex::new(BTreeMap::new()),
            kinds,
            ports,
            enqueued,
            degraded: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            changed,
            failure,
            runtimes: OnceLock::new(),
        });
        if !plans.is_empty() {
            shared.health.set_gate(crate::readiness::GateState::Pending);
        }
        let mut senders = BTreeMap::new();
        let mut tasks = Vec::new();
        for plan in plans {
            // 托管的端点从一开始就封上：在引擎管理器登记出一个通过校验的监听者之前，没有任何凭证发往它，环境变量与
            // `settings.json` 的回退链从不适用于托管的端点（IDORIS-CORE S6，Codex 预审 3）。
            plan.backend.block(&plan.endpoint);
            let (sender, receiver) = mpsc::channel(8);
            // 先登记 `starting`：门控判据与 `/health` 一开始就有这个实例。
            shared.publish(
                &plan.id,
                Status {
                    state: UpstreamState::Starting,
                    reason: None,
                    owned: false,
                    terminal: false,
                    pid: None,
                    failures: 0,
                    last_failure_at_ms: None,
                },
            );
            senders.insert(plan.id.clone(), sender);
            tasks.push(tokio::spawn(
                Actor::new(plan, shared.clone(), receiver).run(),
            ));
        }
        Self {
            shared,
            senders,
            tasks: Mutex::new(tasks),
        }
    }

    /// 所有托管上游的 id。
    pub fn upstream_ids(&self) -> Vec<String> {
        self.senders.keys().cloned().collect()
    }

    /// 等到每个实例都「落定」（同一时刻）：`ready`，或不会自己好的终局（配置性失败、`engine_not_found`、熔断……）。
    /// 还在 `starting`、`restarting` 的不算。已经落定了就立刻返回。有实例一直不落定（引擎既不好也不彻底失败）时
    /// 不会返回：调用方用 [`Self::gate_deadline`] 给它一个上限，到点用 [`Self::give_up_unsettled`] 放弃。
    pub async fn wait_all_settled(&self) {
        let mut changed = self.shared.changed.subscribe();
        loop {
            {
                let statuses = self
                    .shared
                    .statuses
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if Shared::all_settled(&statuses) {
                    return;
                }
            }
            if changed.changed().await.is_err() {
                // 管理器不再有写者：永远不会再变。
                std::future::pending::<()>().await;
            }
        }
    }

    /// 门控最晚等到这个时刻：管理器启动之后一个启动超时（oMLX 缺省 90 秒，`Timings::startup_timeout`）。
    /// 到点还有实例没落定，调用方 [`Self::give_up_unsettled`] 之后放行（降级）：门控的等待有界。
    pub fn gate_deadline(&self) -> Instant {
        self.shared.started + self.shared.config.timings.startup_timeout
    }

    /// 等所有实例落定，**最多等到 [`Self::gate_deadline`]**；到点还没落定的放弃（[`Self::give_up_unsettled`]）。返回时每个
    /// 实例都落定了，除非有实例任务已经没了（管理器在停止）：调用方再用 [`Self::open_gate`] 确认。
    pub async fn settle(&self) {
        let _ = tokio::time::timeout_at(self.gate_deadline(), self.wait_all_settled()).await;
        self.give_up_unsettled().await;
    }

    /// 等到可以放行并放行：所有实例落定（[`Self::settle`]：最多等到 [`Self::gate_deadline`]，到点还没落定的放弃），再在同一把锁里
    /// 确认并放行（[`Self::open_gate`]）。确认时又有实例不落定了（一个就绪的引擎在这之间又失效了，或者放弃时有实例没答）：
    /// 等下一次状态变化再来，不空转；下一轮仍受**同一个**截止时间约束，所以整个等待有界（实例任务已经没了的除外：管理器在停止）。
    pub async fn open_when_settled(&self) -> GateOutcome {
        loop {
            // 先订阅、后判断：之后的任何一次状态变化都不会漏掉。
            let mut changes = self.shared.changed.subscribe();
            self.settle().await;
            if let Some(outcome) = self.open_gate() {
                return outcome;
            }
            if changes.changed().await.is_err() {
                // 管理器不再有写者：永远不会再变。
                std::future::pending::<()>().await;
            }
        }
    }

    /// 门控放行：**在同一把锁里**确认所有实例仍然落定（[`Self::wait_all_settled`] 返回之后可能又变了：例如一个就绪的
    /// 引擎又失效了），是就把 `gate_open` 置位并返回放行的结果；不是返回 `None`，调用方回去再等。
    ///
    /// 放行时没有就绪的实例（终局的失败）进 [`GateOutcome::degraded`]，并从这一刻起**冻结**：不再半开重试，不再接受
    /// `start`，要重启 iDoris（它们的 Supervisor 在放行时创建，看到的是一个失败的引擎）。置位之后，已经就绪的自有
    /// 引擎的进程退出就是退出码 5（[`Self::engine_failed`]）；冻结的实例没有进程，不会触发它。
    pub fn open_gate(&self) -> Option<GateOutcome> {
        let statuses = self
            .shared
            .statuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !Shared::all_settled(&statuses) {
            return None;
        }
        let degraded: BTreeMap<String, ManagedFailure> = statuses
            .iter()
            .filter(|(_, status)| status.state != UpstreamState::Ready)
            .map(|(id, status)| {
                let failure = ManagedFailure {
                    engine_kind: self.shared.kinds.get(id).cloned().unwrap_or_default(),
                    state: status.state,
                    reason: status.reason,
                };
                (id.clone(), failure)
            })
            .collect();
        *self
            .shared
            .degraded
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = degraded.clone();
        self.shared.gate_open.store(true, Ordering::SeqCst);
        Some(GateOutcome { degraded })
    }

    /// 门控等不下去了（到了 [`Self::gate_deadline`]）：还没落定的实例放弃：停掉半拉起的引擎，报 `failed` +
    /// `startup_timeout`，不再重试。等它们回答（至多 `stop_grace` 加一点余量）。已经落定的不动。
    pub async fn give_up_unsettled(&self) {
        let unsettled: Vec<String> = {
            let statuses = self
                .shared
                .statuses
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            statuses
                .iter()
                .filter(|(_, status)| !status.is_settled())
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut waits = Vec::new();
        for id in unsettled {
            let Some(sender) = self.senders.get(&id) else {
                continue;
            };
            let (reply, answer) = oneshot::channel();
            let request = Request {
                command: Command::GiveUp,
                reply: Some(reply),
                seq: 0,
            };
            if sender.send(request).await.is_ok() {
                waits.push(answer);
            }
        }
        let budget = self.shared.config.timings.stop_grace + Duration::from_secs(3);
        let _ = tokio::time::timeout(budget, async {
            for answer in waits {
                let _ = answer.await;
            }
        })
        .await;
    }

    pub fn gate_is_open(&self) -> bool {
        self.shared.gate_open.load(Ordering::SeqCst)
    }

    /// 完整的 Router 创建之后，把 Supervisor 表交给管理器：此后对 `ready` 的托管引擎，每个周期问一次 Supervisor
    /// （200 毫秒），启动错误粘滞要报 `unavailable` + `startup_latched`，不能因为引擎本身答得上来就报 `ready`。
    pub fn attach_runtimes(&self, runtimes: RuntimeRegistry) {
        let _ = self.shared.runtimes.set(runtimes);
    }

    /// 门控完成之后，有自有引擎失效（进程退出），进程内恢复不了时返回。之前不会返回；永远没有时也不会返回，
    /// 所以可以直接放进 `select!` 与信号并列。
    pub async fn engine_failed(&self) -> EngineFailure {
        let mut failure = self.shared.failure.subscribe();
        loop {
            if let Some(found) = failure.borrow_and_update().clone() {
                return found;
            }
            if failure.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn command(&self, id: &str, command: Command) -> Result<(), CommandError> {
        let sender = self.senders.get(id).ok_or(CommandError::NotFound)?;
        let (reply, answer) = oneshot::channel();
        // 入队时盖章（先盖章、后入队）：序号不大于某次操作完成时的 `seq_now` 的 `restart`，都是在那次操作进行期间（或之前）到达的，
        // 它们并进那次操作的结果，不再换第二次引擎（见 `Actor::restart_cover`）。
        let seq = self.shared.next_seq(id);
        sender
            .send(Request {
                command,
                reply: Some(reply),
                seq,
            })
            .await
            .map_err(|_| CommandError::Closed)?;
        answer.await.map_err(|_| CommandError::Closed)?
    }

    /// 下命令并留一行日志（谁的、什么命令、结果；没有 token、key：只有上游 id 与命令名）。不认识的 id 不进日志：它来自调用方，
    /// 不能让它往日志里写东西。
    async fn command_logged(
        &self,
        id: &str,
        name: &str,
        command: Command,
    ) -> Result<(), CommandError> {
        let result = self.command(id, command).await;
        if result != Err(CommandError::NotFound) {
            let outcome = match result {
                Ok(()) => "已受理".to_owned(),
                Err(error) => format!("拒绝（{}）", error.as_str()),
            };
            self.shared
                .log(&format!("[idoris] 引擎 {id}：收到命令 {name}，{outcome}"));
        }
        result
    }

    /// 从 `stopped`、`failed`、`absent`、`unavailable` 重新走启动决策；已经 `ready`、或正在启动时幂等。一个还活着的自有引擎
    /// （`unavailable`：活着却不答）先走完整的停止路径（优雅停止、撤销私有 key），不是丢弃句柄（G3）。门控放行之后引擎换了一个
    /// 进程，Supervisor 的账本在发布 `ready` 之前复位（S6）；放行时没就绪而冻结的上游 [`CommandError::RestartRequired`]。
    pub async fn start_upstream(&self, id: &str) -> Result<(), CommandError> {
        self.command_logged(id, "start", Command::Start).await
    }

    /// 停止 iDoris 自己拥有的引擎，状态 `stopped`；不是自己的（外部引擎、没有引擎）一律 [`CommandError::NotOwned`]，
    /// 已经 `stopped` 时幂等。命令被接受时就回答，引擎停下之后状态才变成 `stopped`（至多 `stop_grace` 加一点余量）：调用方轮询
    /// `/health` 或 Admin。进程确认消失之后，私有 key 才撤销（G2），Supervisor 的账本也清空（引擎已经没有了）。
    pub async fn stop_upstream(&self, id: &str) -> Result<(), CommandError> {
        self.command_logged(id, "stop", Command::Stop).await
    }

    /// `stop` 之后重新走启动决策；不是自己拥有的引擎同样 [`CommandError::NotOwned`]。门控放行之后新引擎就绪时，Supervisor 的
    /// 账本在发布 `ready` 之前对着新进程重建（S6）；放行时没就绪而冻结的上游 [`CommandError::RestartRequired`]。
    pub async fn restart_upstream(&self, id: &str) -> Result<(), CommandError> {
        self.command_logged(id, "restart", Command::Restart).await
    }

    /// 每个托管上游此刻的只读快照（`GET /admin/api/v1/upstreams`，S6）。在管理器自己的状态表的锁里一次读完，所以每个实例的
    /// 各个字段彼此一致；冻结的名单只在 [`Self::open_gate`] 里同一把锁下写一次，所以 `restart_required` 与状态一致。
    pub fn views(&self) -> BTreeMap<String, ManagedView> {
        let statuses = self
            .shared
            .statuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        statuses
            .iter()
            .filter_map(|(id, status)| {
                let port = *self.shared.ports.get(id)?;
                Some((
                    id.clone(),
                    ManagedView {
                        state: status.state,
                        reason: status.reason,
                        owned: status.owned,
                        port,
                        pid: status.pid,
                        restart_required: self.shared.is_frozen(id),
                        failure_count: status.failures,
                        last_failure_at_ms: status.last_failure_at_ms,
                    },
                ))
            })
            .collect()
    }

    /// iDoris 停止时调用：各实例收尾。`stop_engines` 为真就停掉自己拉起的引擎（SIGTERM 整组，宽限之后 SIGKILL，
    /// 删记录）；为假（`stop_on_exit:"never"` 的正常退出）就让引擎继续跑，所有权记录留给下一个 iDoris。
    /// 等各实例完成，至多 `stop_grace` 加一点余量；超时的实例任务被中止（`OwnedEngine` 的 `Drop` 仍会兜底）。
    pub async fn shutdown(&self, stop_engines: bool) {
        let mut waits = Vec::new();
        for sender in self.senders.values() {
            let (reply, answer) = oneshot::channel();
            let request = Request {
                command: Command::Shutdown {
                    stop_engine: stop_engines,
                },
                reply: Some(reply),
                seq: 0,
            };
            if sender.send(request).await.is_ok() {
                waits.push(answer);
            }
        }
        let budget = self.shared.config.timings.stop_grace + Duration::from_secs(3);
        let finished = tokio::time::timeout(budget, async {
            for answer in waits {
                let _ = answer.await;
            }
        })
        .await;
        let tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            if finished.is_err() {
                task.abort();
            }
            let _ = task.await;
        }
    }
}

impl Drop for EngineManager {
    fn drop(&mut self) {
        // 没有走 `shutdown` 就丢弃：任务随着通道关闭自行结束（见 `Actor::wait`）；这里不阻塞。
        self.senders.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn status(state: UpstreamState, terminal: bool) -> Status {
        Status {
            state,
            reason: None,
            owned: false,
            terminal,
            pid: None,
            failures: 0,
            last_failure_at_ms: None,
        }
    }

    #[test]
    fn only_entering_restarting_or_failed_from_something_else_is_a_new_failure() {
        use UpstreamState::*;
        let all = [
            Ready,
            Starting,
            Unavailable,
            Restarting,
            Failed,
            Stopped,
            Absent,
        ];
        for previous in all {
            for next in all {
                let expected =
                    matches!(next, Restarting | Failed) && !matches!(previous, Restarting | Failed);
                assert_eq!(
                    counts_as_failure(previous, next),
                    expected,
                    "{previous:?} -> {next:?}"
                );
            }
        }
        // The two ways a crash is published back to back are one failure, not two.
        assert!(counts_as_failure(Ready, Failed));
        assert!(!counts_as_failure(Failed, Restarting));
        assert!(!counts_as_failure(Failed, Failed));
        // `unavailable` flaps with the probe: never a failure.
        assert!(!counts_as_failure(Ready, Unavailable));
    }

    #[test]
    fn command_errors_have_stable_short_names() {
        for (error, name) in [
            (CommandError::NotFound, "not_found"),
            (CommandError::NotOwned, "not_owned"),
            (CommandError::Closed, "closed"),
            (CommandError::RestartRequired, "restart_required"),
        ] {
            assert_eq!(error.as_str(), name);
        }
    }

    fn table(list: &[Status]) -> BTreeMap<String, Status> {
        list.iter()
            .enumerate()
            .map(|(index, status)| (format!("engine-{index}"), *status))
            .collect()
    }

    #[test]
    fn the_gate_waits_for_every_instance_to_be_ready_or_terminal() {
        let ready = status(UpstreamState::Ready, false);
        let terminal = status(UpstreamState::Failed, true);
        // 一个落定、另一个还在往前走：不放行（只要有一个还没有结论，Supervisor 就不能创建）。
        for pending in [
            status(UpstreamState::Starting, false),
            status(UpstreamState::Restarting, false),
            // `owned_engine_exited` 在重启之前短暂发布的 `failed`：不是终局。
            status(UpstreamState::Failed, false),
            status(UpstreamState::Unavailable, false),
        ] {
            assert!(!Shared::all_settled(&table(&[pending])), "{pending:?}");
            assert!(
                !Shared::all_settled(&table(&[ready, pending])),
                "{pending:?}"
            );
            assert!(
                !Shared::all_settled(&table(&[terminal, pending])),
                "{pending:?}"
            );
        }
        // 就绪或终局的都落定了，不管几个。
        assert!(Shared::all_settled(&table(&[ready])));
        assert!(Shared::all_settled(&table(&[terminal])));
        assert!(Shared::all_settled(&table(&[ready, terminal, ready])));
    }

    #[test]
    fn only_a_loopback_endpoint_with_an_explicit_port_can_be_managed() {
        for (endpoint, expected) in [
            ("http://127.0.0.1:19731", Some(19731)),
            ("http://127.0.0.1:19731/", Some(19731)),
            ("http://127.0.0.1:65535", Some(65535)),
            ("http://127.0.0.1:1", Some(1)),
            // Not manageable: the engine listens on 127.0.0.1 only and the port comes from the card.
            ("http://127.0.0.1", None),
            ("http://127.0.0.1:0", None),
            ("http://localhost:19731", None),
            ("http://[::1]:19731", None),
            ("http://127.0.0.2:19731", None),
            ("https://127.0.0.1:19731", None),
            ("http://127.0.0.1:19731/v1", None),
            ("http://127.0.0.1:19731/?x=1", None),
            ("http://127.0.0.1:19731/#f", None),
            ("http://user@127.0.0.1:19731", None),
            ("http://user:pw@127.0.0.1:19731", None),
            ("http://203.0.113.7:19731", None),
            ("not a url", None),
            ("", None),
        ] {
            assert_eq!(managed_port(endpoint), expected, "{endpoint:?}");
        }
    }
}
