//! `POST /v1/models/warmup`（IDORIS-CORE S8；接口规范 §2.11.2，修订 1.6 勘误八）：让一个模型进入「可用」状态，
//! 之后第一次真正的请求不用再等加载。数据面、数据 key（不是 Admin）。
//!
//! - **鉴权与 key 范围同聊天**：聊天在该模型上会被拒的 key，预热也被拒（`auth::enforce_scope`，与 embeddings 同一个入口
//!   `enforce_protocol_key_scope`）。
//! - **选卡同聊天**（同一套 `policy_cards` + `select_with_gate_fallback`，含角色绑定、隐私、熔断的就绪快照），另有三条：
//!   不在本机的卡不参与（[`is_local_endpoint`]：订阅中转卡、远程与局域网的卡、声明在本机而端点指向别处的卡；`X-iDoris-Privacy: any`
//!   也不放宽它，那只对聊天开放远程卡）；请求的能力由角色定（`embed` 是 embedding，`rerank` 是 rerank，其余是 chat）；
//!   单价非零的卡拒绝（预热只服务本地免费卡）。
//! - **生命周期卡（oMLX 等）**：走 [`dispatch::ensure_loaded`]——和聊天的按需加载是同一段代码（读 catalog、拒未知模型、
//!   Supervisor 的幂等 load：余量确认与驱逐都在里面）；加载完成即返回，**不生成任何 token**。加载之前同样按 [`is_local_endpoint`]
//!   复核选中的卡。三种角色（chat、embedding、rerank）都一样，因为「就绪」就是「已加载」。同一模型的并发预热与并发的首个聊天请求由
//!   Supervisor 合并成一次加载。
//! - **resident 卡**：没有生命周期可管，按能力发一个最小的真实请求（chat：`max_tokens: 1`；embedding：输入 `"warmup"`），
//!   发出之前对选中的卡按 [`is_local_endpoint`] 复核一次（看的是真要发往的地址），不过就什么也不发。
//!   请求在自己的任务里跑、有自己的响应头期限（[`crate::proxy::WARMUP_HEADER_TIMEOUT`]，不是聊天的 10 秒，也不是 `timeout_ms`）。
//!   规范没有定义 rerank 的最小请求，rerank 的 resident 卡明确拒绝（400 `unsupported_field` / `unsupported_model`），什么也不发。
//!   请求占着代理的一个并发许可（共 32 个，和聊天共用）直到结束，所以有四道限制（[`Registry`]）：同一模型的并发预热合并成一个请求；
//!   每个在途请求上并入者的个数有上限；每把数据 key 自己发起的在途请求有上限；全部 key 合计的在途请求也有上限。后三道超了是 429
//!   `warmup_in_flight_limited`，带 `Retry-After`。一个预热并入别人的请求之前，先过它自己的鉴权、本机候选与预算（顺序：鉴权、本机候选、
//!   并发上限的预检、预算，最后才登记），所以并入不是绕开这些检查的后门；并入的那个 200 里多一个 `"coalesced": true`。
//!   最小请求本身不带幂等键（`ForwardOpts.request_id` 恒为 `None`，见 [`ResidentRequest::run`]）。
//! - **`timeout_ms` 到点只是放弃等待**：返回 504 `warmup_timeout`，**不取消、不缩短**任何东西。生命周期卡：Supervisor 的加载任务
//!   与请求的 future 无关，照常跑完并清掉 `load.pending`，之后的预热或聊天直接看到「已就绪」。resident 卡：最小请求在它自己的
//!   任务里，不被打断，直到上游答了或它自己的期限到点，代理照常结算熔断的准入（504 按既有规则计入提供方熔断）。客户端断开同理。
//!   预热自己的 `timeout_ms` 与客户端断开不留围栏；引擎侧的超时（`RuntimeAdapter::load_timeout`，生产缺省 180 秒）或 panic 保持
//!   既有的「结果未知」保护，围栏保留（行为同聊天，没有变）。
//! - **预算**：接了账本的部署里，`SpendGate::All` 下余额不足的租户聊天是 402，预热也一样（一次不插入预留行的 `check_admission`，
//!   不结算；它要拿账本的锁，所以排在并发上限的预检之后），免费调用在默认的 `PaidOnly` 下放行。
//! - 不进用量记录、不占用预算（本地免费；付费卡直接拒绝），写一条 `intent: warmup` 的审计事件（见 `record_id_middleware`）。请求被取消
//!   （客户端断开）时中间件走不到那里，由 [`CancelAudit`] 补上：完成、`timeout_ms` 到点、取消各写恰好一条。

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use idoris_backend::BackendError;
use idoris_contracts::ComponentCard;
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
use crate::proxy::{BufferedUpstreamPath, ChatProxy, ForwardOpts, ForwardOutcome, is_loopback_url};
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

/// 预热服务的是**本机**的卡：它要发的请求（resident 的最小请求，或生命周期卡的加载）发往这台机器上的地址。判据是两个已有的判断
/// 合在一起，没有另写一套：
///
/// - [`models::is_trusted_local`]：路由给 `local_only` 请求用的可信本地三条件（实际 loopback、卡的隐私等级是 `local_only`、出站是 none
///   或 loopback），`/v1/models` 与 `/health` 探活的熔断口径用的也是它；订阅中转与 `spawn_cli` 卡在里面算远程。
/// - [`is_loopback_url`]：代理自己判断「这个请求算不算非本机出口」的那个函数（出口计数、凭据类别、去标识都用它），按**真要发往的 URL
///   的主机**判，不信卡自己声明的 locality：声明 loopback 而端点指向别处的卡（注册校验没拦住，或根本没走注册）在这里过不去。
///
/// 单价为零不能说明在本机：免费的远程卡同样会被选中（`X-iDoris-Privacy: any` 下聊天本来就可以选它），预热却会向它的真实地址发一个请求。
fn is_local_endpoint(card: &ComponentCard) -> bool {
    models::is_trusted_local(card) && is_loopback_url(&card.endpoint)
}

/// 选中的卡不在本机：503 `no_eligible_candidate`（既有的错误类型，意思是「它不是预热的候选」），什么也不发。选卡阶段已经把这样的卡
/// 排除在候选之外，这是请求发出之前的复核（[`warm_resident`]）没过时的回答。
fn not_local_response(selected: &Selected) -> Response {
    with_headers(
        error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_eligible_candidate",
            "warmup is only available for free local models; the selected card's endpoint is not on this machine, so nothing was sent",
        ),
        selected,
    )
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
    // 复核（同 [`warm_resident`]）：选卡已经把不在本机的卡排除了，这是加载请求发给引擎之前的最后一道。在查 Supervisor 之前，
    // 所以过不去的卡不碰任何运行时。
    if !is_local_endpoint(&selected.card) {
        return Err(not_local_response(selected));
    }
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
    /// 这个预热并入了别人发起的在途请求（[`Join::Follow`]）：自己没有向上游发请求。200 的回答里标出来（`"coalesced": true`）。
    coalesced: &'a AtomicBool,
}

// ---------------------------------------------------------------------------------------------
// 同一模型的并发预热合并，与在途请求的几道上限
// ---------------------------------------------------------------------------------------------

/// 每把数据 key 此刻自己发起的 resident 预热请求数的上限（见 [`Registry`]）。2 个够 Agent24 同时预热一个 chat 模型与一个
/// embedding 模型；再多的不同模型要等一个结束。
pub(crate) const MAX_IN_FLIGHT_PER_KEY: usize = 2;

/// 全部 key 合计的在途领头请求数的上限。每个领头请求占代理 32 个并发许可里的一个，8 个把聊天剩下至少 24 个：每把 key 的上限再
/// 高，凑够许多把 key 也不能占满它们。
pub(crate) const MAX_LEADERS_IN_FLIGHT: usize = 8;

/// 一个在途请求上同时等着的并入者（不含领头的调用方）的上限。并入不占并发许可，但每个并入者都是一个等着的任务，还要先过自己的
/// 预算检查（拿账本的锁）；64 个远多于 Agent24 会有的并发预热。
pub(crate) const MAX_FOLLOWERS_PER_REQUEST: usize = 64;

/// 429 的 `Retry-After`（秒）：只是提示。请求可能跑到 180 秒，没有哪个数是对的；10 秒够领头的请求答完或并入者走掉一批，又不至于
/// 让调用方干等。
pub(crate) const RETRY_AFTER_SECS: u64 = 10;

/// 超过上面几个上限时的错误类型与原因码（429）。
const LIMITED: &str = "warmup_in_flight_limited";

/// 429 是因为哪一道上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limit {
    /// 这把 key 自己发起的在途请求已经有 [`MAX_IN_FLIGHT_PER_KEY`] 个。
    Key,
    /// 全部 key 合计的在途领头请求已经有 [`MAX_LEADERS_IN_FLIGHT`] 个。
    Global,
    /// 要并入的那个在途请求上已经等着 [`MAX_FOLLOWERS_PER_REQUEST`] 个并入者。
    Followers,
}

/// 一个 resident 最小请求在合并表里的身份：同一张卡（id 与端点）、同一种最小请求、同一个模型。
#[derive(Clone, PartialEq, Eq, Hash)]
struct RequestKey {
    provider_id: String,
    endpoint: String,
    /// `"chat"` 或 `"embedding"`：同一个模型的两种最小请求是两个请求。
    kind: &'static str,
    model: String,
}

impl RequestKey {
    fn of(card: &ComponentCard, kind: &'static str, model: &str) -> Self {
        Self {
            provider_id: card.provider.id.clone(),
            endpoint: card.endpoint.clone(),
            kind,
            model: model.to_string(),
        }
    }
}

/// 预热哪种最小请求：chat 与 embedding 才有（规范只定义了这两种），别的能力（rerank）没有。
fn request_kind(capability: Capability) -> Option<&'static str> {
    match capability {
        Capability::Chat => Some("chat"),
        Capability::Embedding => Some("embedding"),
        _ => None,
    }
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
struct Waiting {
    answer: Shared,
    /// 并入者持有：它不再等（拿到了结果、`timeout_ms` 到点、客户端断开）时丢掉，把并入者的名额还回去。领头的调用方没有。
    _follower: Option<FollowerGuard>,
}

impl Waiting {
    /// `None`：请求的任务没有留下结果就没了（panic，或运行时在关闭）。
    async fn answer(mut self) -> Option<Arc<Answer>> {
        let answered = self.answer.wait_for(Option::is_some).await.ok()?;
        answered.clone()
    }
}

/// 一个并入者在它所并入的在途请求上占的名额：丢掉就还。计数放在表项与并入者共享的原子数里，请求先结束、表项先没了也无妨。
struct FollowerGuard(Arc<AtomicUsize>);

impl Drop for FollowerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 表里一个在途的请求。
struct InFlight {
    answer: Shared,
    /// 此刻等着它的并入者数（不含领头的调用方）。加在持表的锁里，减在并入者丢掉 [`FollowerGuard`] 时、不持锁：所以持锁时读到的
    /// 数可能比实际多一点点（刚走的那个还没减），只会让上限在边界上早一步拒绝，不会放多。
    followers: Arc<AtomicUsize>,
}

/// 一个代理上在途的 resident 预热请求，与它们的几道上限。
///
/// 为什么需要（PR-Daemon 对 #634 的预审 W-1，及其后的本地预审 L-3、L-4）：resident 预热的请求在自己的任务里占着代理的一个并发
/// 许可，最长 180 秒加正文，调用方却早就拿到了 504；一把数据 key 凑满 32 个并发预热就占满全部许可，别的租户的聊天得 503。四道限制：
///
/// 1. **合并**：同一（卡、端点、最小请求、模型）已经有请求在途时，后来的预热并入它——不再发请求、不占许可，等同一个结果。
/// 2. **并入者上限**：一个在途请求上同时等着的并入者最多 [`MAX_FOLLOWERS_PER_REQUEST`] 个，再多的 429 [`LIMITED`]。并入者不占许可，
///    但每个都是一个等着的任务，还要先过自己的预算检查（拿账本的锁），所以不能无限多。
/// 3. **每 key 上限**：一把 key 自己发起（领头）的在途请求最多 [`MAX_IN_FLIGHT_PER_KEY`] 个，再多的 429。并入别人的请求不算
///    （它不占许可）。名额由请求的任务持有，**请求真正结束才还**：调用方的 `timeout_ms` 到点、客户端断开都不还，所以一把 key 不论
///    超时多少次，最多只占 N 个许可。
/// 4. **全局上限**：全部 key 合计的在途领头请求最多 [`MAX_LEADERS_IN_FLIGHT`] 个，再多的 429。每把 key 的上限再低，凑够许多把 key
///    也能占满 32 个许可；这一道把聊天剩下的许可留出来。每个在途请求恰有一个领头的，所以在途请求数就是领头请求数。
///
/// 只管 resident 卡：生命周期卡的预热不走代理的许可（加载在 Supervisor 里，同一模型本来就合并，别的模型 `supervisor_busy`）。
/// 没有认证的调用方（开发的无 key 模式，只在本机回环）算一个整体。每个代理各有一份，所以每个路由器各管各的。
#[derive(Default)]
pub(crate) struct Registry {
    table: Mutex<Table>,
}

#[derive(Default)]
struct Table {
    requests: HashMap<RequestKey, InFlight>,
    leading: HashMap<Option<String>, usize>,
}

/// 一个预热在表里的去向。
enum Join {
    /// 没有同样的请求在途，这个预热领头：发请求，请求结束时 [`Lead::finish`]。
    Lead(Lead, Waiting),
    /// 同样的请求已经在途：并入它，等它的结果。
    Follow(Waiting),
    /// 超了一道上限：429。
    Refused(Limit),
}

/// [`decide`] 的结论：只是判断，还没有登记任何东西。
enum Decision<'t> {
    Follow(&'t InFlight),
    Lead,
    Refused(Limit),
}

/// 一个预热此刻在表里的去向，不改任何东西。**登记**（[`Registry::join_or_lead`]）与**预检**（[`Registry::refusal`]）用的是这同一个
/// 判断，所以预检说放行的，登记时只会因为中间有别人先到才可能被拒。先看有没有同样的请求在途（并入不占许可，所以先于领头的几道
/// 上限），再依次看这把 key 的名额、全局的名额。
fn decide<'t>(table: &'t Table, key: &RequestKey, caller: &Option<String>) -> Decision<'t> {
    if let Some(in_flight) = table.requests.get(key) {
        return if in_flight.followers.load(Ordering::SeqCst) >= MAX_FOLLOWERS_PER_REQUEST {
            Decision::Refused(Limit::Followers)
        } else {
            Decision::Follow(in_flight)
        };
    }
    if table.leading.get(caller).copied().unwrap_or(0) >= MAX_IN_FLIGHT_PER_KEY {
        return Decision::Refused(Limit::Key);
    }
    if table.requests.len() >= MAX_LEADERS_IN_FLIGHT {
        return Decision::Refused(Limit::Global);
    }
    Decision::Lead
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, Table> {
        // 持锁的临界区里只对两张哈希表做插入、删除与读取（不调用外部代码、不 await），不会半途 panic；别的线程留下的毒不必当真。
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 登记：并入同样的请求、领头，或者被拒。判断与登记在同一个临界区里。
    fn join_or_lead(self: &Arc<Self>, key: RequestKey, caller: Option<&str>) -> Join {
        let caller = caller.map(str::to_string);
        let mut table = self.lock();
        match decide(&table, &key, &caller) {
            Decision::Refused(limit) => Join::Refused(limit),
            Decision::Follow(in_flight) => {
                in_flight.followers.fetch_add(1, Ordering::SeqCst);
                Join::Follow(Waiting {
                    answer: in_flight.answer.clone(),
                    _follower: Some(FollowerGuard(Arc::clone(&in_flight.followers))),
                })
            }
            Decision::Lead => {
                *table.leading.entry(caller.clone()).or_insert(0) += 1;
                let (sender, receiver) = watch::channel(None);
                table.requests.insert(
                    key.clone(),
                    InFlight {
                        answer: receiver.clone(),
                        followers: Arc::default(),
                    },
                );
                Join::Lead(
                    Lead {
                        registry: Arc::clone(self),
                        key,
                        caller,
                        sender,
                        released: false,
                    },
                    Waiting {
                        answer: receiver,
                        _follower: None,
                    },
                )
            }
        }
    }

    /// 预检：此刻这样一个预热会不会因为哪一道上限被 429。什么也不登记。放在预算检查之前（它要拿账本的锁），免得一堆注定被拒的
    /// 预热每个都去拿；真正的判断仍在登记时再做一次，两次之间别人先到的话以登记为准。
    fn refusal(&self, key: &RequestKey, caller: Option<&str>) -> Option<Limit> {
        let caller = caller.map(str::to_string);
        let table = self.lock();
        match decide(&table, key, &caller) {
            Decision::Refused(limit) => Some(limit),
            Decision::Follow(_) | Decision::Lead => None,
        }
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

    /// 观测：此刻在途的请求数（领头的，全部 key 合计）。
    pub(crate) fn requests_in_flight(&self) -> usize {
        self.lock().requests.len()
    }

    /// 观测：此刻等着别人发起的在途请求的并入者数（全部在途请求合计）。
    pub(crate) fn followers_waiting(&self) -> usize {
        self.lock()
            .requests
            .values()
            .map(|in_flight| in_flight.followers.load(Ordering::SeqCst))
            .sum()
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

/// 超了一道在途上限：429，带 `Retry-After`。三道上限共用一个错误类型与原因码，只有说明文字不同。
fn limited_response(selected: &Selected, limit: Limit) -> Response {
    let message = match limit {
        Limit::Key => format!(
            "this key already has {MAX_IN_FLIGHT_PER_KEY} resident warmup requests in flight (the limit): \
             wait for one to finish and retry, or warm a model that is already being warmed, \
             because concurrent warmups of the same model share one request"
        ),
        Limit::Global => format!(
            "{MAX_LEADERS_IN_FLIGHT} resident warmup requests are already in flight across all keys (the limit): \
             retry shortly, or warm a model that is already being warmed, \
             because concurrent warmups of the same model share one request"
        ),
        Limit::Followers => format!(
            "{MAX_FOLLOWERS_PER_REQUEST} callers are already waiting on the warmup of this model (the limit): \
             retry shortly, by when this warmup has most likely finished"
        ),
    };
    let mut response = with_headers(
        error_envelope(StatusCode::TOO_MANY_REQUESTS, LIMITED, message),
        selected,
    );
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from(RETRY_AFTER_SECS),
    );
    response
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

/// 测试注入点（[`ResidentRequest::run`]）：让一个 resident 预热的领头任务停在发请求之前，等测试放行；放行之后可以让它 panic，
/// 而不是发请求。用来让「并入者一定在领头的结束之前到」、以及领头任务 panic 的路径都不靠时间碰运气。
#[cfg(test)]
pub(crate) struct RunHook {
    /// 领头任务在这里等：测试往里放许可才继续（`add_permits`）。
    pub(crate) gate: tokio::sync::Semaphore,
    /// 放行之后 panic，而不是发请求。
    pub(crate) panic_after_gate: bool,
    /// 到过门口的领头任务数。
    pub(crate) arrived: AtomicUsize,
}

#[cfg(test)]
impl RunHook {
    /// 关着的门；`panic_after_gate` 为真时放行之后 panic。
    pub(crate) fn closed(panic_after_gate: bool) -> Arc<Self> {
        Arc::new(Self {
            gate: tokio::sync::Semaphore::new(0),
            panic_after_gate,
            arrived: AtomicUsize::new(0),
        })
    }
}

impl ResidentRequest {
    /// 经聊天用的同一个代理发出去，一直等到它收尾（成功、上游的错误，或响应头期限到点的 504）。并发许可由代理在发送之前拿，随
    /// 返回的结果一起带着，结果被丢掉时才还（等结果的 handler 用完了丢，handler 已经走了则在任务结束时丢）；熔断的准入由代理在
    /// 结果产生之前按既有规则结算（`claim.finish`）。
    ///
    /// **不变式：最小请求不带幂等键，`request_id` 恒为 `None`。** 代理的幂等缓存与在途表（重放）都按它建键：带了，这个最小请求的结果
    /// 就会按领头调用方的 `X-iDoris-Request-Id` 登记下来，之后同一张卡上用同一个 id 的聊天会撞上它（409 `request_id_conflict`，或者
    /// 重放预热的结果）。预热的合并由 [`Registry`] 做，不靠幂等键；`X-iDoris-Request-Id` 在预热里只用于审计关联，属于每个调用方
    /// 自己（领头的与并入的各记各的），不属于那个被共享的请求。
    async fn run(self) -> ForwardOutcome {
        #[cfg(test)]
        if let Some(hook) = self.proxy.warmup_run_hook.as_ref() {
            hook.arrived.fetch_add(1, Ordering::SeqCst);
            if let Ok(permit) = hook.gate.acquire().await {
                permit.forget();
            }
            if hook.panic_after_gate {
                panic!("injected panic in the warmup leader task");
            }
        }
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
    // 规范只定义了 chat 与 embedding 的最小请求：别的能力（rerank）的 resident 卡不发一个种类不对的请求，明确拒绝，什么也不发。
    let Some(kind) = request_kind(job.capability) else {
        return Err(with_headers(
            BadField::new(
                "unsupported_model",
                "warmup has a minimal request for chat and embedding only: a resident card for this capability cannot be warmed",
            )
            .into_response(),
            selected,
        ));
    };
    let (path, body) = match job.capability {
        Capability::Embedding => (
            BufferedUpstreamPath::EMBEDDINGS,
            json!({"model": target, "input": MINIMAL_INPUT}),
        ),
        // 到这里只剩 chat：`request_kind` 已经把 chat 与 embedding 之外的能力挡在上面。
        _ => (
            BufferedUpstreamPath::CHAT_COMPLETIONS,
            json!({
                "model": target,
                "messages": [{"role": "user", "content": MINIMAL_INPUT}],
                "max_tokens": 1,
            }),
        ),
    };
    // 复核：选卡已经把不在本机的卡排除了，这是请求真正发出去之前的最后一道，看的是 `ResidentRequest` 里要发往的那个地址。
    // 在登记合并表之前，所以过不去的卡不占名额、不留表项。
    if !is_local_endpoint(&selected.card) {
        return Err(not_local_response(selected));
    }
    let key = RequestKey::of(&selected.card, kind, target);
    let waiting = match state.proxy.warmups.join_or_lead(key, job.caller) {
        Join::Refused(limit) => return Err(limited_response(selected, limit)),
        // 同样的请求已经在途：并入它，不发请求、不占并发许可。
        Join::Follow(waiting) => {
            job.coalesced.store(true, Ordering::SeqCst);
            waiting
        }
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
    // 并入了别人发起的请求时置位，只用来在 200 的回答里标一个 `"coalesced": true`。
    let coalesced = AtomicBool::new(false);
    let response = handle(&state, &headers, identity, &record_id, &body, &coalesced).await;
    cancel_audit.defuse();
    response
}

async fn handle(
    state: &AppState,
    headers: &HeaderMap,
    identity: Option<&AuthenticatedVirtualKey>,
    record_id: &str,
    body: &Bytes,
    coalesced: &AtomicBool,
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
    // 候选限于本机的卡：订阅中转卡、远程与局域网的卡、端点指向别处的卡都不参与，隐私头是 `any` 也一样（`any` 放开的是聊天的远程候选，
    // 预热不跟）。在选卡之前排除，而不是选完再拒绝：排在前面的远程卡不再把同一角色下的本机卡挡住；没有本机候选时得到的是选卡管线自己
    // 的 503（缺省的 `local_only` 是 `local_only_unavailable`，`any` 是 `no_eligible_candidate`），什么也不发。
    let cards: Vec<_> = policy.cards.into_iter().filter(is_local_endpoint).collect();
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
    let caller = identity.map(|identity| identity.key_id.as_str());
    // 并发上限的预检（本地预审 L-3）：resident 预热要是已经注定被 429，就别再去拿账本的锁。顺序是 鉴权、本机候选、并发上限的预检、
    // 预算、最后才登记（`warm_resident` 里的 `join_or_lead`，那里再判断一次，以登记为准）：并入别人请求的预热同样先过自己的鉴权、
    // 候选与预算，并入不是绕开它们的后门；而一个会被拒的，也不必为它付账本的锁。
    if dispatch::is_resident_http_service(&selected.card)
        && let Some(kind) = request_kind(capability)
        && let Some(limit) = state
            .proxy
            .warmups
            .refusal(&RequestKey::of(&selected.card, kind, &target), caller)
    {
        return limited_response(&selected, limit);
    }
    // 预算（PR-Daemon 对 #634 的预审 W-2）：聊天对免费的本地模型也会过一遍账本（`SpendGate::All` 下余额为 0 是 402），预热不能是
    // 绕开它的后门——加载或驱逐一个模型、对 resident 卡发一个 token，都是这个租户在用本机。这里做一次不插入预留行的准入检查
    // （`check_admission`：不预留、不结算，但它要拿账本的锁、开一个写事务，所以不是零成本，才排在并发上限的预检之后）：预热本身不花钱，
    // 不占用预算，也不进用量记录。没有账本就不查（同聊天：没接账本的部署里免费调用照常可用）；默认的 `PaidOnly` 对免费调用放行。
    // 余额不足的错误与聊天是同一个函数映射出来的。
    if let Some(ledger) = state.budget_ledger.as_deref() {
        let budget_identity = budget::BudgetIdentity::resolve(
            caller,
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
        caller,
        record_id,
        capability,
        target: &target,
        started,
        coalesced,
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
    let mut answer = json!({
        "model": target,
        "provider_id": selected.card.provider.id,
        "state": "ready",
        "already_loaded": already_loaded,
        "load_ms": crate::elapsed_ms(started),
    });
    // 并入了别人发起的请求：说一声，这个 `load_ms` 是它自己等了多久，不是一个它发出去的请求的耗时（L-1，只增）。
    if coalesced.load(Ordering::SeqCst) {
        answer["coalesced"] = json!(true);
    }
    with_headers(Json(answer).into_response(), &selected)
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
            Join::Refused(limit) => panic!("{model}: expected to lead, but refused: {limit:?}"),
        }
    }

    fn follows(registry: &Arc<Registry>, model: &str, caller: Option<&str>) -> Waiting {
        match registry.join_or_lead(request(model), caller) {
            Join::Follow(waiting) => waiting,
            Join::Lead(..) => panic!("{model}: expected to join an in-flight request, but led"),
            Join::Refused(limit) => panic!("{model}: expected to join, but refused: {limit:?}"),
        }
    }

    /// 被拒就返回是哪一道上限；没被拒（领头或并入了）就让用例失败。被拒时什么也没登记。
    fn refused_by(registry: &Arc<Registry>, model: &str, caller: Option<&str>) -> Limit {
        match registry.join_or_lead(request(model), caller) {
            Join::Refused(limit) => limit,
            Join::Lead(..) => panic!("{model}: expected a refusal, but led"),
            Join::Follow(_) => panic!("{model}: expected a refusal, but joined"),
        }
    }

    fn is_refused(registry: &Arc<Registry>, model: &str, caller: Option<&str>) -> bool {
        matches!(
            registry.join_or_lead(request(model), caller),
            Join::Refused(_)
        )
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

    // ---- 只服务本机的卡（M1）：发请求之前的复核 ----

    /// 一张 resident 的 chat 卡，声明的一切都是本机的（loopback、`local_only`、出站 loopback）；端点由测试定。
    fn local_chat_card(endpoint: &str) -> ComponentCard {
        serde_yaml::from_str(&format!(
            r#"provider:
  id: recheck
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: {{ input_per_m: 0, output_per_m: 0 }}
  locality: loopback
form: http_service
endpoint: "{endpoint}"
version_pin: "openai_compat@unknown"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: {{ mode: resident, keepalive: {{ pinned: true }}, admission: coexist }}
"#
        ))
        .unwrap()
    }

    /// 绕过选卡阶段：直接把一张卡当作「已选中的候选」。
    fn selected_as_is(card: ComponentCard) -> Selected {
        Selected {
            decision: idoris_policy::Decision {
                chosen_id: card.provider.id.clone(),
                reason_codes: Vec::new(),
                degradations: Vec::new(),
            },
            served_locality: card.provider.locality,
            load_policy: card.load_policy.unwrap(),
            card,
            estimated_cost_minor: 0,
        }
    }

    /// 选卡阶段已经把不在本机的卡排除了，所以从 HTTP 入口看不到复核起作用；这里把选卡绕过去，直接交给 `warm_resident`：请求照样发不
    /// 出去，合并表里也不留名额与表项。两种卡各由判据的一半拦下：声明在远程的（端点碰巧在本机，由「声明」那一半拦下），
    /// 和声明在本机而端点指向别处的（由「地址」那一半拦下）。
    #[tokio::test]
    async fn the_recheck_refuses_a_card_that_is_not_on_this_machine_before_anything_is_sent() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&upstream)
            .await;
        let mut declared_remote = local_chat_card(&upstream.uri());
        declared_remote.provider.locality = Locality::Remote;
        let off_host = local_chat_card("http://warmup-recheck.invalid:9");
        for (what, card) in [
            ("declared remote, endpoint on this machine", declared_remote),
            ("declared local, endpoint elsewhere", off_host),
        ] {
            let state = AppState::default();
            let selected = selected_as_is(card);
            let coalesced = AtomicBool::new(false);
            let job = Job {
                privacy: PrivacyClass::Any,
                tenant_id: None,
                caller: Some("key"),
                record_id: "rec",
                capability: Capability::Chat,
                target: "model",
                started: Instant::now(),
                coalesced: &coalesced,
            };
            let refused = warm_resident(&state, &selected, &job).await.unwrap_err();
            assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE, "{what}");
            let body = refused.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body["error"]["type"], "no_eligible_candidate",
                "{what}: {body}"
            );
            assert!(
                state.proxy.warmups.is_idle(),
                "{what}: a refused card takes no slot and leaves no entry"
            );
            assert_eq!(
                state.proxy.egress().requests,
                0,
                "{what}: no attempt towards an address off this machine"
            );
        }
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "nothing reached the endpoint that is on this machine either"
        );
    }

    /// 对照：同一条路径上，一张声明与端点都在本机的卡过得了复核（复核没有把本机的卡也拒掉）。
    #[tokio::test]
    async fn the_recheck_lets_a_card_that_is_on_this_machine_through() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&upstream)
            .await;
        let state = AppState::default();
        let selected = selected_as_is(local_chat_card(&upstream.uri()));
        let coalesced = AtomicBool::new(false);
        let job = Job {
            privacy: PrivacyClass::LocalOnly,
            tenant_id: None,
            caller: None,
            record_id: "rec",
            capability: Capability::Chat,
            target: "model",
            started: Instant::now(),
            coalesced: &coalesced,
        };
        warm_resident(&state, &selected, &job).await.unwrap();
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        assert!(
            !coalesced.load(Ordering::SeqCst),
            "the one that sent the request did not join anyone's"
        );
    }

    // ---- 全局上限、并入者上限、预检（本地预审 L-3、L-4） ----

    /// 四把 key 各领头两个，正好是全局上限；返回它们的守卫（丢掉就还）。
    fn fill_the_global_limit(registry: &Arc<Registry>) -> Vec<(Lead, Waiting)> {
        let mut held = Vec::new();
        for key in ["key-a", "key-b", "key-c", "key-d"] {
            for index in 0..MAX_IN_FLIGHT_PER_KEY {
                held.push(leads(registry, &format!("{key}-{index}"), Some(key)));
            }
        }
        assert_eq!(held.len(), MAX_LEADERS_IN_FLIGHT);
        held
    }

    #[test]
    fn the_global_limit_counts_the_leaders_of_all_keys_and_followers_do_not_count() {
        let registry = new_registry();
        let mut held = fill_the_global_limit(&registry);
        assert_eq!(registry.requests_in_flight(), MAX_LEADERS_IN_FLIGHT);
        // 第五把 key 自己一个也没有，领头被全局上限拒绝；并入已经在途的请求不算领头，照样放行。
        assert_eq!(
            refused_by(&registry, "key-e-0", Some("key-e")),
            Limit::Global
        );
        let _joined = follows(&registry, "key-a-0", Some("key-e"));
        assert_eq!(registry.leading_for(Some("key-e")), 0);
        // 一个结束了，全局的名额回来，第五把 key 可以领头。
        let (finished, _) = held.remove(0);
        finished.finish(answer(200));
        let _fifth = leads(&registry, "key-e-0", Some("key-e"));
        assert_eq!(registry.requests_in_flight(), MAX_LEADERS_IN_FLIGHT);
    }

    #[test]
    fn the_per_key_limit_is_reported_before_the_global_one() {
        let registry = new_registry();
        let _held = fill_the_global_limit(&registry);
        // key-a 已经领着自己的两个，表也满了：报的是更具体的那一道。
        assert_eq!(refused_by(&registry, "more", Some("key-a")), Limit::Key);
        assert_eq!(refused_by(&registry, "more", Some("key-e")), Limit::Global);
    }

    #[test]
    fn a_request_takes_at_most_the_follower_limit_and_a_follower_that_stops_waiting_gives_its_place_back()
     {
        let registry = new_registry();
        let (_lead, _) = leads(&registry, "m", Some("key"));
        let mut followers: Vec<Waiting> = (0..MAX_FOLLOWERS_PER_REQUEST)
            .map(|index| follows(&registry, "m", Some(&format!("follower-{index}"))))
            .collect();
        assert_eq!(registry.followers_waiting(), MAX_FOLLOWERS_PER_REQUEST);
        // 再来一个并入者：被并入者上限拒绝，什么也没登记。
        assert_eq!(
            refused_by(&registry, "m", Some("one-more")),
            Limit::Followers
        );
        assert_eq!(registry.followers_waiting(), MAX_FOLLOWERS_PER_REQUEST);
        // 别的请求不受影响（它自己领头，不是这个请求的并入者）。
        let _other = leads(&registry, "other", Some("one-more"));
        // 一个并入者不再等了（丢掉 `Waiting`，等同于 `timeout_ms` 到点或客户端断开）：它的名额回来。
        drop(followers.pop());
        assert_eq!(registry.followers_waiting(), MAX_FOLLOWERS_PER_REQUEST - 1);
        let _again = follows(&registry, "m", Some("late"));
        assert_eq!(registry.followers_waiting(), MAX_FOLLOWERS_PER_REQUEST);
    }

    #[test]
    fn the_follower_count_belongs_to_the_request_it_was_counted_on() {
        let registry = new_registry();
        let (first, _) = leads(&registry, "m", Some("key"));
        // 它还占着旧请求上的名额（还没丢掉 `Waiting`），旧请求就结束了。
        let straggler = follows(&registry, "m", Some("late"));
        first.finish(answer(200));
        // 同一个模型的新请求从零个并入者开始，不管旧请求上落在后面的那个怎么样；它之后走掉也不动新请求的计数。
        let (_second, _) = leads(&registry, "m", Some("key"));
        assert_eq!(registry.followers_waiting(), 0);
        let _followers: Vec<Waiting> = (0..MAX_FOLLOWERS_PER_REQUEST)
            .map(|index| follows(&registry, "m", Some(&format!("follower-{index}"))))
            .collect();
        drop(straggler);
        assert_eq!(registry.followers_waiting(), MAX_FOLLOWERS_PER_REQUEST);
        assert!(is_refused(&registry, "m", Some("one-more")));
    }

    #[test]
    fn the_precheck_judges_like_registration_and_registers_nothing() {
        let registry = new_registry();
        assert_eq!(registry.refusal(&request("m"), Some("key")), None);
        assert!(registry.is_idle(), "asking registered nothing");
        let held = [
            leads(&registry, "m", Some("key")),
            leads(&registry, "n", Some("key")),
        ];
        // 这把 key 到了自己的上限：第三个模型会被拒，在途的两个模型仍然可以并入；问一问不会并入任何东西。
        assert_eq!(
            registry.refusal(&request("o"), Some("key")),
            Some(Limit::Key)
        );
        assert_eq!(registry.refusal(&request("m"), Some("key")), None);
        assert_eq!(registry.followers_waiting(), 0, "asking never joins");
        assert_eq!(registry.leading_for(Some("key")), MAX_IN_FLIGHT_PER_KEY);
        // 并入者上限：问一问同样说「会被拒」。
        let _followers: Vec<Waiting> = (0..MAX_FOLLOWERS_PER_REQUEST)
            .map(|index| follows(&registry, "m", Some(&format!("follower-{index}"))))
            .collect();
        assert_eq!(
            registry.refusal(&request("m"), Some("late")),
            Some(Limit::Followers)
        );
        drop(held);
    }

    // ---- 「本机」的判据（本地预审 L-6）与生命周期卡的复核（L-7） ----

    /// 把端点换成测试给的字符串（不经 YAML：反斜杠、`@`、`#` 在 YAML 的双引号里有自己的意思）。
    fn local_card_at(endpoint: &str) -> ComponentCard {
        let mut card = local_chat_card("http://127.0.0.1:8088");
        card.endpoint = endpoint.to_string();
        card
    }

    /// 判据随**真要连接的目标**走，不随字符串的样子走：端点交给 URL 解析器，连接落在解析出来的主机上。
    ///
    /// - 落在本机的各种写法：`localhost`（含全限定的 `localhost.`）、各种 loopback 地址（含 IPv4 映射的 IPv6 loopback，那条连接落在
    ///   `127.0.0.1`）、未指定的地址（`0.0.0.0`、`::` 作为目的地就是「本机」；在 macOS 上实测落在 `127.0.0.1` 与 `::1`），以及解析器
    ///   规范成 `127.0.0.1` 的数字写法；用户信息（`@` 之前）不是主机；`?` 与 `#` 之后的一切不是主机（代理把路径接在端点后面，
    ///   连接仍然落在端点的主机上）。
    /// - 不是本机，或者从 URL 看不出落在哪：主机是别处的（含 `127.0.0.1@evil.com` 这种主机其实是 `evil.com` 的）；除 `localhost`
    ///   以外的名字，由跑解析器的人说了算，不是 URL 自己的事实（`127.0.0.1.nip.io`、`localtest.me` 今天碰巧解析到 `127.0.0.1`，
    ///   `*.localhost` 按 RFC 6761 该是 loopback，可不是每个解析器都这样做）；没有协议头的写法解析不出主机，连不出去。
    #[test]
    fn is_local_endpoint_follows_where_the_connection_would_actually_land() {
        let lands_here = [
            "http://127.0.0.1:8088",
            "http://127.0.0.1:8088/v1/",
            "https://localhost:8088",
            "http://LOCALHOST:8088",
            "http://localhost.:8088",
            "http://[::1]:8088",
            "http://[0:0:0:0:0:0:0:1]:8088",
            "http://[::ffff:127.0.0.1]:8088",
            "http://[::ffff:7f00:1]:8088",
            "http://0.0.0.0:8088",
            "http://0:8088",
            "http://[::]:8088",
            "http://127.1:8088",
            "http://2130706433:8088",
            "http://0x7f.1:8088",
            "http://0177.0.0.1:8088",
            "http://%6c%6f%63%61%6c%68%6f%73%74:8088",
            "http://evil.com@127.0.0.1:8088",
            "http://127.0.0.1:8088?@evil.com",
            "http://127.0.0.1:8088#@evil.com",
        ];
        for endpoint in lands_here {
            assert!(
                is_local_endpoint(&local_card_at(endpoint)),
                "{endpoint} lands on this machine"
            );
        }
        let lands_elsewhere_or_nowhere_we_can_tell = [
            "http://127.0.0.1@evil.com:8088",
            "http://127.0.0.1:80@evil.com",
            "http://localhost@evil.com:8088",
            "http://evil.com\\@127.0.0.1",
            "http://127.0.0.1%2f@evil.com",
            "http://127.0.0.1.nip.io:8088",
            "http://localtest.me:8088",
            "http://foo.localhost:8088",
            "http://localhost.localdomain:8088",
            "http://10.0.0.5:8088",
            "http://192.168.1.2:8088",
            "http://8.8.8.8:8088",
            "http://[::ffff:8.8.8.8]:8088",
            "http://[::ffff:10.0.0.5]:8088",
            "https://api.openai.com/v1",
            "127.0.0.1:8088",
            "localhost:8088",
            "//127.0.0.1:8088",
            "",
            "not a url",
        ];
        for endpoint in lands_elsewhere_or_nowhere_we_can_tell {
            assert!(
                !is_local_endpoint(&local_card_at(endpoint)),
                "{endpoint} does not land on this machine, or nothing says it does"
            );
        }
    }

    /// 判据的另一半：卡自己也得声明是本机的（可信本地三条件），端点在 loopback 上也不够。
    #[test]
    fn is_local_endpoint_also_needs_the_card_to_claim_to_be_local() {
        assert!(is_local_endpoint(&local_card_at("http://127.0.0.1:8088")));
        type Change = fn(&mut ComponentCard);
        let claims: [(&str, Change); 7] = [
            ("declared remote", |card| {
                card.provider.locality = Locality::Remote;
            }),
            ("declared on the LAN", |card| {
                card.provider.locality = Locality::Lan;
            }),
            ("privacy class any", |card| {
                card.privacy_class = PrivacyClass::Any;
            }),
            ("may egress to the internet", |card| {
                card.allowed_egress = vec![idoris_contracts::component_card::Egress::Internet];
            }),
            ("may egress to the LAN", |card| {
                card.allowed_egress = vec![idoris_contracts::component_card::Egress::Lan];
            }),
            ("a spawned CLI", |card| {
                card.form = idoris_contracts::component_card::Form::SpawnCli;
            }),
            ("the subscription relay", |card| {
                card.provider.id = idoris_policy::SUBSCRIPTION_PROVIDER_ID.to_string();
            }),
        ];
        for (what, change) in claims {
            let mut card = local_card_at("http://127.0.0.1:8088");
            change(&mut card);
            assert!(
                !is_local_endpoint(&card),
                "{what}: the endpoint is on this machine, but the card does not claim to be"
            );
        }
    }

    /// 生命周期卡在加载之前也复核（本地预审 L-7）：选卡阶段被绕过时，一张端点指向别处的生命周期卡不碰任何运行时。
    /// 对照：同一张卡、端点在本机，走到了查 Supervisor 那一步（这里没有注册运行时，所以是 `supervisor_unavailable`）——所以前一个
    /// 回答是复核给的，不是别的什么给的。
    #[tokio::test]
    async fn a_lifecycle_card_whose_endpoint_is_not_on_this_machine_is_refused_before_the_runtime_is_asked()
     {
        let card: ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        let state = AppState::default();
        let error_type = |response: Response| async move {
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            (status, body["error"]["type"].as_str().unwrap().to_string())
        };

        let mut elsewhere = card.clone();
        elsewhere.endpoint = "http://warmup-lifecycle.invalid:8000".to_string();
        let refused = warm_lifecycle(&state, &selected_as_is(elsewhere), "model", false)
            .await
            .unwrap_err();
        assert_eq!(
            error_type(refused).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "no_eligible_candidate".to_string()
            )
        );

        let on_this_machine = warm_lifecycle(&state, &selected_as_is(card), "model", false)
            .await
            .unwrap_err();
        assert_eq!(
            error_type(on_this_machine).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "local_only_unavailable".to_string()
            ),
            "no runtime is registered for it: the lookup was reached"
        );
    }

    // ---- 最小请求不带幂等键（本地预审 L-1） ----

    /// 最小请求的 `request_id` 恒为 `None`（[`ResidentRequest::run`] 的不变式）：若带了，代理会把这个请求的结果按它登记进幂等缓存。
    #[tokio::test]
    async fn the_minimal_request_carries_no_idempotency_key_so_the_proxy_keeps_nothing_of_it() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(json!({"choices": [{"message": {"content": "x"}}]})),
            )
            .mount(&upstream)
            .await;
        let state = AppState::default();
        let selected = selected_as_is(local_chat_card(&upstream.uri()));
        let coalesced = AtomicBool::new(false);
        let job = Job {
            privacy: PrivacyClass::LocalOnly,
            tenant_id: None,
            caller: None,
            record_id: "the-callers-record-id",
            capability: Capability::Chat,
            target: "model",
            started: Instant::now(),
            coalesced: &coalesced,
        };
        warm_resident(&state, &selected, &job).await.unwrap();
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        assert!(
            state.proxy.cache.lock().unwrap().is_empty(),
            "nothing of a warmup request is remembered for replay"
        );
    }
}
