//! 引擎管理器：每个托管卡一个实例任务（`instance.rs`），这里是它们共用的东西、对外的接口，以及从组件卡到实例计划的转换。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use idoris_upstream::engine::{ProcessInspector, StopOnExit};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::backend::{EngineBackend, OmlxBackend};
use super::backoff::Timings;
use super::instance::{Actor, Command, Request};
use crate::readiness::{HealthState, UpstreamPlan, UpstreamReason, UpstreamState};
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
}

/// 一个实例当前的状态（管理器内部，也是门控判据的来源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Status {
    pub state: UpstreamState,
    pub reason: Option<UpstreamReason>,
    pub owned: bool,
}

/// 各实例任务共用的东西。
pub(super) struct Shared {
    pub config: ManagerConfig,
    pub health: HealthState,
    /// 完整的 Router 已经按「所有引擎就绪」的判据放行（[`EngineManager::open_gate`]）：之后自有引擎的进程退出
    /// 无法在进程内恢复，是退出码 5。
    pub gate_open: AtomicBool,
    statuses: Mutex<BTreeMap<String, Status>>,
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
        let mut statuses = self.statuses.lock().unwrap_or_else(PoisonError::into_inner);
        statuses.insert(id.to_owned(), status);
        self.health
            .set_managed(id, status.state, status.reason, status.owned);
        drop(statuses);
        self.changed.send_modify(|version| *version += 1);
    }

    fn all_ready(statuses: &BTreeMap<String, Status>) -> bool {
        statuses
            .values()
            .all(|status| status.state == UpstreamState::Ready)
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
        let shared = Arc::new(Shared {
            config,
            health,
            gate_open: AtomicBool::new(false),
            statuses: Mutex::new(BTreeMap::new()),
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
            let (sender, receiver) = mpsc::channel(8);
            // 先登记 `starting`：门控判据与 `/health` 一开始就有这个实例。
            shared.publish(
                &plan.id,
                Status {
                    state: UpstreamState::Starting,
                    reason: None,
                    owned: false,
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

    /// 等到每个实例都处于 `ready`（同一时刻）。已经是了就立刻返回；有实例处于终态时一直等下去，直到它们被
    /// `start`、半开重试等带回就绪，或进程停止。
    pub async fn wait_all_ready(&self) {
        let mut changed = self.shared.changed.subscribe();
        loop {
            {
                let statuses = self
                    .shared
                    .statuses
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if Shared::all_ready(&statuses) {
                    return;
                }
            }
            if changed.changed().await.is_err() {
                // 管理器不再有写者：永远不会再变。
                std::future::pending::<()>().await;
            }
        }
    }

    /// 门控放行：**在同一把锁里**确认所有实例仍是 `ready`，是就把 `gate_open` 置位并返回 `true`；不是（引擎在
    /// [`Self::wait_all_ready`] 返回之后又失效了）返回 `false`，调用方回去再等。置位之后，自有引擎的进程退出就是
    /// 退出码 5（[`Self::engine_failed`]）。
    pub fn open_gate(&self) -> bool {
        let statuses = self
            .shared
            .statuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if Shared::all_ready(&statuses) {
            self.shared.gate_open.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
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
        sender
            .send(Request {
                command,
                reply: Some(reply),
            })
            .await
            .map_err(|_| CommandError::Closed)?;
        answer.await.map_err(|_| CommandError::Closed)?
    }

    /// 从 `stopped`、`failed`、`absent`、`unavailable` 重新走启动决策；已经 `ready`、或正在启动时幂等。
    pub async fn start_upstream(&self, id: &str) -> Result<(), CommandError> {
        self.command(id, Command::Start).await
    }

    /// 停止 iDoris 自己拥有的引擎，状态 `stopped`；不是自己的（外部引擎、没有引擎）一律 [`CommandError::NotOwned`]，
    /// 已经 `stopped` 时幂等。
    pub async fn stop_upstream(&self, id: &str) -> Result<(), CommandError> {
        self.command(id, Command::Stop).await
    }

    /// `stop` 之后重新走启动决策；不是自己拥有的引擎同样 [`CommandError::NotOwned`]。
    pub async fn restart_upstream(&self, id: &str) -> Result<(), CommandError> {
        self.command(id, Command::Restart).await
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
