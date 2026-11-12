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
use idoris_upstream::cloud::{
    ChunkKind, CloudFailure, ResponseHead, SseEvent, SseParser, UsageParse, classify,
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

/// 流的观察器：供应商请求号、最终 usage、是否见过 `[DONE]`。
#[derive(Debug)]
pub(crate) struct StreamTracker {
    wants_usage_chunk: bool,
    pub(crate) id: Option<String>,
    /// 流中途出现的、与第一个不同的请求号（上游出问题的信号；对账时要标注）。
    pub(crate) changed_id: Option<String>,
    /// 最近一个带 usage 的事件的判定（最终 usage 在最后）。
    pub(crate) usage: UsageParse,
    pub(crate) saw_done: bool,
}

impl StreamTracker {
    pub(crate) fn new(wants_usage_chunk: bool) -> Self {
        Self {
            wants_usage_chunk,
            id: None,
            changed_id: None,
            usage: UsageParse::Absent,
            saw_done: false,
        }
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
                match (&self.id, &info.id) {
                    (None, Some(_)) => self.id.clone_from(&info.id),
                    (Some(first), Some(id)) if first != id && self.changed_id.is_none() => {
                        self.changed_id = Some(id.clone());
                    }
                    _ => {}
                }
                let carries_usage = !matches!(info.usage, UsageParse::Absent);
                if carries_usage {
                    self.usage = info.usage.clone();
                }
                if carries_usage && !self.wants_usage_chunk {
                    // 客户端没要 usage：整块只有 usage 就不转发，内容块里夹带的 usage 去掉
                    if info.choices_empty {
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
            phase: Phase::Reading,
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
                // 连接结束：`[DONE]` 或最终 usage 已经到了才算完成；否则是被截断
                let complete = self.tracker.saw_done
                    || (self.tracker.usage.valid_usage().is_some()
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
        // usage：只要看到了通过判定的最终 usage 就用它（它是上游对整个响应的最终计数），不管流是怎么结束的
        let (usage, claim) = judge_usage(&self.tracker.usage, &attempt);
        if !matches!(self.tracker.usage, UsageParse::Absent) || end == End::Complete {
            self.run.record_usage_claim(self.idx, &claim);
        }
        // 状态机事件：
        // - 看到了通过判定的最终 usage：`UsageReported`（不管流是怎么结束的）；
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
