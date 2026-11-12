//! 云上游执行器：用 S0 契约的类型和纯函数，把一次用户请求驱动成「候选选择 -> 准入 -> 尝试 -> 发布边界 -> 结算」。
//!
//! 执行器**不重新实现**任何契约规则：
//!
//! | 规则 | 谁裁决 |
//! |---|---|
//! | 哪些候选合规（数据类别、区域、留存、DPA、核验过期、云形态排除 subscription、付费 / 补贴的流式请求只落支持流式 usage 的端点） | `permit::select_compliant`；没有合规候选返回 `NoCompliantEndpoint`，**此时没有任何上游调用** |
//! | 每个候选的预留额 | `reserve::reserve`（经 [`CandidateAdmission`]） |
//! | 状态怎么变 | `attempt::transition`（经 `Request::apply` 与 `AttemptStore::apply_events`） |
//! | 能不能切到下一个候选 | `Request::begin_next_attempt`：放行类的新准入、付款账户一致、尝试数、上一次不在途、**发布边界** |
//! | 谁付多少 | `request::settle_attempts`（经 `Request::plan_settlement`） |
//!
//! # 一次请求的流程
//!
//! ```text
//! select_compliant(剩下的候选) ─▶ 对第一个候选准入 ─▶ Request::admit / begin_next_attempt（落库）
//!   ─▶ prepare（没有字节离开本机；失败 = NeverLeftHost）
//!   ─▶ 落库 Dispatch + WireWriteStarted（先记状态再写字节）
//!   ─▶ send ─▶ 响应头
//!        4xx    ─▶ UpstreamRefused（证明没执行）
//!        其它失败 ─▶ 状态保持 maybe_executed（不释放）
//!        2xx    ─▶ 非流式：读完整个响应，记 usage（UsageReported 或 ExecutionObservedUsageUnknown），交给调用方发布
//!                 流式：读到第一个可转发的事件，交给 HTTP 层；第一次输出时发布
//!   首字节之前失败 ─▶ 换下一个候选（每个候选重新 select_compliant、重新准入）
//!   首字节之后失败 ─▶ 不再有任何上游调用：流以错误事件结束，按已产生的 usage 结算（未知按预留）
//! ```
//!
//! # 失败切换的范围（设计稿没写死，这里的理解）
//!
//! 首字节之前的失败都**可以**切换，但「请求本身有问题」的明确拒绝不切换：上游对**请求形状**的 4xx
//! （400 / 413 / 422 等）换一个同样合规的上游多半也会被拒，而每多发一次，孩子的输入就多送给一个上游——
//! 数据最小化。会切换的：连接 / 传输失败、超时、5xx、3xx、2xx 但响应不可用、以及说明「我们自己的凭证或额度有问题」
//! 的 401 / 402 / 403 / 404 / 408 / 425 / 429。见 [`status_allows_failover`]。

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::attempt::AttemptEvent;
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::form::DeploymentForm;
use idoris_compute_contracts::ids::{AttemptIndex, EndpointKey, RequestId};
use idoris_compute_contracts::permit::{
    Billing, CapabilityNeeds, NoCompliantEndpoint, PermitTable, RouteRequirements,
    TenantDataPolicy, select_compliant,
};
use idoris_compute_contracts::pricing::Usage;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::request::AdmittedCandidate;
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest};
use idoris_tenancy::attempts::{AttemptStore, AttemptStoreError, NewRequest};
use idoris_upstream::cloud::{
    BuildError, ChatCall, CloudFailure, CloudTimeouts, PrepareError, ResponseHead, SseError,
    SseParser, parse_completion,
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::admission::{AdmissionError, AdmissionRequest, CandidateAdmission};
use super::answer::{
    AnswerMeta, CloudOutcome, CloudStream, Ingested, PendingAnswer, StreamParts, StreamState,
    StreamTracker,
};
use super::clock::Clock;
use super::config::{CloudLimits, EndpointRegistry, EndpointRuntime};
use super::run::{RunState, judge_usage};

/// 一次云调用的输入。
pub struct CloudCall {
    pub request_id: RequestId,
    pub payment: PaymentContext,
    pub tenant: TenantDataPolicy,
    pub billing: Billing,
    /// 客户端的请求体（JSON 对象）。
    pub client_body: Map<String, Value>,
    pub stream: bool,
    /// 客户端自己要了流内 usage 块（`stream_options.include_usage`）：要了就转发，没要就不转发（结算照样用）。
    pub client_wants_usage_chunk: bool,
    pub requested_max_tokens: Option<u64>,
    pub input_tokens_estimate: u64,
    /// 按偏好排好序的候选。
    pub candidates: Vec<EndpointKey>,
    pub needs: CapabilityNeeds,
    /// 客户端断开（或请求被取消）时触发：取消进行中的上游调用。
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for CloudCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印请求体（里面是用户的内容）
        f.debug_struct("CloudCall")
            .field("request_id", &self.request_id)
            .field("stream", &self.stream)
            .field("candidates", &self.candidates.len())
            .finish_non_exhaustive()
    }
}

/// 一次失败的摘要（稳定的代码，不含正文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureInfo {
    pub code: &'static str,
    pub http_status: Option<u16>,
    pub timeout: bool,
}

impl From<CloudFailure> for FailureInfo {
    fn from(failure: CloudFailure) -> Self {
        Self {
            code: failure.code(),
            http_status: failure.http_status(),
            timeout: failure.is_timeout(),
        }
    }
}

/// 云调用的错误。
#[derive(Debug)]
pub enum CloudError {
    /// 没有合规候选（硬约束过滤后为空）。**没有任何上游调用发生。**
    NoCompliantEndpoint(NoCompliantEndpoint),
    /// 准入拒绝了每一个候选（原因码是第一个被拒绝的候选的）。
    Denied(ReasonCode),
    /// 合规的候选都没有配置付款账户单位对应的价目。
    NoPricedEndpoint,
    /// 合规的候选在配置里找不到。
    EndpointNotConfigured,
    /// 客户端的请求体不合法（构造上游请求时发现）。
    InvalidRequest(BuildError),
    /// 上游明确拒绝了请求（4xx，且不适合换上游重试）。
    UpstreamRejected {
        status: u16,
    },
    /// 每个能试的候选都失败了（或候选用完了）。
    UpstreamUnavailable {
        last: Option<FailureInfo>,
        attempts: usize,
    },
    DeadlineExceeded,
    Cancelled,
    /// 准入本身出错（失败即关闭）。
    Admission(AdmissionError),
    /// 尝试记录库出错（失败即关闭：不放行、不出网）。
    Ledger(String),
    Internal(String),
}

impl CloudError {
    pub(crate) fn ledger(error: AttemptStoreError) -> Self {
        CloudError::Ledger(error.to_string())
    }

    /// 写进请求记录的稳定原因（没有任何尝试发布时）。
    pub fn close_reason(&self) -> &'static str {
        match self {
            CloudError::NoCompliantEndpoint(_) => "no_compliant_endpoint",
            CloudError::Denied(code) => code.as_str(),
            CloudError::NoPricedEndpoint => "no_priced_endpoint",
            CloudError::EndpointNotConfigured => "endpoint_not_configured",
            CloudError::InvalidRequest(_) => "invalid_request",
            CloudError::UpstreamRejected { .. } => "upstream_rejected",
            CloudError::UpstreamUnavailable { .. } => "upstream_unavailable",
            CloudError::DeadlineExceeded => "deadline_exceeded",
            CloudError::Cancelled => "client_cancelled",
            CloudError::Admission(_) | CloudError::Ledger(_) => "ledger_unavailable",
            CloudError::Internal(_) => "internal_error",
        }
    }
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不含请求内容、响应正文或密钥
        match self {
            CloudError::NoCompliantEndpoint(none) => write!(f, "{none}"),
            CloudError::Denied(code) => write!(f, "admission denied: {code}"),
            CloudError::NoPricedEndpoint => {
                f.write_str("no compliant endpoint has a price for the payer's unit")
            }
            CloudError::EndpointNotConfigured => {
                f.write_str("a compliant endpoint is not configured")
            }
            CloudError::InvalidRequest(e) => write!(f, "invalid request: {e}"),
            CloudError::UpstreamRejected { status } => {
                write!(f, "upstream rejected the request: HTTP {status}")
            }
            CloudError::UpstreamUnavailable { last, attempts } => match last {
                Some(last) => write!(
                    f,
                    "upstream unavailable after {attempts} attempt(s): {}",
                    last.code
                ),
                None => write!(f, "upstream unavailable after {attempts} attempt(s)"),
            },
            CloudError::DeadlineExceeded => f.write_str("the request deadline was exceeded"),
            CloudError::Cancelled => f.write_str("the request was cancelled"),
            CloudError::Admission(e) => write!(f, "{e}"),
            CloudError::Ledger(e) => write!(f, "attempt ledger error: {e}"),
            CloudError::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl std::error::Error for CloudError {}

/// 一次上游 4xx 是否值得换下一个候选。
///
/// 会切换：说明「我们自己的凭证 / 额度 / 路由有问题」或临时性的（401、402、403、404、408、425、429）。
/// 不切换：请求形状本身被拒（400、413、422……）：换一个同样合规的上游多半一样被拒，
/// 而每多发一次，请求内容就多送给一个上游（数据最小化）。
/// 3xx / 5xx / 其它：都切换（它们不能证明没执行，状态保持 `maybe_executed`）。
pub fn status_allows_failover(status: u16) -> bool {
    match status {
        401 | 402 | 403 | 404 | 408 | 425 | 429 => true,
        400..=499 => false,
        _ => true,
    }
}

enum AttemptResult {
    Answered(Answered),
    Failed {
        failure: FailureInfo,
        failover: bool,
    },
}

enum Answered {
    Complete { body: Vec<u8> },
    Stream(Box<StreamHandoff>),
}

/// 首个可转发事件已经读到、准备交给 HTTP 层的流。
struct StreamHandoff {
    head: ResponseHead,
    parser: SseParser,
    tracker: StreamTracker,
    pending: VecDeque<Vec<u8>>,
}

/// 云上游执行器。
pub struct CloudExecutor {
    pub(crate) permits: Arc<PermitTable>,
    pub(crate) endpoints: Arc<EndpointRegistry>,
    pub(crate) admission: Arc<dyn CandidateAdmission>,
    pub(crate) store: Arc<AttemptStore>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) reserve_policy: ReservePolicy,
    pub(crate) limits: CloudLimits,
}

impl std::fmt::Debug for CloudExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudExecutor")
            .field("endpoints", &self.endpoints.len())
            .field("permits", &self.permits.len())
            .finish_non_exhaustive()
    }
}

impl CloudExecutor {
    pub fn new(
        permits: Arc<PermitTable>,
        endpoints: Arc<EndpointRegistry>,
        admission: Arc<dyn CandidateAdmission>,
        store: Arc<AttemptStore>,
        clock: Arc<dyn Clock>,
        reserve_policy: ReservePolicy,
        limits: CloudLimits,
    ) -> Self {
        Self {
            permits,
            endpoints,
            admission,
            store,
            clock,
            reserve_policy,
            limits,
        }
    }

    /// 执行一次云调用。成功返回一个等待发布的回答或一条流；失败时请求已经收尾（没有悬空的尝试）。
    pub async fn execute(&self, call: CloudCall) -> Result<CloudOutcome, CloudError> {
        let mut slot: Option<RunState> = None;
        match self.execute_inner(&call, &mut slot).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                // 已经开始的请求：记下原因并收尾。没开始的（被拒绝）已经在 execute_inner 里记了一行。
                if let Some(mut run) = slot.take() {
                    run.close_reason = Some(error.close_reason());
                    if let Err(finalize_error) = run.finalize() {
                        eprintln!(
                            "[idoris] cloud: could not settle a failed request: {finalize_error}"
                        );
                    }
                }
                Err(error)
            }
        }
    }

    fn timeouts(&self, deadline: Instant) -> CloudTimeouts {
        let remaining = deadline.saturating_duration_since(Instant::now());
        CloudTimeouts {
            response_head: self.limits.response_head.min(remaining),
            body_total: self.limits.body_total.min(remaining),
            stream_idle: self.limits.stream_idle,
        }
    }

    fn new_request<'a>(
        &self,
        call: &'a CloudCall,
        classes: &'a BTreeSet<idoris_compute_contracts::permit::DataClass>,
    ) -> NewRequest<'a> {
        NewRequest {
            id: &call.request_id,
            payment: &call.payment,
            form: DeploymentForm::Cloud,
            billing: call.billing,
            stream: call.stream,
            data_classes: classes,
            created_at: self.clock.now(),
        }
    }

    /// 没有任何尝试发出就被拒：留一行已关闭的请求记录（审计），然后返回错误。
    fn reject(&self, call: &CloudCall, error: CloudError) -> CloudError {
        if let CloudError::NoCompliantEndpoint(none) = &error {
            // 只有端点名和排除原因的枚举，没有任何请求内容：给运维看「为什么没有合规候选」
            for excluded in &none.excluded {
                eprintln!(
                    "[idoris] cloud: request {} excluded {}: {:?}",
                    call.request_id, excluded.endpoint, excluded.reasons
                );
            }
        }
        let classes = call.tenant.data_classes();
        if let Err(record_error) = self
            .store
            .record_rejected_request(&self.new_request(call, classes), error.close_reason())
        {
            eprintln!("[idoris] cloud: could not record a rejected request: {record_error}");
        }
        error
    }

    async fn execute_inner(
        &self,
        call: &CloudCall,
        slot: &mut Option<RunState>,
    ) -> Result<CloudOutcome, CloudError> {
        let deadline = Instant::now() + self.limits.total_deadline;
        let requirements = RouteRequirements {
            form: DeploymentForm::Cloud,
            tenant: call.tenant.clone(),
            stream: call.stream,
            billing: call.billing,
            needs: call.needs,
        };
        let mut tried: BTreeSet<EndpointKey> = BTreeSet::new();
        let mut last_failure: Option<FailureInfo> = None;
        let mut denial: Option<ReasonCode> = None;
        let mut no_price = false;
        let mut unconfigured = false;
        let mut started = 0usize;

        loop {
            if call.cancel.is_cancelled() {
                return Err(self.stop(call, slot, CloudError::Cancelled));
            }
            if Instant::now() >= deadline {
                return Err(self.stop(call, slot, CloudError::DeadlineExceeded));
            }
            if started >= self.limits.max_attempts {
                let error = CloudError::UpstreamUnavailable {
                    last: last_failure,
                    attempts: started,
                };
                return Err(error);
            }

            // ---- 1. 候选：每一轮都对「还没试过的」重新跑一遍硬约束（核验可能在两次尝试之间过期）----
            let remaining: Vec<EndpointKey> = call
                .candidates
                .iter()
                .filter(|key| !tried.contains(*key))
                .cloned()
                .collect();
            let compliant = match select_compliant(
                &self.permits,
                &remaining,
                &requirements,
                self.clock.now(),
            ) {
                Ok(list) => list.iter().map(|permit| permit.key()).collect::<Vec<_>>(),
                Err(none) => {
                    return Err(if slot.is_none() {
                        self.reject(call, CloudError::NoCompliantEndpoint(none))
                    } else {
                        CloudError::UpstreamUnavailable {
                            last: last_failure,
                            attempts: started,
                        }
                    });
                }
            };

            // ---- 2. 准入：按偏好顺序，第一个被放行的候选 ----
            let mut picked: Option<(&EndpointRuntime, AdmittedCandidate)> = None;
            for key in compliant {
                tried.insert(key.clone());
                let Some(endpoint) = self.endpoints.get(&key) else {
                    unconfigured = true;
                    continue;
                };
                let Some(price_rates) = endpoint.price_rates.get(call.payment.unit()) else {
                    no_price = true;
                    continue;
                };
                let decision = self
                    .admission
                    .admit(&AdmissionRequest {
                        request_id: &call.request_id,
                        payment: &call.payment,
                        endpoint: &key,
                        sizing: ReserveRequest {
                            requested_max_tokens: call.requested_max_tokens,
                            input_tokens_estimate: call.input_tokens_estimate,
                        },
                        policy: &self.reserve_policy,
                        price_rates,
                        cost_rates: &endpoint.cost_rates,
                        candidate_ordinal: started,
                        now: self.clock.now(),
                    })
                    .map_err(|e| self.stop(call, slot, CloudError::Admission(e)))?;
                match decision {
                    AdmissionDecision::Reject { reason } => {
                        denial.get_or_insert(reason.code());
                    }
                    admitted => {
                        picked = Some((
                            endpoint,
                            AdmittedCandidate {
                                decision: admitted,
                                price_version: endpoint.price_version.clone(),
                                cost_rates: endpoint.cost_rates.clone(),
                                price_rates: price_rates.clone(),
                            },
                        ));
                        break;
                    }
                }
            }
            let Some((endpoint, candidate)) = picked else {
                let error = if slot.is_some() && last_failure.is_some() {
                    CloudError::UpstreamUnavailable {
                        last: last_failure,
                        attempts: started,
                    }
                } else if let Some(code) = denial {
                    CloudError::Denied(code)
                } else if no_price {
                    CloudError::NoPricedEndpoint
                } else if unconfigured {
                    CloudError::EndpointNotConfigured
                } else {
                    CloudError::UpstreamUnavailable {
                        last: last_failure,
                        attempts: started,
                    }
                };
                return Err(if slot.is_none() {
                    self.reject(call, error)
                } else {
                    error
                });
            };

            // ---- 3. 开始这次尝试（契约校验 + 落库）----
            let idx = match slot.as_mut() {
                None => {
                    let classes = call.tenant.data_classes();
                    let meta = self.new_request(call, classes);
                    *slot = Some(RunState::begin(
                        self.store.clone(),
                        self.clock.clone(),
                        &meta,
                        candidate,
                    )?);
                    AttemptIndex(0)
                }
                Some(run) => run.begin_next(candidate)?,
            };
            started += 1;
            let run = slot
                .as_mut()
                .ok_or_else(|| CloudError::Internal("the run vanished".into()))?;

            // ---- 4. 执行这次尝试 ----
            match self.run_attempt(call, run, idx, endpoint, deadline).await {
                Err(error) => return Err(error),
                Ok(AttemptResult::Failed { failure, failover }) => {
                    last_failure = Some(failure);
                    if !failover {
                        return Err(match failure.http_status {
                            Some(status) => CloudError::UpstreamRejected { status },
                            None => CloudError::UpstreamUnavailable {
                                last: last_failure,
                                attempts: started,
                            },
                        });
                    }
                }
                Ok(AttemptResult::Answered(answered)) => {
                    let run = slot
                        .take()
                        .ok_or_else(|| CloudError::Internal("the run vanished".into()))?;
                    let meta = AnswerMeta {
                        reasons: run.prospective_reasons(idx),
                        request_id: call.request_id.to_string(),
                    };
                    return Ok(match answered {
                        Answered::Complete { body } => {
                            CloudOutcome::Complete(Box::new(PendingAnswer {
                                run,
                                idx,
                                body,
                                meta,
                            }))
                        }
                        Answered::Stream(handoff) => {
                            let StreamHandoff {
                                head,
                                parser,
                                tracker,
                                pending,
                            } = *handoff;
                            CloudOutcome::Stream(CloudStream::new(
                                StreamState::new(StreamParts {
                                    run,
                                    idx,
                                    head,
                                    parser,
                                    tracker,
                                    pending,
                                    cancel: call.cancel.clone(),
                                    stream_total: self.limits.stream_total,
                                    upstream_stops_on_cancel: self
                                        .permits
                                        .lookup(&endpoint.key)
                                        .is_some_and(|permit| permit.capabilities.cancellation),
                                }),
                                meta,
                            ))
                        }
                    });
                }
            }
        }
    }

    /// 在循环里遇到不可恢复的错误：已经开始的请求交给 `execute` 收尾；没开始的留一行拒绝记录。
    fn stop(&self, call: &CloudCall, slot: &Option<RunState>, error: CloudError) -> CloudError {
        if slot.is_none() {
            self.reject(call, error)
        } else {
            error
        }
    }

    /// 在等待网络时同时盯着取消信号。取消时丢弃 `future`（关闭到上游的连接）。
    async fn cancellable<T>(
        &self,
        call: &CloudCall,
        future: impl std::future::Future<Output = T>,
    ) -> Result<T, CloudError> {
        tokio::select! {
            biased;
            () = call.cancel.cancelled() => Err(CloudError::Cancelled),
            value = future => Ok(value),
        }
    }

    async fn run_attempt(
        &self,
        call: &CloudCall,
        run: &mut RunState,
        idx: AttemptIndex,
        endpoint: &EndpointRuntime,
        deadline: Instant,
    ) -> Result<AttemptResult, CloudError> {
        let attempt = run.attempt(idx)?;
        let no_extra = Map::new();
        let chat_call = ChatCall {
            client_body: &call.client_body,
            upstream_model: endpoint.key.model.as_str(),
            // 必须是预留时算出的有效值：否则上游可以输出超过预留的内容
            max_tokens: attempt.effective_max_tokens(),
            stream: call.stream,
            extra_body: &no_extra,
        };

        // ---- prepare：没有任何字节离开本机，失败是安全的 ----
        let prepared = match self
            .cancellable(
                call,
                endpoint.client.prepare(&chat_call, self.timeouts(deadline)),
            )
            .await?
        {
            Ok(prepared) => prepared,
            Err(PrepareError::Body(error)) => {
                run.apply(idx, &[AttemptEvent::NeverLeftHost])?;
                run.note_failure(idx, "invalid_request_body", None);
                return Err(CloudError::InvalidRequest(error));
            }
            Err(error) => {
                run.apply(idx, &[AttemptEvent::NeverLeftHost])?;
                run.note_failure(idx, error.code(), None);
                return Ok(AttemptResult::Failed {
                    failure: FailureInfo {
                        code: error.code(),
                        http_status: None,
                        timeout: false,
                    },
                    failover: true,
                });
            }
        };

        // ---- 先记状态，再写字节 ----
        // 库写失败就返回错误：**不发**。从这一刻起，崩溃后账上永远是保守的（maybe_executed）。
        run.apply(
            idx,
            &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        )?;

        // ---- send：字节可能已经出去 ----
        let head = match self.cancellable(call, prepared.send()).await? {
            Ok(head) => head,
            Err(failure) => {
                // 连接 / 传输 / 响应头超时：没有事件能证明没执行，状态保持 maybe_executed
                run.note_failure(idx, failure.code(), failure.http_status());
                return Ok(AttemptResult::Failed {
                    failure: failure.into(),
                    failover: true,
                });
            }
        };

        if !head.is_success() {
            let status = head.status;
            drop(head); // 不读错误正文，直接关连接
            run.note_failure(idx, "http_status", Some(status));
            if (400..=499).contains(&status) {
                // 4xx：上游明确拒绝，证明没执行（契约只认 4xx）
                run.apply(
                    idx,
                    &[AttemptEvent::UpstreamRefused {
                        http_status: status,
                    }],
                )?;
            }
            return Ok(AttemptResult::Failed {
                failure: CloudFailure::Status(status).into(),
                failover: status_allows_failover(status),
            });
        }

        if call.stream {
            self.start_stream(call, run, idx, head, deadline).await
        } else {
            self.finish_non_stream(call, run, idx, head).await
        }
    }

    /// 2xx 之后出的任何问题：上游已经接受并（多半）执行了，所以是「确认执行了，usage 未知」。
    fn observed_but_unusable(
        run: &mut RunState,
        idx: AttemptIndex,
        failure: CloudFailure,
    ) -> Result<AttemptResult, CloudError> {
        run.note_failure(idx, failure.code(), failure.http_status());
        run.apply(idx, &[AttemptEvent::ExecutionObservedUsageUnknown])?;
        Ok(AttemptResult::Failed {
            failure: failure.into(),
            failover: true,
        })
    }

    async fn finish_non_stream(
        &self,
        call: &CloudCall,
        run: &mut RunState,
        idx: AttemptIndex,
        head: ResponseHead,
    ) -> Result<AttemptResult, CloudError> {
        let header_id = head.supplier_request_id_header.clone();
        let body = match self.cancellable(call, head.read_body()).await? {
            Ok(body) => body,
            Err(failure) => return Self::observed_but_unusable(run, idx, failure),
        };
        let parsed = match parse_completion(&body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Self::observed_but_unusable(run, idx, CloudFailure::Response(error));
            }
        };
        run.record_supplier_id(idx, parsed.id.as_deref(), header_id.as_deref());
        let attempt = run.attempt(idx)?;
        let (usage, claim) = judge_usage(&parsed.usage, &attempt);
        run.record_usage_claim(idx, &claim);
        run.apply(idx, &[usage_event(usage)])?;
        Ok(AttemptResult::Answered(Answered::Complete { body }))
    }

    /// 流式：读到第一个**可转发**的事件才算这次尝试成功。在此之前的任何失败（断开、静默超时、上游报错、
    /// 不是合法的流）都还没有向客户端输出任何字节，可以换下一个候选。
    async fn start_stream(
        &self,
        call: &CloudCall,
        run: &mut RunState,
        idx: AttemptIndex,
        mut head: ResponseHead,
        deadline: Instant,
    ) -> Result<AttemptResult, CloudError> {
        if !head.is_event_stream() {
            return Self::observed_but_unusable(run, idx, CloudFailure::StreamMalformed);
        }
        let header_id = head.supplier_request_id_header.clone();
        let mut parser = SseParser::new(self.limits.max_event_bytes);
        let mut tracker = StreamTracker::new(call.client_wants_usage_chunk);
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        while pending.is_empty() {
            // 首字节之前也受整个请求的期限约束
            let left = deadline.saturating_duration_since(Instant::now());
            let chunk = match self
                .cancellable(call, tokio::time::timeout(left, head.next_chunk()))
                .await?
            {
                Err(_elapsed) => {
                    return Self::observed_but_unusable(run, idx, CloudFailure::StreamIdleTimeout);
                }
                Ok(Err(failure)) => return Self::observed_but_unusable(run, idx, failure),
                // 在任何可转发的事件之前流就结束了
                Ok(Ok(None)) => {
                    return Self::observed_but_unusable(run, idx, CloudFailure::StreamTruncated);
                }
                Ok(Ok(Some(chunk))) => chunk,
            };
            let events = match parser.push(&chunk) {
                Ok(events) => events,
                Err(SseError::EventTooLarge) => {
                    return Self::observed_but_unusable(
                        run,
                        idx,
                        CloudFailure::StreamEventTooLarge,
                    );
                }
            };
            for event in events {
                match tracker.ingest(&event) {
                    Ingested::Forward(bytes) => pending.push_back(bytes),
                    Ingested::Skip => {}
                    // 发布之前不转发注释：一个 keep-alive 不能提前占用「第一个字节」
                    Ingested::Comment(bytes) => {
                        if !pending.is_empty() {
                            pending.push_back(bytes);
                        }
                    }
                    Ingested::Done(bytes) => {
                        if pending.is_empty() {
                            // 一个内容都没有就 [DONE]：空流
                            return Self::observed_but_unusable(
                                run,
                                idx,
                                CloudFailure::StreamMalformed,
                            );
                        }
                        pending.push_back(bytes);
                    }
                    Ingested::UpstreamError => {
                        return Self::observed_but_unusable(
                            run,
                            idx,
                            CloudFailure::StreamErrorEvent,
                        );
                    }
                    Ingested::Malformed => {
                        return Self::observed_but_unusable(
                            run,
                            idx,
                            CloudFailure::StreamMalformed,
                        );
                    }
                }
            }
        }
        run.record_supplier_id(idx, tracker.id.as_deref(), header_id.as_deref());
        Ok(AttemptResult::Answered(Answered::Stream(Box::new(
            StreamHandoff {
                head,
                parser,
                tracker,
                pending,
            },
        ))))
    }
}

fn usage_event(usage: Option<Usage>) -> AttemptEvent {
    match usage {
        Some(usage) => AttemptEvent::UsageReported { usage },
        None => AttemptEvent::ExecutionObservedUsageUnknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_policy_by_status() {
        // 说明「我们自己的凭证 / 额度 / 路由有问题」或临时性的：换下一个候选
        for status in [401, 402, 403, 404, 408, 425, 429] {
            assert!(status_allows_failover(status), "{status}");
        }
        // 请求形状本身被拒：不换（数据最小化：不把输入多送给一个上游）
        for status in [400, 405, 406, 410, 413, 414, 415, 422, 431, 451] {
            assert!(!status_allows_failover(status), "{status}");
        }
        // 3xx / 5xx / 其它：都换（它们不能证明没执行，状态保持 maybe_executed）
        for status in [
            300, 301, 302, 307, 500, 501, 502, 503, 504, 520, 599, 0, 600,
        ] {
            assert!(status_allows_failover(status), "{status}");
        }
    }

    #[test]
    fn close_reasons_are_stable_strings() {
        assert_eq!(CloudError::Cancelled.close_reason(), "client_cancelled");
        assert_eq!(
            CloudError::DeadlineExceeded.close_reason(),
            "deadline_exceeded"
        );
        assert_eq!(
            CloudError::UpstreamUnavailable {
                last: None,
                attempts: 1
            }
            .close_reason(),
            "upstream_unavailable"
        );
        assert_eq!(
            CloudError::Denied(ReasonCode::BudgetExhausted).close_reason(),
            "budget_exhausted"
        );
        assert_eq!(
            CloudError::Ledger("x".into()).close_reason(),
            "ledger_unavailable"
        );
    }

    #[test]
    fn errors_print_without_content_or_secrets() {
        let text = format!(
            "{} | {}",
            CloudError::Ledger("disk full".into()),
            CloudError::UpstreamRejected { status: 400 }
        );
        assert!(text.contains("disk full") && text.contains("HTTP 400"));
    }
}
