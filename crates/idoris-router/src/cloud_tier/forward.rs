//! 向云端发一次 `POST /api/v1/chat/completions`：非流式，一次，不重试。
//!
//! # 安全立场（和云上游适配器 `idoris-upstream::cloud::client` 同一套）
//!
//! - 请求是**从零构造**的：只发 `Authorization`、`Content-Type`、`Accept`、`Accept-Encoding`、`User-Agent` 五个头和一个 JSON 请求体。
//!   不转发调用方的任何头（尤其是调用方的本地虚拟 key、`X-iDoris-Tenant`、`X-iDoris-Request-Id`、会话头），也**不带** `X-iDoris-Member`：
//!   Node 凭证不能带成员头，设备凭证带了也只能是自己（S5 的交接说明），所以一律不带；
//! - **不跟随重定向**（重定向会把 `Authorization` 带去别的主机）、**不走代理**（`idoris_upstream::cloud::build_http_client`）；
//! - 响应体有大小上限；总期限由配置给；调用方断开（取消令牌）立刻丢弃整个请求，云端看到的是客户端断开；
//! - 密钥只在 `Authorization` 头里（标记为 sensitive），不进日志、不进任何错误。
//!
//! # 不重试，不重放
//!
//! 一次请求最多向云端发一次。失败不重试：请求一旦可能已经到达云端，再发一次就是第二笔账（云端的「用户只付一次」
//! 是对**一个请求**说的，两个请求是两次）。设计稿 §9：恢复连接后不补发、不重放离线期间的请求——这里没有队列，
//! 也没有任何东西会在以后替这个请求重新发。
//!
//! # 云端的答复怎么归类
//!
//! | 云端 | 本机归类（[`CallOutcome`]） |
//! |---|---|
//! | 2xx + JSON 对象 + 有 `X-iDoris-Record-Id` | `served` |
//! | 2xx 但不是可用的回答 | `invalid_response` |
//! | 401 | `unauthorized`（凭证被撤销、过期、不认识） |
//! | 402 | `budget_exhausted`（云账户额度用完） |
//! | 403 | `forbidden`（租户没开通、账户停用、成员关系撤销……） |
//! | 429 | `rate_limited` |
//! | 400 / 404 / 413 / 422 和其它 4xx | `rejected`（请求形状或模型名被拒） |
//! | 408 | `timeout` |
//! | 5xx、3xx、其它 | `cloud_error` |
//! | 连接不上（DNS、连接被拒、TLS） | `unreachable` |
//! | 总期限内没读完 | `timeout` |
//! | 2xx 但 `usage` 里的 token 数超出调用记录能存的范围（i64） | `invalid_response` |
//! | 出网前的最后一道门拦下了它；或熔断取消在「取消 / 正文开始交付」的原子竞争里**赢了**（请求体此后拒绝交付任何字节） | `cancelled`（[`CloudFailure::Withheld`]：调用方拿到本机原来的响应，没有 attempt 头） |
//! | 熔断打开时请求体已经**赢得**了交付（正文可能已经写出） | `cancelled`（[`CloudFailure::PossiblySent`]：调用方拿到本机原来的响应，并带 `X-iDoris-Cloud-Attempt: cancelled`） |

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use futures_util::StreamExt;
use idoris_tenancy::cloud_tier::CallOutcome;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, USER_AGENT,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::config::CloudTierConfig;
use super::credential::CloudCredential;
use super::gate::EgressPermit;

/// 出网的 `User-Agent`：固定，不含主机名、用户名、版本以外的任何信息。
pub const USER_AGENT_VALUE: &str = concat!("idoris-cloud-tier/", env!("CARGO_PKG_VERSION"));

/// 非 2xx 响应体读取上限：只为取原因码。
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// 云端答了、本机能交给调用方的回答。
#[derive(Debug)]
pub struct CloudAnswer {
    /// 云端的响应体，**原样**（不重新序列化）。
    pub body: Bytes,
    pub status: u16,
    /// 云端的 `X-iDoris-Record-Id`（它那边的请求 id）：对账指针。
    pub cloud_record_id: String,
    /// 云端的 `X-iDoris-Reason`（体验分级 / 失败切换的原因码，逗号连接）。
    pub reason: Option<String>,
    /// 云端的 `X-iDoris-Tier`（normal / degraded / grace）。
    pub service_tier: Option<String>,
    /// 云端压低 `max_tokens` 时发给上游的值（`X-iDoris-Max-Tokens`）。
    pub max_tokens: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

/// 没能拿到可用回答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudFailure {
    /// 连接不上：请求没有到达云端。
    Unreachable,
    /// 总期限内没有收到完整的回答。
    Timeout,
    /// 调用方在云端答复之前断开了。
    Cancelled,
    /// 云端答了，但不是可用的回答（带它的状态码、原因码和 record id，对账用）。
    Answered {
        outcome: CallOutcome,
        status: u16,
        reason: Option<String>,
        cloud_record_id: Option<String>,
    },
    /// 传输层在请求发出之后出了别的错（连接中途被重置……）：请求可能已经到达。
    Transport,
    /// 出网前的最后一道检查（远程熔断）把请求拦下了，或者熔断取消在「取消 / 正文开始交付」的原子竞争里**赢了**
    /// （`EgressLatch` 上的 `Idle -> Cancelled`）：请求体从此拒绝交付任何字节，请求头也没有被写出——**请求没有离开本机**。
    /// 只有赢了这场竞争才能说这句话；证明不了就是 [`PossiblySent`](CloudFailure::PossiblySent)。
    Withheld,
    /// 熔断打开时，请求体已经**赢得**了交付（`Idle -> Streaming`）：正文可能已经写进套接字，不能再当成零出网。
    /// 结局仍是 [`CallOutcome::Cancelled`]：调用记录表的 `outcome` CHECK 没有单独的取值，不加新结局。
    /// 调用方靠 `X-iDoris-Cloud-Attempt: cancelled` 知道「可能已经发出」；仅凭记录行分不出
    /// 这一支、[`Withheld`](CloudFailure::Withheld) 和调用方自己断开（三者都是 `cancelled`，没有云端 id、没有金额）。
    PossiblySent,
}

impl CloudFailure {
    /// 归到调用记录的结局。
    pub fn outcome(&self) -> CallOutcome {
        match self {
            CloudFailure::Unreachable => CallOutcome::Unreachable,
            CloudFailure::Timeout => CallOutcome::Timeout,
            CloudFailure::Cancelled => CallOutcome::Cancelled,
            CloudFailure::Answered { outcome, .. } => *outcome,
            CloudFailure::Transport => CallOutcome::CloudError,
            // 记成 cancelled：这一行在发送之前就落下了（先写后出网），它必须有终态；没有答复、没有云端 id、没有金额。
            // Withheld 是取消赢了、正文一个字节都没有交付；PossiblySent 是正文可能已经发出。表的 CHECK 没有第三种写法，所以两者都记 cancelled。
            CloudFailure::Withheld | CloudFailure::PossiblySent => CallOutcome::Cancelled,
        }
    }
}

/// 状态码 -> 结局（非 2xx）。纯函数，表驱动测试钉死。
pub fn outcome_for_status(status: u16) -> CallOutcome {
    match status {
        401 => CallOutcome::Unauthorized,
        402 => CallOutcome::BudgetExhausted,
        403 => CallOutcome::Forbidden,
        408 => CallOutcome::Timeout,
        429 => CallOutcome::RateLimited,
        400..=499 => CallOutcome::Rejected,
        _ => CallOutcome::CloudError,
    }
}

/// 云端响应头里的短字符串：可见 ASCII、不长。不合格就当没有（不转发、不记录）。
fn short_header(headers: &reqwest::header::HeaderMap, name: &str, max: usize) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    (!value.is_empty()
        && value.len() <= max
        && value.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
    .then(|| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

/// 云端的 record id：和调用记录表里的标识符形状一致。
fn record_id_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = short_header(headers, "x-idoris-record-id", 128)?;
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
        .then_some(value)
}

/// 原因码形状（和调用记录表一致）：小写字母、数字、`_`、`,`。
fn reason_shape(text: &str) -> Option<String> {
    (!text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b',')))
    .then(|| text.to_string())
}

/// 从云端的错误体里取稳定原因码（`error.reason_code`）；取不到就看 `X-iDoris-Reason` 头。
fn error_reason(body: &[u8], header: Option<&str>) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/reason_code")
                .and_then(Value::as_str)
                .and_then(reason_shape)
        })
        .or_else(|| header.and_then(reason_shape))
}

/// 云端 `usage` 里的 token 数（没有或不是非负整数就是 `None`）。
///
/// 数字必须能写进调用记录（`tokens_*` 列是 i64）：超出范围的值（云端的 JSON 里可以写任意 u64）整个返回 `None`，
/// 调用方把这个回答归为 `invalid_response`。不能留到写库的时候才发现——那时 `pending` 守卫已经解除，终态写不进去，
/// 这一行会永远 `pending`，对账游标越不过它（审查 r4 Medium#5）。
fn usage_of(body: &Value) -> Option<(Option<u64>, Option<u64>)> {
    let usage = body.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    let (tokens_in, tokens_out) = (count("prompt_tokens"), count("completion_tokens"));
    let storable = |tokens: Option<u64>| tokens.is_none_or(|value| i64::try_from(value).is_ok());
    (storable(tokens_in) && storable(tokens_out)).then_some((tokens_in, tokens_out))
}

fn classify_transport(error: &reqwest::Error) -> CloudFailure {
    if error.is_connect() {
        CloudFailure::Unreachable
    } else if error.is_timeout() {
        CloudFailure::Timeout
    } else {
        CloudFailure::Transport
    }
}

/// 出网边界上「此刻还能不能发」的答案：`true` = 还能发。
///
/// 共享（`Arc`）、可跨线程、`'static`：请求体在 hyper 的**后台连接任务**里被 poll，它要在**交付字节的那一刻**自己再问一次
/// （见 [`EgressLatch`]）；借来的闭包做不到这一点。
pub(super) type StillAllowed = Arc<dyn Fn() -> bool + Send + Sync>;

/// 发请求。`_permit` 是出网凭条（只有 `gate::grant` 能造）：没有它就发不了。
///
/// `still_allowed` 是**出网边界上的最后一道门**：请求构造好之后、第一次真正去发之前调用，返回 `false` 就什么都不发
/// （[`CloudFailure::Withheld`]）。凭条只证明「闸门在某一刻放行过」，而放行到发出之间隔着读凭证、占名额、写审计事件、
/// 落 `pending` 行这些让出点，远程熔断可能在这期间被打开（它是个共享的原子变量）；调用方在这里给出**此刻**的答案，
/// 而不是闸门那一刻的快照。
///
/// # 熔断和「正文开始交付」是同一个原子转换（审查 r4 复审、第六轮）
///
/// 这一下通过之后，`request.send()` 要等连接，并且要等云端把响应头送回来。请求正文是 hyper 在**独立的后台连接任务**里取走的：
/// 连接就绪之后，它先把请求头写进缓冲，**第一次 poll 请求体**，再一起写出。这个动作和这里的 `select!`（熔断分支）不在同一个任务里，
/// 没有天然的先后；只靠「轮询一个标志」做不到安全：
///
/// - 熔断轮询（10 毫秒一轮）和请求体交付之间有窗口：轮询看过之后熔断才打开，下一次轮询之前连接就绪，正文照样发出去；
/// - 熔断那一边「读到标志还没置位，于是报 `Withheld`」，和请求体「随后置位、取走正文、写出」之间没有任何同步：读标志不阻止后面的发送，
///   结果是正文实际外送了，却报成没出网（审查 r4 新问题）。
///
/// 所以两边不再靠读标志约定，而是在**同一个原子变量**（[`EgressLatch`]）上竞争同一个状态转换，只有一方能赢：
///
/// - 请求体第一次被 poll、要交付字节时：**先复核熔断此刻的值**（`still_allowed`），仍然放行才去抢 `Idle -> Streaming`；熔断已经打开、
///   或者抢输了，就拒绝交付——返回一个错误，一个字节都不给 hyper（请求头还在缓冲里，也不会被写出）；
/// - 熔断分支看到熔断打开：去抢 `Idle -> Cancelled`。抢到了，请求体此后永远拒绝交付，**这时才报 [`CloudFailure::Withheld`]**；
///   抢输了（请求体已经赢得交付），一律 [`CloudFailure::PossiblySent`]；
/// - 请求体因为熔断而拒绝时，`request.send()` 会带着错误返回；此时原子变量已经是 `Cancelled`，同样按 `Withheld` 返回，
///   而不是 `Transport`。
///
/// 因此 `Withheld` 蕴含「请求头和正文都没有写出」，不论后台连接任务什么时候才被调度；轮询间隔只影响多快放弃等待，不影响这个保证。
/// 熔断恰好在请求体复核通过**之后**才打开，是真的「在交付之后打开」，按 `PossiblySent` 处理——共享原子变量上不存在比这更细的切分。
/// `send` 无论怎么结束（成功、失败、超时、调用方断开、future 被丢弃）都会把还没交付的请求体取消（[`CancelOnExit`]）：
/// 它返回之后，后台任务不会再把正文补发出去。已经拿到响应不再收回。
///
/// 只有 `cloud_tier` 模块树里的代码能调它（`pub(super)`）；而 [`EgressPermit`] 只有 `gate::grant` 能造，它同样只对这个
/// 模块树可见：crate 外的代码调不到 `send`，也造不出凭条。文档测试钉住 `send` 的可见性
/// （`grant` 的在 [`super::gate::grant`] 的文档里）：
///
/// ```compile_fail
/// let _ = idoris_router::cloud_tier::forward::send;
/// ```
///
/// 对照（路径没写错）：
///
/// ```
/// let _: Option<idoris_router::cloud_tier::forward::CloudFailure> = None;
/// ```
pub(super) async fn send(
    client: &reqwest::Client,
    config: &CloudTierConfig,
    permit: EgressPermit,
    credential: &CloudCredential,
    body: &Value,
    cancel: &CancellationToken,
    still_allowed: StillAllowed,
) -> Result<CloudAnswer, CloudFailure> {
    send_probed(
        client,
        config,
        permit,
        credential,
        body,
        cancel,
        still_allowed,
        BodyProbe::default(),
    )
    .await
}

/// [`send`] 的实现。`probe` 在生产里是个空结构；测试用它在请求体做决定的前后停住、调整熔断轮询间隔，
/// 把「取消」和「正文开始交付」的各种交错确定性地摆出来。
#[allow(clippy::too_many_arguments)]
async fn send_probed(
    client: &reqwest::Client,
    config: &CloudTierConfig,
    _permit: EgressPermit,
    credential: &CloudCredential,
    body: &Value,
    cancel: &CancellationToken,
    still_allowed: StillAllowed,
    probe: BodyProbe,
) -> Result<CloudAnswer, CloudFailure> {
    let bytes = serde_json::to_vec(body).map_err(|_| CloudFailure::Transport)?;
    let content_length = bytes.len();
    let latch = Arc::new(EgressLatch::new());
    // 不论怎么离开这个函数（返回、超时、调用方断开、future 被丢弃），还没交付的请求体都不会再交付。
    let _settle = CancelOnExit(latch.clone());
    let poll_period = probe.poll_period();
    // 显式 Content-Length：流式 body 的 size_hint 是未知的，不设这个头 hyper 会改成 chunked。
    // 已有 Content-Length 时 hyper 按定长写正文，并且要等连接建立、开始写正文时才 poll 请求体
    // （hyper 1.11.1 `Client::set_length`，2026-11-12 核对：
    // https://github.com/hyperium/hyper/blob/v1.11.1/src/proto/h1/role.rs ）。
    // reqwest 0.13 的 `Body::wrap_stream`（本仓库 Cargo.lock 是 0.13.5，2026-11-12 核对：
    // https://docs.rs/reqwest/0.13.5/reqwest/struct.Body.html#method.wrap_stream ）。
    let body = reqwest::Body::wrap_stream(GatedBody {
        state: BodyState::Unsent(Bytes::from(bytes)),
        latch: latch.clone(),
        still_allowed: still_allowed.clone(),
        probe,
    });
    let request = client
        .post(config.chat_url.clone())
        .header(AUTHORIZATION, credential.authorization())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .header(ACCEPT_ENCODING, "identity")
        .header(CONTENT_LENGTH, content_length)
        .header(USER_AGENT, USER_AGENT_VALUE)
        .body(body);
    let run = async {
        if !still_allowed() {
            return Err(CloudFailure::Withheld);
        }
        // biased：发送先完成（响应头已经到了）就认这个结果，不再收回。
        let mut connecting = std::pin::pin!(request.send());
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CloudFailure::Cancelled),
            result = &mut connecting => match result {
                Ok(response) => read_answer(response, config.max_response_bytes).await,
                // 请求体在交付边界上看到熔断已经打开、拒绝了交付（原子变量已经是 `Cancelled`）：
                // 请求头和正文都没有写出，不是传输层的错
                Err(_) if latch.is_cancelled() => Err(CloudFailure::Withheld),
                Err(error) => Err(classify_transport(&error)),
            },
            // 熔断打开了：去抢 `Idle -> Cancelled`。抢到就是 `Withheld`（此后请求体拒绝交付任何字节），抢输了就是 `PossiblySent`
            () = await_revoked(&still_allowed, poll_period) => Err(latch.cancel()),
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CloudFailure::Cancelled),
        outcome = tokio::time::timeout(config.total_timeout, run) => match outcome {
            Ok(result) => result,
            Err(_) => Err(CloudFailure::Timeout),
        },
    }
}

/// 隔一会儿看一次熔断。打开了就结束，好让外面的 `select` 去取消发送。
/// 熔断是个原子变量，没有现成的唤醒；这一轮只决定「多快放弃等待」（10 毫秒一轮覆盖整段 `send()`：连上之前，以及正文已经写出、
/// 还在等响应头），**不**决定安全：请求体在交付字节的那一刻自己再复核熔断，取消和交付在同一个原子变量上竞争
/// （见 [`send`] 的说明），所以把这个间隔缩短、拉长，都不会让正文在熔断打开之后发出去，也不会让 `Withheld` 说错。
const KILL_SWITCH_POLL: Duration = Duration::from_millis(10);

async fn await_revoked(still_allowed: &StillAllowed, period: Duration) {
    loop {
        if !still_allowed() {
            return;
        }
        tokio::time::sleep(period).await;
    }
}

// 出网的原子状态。
const IDLE: u8 = 0;
const STREAMING: u8 = 1;
const CANCELLED: u8 = 2;

/// 「熔断取消成功」和「正文开始交付字节」是**同一个原子变量上两个互斥的转换**，谁先到谁赢，只有一方能赢：
///
/// ```text
///               begin_delivery()  请求体第一次被 poll，熔断仍然放行
///   Idle ───────────────────────────────▶ Streaming   正文可能已经写出；此后熔断只能报 PossiblySent
///     │
///     └───────────────────────────────▶ Cancelled   取消赢了；请求体此后拒绝交付任何字节；可以报 Withheld
///               cancel()  熔断分支看到熔断打开
/// ```
///
/// 状态不会倒退：`Streaming` 和 `Cancelled` 都是终态。
struct EgressLatch(AtomicU8);

impl EgressLatch {
    fn new() -> Self {
        Self(AtomicU8::new(IDLE))
    }

    /// 请求体要开始交付字节了。`true` = 可以交付（这一次赢得了 `Idle -> Streaming`，或者早就在交付中）；
    /// `false` = 取消已经赢了，一个字节都不能交。
    fn begin_delivery(&self) -> bool {
        matches!(
            self.0
                .compare_exchange(IDLE, STREAMING, Ordering::SeqCst, Ordering::SeqCst),
            Ok(_) | Err(STREAMING)
        )
    }

    /// 熔断要取消这次发送。赢了（`Idle -> Cancelled`，或者已经被取消）：[`CloudFailure::Withheld`]，
    /// 此后请求体拒绝交付任何字节；输了（请求体已经在交付）：[`CloudFailure::PossiblySent`]，正文可能已经写出。
    fn cancel(&self) -> CloudFailure {
        match self
            .0
            .compare_exchange(IDLE, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) | Err(CANCELLED) => CloudFailure::Withheld,
            Err(_) => CloudFailure::PossiblySent,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst) == CANCELLED
    }
}

/// `send` 结束（不论怎么结束）时取消还没交付的请求体：返回之后，后台连接任务不会再把正文补发出去。
/// 已经在交付（`Streaming`）的不受影响。
struct CancelOnExit(Arc<EgressLatch>);

impl Drop for CancelOnExit {
    fn drop(&mut self) {
        let _ = self.0.cancel();
    }
}

/// 请求体在交付边界上拒绝交付时给 hyper 的错误。不含任何请求内容或密钥。
#[derive(Debug)]
struct BodyRefused;

impl std::fmt::Display for BodyRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the request body was not delivered: the remote kill switch is on")
    }
}

impl std::error::Error for BodyRefused {}

enum BodyState {
    /// 正文还没交付。
    Unsent(Bytes),
    /// 已经交付（流到头了）。
    Sent,
    /// 拒绝了交付（熔断赢了，或者熔断此刻是开着的）。之后每一次 poll 都还是拒绝。
    Refused,
}

/// 交给 `reqwest::Body::wrap_stream` 的请求体：**第一次被 poll 就是交付边界**，在这里做决定，不在别处。
///
/// 它必须**立刻**给出结果（`Ready`），不能 `Pending`：hyper 在 poll 请求体返回 `Pending` 时会先把已经缓冲的请求头
/// （含 `Authorization`）冲出去。拒绝时返回的 `Err` 则不会：hyper 1.11.1 的 `Dispatcher::poll_write` 里，`write_head` 只把请求头
/// 写进缓冲，紧接着在同一个循环里 poll 请求体，请求体出错就直接返回错误，连接被丢弃时缓冲里的请求头没有被冲出去
/// （2026-11-17 核对：https://github.com/hyperium/hyper/blob/v1.11.1/src/proto/h1/dispatch.rs ）。
/// 这是依赖的行为，不是我们的保证：`interleaving` 测试里「云端收到 0 字节」的断言就是钉它的，升级 hyper 之后若变了，
/// 那些测试会失败。
struct GatedBody {
    state: BodyState,
    latch: Arc<EgressLatch>,
    still_allowed: StillAllowed,
    probe: BodyProbe,
}

impl futures_util::Stream for GatedBody {
    type Item = Result<Bytes, BodyRefused>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match std::mem::replace(&mut this.state, BodyState::Sent) {
            BodyState::Sent => std::task::Poll::Ready(None),
            BodyState::Refused => {
                this.state = BodyState::Refused;
                std::task::Poll::Ready(Some(Err(BodyRefused)))
            }
            BodyState::Unsent(chunk) => {
                this.probe.before_decision();
                // 交付边界上的复核：熔断此刻是不是开着。开着就是取消赢了（我们自己去抢 `Idle -> Cancelled`，
                // 熔断分支的轮询来不来得及看到，不影响结果）；还放行才去抢 `Idle -> Streaming`，抢输了说明取消已经赢了。
                let deliver = if (this.still_allowed)() {
                    this.latch.begin_delivery()
                } else {
                    let _ = this.latch.cancel();
                    false
                };
                if !deliver {
                    this.state = BodyState::Refused;
                    return std::task::Poll::Ready(Some(Err(BodyRefused)));
                }
                this.probe.delivery_won();
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
        }
    }
}

/// 测试注入点。生产里是个空结构，什么都不做。
#[derive(Clone, Default)]
struct BodyProbe {
    /// 请求体第一次被 poll、还没做决定之前调用（可以停住这个线程）。
    #[cfg(test)]
    before_decision: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 请求体赢得 `Idle -> Streaming`、还没把字节交给 hyper 之前调用。
    #[cfg(test)]
    delivery_won: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 熔断轮询的间隔（缺省 [`KILL_SWITCH_POLL`]）。拉得很长，取消那一边就来不及看：这时拦住正文的只有交付边界上的复核。
    #[cfg(test)]
    poll_period: Option<Duration>,
}

impl BodyProbe {
    fn before_decision(&self) {
        #[cfg(test)]
        if let Some(hook) = &self.before_decision {
            hook();
        }
    }

    fn delivery_won(&self) {
        #[cfg(test)]
        if let Some(hook) = &self.delivery_won {
            hook();
        }
    }

    fn poll_period(&self) -> Duration {
        #[cfg(test)]
        if let Some(period) = self.poll_period {
            return period;
        }
        KILL_SWITCH_POLL
    }
}

async fn read_answer(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> Result<CloudAnswer, CloudFailure> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let success = (200..300).contains(&status);
    let cap = if success {
        max_response_bytes
    } else {
        MAX_ERROR_BODY_BYTES.min(max_response_bytes)
    };
    let cloud_record_id = record_id_header(&headers);
    let reason_header = short_header(&headers, "x-idoris-reason", 128);
    let answered = |outcome: CallOutcome, reason: Option<String>| CloudFailure::Answered {
        outcome,
        status,
        reason,
        cloud_record_id: cloud_record_id.clone(),
    };

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) if error.is_timeout() => return Err(CloudFailure::Timeout),
            // 云端已经答了，但回答没有完整地到本机：用不了
            Err(_) => return Err(answered(CallOutcome::InvalidResponse, None)),
        };
        if body.len().saturating_add(chunk.len()) > cap {
            return Err(answered(
                if success {
                    CallOutcome::InvalidResponse
                } else {
                    outcome_for_status(status)
                },
                None,
            ));
        }
        body.extend_from_slice(&chunk);
    }

    if !success {
        let reason = error_reason(&body, reason_header.as_deref());
        return Err(answered(outcome_for_status(status), reason));
    }
    // 2xx：必须是 JSON、必须是一个对象、必须有 choices、必须有云端 record id（对账指针）
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"));
    let parsed: Option<Value> = is_json
        .then(|| serde_json::from_slice(&body).ok())
        .flatten();
    let usable = parsed
        .as_ref()
        .is_some_and(|value| value.get("choices").is_some_and(Value::is_array));
    let (Some(parsed), true, Some(cloud_record_id)) = (parsed, usable, cloud_record_id.clone())
    else {
        return Err(answered(CallOutcome::InvalidResponse, None));
    };
    let Some((tokens_in, tokens_out)) = usage_of(&parsed) else {
        return Err(answered(CallOutcome::InvalidResponse, None));
    };
    Ok(CloudAnswer {
        body: Bytes::from(body),
        status,
        cloud_record_id,
        reason: reason_header.as_deref().and_then(reason_shape),
        service_tier: short_header(&headers, "x-idoris-tier", 32),
        max_tokens: short_header(&headers, "x-idoris-max-tokens", 16)
            .filter(|value| value.bytes().all(|b| b.is_ascii_digit())),
        tokens_in,
        tokens_out,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::cloud_tier::gate::{Consent, GateInput, LocalFailure, PrivacyFacts, grant};
    use idoris_backend::BackendError;
    use idoris_contracts::common::PrivacyClass;

    fn secret() -> String {
        format!("idk_{}", "a".repeat(64))
    }

    fn config(server: &MockServer, extra: &str) -> CloudTierConfig {
        CloudTierConfig::parse(
            &format!(
                "schema_version: 1\nbase_url: {}/api\nmodel: idoris/agent\n{extra}",
                server.uri()
            ),
            Path::new("."),
        )
        .unwrap()
    }

    /// 造一张凭条：和生产一样走 `grant`（第 2 层用一个现成的 `Selected`）。
    fn permit() -> EgressPermit {
        let error = BackendError::SupervisorUnavailable;
        let input = GateInput {
            privacy: PrivacyFacts {
                effective: Some(PrivacyClass::Any),
                pinned_local_by_content: false,
                kill_switch: false,
            },
            dev_no_key: false,
            consent: Consent::Given,
            stream: false,
            failure: LocalFailure::Backend(&error),
            cloud_model: Some("m"),
        };
        let card = serde_yaml::from_str(include_str!(
            "../../../../config/components/subscription.yaml"
        ))
        .unwrap();
        grant(&input, || {
            Ok(crate::dispatch::Selected {
                decision: idoris_policy::Decision {
                    chosen_id: "x".into(),
                    reason_codes: Vec::new(),
                    degradations: Vec::new(),
                },
                served_locality: idoris_contracts::provider::Locality::Remote,
                card,
                load_policy: idoris_contracts::load_policy::LoadPolicy {
                    mode: idoris_contracts::load_policy::LoadMode::Resident,
                    keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
                    admission: idoris_contracts::load_policy::Admission::Coexist,
                },
                estimated_cost_minor: 0,
            })
        })
        .unwrap()
        .0
    }

    fn client(config: &CloudTierConfig) -> reqwest::Client {
        idoris_upstream::cloud::build_http_client(config.connect_timeout).unwrap()
    }

    /// 把一个闭包包成 `send` 要的共享形状。
    fn still(check: impl Fn() -> bool + Send + Sync + 'static) -> StillAllowed {
        Arc::new(check)
    }

    async fn call(server: &MockServer, extra: &str) -> Result<CloudAnswer, CloudFailure> {
        let config = config(server, extra);
        send(
            &client(&config),
            &config,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "hi"}]}),
            &CancellationToken::new(),
            still(|| true),
        )
        .await
    }

    fn ok_body() -> serde_json::Value {
        json!({"id": "c", "choices": [{"message": {"role": "assistant", "content": "hello"}}],
               "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42}})
    }

    #[tokio::test]
    async fn a_good_answer_is_passed_through_byte_for_byte_with_its_reconciliation_facts() {
        let server = MockServer::start().await;
        let raw = br#"{"id":"c","choices":[{"message":{"content":"x"}}],"usage":{"prompt_tokens":12,"completion_tokens":30}}"#;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("x-idoris-record-id", "0b9f5c1e-3d71-4f2a-8f55-bbbbbbbbbbbb")
                    .insert_header("x-idoris-tier", "degraded")
                    .insert_header("x-idoris-reason", "budget_degraded")
                    .insert_header("x-idoris-max-tokens", "512")
                    .set_body_raw(raw.as_slice(), "application/json"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let answer = call(&server, "").await.unwrap();
        assert_eq!(
            answer.body.as_ref(),
            raw.as_slice(),
            "forwarded verbatim, not re-serialized"
        );
        assert_eq!(answer.status, 200);
        assert_eq!(
            answer.cloud_record_id,
            "0b9f5c1e-3d71-4f2a-8f55-bbbbbbbbbbbb"
        );
        assert_eq!((answer.tokens_in, answer.tokens_out), (Some(12), Some(30)));
        assert_eq!(answer.service_tier.as_deref(), Some("degraded"));
        assert_eq!(answer.reason.as_deref(), Some("budget_degraded"));
        assert_eq!(answer.max_tokens.as_deref(), Some("512"));
    }

    #[tokio::test]
    async fn only_five_headers_and_the_json_body_go_out_and_never_the_member_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "rec-1")
                    .set_body_raw(ok_body().to_string(), "application/json"),
            )
            .mount(&server)
            .await;
        call(&server, "").await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let mut names: Vec<String> = requests[0]
            .headers
            .iter()
            .map(|(name, _)| name.as_str().to_ascii_lowercase())
            .collect();
        names.sort();
        names.dedup();
        // reqwest 自己会加 host / content-length
        for allowed in [
            "accept",
            "accept-encoding",
            "authorization",
            "content-length",
            "content-type",
            "host",
            "user-agent",
        ] {
            names.retain(|name| name != allowed);
        }
        assert!(names.is_empty(), "unexpected headers leaked: {names:?}");
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", secret())
        );
        assert_eq!(
            requests[0]
                .headers
                .get("accept-encoding")
                .unwrap()
                .to_str()
                .unwrap(),
            "identity"
        );
        assert!(
            requests[0]
                .headers
                .get("user-agent")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("idoris-cloud-tier/")
        );
        assert!(requests[0].headers.get("x-idoris-member").is_none());
        assert_eq!(requests[0].method.as_str(), "POST");
        assert_eq!(requests[0].url.path(), "/api/v1/chat/completions");
    }

    #[test]
    fn the_status_table_is_pinned() {
        for (status, outcome) in [
            (401, CallOutcome::Unauthorized),
            (402, CallOutcome::BudgetExhausted),
            (403, CallOutcome::Forbidden),
            (408, CallOutcome::Timeout),
            (429, CallOutcome::RateLimited),
            (400, CallOutcome::Rejected),
            (404, CallOutcome::Rejected),
            (413, CallOutcome::Rejected),
            (422, CallOutcome::Rejected),
            (499, CallOutcome::Rejected),
            (500, CallOutcome::CloudError),
            (502, CallOutcome::CloudError),
            (503, CallOutcome::CloudError),
            (504, CallOutcome::CloudError),
            (301, CallOutcome::CloudError),
            (307, CallOutcome::CloudError),
        ] {
            assert_eq!(outcome_for_status(status), outcome, "{status}");
        }
    }

    #[tokio::test]
    async fn every_refusal_is_a_failure_with_the_cloud_reason_and_record_id_when_it_has_them() {
        let server = MockServer::start().await;
        for (status, reason, outcome) in [
            (401, "unauthenticated", CallOutcome::Unauthorized),
            (402, "budget_exhausted", CallOutcome::BudgetExhausted),
            (403, "tenant_not_provisioned", CallOutcome::Forbidden),
            (429, "request_rate_limited", CallOutcome::RateLimited),
            (400, "unsupported_model", CallOutcome::Rejected),
            (503, "cloud_unavailable", CallOutcome::CloudError),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("x-idoris-record-id", "rec-9")
                        .set_body_json(json!({"error": {"type": "x", "reason_code": reason}})),
                )
                .mount(&server)
                .await;
            assert_eq!(
                call(&server, "").await.unwrap_err(),
                CloudFailure::Answered {
                    outcome,
                    status,
                    reason: Some(reason.to_string()),
                    cloud_record_id: Some("rec-9".to_string())
                },
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn an_error_body_cannot_smuggle_text_into_the_recorded_reason() {
        let server = MockServer::start().await;
        let sneaky = "please summarise my medical history";
        for body in [
            json!({"error": {"reason_code": sneaky}}).to_string(),
            json!({"error": {"reason_code": "Mixed Case"}}).to_string(),
            "not json at all".to_string(),
            String::new(),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(502).set_body_string(body.clone()))
                .mount(&server)
                .await;
            assert_eq!(
                call(&server, "").await.unwrap_err(),
                CloudFailure::Answered {
                    outcome: CallOutcome::CloudError,
                    status: 502,
                    reason: None,
                    cloud_record_id: None
                },
                "{body:?}"
            );
        }
        // 云端自己在 X-iDoris-Reason 头里给的码也要过同样的形状检查
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(402).insert_header("x-idoris-reason", "budget_exhausted"),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            call(&server, "").await.unwrap_err(),
            CloudFailure::Answered { reason: Some(r), .. } if r == "budget_exhausted"
        ));
    }

    #[tokio::test]
    async fn a_success_that_is_not_a_usable_answer_is_an_invalid_response() {
        let server = MockServer::start().await;
        let cases: Vec<(&str, ResponseTemplate)> = vec![
            (
                "html",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("<html>", "text/html"),
            ),
            (
                "not json",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("nope", "application/json"),
            ),
            (
                "array",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("[]", "application/json"),
            ),
            (
                "no choices",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw("{}", "application/json"),
            ),
            (
                "choices not an array",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw(r#"{"choices":1}"#, "application/json"),
            ),
            (
                "no record id",
                ResponseTemplate::new(200).set_body_raw(ok_body().to_string(), "application/json"),
            ),
            (
                "bad record id",
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "has space")
                    .set_body_raw(ok_body().to_string(), "application/json"),
            ),
        ];
        for (name, template) in cases {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(template)
                .mount(&server)
                .await;
            match call(&server, "").await.unwrap_err() {
                CloudFailure::Answered {
                    outcome: CallOutcome::InvalidResponse,
                    status: 200,
                    ..
                } => {}
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    /// `forward` 交出去的每一个 `Ok(CloudAnswer)` 都必须能被调用记录原样写成 `served`；记录存不下的（超出 i64 的用量）
    /// 在这里就被拒绝成 `invalid_response`，而不是留到写库的时候才发现（审查 r4 Medium#5）。
    #[tokio::test]
    async fn every_answer_forward_accepts_can_be_recorded_and_the_ones_the_record_cannot_hold_are_refused()
     {
        use idoris_tenancy::cloud_tier::{CallFinish, CloudTierStore};
        let server = MockServer::start().await;
        let store = CloudTierStore::open_in_memory().unwrap();
        let max = i64::MAX;
        // (名字, usage, 记录存得下吗)
        let cases: Vec<(&str, serde_json::Value, bool)> = vec![
            ("no usage at all", json!(null), true),
            (
                "zero",
                json!({"prompt_tokens": 0, "completion_tokens": 0}),
                true,
            ),
            (
                "i64::MAX",
                json!({"prompt_tokens": max, "completion_tokens": max}),
                true,
            ),
            (
                "prompt_tokens = i64::MAX + 1",
                json!({"prompt_tokens": u64::try_from(max).unwrap() + 1, "completion_tokens": 1}),
                false,
            ),
            (
                "completion_tokens = i64::MAX + 1",
                json!({"prompt_tokens": 1, "completion_tokens": u64::try_from(max).unwrap() + 1}),
                false,
            ),
            (
                "u64::MAX",
                json!({"prompt_tokens": u64::MAX, "completion_tokens": 0}),
                false,
            ),
            // 不是非负整数的值当作没有（和以前一样），不是错误
            ("negative", json!({"prompt_tokens": -1}), true),
            ("fraction", json!({"prompt_tokens": 1.5}), true),
            ("string", json!({"prompt_tokens": "12"}), true),
        ];
        for (n, (name, usage, storable)) in cases.into_iter().enumerate() {
            let mut body = json!({"id": "c", "choices": [{"message": {"content": "x"}}]});
            if !usage.is_null() {
                body["usage"] = usage;
            }
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-idoris-record-id", format!("rec-{n}").as_str())
                        .set_body_raw(body.to_string(), "application/json"),
                )
                .mount(&server)
                .await;
            match call(&server, "").await {
                Ok(answer) => {
                    assert!(
                        storable,
                        "{name}: a usage the record cannot hold was accepted"
                    );
                    let ticket = store.begin_call(&format!("local-{n}"), 1, 1).unwrap();
                    store
                        .finish_call(
                            &ticket,
                            &CallFinish {
                                outcome: CallOutcome::Served,
                                finished_at_ms: 2,
                                cloud_record_id: Some(&answer.cloud_record_id),
                                http_status: Some(answer.status),
                                cloud_reason: answer.reason.as_deref(),
                                tokens_in: answer.tokens_in,
                                tokens_out: answer.tokens_out,
                            },
                        )
                        .unwrap_or_else(|error| panic!("{name}: {error}"));
                }
                Err(failure) => {
                    assert!(!storable, "{name}: refused although the record can hold it");
                    assert!(
                        matches!(
                            failure,
                            CloudFailure::Answered {
                                outcome: CallOutcome::InvalidResponse,
                                status: 200,
                                ..
                            }
                        ),
                        "{name}: {failure:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn an_oversized_answer_is_refused_and_a_redirect_is_never_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "r")
                    .set_body_raw(vec![b'x'; 2048], "application/json"),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            call(&server, "limits: {max_response_bytes: 1024}\n")
                .await
                .unwrap_err(),
            CloudFailure::Answered {
                outcome: CallOutcome::InvalidResponse,
                ..
            }
        ));

        let outside = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&outside)
            .await;
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/capture", outside.uri())),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            call(&server, "").await.unwrap_err(),
            CloudFailure::Answered {
                outcome: CallOutcome::CloudError,
                status: 307,
                ..
            }
        ));
        assert!(outside.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_listening_is_unreachable_and_a_stalled_answer_times_out() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dead = CloudTierConfig::parse(
            &format!("schema_version: 1\nbase_url: http://127.0.0.1:{port}/api\nmodel: m\n"),
            Path::new("."),
        )
        .unwrap();
        let outcome = send(
            &client(&dead),
            &dead,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({}),
            &CancellationToken::new(),
            still(|| true),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Unreachable);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let slow = config(
            &server,
            "limits: {connect_timeout_ms: 100, total_timeout_ms: 200}\n",
        );
        let started = std::time::Instant::now();
        let outcome = send(
            &client(&slow),
            &slow,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({}),
            &CancellationToken::new(),
            still(|| true),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Timeout);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn the_caller_going_away_drops_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let config = config(&server, "");
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let outcome = send(
            &client(&config),
            &config,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({}),
            &cancel,
            still(|| true),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(CloudFailure::Cancelled.outcome(), CallOutcome::Cancelled);
    }

    #[tokio::test]
    async fn a_request_the_last_guard_refuses_is_not_sent_at_all() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "rec-1")
                    .set_body_raw(ok_body().to_string(), "application/json"),
            )
            .mount(&server)
            .await;
        let config = config(&server, "");
        let outcome = send(
            &client(&config),
            &config,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({"model": "idoris/agent", "messages": []}),
            &CancellationToken::new(),
            still(|| false),
        )
        .await;
        assert_eq!(outcome.unwrap_err(), CloudFailure::Withheld);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a refused request must not reach the network"
        );
        // 对照：同样的请求，最后一道门放行就发出去
        let served = send(
            &client(&config),
            &config,
            permit(),
            &CloudCredential::parse(&secret()).unwrap(),
            &json!({"model": "idoris/agent", "messages": []}),
            &CancellationToken::new(),
            still(|| true),
        )
        .await;
        assert!(served.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// 把接受队列填满，让下一个连接停在 SYN_SENT：握手还没完成，请求正文不可能写进套接字。
    fn saturate_accept_queue() -> (
        std::net::TcpListener,
        std::net::SocketAddr,
        Vec<std::net::TcpStream>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut fillers = Vec::new();
        loop {
            match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
                Ok(stream) => fillers.push(stream),
                Err(_) => break,
            }
            assert!(
                fillers.len() < 4096,
                "the accept queue never filled; a later connect would not block"
            );
        }
        assert!(
            !fillers.is_empty(),
            "expected the listener to queue at least one connection"
        );
        (listener, addr, fillers)
    }

    fn waiting_payloads(listener: &std::net::TcpListener) -> Vec<Vec<u8>> {
        use std::io::Read;
        listener.set_nonblocking(true).unwrap();
        let mut payloads = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(30)));
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    buf.truncate(n);
                    payloads.push(buf);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("accept: {error}"),
            }
        }
        payloads
    }

    /// 审查 r4#1 复审：最后一道门已经放过，`send().await` 还堵在连接上时熔断打开。
    /// 云端收不到完整请求（队列里的连接都是测试自己填进去的空连接），调用按没出网返回。
    #[tokio::test]
    async fn a_kill_switch_opened_during_connect_cancels_before_the_request_is_written() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (listener, addr, _fillers) = saturate_accept_queue();
        let config = CloudTierConfig::parse(
            &format!(
                "schema_version: 1\nbase_url: http://{addr}/api\nmodel: idoris/agent\n\
                 limits: {{connect_timeout_ms: 20000, total_timeout_ms: 30000}}\n"
            ),
            Path::new("."),
        )
        .unwrap();
        let opened = Arc::new(AtomicBool::new(false));
        let flag = opened.clone();
        let task = tokio::spawn(async move {
            let started = std::sync::Mutex::new(None);
            let still_allowed = move || {
                // 第一次（进 send 之前的那道门）放行，并记下时刻；150ms 之后视为熔断已打开。
                // 这 150ms 里连接因为队列满了还停在 SYN_SENT，正文还没写。
                let mut slot = started.lock().unwrap();
                let start = *slot.get_or_insert_with(std::time::Instant::now);
                !flag.load(Ordering::SeqCst) && start.elapsed() < Duration::from_millis(150)
            };
            send(
                &client(&config),
                &config,
                permit(),
                &CloudCredential::parse(&secret()).unwrap(),
                &json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "hi"}]}),
                &CancellationToken::new(),
                Arc::new(still_allowed),
            )
            .await
        });
        // 连接等待期间打开熔断（和上面 150ms 的时限是同一件事；这里再显式拨一次，贴近「共享原子变量被写上」）。
        tokio::time::sleep(Duration::from_millis(80)).await;
        opened.store(true, Ordering::SeqCst);
        let outcome = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("send stayed blocked in connect after the kill switch opened");
        assert_eq!(outcome.unwrap().unwrap_err(), CloudFailure::Withheld);
        let payloads = waiting_payloads(&listener);
        assert!(
            payloads.iter().all(|bytes| !bytes.starts_with(b"POST")),
            "the cloud accepted a request body although the switch opened during connect: {payloads:?}"
        );
    }

    /// 假云端把整个请求正文读完之后一直不回应，这时再打开熔断。
    /// `send()` 要等响应头，正文其实已经离开本机。不能再报 [`CloudFailure::Withheld`]。
    #[tokio::test]
    async fn a_kill_switch_after_the_body_was_fully_read_is_not_reported_as_withheld() {
        use std::io::Read;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let body =
            json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "hi"}]});
        let expected = serde_json::to_vec(&body).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let seen = ready.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let buf = read_full_http_request(&mut stream);
            seen.store(true, Ordering::SeqCst);
            // 不写任何响应，把连接留着，让客户端一直等响应头。
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut rest = [0u8; 64];
            let _ = stream.read(&mut rest);
            buf
        });

        let config = CloudTierConfig::parse(
            &format!(
                "schema_version: 1\nbase_url: http://{addr}/api\nmodel: idoris/agent\n\
                 limits: {{connect_timeout_ms: 2000, total_timeout_ms: 30000}}\n"
            ),
            Path::new("."),
        )
        .unwrap();
        let opened = Arc::new(AtomicBool::new(false));
        let flag = opened.clone();
        let task = tokio::spawn(async move {
            send(
                &client(&config),
                &config,
                permit(),
                &CloudCredential::parse(&secret()).unwrap(),
                &body,
                &CancellationToken::new(),
                still(move || !flag.load(Ordering::SeqCst)),
            )
            .await
        });
        let started = std::time::Instant::now();
        while !ready.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the fake cloud never finished reading the request body"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        opened.store(true, Ordering::SeqCst);
        let outcome = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("send stayed blocked after the body had left and the kill switch opened")
            .unwrap()
            .unwrap_err();
        let received = server.join().unwrap();
        assert!(
            received
                .windows(expected.len())
                .any(|window| window == expected),
            "the fake cloud did not receive the full body: {received:?}"
        );
        assert_eq!(outcome, CloudFailure::PossiblySent);
        assert_eq!(outcome.outcome(), CallOutcome::Cancelled);
    }

    /// 原子状态只有一方能赢，而且状态不会倒退。
    #[test]
    fn the_egress_latch_lets_exactly_one_side_win() {
        // 取消先到：之后一个字节都不能交
        let latch = EgressLatch::new();
        assert_eq!(latch.cancel(), CloudFailure::Withheld);
        assert!(!latch.begin_delivery(), "a cancelled latch never delivers");
        assert!(!latch.begin_delivery());
        assert_eq!(latch.cancel(), CloudFailure::Withheld, "cancelling twice");
        assert!(latch.is_cancelled());
        // 交付先到：取消只能报「可能已经发出」，而且撤不回已经开始的交付
        let latch = EgressLatch::new();
        assert!(latch.begin_delivery());
        assert!(latch.begin_delivery(), "already delivering");
        assert_eq!(latch.cancel(), CloudFailure::PossiblySent);
        assert!(!latch.is_cancelled());
        assert!(
            latch.begin_delivery(),
            "a late cancel does not undo a delivery"
        );
        assert_eq!(latch.cancel(), CloudFailure::PossiblySent);
    }

    /// 两个线程同时抢：要么（请求体交付、取消报 PossiblySent），要么（请求体拒绝、取消报 Withheld）。
    /// 绝不会出现「交付了却报 Withheld」，也不会出现「拒绝了却报 PossiblySent」。
    #[test]
    fn the_egress_latch_never_lets_both_sides_win_under_contention() {
        for round in 0..5000 {
            let latch = Arc::new(EgressLatch::new());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (mine, gate) = (latch.clone(), barrier.clone());
            let body = std::thread::spawn(move || {
                gate.wait();
                mine.begin_delivery()
            });
            let (mine, gate) = (latch.clone(), barrier.clone());
            let cancel = std::thread::spawn(move || {
                gate.wait();
                mine.cancel()
            });
            let delivered = body.join().unwrap();
            let outcome = cancel.join().unwrap();
            assert_eq!(
                delivered,
                outcome == CloudFailure::PossiblySent,
                "round {round}: delivered={delivered} but the cancel reported {outcome:?}"
            );
        }
    }

    /// 第六轮（审查 r4 新问题 + r4#1 复审）：「熔断取消成功」和「正文开始交付字节」的确定性交错。
    ///
    /// 两个窗口，都不是轮询能封住的：
    /// - 熔断在「熔断轮询看过之后、连接就绪之前」打开，连接又在下一次轮询之前就绪，hyper 取走正文并发送——
    ///   请求体在交付字节的那一刻必须自己再复核熔断；
    /// - 熔断分支读到「请求体还没被 poll」就报 `Withheld`，而后台连接任务随后才 poll 请求体、写出正文——
    ///   「取消成功」和「开始交付」必须是同一个原子变量上的两个互斥转换，不能是「读一个标志」。
    ///
    /// 测试用 [`BodyProbe`] 把请求体停在做决定之前 / 之后（std 的阻塞通道，停的是 hyper 的连接任务所在的线程，
    /// 所以用多线程运行时），在那一刻打开熔断，再放行。假云端从不回应，只记它收到的字节。
    mod interleaving {
        use std::net::SocketAddr;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::{Duration, Instant};

        use super::*;

        type Allowed = Arc<dyn Fn() -> bool + Send + Sync>;

        /// 假云端：接受一个连接，一直读到对方关闭（或 5 秒），返回读到的全部字节。从不回应，也从不先关连接。
        /// 没有连接进来（`send` 在连接之前就被拦下）时，`finish` 让它尽快收工。
        struct SilentCloud {
            addr: SocketAddr,
            stop: Arc<AtomicBool>,
            thread: std::thread::JoinHandle<Vec<u8>>,
        }

        impl SilentCloud {
            fn start() -> Self {
                use std::io::Read;
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let addr = listener.local_addr().unwrap();
                let stop = Arc::new(AtomicBool::new(false));
                let flag = stop.clone();
                let thread = std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                if flag.load(Ordering::SeqCst) || Instant::now() >= deadline {
                                    return Vec::new();
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => panic!("accept: {error}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_millis(20)))
                        .unwrap();
                    let mut received = Vec::new();
                    let mut buf = [0u8; 4096];
                    while Instant::now() < deadline {
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => received.extend_from_slice(&buf[..n]),
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    received
                });
                Self { addr, stop, thread }
            }

            /// 收工：等到对方关闭连接（或根本没有连接），返回云端收到的全部字节。
            fn finish(self) -> Vec<u8> {
                self.stop.store(true, Ordering::SeqCst);
                self.thread.join().unwrap()
            }
        }

        fn config_for(addr: SocketAddr, total_timeout_ms: u64) -> CloudTierConfig {
            // 总期限不能小于连接期限
            let connect_timeout_ms = total_timeout_ms.min(5000);
            CloudTierConfig::parse(
                &format!(
                    "schema_version: 1\nbase_url: http://{addr}/api\nmodel: idoris/agent\n\
                     limits: {{connect_timeout_ms: {connect_timeout_ms}, total_timeout_ms: {total_timeout_ms}}}\n"
                ),
                Path::new("."),
            )
            .unwrap()
        }

        /// 熔断关着才放行；`switch` 置位就是熔断打开。
        fn allowed_while_off(switch: &Arc<AtomicBool>) -> Allowed {
            let switch = switch.clone();
            Arc::new(move || !switch.load(Ordering::SeqCst))
        }

        /// 停住请求体：走到那一点时先通知测试「到了」，再等测试说「放行」（最多 5 秒）。
        struct Pause {
            reached: Option<tokio::sync::oneshot::Receiver<()>>,
            release: mpsc::Sender<()>,
            hook: Arc<dyn Fn() + Send + Sync>,
        }

        impl Pause {
            fn new() -> Self {
                let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let reached_tx = Mutex::new(Some(reached_tx));
                let release_rx = Mutex::new(release_rx);
                Self {
                    reached: Some(reached_rx),
                    release: release_tx,
                    hook: Arc::new(move || {
                        if let Some(tx) = reached_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        // `block_in_place`：这个钩子跑在 hyper 的连接任务里（运行时的工作线程上）。直接阻塞会让
                        // 运行时的定时器和 I/O 驱动没人轮询，熔断轮询的 10 毫秒定时器也就不响了。
                        tokio::task::block_in_place(|| {
                            let _ = release_rx
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5));
                        });
                    }),
                }
            }

            fn hook(&self) -> Arc<dyn Fn() + Send + Sync> {
                self.hook.clone()
            }

            /// 等请求体走到停住的那一点。
            async fn reached(&mut self) {
                let reached = self.reached.take().expect("reached() called twice");
                tokio::time::timeout(Duration::from_secs(5), reached)
                    .await
                    .expect("the request body was never polled: the connection never came up")
                    .unwrap();
            }

            fn release(&self) {
                let _ = self.release.send(());
            }
        }

        async fn run_probed(
            addr: SocketAddr,
            still_allowed: Allowed,
            probe: BodyProbe,
        ) -> Result<CloudAnswer, CloudFailure> {
            run_probed_until(addr, 30_000, CancellationToken::new(), still_allowed, probe).await
        }

        async fn run_probed_until(
            addr: SocketAddr,
            total_timeout_ms: u64,
            cancel: CancellationToken,
            still_allowed: Allowed,
            probe: BodyProbe,
        ) -> Result<CloudAnswer, CloudFailure> {
            let config = config_for(addr, total_timeout_ms);
            send_probed(
                &client(&config),
                &config,
                permit(),
                &CloudCredential::parse(&secret()).unwrap(),
                &json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "hi"}]}),
                &cancel,
                still_allowed,
                probe,
            )
            .await
        }

        async fn finish(cloud: SilentCloud) -> Vec<u8> {
            tokio::task::spawn_blocking(move || cloud.finish())
                .await
                .unwrap()
        }

        /// 熔断取消先到：请求体已经被 hyper 的连接任务取走、停在第一次 poll 之前（还没做决定）。这时熔断打开，
        /// 取消那一边（轮询）看到它、赢得 `Idle -> Cancelled`，报 `Withheld`。再放行请求体：它必须拒绝交付，
        /// 云端一个字节都收不到。（原来：熔断那一边读到「请求体还没被 poll」就报 `Withheld`，放行之后请求体才置位、
        /// 取走正文、写出——正文实际外送了，却报成没出网。）
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_cancel_that_wins_before_the_body_is_polled_keeps_every_byte_off_the_wire() {
            let cloud = SilentCloud::start();
            let switch = Arc::new(AtomicBool::new(false));
            let mut pause = Pause::new();
            let probe = BodyProbe {
                before_decision: Some(pause.hook()),
                ..BodyProbe::default()
            };
            let send = tokio::spawn(run_probed(cloud.addr, allowed_while_off(&switch), probe));
            pause.reached().await;
            switch.store(true, Ordering::SeqCst);
            let outcome = tokio::time::timeout(Duration::from_secs(3), send)
                .await
                .expect("the kill switch opened but the send never noticed")
                .unwrap();
            assert_eq!(outcome.unwrap_err(), CloudFailure::Withheld);
            pause.release();
            let received = finish(cloud).await;
            assert!(
                received.is_empty(),
                "the cloud received bytes although the call was reported as withheld: {:?}",
                String::from_utf8_lossy(&received)
            );
        }

        /// 取消赢了就是赢了：调用方已经被告知「没出网」，之后即使熔断又被关上，请求体也不能再把正文交出去。
        /// 这是「同一个原子转换」和「各自读一次标志」的区别：如果取消那一边只是读了状态、没有真的把它推进到 `Cancelled`，
        /// 熔断关上之后请求体复核通过、抢到 `Idle -> Streaming`，正文就出去了，而调用方拿到的是 `Withheld`。
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_cancel_that_won_stays_won_even_if_the_switch_is_closed_again() {
            let cloud = SilentCloud::start();
            let switch = Arc::new(AtomicBool::new(false));
            let mut pause = Pause::new();
            let probe = BodyProbe {
                before_decision: Some(pause.hook()),
                ..BodyProbe::default()
            };
            let send = tokio::spawn(run_probed(cloud.addr, allowed_while_off(&switch), probe));
            pause.reached().await;
            switch.store(true, Ordering::SeqCst);
            let outcome = tokio::time::timeout(Duration::from_secs(3), send)
                .await
                .expect("the kill switch opened but the send never noticed")
                .unwrap();
            assert_eq!(outcome.unwrap_err(), CloudFailure::Withheld);
            // 熔断又被关上了；请求体这时才第一次做决定
            switch.store(false, Ordering::SeqCst);
            pause.release();
            let received = finish(cloud).await;
            assert!(
                received.is_empty(),
                "the call was reported as withheld, the switch was closed again, and the body left anyway: {:?}",
                String::from_utf8_lossy(&received)
            );
        }

        /// 复审 r4#1 的窗口：熔断在「熔断轮询看过之后、下一次轮询之前」打开，连接恰好在这段时间里就绪。
        /// 轮询间隔拉到 60 秒，取消那一边根本来不及看——能拦住正文的只有请求体在交付边界上自己复核熔断。
        /// （原来：请求体不看熔断，照样把正文和 `Authorization` 发出去；缩短轮询间隔也只是缩小窗口。）
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_kill_switch_opened_before_hyper_polls_the_body_is_caught_at_the_delivery_boundary()
         {
            let cloud = SilentCloud::start();
            let switch = Arc::new(AtomicBool::new(false));
            let flip = switch.clone();
            let probe = BodyProbe {
                // 连接已经就绪，hyper 正要第一次 poll 请求体：熔断恰好在这一刻打开
                before_decision: Some(Arc::new(move || flip.store(true, Ordering::SeqCst))),
                poll_period: Some(Duration::from_secs(60)),
                ..BodyProbe::default()
            };
            let outcome = tokio::time::timeout(
                Duration::from_secs(3),
                run_probed(cloud.addr, allowed_while_off(&switch), probe),
            )
            .await
            .expect(
                "the kill switch was open when hyper polled the body, but the body was delivered anyway \
                 (nothing re-checked the switch at the delivery boundary)",
            );
            assert_eq!(outcome.unwrap_err(), CloudFailure::Withheld);
            let received = finish(cloud).await;
            assert!(
                received.is_empty(),
                "the cloud received bytes although the switch was open at the delivery boundary: {:?}",
                String::from_utf8_lossy(&received)
            );
        }

        /// 正文先赢：请求体已经赢得交付（还没把字节交给 hyper），这时熔断打开。取消输了这场竞争，必须报 `PossiblySent`，
        /// 绝不能报 `Withheld`。
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_cancel_that_loses_to_the_body_is_possibly_sent_and_never_withheld() {
            let cloud = SilentCloud::start();
            let switch = Arc::new(AtomicBool::new(false));
            let mut pause = Pause::new();
            let probe = BodyProbe {
                delivery_won: Some(pause.hook()),
                ..BodyProbe::default()
            };
            let send = tokio::spawn(run_probed(cloud.addr, allowed_while_off(&switch), probe));
            pause.reached().await;
            switch.store(true, Ordering::SeqCst);
            let outcome = tokio::time::timeout(Duration::from_secs(3), send)
                .await
                .expect("the kill switch opened but the send never noticed")
                .unwrap();
            assert_eq!(outcome.unwrap_err(), CloudFailure::PossiblySent);
            pause.release();
            // 这一支云端可能收到完整请求，也可能一个字节都没收到（请求体放行时 `send` 已经返回）：
            // 保守地报 PossiblySent 都是对的，这里不断言字节数
            let _ = finish(cloud).await;
        }

        /// `send` 不论怎么结束，还没交付的请求体都不会在它返回**之后**才发出去：调用方断开（`Cancelled`）、总期限到了（`Timeout`）。
        /// 请求体停在做决定之前；`send` 先因为别的原因返回；再放行请求体——熔断一直是关着的，只有 [`CancelOnExit`] 能拦住它。
        /// （原来：`send` 返回之后，后台连接任务照样取走正文写出去，调用记录却已经按「调用方断开 / 超时」写完了。）
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_send_that_ends_for_any_other_reason_never_lets_the_body_leave_afterwards() {
            for ending in ["the caller goes away", "the total deadline passes"] {
                let cloud = SilentCloud::start();
                let switch = Arc::new(AtomicBool::new(false));
                let mut pause = Pause::new();
                let probe = BodyProbe {
                    before_decision: Some(pause.hook()),
                    ..BodyProbe::default()
                };
                let cancel = CancellationToken::new();
                let send = tokio::spawn(run_probed_until(
                    cloud.addr,
                    if ending == "the caller goes away" {
                        30_000
                    } else {
                        300
                    },
                    cancel.clone(),
                    allowed_while_off(&switch),
                    probe,
                ));
                pause.reached().await;
                if ending == "the caller goes away" {
                    cancel.cancel();
                }
                let outcome = tokio::time::timeout(Duration::from_secs(3), send)
                    .await
                    .expect("the send never ended")
                    .unwrap();
                assert_eq!(
                    outcome.unwrap_err(),
                    if ending == "the caller goes away" {
                        CloudFailure::Cancelled
                    } else {
                        CloudFailure::Timeout
                    },
                    "{ending}"
                );
                pause.release();
                let received = finish(cloud).await;
                assert!(
                    received.is_empty(),
                    "{ending}: the body left the machine after the send had already returned: {:?}",
                    String::from_utf8_lossy(&received)
                );
            }
        }

        /// 200 次交错：四种摆法各 50 次，每一次都必须守住「报了 `Withheld` 就一个字节都没出网」。
        ///
        /// - 0：取消先到（请求体停在决定之前，熔断在那一刻打开）——必须 `Withheld`；
        /// - 1：正文先赢（请求体停在赢得交付之后）——必须 `PossiblySent`；
        /// - 2：复核在轮询之前（轮询间隔 60 秒）——必须 `Withheld`；
        /// - 3：自由竞争（请求体和熔断各自随机抖动 0–3 毫秒）——`Withheld` 或 `PossiblySent`，只检查不变量。
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn withheld_never_coexists_with_bytes_on_the_wire_across_200_interleavings() {
            let mut counts = [0u32; 2];
            for round in 0u32..200 {
                let mode = round % 4;
                let cloud = SilentCloud::start();
                let switch = Arc::new(AtomicBool::new(false));
                let mut probe = BodyProbe::default();
                let mut pause = None;
                let mut flip_after = None;
                match mode {
                    0 => {
                        let p = Pause::new();
                        probe.before_decision = Some(p.hook());
                        pause = Some(p);
                    }
                    1 => {
                        let p = Pause::new();
                        probe.delivery_won = Some(p.hook());
                        pause = Some(p);
                    }
                    2 => {
                        let flip = switch.clone();
                        probe.before_decision =
                            Some(Arc::new(move || flip.store(true, Ordering::SeqCst)));
                        probe.poll_period = Some(Duration::from_secs(60));
                    }
                    _ => {
                        // 确定性的伪随机抖动（线性同余），同一轮每次都一样
                        let mix = |salt: u64| {
                            (u64::from(round) + salt)
                                .wrapping_mul(6_364_136_223_846_793_005)
                                .wrapping_add(1_442_695_040_888_963_407)
                                >> 33
                        };
                        let body_delay = Duration::from_micros(mix(1) % 3000);
                        probe.before_decision = Some(Arc::new(move || {
                            tokio::task::block_in_place(|| std::thread::sleep(body_delay));
                        }));
                        flip_after = Some(Duration::from_micros(mix(7) % 3000));
                    }
                }
                let send = tokio::spawn(run_probed(cloud.addr, allowed_while_off(&switch), probe));
                if let Some(pause) = pause.as_mut() {
                    pause.reached().await;
                    switch.store(true, Ordering::SeqCst);
                } else if let Some(delay) = flip_after {
                    tokio::time::sleep(delay).await;
                    switch.store(true, Ordering::SeqCst);
                }
                let outcome = tokio::time::timeout(Duration::from_secs(5), send)
                    .await
                    .unwrap_or_else(|_| panic!("round {round} (mode {mode}): the send never ended"))
                    .unwrap();
                if mode == 0 {
                    // 取消赢了就是赢了：熔断又关上，请求体放行之后仍然不能交付
                    switch.store(false, Ordering::SeqCst);
                }
                if let Some(pause) = &pause {
                    pause.release();
                }
                let received = finish(cloud).await;
                match outcome.as_ref().unwrap_err() {
                    CloudFailure::Withheld => {
                        counts[0] += 1;
                        assert!(
                            received.is_empty(),
                            "round {round} (mode {mode}): reported Withheld but the cloud received {:?}",
                            String::from_utf8_lossy(&received)
                        );
                    }
                    CloudFailure::PossiblySent => counts[1] += 1,
                    other => panic!("round {round} (mode {mode}): unexpected outcome {other:?}"),
                }
                match mode {
                    0 | 2 => assert_eq!(
                        outcome.unwrap_err(),
                        CloudFailure::Withheld,
                        "round {round} (mode {mode})"
                    ),
                    1 => assert_eq!(
                        outcome.unwrap_err(),
                        CloudFailure::PossiblySent,
                        "round {round} (mode {mode})"
                    ),
                    _ => {}
                }
            }
            eprintln!(
                "200 interleavings: {} withheld (0 bytes on the wire), {} possibly sent",
                counts[0], counts[1]
            );
        }
    }

    fn read_full_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        use std::io::Read;
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if http_request_complete(&buf) {
                        break;
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    if http_request_complete(&buf) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        buf
    }

    fn http_request_complete(buf: &[u8]) -> bool {
        let Some(split) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let Ok(head) = std::str::from_utf8(&buf[..split]) else {
            return false;
        };
        let body = &buf[split + 4..];
        for line in head.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                let Ok(len) = value.trim().parse::<usize>() else {
                    return false;
                };
                return body.len() >= len;
            }
        }
        false
    }

    #[test]
    fn failures_map_to_recorded_outcomes() {
        assert_eq!(
            CloudFailure::Unreachable.outcome(),
            CallOutcome::Unreachable
        );
        assert_eq!(CloudFailure::Timeout.outcome(), CallOutcome::Timeout);
        assert_eq!(CloudFailure::Transport.outcome(), CallOutcome::CloudError);
        // 没出网的那一行也必须有终态：记成 cancelled（没有答复、没有云端 id、没有金额）
        assert_eq!(CloudFailure::Withheld.outcome(), CallOutcome::Cancelled);
        assert_eq!(CloudFailure::PossiblySent.outcome(), CallOutcome::Cancelled);
        assert_eq!(
            CloudFailure::Answered {
                outcome: CallOutcome::RateLimited,
                status: 429,
                reason: None,
                cloud_record_id: None
            }
            .outcome(),
            CallOutcome::RateLimited
        );
    }
}
