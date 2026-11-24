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
//! 任一上游还是 `starting`（没有第一次观测）→ `starting`；否则至少一个 `ready` → `ready`；否则 `degraded`。
//! S1 没有引擎管理（那是 S5 的启动门控）：`starting` 持续到每个上游都有了第一次观测为止。resident 卡通常一轮探测
//! 就有结论；Supervisor 管理的卡在它还在做启动采样时，状态查询超时按「没有新信息」处理、保持 `starting`，所以会更久
//! （单次调用上限是 `adapter_call_timeout`，默认 30 秒）。期间数据面照常服务（没有托管卡时行为不变，规范 §2.7.6）。
//! `status` 仍恒为 `"ok"`。

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
/// 没有托管上游时这一条不起作用（`managed` 恒为 false），行为与 v0.5.0 一致。
pub fn phase_of(upstreams: &[UpstreamStatus]) -> Phase {
    if upstreams
        .iter()
        .any(|upstream| upstream.state == UpstreamState::Starting)
    {
        Phase::Starting
    } else if upstreams
        .iter()
        .any(|upstream| upstream.managed && is_terminal(upstream.state))
    {
        Phase::Degraded
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
    // 持有到响应读完：端点上的一个在途操作。
    let authorization = if crate::models::is_loopback_omlx(card) {
        let Ok(authorization) =
            idoris_upstream::omlx::key::authorize_omlx_request(&card.endpoint, admitted_generation)
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
    let _authorization = if let Some(authorization) = authorization {
        if let Some(key) = authorization.key_os().filter(|key| !key.is_empty()) {
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
    let Ok(response) = request.send().await else {
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
        // 一个上游还没有第一次观测（探测器刚起）：还是 `starting`。
        health.apply("lmstudio", Observation::Unavailable(None));
        health.set_managed("omlx", Failed, Some(UpstreamReason::CrashLoop), false);
        assert_eq!(health.view().phase, Phase::Degraded);
        // 托管引擎好了（或者它根本就绪），别的上游照常：`ready`。
        health.apply("lmstudio", Observation::Ready);
        health.set_managed("omlx", Ready, None, true);
        assert_eq!(health.view().phase, Phase::Ready);
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
        // 监听在 127.0.0.1 的服务器；卡的 endpoint 写成 IPv4-mapped IPv6 字面量，`is_loopback_base_url` 不认它是
        // loopback：探测器必须一个请求都不发（出站纪律：宁可报 unavailable，也不对「不确定是不是本机」的地址发流量）。
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .expect(0)
            .mount(&server)
            .await;
        let endpoint = format!("http://[::ffff:127.0.0.1]:{}", server.address().port());
        assert!(!idoris_upstream::omlx::key::is_loopback_base_url(&endpoint));
        let card = resident_card("r", &endpoint, "fixture@1");
        assert_eq!(
            probe_resident(&client(), &card, Duration::from_millis(500)).await,
            Observation::Unavailable(None)
        );
        server.verify().await;
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
