//! 云调用的两种结果：一个等待发布的完整回答（非流式），或一条流。
//!
//! # 发布边界
//!
//! **发布** = 已向客户端输出第一个字节。它是不可逆的承诺点：
//!
//! - 非流式：执行器拿到上游的整个响应、记好 usage 之后，把 [`PendingAnswer`] 交给调用方，**此时还没有发布**。
//!   调用方（HTTP 层；S6 的儿童安全检查也在这里）决定是 [`PendingAnswer::publish`]（用户付这一次的钱）还是
//!   [`PendingAnswer::withhold`]（比如完整输出没通过检查：用户付 0，上游成本由平台承担）。
//! - 流式：[`CloudStream`] 在**第一次把字节交给 HTTP 层之前**记下发布。之后上游出任何问题（断开、静默超时、
//!   上游自己报错、客户端断开）都**不会**再向别的候选发任何请求：流以一个错误事件结束，并按已产生的 usage
//!   结算，usage 未知按预留额结算（设计稿 §6）。
//!
//! 两种结果都持有 [`RunState`]：丢弃它们（客户端断开、future 被取消）时 `Drop` 会收尾，不留悬空的请求。

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use futures_util::Stream;
use idoris_compute_contracts::attempt::AttemptEvent;
use idoris_compute_contracts::ids::AttemptIndex;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::attempts::UsageClaim;
use idoris_upstream::cloud::{
    ChoiceFacts, ChunkKind, CloudFailure, ResponseHead, SseEvent, SseParser, UsageParse, classify,
};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::executor::CloudError;
use super::run::{RunState, Settled, judge_usage};

/// 交给 HTTP 层的元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerMeta {
    /// 被服务时带的原因码（降级、宽限、首字节前切换过）。
    pub reasons: Vec<ReasonCode>,
    pub request_id: String,
    /// 准入因为钱不够**压低了 `max_tokens`**（`budget_degraded`）时，实际发给上游的 `max_tokens`。
    /// 调用方可以据此知道回答可能因为长度被截断（`finish_reason: length`）；没压低就是 `None`。
    pub max_tokens_reduced_to: Option<u64>,
}

/// 一个等待发布的完整回答（非流式）。
pub struct PendingAnswer {
    pub(crate) run: RunState,
    pub(crate) idx: AttemptIndex,
    pub(crate) body: Vec<u8>,
    pub(crate) meta: AnswerMeta,
}

impl std::fmt::Debug for PendingAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印正文
        f.debug_struct("PendingAnswer")
            .field("request_id", &self.meta.request_id)
            .field("bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// 已发布的回答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedAnswer {
    pub body: Vec<u8>,
    pub meta: AnswerMeta,
    pub settled: Option<Settled>,
}

impl PendingAnswer {
    /// 上游的响应体（原样，调用方在发布前可以检查）。
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// 如果现在发布会带哪些原因码。
    pub fn meta(&self) -> &AnswerMeta {
        &self.meta
    }

    /// 发布：记下「已向客户端输出第一个字节」，然后收尾结算。返回的回答可以交给客户端。
    ///
    /// 记发布失败（库写失败）返回 `Err`：**不能**把内容交给客户端，否则用户拿到了输出却没有记账。
    /// 收尾失败不拦回答（上游的钱已经花了，拦也追不回），只记日志；发布已经落库，`Drop` 会再试一次收尾，
    /// 之后由对账补上（ops §7）。
    pub fn publish(mut self) -> Result<PublishedAnswer, CloudError> {
        self.run.publish(self.idx)?;
        let meta = AnswerMeta {
            reasons: self.run.served_reasons(),
            request_id: self.meta.request_id.clone(),
            max_tokens_reduced_to: self.run.reduced_max_tokens(self.idx),
        };
        let settled = match self.run.finalize() {
            Ok(settled) => Some(settled),
            Err(error) => {
                eprintln!("[idoris] cloud: settlement failed after publish: {error}");
                None
            }
        };
        Ok(PublishedAnswer {
            body: std::mem::take(&mut self.body),
            meta,
            settled,
        })
    }

    /// 不发布（比如完整输出没通过检查）：用户付 0，这次尝试的成本由平台承担。`reason` 是稳定的原因字符串。
    pub fn withhold(mut self, reason: &'static str) -> Result<Settled, CloudError> {
        self.run.close_reason = Some(reason);
        self.run.finalize()
    }
}

// ---------------------------------------------------------------------------
// 流
// ---------------------------------------------------------------------------

/// 流里一个事件被处理后的结果。
pub(crate) enum Ingested {
    /// 转发给客户端的字节。
    Forward(Vec<u8>),
    /// 不转发（客户端没要 usage 时的 usage 块）。
    Skip,
    /// 注释 / keep-alive：已经发布之后才转发。
    Comment(Vec<u8>),
    /// `[DONE]`。
    Done(Vec<u8>),
    /// 上游在流里报错。
    UpstreamError,
    /// 不是合法的 chunk。
    Malformed,
}

/// 流的观察器：供应商请求号、最近的 usage、新内容出现的位置、每个 choice 的结束位置、是否见过 `[DONE]`。
///
/// # 一个 usage 什么时候可信（R3 N3 + B4 + R6 ②）
///
/// 上游可以在流中途发一个 usage（比如累计用量），后面还有内容。客户端在那之后断开、或者流在那之后失败，甚至流最后
/// 干净地 `[DONE]` 了，中途的数字也只是已生成部分的用量，不是整个响应的用量：按它结算会少收钱（上游已经生成并计费的
/// 部分没有算进去），而且状态机里写成「用量已知」之后，这次尝试既不挂账也不会被对账看到。反过来，一个确实已经是最终
/// 用量的 usage，也不能因为后面又来了一个**没有任何新内容**的块（空 delta 的结束块）就被作废：那样本来按实际用量收的
/// 变成按整笔预留收（多收）。所以分三样分别跟踪，**不是**一个「是不是最终」的布尔：
///
/// - 最近的 usage，以及携带它的那个事件的位置（[`Self::usage`] / `usage_at`）；
/// - 最后一次出现**新内容**的事件的位置（`content_at`）；只有 `delta` 里有新的生成内容才算
///   （见 `idoris_upstream::cloud::ChunkInfo::has_content`），空 delta 的结束块、只带 `role` 的块都不算；
/// - 每个 choice 最近一次**结束**（非空字符串的 `finish_reason`）和最近一次**新内容**的事件位置（`choices`）。
///   **结束不是永久的**：一个 choice 的结束只在它**不早于**这个 choice 最近一次新内容时才算数
///   （[`ChoiceProgress::is_finished`]）。已经结束的 choice 又出现新内容，旧的结束标志就作废——它不能确认当前这一段
///   生成已经完成，直到这个 choice 再次结束。原来这里是个永不清除的 `finished` 布尔：内容 -> `finish_reason: "stop"` ->
///   新内容 + 累计 usage（`finish_reason: null`）-> 上游错误 / 客户端断开，第三块使 `content_at == usage_at`，前面那个
///   结束标志却仍然为真，累计数字被当成最终用量结清、不挂账（少收，对账也看不到）。
///
/// 为什么选「结束标志作废」而不是「把这条流判成畸形」：两条路对这个场景的结算一样（预留价，用量未知），但作废保留了
/// 上游声称的累计数字（`usage_claim_json`，判定 `untrusted:not_final`）供对账，也不会因为一个不守约定的上游把用户
/// 已经收到的回答在半路掐断；而且后面如果这个 choice 又结束了、或者 `[DONE]` 到了，流就按最终 usage 正常结算。
///
/// 每个 choice 单独看（按 `index`，没写按它在数组里的位置）：只有**出现过的每一个** choice 都结束了，夹带在内容块里的
/// usage 才算跟着结束标志。我们从不请求 `n > 1`，正常的上游只有一个 choice；不守约定时，一个 choice 的结束不能代替
/// 另一个。最多分别跟踪 [`MAX_TRACKED_CHOICES`] 个，超出的跟踪不了，保守地当作「没有结束」（`untracked_choice`）。
///
/// [`Self::usage_is_trusted`] 的规则：
///
/// 1. 没有 usage：不可信（没有东西可信）；
/// 2. **usage 之后出现了新内容**（`content_at` 严格晚于 `usage_at`）：不可信，**不论流后来怎么结束，包括 `[DONE]`**；
///    usage 和内容在同一个事件里不算「之后」（每个 chunk 带累计 usage 的实现，最后一个内容块带的就是最终值）；
/// 3. usage 之后没有新内容：单独的 usage 块（`choices` 为空或没有）可信——OpenAI 约定它就是整个请求的最终用量；
///    夹带在内容块里的 usage，要等到所有 choice 都完成（本块带 `finish_reason`，或者之后来了空 delta 的结束块）或者见到
///    `[DONE]` 才可信：还没有任何结束的迹象就断了，生成可能还在继续。
///
/// 不可信时（并且 usage 确实是有效的）：**按原规则**——预留价、成本挂账待核销，上游声称的数字原样记进 `usage_claim_json`
/// （判定 `untrusted:not_final`）供对账。
#[derive(Debug)]
pub(crate) struct StreamTracker {
    wants_usage_chunk: bool,
    pub(crate) id: Option<String>,
    /// 流中途出现的、与第一个不同的请求号（上游出问题的信号；对账时要标注）。
    pub(crate) changed_id: Option<String>,
    /// 最近一个带 usage 的事件的判定。**不一定可信**，见 [`Self::usage_is_trusted`]。
    pub(crate) usage: UsageParse,
    /// 已经处理的 JSON 事件个数：给事件编号，只用来比较先后。
    events: u64,
    /// 携带 [`Self::usage`] 的那个事件的编号。
    usage_at: Option<u64>,
    /// [`Self::usage`] 来自单独的 usage 块（`choices` 为空或没有）。
    usage_standalone: bool,
    /// 最后一个带**新内容**的事件的编号（任何 choice）。
    content_at: Option<u64>,
    /// 每个出现过的 choice 的结束 / 新内容位置。
    choices: Vec<ChoiceProgress>,
    /// 出现过跟踪不了的 choice（超过 [`MAX_TRACKED_CHOICES`] 个不同的 `index`）：保守地当作没有结束。
    untracked_choice: bool,
    pub(crate) saw_done: bool,
}

/// 最多分别跟踪几个 choice。我们从不请求 `n > 1`，正常的上游只有一个。
const MAX_TRACKED_CHOICES: usize = 8;

/// 一个 choice 最近一次「结束」和最近一次「新内容」的事件编号。
#[derive(Debug, Clone, Copy)]
struct ChoiceProgress {
    index: u64,
    finish_at: Option<u64>,
    content_at: Option<u64>,
}

impl ChoiceProgress {
    /// 这个 choice 的最近一次结束**不早于**它最近一次新内容（同一个事件里既有新内容又有结束也算结束）。
    /// 从没结束过：没有结束；从没有新内容：结束过就是结束了。
    fn is_finished(&self) -> bool {
        self.finish_at
            .is_some_and(|finish| self.content_at.is_none_or(|content| finish >= content))
    }
}

impl StreamTracker {
    pub(crate) fn new(wants_usage_chunk: bool) -> Self {
        Self {
            wants_usage_chunk,
            id: None,
            changed_id: None,
            usage: UsageParse::Absent,
            events: 0,
            usage_at: None,
            usage_standalone: false,
            content_at: None,
            choices: Vec::new(),
            untracked_choice: false,
            saw_done: false,
        }
    }

    /// 出现过的每一个 choice 都已经结束（并且结束没有被之后的新内容作废）。
    fn generation_finished(&self) -> bool {
        !self.untracked_choice
            && !self.choices.is_empty()
            && self.choices.iter().all(ChoiceProgress::is_finished)
    }

    /// 记下一个 choice 在当前事件里的动静：新内容、结束。
    fn note_choice(&mut self, facts: &ChoiceFacts) {
        if !self.choices.iter().any(|known| known.index == facts.index) {
            if self.choices.len() >= MAX_TRACKED_CHOICES {
                self.untracked_choice = true;
                return;
            }
            self.choices.push(ChoiceProgress {
                index: facts.index,
                finish_at: None,
                content_at: None,
            });
        }
        if let Some(progress) = self
            .choices
            .iter_mut()
            .find(|known| known.index == facts.index)
        {
            if facts.has_content {
                progress.content_at = Some(self.events);
            }
            if facts.finished {
                progress.finish_at = Some(self.events);
            }
        }
    }

    /// 最近的 usage 可以当结算依据吗？规则见类型文档。
    pub(crate) fn usage_is_trusted(&self) -> bool {
        let Some(usage_at) = self.usage_at else {
            return false;
        };
        // usage 之后又生成了新内容：那个数字只是到它为止的用量，后来的 `[DONE]` 也改变不了这一点
        if self
            .content_at
            .is_some_and(|content_at| content_at > usage_at)
        {
            return false;
        }
        self.usage_standalone || self.generation_finished() || self.saw_done
    }

    pub(crate) fn ingest(&mut self, event: &SseEvent) -> Ingested {
        match classify(event) {
            ChunkKind::Ignorable => Ingested::Comment(event.to_wire()),
            ChunkKind::Done => {
                self.saw_done = true;
                Ingested::Done(event.to_wire())
            }
            ChunkKind::Opaque => Ingested::Malformed,
            ChunkKind::Json(info) => {
                if info.error {
                    return Ingested::UpstreamError;
                }
                let carries_usage = !matches!(info.usage, UsageParse::Absent);
                // 合法的 chunk 只有两种：completion chunk（`choices` 是数组，每个元素是像样的 choice：带 `delta` 对象，
                // 或带已结束的 `finish_reason`），和单独的 usage 块（带 usage，`choices` 为空或者没有）。其它任意 JSON
                // 对象（`{}`、`{"x":1}`、`{"choices":[{}]}`）不是回答：不能转发，更不能让它跨过发布边界（之后就不能切换
                // 候选，用户没有拿到回答却要按预留价付费）。
                let completion_chunk = info.has_choices && info.choices_well_formed;
                let usage_only_chunk = carries_usage && (!info.has_choices || info.choices_empty);
                if !completion_chunk && !usage_only_chunk {
                    return Ingested::Malformed;
                }
                match (&self.id, &info.id) {
                    (None, Some(_)) => self.id.clone_from(&info.id),
                    (Some(first), Some(id)) if first != id && self.changed_id.is_none() => {
                        self.changed_id = Some(id.clone());
                    }
                    _ => {}
                }
                self.events += 1;
                if carries_usage {
                    self.usage = info.usage.clone();
                    self.usage_at = Some(self.events);
                    self.usage_standalone = !info.has_choices || info.choices_empty;
                }
                if info.has_content {
                    self.content_at = Some(self.events);
                }
                for choice in &info.choices {
                    self.note_choice(choice);
                }
                if carries_usage && !self.wants_usage_chunk {
                    // 客户端没要 usage：整块只有 usage 就不转发，内容块里夹带的 usage 去掉
                    if usage_only_chunk {
                        return Ingested::Skip;
                    }
                    return Ingested::Forward(
                        strip_usage(event).unwrap_or_else(|| event.to_wire()),
                    );
                }
                Ingested::Forward(event.to_wire())
            }
        }
    }
}

/// 把事件里 `data` JSON 的顶层 `usage` 去掉，重新序列化成一个规范事件。失败返回 `None`（调用方原样转发）。
fn strip_usage(event: &SseEvent) -> Option<Vec<u8>> {
    let data = event.data.as_deref()?;
    let mut value: Value = serde_json::from_slice(data).ok()?;
    value.as_object_mut()?.remove("usage");
    let mut out = b"data: ".to_vec();
    out.extend_from_slice(&serde_json::to_vec(&value).ok()?);
    out.extend_from_slice(b"\n\n");
    Some(out)
}

/// 流为什么结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// 上游正常结束（`[DONE]`，或已经给了最终 usage 之后连接结束）。
    Complete,
    /// 上游中途失败。
    Failed(CloudFailure),
    /// 客户端断开（或请求被取消）。
    ClientGone,
    /// 记发布失败：没有把任何内容交给客户端。
    PublishFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Reading,
    Finishing(End),
    Ended,
}

/// 流的内部状态。`Drop` 兜底：被丢弃（客户端断开）时按「客户端断开」收尾。
pub(crate) struct StreamState {
    pub(crate) run: RunState,
    pub(crate) idx: AttemptIndex,
    pub(crate) head: ResponseHead,
    pub(crate) parser: SseParser,
    pub(crate) tracker: StreamTracker,
    pub(crate) pending: VecDeque<Vec<u8>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) deadline: Instant,
    pub(crate) published: bool,
    /// 许可表里这个端点的「取消」能力：客户端断开、我们关掉连接之后，上游是否停止生成与计费。
    upstream_stops_on_cancel: bool,
    phase: Phase,
    concluded: bool,
}

/// 构造 [`StreamState`] 的参数。
pub(crate) struct StreamParts {
    pub(crate) run: RunState,
    pub(crate) idx: AttemptIndex,
    pub(crate) head: ResponseHead,
    pub(crate) parser: SseParser,
    pub(crate) tracker: StreamTracker,
    pub(crate) pending: VecDeque<Vec<u8>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) stream_total: Duration,
    /// 见 [`StreamState`] 的同名字段。
    pub(crate) upstream_stops_on_cancel: bool,
}

impl StreamState {
    pub(crate) fn new(parts: StreamParts) -> Self {
        // 第一次读流就已经见到 `[DONE]`（回答、usage 和 `[DONE]` 在同一个网络块里，上游随后迟迟不关闭连接）：
        // 流已经完成，交接之后只把排队的字节发完就收尾，不再等上游的下一块（否则要等到静默超时，
        // 然后在 `[DONE]` 之后追加一个超时错误事件，还把并发名额多占 30 秒）。
        let phase = if parts.tracker.saw_done {
            Phase::Finishing(End::Complete)
        } else {
            Phase::Reading
        };
        Self {
            run: parts.run,
            idx: parts.idx,
            head: parts.head,
            parser: parts.parser,
            tracker: parts.tracker,
            pending: parts.pending,
            cancel: parts.cancel,
            deadline: Instant::now() + parts.stream_total,
            published: false,
            upstream_stops_on_cancel: parts.upstream_stops_on_cancel,
            phase,
            concluded: false,
        }
    }

    /// 下一个要交给 HTTP 层的块；`None` 是流结束。
    pub(crate) async fn next_item(&mut self) -> Option<Result<Bytes, io::Error>> {
        loop {
            if let Some(bytes) = self.pending.pop_front() {
                if !self.published {
                    // 第一次把字节交给 HTTP 层之前，先记发布（先落库）。失败就不交付任何内容。
                    if let Err(error) = self.run.publish(self.idx) {
                        eprintln!("[idoris] cloud: cannot record the publish: {error}");
                        self.pending.clear();
                        self.phase = Phase::Finishing(End::PublishFailed);
                        continue;
                    }
                    self.published = true;
                }
                return Some(Ok(Bytes::from(bytes)));
            }
            match self.phase {
                Phase::Ended => return None,
                Phase::Finishing(end) => {
                    let last = self.conclude(end);
                    self.phase = Phase::Ended;
                    if end == End::PublishFailed {
                        return Some(Err(io::Error::other("ledger unavailable")));
                    }
                    if let Some(bytes) = last {
                        return Some(Ok(Bytes::from(bytes)));
                    }
                    return None;
                }
                Phase::Reading => self.read_more().await,
            }
        }
    }

    async fn read_more(&mut self) {
        let next = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(End::ClientGone),
            () = tokio::time::sleep_until(self.deadline) => Err(End::Failed(CloudFailure::StreamTotalTimeout)),
            chunk = self.head.next_chunk() => match chunk {
                Ok(Some(chunk)) => Ok(Some(chunk)),
                Ok(None) => Ok(None),
                Err(failure) => Err(End::Failed(failure)),
            },
        };
        match next {
            Err(end) => self.phase = Phase::Finishing(end),
            Ok(None) => {
                // 连接结束：`[DONE]` 或**可信的**最终 usage 已经到了才算完成；否则是被截断
                // （中途的 usage 之后还有内容、连接却断了，不是完成）
                let complete = self.tracker.saw_done
                    || (self.tracker.usage_is_trusted()
                        && self.tracker.usage.valid_usage().is_some()
                        && !self.parser.has_partial_event());
                if complete {
                    if !self.tracker.saw_done {
                        // 上游没写 `[DONE]` 就结束了，客户端需要它来确认流完整
                        self.pending.push_back(b"data: [DONE]\n\n".to_vec());
                    }
                    self.phase = Phase::Finishing(End::Complete);
                } else {
                    self.phase = Phase::Finishing(End::Failed(CloudFailure::StreamTruncated));
                }
            }
            Ok(Some(chunk)) => match self.parser.push(&chunk) {
                Err(_) => {
                    self.phase = Phase::Finishing(End::Failed(CloudFailure::StreamEventTooLarge))
                }
                Ok(events) => {
                    for event in events {
                        match self.tracker.ingest(&event) {
                            Ingested::Forward(bytes) => self.pending.push_back(bytes),
                            Ingested::Skip => {}
                            // 注释只在发布之后转发（发布之前不能让一个 keep-alive 提前占用「第一个字节」）
                            Ingested::Comment(bytes) => {
                                if self.published || !self.pending.is_empty() {
                                    self.pending.push_back(bytes);
                                }
                            }
                            Ingested::Done(bytes) => {
                                self.pending.push_back(bytes);
                                self.phase = Phase::Finishing(End::Complete);
                                break;
                            }
                            Ingested::UpstreamError => {
                                self.phase =
                                    Phase::Finishing(End::Failed(CloudFailure::StreamErrorEvent));
                                break;
                            }
                            Ingested::Malformed => {
                                self.phase =
                                    Phase::Finishing(End::Failed(CloudFailure::StreamMalformed));
                                break;
                            }
                        }
                    }
                }
            },
        }
    }

    /// 流结束：把结果落进状态机、收尾结算，返回要补发给客户端的最后一块（失败时是错误事件）。
    pub(crate) fn conclude(&mut self, end: End) -> Option<Vec<u8>> {
        if self.concluded {
            return None;
        }
        self.concluded = true;
        let Ok(attempt) = self.run.attempt(self.idx) else {
            return None;
        };
        // 供应商请求号：流里每个 chunk 都带同一个 id，记第一个；中途变了就再记一次，让库标注冲突（不覆盖）
        if let Some(changed) = self.tracker.changed_id.clone() {
            self.run
                .record_supplier_id(self.idx, self.tracker.id.as_deref(), None);
            self.run.record_supplier_id(self.idx, Some(&changed), None);
        }
        self.run.record_supplier_id(
            self.idx,
            self.tracker.id.as_deref(),
            self.head.supplier_request_id_header.as_deref(),
        );
        // usage：**只信最终用量**，而且**不看流是怎么结束的**：`[DONE]` 不能把中途的 usage 变成最终用量（R3 B4）——
        // usage 之后又生成了新内容，那个数字只是已生成部分的用量，按它结算会少收钱，而且「用量已知」会盖过下面
        // 「保持 maybe_executed」的诚实标注，成本既不挂账也不会被对账看到。反过来，usage 之后只有没有新内容的块
        // （空 delta 的结束块、`[DONE]`）时它就是最终用量，客户端读到结束标志就断开也照样按它结算（R3 N3：不多收）。
        // 规则见 [`StreamTracker::usage_is_trusted`]。不被信任的 usage 原文照样记进 `usage_claim_json`
        // （判定 `untrusted:not_final`）供对账。
        let trusted = self.tracker.usage_is_trusted();
        let (usage, claim) = if trusted || matches!(self.tracker.usage, UsageParse::Absent) {
            judge_usage(&self.tracker.usage, &attempt)
        } else {
            (
                None,
                UsageClaim {
                    verdict: "untrusted:not_final".to_string(),
                    raw: self.tracker.usage.raw().cloned(),
                },
            )
        };
        if !matches!(self.tracker.usage, UsageParse::Absent) || end == End::Complete {
            self.run.record_usage_claim(self.idx, &claim);
        }
        // 状态机事件：
        // - 看到了通过判定的、可信的最终 usage：`UsageReported`；
        // - 否则：确认执行了、usage 未知（`ExecutionObservedUsageUnknown`，按预留结算）——
        //   **除了**客户端断开、而许可表里这个端点没有「取消」能力的情形：我们关掉了连接，但上游可能还在生成、
        //   还在计费，是否停计费不确定，所以不下结论，保持 `maybe_executed`（成本按预留额挂账待核销，
        //   用户按预留价付费——这一点与 `executed_usage_unknown` 的结算数字相同，区别只在于状态诚实地写着「不确定」）。
        let event = match (usage, end) {
            (Some(usage), _) => Some(AttemptEvent::UsageReported { usage }),
            (None, End::ClientGone) if !self.upstream_stops_on_cancel => None,
            (None, _) => Some(AttemptEvent::ExecutionObservedUsageUnknown),
        };
        match end {
            End::Failed(failure) => {
                self.run
                    .note_failure(self.idx, failure.code(), failure.http_status())
            }
            End::ClientGone => self.run.note_failure(self.idx, "client_gone", None),
            End::PublishFailed => self.run.note_failure(self.idx, "publish_failed", None),
            End::Complete => {}
        }
        if let Some(event) = event
            && let Err(error) = self.run.apply(self.idx, &[event])
        {
            eprintln!("[idoris] cloud: could not record the stream outcome: {error}");
        }
        self.run.close_reason = Some(match end {
            End::Complete => "stream_complete",
            End::Failed(_) => "upstream_stream_failed",
            End::ClientGone => "client_cancelled",
            End::PublishFailed => "ledger_unavailable",
        });
        if let Err(error) = self.run.finalize() {
            eprintln!("[idoris] cloud: could not settle the stream: {error}");
        }
        match end {
            End::Failed(failure) => Some(stream_error_event(failure)),
            _ => None,
        }
    }
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if !self.concluded {
            // 客户端断开（或请求被取消）：丢弃 `head` 会关闭到上游的连接（取消）；这里先收尾结算
            self.conclude(End::ClientGone);
        }
    }
}

/// 流中途失败时发给客户端的最后一个事件。**不带 `[DONE]`**：`[DONE]` 表示正常完成。
pub(crate) fn stream_error_event(failure: CloudFailure) -> Vec<u8> {
    let body = serde_json::json!({
        "error": {
            "type": "upstream_error",
            "rule_id": null,
            "reason_code": failure.code(),
            "evidence": null,
            "remediation": "the upstream stream was interrupted after the response had started; the response is incomplete",
        }
    });
    format!("data: {body}\n\n").into_bytes()
}

type BoxedItems = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// 一条流。实现 [`Stream`]，直接交给 `axum::body::Body::from_stream`。
pub struct CloudStream {
    inner: BoxedItems,
    meta: AnswerMeta,
}

impl CloudStream {
    pub(crate) fn new(state: StreamState, meta: AnswerMeta) -> Self {
        let inner = futures_util::stream::unfold(state, |mut state| async move {
            let item = state.next_item().await?;
            Some((item, state))
        });
        Self {
            inner: Box::pin(inner),
            meta,
        }
    }

    /// 被服务时带的原因码（首字节前切换过、准入降级……）。流还没开始输出，所以这些在响应头里就能给。
    pub fn meta(&self) -> &AnswerMeta {
        &self.meta
    }
}

impl Stream for CloudStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for CloudStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudStream")
            .field("request_id", &self.meta.request_id)
            .finish_non_exhaustive()
    }
}

/// 一次云调用的结果。
#[derive(Debug)]
pub enum CloudOutcome {
    Complete(Box<PendingAnswer>),
    Stream(CloudStream),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ev(data: &str) -> SseEvent {
        SseEvent::from_lines(&[format!("data: {data}").as_bytes()])
    }

    #[test]
    fn the_usage_only_block_is_hidden_unless_the_client_asked_for_it() {
        let usage_block =
            ev(r#"{"id":"c","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#);
        let mut hidden = StreamTracker::new(false);
        assert!(matches!(hidden.ingest(&usage_block), Ingested::Skip));
        // 但仍然被记下来用于结算
        assert_eq!(hidden.usage.valid_usage().unwrap().completion_tokens, 4);
        assert_eq!(hidden.id.as_deref(), Some("c"));
        let mut shown = StreamTracker::new(true);
        match shown.ingest(&usage_block) {
            Ingested::Forward(bytes) => assert_eq!(bytes, usage_block.to_wire()),
            _ => panic!("the client asked for the usage chunk"),
        }
    }

    #[test]
    fn usage_riding_on_a_content_chunk_is_stripped_not_dropped() {
        let chunk = ev(
            r#"{"id":"c","choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
        );
        let mut tracker = StreamTracker::new(false);
        let Ingested::Forward(bytes) = tracker.ingest(&chunk) else {
            panic!("a content chunk is always forwarded");
        };
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"content\":\"hi\""), "{text}");
        assert!(!text.contains("usage"), "{text}");
        assert!(text.ends_with("\n\n"));
        assert_eq!(tracker.usage.valid_usage().unwrap().prompt_tokens, 3);
    }

    #[test]
    fn null_usage_and_plain_chunks_pass_through_untouched() {
        let chunk = ev(r#"{"id":"c","choices":[{"delta":{"content":"x"}}],"usage":null}"#);
        let mut tracker = StreamTracker::new(false);
        let Ingested::Forward(bytes) = tracker.ingest(&chunk) else {
            panic!("forwarded");
        };
        assert_eq!(bytes, chunk.to_wire());
        assert!(matches!(tracker.usage, UsageParse::Absent));
    }

    #[test]
    fn errors_malformed_data_and_done_are_classified() {
        let mut tracker = StreamTracker::new(false);
        assert!(matches!(
            tracker.ingest(&ev(r#"{"error":{"message":"overloaded"}}"#)),
            Ingested::UpstreamError
        ));
        assert!(matches!(
            tracker.ingest(&ev("not json")),
            Ingested::Malformed
        ));
        assert!(matches!(
            tracker.ingest(&SseEvent::from_lines(&[b": keep-alive"])),
            Ingested::Comment(_)
        ));
        assert!(!tracker.saw_done);
        assert!(matches!(tracker.ingest(&ev("[DONE]")), Ingested::Done(_)));
        assert!(tracker.saw_done);
    }

    /// R2 Medium#3：只有两种 JSON 对象是流里的合法事件——completion chunk（`choices` 是对象数组）和单独的 usage 块
    /// （带 usage，`choices` 为空或者没有）。其它任意对象不是回答，不能转发（转发了就跨过发布边界，之后不能切换候选）。
    #[test]
    fn only_completion_chunks_and_usage_chunks_are_valid_stream_events() {
        let mut tracker = StreamTracker::new(false);
        for data in [
            "{}",
            r#"{"x":1}"#,
            r#"{"id":"c"}"#,
            r#"{"choices":"nope"}"#,
            r#"{"choices":[1]}"#,
            r#"{"choices":{}}"#,
        ] {
            assert!(
                matches!(tracker.ingest(&ev(data)), Ingested::Malformed),
                "{data}"
            );
        }
        assert!(tracker.id.is_none(), "a malformed chunk teaches us nothing");
        for data in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ] {
            assert!(
                matches!(tracker.ingest(&ev(data)), Ingested::Forward(_)),
                "{data}"
            );
        }
        // 没有 `choices` 的单独 usage 块：合法；客户端没要就不转发，要了就原样转发
        let usage_only = ev(r#"{"id":"c","usage":{"prompt_tokens":3,"completion_tokens":4}}"#);
        assert!(matches!(
            StreamTracker::new(false).ingest(&usage_only),
            Ingested::Skip
        ));
        assert!(matches!(
            StreamTracker::new(true).ingest(&usage_only),
            Ingested::Forward(_)
        ));
    }

    /// PR-Daemon B4：中途的 usage 不是最终用量。usage 只有在它来自「最后一块」（单独的 usage 块，或带着 `finish_reason`
    /// 的最后一块内容）并且之后流里没有再出现过带内容的块时，才是最终用量。
    #[test]
    fn a_usage_is_final_only_when_it_rides_the_last_chunk_and_no_content_follows() {
        let usage = r#""usage":{"prompt_tokens":12,"completion_tokens":2}"#;
        let content = |text: &str| format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}}}}]}}"#);

        // 单独的 usage 块（choices 为空）：最终
        let mut tracker = StreamTracker::new(false);
        tracker.ingest(&ev(&content("a")));
        assert!(!tracker.usage_is_trusted(), "no usage yet");
        tracker.ingest(&ev(&format!(r#"{{"choices":[],{usage}}}"#)));
        assert!(tracker.usage_is_trusted());
        // 之后又来了内容：不是最终用量
        tracker.ingest(&ev(&content("b")));
        assert!(!tracker.usage_is_trusted(), "content followed the usage");
        assert_eq!(
            tracker.usage.valid_usage().unwrap().completion_tokens,
            2,
            "the number is still recorded (for the usage claim)"
        );
        // 之后的空 choices 块（没有 usage）不改变判断
        let mut tracker = StreamTracker::new(false);
        tracker.ingest(&ev(&format!(r#"{{"choices":[],{usage}}}"#)));
        tracker.ingest(&ev(r#"{"choices":[]}"#));
        assert!(tracker.usage_is_trusted());

        // 夹带在内容块里的 usage：没有 finish_reason 的不是最终用量，带 finish_reason 的是
        let mut tracker = StreamTracker::new(false);
        tracker.ingest(&ev(&format!(
            r#"{{"choices":[{{"delta":{{"content":"a"}},"finish_reason":null}}],{usage}}}"#
        )));
        assert!(!tracker.usage_is_trusted(), "a cumulative usage mid-stream");
        tracker.ingest(&ev(&format!(
            r#"{{"choices":[{{"delta":{{}},"finish_reason":"stop"}}],{usage}}}"#
        )));
        assert!(tracker.usage_is_trusted());

        // 没有 usage 的流：什么都不是最终用量
        let mut tracker = StreamTracker::new(false);
        tracker.ingest(&ev(&content("a")));
        tracker.ingest(&SseEvent::from_lines(&[b"data: [DONE]"]));
        assert!(tracker.saw_done && !tracker.usage_is_trusted());
    }

    /// 喂给观察器的事件序列。每个词是一种 chunk（usage 都是 12 入 2 出）：
    ///
    /// - `content`：带内容、没有 usage；`content+usage`：带内容、夹带 usage、没有 `finish_reason`；
    /// - `content+usage+finish`：最后一块内容，夹带 usage 并带 `finish_reason`；
    /// - `usage`：单独的 usage 块（`choices` 为空）；`usage-no-choices`：单独的 usage 块（没有 `choices`）；
    /// - `finish`：只有结束标志（空 delta、`finish_reason: stop`），没有 usage、没有新内容；`finish+usage`：同上并带 usage；
    /// - `role`：只有 `role` 的开场块；`empty-delta`：`delta: {}`、没有 `finish_reason`；`blank-finish`：`finish_reason: ""`；
    /// - `tool-call`：带工具调用参数的块；`empty-choices`：`{"choices":[]}`；`done`：`[DONE]`。
    ///
    /// 任何一个带 choice 的词后面可以接 `@N`（比如 `content@1`、`finish@1`）：这个块的 choice 是 `index: N`
    /// （没有 `@N` 的块里 choice 没写 `index`，按它在数组里的位置算 0）。上游本来不该有第二个 choice
    /// （我们从不请求 `n > 1`），这些词用来钉住「不守约定的上游」那一类流。
    fn track(steps: &[&str]) -> StreamTracker {
        let usage = r#""usage":{"prompt_tokens":12,"completion_tokens":2}"#;
        let mut tracker = StreamTracker::new(false);
        for step in steps {
            let (name, choice_index) = match step.split_once('@') {
                Some((name, index)) => (name, Some(index.parse::<u64>().unwrap())),
                None => (*step, None),
            };
            let mut data = match name {
                "content" => r#"{"id":"c","choices":[{"delta":{"content":"hi"}}]}"#.to_string(),
                "content+usage" => format!(
                    r#"{{"id":"c","choices":[{{"delta":{{"content":"hi"}}}}],{usage}}}"#
                ),
                "content+usage+finish" => format!(
                    r#"{{"id":"c","choices":[{{"delta":{{"content":"hi"}},"finish_reason":"stop"}}],{usage}}}"#
                ),
                "usage" => format!(r#"{{"id":"c","choices":[],{usage}}}"#),
                "usage-no-choices" => format!(r#"{{"id":"c",{usage}}}"#),
                "finish" => {
                    r#"{"id":"c","choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string()
                }
                "finish+usage" => format!(
                    r#"{{"id":"c","choices":[{{"delta":{{}},"finish_reason":"stop"}}],{usage}}}"#
                ),
                "role" => {
                    r#"{"id":"c","choices":[{"delta":{"role":"assistant","content":""}}]}"#
                        .to_string()
                }
                "empty-delta" => r#"{"id":"c","choices":[{"delta":{}}]}"#.to_string(),
                "blank-finish" => {
                    r#"{"id":"c","choices":[{"delta":{},"finish_reason":""}]}"#.to_string()
                }
                "tool-call" => r#"{"id":"c","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":1}"}}]}}]}"#.to_string(),
                "empty-choices" => r#"{"id":"c","choices":[]}"#.to_string(),
                "done" => "[DONE]".to_string(),
                other => panic!("unknown step {other}"),
            };
            if let Some(index) = choice_index {
                let mut value: Value = serde_json::from_str(&data).unwrap();
                value["choices"][0]["index"] = serde_json::json!(index);
                data = value.to_string();
            }
            let ingested = tracker.ingest(&ev(&data));
            assert!(
                !matches!(ingested, Ingested::Malformed | Ingested::UpstreamError),
                "{step}: {data}"
            );
        }
        tracker
    }

    /// R3 N3 + B4 的场景表：usage 什么时候可信（结算按它收实际用量），什么时候不可信（按原规则：预留价、挂账待核销）。
    /// 规则只看三样东西：最近的 usage、最后一次新内容的位置、choice 有没有完成；**不看流是怎么结束的**。
    #[test]
    fn the_usage_trust_table() {
        // (场景, 事件序列, 可信吗)
        let cases: &[(&str, &[&str], bool)] = &[
            // ---- 没有 usage ----
            ("no usage at all", &["content", "finish", "done"], false),
            ("nothing yet", &[], false),
            // ---- 单独的 usage 块（OpenAI 约定：整个请求的最终用量）----
            (
                "usage-only chunk after the content",
                &["content", "usage"],
                true,
            ),
            (
                "usage-only chunk without a choices field",
                &["content", "usage-no-choices"],
                true,
            ),
            (
                "content, finish, usage-only",
                &["content", "finish", "usage"],
                true,
            ),
            (
                "content, finish, usage-only, done",
                &["content", "finish", "usage", "done"],
                true,
            ),
            // usage 之后没有新内容的块：空 choices、空 delta 的结束块、`[DONE]` 都不算
            (
                "usage-only then an empty choices chunk",
                &["content", "usage", "empty-choices"],
                true,
            ),
            (
                "usage-only then a bare finish chunk",
                &["content", "usage", "finish"],
                true,
            ),
            (
                "usage-only then a bare finish chunk and done",
                &["content", "usage", "finish", "done"],
                true,
            ),
            (
                "usage-only then an empty delta",
                &["content", "usage", "empty-delta"],
                true,
            ),
            // ---- usage 之后又有新内容：不可信，不论流怎么结束 ----
            (
                "usage-only then more content (client leaves / stream fails)",
                &["content", "usage", "content"],
                false,
            ),
            (
                "usage-only then lots of content",
                &[
                    "content", "usage", "content", "content", "content", "content",
                ],
                false,
            ),
            (
                "usage-only then lots of content and a finish",
                &["content", "usage", "content", "content", "finish"],
                false,
            ),
            (
                "usage-only then lots of content, a finish and DONE: DONE does not make the mid-stream number final",
                &["content", "usage", "content", "content", "finish", "done"],
                false,
            ),
            (
                "usage-only then a tool call",
                &["content", "usage", "tool-call", "finish", "done"],
                false,
            ),
            (
                "usage on the first chunk, the answer after it",
                &["content+usage", "content", "finish", "done"],
                false,
            ),
            (
                "an early usage-only chunk, then the answer, no later usage",
                &["usage", "content", "content", "finish", "done"],
                false,
            ),
            // 后面又来了更新的 usage（最新的一个才算）：用新的位置重新判断
            (
                "a later usage-only chunk replaces the early one",
                &["content", "usage", "content", "usage", "finish", "done"],
                true,
            ),
            // ---- 夹带在内容块里的 usage ----
            (
                "cumulative usage mid-stream, nothing says the generation ended",
                &["content+usage"],
                false,
            ),
            (
                "cumulative usage mid-stream, then the client leaves after a role chunk",
                &["content+usage", "role"],
                false,
            ),
            (
                "cumulative usage, an empty-string finish_reason is not a finish",
                &["content+usage", "blank-finish"],
                false,
            ),
            (
                "cumulative usage then DONE",
                &["content+usage", "done"],
                true,
            ),
            (
                "cumulative usage in the last content chunk, then a bare finish chunk (N3)",
                &["content", "content+usage", "finish"],
                true,
            ),
            (
                "... and DONE",
                &["content", "content+usage", "finish", "done"],
                true,
            ),
            (
                "the finish_reason rides the usage-carrying content chunk",
                &["content", "content+usage+finish"],
                true,
            ),
            (
                "... and the usage rides the finish chunk",
                &["content", "finish+usage"],
                true,
            ),
            (
                "usage on every content chunk, then the finish chunk (cumulative stats)",
                &["content+usage", "content+usage", "content+usage", "finish"],
                true,
            ),
            (
                "usage on every content chunk but the stream stops before any finish",
                &["content+usage", "content+usage", "content+usage"],
                false,
            ),
            (
                "cumulative usage, then content without usage, then finish",
                &["content+usage", "content", "finish"],
                false,
            ),
            (
                "cumulative usage, then content without usage, then finish and DONE",
                &["content+usage", "content", "finish", "done"],
                false,
            ),
            // ---- 结束标志先于 usage（顺序颠倒）----
            (
                "finish chunk first, usage-only after",
                &["content", "finish", "usage"],
                true,
            ),
            (
                "finish chunk first, usage on a later content-free chunk",
                &["content", "finish", "finish+usage"],
                true,
            ),
        ];
        for (name, steps, trusted) in cases {
            let tracker = track(steps);
            assert_eq!(
                tracker.usage_is_trusted(),
                *trusted,
                "{name}: {steps:?} (usage_at={:?} content_at={:?} finished={} done={})",
                tracker.usage_at,
                tracker.content_at,
                tracker.generation_finished(),
                tracker.saw_done
            );
        }
    }

    /// 第六轮（R4-r2 新问题②）：结束标志不是永久生效的。一个 choice 的「结束」只在它**不早于**这个 choice 最近一次
    /// 新内容时才算数：已经结束的 choice 又出现新内容，说明那个结束标志不能确认当前这一段生成已经完成，
    /// 夹带在这个新内容块里的累计 usage 就还不是最终值。原来 `finished` 是个永不清除的布尔：
    /// 内容 -> `finish_reason: "stop"` -> 新内容 + 累计 usage（`finish_reason: null`）-> 上游报错 / 客户端断开，
    /// 第三块使 `content_at == usage_at`，前面的结束标志却仍然为真，那个累计数字被当成最终用量结清、不挂账（少收，
    /// 对账也看不到）。
    #[test]
    fn the_usage_trust_table_after_a_finish_event() {
        // (场景, 事件序列, 可信吗)
        let cases: &[(&str, &[&str], bool)] = &[
            // ---- 报告里的序列：结束之后又来新内容 + 累计 usage ----
            (
                "REPORT: content, finish, content + cumulative usage, then the stream fails / the client leaves",
                &["content", "finish", "content+usage"],
                false,
            ),
            (
                "the same, with a role-only chunk and an empty delta in between",
                &["content", "finish", "role", "empty-delta", "content+usage"],
                false,
            ),
            // 之后有了新的结束标志（或 `[DONE]`、单独的 usage 块）：这一段生成确实完成了
            (
                "... then a new finish chunk: the latest finish is not earlier than the latest content",
                &["content", "finish", "content+usage", "finish"],
                true,
            ),
            (
                "... then DONE",
                &["content", "finish", "content+usage", "done"],
                true,
            ),
            (
                "... then a usage-only chunk",
                &["content", "finish", "content+usage", "usage"],
                true,
            ),
            (
                "... then finish+usage",
                &["content", "finish", "content+usage", "finish+usage"],
                true,
            ),
            (
                "the new content carries the new finish_reason itself",
                &["content", "finish", "content+usage+finish"],
                true,
            ),
            // ---- 反复结束 / 反复重开 ----
            (
                "finish, content, finish, content+usage: the last finish is older than the last content",
                &["content", "finish", "content", "finish", "content+usage"],
                false,
            ),
            (
                "... and one more finish at the end",
                &[
                    "content",
                    "finish",
                    "content",
                    "finish",
                    "content+usage",
                    "finish",
                ],
                true,
            ),
            (
                "finish, more content without usage, then a usage-only chunk (the usage-only chunk is final by convention)",
                &["content", "finish", "content", "usage"],
                true,
            ),
            // usage 之后的新内容仍然使它不可信（第 2 条规则不受影响）
            (
                "finish, usage-carrying finish chunk, then more content",
                &["content", "finish+usage", "content"],
                false,
            ),
            (
                "usage on the first chunk, finish, content + a later cumulative usage",
                &["content+usage", "finish", "content+usage"],
                false,
            ),
            (
                "... and a finish after it",
                &["content+usage", "finish", "content+usage", "finish"],
                true,
            ),
            // ---- 对照：结束标志之后没有新内容，原来的行为不变 ----
            (
                "control: content, finish, finish+usage (the finish chunk repeats, no new content)",
                &["content", "finish", "finish+usage"],
                true,
            ),
            (
                "control: content, content+usage, finish",
                &["content", "content+usage", "finish"],
                true,
            ),
            (
                "control: content+usage+finish alone",
                &["content+usage+finish"],
                true,
            ),
            // ---- 不守约定的上游：不止一个 choice（我们从不请求 n > 1）----
            // 每个 choice 单独看：所有出现过的 choice 都已结束，夹带在内容块里的 usage 才算跟着结束标志
            (
                "two choices: only choice 0 finished, choice 1 is still generating",
                &["content", "content@1", "finish", "content+usage@1"],
                false,
            ),
            (
                "two choices: choice 1 finishes afterwards, both are finished",
                &[
                    "content",
                    "content@1",
                    "finish",
                    "content+usage@1",
                    "finish@1",
                ],
                true,
            ),
            (
                "two choices: choice 1's finish comes first, choice 0 has the later content",
                &["content", "content@1", "finish@1", "content+usage"],
                false,
            ),
            (
                "two choices: the last finish event belongs to choice 0, choice 1 never finished",
                &["content@1", "content+usage", "finish"],
                false,
            ),
            (
                "two choices: both finished long ago, then choice 1 re-opens with content + usage",
                &[
                    "content",
                    "content@1",
                    "finish",
                    "finish@1",
                    "content+usage@1",
                ],
                false,
            ),
            (
                "two choices: both finished, the usage rides the last finish chunk",
                &["content", "content@1", "finish", "finish+usage@1"],
                true,
            ),
        ];
        for (name, steps, trusted) in cases {
            let tracker = track(steps);
            assert_eq!(
                tracker.usage_is_trusted(),
                *trusted,
                "{name}: {steps:?} (usage_at={:?} content_at={:?} done={})",
                tracker.usage_at,
                tracker.content_at,
                tracker.saw_done
            );
        }
    }

    /// 第六轮（R4-r2 Medium-3 残留）：`finish_reason` 不是字符串（布尔值、数字、对象、数组）的 choice 不是合法的流事件：
    /// 不转发（转发了就跨过发布边界，不能再切换候选，用户没拿到回答却要按预留价付费），不认供应商请求号，不改变任何判断。
    #[test]
    fn a_finish_reason_that_is_not_a_string_is_not_a_valid_stream_event() {
        let mut tracker = StreamTracker::new(false);
        for data in [
            r#"{"choices":[{"finish_reason":false}]}"#,
            r#"{"id":"c","choices":[{"finish_reason":true}]}"#,
            r#"{"id":"c","choices":[{"finish_reason":0}]}"#,
            r#"{"id":"c","choices":[{"finish_reason":{}}]}"#,
            r#"{"id":"c","choices":[{"finish_reason":[]}]}"#,
            r#"{"id":"c","choices":[{"delta":{},"finish_reason":false}]}"#,
            r#"{"id":"c","choices":[{"delta":{"content":"x"},"finish_reason":1}]}"#,
            // 带 usage 也一样
            r#"{"id":"c","choices":[{"finish_reason":false}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            // `delta` / `index` 类型不对
            r#"{"id":"c","choices":[{"delta":"text","finish_reason":"stop"}]}"#,
            r#"{"id":"c","choices":[{"index":"0","delta":{}}]}"#,
        ] {
            assert!(
                matches!(tracker.ingest(&ev(data)), Ingested::Malformed),
                "{data}"
            );
        }
        assert!(tracker.id.is_none(), "a malformed chunk teaches us nothing");
        assert!(matches!(tracker.usage, UsageParse::Absent));
        assert!(!tracker.usage_is_trusted());
        // 对照：字符串的 finish_reason 和缺省 / null 都转发
        for data in [
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":{"content":"x"},"finish_reason":null}]}"#,
            r#"{"choices":[{"finish_reason":"length"}]}"#,
        ] {
            assert!(
                matches!(tracker.ingest(&ev(data)), Ingested::Forward(_)),
                "{data}"
            );
        }
    }

    /// 可信的判定看的是**最近**的 usage：后面一个无效的 usage 会把前面有效的顶掉（保守：宁可当未知）。
    #[test]
    fn the_latest_usage_is_the_one_that_is_judged() {
        let mut tracker = track(&["content", "usage"]);
        assert!(tracker.usage_is_trusted());
        assert_eq!(tracker.usage.valid_usage().unwrap().completion_tokens, 2);
        // 一个新的、数值不同的 usage 块替换它
        tracker.ingest(&ev(
            r#"{"id":"c","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5}}"#,
        ));
        assert!(tracker.usage_is_trusted());
        assert_eq!(tracker.usage.valid_usage().unwrap().completion_tokens, 5);
        // 无效的 usage（缺 completion_tokens）也是「最近的」：可信与否没意义了，结算时它是无效的 -> 用量未知
        tracker.ingest(&ev(
            r#"{"id":"c","choices":[],"usage":{"prompt_tokens":12}}"#,
        ));
        assert!(tracker.usage.valid_usage().is_none());
    }

    /// R3 Medium-3：空 choice（`{"choices":[{}]}` 之类）不是合法的流事件：不转发、不认供应商请求号、不改变任何判断。
    #[test]
    fn an_empty_choice_is_not_a_valid_stream_event() {
        let mut tracker = StreamTracker::new(false);
        for data in [
            r#"{"choices":[{}]}"#,
            r#"{"id":"c","choices":[{}]}"#,
            r#"{"id":"c","choices":[{"index":0}]}"#,
            r#"{"id":"c","choices":[{"delta":"text"}]}"#,
            r#"{"id":"c","choices":[{"delta":null,"finish_reason":null}]}"#,
            // 带 usage 也一样：空 choice 不是「单独的 usage 块」（choices 不为空）
            r#"{"id":"c","choices":[{}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        ] {
            assert!(
                matches!(tracker.ingest(&ev(data)), Ingested::Malformed),
                "{data}"
            );
        }
        assert!(tracker.id.is_none(), "a malformed chunk teaches us nothing");
        assert!(matches!(tracker.usage, UsageParse::Absent));
        assert!(!tracker.usage_is_trusted());
        // 对照：像样的 choice 都转发
        for data in [
            r#"{"choices":[{"delta":{}}]}"#,
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
        ] {
            assert!(
                matches!(tracker.ingest(&ev(data)), Ingested::Forward(_)),
                "{data}"
            );
        }
    }

    #[test]
    fn the_stream_error_event_carries_the_stable_code_and_never_done() {
        let bytes = stream_error_event(CloudFailure::StreamTruncated);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("data: {"));
        assert!(text.ends_with("\n\n"));
        assert!(text.contains("\"reason_code\":\"stream_truncated\""));
        assert!(!text.contains("[DONE]"));
    }
}
