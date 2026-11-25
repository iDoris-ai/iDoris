//! 引擎管理器：每个托管卡一个实例任务（`instance.rs`），这里是它们共用的东西、对外的接口，以及从组件卡到实例计划的转换。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use idoris_upstream::engine::{EngineKind, ProcessInspector, StopOnExit};
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
    plan_managed_with(upstreams, production_backend)
}

/// 生产的引擎种类到 backend 的对应：种类的认定只在 [`managed_backend_kind`] 一处，这里按它给出的种类构造 backend。
fn production_backend(engine_kind: &str) -> Option<Arc<dyn EngineBackend>> {
    match managed_backend_kind(engine_kind) {
        Some(kind) if kind == EngineKind::OMLX => {
            Some(Arc::new(OmlxBackend::from_env()) as Arc<dyn EngineBackend>)
        }
        _ => None,
    }
}

/// 本版本能托管的引擎种类到 backend 种类的对应（v0.6.0 只有 oMLX）。纯函数：不构造 backend、不碰环境。
/// `plan_managed` 与 `setup` 的落盘前校验（[`crate::preflight`]）共用它。
pub fn managed_backend_kind(engine_kind: &str) -> Option<EngineKind> {
    match engine_kind {
        "omlx" => Some(EngineKind::OMLX),
        _ => None,
    }
}

/// 托管卡的**纯配置校验**（不构造 backend、不读环境、不碰文件系统）：endpoint 形状、引擎种类有对应的管理器、端口不重复、
/// 每种引擎只有一个托管实例。`plan_managed_with`（serve 启动）与 `setup` 在落盘前对暂存目录做的校验用的是同一个函数，
/// 所以 setup 放行的目录，serve 不会再因为这些原因退出 1。返回按卡顺序排列的 `(上游 id, 端口)`。
pub fn validate_managed_plan(
    upstreams: &[UpstreamPlan],
    backend_kind_for: impl Fn(&str) -> Option<EngineKind>,
) -> Result<Vec<(String, u16)>, PlanError> {
    let mut seen: Vec<(String, u16, EngineKind)> = Vec::new();
    for upstream in upstreams.iter().filter(|plan| plan.identity.managed) {
        let id = upstream.id.clone();
        let port = managed_port(&upstream.card().endpoint)
            .ok_or_else(|| PlanError::EndpointNotManageable { id: id.clone() })?;
        let kind = backend_kind_for(&upstream.identity.engine_kind).ok_or_else(|| {
            PlanError::NoBackend {
                id: id.clone(),
                engine_kind: upstream.identity.engine_kind.clone(),
            }
        })?;
        if let Some((other, _, _)) = seen.iter().find(|(_, seen_port, _)| *seen_port == port) {
            return Err(PlanError::PortShared {
                id,
                other: other.clone(),
            });
        }
        if let Some((other, _, _)) = seen.iter().find(|(_, _, seen_kind)| *seen_kind == kind) {
            return Err(PlanError::SecondOfKind {
                id,
                other: other.clone(),
                engine_kind: upstream.identity.engine_kind.clone(),
            });
        }
        seen.push((id, port, kind));
    }
    Ok(seen.into_iter().map(|(id, port, _)| (id, port)).collect())
}

/// 同上，引擎种类到 backend 的对应由调用方给（测试里换假的 backend）。每张托管卡只解析一次 backend：先解析，再对解析出的
/// 种类做 [`validate_managed_plan`]，保留下来的就是校验过的那一个。
pub fn plan_managed_with(
    upstreams: &[UpstreamPlan],
    backend_for: impl Fn(&str) -> Option<Arc<dyn EngineBackend>>,
) -> Result<Vec<ManagedPlan>, PlanError> {
    let resolved: Vec<(&UpstreamPlan, Option<Arc<dyn EngineBackend>>)> = upstreams
        .iter()
        .filter(|plan| plan.identity.managed)
        .map(|plan| (plan, backend_for(&plan.identity.engine_kind)))
        .collect();
    let kinds: BTreeMap<&str, EngineKind> = resolved
        .iter()
        .filter_map(|(plan, backend)| {
            backend
                .as_ref()
                .map(|backend| (plan.identity.engine_kind.as_str(), backend.kind()))
        })
        .collect();
    let checked = validate_managed_plan(upstreams, |kind| kinds.get(kind).cloned())?;
    let mut plans: Vec<ManagedPlan> = Vec::with_capacity(checked.len());
    for ((upstream, backend), (id, port)) in resolved.into_iter().zip(checked) {
        let backend = backend.ok_or_else(|| PlanError::NoBackend {
            id: id.clone(),
            engine_kind: upstream.identity.engine_kind.clone(),
        })?;
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
    /// `stop` 之后的 `start`）时它们的 Supervisor 复位（`instance` 模块的 `sync_supervisor`）。要重启 iDoris。
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

/// iDoris 停止的指令（与 [`Command::Shutdown`] 同样的两个参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShutdownOrder {
    pub stop_engine: bool,
    pub deadline: Option<Instant>,
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
    /// iDoris 在停（[`EngineManager::shutdown_by`] 在送 `Shutdown` 命令**之前**先写）：实例任务忙在退役（排空、停进程、复位）里不去收命令时，
    /// 靠它知道要立刻收尾，不必等邮箱里排队的 `Shutdown`（IDORIS-CORE S6，Codex 预审 H3）。只记第一个。
    pub shutdown: watch::Sender<Option<ShutdownOrder>>,
    /// 盖章与入队同属一把锁（[`EngineManager::command`]）：序号的先后就是邮箱里的先后。
    stamp: Mutex<()>,
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
            shutdown: watch::channel(None).0,
            stamp: Mutex::new(()),
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
            // 共用这个引擎的卡（同一个 endpoint：伴生卡）据此继承它的失败状态（Codex 审计 H1）。
            shared
                .health
                .register_managed_endpoint(&plan.endpoint, &plan.id);
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
        // 一个绝对的截止时间管入队与等回答两头（IDORIS-CORE S6，Codex 终审 M5）：邮箱满了、实例任务又忙着排空或复位时，
        // 入队本身就会等，不能让它排在「预算开始计时」之前。
        let deadline =
            Instant::now() + self.shared.config.timings.stop_grace + Duration::from_secs(3);
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
                deadline: None,
            };
            if tokio::time::timeout_at(deadline, sender.send(request))
                .await
                .is_ok_and(|sent| sent.is_ok())
            {
                waits.push(answer);
            }
        }
        let _ = tokio::time::timeout_at(deadline, async {
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
        // 一个绝对的截止时间管入队、等回答两头（`Timings::command_timeout`，IDORIS-CORE S6，Codex 终审 M5）；同一个截止时间也盖在
        // 命令上：实例任务到点还没收下它，就当它没有发生过（回答 `Closed`、不执行），所以一个已经超时的、有副作用的命令不会在
        // 调用方放弃之后才被执行。
        let deadline = Instant::now() + self.shared.config.timings.command_timeout;
        let call = async {
            // 先占邮箱的位子（可能要等：邮箱满了），**占到之后**才盖章并入队，两步在同一把锁里（Codex 复审 M4）：序号的先后就是命令在邮箱里的先后。
            // 先盖章、后入队时，一个盖了章的命令可以在入队之前被抢占，排在更晚盖章的 `stop` 后面，`stop` 的截断就盖不住它。序号不大于某次
            // 操作完成时的 `seq_now` 的 `restart`，都是在那次操作进行期间（或之前）入队的，它们并进那次操作的结果（见 `Actor::restart_cover`）。
            let permit = sender.reserve().await.map_err(|_| CommandError::Closed)?;
            {
                let _order = self
                    .shared
                    .stamp
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let seq = self.shared.next_seq(id);
                permit.send(Request {
                    command,
                    reply: Some(reply),
                    seq,
                    deadline: Some(deadline.into_std()),
                });
            }
            answer.await.map_err(|_| CommandError::Closed)?
        };
        tokio::time::timeout_at(deadline, call)
            .await
            .unwrap_or(Err(CommandError::Closed))
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
    /// 已经 `stopped` 时幂等。命令被接受**之前**已经关了 Supervisor 的门、发布了 `stopped`（不可服务：数据面立刻 503，Supervisor 不再
    /// 准入新的加载与聊天）；之后才排空在途的、停进程（至多一个 `stop_grace` 加排空的上限）：这几秒里引擎还握在手里，`owned`、`pid`
    /// 还在，进程确认消失之后才是 `owned: false`，调用方轮询 `/health` 或 Admin。进程确认消失之后，端点才被封上（G2，不是撤销登记），
    /// Supervisor 的账本也清空（引擎已经没有了）。
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
        self.shutdown_by(stop_engines, None).await;
    }

    /// [`Self::shutdown`]，带整个停止必须完成的期限（iDoris 收到信号之后，launcher 在 17 秒时 SIGKILL 它）：停引擎的宽限只拿
    /// 期限里剩下的（[`super::backoff::stop_windows`]：不到 2 秒就 SIGTERM 之后立刻 SIGKILL），等各实例的上限也跟着缩到期限
    /// 之后一点余量。`None` 与 [`Self::shutdown`] 相同。
    pub async fn shutdown_by(&self, stop_engines: bool, deadline: Option<tokio::time::Instant>) {
        // 先公告、后送命令（Codex 预审 H3）：实例任务可能正忙在一次要等 30 秒的排空里，邮箱里排在后面的 `Shutdown` 它看不到；公告让它的每一个
        // 长等待立刻放手，改走收尾。
        self.shared.shutdown.send_if_modified(|order| {
            let first = order.is_none();
            if first {
                *order = Some(ShutdownOrder {
                    stop_engine: stop_engines,
                    deadline,
                });
            }
            first
        });
        // 整个收尾（送达加等各实例答复）只有这**一个**结束时刻：实例自己在期限里收尾（宽限加确认窗口），这是外面的兜底。
        let end = tokio::time::Instant::now()
            + match deadline {
                Some(deadline) => {
                    deadline.saturating_duration_since(tokio::time::Instant::now())
                        + super::backoff::MIN_CONFIRM
                        + Duration::from_millis(100)
                }
                None => self.shared.config.timings.stop_grace + Duration::from_secs(3),
            };
        let senders: Vec<(&str, &mpsc::Sender<Request>)> = self
            .senders
            .iter()
            .map(|(id, sender)| (id.as_str(), sender))
            .collect();
        let delivered = deliver_shutdown(&senders, stop_engines, deadline, end).await;
        for id in &delivered.undelivered {
            self.shared.log(&format!(
                "[idoris] 引擎 {id}：停止命令没能送到（命令队列一直是满的，直到收尾期限）：中止它的实例任务，所有权记录保留，下一个 iDoris 会认回并收尾"
            ));
        }
        let finished = tokio::time::timeout_at(end, async {
            for answer in delivered.answers {
                let _ = answer.await;
            }
        })
        .await;
        let tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            // 超时、或命令没送到的实例（它没有收到停止命令，不会自己结束）：中止；`OwnedEngine` 的 `Drop` 仍会兜底。
            if finished.is_err() || !delivered.undelivered.is_empty() {
                task.abort();
            }
            let _ = task.await;
        }
    }
}

/// [`deliver_shutdown`] 的结果。
struct Delivered {
    /// 已送到的实例的答复。
    answers: Vec<oneshot::Receiver<Result<(), CommandError>>>,
    /// 到 `end` 命令队列还是满的、没能送到的实例。
    undelivered: Vec<String>,
}

/// 把 `Shutdown` **同时**送给每个实例（M9）：一个实例的命令队列满了（任务忙在一次长停止里、队列里还压着别的命令），不能让后面的实例也收不到停止命令
/// （此前是逐个 `send().await`，队列一满就卡在那里，之后的实例收不到、送达这一步也没有时限）。每个送达各自受 `end` 约束；到 `end` 还没送到的记进
/// `undelivered`（调用方点名写日志、中止那个实例任务）。实例任务已经结束（通道已关）的没有东西可停，既不等也不算没送到。
async fn deliver_shutdown(
    senders: &[(&str, &mpsc::Sender<Request>)],
    stop_engine: bool,
    deadline: Option<tokio::time::Instant>,
    end: tokio::time::Instant,
) -> Delivered {
    let deliveries = senders.iter().map(|(id, sender)| async move {
        let (reply, answer) = oneshot::channel();
        let request = Request {
            command: Command::Shutdown {
                stop_engine,
                deadline,
            },
            reply: Some(reply),
            seq: 0,
            deadline: None,
        };
        match tokio::time::timeout_at(end, sender.send(request)).await {
            Ok(Ok(())) => (*id, Some(answer), false),
            Ok(Err(_)) => (*id, None, false),
            Err(_) => (*id, None, true),
        }
    });
    let mut delivered = Delivered {
        answers: Vec::new(),
        undelivered: Vec::new(),
    };
    for (id, answer, undelivered) in futures_util::future::join_all(deliveries).await {
        if let Some(answer) = answer {
            delivered.answers.push(answer);
        }
        if undelivered {
            delivered.undelivered.push(id.to_string());
        }
    }
    delivered
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

    /// 引擎种类到 backend 的对应只在 `managed_backend_kind` 一处认定：生产的构造器与纯校验的种类认定同进同退。
    /// M9：第一个实例的命令队列满了（没人在取），第二个是好的。停止命令是同时送的：好的那个**立刻**收到，不等第一个；第一个到期限还没送到，
    /// 记进「没送到」（调用方点名写日志、中止它的实例任务）。逐个 `send().await` 时，第二个永远收不到。
    #[tokio::test(start_paused = true)]
    async fn one_full_command_queue_does_not_keep_the_stop_from_the_other_instances() {
        let (blocked, _nobody_reads_it) = mpsc::channel::<Request>(1);
        blocked
            .send(Request {
                command: Command::Start,
                reply: None,
                seq: 0,
                deadline: None,
            })
            .await
            .unwrap();
        let (healthy, mut healthy_requests) = mpsc::channel::<Request>(8);
        // 字典序里被堵住的排在前面：旧写法（按顺序逐个送）会先卡在它上面。
        let senders = [("a-blocked", &blocked), ("b-healthy", &healthy)];
        let begun = tokio::time::Instant::now();
        let end = begun + Duration::from_secs(5);
        let deadline = Some(begun + Duration::from_secs(4));
        let delivery = deliver_shutdown(&senders, true, deadline, end);
        tokio::pin!(delivery);
        let request = tokio::select! {
            request = healthy_requests.recv() => {
                request.unwrap_or_else(|| panic!("the healthy instance gets the stop"))
            }
            _ = &mut delivery => panic!("the delivery cannot be over while one queue is still full"),
        };
        assert_eq!(
            begun.elapsed(),
            Duration::ZERO,
            "the healthy instance had its command before any time passed"
        );
        match request.command {
            Command::Shutdown {
                stop_engine,
                deadline: carried,
            } => {
                assert!(stop_engine);
                assert_eq!(carried, deadline, "the deadline travels with the command");
            }
            _ => panic!("the healthy instance was sent something other than the stop"),
        }
        let delivered = delivery.await;
        assert_eq!(delivered.undelivered, ["a-blocked"]);
        assert_eq!(
            delivered.answers.len(),
            1,
            "only the healthy one will answer"
        );
        assert_eq!(
            begun.elapsed(),
            Duration::from_secs(5),
            "given up at the end, not before"
        );
    }

    /// 实例任务已经结束（通道关了）：没有东西可停，既不等它的答复、也不算没送到。
    #[tokio::test(start_paused = true)]
    async fn an_instance_that_has_already_ended_is_neither_waited_for_nor_reported_undelivered() {
        let (gone, receiver) = mpsc::channel::<Request>(8);
        drop(receiver);
        let senders = [("gone", &gone)];
        let begun = tokio::time::Instant::now();
        let delivered =
            deliver_shutdown(&senders, false, None, begun + Duration::from_secs(5)).await;
        assert!(delivered.answers.is_empty());
        assert!(delivered.undelivered.is_empty());
        assert_eq!(begun.elapsed(), Duration::ZERO);
    }

    #[test]
    fn the_production_backend_follows_the_single_kind_mapping() {
        for kind in ["omlx", "lmstudio", "", "OMLX", "omlx "] {
            assert_eq!(
                production_backend(kind).map(|backend| backend.kind()),
                managed_backend_kind(kind),
                "{kind:?}"
            );
        }
        assert!(production_backend("omlx").is_some());
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

    /// 一个邮箱已经被命令填满、实例任务又在忙（谁也不收）的管理器：实例任务握着邮箱的接收端（通道开着、满着），既不收命令也不结束
    /// （直到被中止）。
    fn manager_with_a_busy_instance(
        command_timeout: Duration,
        stop_grace: Duration,
    ) -> EngineManager {
        let timings = Timings {
            command_timeout,
            stop_grace,
            ..Timings::default()
        };
        let (sender, receiver) = mpsc::channel::<Request>(8);
        for seq in 1..=8 {
            sender
                .try_send(Request {
                    command: Command::Stop,
                    reply: None,
                    seq,
                    deadline: None,
                })
                .unwrap();
        }
        let shared = Arc::new(Shared {
            config: ManagerConfig {
                state_dir: std::env::temp_dir(),
                allocated_budget_bytes: None,
                stop_on_exit: StopOnExit::Owned,
                executables: BTreeMap::new(),
                launched_by: "unit-test".to_owned(),
                timings,
                inspector: Arc::new(idoris_upstream::engine::SystemInspector::new()),
                log: Arc::new(|_| {}),
            },
            health: HealthState::default(),
            gate_open: AtomicBool::new(false),
            statuses: Mutex::new(BTreeMap::new()),
            kinds: BTreeMap::new(),
            ports: BTreeMap::new(),
            enqueued: BTreeMap::from([("omlx".to_owned(), AtomicU64::new(0))]),
            degraded: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            changed: watch::channel(0_u64).0,
            failure: watch::channel(None).0,
            shutdown: watch::channel(None).0,
            stamp: Mutex::new(()),
            runtimes: OnceLock::new(),
        });
        let task = tokio::spawn(async move {
            let _mailbox = receiver;
            std::future::pending::<()>().await;
        });
        EngineManager {
            shared,
            senders: BTreeMap::from([("omlx".to_owned(), sender)]),
            tasks: Mutex::new(vec![task]),
        }
    }

    /// `shutdown` bounded by an outer timeout: a mutated (or broken) budget must fail the test, never hang the test binary.
    async fn shutdown_bounded(manager: &EngineManager) {
        tokio::time::timeout(Duration::from_secs(20), manager.shutdown(true))
            .await
            .unwrap_or_else(|_| panic!("shutdown hung on a full mailbox"));
    }

    /// IDORIS-CORE S6, Codex final review M5: the shutdown budget starts **before** the enqueue. A full mailbox in front of a busy
    /// instance used to hold `shutdown` at `sender.send().await` for as long as the instance stayed busy, before the budget even
    /// began; now the enqueue is inside the budget, the instance that could not take the command is aborted, and the call
    /// returns.
    #[tokio::test]
    async fn shutdown_returns_within_its_budget_when_the_mailbox_of_a_busy_instance_is_full() {
        let manager =
            manager_with_a_busy_instance(Duration::from_secs(60), Duration::from_millis(100));
        let started = std::time::Instant::now();
        let finished = tokio::time::timeout(Duration::from_secs(20), manager.shutdown(true)).await;
        assert!(finished.is_ok(), "shutdown hung on a full mailbox");
        // The budget is `stop_grace` (100 ms) plus three seconds, counted from before the enqueue.
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
    }

    /// The same for the settling gate's give-up: the enqueue is inside its budget.
    #[tokio::test]
    async fn giving_up_unsettled_instances_returns_within_its_budget_when_the_mailbox_is_full() {
        let manager =
            manager_with_a_busy_instance(Duration::from_secs(60), Duration::from_millis(100));
        manager
            .shared
            .publish("omlx", status(UpstreamState::Starting, false));
        let finished =
            tokio::time::timeout(Duration::from_secs(20), manager.give_up_unsettled()).await;
        assert!(finished.is_ok(), "give_up_unsettled hung on a full mailbox");
        shutdown_bounded(&manager).await;
    }

    /// An operator command that cannot be enqueued (the mailbox is full, the instance is busy) is abandoned at its one absolute
    /// deadline (`command_timeout`) with `Closed` (Admin: 503 `idoris_stopping`), instead of waiting for as long as the instance
    /// stays busy.
    #[tokio::test]
    async fn a_command_that_cannot_be_enqueued_is_abandoned_at_its_deadline() {
        let manager =
            manager_with_a_busy_instance(Duration::from_millis(300), Duration::from_millis(100));
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(20), manager.stop_upstream("omlx"))
            .await
            .unwrap_or_else(|_| panic!("the command did not wait forever"));
        assert_eq!(result, Err(CommandError::Closed));
        assert!(
            started.elapsed() >= Duration::from_millis(250)
                && started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        shutdown_bounded(&manager).await;
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
