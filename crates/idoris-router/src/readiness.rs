//! `/health` 的就绪快照与上游探测器 v0（接口规范 §2.11.1，wire 契约 1.3.0 的内容）。
//!
//! # 版本号：按字段是否存在判断，不要看 `contract_version`
//!
//! 新字段（`phase`、`ready`、`started_at_ms`、`registered`、`upstreams[]`）**从 IDORIS-CORE 切片 S1 起就出现**，
//! 但 `/health.contract_version` 到 v0.6.0 发版才升到 1.3.0（规范 §2.11：发版前仍是 1.2.0；切片 S11 统一升）。
//! 原因是 warmup、embeddings 伴生卡、Admin `upstreams` 等 1.3.0 的其他功能还没做齐，调用方不能从版本号推断它们
//! 已经有了。所以调用方按字段是否存在判断，不要看版本号。
//!
//! # 快照，不是探测
//!
//! `/health` 必须 O(1)，不做任何上游 I/O。规范的设计目标是响应 ≤ 200 毫秒，靠「不做 I/O」达到，没有测试直接钉住
//! 这个毫秒数。Agent24 每 5 秒探测、2 秒超时、连续 3 次超时就判卡死并 SIGTERM，一个会被慢上游拖住的 `/health`
//! 会造成误杀与重启风暴。所以 `/health` 只读 [`HealthState`]，状态由后台探测器（[`spawn_prober`]）写入。
//!
//! # 哪些卡算「上游」
//!
//! 规范：「Supervisor 管理的卡与 resident 的 HTTP 卡」。这里再收紧一步，只列 **loopback** 的 `http_service` 卡：
//! 远程 / 局域网卡不是本机引擎，探测它们等于让 iDoris 在没有任何用户请求时对外发流量（违背本项目的出站
//! 纪律）；订阅中转（`provider.id == "subscription"`，进程卡）不是引擎，也没有 O(1) 的就绪判据。这两类都不列入
//! `upstreams` 与发现文件的 `launch.engines`（规范 §2.11.1 把「订阅中转是否列入」留给 S1 实现前确认，
//! 这是 S1 的取舍，PR 说明里列为「需要 owner 确认」）。`components` 仍计所有卡。
//!
//! # 状态来源
//!
//! - Supervisor 管理的卡（oMLX 这类 on_demand 卡）：状态取自 Supervisor，带 200 毫秒超时。Supervisor 返回错误
//!   （启动错误粘滞、被毒化、actor 已退出）报 `unavailable` + `startup_latched`，**不能**因为 `/v1/models` 能通就报
//!   `ready`（聊天实际是 503）。超时（actor 还在做启动采样）没有新信息，保持原状态。
//! - resident 卡：每 5 秒（启动期每 250 毫秒，直到每个上游都有了第一次观测）`GET /v1/models`，2 秒超时；
//!   401/403 报 `auth_required`（没带 key）或 `auth_failed`（带了 key 被拒）。key 的解析与 `GET /v1/models`
//!   一致：只对 loopback 的 oMLX 卡，按「oMLX 密钥」的顺序（规范 §2.9）。
//! - 远程熔断开关（`IDORIS_REMOTE_KILL_SWITCH=1`，运行时可切）闭合时，不可信本地的卡不发任何探测，报
//!   `unavailable`（与 `/v1/models`、Supervisor 启动探测的口径一致）。
//!
//! # `phase`
//!
//! 有托管上游处于终态 → `degraded`（优先；勘误十二（4））；否则任一上游还是 `starting`（没有第一次观测）→ `starting`；
//! 否则至少一个 `ready` → `ready`；否则 `degraded`。门控未开时 `starting` 之外另有 [`gated_phase`]。
//! `starting` 不等于数据面不可用：只有门控未开时才 503 `idoris_starting`；门控已开（或没有托管卡）而仍有上游没有第一次观测时，
//! `phase` 仍是 `starting`，数据面已经在服务（规范 §2.11.1）。
//! S1 没有引擎管理（那是 S5 的启动门控）：`starting` 持续到每个上游都有了第一次观测为止。resident 卡通常一轮探测
//! 就有结论；Supervisor 管理的卡在它还在做启动采样时，状态查询超时按「没有新信息」处理、保持 `starting`，所以会更久
//! （单次调用上限是 `adapter_call_timeout`，默认 30 秒）。期间数据面照常服务（没有托管卡时行为不变，规范 §2.7.6）。
//! `status` 仍恒为 `"ok"`。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use idoris_contracts::ComponentCard;
use idoris_contracts::component_card::Form;
use idoris_contracts::provider::Locality;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::engine_kind::{EngineIdentity, EngineKindError, engine_identity};
use crate::runtime::RuntimeRegistry;

/// resident 卡的探测间隔。
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// 有上游还没有第一次观测时的重试间隔：让启动期很快收敛，不必等满 5 秒。
pub const STARTING_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// 一次 resident 探测的总时限（连接加响应）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// 向 Supervisor 问状态的时限。
pub const SUPERVISOR_STATUS_TIMEOUT: Duration = Duration::from_millis(200);
/// 探测响应体上限：只需要看到 `data` 数组，不为一个异常的本地服务囤内存。
const MAX_PROBE_BODY_BYTES: usize = 4 * 1024 * 1024;

/// `upstreams[].state`（规范 §2.11.1）。S1 只会产生 `ready`、`starting`、`unavailable`，
/// 其余是 S5 引擎管理器的状态，线上字符串现在就固定下来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamState {
    Ready,
    Starting,
    Unavailable,
    Restarting,
    Failed,
    Stopped,
    Absent,
}

impl UpstreamState {
    /// 线上的字符串（与 serde 的 `snake_case` 一致），给日志用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Starting => "starting",
            Self::Unavailable => "unavailable",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Absent => "absent",
        }
    }
}

impl UpstreamReason {
    /// 线上的字符串（与 serde 的 `snake_case` 一致），给日志用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EngineNotFound => "engine_not_found",
            Self::PortInUse => "port_in_use",
            Self::AuthRequired => "auth_required",
            Self::AuthFailed => "auth_failed",
            Self::StartupTimeout => "startup_timeout",
            Self::CrashLoop => "crash_loop",
            Self::FencePending => "fence_pending",
            Self::BudgetZero => "budget_zero",
            Self::BaseUnavailable => "base_unavailable",
            Self::StartupLatched => "startup_latched",
        }
    }
}

/// `upstreams[].reason`：稳定原因码。给人看的文字另放，不属于契约。S1 只会产生 `auth_required`、
/// `auth_failed`、`startup_latched`；其余由引擎管理器（S5）产生。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamReason {
    EngineNotFound,
    PortInUse,
    AuthRequired,
    AuthFailed,
    StartupTimeout,
    CrashLoop,
    FencePending,
    BudgetZero,
    /// iDoris 自己的 oMLX base 目录建不出来或核对不过（规范 §2.7.6）。
    BaseUnavailable,
    StartupLatched,
}

/// 门控降级放行时没有就绪的托管上游（接口规范 §2.7.6，勘误六）：路由到它的请求不碰引擎，直接 503，原因码取自 `reason`
/// （没有稳定原因码的失败，例如模型目录取不到，沿用已有的 `supervisor_unavailable`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedFailure {
    pub engine_kind: String,
    pub state: UpstreamState,
    pub reason: Option<UpstreamReason>,
}

/// 一个托管上游处于这个状态与原因时，数据面是不是该把请求交给它（IDORIS-CORE S6，G1；接口规范 §2.7.6 勘误六（9）、勘误十）。
///
/// 只有就绪的才交；正在拉起（`starting`、`restarting`）、没有引擎（`absent`）、失败（`failed`，含外部引擎的端口换了主人：
/// `port_in_use`）、被停下（`stopped`）的不交：请求不碰那个端口，直接 503 带原因。`unavailable` 要看有没有确定的原因：
/// 有（要 key、key 被拒、围栏还在、Supervisor 粘滞、等不到就绪）是确定的失败，不交；没有（引擎活着、探测没答上来，例如 oMLX
/// 正忙着加载大模型，`/v1/models` 变慢）还是交：那只是慢，Supervisor 自己的调用有超时，不能因为一次探测慢就让整张卡 503。
pub fn upstream_serves(state: UpstreamState, reason: Option<UpstreamReason>) -> bool {
    match state {
        UpstreamState::Ready => true,
        UpstreamState::Unavailable => reason.is_none(),
        UpstreamState::Starting
        | UpstreamState::Restarting
        | UpstreamState::Failed
        | UpstreamState::Stopped
        | UpstreamState::Absent => false,
    }
}

/// `/health.upstreams[]` 的元素。`reason` 出现时 `state` 不是 `ready`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpstreamStatus {
    pub id: String,
    pub engine_kind: String,
    pub state: UpstreamState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<UpstreamReason>,
    /// 卡声明了由 iDoris 管理引擎（`extensions.idoris_managed`）。
    pub managed: bool,
    /// 引擎进程是 iDoris 自己拉起的（含接管上一个 iDoris 留下的）。外部引擎、没有引擎管理器的卡恒为 false。
    pub owned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Ready,
    Degraded,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
        }
    }
}

/// 启动门控（规范 §2.7.6「与 Supervisor、围栏、`/health` 的关系」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    /// 没有托管卡：门控视为立即完成，`phase` 只由上游状态推出（v0.5.0 的行为，字节级不变）。
    NotGated,
    /// 有托管卡，完整的 Router 还没有替换上去：数据面除 `/health` 外一律 503 `idoris_starting`。
    Pending,
    /// 门控已完成（完整的 Router 已替换）。
    Open,
}

/// 上游处于终态（`absent`、`failed`、`unavailable`、`stopped`）：此刻没有在往就绪走，要等外部变化（用户装好引擎、外部
/// 引擎自己恢复）、半开重试或显式 `start` 才会回到就绪（门控降级放行之后，没就绪的托管引擎冻结，只有重启 iDoris 才会）。
/// 托管上游处于终态是它「落定」的失败，不拖住门控（接口规范 §2.7.6，勘误六）；`phase` 是 `degraded`（规范 §2.11.1）。
fn is_terminal(state: UpstreamState) -> bool {
    matches!(
        state,
        UpstreamState::Absent
            | UpstreamState::Failed
            | UpstreamState::Unavailable
            | UpstreamState::Stopped
    )
}

/// 门控未完成时的 `phase`（规范 §2.11.1）：任一托管上游已经处于终态（落定的失败；其余托管上游落定之后门控降级放行）是
/// `degraded`；否则门控仍在进行
/// （正在探测、拉起引擎或等它就绪），是 `starting`。门控未完成时不会是 `ready`，哪怕某张 resident 卡已经探测为 `ready`。
fn gated_phase(upstreams: &[UpstreamStatus]) -> Phase {
    if upstreams
        .iter()
        .any(|upstream| upstream.managed && is_terminal(upstream.state))
    {
        Phase::Degraded
    } else {
        Phase::Starting
    }
}

/// 由上游状态推出 `phase`（没有门控，或门控已完成）。
///
/// 门控放行之后，有托管上游处于终态（`absent`、`failed`、`unavailable`、`stopped`）是 `degraded`，哪怕别的上游
/// 都在服务：门控不会被一个起不来的托管引擎拖住（接口规范 §2.7.6，勘误六），但界面要能看出有一个托管引擎没好。
/// **托管上游的终态优先于「某个上游还没有第一次观测」**（勘误十二（4）：探测器在门控放行之后才起，别的上游此刻还是 `starting`，
/// 此前这个 `starting` 遮住了托管引擎的终局失败）。没有托管上游时这一条不起作用（`managed` 恒为 false），行为与 v0.5.0 一致。
///
/// 门控已开（或没有门控）而仍有上游没有第一次观测时 `phase` 可以是 `starting`，这时数据面已经在服务（只有门控未开时才 503
/// `idoris_starting`，规范 §2.11.1）。取值集合不变：`starting`、`ready`、`degraded`。
pub fn phase_of(upstreams: &[UpstreamStatus]) -> Phase {
    // 有托管上游时 `phase` 只反映托管上游（与 Agent24 商定的契约，IDORIS-CORE S6，Codex 预审 M1）：别的上游（resident、云）是否就绪、是否还没
    // 第一次观测，既不能在托管引擎重启时把 `phase` 撑成 `ready`，也不能在托管引擎就绪时把它拉回 `starting`。次序：终态（`degraded`，哪怕另一个
    // 托管上游还在启动：Agent24 据此做有界等待）> 启动或重启中（`starting`）> 全部就绪（`ready`）。
    if upstreams.iter().any(|upstream| upstream.managed) {
        let managed = || upstreams.iter().filter(|upstream| upstream.managed);
        return if managed().any(|upstream| is_terminal(upstream.state)) {
            Phase::Degraded
        } else if managed().any(|upstream| {
            matches!(
                upstream.state,
                UpstreamState::Starting | UpstreamState::Restarting
            )
        }) {
            Phase::Starting
        } else {
            Phase::Ready
        };
    }
    if upstreams
        .iter()
        .any(|upstream| upstream.managed && is_terminal(upstream.state))
    {
        Phase::Degraded
    } else if upstreams
        .iter()
        .any(|upstream| upstream.state == UpstreamState::Starting)
    {
        Phase::Starting
    } else if upstreams
        .iter()
        .any(|upstream| upstream.state == UpstreamState::Ready)
    {
        Phase::Ready
    } else {
        Phase::Degraded
    }
}

/// `/health` 的就绪部分，一次读出的一致快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthView {
    pub phase: Phase,
    /// 等于 `phase == Ready`。
    pub ready: bool,
    pub started_at_ms: u64,
    pub registered: bool,
    pub upstreams: Vec<UpstreamStatus>,
}

/// 一次探测的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Ready,
    Unavailable(Option<UpstreamReason>),
    /// 没有新信息（Supervisor 还没应答）：保持原状态。
    NoChange,
}

struct Inner {
    started_at_ms: u64,
    registered: AtomicBool,
    gate: RwLock<GateState>,
    upstreams: RwLock<Vec<UpstreamStatus>>,
    /// 引擎管理器登记的：托管引擎的 endpoint（规范化的标识）→ 管着它的托管上游的 id。共用这个引擎的别的卡（同一个 endpoint：伴生卡
    /// `omlx-embed`）据此继承它的失败（[`HealthState::failure_of_card`]）。
    managed_endpoints: RwLock<BTreeMap<String, String>>,
}

/// 共享的就绪状态。克隆很便宜（一个 `Arc`），`/health` 与探测器各持一份。
#[derive(Clone)]
pub struct HealthState {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for HealthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthState")
            .field("view", &self.view())
            .finish()
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new(unix_millis_now())
    }
}

impl HealthState {
    /// 没有任何上游（库调用方与不探测的测试）：`phase` 是 `degraded`。
    pub fn new(started_at_ms: u64) -> Self {
        Self::with_upstreams(started_at_ms, Vec::new())
    }

    pub fn with_upstreams(started_at_ms: u64, upstreams: Vec<UpstreamStatus>) -> Self {
        Self {
            inner: Arc::new(Inner {
                started_at_ms,
                registered: AtomicBool::new(false),
                gate: RwLock::new(GateState::NotGated),
                upstreams: RwLock::new(upstreams),
                managed_endpoints: RwLock::new(BTreeMap::new()),
            }),
        }
    }

    pub fn started_at_ms(&self) -> u64 {
        self.inner.started_at_ms
    }

    /// 当前是否有已 `ack` 的登记会话。没有 launcher、没有 `register_socket`、或登记不上时恒为 false；登记客户端
    /// （`register`）在会话建立（`ack.proof` 校验通过）时设为 true，会话断开或停止时设回 false。
    pub fn registered(&self) -> bool {
        self.inner.registered.load(Ordering::SeqCst)
    }

    pub fn set_registered(&self, registered: bool) {
        self.inner.registered.store(registered, Ordering::SeqCst);
    }

    /// 启动门控的当前状态。
    pub fn gate(&self) -> GateState {
        *self
            .inner
            .gate
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 引擎管理器（S5）设置：有托管卡时先 [`GateState::Pending`]，完整的 Router 替换上去之后 [`GateState::Open`]。
    pub fn set_gate(&self, gate: GateState) {
        *self
            .inner
            .gate
            .write()
            .unwrap_or_else(PoisonError::into_inner) = gate;
    }

    fn read(&self) -> Vec<UpstreamStatus> {
        self.inner
            .upstreams
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 引擎管理器写一个托管上游的状态（它是这些上游的唯一写者，探测器不碰它们）。不认识的 id 什么也不改。
    pub fn set_managed(
        &self,
        id: &str,
        state: UpstreamState,
        reason: Option<UpstreamReason>,
        owned: bool,
    ) {
        let mut upstreams = self
            .inner
            .upstreams
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(upstream) = upstreams.iter_mut().find(|upstream| upstream.id == id) {
            upstream.state = state;
            upstream.reason = reason;
            upstream.owned = owned;
        }
    }

    /// 引擎管理器登记：`endpoint` 上的引擎归托管上游 `id` 管（IDORIS-CORE S6，Codex 审计 H1）。引擎管理器启动时对每个托管上游登记一次，
    /// 先于它的第一次探测。`endpoint` 不是 http(s) URL 时什么也不做。
    pub fn register_managed_endpoint(&self, endpoint: &str, id: &str) {
        if let Some(identity) = idoris_upstream::omlx::key::endpoint_identity(endpoint) {
            self.inner
                .managed_endpoints
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(identity, id.to_owned());
        }
    }

    /// 这张卡此刻没在服务就给出状态与原因，否则 `None`。两种情况：它自己是没在服务的托管上游（[`Self::managed_failure`]）；或者它与
    /// 一个这样的托管上游**共用同一个引擎**（同一个 endpoint：伴生卡 `omlx-embed` 与主卡 `omlx`）——共用引擎的卡**继承**那个引擎的
    /// 失败状态，所以主卡的端口被别的东西占了（`failed` + `port_in_use`）时，伴生卡的后台探测、`/v1/models`、预热与转发同样不碰那个
    /// 端口（IDORIS-CORE S6，Codex 审计 H1）。不共用引擎的上游不受影响，照常服务。每次调用都读此刻的状态。
    pub fn failure_of_card(&self, card: &ComponentCard) -> Option<ManagedFailure> {
        if let Some(failure) = self.managed_failure(&card.provider.id) {
            return Some(failure);
        }
        let identity = idoris_upstream::omlx::key::endpoint_identity(&card.endpoint)?;
        let owner = self
            .inner
            .managed_endpoints
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&identity)
            .cloned()?;
        if owner == card.provider.id {
            return None;
        }
        self.managed_failure(&owner)
    }

    /// 这个托管上游此刻没在服务就给出它的状态与原因（[`upstream_serves`]），否则（在服务、不是托管上游、没有这个 id）`None`。
    /// **每次调用都读此刻的快照**：数据面据此在每个请求上决定要不要把它交给引擎，引擎在运行期失败、被 Admin 停下、重启中、
    /// 端口换了主人，下一个请求就 503，不靠完整的 Router 创建时拍下的名单（IDORIS-CORE S6，G1）。
    pub fn managed_failure(&self, id: &str) -> Option<ManagedFailure> {
        self.inner
            .upstreams
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|upstream| upstream.managed && upstream.id == id)
            .filter(|upstream| !upstream_serves(upstream.state, upstream.reason))
            .map(|upstream| ManagedFailure {
                engine_kind: upstream.engine_kind.clone(),
                state: upstream.state,
                reason: upstream.reason,
            })
    }

    pub fn view(&self) -> HealthView {
        // 先读门控再读上游：门控从 Pending 变成 Open 发生在上游都落定之后，这样读到 Open 时看到的上游不会比门控旧。
        let gate = self.gate();
        let upstreams = self.read();
        let phase = match gate {
            GateState::Pending => gated_phase(&upstreams),
            GateState::NotGated | GateState::Open => phase_of(&upstreams),
        };
        HealthView {
            phase,
            ready: phase == Phase::Ready,
            started_at_ms: self.inner.started_at_ms,
            registered: self.registered(),
            upstreams,
        }
    }

    /// 是否还有**探测器管的**上游没有第一次观测。托管上游由引擎管理器写，它们的 `starting` 是在拉起、等就绪，
    /// 不该让探测器一直用启动期的勤快间隔去探别的卡。
    pub fn any_starting(&self) -> bool {
        self.inner
            .upstreams
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|upstream| !upstream.managed && upstream.state == UpstreamState::Starting)
    }

    /// 写入一次观测。`reason` 只在 `unavailable` 时出现；`NoChange` 不动。
    pub fn apply(&self, id: &str, observation: Observation) {
        let mut upstreams = self
            .inner
            .upstreams
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(upstream) = upstreams.iter_mut().find(|upstream| upstream.id == id) else {
            return;
        };
        match observation {
            Observation::Ready => {
                upstream.state = UpstreamState::Ready;
                upstream.reason = None;
            }
            Observation::Unavailable(reason) => {
                upstream.state = UpstreamState::Unavailable;
                upstream.reason = reason;
            }
            Observation::NoChange => {}
        }
    }
}

/// 当前 Unix 毫秒（墙钟）。时钟早于 epoch 时取 0。
pub fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    /// 由 Supervisor 管理（on_demand / evict_to_load 的 oMLX 型卡）。
    Supervised,
    /// `LoadMode::Resident` 的 HTTP 卡，走直连代理。
    Resident,
}

/// 启动时从组件卡读出的一个引擎型上游：静态信息（id、种类、托管声明）加上探测用的卡。
#[derive(Debug, Clone)]
pub struct UpstreamPlan {
    pub id: String,
    pub identity: EngineIdentity,
    pub kind: UpstreamKind,
    card: ComponentCard,
}

impl UpstreamPlan {
    pub fn card(&self) -> &ComponentCard {
        &self.card
    }

    /// 探测之前的初始状态：`starting`，没有原因，`owned` 为 false（托管卡由引擎管理器接着写）。
    pub fn initial_status(&self) -> UpstreamStatus {
        UpstreamStatus {
            id: self.id.clone(),
            engine_kind: self.identity.engine_kind.clone(),
            state: UpstreamState::Starting,
            reason: None,
            managed: self.identity.managed,
            owned: false,
        }
    }
}

/// 从（已过订阅授权的）组件卡里挑出引擎型上游，并按规范 §2.3 的来源规则解析 `engine_kind`。
/// `idoris_managed` 声明不合法（引擎种类不认识、与 `version_pin` 不一致）就是卡校验失败。
pub fn plan_upstreams(cards: &[ComponentCard]) -> Result<Vec<UpstreamPlan>, EngineKindError> {
    let mut plans = Vec::new();
    for card in cards {
        if card.form != Form::HttpService
            || card.provider.locality != Locality::Loopback
            || idoris_policy::is_subscription_provider_id(&card.provider.id)
        {
            continue;
        }
        plans.push(UpstreamPlan {
            id: card.provider.id.clone(),
            identity: engine_identity(card)?,
            kind: if crate::dispatch::is_resident_http_service(card) {
                UpstreamKind::Resident
            } else {
                UpstreamKind::Supervised
            },
            card: card.clone(),
        });
    }
    Ok(plans)
}

/// 读完响应体并解析成 JSON，最多 `cap` 字节；读不完、超限、不是 JSON 都是 `None`。
async fn read_capped_json(
    mut response: reqwest::Response,
    cap: usize,
) -> Option<serde_json::Value> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len().saturating_add(chunk.len()) > cap {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

/// 探测一张 resident 卡：`GET {endpoint}/v1/models`。返回 200 且 JSON 里有 `data` 数组（空列表也算）才是 `ready`。
/// 绝不对非 loopback 的 endpoint 发请求。
pub async fn probe_resident(
    client: &reqwest::Client,
    card: &ComponentCard,
    timeout: Duration,
) -> Observation {
    probe_resident_admitted(client, card, None, timeout).await
}

/// 同 [`probe_resident`]，带着这一轮观测开始时读到的这张卡的引擎代际（`crate::models::admission_generation`，先于状态检查读）：
/// 能带 oMLX key 的卡**在发出请求之前**经 `authorize_omlx_request`（Codex 终审 H1、M6）：核对代际与封上、让引擎管理器的守卫重新确认
/// 监听者，被拒绝就不发，这一轮的观测是「不可用」。之前被封上的端点会被当成「没有 key」照样发一个匿名的 GET。
pub async fn probe_resident_admitted(
    client: &reqwest::Client,
    card: &ComponentCard,
    admitted_generation: Option<u64>,
    timeout: Duration,
) -> Observation {
    if !idoris_upstream::omlx::key::is_loopback_base_url(&card.endpoint) {
        return Observation::Unavailable(None);
    }
    let url = format!("{}/v1/models", card.endpoint.trim_end_matches('/'));
    let mut key_sent = false;
    // 持有到响应读完：端点上的一个在途操作。被管理的端点由端点决定（Codex 预审 L2 的 H1）：不能带 key 的卡也过同一道授权，只是不带凭证。
    let carries_key = crate::models::is_loopback_omlx(card);
    let authorization = if carries_key {
        let Ok(authorization) =
            idoris_upstream::omlx::key::authorize_omlx_request(&card.endpoint, admitted_generation)
                .await
        else {
            return Observation::Unavailable(None);
        };
        Some(authorization)
    } else if idoris_upstream::omlx::key::is_governed_endpoint(&card.endpoint) {
        let Ok(authorization) = idoris_upstream::omlx::key::authorize_omlx_request_without_key(
            &card.endpoint,
            admitted_generation,
        )
        .await
        else {
            return Observation::Unavailable(None);
        };
        Some(authorization)
    } else {
        None
    };
    // 托管的端点：发往授权核验过的拨号地址（不是卡里写的别名），用不保留空闲连接的客户端（Codex 专项评审 H1、H6）。
    let (client, send_url) = match &authorization {
        Some(authorization) => (authorization.client(client), authorization.dial_url(&url)),
        None => (client.clone(), url.clone()),
    };
    let mut request = client.get(&send_url).timeout(timeout);
    let held = if let Some(authorization) = authorization {
        if let Some(key) = carries_key
            .then(|| authorization.key_os())
            .flatten()
            .filter(|key| !key.is_empty())
        {
            // 非 UTF-8、放不进请求头的 key 是无效凭证，不是「没有凭证」（与 `GET /v1/models` 同口径）。
            let Some(mut header) = key
                .to_str()
                .and_then(|key| HeaderValue::from_str(&format!("Bearer {key}")).ok())
            else {
                return Observation::Unavailable(Some(UpstreamReason::AuthFailed));
            };
            header.set_sensitive(true);
            request = request.header(AUTHORIZATION, header);
            key_sent = true;
        }
        Some(authorization)
    } else {
        None
    };
    // 请求**建好之后**、发出前最后同步看一眼端点（Codex 预审 L2 的 H3；复审 L2 的 M1'）。
    let (client, built) = request.build_split();
    let Ok(built) = built else {
        return Observation::Unavailable(None);
    };
    #[cfg(test)]
    idoris_upstream::omlx::key::test_hooks::between_build_and_send(built.url().as_str());
    if held
        .as_ref()
        .is_some_and(|authorization| authorization.ensure_current().is_err())
    {
        return Observation::Unavailable(None);
    }
    let Ok(response) = client.execute(built).await else {
        return Observation::Unavailable(None);
    };
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Observation::Unavailable(Some(if key_sent {
            UpstreamReason::AuthFailed
        } else {
            UpstreamReason::AuthRequired
        }));
    }
    if !status.is_success() {
        return Observation::Unavailable(None);
    }
    match read_capped_json(response, MAX_PROBE_BODY_BYTES).await {
        Some(body) if body.get("data").is_some_and(serde_json::Value::is_array) => {
            Observation::Ready
        }
        _ => Observation::Unavailable(None),
    }
}

/// 探测器的时间参数。生产用 [`ProberTimings::default`]（规范给的数值）；测试缩短它们。
#[derive(Debug, Clone, Copy)]
pub struct ProberTimings {
    pub interval: Duration,
    pub starting_interval: Duration,
    pub probe_timeout: Duration,
    pub supervisor_timeout: Duration,
}

impl Default for ProberTimings {
    fn default() -> Self {
        Self {
            interval: PROBE_INTERVAL,
            starting_interval: STARTING_RETRY_INTERVAL,
            probe_timeout: PROBE_TIMEOUT,
            supervisor_timeout: SUPERVISOR_STATUS_TIMEOUT,
        }
    }
}

/// 探测器需要的一切（都是廉价克隆的句柄）。
#[derive(Clone)]
pub struct ProberContext {
    pub health: HealthState,
    pub plans: Vec<UpstreamPlan>,
    pub runtimes: RuntimeRegistry,
    pub client: reqwest::Client,
    pub remote_kill_switch: Arc<AtomicBool>,
    pub timings: ProberTimings,
}

impl ProberContext {
    /// 对一个上游做一次观测。
    pub async fn observe(&self, plan: &UpstreamPlan) -> Observation {
        // 熔断闭合：不可信本地的卡一个请求都不发。可信本地三条件（实际 loopback、`local_only`、出站仅 none/loopback）
        // 与 `GET /v1/models` 共用 `models::is_trusted_local`，所以两处的熔断口径是同一个判据。
        if self.remote_kill_switch.load(Ordering::SeqCst)
            && !crate::models::is_trusted_local(plan.card())
        {
            return Observation::Unavailable(None);
        }
        match plan.kind {
            UpstreamKind::Supervised => {
                let Some(bound) = self.runtimes.get(&plan.id) else {
                    return Observation::Unavailable(None);
                };
                match tokio::time::timeout(self.timings.supervisor_timeout, bound.status()).await {
                    Ok(Ok(_)) => Observation::Ready,
                    Ok(Err(_)) => Observation::Unavailable(Some(UpstreamReason::StartupLatched)),
                    Err(_) => Observation::NoChange,
                }
            }
            UpstreamKind::Resident => {
                // 这一轮观测准入时的引擎代际，先于状态检查读（Codex 终审 M6）。
                let admitted = crate::models::admission_generation(plan.card());
                // 与一个没在服务的托管引擎共用 endpoint 的卡（伴生卡）继承它的失败，一个请求都不发（Codex 审计 H1）。
                if let Some(failure) = self.health.failure_of_card(plan.card()) {
                    return Observation::Unavailable(failure.reason);
                }
                probe_resident_admitted(
                    &self.client,
                    plan.card(),
                    admitted,
                    self.timings.probe_timeout,
                )
                .await
            }
        }
    }

    /// 所有上游并发观测一轮并写入快照。
    pub async fn probe_round(&self) {
        let observations =
            futures_util::future::join_all(self.plans.iter().map(|plan| self.observe(plan))).await;
        for (plan, observation) in self.plans.iter().zip(observations) {
            self.health.apply(&plan.id, observation);
        }
    }
}

/// 还有上游没有第一次观测时，第 `round`（从 1 开始）次重试前的等待：`base`、2×`base`、4×`base`，之后保持 4×`base`
/// （生产是 250 毫秒、500 毫秒，之后 1 秒）。Supervisor 的邮箱容量是 64，超时没等到应答的 `status()` 消息仍留在邮箱里，
/// 所以 Supervisor 的启动采样卡住（最长一个 `adapter_call_timeout`）时，不能一直以最短间隔去追加消息。
pub fn starting_wait(base: Duration, round: u32) -> Duration {
    base.saturating_mul(1 << round.saturating_sub(1).min(2))
}

/// 启动后台探测器：立即做第一轮，之后每 [`PROBE_INTERVAL`] 一轮；还有上游没有第一次观测时按 [`starting_wait`] 更勤地重试。
/// 没有上游就不起探测循环（仍返回一个立即结束的空任务，保持返回类型一致）。`cancel` 触发后尽快退出。
pub fn spawn_prober(
    context: ProberContext,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    if context.plans.is_empty() {
        return tokio::spawn(async {});
    }
    tokio::spawn(async move {
        let mut starting_rounds = 0_u32;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = context.probe_round() => {}
            }
            let wait = if context.health.any_starting() {
                starting_rounds = starting_rounds.saturating_add(1);
                starting_wait(context.timings.starting_interval, starting_rounds)
            } else {
                starting_rounds = 0;
                context.timings.interval
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use idoris_backend::{
        BackendError, BackendStatus, ChatRequest, ChatResponse, MockAdapter, ModelInfo, Pressure,
        RuntimeAdapter, Supervisor, SupervisorConfig,
    };
    use idoris_contracts::LoadPolicy;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn omlx_card() -> ComponentCard {
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap()
    }

    fn resident_card(id: &str, endpoint: &str, version_pin: &str) -> ComponentCard {
        let mut card = omlx_card();
        card.provider.id = id.to_string();
        card.endpoint = endpoint.to_string();
        card.version_pin = version_pin.to_string();
        card.load_policy = Some(
            serde_json::from_value(
                json!({"mode": "resident", "keepalive": {"pinned": true}, "admission": "coexist"}),
            )
            .unwrap(),
        );
        card
    }

    fn status(id: &str, state: UpstreamState, reason: Option<UpstreamReason>) -> UpstreamStatus {
        UpstreamStatus {
            id: id.to_string(),
            engine_kind: "omlx".to_string(),
            state,
            reason,
            managed: false,
            owned: false,
        }
    }

    /// Codex 预审 M1：有托管上游时 `phase` 只反映托管上游。
    #[test]
    fn phase_reflects_only_the_managed_upstreams_when_there_are_any() {
        use UpstreamState::*;
        // 托管引擎在重启：别的上游就绪不能让 `phase` 成为 `ready`（修复前：Ready）。
        assert_eq!(
            phase_of(&[
                status("cloud", Ready, None),
                managed_status("omlx", Restarting)
            ]),
            Phase::Starting
        );
        // 托管引擎在启动：同上。
        assert_eq!(
            phase_of(&[
                status("cloud", Ready, None),
                managed_status("omlx", Starting)
            ]),
            Phase::Starting
        );
        // 托管引擎就绪：别的上游还没第一次观测也不拉回 `starting`（修复前：Starting）。
        assert_eq!(
            phase_of(&[
                status("cloud", Starting, None),
                managed_status("omlx", Ready)
            ]),
            Phase::Ready
        );
        // 托管的失败与另一个托管上游的启动并存：保持 `degraded`（Agent24 据此有界等待）。
        assert_eq!(
            phase_of(&[managed_status("a", Failed), managed_status("b", Starting)]),
            Phase::Degraded
        );
        assert_eq!(
            phase_of(&[managed_status("a", Failed), managed_status("b", Restarting)]),
            Phase::Degraded
        );
        // 全部托管都在重启：不是 `degraded`（那是终态），是 `starting`。
        assert_eq!(
            phase_of(&[managed_status("a", Restarting)]),
            Phase::Starting
        );
        // 没有托管上游：与 v0.5.0 一致。
        assert_eq!(
            phase_of(&[status("a", Ready, None), status("b", Starting, None)]),
            Phase::Starting
        );
    }

    #[test]
    fn phase_is_starting_while_any_upstream_has_no_first_observation_then_ready_or_degraded() {
        use UpstreamState::*;
        assert_eq!(phase_of(&[]), Phase::Degraded);
        assert_eq!(phase_of(&[status("a", Starting, None)]), Phase::Starting);
        assert_eq!(
            phase_of(&[status("a", Ready, None), status("b", Starting, None)]),
            Phase::Starting
        );
        assert_eq!(
            phase_of(&[status("a", Ready, None), status("b", Unavailable, None)]),
            Phase::Ready
        );
        assert_eq!(
            phase_of(&[
                status("a", Unavailable, Some(UpstreamReason::AuthRequired)),
                status("b", Failed, Some(UpstreamReason::CrashLoop))
            ]),
            Phase::Degraded
        );
        for state in [Restarting, Failed, Stopped, Absent, Unavailable] {
            assert_eq!(phase_of(&[status("a", state, None)]), Phase::Degraded);
        }
    }

    fn managed_status(id: &str, state: UpstreamState) -> UpstreamStatus {
        UpstreamStatus {
            managed: true,
            ..status(id, state, None)
        }
    }

    #[test]
    fn while_the_gate_is_pending_phase_is_starting_unless_a_managed_engine_is_stuck() {
        use UpstreamState::*;
        let health = HealthState::with_upstreams(0, vec![managed_status("omlx", Starting)]);
        assert_eq!(
            health.gate(),
            GateState::NotGated,
            "no managed card: no gate"
        );
        health.set_gate(GateState::Pending);
        // Probing, spawning, waiting for ready, backing off: still in progress.
        for state in [Starting, Restarting] {
            health.set_managed("omlx", state, None, false);
            let view = health.view();
            assert_eq!(
                (view.phase, view.ready),
                (Phase::Starting, false),
                "{state:?}"
            );
        }
        // Terminal states keep the gate from ever completing: degraded, not starting.
        for state in [Absent, Failed, Unavailable, Stopped] {
            health.set_managed("omlx", state, None, false);
            let view = health.view();
            assert_eq!(
                (view.phase, view.ready),
                (Phase::Degraded, false),
                "{state:?}"
            );
        }
        // Even a ready engine is not `ready` until the full router has replaced the starting body.
        health.set_managed("omlx", Ready, None, true);
        let view = health.view();
        assert_eq!((view.phase, view.ready), (Phase::Starting, false));
        health.set_gate(GateState::Open);
        let view = health.view();
        assert_eq!((view.phase, view.ready), (Phase::Ready, true));
    }

    #[test]
    fn a_resident_card_that_probes_ready_does_not_make_a_pending_gate_ready() {
        use UpstreamState::*;
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", Starting),
                status("lmstudio", Ready, None),
            ],
        );
        health.set_gate(GateState::Pending);
        assert_eq!(health.view().phase, Phase::Starting);
        // A terminal state on a card that is not managed does not hold the gate (it is not the manager's).
        health.apply("lmstudio", Observation::Unavailable(None));
        assert_eq!(health.view().phase, Phase::Starting);
        health.set_managed("omlx", Failed, Some(UpstreamReason::CrashLoop), false);
        assert_eq!(health.view().phase, Phase::Degraded);
    }

    #[test]
    fn after_the_gate_opens_phase_follows_the_upstreams_again() {
        use UpstreamState::*;
        let health = HealthState::with_upstreams(0, vec![managed_status("omlx", Ready)]);
        health.set_gate(GateState::Open);
        assert_eq!(health.view().phase, Phase::Ready);
        health.set_managed("omlx", Unavailable, None, true);
        assert_eq!(health.view().phase, Phase::Degraded);
        health.set_managed("omlx", Ready, None, true);
        assert_eq!(health.view().phase, Phase::Ready);
    }

    #[test]
    fn after_the_gate_a_managed_upstream_in_a_terminal_state_is_degraded_even_while_another_upstream_serves()
     {
        // 门控不被一个起不来的托管引擎拖住（降级放行，规范 §2.7.6 勘误六）：别的上游照常服务，但 `phase` 要让界面看出
        // 有一个托管引擎没好：`degraded`，不是 `ready`。
        use UpstreamState::*;
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", Absent),
                status("lmstudio", Ready, None),
            ],
        );
        health.set_gate(GateState::Open);
        for state in [Absent, Failed, Unavailable, Stopped] {
            health.set_managed("omlx", state, None, false);
            let view = health.view();
            assert_eq!(
                (view.phase, view.ready),
                (Phase::Degraded, false),
                "{state:?}"
            );
        }
        // 别的上游观测到了（不再是 `starting`）：托管引擎的终局失败仍是 `degraded`。
        health.apply("lmstudio", Observation::Unavailable(None));
        health.set_managed("omlx", Failed, Some(UpstreamReason::CrashLoop), false);
        assert_eq!(health.view().phase, Phase::Degraded);
        // 托管引擎好了（或者它根本就绪），别的上游照常：`ready`。
        health.apply("lmstudio", Observation::Ready);
        health.set_managed("omlx", Ready, None, true);
        assert_eq!(health.view().phase, Phase::Ready);
    }

    /// IDORIS-CORE S6 (G1): which managed states keep a request away from the engine.
    #[test]
    fn only_a_ready_engine_or_one_that_is_merely_slow_to_answer_is_handed_requests() {
        use UpstreamReason::*;
        use UpstreamState::*;
        assert!(upstream_serves(Ready, None));
        // Alive but the probe did not answer, no definite reason (oMLX busy loading a big model): still served.
        assert!(upstream_serves(Unavailable, None));
        // A definite reason for `unavailable` is a definite failure.
        for reason in [
            AuthRequired,
            AuthFailed,
            FencePending,
            StartupLatched,
            StartupTimeout,
            PortInUse,
        ] {
            assert!(!upstream_serves(Unavailable, Some(reason)), "{reason:?}");
        }
        // Not running, or running a state that has not served yet: never, with or without a reason.
        for state in [Starting, Restarting, Failed, Stopped, Absent] {
            assert!(!upstream_serves(state, None), "{state:?}");
            assert!(!upstream_serves(state, Some(CrashLoop)), "{state:?}");
        }
    }

    /// IDORIS-CORE S6 (Codex audit H1): a card that shares a managed engine's endpoint (the companion card) inherits that
    /// engine's failure; a card that does not share it, and a card whose engine is serving, are untouched.
    #[test]
    fn a_card_that_shares_a_failed_managed_engine_inherits_its_failure_and_an_independent_one_does_not()
     {
        use UpstreamReason::*;
        use UpstreamState::*;
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", Starting),
                status("omlx-embed", Ready, None),
                status("lmstudio", Ready, None),
            ],
        );
        health.register_managed_endpoint("http://127.0.0.1:8088", "omlx");
        let companion = resident_card("omlx-embed", "http://127.0.0.1:8088/", "omlx@0.6.4");
        let independent =
            resident_card("lmstudio", "http://127.0.0.1:1234", "openai_compat@unknown");
        let main = omlx_card();

        // Not serving yet (`starting`): the companion is held back with the managed card's state.
        for card in [&main, &companion] {
            assert_eq!(
                health.failure_of_card(card).map(|failure| failure.state),
                Some(Starting),
                "{}",
                card.provider.id
            );
        }
        assert_eq!(health.failure_of_card(&independent), None);

        // A terminal failure with a reason: the companion reports the same reason (the endpoint match ignores a
        // trailing slash, as the governance table does).
        health.set_managed("omlx", Failed, Some(PortInUse), false);
        let inherited = health.failure_of_card(&companion).unwrap();
        assert_eq!(
            (inherited.state, inherited.reason),
            (Failed, Some(PortInUse))
        );
        assert_eq!(
            health.failure_of_card(&independent),
            None,
            "independent upstreams keep serving"
        );

        // The engine serves: nobody is held back.
        health.set_managed("omlx", Ready, None, true);
        for card in [&main, &companion, &independent] {
            assert_eq!(health.failure_of_card(card), None, "{}", card.provider.id);
        }

        // An engine whose endpoint was never registered shares nothing.
        let unregistered = HealthState::with_upstreams(0, vec![managed_status("omlx", Failed)]);
        assert_eq!(unregistered.failure_of_card(&companion), None);
    }

    #[test]
    fn managed_failure_reads_the_live_snapshot_for_managed_upstreams_only() {
        use UpstreamState::*;
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", Ready),
                status("lmstudio", Ready, None),
            ],
        );
        assert_eq!(health.managed_failure("omlx"), None, "ready: served");
        // The engine fails at run time: the very next read says so (nothing was snapshotted at gate time).
        health.set_managed("omlx", Failed, Some(UpstreamReason::PortInUse), false);
        assert_eq!(
            health.managed_failure("omlx"),
            Some(ManagedFailure {
                engine_kind: "omlx".to_owned(),
                state: Failed,
                reason: Some(UpstreamReason::PortInUse),
            })
        );
        health.set_managed("omlx", Stopped, None, false);
        assert_eq!(
            health.managed_failure("omlx").map(|failure| failure.state),
            Some(Stopped)
        );
        health.set_managed("omlx", Ready, None, true);
        assert_eq!(
            health.managed_failure("omlx"),
            None,
            "and it is served again once ready"
        );
        // Not managed: never (the prober's cards keep their v0.5.0 path), however bad the state; unknown id: none.
        health.apply(
            "lmstudio",
            Observation::Unavailable(Some(UpstreamReason::AuthRequired)),
        );
        assert_eq!(health.managed_failure("lmstudio"), None);
        assert_eq!(health.managed_failure("nope"), None);
    }

    /// 规范勘误十二（4）(a)：门控已开之后，托管上游的终局失败优先于「别的上游还没有第一次观测」。探测器在门控放行之后才起，
    /// 放行那一刻别的上游还是 `starting`；此前这个 `starting` 遮住了托管引擎的 `degraded`，直到别的上游有了第一次观测。
    #[test]
    fn after_the_gate_a_managed_terminal_failure_outranks_another_upstream_that_has_no_first_observation()
     {
        use UpstreamState::*;
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", Absent),
                status("lmstudio", Starting, None),
            ],
        );
        health.set_gate(GateState::Open);
        for state in [Absent, Failed, Unavailable, Stopped] {
            health.set_managed("omlx", state, Some(UpstreamReason::EngineNotFound), false);
            let view = health.view();
            assert_eq!(
                (view.phase, view.ready),
                (Phase::Degraded, false),
                "{state:?}"
            );
            assert_eq!(
                view.upstreams[1].state, Starting,
                "the other upstream really has no observation yet"
            );
        }
        // 同一个判据的纯函数形式，没有门控：有托管上游终态就是 `degraded`，与别的上游是否 `starting` 无关。
        assert_eq!(
            phase_of(&[
                managed_status("omlx", Failed),
                status("lmstudio", Starting, None)
            ]),
            Phase::Degraded
        );
        // 反过来：托管上游没有终局失败时，`phase` 只看托管上游（Codex 预审 M1）：就绪就是 `ready`（别的上游还没有第一次观测不拉回
        // `starting`），启动或重启中就是 `starting`。
        for (state, phase) in [
            (Ready, Phase::Ready),
            (Starting, Phase::Starting),
            (Restarting, Phase::Starting),
        ] {
            assert_eq!(
                phase_of(&[
                    managed_status("omlx", state),
                    status("lmstudio", Starting, None)
                ]),
                phase,
                "{state:?}"
            );
        }
    }

    /// 规范勘误十二（4）(b)：门控已开（或根本没有托管卡）而仍有上游没有第一次观测时，`phase` 是 `starting`，而数据面此刻已经在服务：
    /// `starting` 在这里的意思只是「还有上游没有首次观测」，不是「数据面 503」。取值集合不变（`starting`、`ready`、`degraded`）。
    #[test]
    fn starting_after_the_gate_only_means_an_upstream_has_no_first_observation_yet() {
        use UpstreamState::*;
        // 没有托管卡：没有门控，`phase` 只由上游状态推出，没有观测就是 `starting`。
        let plain = HealthState::with_upstreams(0, vec![status("lmstudio", Starting, None)]);
        assert_eq!(plain.gate(), GateState::NotGated);
        assert_eq!(plain.view().phase, Phase::Starting);
        plain.apply("lmstudio", Observation::Ready);
        assert_eq!(plain.view().phase, Phase::Ready);

        // 有托管卡，门控已开，托管引擎就绪，别的上游刚起探测器：`phase` 只看托管上游，已经是 `ready`（Codex 预审 M1）。
        let managed = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", Ready),
                status("lmstudio", Starting, None),
            ],
        );
        managed.set_gate(GateState::Open);
        let view = managed.view();
        assert_eq!((view.phase, view.ready), (Phase::Ready, true));
        managed.apply("lmstudio", Observation::Ready);
        let view = managed.view();
        assert_eq!((view.phase, view.ready), (Phase::Ready, true));
        // 门控未开时，`starting` 才是「数据面 503」的意思：托管上游的终态此时仍是 `degraded`（`gated_phase` 不变）。
        let pending = HealthState::with_upstreams(0, vec![managed_status("omlx", Failed)]);
        pending.set_gate(GateState::Pending);
        assert_eq!(pending.view().phase, Phase::Degraded);
    }

    #[test]
    fn a_non_managed_upstream_that_is_down_never_degrades_the_phase_while_another_is_ready() {
        // v0.5.0 的口径不变：`managed` 才算；没有托管卡时这一条不起作用。
        use UpstreamState::*;
        let health = HealthState::with_upstreams(
            0,
            vec![status("a", Ready, None), status("b", Unavailable, None)],
        );
        assert_eq!(health.gate(), GateState::NotGated);
        assert_eq!(health.view().phase, Phase::Ready);
        health.set_gate(GateState::Open);
        assert_eq!(health.view().phase, Phase::Ready);
    }

    #[test]
    fn set_managed_writes_state_reason_and_owned_and_ignores_unknown_ids() {
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", UpstreamState::Starting),
                status("other", UpstreamState::Starting, None),
            ],
        );
        health.set_managed(
            "omlx",
            UpstreamState::Restarting,
            Some(UpstreamReason::StartupTimeout),
            true,
        );
        let view = health.view();
        let omlx = &view.upstreams[0];
        assert_eq!(omlx.state, UpstreamState::Restarting);
        assert_eq!(omlx.reason, Some(UpstreamReason::StartupTimeout));
        assert!(omlx.owned);
        assert!(
            omlx.managed,
            "the manager does not change what the card declared"
        );
        // Another upstream is untouched, and so is everything when the id is unknown.
        assert_eq!(view.upstreams[1].state, UpstreamState::Starting);
        health.set_managed("missing", UpstreamState::Failed, None, true);
        assert_eq!(health.view().upstreams.len(), 2);
        // A later state without a reason clears the old one.
        health.set_managed("omlx", UpstreamState::Ready, None, true);
        assert_eq!(health.view().upstreams[0].reason, None);
    }

    #[test]
    fn the_prober_ignores_a_managed_engines_starting_state() {
        let health = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", UpstreamState::Starting),
                status("lmstudio", UpstreamState::Ready, None),
            ],
        );
        assert!(
            !health.any_starting(),
            "the manager's `starting` is not an upstream the prober has yet to observe"
        );
        health.apply("lmstudio", Observation::Unavailable(None));
        let other = HealthState::with_upstreams(
            0,
            vec![
                managed_status("omlx", UpstreamState::Ready),
                status("lmstudio", UpstreamState::Starting, None),
            ],
        );
        assert!(other.any_starting());
    }

    #[test]
    fn the_view_is_internally_consistent_and_registered_defaults_to_false() {
        let health = HealthState::with_upstreams(
            1_763_625_330_123,
            vec![status("a", UpstreamState::Ready, None)],
        );
        let view = health.view();
        assert_eq!(view.phase, Phase::Ready);
        assert!(view.ready);
        assert_eq!(view.started_at_ms, 1_763_625_330_123);
        assert!(!view.registered);
        health.set_registered(true);
        assert!(health.view().registered);
        health.apply("a", Observation::Unavailable(None));
        let view = health.view();
        assert_eq!(view.phase, Phase::Degraded);
        assert!(!view.ready);
    }

    #[test]
    fn apply_sets_and_clears_the_reason_and_no_change_keeps_everything() {
        let health =
            HealthState::with_upstreams(0, vec![status("a", UpstreamState::Starting, None)]);
        health.apply(
            "a",
            Observation::Unavailable(Some(UpstreamReason::AuthRequired)),
        );
        let upstream = &health.view().upstreams[0];
        assert_eq!(upstream.state, UpstreamState::Unavailable);
        assert_eq!(upstream.reason, Some(UpstreamReason::AuthRequired));
        health.apply("a", Observation::NoChange);
        assert_eq!(
            health.view().upstreams[0].reason,
            Some(UpstreamReason::AuthRequired)
        );
        health.apply("a", Observation::Ready);
        let upstream = &health.view().upstreams[0];
        assert_eq!(upstream.state, UpstreamState::Ready);
        assert_eq!(upstream.reason, None);
        // 不认识的 id 不改任何东西，也不 panic。
        health.apply("missing", Observation::Ready);
        assert_eq!(health.view().upstreams.len(), 1);
    }

    #[test]
    fn upstream_status_serializes_to_the_specified_wire_shape() {
        let json = serde_json::to_value(UpstreamStatus {
            id: "omlx".to_string(),
            engine_kind: "omlx".to_string(),
            state: UpstreamState::Unavailable,
            reason: Some(UpstreamReason::StartupLatched),
            managed: true,
            owned: false,
        })
        .unwrap();
        assert_eq!(
            json,
            json!({"id": "omlx", "engine_kind": "omlx", "state": "unavailable",
                   "reason": "startup_latched", "managed": true, "owned": false})
        );
        // `reason` 缺省时整个字段不出现。
        let json = serde_json::to_value(status("a", UpstreamState::Ready, None)).unwrap();
        assert!(json.get("reason").is_none(), "{json}");
        // 全部状态与原因码的线上字符串固定下来（S5 会用到其余的）。
        for (state, expected) in [
            (UpstreamState::Ready, "ready"),
            (UpstreamState::Starting, "starting"),
            (UpstreamState::Unavailable, "unavailable"),
            (UpstreamState::Restarting, "restarting"),
            (UpstreamState::Failed, "failed"),
            (UpstreamState::Stopped, "stopped"),
            (UpstreamState::Absent, "absent"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(expected));
            assert_eq!(
                state.as_str(),
                expected,
                "the log spelling follows the wire"
            );
        }
        for (reason, expected) in [
            (UpstreamReason::EngineNotFound, "engine_not_found"),
            (UpstreamReason::PortInUse, "port_in_use"),
            (UpstreamReason::AuthRequired, "auth_required"),
            (UpstreamReason::AuthFailed, "auth_failed"),
            (UpstreamReason::StartupTimeout, "startup_timeout"),
            (UpstreamReason::CrashLoop, "crash_loop"),
            (UpstreamReason::FencePending, "fence_pending"),
            (UpstreamReason::BudgetZero, "budget_zero"),
            (UpstreamReason::BaseUnavailable, "base_unavailable"),
            (UpstreamReason::StartupLatched, "startup_latched"),
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), json!(expected));
            assert_eq!(
                reason.as_str(),
                expected,
                "the log spelling follows the wire"
            );
        }
    }

    #[test]
    fn plan_lists_only_loopback_http_engines_and_classifies_them() {
        let supervised = omlx_card();
        let resident = resident_card("lmstudio-local", "http://127.0.0.1:1234", "lmstudio@0.3");
        let mut remote = resident_card("cloud", "https://example.invalid", "cloud@1");
        remote.provider.locality = Locality::Remote;
        let mut subscription = resident_card("subscription", "http://127.0.0.1:1", "sub@1");
        subscription.form = Form::SpawnCli;
        let mut subscription_http = resident_card("subscription", "http://127.0.0.1:2", "sub@1");
        subscription_http.form = Form::HttpService;
        let mut non_http = resident_card("bundle", "http://127.0.0.1:3", "bundle@1");
        non_http.form = Form::BundledBinary;

        let plans = plan_upstreams(&[
            supervised,
            resident,
            remote,
            subscription,
            subscription_http,
            non_http,
        ])
        .unwrap();
        let summary = plans
            .iter()
            .map(|plan| {
                (
                    plan.id.as_str(),
                    plan.identity.engine_kind.as_str(),
                    plan.kind,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                ("omlx", "omlx", UpstreamKind::Supervised),
                ("lmstudio-local", "lmstudio", UpstreamKind::Resident),
            ]
        );
        for plan in &plans {
            let initial = plan.initial_status();
            assert_eq!(initial.state, UpstreamState::Starting);
            assert!(initial.reason.is_none() && !initial.owned && !initial.managed);
        }
    }

    #[test]
    fn plan_surfaces_a_bad_managed_declaration_as_a_card_validation_error() {
        let mut card = omlx_card();
        card.extensions = Some(
            json!({"_degradation": "d", "idoris_managed": {"engine": "lmstudio", "mode": "auto"}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let error = plan_upstreams(&[card]).unwrap_err();
        assert!(matches!(
            error,
            EngineKindError::ManagedEngineUnsupported { .. }
        ));
    }

    #[test]
    fn a_remote_card_with_a_bogus_managed_declaration_is_not_an_engine_and_is_ignored() {
        let mut card = resident_card("cloud", "https://example.invalid", "cloud@1");
        card.provider.locality = Locality::Remote;
        card.extensions = Some(
            json!({"_degradation": "d", "idoris_managed": "garbage"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(plan_upstreams(&[card]).unwrap().is_empty());
    }

    async fn models_server(response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    fn client() -> reqwest::Client {
        idoris_upstream::http_client().unwrap()
    }

    #[tokio::test]
    async fn probe_reports_ready_only_for_a_200_with_a_data_array_even_when_empty() {
        for body in [
            json!({"data": []}),
            json!({"object": "list", "data": [{"id": "m"}]}),
        ] {
            let server = models_server(ResponseTemplate::new(200).set_body_json(body)).await;
            let card = resident_card("r", &server.uri(), "fixture@1");
            assert_eq!(
                probe_resident(&client(), &card, Duration::from_secs(2)).await,
                Observation::Ready
            );
        }
        for response in [
            ResponseTemplate::new(200).set_body_json(json!({"object": "list"})),
            ResponseTemplate::new(200).set_body_json(json!({"data": "not an array"})),
            ResponseTemplate::new(200).set_body_string("not json"),
            ResponseTemplate::new(200).set_body_json(json!([])),
            ResponseTemplate::new(500),
            ResponseTemplate::new(404),
            ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1:1/"),
        ] {
            let server = models_server(response).await;
            let card = resident_card("r", &server.uri(), "fixture@1");
            assert_eq!(
                probe_resident(&client(), &card, Duration::from_secs(2)).await,
                Observation::Unavailable(None)
            );
        }
    }

    #[tokio::test]
    async fn probe_of_an_unreachable_or_slow_upstream_is_unavailable_without_a_reason() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let card = resident_card("r", &format!("http://127.0.0.1:{port}"), "fixture@1");
        assert_eq!(
            probe_resident(&client(), &card, Duration::from_secs(2)).await,
            Observation::Unavailable(None)
        );

        let slow = models_server(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": []}))
                .set_delay(Duration::from_secs(5)),
        )
        .await;
        let card = resident_card("r", &slow.uri(), "fixture@1");
        let started = std::time::Instant::now();
        assert_eq!(
            probe_resident(&client(), &card, Duration::from_millis(100)).await,
            Observation::Unavailable(None)
        );
        // 在探测时限（100 毫秒）就返回，没有等完服务器的 5 秒延迟；阈值留足调度余量。
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn probe_never_sends_a_request_to_an_endpoint_that_is_not_a_loopback_literal() {
        // 一个不是 loopback 的 IPv4 映射写法（`::ffff:8.8.8.8`）：探测器必须一个请求都不发（出站纪律：宁可报 unavailable，也不对「不是本机」的地址发流量）。
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        let endpoint = format!("http://[::ffff:8.8.8.8]:{}", server.address().port());
        assert!(!idoris_upstream::omlx::key::is_loopback_base_url(&endpoint));
        let card = resident_card("r", &endpoint, "fixture@1");
        assert_eq!(
            probe_resident(&client(), &card, Duration::from_millis(500)).await,
            Observation::Unavailable(None)
        );
        server.verify().await;
    }

    /// 登记表把 IPv4 映射的写法（`::ffff:127.0.0.1`）并进同一个端口，key 的资格与「是不是 loopback」也这样认（Codex 预审 L2 的 H1：此前它们对同一个
    /// 套接字各说各的）。所以映射写法的卡落在一个封上的、被管理的端口上，探测器**一个请求都不发**，不管这张卡能不能带 key。
    #[tokio::test]
    async fn probe_of_an_ipv4_mapped_loopback_card_on_a_blocked_governed_port_sends_nothing() {
        use idoris_upstream::omlx::key::{block_endpoint, forget_endpoint};

        struct Governed(String);
        impl Drop for Governed {
            fn drop(&mut self) {
                forget_endpoint(&self.0);
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        let canonical = server.uri();
        let endpoint = format!("http://[::ffff:127.0.0.1]:{}", server.address().port());
        assert!(
            idoris_upstream::omlx::key::is_loopback_base_url(&endpoint),
            "the same socket as {canonical}"
        );
        let _governed = Governed(canonical.clone());
        block_endpoint(&canonical);
        // A card that may carry the key, and one that may not: both are the managed port as far as the registry is concerned.
        for pin in ["omlx@0.6.4", "fixture@1"] {
            let card = resident_card("r", &endpoint, pin);
            let admitted = crate::models::admission_generation(&card);
            assert!(
                admitted.is_some(),
                "{pin}: bound to the governed endpoint's generation"
            );
            assert_eq!(
                probe_resident_admitted(&client(), &card, admitted, Duration::from_millis(500))
                    .await,
                Observation::Unavailable(None),
                "{pin}"
            );
        }
        server.verify().await;
    }

    /// Codex 预审 L2 的 H1：不能带 key 的卡（别的产品名、`idoris_upstream`）发往一个被管理的端点，探测与别的发送走同一道授权：封上的端点一个请求都不发；放开之后
    /// 请求发出，**不带** bearer（登记的私有 key 不是这张卡的）。
    #[tokio::test]
    async fn probe_of_a_card_that_may_not_carry_the_key_is_gated_by_its_governed_endpoint_and_sends_no_key()
     {
        use idoris_upstream::omlx::key::{
            EndpointKey, block_endpoint, forget_endpoint, govern_endpoint,
        };

        struct Governed(String);
        impl Drop for Governed {
            fn drop(&mut self) {
                forget_endpoint(&self.0);
            }
        }

        let server =
            models_server(ResponseTemplate::new(200).set_body_json(json!({"data": []}))).await;
        let endpoint = server.uri();
        let _governed = Governed(endpoint.clone());
        let card = resident_card("lm", &endpoint, "lmstudio@1");
        block_endpoint(&endpoint);
        assert_eq!(
            probe_resident_admitted(
                &client(),
                &card,
                crate::models::admission_generation(&card),
                Duration::from_secs(2)
            )
            .await,
            Observation::Unavailable(None)
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a probe went to a blocked governed endpoint"
        );

        // Registered again (a replacement verified): the probe goes out, without any credential.
        govern_endpoint(&endpoint, EndpointKey::Private("engine-key".to_owned()));
        assert_eq!(
            probe_resident(&client(), &card, Duration::from_secs(2)).await,
            Observation::Ready
        );
        let seen = server.received_requests().await.unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].headers.get("authorization").is_none(),
            "the engine's private key went to a card that may not carry it"
        );
    }

    /// 复审 L2 终检：resident 卡的就绪探测，请求**建好之后、最后一眼之前**换掉引擎：不发，这一轮的观测是「不可用」。
    #[tokio::test]
    async fn a_probe_built_for_a_replaced_engine_is_not_sent() {
        use idoris_upstream::omlx::key::test_hooks::between_build_and_send_for;
        use idoris_upstream::omlx::key::{EndpointKey, forget_endpoint, govern_endpoint};

        struct Governed(String);
        impl Drop for Governed {
            fn drop(&mut self) {
                forget_endpoint(&self.0);
            }
        }

        let server =
            models_server(ResponseTemplate::new(200).set_body_json(json!({"data": []}))).await;
        let endpoint = server.uri();
        let _governed = Governed(endpoint.clone());
        govern_endpoint(&endpoint, EndpointKey::Private("first-engine".to_owned()));
        let card = resident_card("omlx-card", &endpoint, "omlx@0.6.4");
        let admitted = crate::models::admission_generation(&card);
        assert!(admitted.is_some());

        // 对照：没有换引擎，探测发得出去。
        assert_eq!(
            probe_resident_admitted(&client(), &card, admitted, Duration::from_secs(2)).await,
            Observation::Ready
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        let _hook = between_build_and_send_for(&format!("{endpoint}/v1/models"), {
            let endpoint = endpoint.clone();
            move || {
                govern_endpoint(&endpoint, EndpointKey::Private("replacement".to_owned()));
            }
        });
        assert_eq!(
            probe_resident_admitted(&client(), &card, admitted, Duration::from_secs(2)).await,
            Observation::Unavailable(None)
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the probe built for the replaced engine was sent"
        );
    }

    #[tokio::test]
    async fn probe_maps_401_and_403_to_auth_required_when_no_key_was_sent() {
        // 非 oMLX 的卡永远不带 key，所以 401/403 是 `auth_required`。「oMLX 卡、而且哪里都没有 key」这个组合取决于
        // 环境（`resolve_omlx_api_key_os` 还会读 `~/.omlx/settings.json`），在不密封的单元测试里固定不了，这里不测。
        for status_code in [401, 403] {
            let server = models_server(ResponseTemplate::new(status_code)).await;
            let card = resident_card("lm", &server.uri(), "lmstudio@1");
            assert_eq!(
                probe_resident(&client(), &card, Duration::from_secs(2)).await,
                Observation::Unavailable(Some(UpstreamReason::AuthRequired))
            );
        }
    }

    #[tokio::test]
    async fn a_non_omlx_card_never_receives_the_omlx_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer should-never-be-sent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;
        let card = resident_card("lm", &server.uri(), "lmstudio@1");
        assert_eq!(
            probe_resident(&client(), &card, Duration::from_secs(2)).await,
            Observation::Ready
        );
        server.verify().await;
    }

    struct FailingStatusAdapter;

    #[async_trait::async_trait]
    impl RuntimeAdapter for FailingStatusAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(idoris_backend::mock::temporary_load_fence_path())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Err(BackendError::internal("list"))
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Err(BackendError::internal("load"))
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            Err(BackendError::internal("unload"))
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            Err(BackendError::internal("engine unreachable at startup"))
        }
        async fn probe_ready(&self, _id: &str) -> Result<bool, BackendError> {
            Ok(false)
        }
        async fn chat(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, BackendError> {
            Err(BackendError::internal("chat"))
        }
    }

    fn registry_with(card: &ComponentCard, adapter: Arc<dyn RuntimeAdapter>) -> RuntimeRegistry {
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        RuntimeRegistry::from_supervisors([crate::dispatch::BoundSupervisor::new(card, handle)])
    }

    fn prober_context(
        plans: Vec<UpstreamPlan>,
        runtimes: RuntimeRegistry,
        kill_switch: bool,
    ) -> ProberContext {
        let health = HealthState::with_upstreams(
            1,
            plans.iter().map(UpstreamPlan::initial_status).collect(),
        );
        ProberContext {
            health,
            plans,
            runtimes,
            client: client(),
            remote_kill_switch: Arc::new(AtomicBool::new(kill_switch)),
            timings: ProberTimings {
                interval: Duration::from_millis(50),
                starting_interval: Duration::from_millis(10),
                probe_timeout: Duration::from_millis(500),
                supervisor_timeout: Duration::from_millis(500),
            },
        }
    }

    #[tokio::test]
    async fn a_supervised_upstream_is_ready_when_the_supervisor_answers() {
        let card = omlx_card();
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "m".to_string(),
            memory_gb: 1.0,
        }]));
        adapter.set_pressure(Pressure::Ok).unwrap();
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            registry_with(&card, adapter),
            false,
        );
        context.probe_round().await;
        let view = context.health.view();
        assert_eq!(view.upstreams[0].state, UpstreamState::Ready);
        assert_eq!(view.upstreams[0].reason, None);
        assert_eq!(view.phase, Phase::Ready);
        assert!(view.ready);
    }

    #[tokio::test]
    async fn a_supervisor_that_latched_a_startup_error_is_unavailable_never_ready() {
        let card = omlx_card();
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            registry_with(&card, Arc::new(FailingStatusAdapter)),
            false,
        );
        // 启动采样是异步的：第一轮可能还没有应答（保持 starting），等到应答为止。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            context.probe_round().await;
            if context.health.view().upstreams[0].state != UpstreamState::Starting {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "supervisor never answered"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let view = context.health.view();
        assert_eq!(view.upstreams[0].state, UpstreamState::Unavailable);
        assert_eq!(
            view.upstreams[0].reason,
            Some(UpstreamReason::StartupLatched)
        );
        assert_eq!(view.phase, Phase::Degraded);
        assert!(!view.ready);
    }

    struct SlowStartupAdapter;

    #[async_trait::async_trait]
    impl RuntimeAdapter for SlowStartupAdapter {
        fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
            Ok(idoris_backend::mock::temporary_load_fence_path())
        }
        async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
            Ok(Vec::new())
        }
        async fn load(&self, _id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
            Ok(())
        }
        async fn unload(&self, _id: &str) -> Result<(), BackendError> {
            Ok(())
        }
        async fn status(&self) -> Result<BackendStatus, BackendError> {
            // Supervisor 启动时采样一次引擎状态；采样没完成之前 actor 不接收任何命令。
            tokio::time::sleep(Duration::from_secs(3)).await;
            Err(BackendError::internal("too late"))
        }
        async fn probe_ready(&self, _id: &str) -> Result<bool, BackendError> {
            Ok(false)
        }
        async fn chat(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<ChatResponse, BackendError> {
            Err(BackendError::internal("chat"))
        }
    }

    #[tokio::test]
    async fn a_supervisor_still_sampling_its_engine_keeps_the_upstream_starting() {
        let card = omlx_card();
        let mut context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            registry_with(&card, Arc::new(SlowStartupAdapter)),
            false,
        );
        context.timings.supervisor_timeout = Duration::from_millis(50);
        assert_eq!(
            context.observe(&context.plans[0]).await,
            Observation::NoChange
        );
        // 没有新信息：快照里仍是 starting，phase 仍是 starting。
        context.probe_round().await;
        let view = context.health.view();
        assert_eq!(view.upstreams[0].state, UpstreamState::Starting);
        assert_eq!(view.phase, Phase::Starting);
        assert!(!view.ready);
        assert!(context.health.any_starting());
    }

    #[tokio::test]
    async fn a_supervised_upstream_without_a_bound_supervisor_is_unavailable() {
        let card = omlx_card();
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            RuntimeRegistry::default(),
            false,
        );
        assert_eq!(
            context.observe(&context.plans[0]).await,
            Observation::Unavailable(None)
        );
    }

    #[tokio::test]
    async fn the_kill_switch_stops_every_probe_of_an_untrusted_card_and_reports_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        let mut card = resident_card("lan-ish", &server.uri(), "lmstudio@1");
        // 可信本地三条件之一不满足（隐私等级不是 local_only）：熔断闭合时不探测。
        card.privacy_class = idoris_contracts::common::PrivacyClass::Any;
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            RuntimeRegistry::default(),
            true,
        );
        context.probe_round().await;
        assert_eq!(
            context.health.view().upstreams[0].state,
            UpstreamState::Unavailable
        );
        server.verify().await;

        // 熔断释放后同一张卡恢复探测。
        let server =
            models_server(ResponseTemplate::new(200).set_body_json(json!({"data": []}))).await;
        let mut card = resident_card("lan-ish", &server.uri(), "lmstudio@1");
        card.privacy_class = idoris_contracts::common::PrivacyClass::Any;
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            RuntimeRegistry::default(),
            true,
        );
        context.remote_kill_switch.store(false, Ordering::SeqCst);
        context.probe_round().await;
        assert_eq!(
            context.health.view().upstreams[0].state,
            UpstreamState::Ready
        );
    }

    #[tokio::test]
    async fn the_kill_switch_does_not_stop_probes_of_a_trusted_local_card() {
        let server =
            models_server(ResponseTemplate::new(200).set_body_json(json!({"data": []}))).await;
        let card = resident_card("local", &server.uri(), "lmstudio@1");
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            RuntimeRegistry::default(),
            true,
        );
        context.probe_round().await;
        assert_eq!(
            context.health.view().upstreams[0].state,
            UpstreamState::Ready
        );
    }

    #[tokio::test]
    async fn the_prober_converges_quickly_at_startup_follows_the_upstream_and_stops_on_cancel() {
        let server = MockServer::start().await;
        let flaky = ResponseTemplate::new(500);
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(flaky)
            .mount(&server)
            .await;
        let card = resident_card("lm", &server.uri(), "lmstudio@1");
        let context = prober_context(
            plan_upstreams(std::slice::from_ref(&card)).unwrap(),
            RuntimeRegistry::default(),
            false,
        );
        let health = context.health.clone();
        assert_eq!(health.view().phase, Phase::Starting);
        let cancel = CancellationToken::new();
        let handle = spawn_prober(context, cancel.clone());

        wait_for(&health, |view| view.phase == Phase::Degraded).await;
        // 上游恢复后，下一轮把它带回 ready。
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;
        wait_for(&health, |view| view.phase == Phase::Ready).await;

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    async fn wait_for(health: &HealthState, done: impl Fn(&HealthView) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(&health.view()) {
            assert!(
                std::time::Instant::now() < deadline,
                "condition not reached: {:?}",
                health.view()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn no_upstreams_means_no_probe_loop_and_a_degraded_phase() {
        let context = prober_context(Vec::new(), RuntimeRegistry::default(), false);
        let handle = spawn_prober(context.clone(), CancellationToken::new());
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.health.view().phase, Phase::Degraded);
    }

    #[test]
    fn the_startup_retry_interval_backs_off_to_four_times_the_base_and_stays_there() {
        let base = STARTING_RETRY_INTERVAL;
        let waits = (1..=6)
            .map(|round| starting_wait(base, round))
            .collect::<Vec<_>>();
        assert_eq!(
            waits,
            [250, 500, 1000, 1000, 1000, 1000].map(Duration::from_millis)
        );
        // 第 0 轮（还没开始重试）等同第 1 轮，不会下溢。
        assert_eq!(starting_wait(base, 0), base);
        // 一直卡 30 秒，追加给 Supervisor（邮箱容量 64）的状态消息也少于容量（约一半，所以下面断言 < 40）。
        let mut elapsed = Duration::ZERO;
        let mut sent = 1;
        let mut round = 1;
        while elapsed < Duration::from_secs(30) {
            elapsed += starting_wait(base, round);
            round += 1;
            sent += 1;
        }
        assert!(sent < 40, "{sent}");
    }

    #[test]
    fn unix_millis_now_is_a_plausible_wall_clock_in_milliseconds() {
        let now = unix_millis_now();
        // 2025-01-01 之后、2100-01-01 之前。
        assert!(now > 1_735_689_600_000 && now < 4_102_444_800_000, "{now}");
    }
}
