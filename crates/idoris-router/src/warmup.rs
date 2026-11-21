//! `POST /v1/models/warmup`（IDORIS-CORE S8；接口规范 §2.11.2，修订 1.6 勘误八）：让一个模型进入「可用」状态，
//! 之后第一次真正的请求不用再等加载。数据面、数据 key（不是 Admin）。
//!
//! - **鉴权与 key 范围同聊天**：聊天在该模型上会被拒的 key，预热也被拒（`auth::enforce_scope`，与 embeddings 同一个入口
//!   `enforce_protocol_key_scope`）。
//! - **选卡同聊天**（同一套 `policy_cards` + `select_with_gate_fallback`，含角色绑定、隐私、熔断的就绪快照），另有三条：
//!   订阅中转卡不参与（没有「加载」可言）；请求的能力由角色定（`embed` 是 embedding，`rerank` 是 rerank，其余是 chat）；
//!   单价非零的卡拒绝（预热只服务本地免费卡）。
//! - **生命周期卡（oMLX 等）**：走 [`dispatch::ensure_loaded`]——和聊天的按需加载是同一段代码（读 catalog、拒未知模型、
//!   Supervisor 的幂等 load：余量确认与驱逐都在里面）；加载完成即返回，**不生成任何 token**。三种角色（chat、embedding、
//!   rerank）都一样，因为「就绪」就是「已加载」。同一模型的并发预热与并发的首个聊天请求由 Supervisor 合并成一次加载。
//! - **resident 卡**：没有生命周期可管，按能力发一个最小的真实请求（chat：`max_tokens: 1`；embedding：输入 `"warmup"`），
//!   请求在自己的任务里跑、有自己的响应头期限（[`crate::proxy::WARMUP_HEADER_TIMEOUT`]，不是聊天的 10 秒，也不是 `timeout_ms`）。
//!   规范没有定义 rerank 的最小请求，rerank 的 resident 卡明确拒绝（400 `unsupported_field` / `unsupported_model`），什么也不发。
//!   请求占着代理的一个并发许可（共 32 个，和聊天共用）直到结束，所以有两道限制（[`Registry`]）：同一模型的并发预热合并成一个请求，
//!   每把数据 key 自己发起的在途请求有上限（429 `warmup_in_flight_limited`）。
//! - **`timeout_ms` 到点只是放弃等待**：返回 504 `warmup_timeout`，**不取消、不缩短**任何东西。生命周期卡：Supervisor 的加载任务
//!   与请求的 future 无关，照常跑完并清掉 `load.pending`，之后的预热或聊天直接看到「已就绪」。resident 卡：最小请求在它自己的
//!   任务里，不被打断，直到上游答了或它自己的期限到点，代理照常结算熔断的准入（504 按既有规则计入提供方熔断）。客户端断开同理。
//!   预热自己的 `timeout_ms` 与客户端断开不留围栏；引擎侧的超时（`RuntimeAdapter::load_timeout`，生产缺省 180 秒）或 panic 保持
//!   既有的「结果未知」保护，围栏保留（行为同聊天，没有变）。
//! - **预算**：接了账本的部署里，`SpendGate::All` 下余额不足的租户聊天是 402，预热也一样（一次只读的 `check_admission`，不预留、
//!   不结算），免费调用在默认的 `PaidOnly` 下放行。
//! - 不进用量记录、不占用预算（本地免费；付费卡直接拒绝），写一条 `intent: warmup` 的审计事件（见 `record_id_middleware`）。请求被取消
//!   （客户端断开）时中间件走不到那里，由 [`CancelAudit`] 补上：完成、`timeout_ms` 到点、取消各写恰好一条。

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use idoris_backend::BackendError;
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_contracts::provider::Locality;
use idoris_policy::Role;
use idoris_tenancy::virtual_key::store::AuthenticatedVirtualKey;
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::dispatch::{
    self, ChatOutcome, DispatchError, DispatchFailure, EnsureLoadedError, Selected,
};
use crate::profile::parse_profile;
use crate::proxy::{BufferedUpstreamPath, ChatProxy, ForwardOpts, ForwardOutcome};
use crate::{
    AppState, BufferedAuditMeta, RequestRecordId, apply_selected_headers, backend_error_response,
    budget, budget_error_response, error_envelope, error_envelope_with_reason,
    headroom_breach_confirmed, managed_unavailable_response, models,
};

/// 路径（`record_id_middleware` 与鉴权中间件的路径名单也用它）。
pub(crate) const WARMUP_PATH: &str = "/v1/models/warmup";
/// 审计事件的 `intent`。
pub(crate) const AUDIT_INTENT: &str = "warmup";

/// `timeout_ms` 的范围与缺省（毫秒）。
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// resident 卡的最小请求里那一句话 / 那一个输入。
const MINIMAL_INPUT: &str = "warmup";

/// 请求体：只有 `model`（必填）与 `timeout_ms`（可选）；别的字段一律拒绝（G-6「不静默」）。
#[derive(Debug)]
struct WarmupRequest {
    model: String,
    timeout: Duration,
}

/// 请求体里一个不被接受的字段：`error.type` 恒为 `unsupported_field`，`reason_code` 说明是哪个字段、怎么不对
/// （`unsupported_parameter`：多出来的字段；`unsupported_model`：`model` 缺失、不是非空字符串，或是没有最小请求的角色；
/// `invalid_timeout_ms`：`timeout_ms` 不是范围内的整数）。
#[derive(Debug, PartialEq, Eq)]
struct BadField {
    reason_code: &'static str,
    remediation: String,
}

impl BadField {
    fn new(reason_code: &'static str, remediation: impl Into<String>) -> Self {
        Self {
            reason_code,
            remediation: remediation.into(),
        }
    }

    fn into_response(self) -> Response {
        error_envelope_with_reason(
            StatusCode::BAD_REQUEST,
            "unsupported_field",
            self.reason_code,
            self.remediation,
        )
    }
}

/// 把调用方给的字段名放进错误信息之前：截断、控制字符换成 `U+FFFD`（同 `RoleParseError`，防日志注入）。
fn printable(text: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut shown: String = text
        .chars()
        .take(MAX_CHARS)
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    if text.chars().count() > MAX_CHARS {
        shown.push('…');
    }
    shown
}

fn parse_request(object: &Map<String, Value>) -> Result<WarmupRequest, BadField> {
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "model" | "timeout_ms"))
    {
        return Err(BadField::new(
            "unsupported_parameter",
            format!(
                "unknown field \"{}\": {WARMUP_PATH} accepts only model and timeout_ms",
                printable(unknown)
            ),
        ));
    }
    let model = match object.get("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => model.clone(),
        _ => {
            return Err(BadField::new(
                "unsupported_model",
                "model is required and must be a non-empty string: a concrete model id or an idoris/<role> alias",
            ));
        }
    };
    let timeout_ms = match object.get("timeout_ms") {
        None => DEFAULT_TIMEOUT_MS,
        // `as_u64` is `None` for a float, a negative number, a string and `null`: the value must be an integer.
        Some(value) => match value
            .as_u64()
            .filter(|ms| (MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(ms))
        {
            Some(ms) => ms,
            None => {
                return Err(BadField::new(
                    "invalid_timeout_ms",
                    format!(
                        "timeout_ms must be an integer between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} (default {DEFAULT_TIMEOUT_MS})"
                    ),
                ));
            }
        },
    };
    Ok(WarmupRequest {
        model,
        timeout: Duration::from_millis(timeout_ms),
    })
}

/// 预热哪种能力：`embed` 角色是 embedding，`rerank` 角色是 rerank，其余（含没有角色的具体模型 id）是 chat，与聊天的缺省选卡
/// 一致。生命周期卡只需要加载，能力只决定哪些卡有资格；resident 卡要发请求，规范只定义了 chat 与 embedding 的最小请求，
/// 别的能力在 [`warm_resident`] 里明确拒绝，不静默当成 chat。
fn warm_capability(role: Option<Role>) -> Capability {
    match role {
        Some(Role::Embed) => Capability::Embedding,
        Some(Role::Rerank) => Capability::Rerank,
        _ => Capability::Chat,
    }
}

/// 选定候选之后的失败都带上候选的 served-locality / reason 头（同聊天）。
fn with_headers(mut response: Response, selected: &Selected) -> Response {
    apply_selected_headers(&mut response, selected.served_locality, &selected.decision);
    response
}

/// 一个后端错误，沿用聊天的信封与代码（`backend_error_response`：`headroom_confirmation_required`、`local_only_unavailable`
/// 加后端自己的 `reason_code`，例如 `supervisor_busy`），只有「模型不存在」按规范改成 404 `model_not_found`。
fn backend_failure(selected: &Selected, err: BackendError) -> Response {
    if let BackendError::ModelNotFound { model_id } = &err {
        return with_headers(
            error_envelope(
                StatusCode::NOT_FOUND,
                "model_not_found",
                format!(
                    "model \"{}\" is not known to the engine behind this card",
                    printable(model_id)
                ),
            ),
            selected,
        );
    }
    let outcome = ChatOutcome {
        decision: selected.decision.clone(),
        served_locality: selected.served_locality,
        result: Err(DispatchFailure::Backend(err.clone())),
        actual_cost_minor: None,
    };
    backend_error_response(&err, &outcome)
}

/// 等了 `waited` 模型还没就绪：504，类型与原因码都是 `warmup_timeout`。通常是调用方的 `timeout_ms` 到点；resident 卡的最小请求
/// 自己的期限（[`crate::proxy::WARMUP_HEADER_TIMEOUT`]）先到点时 `waited` 是那一段实际等待。引擎那一侧可能仍在加载，不等于失败。
fn timeout_response(selected: &Selected, waited: Duration) -> Response {
    with_headers(
        error_envelope(
            StatusCode::GATEWAY_TIMEOUT,
            "warmup_timeout",
            format!(
                "the model was not ready within {} ms; the engine may still be loading it, so this is not a failure: retry the warmup later",
                waited.as_millis()
            ),
        ),
        selected,
    )
}

/// 生命周期卡：取 Supervisor，让模型就绪。返回 `already_loaded`：调用时 Supervisor 的账本里它已经是就绪的。
async fn warm_lifecycle(
    state: &AppState,
    selected: &Selected,
    target: &str,
    headroom_confirmed: bool,
) -> Result<bool, Response> {
    let provider_id = selected.card.provider.id.as_str();
    // 没有这张卡的 Supervisor（或绑定的不是这张卡）：和聊天一样，失败即关闭。
    let Some(bound) = state
        .runtimes
        .get(provider_id)
        .filter(|bound| bound.matches(&selected.card))
    else {
        return Err(backend_failure(
            selected,
            BackendError::supervisor_unavailable(),
        ));
    };
    // 只是给调用方的信息：`false` 时照常走加载，Supervisor 不可用之类的真错误由 `ensure_loaded` 自己报出来。
    let already_loaded = bound.is_ready(target).await.unwrap_or(false);
    dispatch::ensure_loaded(
        bound,
        provider_id,
        target,
        selected.load_policy,
        headroom_confirmed,
    )
    .await
    .map(|()| already_loaded)
    .map_err(|failure: EnsureLoadedError| backend_failure(selected, failure.into_backend()))
}

/// 一次预热里与具体卡无关的那些：调用方是谁、预热哪种能力、预热哪个具体模型，以及这次预热从什么时候开始等。
struct Job<'a> {
    privacy: PrivacyClass,
    tenant_id: Option<&'a str>,
    /// 数据 key 的 id；没有认证（开发的无 key 模式）时是 `None`，那一个调用方算一个整体。
    caller: Option<&'a str>,
    record_id: &'a str,
    capability: Capability,
    target: &'a str,
    started: Instant,
}

// ---------------------------------------------------------------------------------------------
// 同一模型的并发预热合并，与每把数据 key 的在途上限
// ---------------------------------------------------------------------------------------------

/// 每把数据 key 此刻自己发起的 resident 预热请求数的上限（见 [`Registry`]）。2 个够 Agent24 同时预热一个 chat 模型与一个
/// embedding 模型；再多的不同模型要等一个结束。
const MAX_IN_FLIGHT_PER_KEY: usize = 2;

/// 超过 [`MAX_IN_FLIGHT_PER_KEY`] 时的错误类型与原因码（429）。
const LIMITED: &str = "warmup_in_flight_limited";

/// 一个 resident 最小请求在合并表里的身份：同一张卡（id 与端点）、同一种最小请求、同一个模型。
#[derive(Clone, PartialEq, Eq, Hash)]
struct RequestKey {
    provider_id: String,
    endpoint: String,
    /// `"chat"` 或 `"embedding"`：同一个模型的两种最小请求是两个请求。
    kind: &'static str,
    model: String,
}

/// 一个 resident 最小请求的结果，摘出预热需要的部分：状态码，以及非 2xx 时要原样透传的类型与正文。
///
/// 正文是**复制**出来的：代理给结果的正文挂着并发许可（结果被丢掉才还），而这个结果要在合并的几个预热之间共享，许可不能跟着
/// 它们活。2xx 的正文预热用不到，不留。
struct Answer {
    status: u16,
    content_type: Option<String>,
    body: Bytes,
}

impl Answer {
    /// 吃掉代理的结果：函数结束时那个结果被丢掉，并发许可就在这里还。
    fn from_outcome(outcome: ForwardOutcome) -> Self {
        let body = if (200..300).contains(&outcome.status) {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(&outcome.body)
        };
        Self {
            status: outcome.status,
            content_type: outcome.content_type.clone(),
            body,
        }
    }
}

type Shared = watch::Receiver<Option<Arc<Answer>>>;

/// 等一个在途请求的结果。
struct Waiting(Shared);

impl Waiting {
    /// `None`：请求的任务没有留下结果就没了（panic，或运行时在关闭）。
    async fn answer(mut self) -> Option<Arc<Answer>> {
        let answered = self.0.wait_for(Option::is_some).await.ok()?;
        answered.clone()
    }
}

/// 一个代理上在途的 resident 预热请求，与每把数据 key 此刻自己发起了几个。
///
/// 为什么需要（PR-Daemon 对 #634 的预审 W-1）：resident 预热的请求在自己的任务里占着代理的一个并发许可，最长 180 秒加正文，
/// 调用方却早就拿到了 504；一把数据 key 凑满 32 个并发预热就占满全部许可，别的租户的聊天得 503。两道限制：
///
/// 1. **合并**：同一（卡、端点、最小请求、模型）已经有请求在途时，后来的预热并入它——不再发请求、不占许可，等同一个结果。
/// 2. **每 key 上限**：一把 key 自己发起（领头）的在途请求最多 [`MAX_IN_FLIGHT_PER_KEY`] 个，再多的 429 [`LIMITED`]。并入别人的
///    请求不算（它不占许可）。名额由请求的任务持有，**请求真正结束才还**：调用方的 `timeout_ms` 到点、客户端断开都不还，所以
///    一把 key 不论超时多少次，最多只占 N 个许可。
///
/// 只管 resident 卡：生命周期卡的预热不走代理的许可（加载在 Supervisor 里，同一模型本来就合并，别的模型 `supervisor_busy`）。
/// 没有认证的调用方（开发的无 key 模式，只在本机回环）算一个整体。每个代理各有一份，所以每个路由器各管各的。
#[derive(Default)]
pub(crate) struct Registry {
    table: Mutex<Table>,
}

#[derive(Default)]
struct Table {
    requests: HashMap<RequestKey, Shared>,
    leading: HashMap<Option<String>, usize>,
}

/// 一个预热在表里的去向。
enum Join {
    /// 没有同样的请求在途，这个预热领头：发请求，请求结束时 [`Lead::finish`]。
    Lead(Lead, Waiting),
    /// 同样的请求已经在途：并入它，等它的结果。
    Follow(Waiting),
    /// 要领头，但这把 key 已经领着 [`MAX_IN_FLIGHT_PER_KEY`] 个了。
    Refused,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, Table> {
        // 表里只有计数与句柄，没有跨语句的不变量：别的线程 panic 留下的毒不必当真。
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 先看有没有同样的请求在途（并入不占名额，所以先于上限检查），没有才看这把 key 的名额。
    fn join_or_lead(self: &Arc<Self>, key: RequestKey, caller: Option<&str>) -> Join {
        let mut table = self.lock();
        if let Some(shared) = table.requests.get(&key) {
            return Join::Follow(Waiting(shared.clone()));
        }
        let caller = caller.map(str::to_string);
        let leading = table.leading.get(&caller).copied().unwrap_or(0);
        if leading >= MAX_IN_FLIGHT_PER_KEY {
            return Join::Refused;
        }
        table.leading.insert(caller.clone(), leading + 1);
        let (sender, receiver) = watch::channel(None);
        table.requests.insert(key.clone(), receiver.clone());
        Join::Lead(
            Lead {
                registry: Arc::clone(self),
                key,
                caller,
                sender,
                released: false,
            },
            Waiting(receiver),
        )
    }

    /// 请求结束：表项与这把 key 的名额都还回去。
    fn release(&self, key: &RequestKey, caller: &Option<String>) {
        let mut table = self.lock();
        table.requests.remove(key);
        let now_idle = match table.leading.get_mut(caller) {
            Some(leading) => {
                *leading = leading.saturating_sub(1);
                *leading == 0
            }
            None => false,
        };
        if now_idle {
            table.leading.remove(caller);
        }
    }

    /// 测试用：没有任何在途请求，也没有任何 key 占着名额。
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        let table = self.lock();
        table.requests.is_empty() && table.leading.is_empty()
    }

    /// 测试用：这把 key 此刻自己发起了几个在途请求。
    #[cfg(test)]
    pub(crate) fn leading_for(&self, caller: Option<&str>) -> usize {
        let caller = caller.map(str::to_string);
        self.lock().leading.get(&caller).copied().unwrap_or(0)
    }
}

/// 领头的预热：持有表项与这把 key 的名额，直到请求结束。放进请求的任务里，所以不随调用方的等待结束而还。
struct Lead {
    registry: Arc<Registry>,
    key: RequestKey,
    caller: Option<String>,
    sender: watch::Sender<Option<Arc<Answer>>>,
    released: bool,
}

impl Lead {
    /// 请求结束：先还表项与名额（之后到的预热重新领头），再把结果交给所有在等的。
    fn finish(mut self, answer: Answer) {
        self.release();
        // 没有人在等（领头的调用方走了、也没有并入的）也无妨：结果存进通道，没人要就随通道一起丢。
        self.sender.send_replace(Some(Arc::new(answer)));
    }

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.registry.release(&self.key, &self.caller);
        }
    }
}

impl Drop for Lead {
    /// 任务没有留下结果就没了（panic）：名额与表项照样还；等着的拿到「没有结果」。
    fn drop(&mut self) {
        self.release();
    }
}

/// 这把 key 自己发起的在途预热请求已经到了上限：429。
fn limited_response(selected: &Selected) -> Response {
    with_headers(
        error_envelope(
            StatusCode::TOO_MANY_REQUESTS,
            LIMITED,
            format!(
                "this key already has {MAX_IN_FLIGHT_PER_KEY} resident warmup requests in flight (the limit): \
                 wait for one to finish and retry, or warm a model that is already being warmed, \
                 because concurrent warmups of the same model share one request"
            ),
        ),
        selected,
    )
}

/// 一个 resident 预热的最小请求，连同它跑起来需要的一切。它在自己的任务里跑，所以拥有全部数据，不借用请求的任何东西。
struct ResidentRequest {
    proxy: Arc<ChatProxy>,
    endpoint: String,
    path: BufferedUpstreamPath,
    body: Value,
    tenant_id: Option<String>,
    record_id: String,
    provider_id: String,
    served_locality: Locality,
    privacy: PrivacyClass,
    attach_omlx_key: bool,
    /// 这个请求自己的响应头期限（`ChatProxy::warmup_header_timeout`），与调用方的 `timeout_ms` 无关。
    header_timeout: Duration,
}

impl ResidentRequest {
    /// 经聊天用的同一个代理发出去，一直等到它收尾（成功、上游的错误，或响应头期限到点的 504）。并发许可由代理在发送之前拿，随
    /// 返回的结果一起带着，结果被丢掉时才还（等结果的 handler 用完了丢，handler 已经走了则在任务结束时丢）；熔断的准入由代理在
    /// 结果产生之前按既有规则结算（`claim.finish`）。不带幂等键。
    async fn run(self) -> ForwardOutcome {
        let opts = ForwardOpts {
            request_id: None,
            tenant_id: self.tenant_id.as_deref(),
            record_id: &self.record_id,
            provider_id: &self.provider_id,
            served_locality: self.served_locality,
            privacy: self.privacy,
            require_openai_usage: false,
            attach_omlx_key: self.attach_omlx_key,
        };
        match self
            .proxy
            .forward_buffered_path_finalized_within(
                &self.endpoint,
                self.path,
                &self.body,
                &opts,
                Some(self.header_timeout),
                |_| Ok(()),
                (
                    || async { Ok::<(), Infallible>(()) },
                    |_| async { Ok::<(), Infallible>(()) },
                ),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(never) => match never {},
        }
    }
}

/// resident 卡：发一个最小的真实请求，上游答 2xx 就算可用。
///
/// 请求在**自己的任务**里跑，有自己的响应头期限（[`crate::proxy::WARMUP_HEADER_TIMEOUT`]，与调用方的 `timeout_ms` 无关）：
/// 这个函数的 future 被丢掉——`timeout_ms` 到点、客户端断开——只是不再等结果，**不取消**发给上游的请求（上游那一侧的冷加载
/// 不被我们的放弃打断），代理照常在请求结束时结算熔断的准入：成功，或 504 按既有规则计入提供方熔断。任务占着代理的一个
/// 并发许可直到请求结束，所以同时在途的预热请求要有界：同一（卡、模型）的并发预热合并成一个请求，每把数据 key 自己发起的在途
/// 请求有上限（见 [`Registry`]）。
///
/// 上游的回答按预热的错误矩阵翻译：404 是 `model_not_found`，504（响应头或正文超时）是 `warmup_timeout`，其余非 2xx 与聊天的
/// resident 路径一样原样透传（含代理自己的 502 `upstream_auth_failed` 与熔断的 503）。不带幂等键：预热每次都是真请求，不重放缓存。
async fn warm_resident(
    state: &AppState,
    selected: &Selected,
    job: &Job<'_>,
) -> Result<(), Response> {
    let target = job.target;
    let (path, body, kind) = match job.capability {
        Capability::Embedding => (
            BufferedUpstreamPath::EMBEDDINGS,
            json!({"model": target, "input": MINIMAL_INPUT}),
            "embedding",
        ),
        Capability::Chat => (
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            json!({
                "model": target,
                "messages": [{"role": "user", "content": MINIMAL_INPUT}],
                "max_tokens": 1,
            }),
            "chat",
        ),
        // 规范只定义了 chat 与 embedding 的最小请求：别的能力（rerank）的 resident 卡不发一个种类不对的请求，明确拒绝，什么也不发。
        _ => {
            return Err(with_headers(
                BadField::new(
                    "unsupported_model",
                    "warmup has a minimal request for chat and embedding only: a resident card for this capability cannot be warmed",
                )
                .into_response(),
                selected,
            ));
        }
    };
    let key = RequestKey {
        provider_id: selected.card.provider.id.clone(),
        endpoint: selected.card.endpoint.clone(),
        kind,
        model: target.to_string(),
    };
    let waiting = match state.proxy.warmups.join_or_lead(key, job.caller) {
        Join::Refused => return Err(limited_response(selected)),
        // 同样的请求已经在途：并入它，不发请求、不占并发许可。
        Join::Follow(waiting) => waiting,
        Join::Lead(lead, waiting) => {
            let request = ResidentRequest {
                proxy: Arc::clone(&state.proxy),
                endpoint: selected.card.endpoint.clone(),
                path,
                body,
                tenant_id: job.tenant_id.map(str::to_string),
                record_id: job.record_id.to_string(),
                provider_id: selected.card.provider.id.clone(),
                served_locality: selected.served_locality,
                privacy: job.privacy,
                attach_omlx_key: models::is_loopback_omlx(&selected.card),
                header_timeout: state.proxy.warmup_header_timeout,
            };
            tokio::spawn(async move {
                let answer = Answer::from_outcome(request.run().await);
                lead.finish(answer);
            });
            waiting
        }
    };
    let Some(answer) = waiting.answer().await else {
        // 请求的任务没有留下结果就没了（panic，或运行时正在关闭）：没有上游的回答可翻译。
        return Err(with_headers(
            error_envelope(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the warmup request did not complete",
            ),
            selected,
        ));
    };
    match answer.status {
        200..=299 => Ok(()),
        404 => Err(backend_failure(
            selected,
            BackendError::model_not_found(target),
        )),
        504 => Err(timeout_response(selected, job.started.elapsed())),
        status => {
            let mut response = (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                answer.body.clone(),
            )
                .into_response();
            let content_type = answer.content_type.as_deref().unwrap_or("application/json");
            if let Ok(value) = HeaderValue::from_str(content_type) {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, value);
            }
            Err(with_headers(response, selected))
        }
    }
}

/// 审计里「已取消」的结果：状态码（nginx 的「客户端关闭了请求」）与原因。原因的前缀是 `ReasonKind` 里的 `degraded`，和被丢掉的流
/// （`degraded: stream_cancelled`）同一类。
const CANCELLED_STATUS: u16 = 499;
const CANCELLED_REASON: &str = "degraded: request_cancelled";

/// 请求的 future 在回答之前被丢掉（客户端挂了电话）时，把这条预热的审计事件补上。
///
/// 审计中间件在 handler 返回之后才写事件，被丢掉的请求走不到那里；而引擎里的加载（或 resident 的最小请求）照常跑完——没有这个
/// 守卫，这次预热在审计里就像没发生过。守卫在 handler 开头装上、handler 返回响应时拆掉（之后由中间件为那个响应写唯一的一条）；
/// 装着时被丢掉，就把「已取消」的事件交给**它自己的任务**去写（`Drop` 里不能 await，任务也不会随请求一起被丢掉）。这样
/// 正常完成、`timeout_ms` 到点（是一个正常返回的 504）、被取消，三种结局各写**恰好一条**。
///
/// 事件归属的租户按审计中间件同一个规则（`audit_tenant_for`）从已认证的身份算出来；取消发生在认证之前（还没进 handler）时
/// 没有装守卫，也就没有事件——那时还没有任何东西被预热。
struct CancelAudit {
    state: Arc<AppState>,
    meta: Option<BufferedAuditMeta>,
    started: Instant,
}

impl CancelAudit {
    fn arm(
        state: &Arc<AppState>,
        headers: &HeaderMap,
        identity: Option<&AuthenticatedVirtualKey>,
        record_id: &str,
    ) -> Self {
        let meta = BufferedAuditMeta {
            tenant_id: crate::audit_tenant_for(
                state.deploy_mode,
                crate::audit_tenant_id(state, headers),
                identity,
            ),
            request_id: crate::audit_header(headers, crate::HEADER_REQUEST_ID, None),
            privacy: crate::audit_header(headers, "x-idoris-privacy", Some("local_only")),
            intent: Some(AUDIT_INTENT.to_string()),
            record_id: record_id.to_string(),
            status: StatusCode::from_u16(CANCELLED_STATUS).unwrap_or(StatusCode::BAD_REQUEST),
            origin_record_id: None,
            reason: CANCELLED_REASON.to_string(),
            latency_ms: 0,
        };
        Self {
            state: Arc::clone(state),
            meta: Some(meta),
            started: Instant::now(),
        }
    }

    /// handler 返回了响应：这条预热由审计中间件记，守卫不再写。
    fn defuse(&mut self) {
        self.meta = None;
    }
}

impl Drop for CancelAudit {
    fn drop(&mut self) {
        let Some(mut meta) = self.meta.take() else {
            return;
        };
        meta.latency_ms = crate::elapsed_ms(self.started);
        let state = Arc::clone(&self.state);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(crate::finish_buffered_audit(state, meta));
            }
            Err(_) => {
                state.audit_failures.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "idoris: audit of a cancelled warmup could not be written: no async runtime"
                );
            }
        }
    }
}

/// `POST /v1/models/warmup`。
pub(crate) async fn warmup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    remote_identity: Option<Extension<AuthenticatedVirtualKey>>,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    body: Bytes,
) -> Response {
    let identity = remote_identity.as_ref().map(|identity| &identity.0);
    let mut cancel_audit = CancelAudit::arm(&state, &headers, identity, &record_id);
    let response = handle(&state, &headers, identity, &record_id, &body).await;
    cancel_audit.defuse();
    response
}

async fn handle(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthenticatedVirtualKey>,
    record_id: &str,
    body: &Bytes,
) -> Response {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return error_envelope(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "request body is not valid JSON",
            );
        }
    };
    let Some(object) = value.as_object() else {
        return error_envelope(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "request body must be a JSON object",
        );
    };
    let request = match parse_request(object) {
        Ok(request) => request,
        Err(bad) => return bad.into_response(),
    };
    let mut parsed = match parse_profile(headers, Some(&request.model), state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    if let Err(response) =
        crate::enforce_protocol_key_scope(state, headers, identity, &parsed).await
    {
        return response;
    }
    let capability = warm_capability(parsed.role);
    // The endpoint, not the caller's `X-iDoris-Capabilities`, decides what is being warmed.
    parsed.task.capabilities = Some(vec![capability]);
    // 隐私：和其他协议端点一样过一遍（远程熔断开关、内容收紧）。预热的请求体里没有内容，但「开关一合上，一切都只走本地」
    // 不能因为这条路径没有内容就绕过。
    let inspection = crate::protocol_inspection(
        state,
        &value,
        body.len(),
        parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
    )
    .await;
    parsed.task.privacy = Some(inspection.effective);

    let policy = dispatch::policy_cards(&state.cards, &state.routing_policy, &parsed);
    let cards: Vec<_> = policy
        .cards
        .into_iter()
        .filter(|card| !idoris_policy::is_subscription_provider_id(&card.provider.id))
        .collect();
    let blocked = state.proxy.blocked_cards(&cards);
    let selected = match dispatch::select_with_gate_fallback(
        &cards,
        &parsed,
        "",
        None,
        &Default::default(),
        &blocked,
        inspection.tightened && !inspection.killed,
    ) {
        Ok(selected) => selected,
        Err(DispatchError::Rejection(rejection)) => return crate::rejection_response(rejection),
        Err(DispatchError::Internal(message)) => {
            return error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message);
        }
    };
    // 门控降级放行、没就绪的托管卡：不碰引擎，和聊天一样 503 带原因。
    if let Some(response) = managed_unavailable_response(state, &selected) {
        return response;
    }
    // 本地免费才预热：付费卡的最小请求也会花钱，而预热不进用量与预算记录。
    let cost = &selected.card.provider.cost;
    if cost.input_per_m != 0.0 || cost.output_per_m != 0.0 {
        return with_headers(
            error_envelope(
                StatusCode::SERVICE_UNAVAILABLE,
                "paid_proxy_unavailable",
                "warmup is only available for free local models; the selected card has a price",
            ),
            &selected,
        );
    }
    let target = match dispatch::backend_model_id(&parsed, Some(&request.model), &selected.card) {
        Ok(target) => target,
        Err(error) => return crate::model_resolution_error(error),
    };
    // 预算（PR-Daemon 对 #634 的预审 W-2）：聊天对免费的本地模型也会过一遍账本（`SpendGate::All` 下余额为 0 是 402），预热不能是
    // 绕开它的后门——加载或驱逐一个模型、对 resident 卡发一个 token，都是这个租户在用本机。这里只做一次**只读**的准入检查
    // （`check_admission`，不预留、不结算、不写任何行）：预热本身不花钱，不占用预算，也不进用量记录。没有账本就不查（同聊天：
    // 没接账本的部署里免费调用照常可用）；默认的 `PaidOnly` 对免费调用放行。余额不足的错误与聊天是同一个函数映射出来的。
    if let Some(ledger) = state.budget_ledger.as_deref() {
        let budget_identity = budget::BudgetIdentity::resolve(
            identity.map(|identity| identity.key_id.as_str()),
            Some(&target),
            selected.card.provider.id.as_str(),
        );
        if let Err(err) = budget::check_admission(
            ledger,
            parsed.tenant_id.as_deref(),
            &budget_identity,
            selected.card.provider.id.as_str(),
            0,
        ) {
            return budget_error_response(
                &err,
                &ChatOutcome {
                    decision: selected.decision.clone(),
                    served_locality: selected.served_locality,
                    result: Err(DispatchFailure::Budget(err.clone())),
                    actual_cost_minor: None,
                },
            );
        }
    }

    let started = Instant::now();
    let job = Job {
        privacy: parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
        tenant_id: parsed.tenant_id.as_deref(),
        caller: identity.map(|identity| identity.key_id.as_str()),
        record_id,
        capability,
        target: &target,
        started,
    };
    let work = async {
        if dispatch::is_resident_http_service(&selected.card) {
            // 没有账本可问：resident 卡的 `already_loaded` 恒为 false，`load_ms` 是那一个最小请求的耗时。
            warm_resident(state, &selected, &job).await.map(|()| false)
        } else {
            warm_lifecycle(
                state,
                &selected,
                &target,
                headroom_breach_confirmed(headers),
            )
            .await
        }
    };
    let already_loaded = match tokio::time::timeout(request.timeout, work).await {
        Ok(Ok(already_loaded)) => already_loaded,
        Ok(Err(response)) => return response,
        // 只是放弃等待：`work` 被丢掉，但引擎里的加载（Supervisor 的后台任务）与 resident 的最小请求（`warm_resident` 自己的
        // 任务）都与它无关，照常跑完。
        Err(_elapsed) => return timeout_response(&selected, request.timeout),
    };
    with_headers(
        Json(json!({
            "model": target,
            "provider_id": selected.card.provider.id,
            "state": "ready",
            "already_loaded": already_loaded,
            "load_ms": crate::elapsed_ms(started),
        }))
        .into_response(),
        &selected,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use http_body_util::BodyExt;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn model_alone_gets_the_default_timeout_of_two_minutes() {
        let request = parse_request(&object(json!({"model": "idoris/daily"}))).unwrap();
        assert_eq!(request.model, "idoris/daily");
        assert_eq!(request.timeout, Duration::from_millis(120_000));
    }

    #[test]
    fn timeout_ms_is_taken_as_given_across_the_whole_inclusive_range() {
        for ms in [1_000u64, 1_001, 120_000, 299_999, 300_000] {
            let request = parse_request(&object(json!({"model": "m", "timeout_ms": ms}))).unwrap();
            assert_eq!(request.timeout, Duration::from_millis(ms));
        }
    }

    #[tokio::test]
    async fn a_bad_field_renders_as_a_400_unsupported_field_envelope() {
        let bad = BadField::new("invalid_timeout_ms", "nope");
        let response = bad.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "unsupported_field");
        assert_eq!(body["error"]["reason_code"], "invalid_timeout_ms");
        assert_eq!(body["error"]["remediation"], "nope");
    }

    #[test]
    fn every_bad_request_is_rejected_with_its_own_reason_code() {
        let cases = [
            (json!({"model": "m", "extra": 1}), "unsupported_parameter"),
            (json!({"extra": 1}), "unsupported_parameter"),
            // The unknown field is reported before a missing model or a bad timeout.
            (
                json!({"model": 5, "timeout_ms": "x", "extra": 1}),
                "unsupported_parameter",
            ),
            (json!({}), "unsupported_model"),
            (json!({"model": ""}), "unsupported_model"),
            (json!({"model": " \t"}), "unsupported_model"),
            (json!({"model": null}), "unsupported_model"),
            (json!({"model": 7}), "unsupported_model"),
            (
                json!({"model": "m", "timeout_ms": 999}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": 300_001}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": -1000}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": 1500.5}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": 2000.0}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": "2000"}),
                "invalid_timeout_ms",
            ),
            (
                json!({"model": "m", "timeout_ms": null}),
                "invalid_timeout_ms",
            ),
        ];
        for (body, expected) in cases {
            let rejected = parse_request(&object(body.clone())).unwrap_err();
            assert_eq!(rejected.reason_code, expected, "{body}");
        }
    }

    #[test]
    fn printable_truncates_at_64_characters_and_neutralizes_control_characters() {
        assert_eq!(printable("model"), "model");
        let exact = "x".repeat(64);
        assert_eq!(printable(&exact), exact, "64 个字符原样保留");
        let long = "x".repeat(65);
        assert_eq!(printable(&long), format!("{}…", "x".repeat(64)));
        assert_eq!(printable("a\nb\u{1b}c"), "a\u{FFFD}b\u{FFFD}c");
        // Characters, not bytes: 64 multi-byte characters are still not truncated.
        let wide = "模".repeat(64);
        assert_eq!(printable(&wide), wide);
    }

    #[test]
    fn the_role_decides_what_is_warmed_embed_is_embedding_rerank_is_rerank_the_rest_is_chat() {
        assert_eq!(warm_capability(Some(Role::Embed)), Capability::Embedding);
        assert_eq!(warm_capability(Some(Role::Rerank)), Capability::Rerank);
        for role in [
            None,
            Some(Role::Fast),
            Some(Role::Daily),
            Some(Role::Deep),
            Some(Role::Vision),
            Some(Role::Decide),
            Some(Role::Auto),
        ] {
            assert_eq!(warm_capability(role), Capability::Chat, "{role:?}");
        }
    }

    // ---- 合并表与每 key 上限（W-1） ----

    fn request(model: &str) -> RequestKey {
        RequestKey {
            provider_id: "card".to_string(),
            endpoint: "http://127.0.0.1:9".to_string(),
            kind: "chat",
            model: model.to_string(),
        }
    }

    fn new_registry() -> Arc<Registry> {
        Arc::new(Registry::default())
    }

    /// 领头；不是领头就让用例失败（`Join` 里装着 future 与守卫，不实现 `Debug`，所以不用 `unwrap`）。
    fn leads(registry: &Arc<Registry>, model: &str, caller: Option<&str>) -> (Lead, Waiting) {
        match registry.join_or_lead(request(model), caller) {
            Join::Lead(lead, waiting) => (lead, waiting),
            Join::Follow(_) => panic!("{model}: expected to lead, but joined an in-flight request"),
            Join::Refused => panic!("{model}: expected to lead, but the key is at its limit"),
        }
    }

    fn follows(registry: &Arc<Registry>, model: &str, caller: Option<&str>) -> Waiting {
        match registry.join_or_lead(request(model), caller) {
            Join::Follow(waiting) => waiting,
            Join::Lead(..) => panic!("{model}: expected to join an in-flight request, but led"),
            Join::Refused => panic!("{model}: expected to join, but was refused"),
        }
    }

    fn is_refused(registry: &Arc<Registry>, model: &str, caller: Option<&str>) -> bool {
        matches!(registry.join_or_lead(request(model), caller), Join::Refused)
    }

    fn answer(status: u16) -> Answer {
        Answer {
            status,
            content_type: Some("text/plain".to_string()),
            body: Bytes::from_static(b"body"),
        }
    }

    #[tokio::test]
    async fn a_concurrent_warmup_of_the_same_request_follows_and_gets_the_leaders_answer() {
        let registry = new_registry();
        let (lead, leader_waiting) = leads(&registry, "m", Some("key-a"));
        // 另一把 key 也并入同一个请求：合并不分调用方。
        let follower_waiting = follows(&registry, "m", Some("key-b"));
        lead.finish(answer(404));
        for waiting in [leader_waiting, follower_waiting] {
            // The leader published an answer, so there is one.
            let got = waiting.answer().await.unwrap();
            assert_eq!(got.status, 404);
            assert_eq!(got.content_type.as_deref(), Some("text/plain"));
            assert_eq!(&got.body[..], b"body");
        }
        assert!(registry.is_idle());
    }

    #[tokio::test]
    async fn a_follower_that_starts_waiting_after_the_answer_was_published_still_gets_it() {
        let registry = new_registry();
        let (lead, _) = leads(&registry, "m", None);
        let late = follows(&registry, "m", None);
        lead.finish(answer(200));
        // The answer is kept for those who joined before it was published.
        let got = late.answer().await.unwrap();
        assert_eq!(got.status, 200);
    }

    #[test]
    fn a_key_leads_at_most_the_limit_and_following_does_not_use_a_slot() {
        let registry = new_registry();
        let held: Vec<_> = (0..MAX_IN_FLIGHT_PER_KEY)
            .map(|index| leads(&registry, &format!("model-{index}"), Some("key")))
            .collect();
        assert_eq!(registry.leading_for(Some("key")), MAX_IN_FLIGHT_PER_KEY);
        // 新的请求要领头，名额满了。
        assert!(is_refused(&registry, "another", Some("key")));
        // 并入一个已经在途的请求不占名额，名额满了也能并入。
        for index in 0..MAX_IN_FLIGHT_PER_KEY {
            follows(&registry, &format!("model-{index}"), Some("key"));
        }
        assert_eq!(registry.leading_for(Some("key")), MAX_IN_FLIGHT_PER_KEY);
        drop(held);
    }

    #[test]
    fn the_limit_is_per_key_and_the_unauthenticated_caller_is_one_caller() {
        let registry = new_registry();
        let mut held = Vec::new();
        for caller in [Some("key-a"), Some("key-b"), None] {
            for index in 0..MAX_IN_FLIGHT_PER_KEY {
                held.push(leads(&registry, &format!("{caller:?}-{index}"), caller));
            }
            assert!(
                is_refused(&registry, "one-too-many", caller),
                "{caller:?} is at its own limit"
            );
        }
        assert_eq!(registry.leading_for(Some("key-a")), MAX_IN_FLIGHT_PER_KEY);
        assert_eq!(registry.leading_for(Some("key-b")), MAX_IN_FLIGHT_PER_KEY);
        assert_eq!(registry.leading_for(None), MAX_IN_FLIGHT_PER_KEY);
        drop(held);
        assert!(registry.is_idle());
    }

    #[tokio::test]
    async fn the_slot_and_the_table_entry_come_back_when_the_request_ends() {
        let registry = new_registry();
        let (lead, _) = leads(&registry, "m", Some("key"));
        let (_other, _) = leads(&registry, "other", Some("key"));
        assert!(is_refused(&registry, "third", Some("key")));
        lead.finish(answer(200));
        assert_eq!(registry.leading_for(Some("key")), 1);
        // 名额回来了：第三个能领头；同一个模型也重新领头，不再并入已经结束的请求。
        let _third = leads(&registry, "third", Some("key"));
        let _again = leads(&registry, "m", Some("other-key"));
    }

    #[tokio::test]
    async fn a_lead_dropped_without_an_answer_frees_everything_and_leaves_followers_with_none() {
        let registry = new_registry();
        let (lead, _) = leads(&registry, "m", Some("key"));
        let follower = follows(&registry, "m", Some("key"));
        drop(lead); // 任务 panic 了。
        assert!(registry.is_idle());
        assert!(follower.answer().await.is_none());
    }
}
