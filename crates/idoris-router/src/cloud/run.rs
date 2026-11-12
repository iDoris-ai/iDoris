//! 一次云请求的运行状态 [`RunState`]：S0 的 [`Request`]（内存里的权威）+ 尝试记录库（持久化的权威），
//! 两边一起走。
//!
//! # 规矩
//!
//! - **状态变更只走 S0 的 `transition`**：库那一侧由 `AttemptStore::apply_events` 在事务里调用，
//!   内存这一侧由 `Request::apply` 调用，两边用的是同一个函数，应用之后比对结果（对不上就是 bug，返回内部错误，
//!   不继续）；
//! - **先落库，再改内存**：对「先记 `WireWriteStarted` 再写字节」尤其重要——库写失败就**不发**；
//! - **收尾恰好一次**（[`RunState::finalize`]）：算计划（S0 `plan_settlement`，纯函数）、同一个事务里把可结算的
//!   尝试推进到 `settled`、追加分录、关闭请求。`Drop` 兜底：客户端断开、future 被取消、流被丢弃，都会走到这里，
//!   所以不会留下永远「进行中」的请求；
//! - **`maybe_executed` 的尝试不释放**：没有任何事件能结算它，它的成本以预留额挂账（`hold` 分录），状态保持，
//!   等对账（设计稿 §6、ops §7）。
//!
//! # 用户只付一次
//!
//! 谁是「作答的那次」由**发布**决定：[`RunState::publish`] 记下已向客户端输出第一个字节的尝试，之后契约
//! 禁止切换（`Request::begin_next_attempt` 返回 `PublishBoundary`），结算计划里只有这一次有 `UserCharge`。
//! 没有发布过（全部失败、客户端在首字节前断开、输出被安全检查拦下）用户付 0。

use std::sync::Arc;

use idoris_compute_contracts::attempt::{AttemptEvent, AttemptState, AttemptStateKind};
use idoris_compute_contracts::ids::{AttemptId, AttemptIndex, SupplierRequestId};
use idoris_compute_contracts::permit::DataClass;
use idoris_compute_contracts::pricing::{USAGE_ROUNDING, Usage, usage_amount};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::request::{AdmittedCandidate, Attempt, Request};
use idoris_tenancy::attempts::{
    AttemptStore, FailureNote, NewRequest, RequestClose, RequestOutcome, SupplierIdOutcome,
    UsageClaim,
};
use idoris_upstream::cloud::UsageParse;

use super::clock::Clock;
use super::executor::CloudError;

/// 收尾的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub outcome: RequestOutcome,
    /// 向用户收的钱（微单位，付款账户的单位）；没有发布为 `None`。
    pub charged_micros: Option<i64>,
}

pub(crate) struct RunState {
    store: Arc<AttemptStore>,
    clock: Arc<dyn Clock>,
    pub(crate) request: Request,
    /// 每次尝试准入时的原因码（降级、宽限）。
    admission_reasons: Vec<Option<ReasonCode>>,
    finalized: bool,
    /// 没有任何尝试发布时，写进请求记录的稳定原因。
    pub(crate) close_reason: Option<&'static str>,
    /// 这个请求的租户处理儿童数据（`child_edu`）：回答必须先通过完整输出检查（S6）才能发布。
    /// 由请求的数据类别**在创建时**决定（`NewRequest::data_classes`），不是调用方事后设置的：
    /// 以后不论谁新增一个 HTTP 入口、谁在哪里调用 `publish`，儿童租户的回答没有放行标记就发布不出去。
    requires_clearance: bool,
    /// 完整输出检查已经放行（只有 [`RunState::clear_for_publication`] 能置位）。
    cleared: bool,
}

impl RunState {
    /// 请求被准入：用第一个候选开始，写请求行与第一次尝试。
    pub(crate) fn begin(
        store: Arc<AttemptStore>,
        clock: Arc<dyn Clock>,
        meta: &NewRequest<'_>,
        first: AdmittedCandidate,
    ) -> Result<Self, CloudError> {
        let reason = first.decision.reason_code();
        let request =
            Request::admit(meta.id.clone(), meta.payment.clone(), first).map_err(|e| {
                CloudError::Internal(format!("contract refused the first candidate: {e}"))
            })?;
        let first_attempt = request
            .attempts()
            .first()
            .ok_or_else(|| CloudError::Internal("a new request has no attempt".into()))?;
        store
            .create_request(meta, first_attempt)
            .map_err(CloudError::ledger)?;
        Ok(Self {
            store,
            clock,
            request,
            admission_reasons: vec![reason],
            finalized: false,
            close_reason: None,
            requires_clearance: meta.data_classes.contains(&DataClass::ChildEdu),
            cleared: false,
        })
    }

    /// 完整输出检查放行了：之后才能发布（S6）。
    pub(crate) fn clear_for_publication(&mut self) {
        self.cleared = true;
    }

    fn now(&self) -> idoris_compute_contracts::ids::TimestampMs {
        self.clock.now()
    }

    pub(crate) fn attempt_id(&self, idx: AttemptIndex) -> Result<AttemptId, CloudError> {
        self.request
            .attempt(idx)
            .map(|a| a.id().clone())
            .ok_or_else(|| CloudError::Internal(format!("no attempt {idx:?}")))
    }

    pub(crate) fn attempt(&self, idx: AttemptIndex) -> Result<Attempt, CloudError> {
        self.request
            .attempt(idx)
            .cloned()
            .ok_or_else(|| CloudError::Internal(format!("no attempt {idx:?}")))
    }

    /// 切换到下一个候选：先让契约在**副本**上校验（准入类决定、付款账户一致、尝试数、上一次不在途、
    /// **发布边界**），再落库，最后才改内存，所以库写失败不会让内存多出一次库里没有的尝试。
    pub(crate) fn begin_next(
        &mut self,
        candidate: AdmittedCandidate,
    ) -> Result<AttemptIndex, CloudError> {
        let reason = candidate.decision.reason_code();
        let mut trial = self.request.clone();
        let idx = trial
            .begin_next_attempt(candidate)
            .map_err(|e| CloudError::Internal(format!("contract refused the switch: {e}")))?;
        let attempt = trial
            .attempt(idx)
            .ok_or_else(|| CloudError::Internal("the new attempt is missing".into()))?;
        self.store
            .append_attempt(self.request.id(), attempt, self.now())
            .map_err(CloudError::ledger)?;
        self.request = trial;
        self.admission_reasons.push(reason);
        Ok(idx)
    }

    /// 对一次尝试应用事件：**先落库，再改内存**，并核对两边的结果一致。
    pub(crate) fn apply(
        &mut self,
        idx: AttemptIndex,
        events: &[AttemptEvent],
    ) -> Result<AttemptState, CloudError> {
        let id = self.attempt_id(idx)?;
        let from_store = self
            .store
            .apply_events(&id, events, self.now())
            .map_err(CloudError::ledger)?;
        let mut in_memory = None;
        for event in events {
            in_memory = Some(
                self.request
                    .apply(idx, event.clone())
                    .map_err(|e| {
                        CloudError::Internal(format!(
                            "in-memory state refused an event the store accepted: {e}"
                        ))
                    })?
                    .clone(),
            );
        }
        if in_memory.as_ref() != Some(&from_store) {
            return Err(CloudError::Internal(
                "the stored attempt state and the in-memory state diverged".into(),
            ));
        }
        Ok(from_store)
    }

    /// 失败标注（不改变状态）。库写失败只记日志：标注是给对账的人看的，不是账。
    pub(crate) fn note_failure(&self, idx: AttemptIndex, kind: &str, http_status: Option<u16>) {
        if let Ok(id) = self.attempt_id(idx)
            && let Err(error) = self.store.record_failure(
                &id,
                &FailureNote {
                    kind: kind.to_string(),
                    http_status,
                },
            )
        {
            eprintln!("[idoris] cloud: could not annotate attempt failure: {error}");
        }
    }

    /// 记下供应商请求号（正文里的 id 优先，其次是配置的响应头）。格式不对的只留标注。
    pub(crate) fn record_supplier_id(
        &mut self,
        idx: AttemptIndex,
        body_id: Option<&str>,
        header_id: Option<&str>,
    ) {
        let Some(raw) = body_id.or(header_id) else {
            return;
        };
        let Ok(id) = self.attempt_id(idx) else {
            return;
        };
        match SupplierRequestId::new(raw) {
            Err(_) => {
                if let Err(error) = self.store.note_supplier_id_invalid(&id) {
                    eprintln!("[idoris] cloud: could not annotate supplier id: {error}");
                }
            }
            Ok(supplier_id) => match self.store.record_supplier_request_id(&id, &supplier_id) {
                Ok(SupplierIdOutcome::Recorded | SupplierIdOutcome::DuplicateOf(_)) => {
                    if let Err(error) = self.request.record_supplier_request_id(idx, supplier_id) {
                        eprintln!("[idoris] cloud: supplier id mirror failed: {error}");
                    }
                }
                Ok(SupplierIdOutcome::Unchanged | SupplierIdOutcome::Conflict) => {}
                Err(error) => {
                    eprintln!("[idoris] cloud: could not record the supplier request id: {error}");
                }
            },
        }
    }

    pub(crate) fn record_usage_claim(&self, idx: AttemptIndex, claim: &UsageClaim) {
        if let Ok(id) = self.attempt_id(idx)
            && let Err(error) = self.store.record_usage_claim(&id, claim)
        {
            eprintln!("[idoris] cloud: could not record the usage claim: {error}");
        }
    }

    /// 记下「已向客户端输出第一个字节」：**先落库，再改内存**。失败时调用方**不能**把内容交给客户端
    /// （否则用户拿到了输出却没有记账）。
    pub(crate) fn publish(&mut self, idx: AttemptIndex) -> Result<(), CloudError> {
        // 儿童租户的回答没有通过完整输出检查就不能发布：不记发布、不交付任何字节（失败即关闭）
        if self.requires_clearance && !self.cleared {
            return Err(CloudError::Internal(
                "a child-data answer cannot be published before the full-output safety check cleared it"
                    .to_string(),
            ));
        }
        let id = self.attempt_id(idx)?;
        self.store
            .record_publish(&id, self.now())
            .map_err(CloudError::ledger)?;
        self.request
            .publish_first_byte(idx)
            .map_err(|e| CloudError::Internal(format!("contract refused the publish: {e}")))
    }

    pub(crate) fn is_published(&self) -> bool {
        self.request.published().is_some()
    }

    /// 这次请求被服务时带的原因码：作答那次准入时的降级 / 宽限原因，以及「首字节前切换过」。
    pub(crate) fn served_reasons(&self) -> Vec<ReasonCode> {
        let Some(answering) = self.request.published() else {
            return Vec::new();
        };
        let mut reasons = Vec::new();
        if let Some(Some(code)) = self.admission_reasons.get(usize::from(answering.0)) {
            reasons.push(*code);
        }
        if answering.0 > 0 && !reasons.contains(&ReasonCode::UpstreamFailover) {
            reasons.push(ReasonCode::UpstreamFailover);
        }
        reasons
    }

    /// 还没发布时，给调用方预估「如果现在发布会带哪些原因码」（流在交接给 HTTP 层时用）。
    pub(crate) fn prospective_reasons(&self, idx: AttemptIndex) -> Vec<ReasonCode> {
        let mut reasons = Vec::new();
        if let Some(Some(code)) = self.admission_reasons.get(usize::from(idx.0)) {
            reasons.push(*code);
        }
        if idx.0 > 0 && !reasons.contains(&ReasonCode::UpstreamFailover) {
            reasons.push(ReasonCode::UpstreamFailover);
        }
        reasons
    }

    /// **收尾**（幂等，恰好落一次账）。
    ///
    /// 1. 还在 `admitted`/`sending` 的尝试：写入 `WireWriteStarted` 之前没有字节离开本机，
    ///    用 `NeverLeftHost` 结掉；
    /// 2. 用 S0 `plan_settlement` 算计划（已发布却还停在 `maybe_executed` 的尝试——比如客户端断开而上游没有「取消」能力，
    ///    不确定是否停计费——用户按预留价付费，成本按预留额挂账，状态保持 `maybe_executed`，不释放）；
    /// 3. 同一个事务里：可结算的尝试 `Settle`、追加分录、关闭请求（`AttemptStore::finalize_request`）；
    /// 4. 内存里同样 `Settle`。
    pub(crate) fn finalize(&mut self) -> Result<Settled, CloudError> {
        if self.finalized {
            return Ok(Settled {
                outcome: if self.is_published() {
                    RequestOutcome::Answered
                } else {
                    RequestOutcome::Unanswered
                },
                charged_micros: None,
            });
        }
        // 1. 在途的尝试
        let in_flight: Vec<AttemptIndex> = self
            .request
            .attempts()
            .iter()
            .filter(|a| a.state().is_in_flight())
            .map(Attempt::index)
            .collect();
        for idx in in_flight {
            self.apply(idx, &[AttemptEvent::NeverLeftHost])?;
        }
        // 2. 计划
        let plan = self
            .request
            .plan_settlement()
            .map_err(|e| CloudError::Internal(format!("cannot plan the settlement: {e}")))?;
        let to_settle: Vec<AttemptId> = self
            .request
            .attempts()
            .iter()
            .filter(|a| {
                matches!(
                    a.state().kind(),
                    AttemptStateKind::ExecutedUsageKnown | AttemptStateKind::ExecutedUsageUnknown
                )
            })
            .map(|a| a.id().clone())
            .collect();
        let published = self.request.published();
        let outcome = if published.is_some() {
            RequestOutcome::Answered
        } else {
            RequestOutcome::Unanswered
        };
        let reason = if published.is_some() {
            self.served_reasons().first().map(|c| c.as_str())
        } else {
            self.close_reason
        };
        // 3. 落账
        self.store
            .finalize_request(
                &plan,
                &to_settle,
                RequestClose {
                    outcome,
                    reason,
                    at: self.now(),
                },
            )
            .map_err(CloudError::ledger)?;
        // 4. 内存跟上
        for id in &to_settle {
            let idx = id.index();
            self.request
                .apply(idx, AttemptEvent::Settle)
                .map_err(|e| CloudError::Internal(format!("in-memory settle refused: {e}")))?;
        }
        self.finalized = true;
        Ok(Settled {
            outcome,
            charged_micros: plan.user_charge.as_ref().map(|c| c.charged.micros()),
        })
    }
}

impl Drop for RunState {
    fn drop(&mut self) {
        if !self.finalized
            && let Err(error) = self.finalize()
        {
            eprintln!("[idoris] cloud: could not settle an abandoned request: {error}");
        }
    }
}

/// 对一次尝试里上游声称的 usage 做最后的判定（在 [`idoris_upstream::cloud::parse_usage`] 的结构检查之后）。
///
/// 额外的两条，都是为了**不让一个出错的上游改变我们收的钱**：
///
/// - `completion_tokens` 不能超过这次发给上游的 `max_tokens`（有效值，预留就是按它算的）：上游必须遵守
///   `max_tokens`，超过说明它的计数不可信；
/// - 按这次尝试的价目算金额不能溢出（`usage_amount` 全程 checked）。
///
/// 任何一条不过都按「usage 未知」处理（按预留结算），原因写进 `usage_claim_json`。
pub(crate) fn judge_usage(parse: &UsageParse, attempt: &Attempt) -> (Option<Usage>, UsageClaim) {
    let claim = |verdict: String| UsageClaim {
        verdict,
        raw: parse.raw().cloned(),
    };
    match parse {
        UsageParse::Absent => (None, claim("absent".to_string())),
        UsageParse::Invalid { reason, .. } => (None, claim(format!("invalid:{reason}"))),
        UsageParse::Present { usage: raw, .. } => {
            if raw.completion_tokens > attempt.effective_max_tokens() {
                return (
                    None,
                    claim("invalid:completion_exceeds_max_tokens".to_string()),
                );
            }
            let Ok(usage) = Usage::new(
                raw.prompt_tokens,
                raw.completion_tokens,
                raw.cached_prompt_tokens,
            ) else {
                return (None, claim("invalid:cached_exceeds_prompt".to_string()));
            };
            if usage_amount(attempt.price_rates(), &usage, USAGE_ROUNDING).is_err()
                || usage_amount(attempt.cost_rates(), &usage, USAGE_ROUNDING).is_err()
            {
                return (None, claim("invalid:amount_overflow".to_string()));
            }
            (Some(usage), claim("valid".to_string()))
        }
    }
}
