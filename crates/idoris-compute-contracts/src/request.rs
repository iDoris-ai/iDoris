//! 请求（一组尝试）、发布边界、以及「一组尝试 → 谁付多少」的纯函数（设计稿 §6）。
//!
//! 单次尝试的合法转移见 [`crate::attempt`]。本模块管跨尝试的规则，每一条都有测试：
//!
//! 1. **每次尝试单独记录**：[`Attempt`] 记供应商/模型/端点（[`EndpointKey`]）、价格版本、用量、
//!    执行确定性、预留。
//! 2. **切换候选前要重新准入**：[`Request::begin_next_attempt`] 必须带一个新的 [`AdmittedCandidate`]，
//!    里面是对**新候选**的准入决定；它必须是放行类（放行/降级/宽限），付款账户必须就是本次请求的付款上下文。
//!    拒绝的决定进不来。
//! 3. **发布边界**：[`Request::publish_first_byte`] 记下「已向客户端输出第一个字节」。之后
//!    [`Request::begin_next_attempt`] 一律返回 [`RequestError::PublishBoundary`]：禁止透明重试或切换。
//!    只有**最新**的一次尝试可以发布（迟到的、已被放弃的尝试的回答不能再给客户端）。
//! 4. **`maybe_executed` 的成本保留待核销**：[`settle_attempts`] 把它们的成本按**预留额**挂账
//!    （[`CostBasis::ReservedPendingReconciliation`]），列在 [`SettlementPlan::pending_reconciliation`]，
//!    不会被当成零。
//! 5. **用户只付一次**：[`settle_attempts`] 只对**发布了的那一次尝试**向用户计价（[`UserCharge`]）；
//!    其余尝试用户付 0，它们的成本记给平台（[`AttemptRole::Extra`]）。没有任何尝试发布过
//!    （比如输出没通过安全检查）就没有 [`UserCharge`]，用户付 0。
//! 6. **流式 usage 未知时按预留结算**：发布了的尝试若 usage 未知，用户按预留价付
//!    （[`ChargeBasis::ReservedAmount`]）。
//!
//! # 谁付多少（一次请求的结算计划）
//!
//! | 尝试 | 用户付 | 平台承担的上游成本 | 用户侧预留 |
//! |---|---|---|---|
//! | 发布了的那次，usage 已知 | 实际价 | 实际成本（毛利 = 价 - 成本，另算） | 释放 `预留 - 实际价`（若为正）；实际价超出预留的部分是 `overage` |
//! | 发布了的那次，usage 未知 / 仍是 `maybe_executed` | 预留价 | 预留成本，待核销 | 全部转为扣费，不释放 |
//! | 其它尝试，`not_sent` | 0 | 0 | 全部释放 |
//! | 其它尝试，已执行、usage 已知 | 0 | **实际成本**（额外尝试，平台承担） | 全部释放 |
//! | 其它尝试，`maybe_executed` / usage 未知 | 0 | **预留成本，待核销**（不释放） | 全部释放 |
//!
//! 「用户侧预留全部释放」指占用用户额度的那笔**价**的预留；平台承担的**成本**是另一回事，保留在平台账上直到
//! 供应商账单或 usage 回来。超出预留的 `overage` 怎么分给担保人和平台损失账户见 [`split_overage`]。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::admission::{AdmissionDecision, Reservation};
use crate::attempt::{
    AttemptEvent, AttemptEventKind, AttemptState, AttemptStateKind, Certainty, SettleBasis,
    TransitionError, transition,
};
use crate::credential::PaymentContext;
use crate::ids::{AccountId, AttemptId, AttemptIndex, EndpointKey, PriceVersion, RequestId};
use crate::money::{MoneyError, Unit};
use crate::pricing::{Cost, Price, RateCard, USAGE_ROUNDING, usage_amount};
use crate::reason::ReasonCode;

/// 一次请求最多几次尝试。设计稿没有给数；每多一次尝试，平台就可能多背一份成本
/// （「额外尝试的成本由平台承担」），所以必须有上限。
pub const MAX_ATTEMPTS_PER_REQUEST: usize = 4;

/// 一个已经准入、准备开始的候选：准入决定 + 计价所需的价目表版本与价格表。
///
/// 端点、付款账户、预留都在 `decision` 里（[`crate::admission::Admitted`]）：**对哪个候选的准入，
/// 这次尝试就是哪个候选**，没有第二份端点字段可以和准入对不上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedCandidate {
    pub decision: AdmissionDecision,
    pub price_version: PriceVersion,
    /// 上游成本价目（供应商币种）。
    pub cost_rates: RateCard<Cost>,
    /// 对用户的价目（账户单位）。
    pub price_rates: RateCard<Price>,
}

/// 一次尝试的完整记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attempt {
    id: AttemptId,
    endpoint: EndpointKey,
    price_version: PriceVersion,
    cost_rates: RateCard<Cost>,
    price_rates: RateCard<Price>,
    effective_max_tokens: u64,
    reservation: Reservation,
    state: AttemptState,
}

impl Attempt {
    pub fn id(&self) -> &AttemptId {
        &self.id
    }

    pub fn index(&self) -> AttemptIndex {
        self.id.index()
    }

    pub fn endpoint(&self) -> &EndpointKey {
        &self.endpoint
    }

    pub fn price_version(&self) -> &PriceVersion {
        &self.price_version
    }

    pub fn cost_rates(&self) -> &RateCard<Cost> {
        &self.cost_rates
    }

    pub fn price_rates(&self) -> &RateCard<Price> {
        &self.price_rates
    }

    /// 发给上游的 `max_tokens`。
    pub fn effective_max_tokens(&self) -> u64 {
        self.effective_max_tokens
    }

    pub fn reservation(&self) -> &Reservation {
        &self.reservation
    }

    pub fn state(&self) -> &AttemptState {
        &self.state
    }

    pub fn certainty(&self) -> Certainty {
        self.state.certainty()
    }
}

/// 请求层面的错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("the admission decision does not admit the request (reason: {reason:?})")]
    NotAdmitted { reason: Option<ReasonCode> },
    #[error("admission is for payer {got} but the request's payment context is {expected}")]
    PayerMismatch { expected: AccountId, got: AccountId },
    #[error("{what}: expected unit {expected}, got {got}")]
    UnitMismatch {
        what: &'static str,
        expected: Unit,
        got: Unit,
    },
    #[error("effective max_tokens must be >= 1")]
    ZeroMaxTokens,
    #[error(
        "the first byte was already sent to the client: transparent retry or switch is forbidden"
    )]
    PublishBoundary,
    #[error("attempt {index:?} is still in flight; wait for it before starting another")]
    AttemptInFlight { index: AttemptIndex },
    #[error("a request may have at most {max} attempts")]
    TooManyAttempts { max: usize },
    #[error("no such attempt {0:?}")]
    NoSuchAttempt(AttemptIndex),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("a first byte was already sent for this request")]
    AlreadyPublished,
    #[error("only the latest attempt ({latest:?}) may publish, not {index:?}")]
    PublishNotLatestAttempt {
        index: AttemptIndex,
        latest: AttemptIndex,
    },
    #[error("an attempt in state {state:?} cannot publish")]
    PublishInvalidState {
        index: AttemptIndex,
        state: AttemptStateKind,
    },
    #[error("attempt {0:?} already published bytes; it cannot be declared not executed")]
    PublishedAttemptCannotBeRefused(AttemptIndex),
    #[error("invalid request record: {0}")]
    InvalidRecord(String),
}

/// 一次用户请求：付款上下文 + 一串尝试 + 发布边界。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RequestWire")]
pub struct Request {
    id: RequestId,
    payment: PaymentContext,
    attempts: Vec<Attempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    published: Option<AttemptIndex>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptWire {
    id: AttemptId,
    endpoint: EndpointKey,
    price_version: PriceVersion,
    cost_rates: RateCard<Cost>,
    price_rates: RateCard<Price>,
    effective_max_tokens: u64,
    reservation: Reservation,
    state: AttemptState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestWire {
    id: RequestId,
    payment: PaymentContext,
    attempts: Vec<AttemptWire>,
    #[serde(default)]
    published: Option<AttemptIndex>,
}

impl TryFrom<RequestWire> for Request {
    type Error = RequestError;
    fn try_from(wire: RequestWire) -> Result<Self, RequestError> {
        let request = Request {
            id: wire.id,
            payment: wire.payment,
            attempts: wire
                .attempts
                .into_iter()
                .map(|a| Attempt {
                    id: a.id,
                    endpoint: a.endpoint,
                    price_version: a.price_version,
                    cost_rates: a.cost_rates,
                    price_rates: a.price_rates,
                    effective_max_tokens: a.effective_max_tokens,
                    reservation: a.reservation,
                    state: a.state,
                })
                .collect(),
            published: wire.published,
        };
        request.validate()?;
        Ok(request)
    }
}

fn invalid(message: impl Into<String>) -> RequestError {
    RequestError::InvalidRecord(message.into())
}

impl Request {
    /// 请求被准入：用**第一个候选**的准入决定开始。第一次尝试以 `admitted` 状态创建。
    pub fn admit(
        id: RequestId,
        payment: PaymentContext,
        first: AdmittedCandidate,
    ) -> Result<Request, RequestError> {
        let attempt = make_attempt(&id, &payment, AttemptIndex(0), first)?;
        Ok(Request {
            id,
            payment,
            attempts: vec![attempt],
            published: None,
        })
    }

    pub fn id(&self) -> &RequestId {
        &self.id
    }

    pub fn payment(&self) -> &PaymentContext {
        &self.payment
    }

    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    pub fn attempt(&self, index: AttemptIndex) -> Option<&Attempt> {
        self.attempts.get(usize::from(index.0))
    }

    /// 已经向客户端输出第一个字节的那次尝试；没发布过为 `None`。
    pub fn published(&self) -> Option<AttemptIndex> {
        self.published
    }

    /// 最新一次尝试的序号。
    pub fn latest(&self) -> AttemptIndex {
        // 构造时保证 attempts 非空且不超过 MAX_ATTEMPTS_PER_REQUEST（<= u8::MAX）
        AttemptIndex(u8::try_from(self.attempts.len().saturating_sub(1)).unwrap_or(u8::MAX))
    }

    /// 开始下一次尝试（切换候选或重试）。规则，按这个顺序检查：
    ///
    /// 1. 已经发布过第一个字节 → [`RequestError::PublishBoundary`]（**发布边界**）；
    /// 2. 已有 [`MAX_ATTEMPTS_PER_REQUEST`] 次尝试 → [`RequestError::TooManyAttempts`]；
    /// 3. 上一次尝试还在 `admitted`/`sending` → [`RequestError::AttemptInFlight`]；
    /// 4. `next` 里的准入决定必须是放行类、付款账户与本次请求一致、单位一致（**重新准入**）。
    ///
    /// 上一次尝试不会因此改变状态：`maybe_executed` 仍是 `maybe_executed`，成本照旧保留待核销。
    pub fn begin_next_attempt(
        &mut self,
        next: AdmittedCandidate,
    ) -> Result<AttemptIndex, RequestError> {
        if self.published.is_some() {
            return Err(RequestError::PublishBoundary);
        }
        if self.attempts.len() >= MAX_ATTEMPTS_PER_REQUEST {
            return Err(RequestError::TooManyAttempts {
                max: MAX_ATTEMPTS_PER_REQUEST,
            });
        }
        if let Some(last) = self.attempts.last()
            && last.state.is_in_flight()
        {
            return Err(RequestError::AttemptInFlight {
                index: last.index(),
            });
        }
        let index = AttemptIndex(u8::try_from(self.attempts.len()).map_err(|_| {
            RequestError::TooManyAttempts {
                max: usize::from(u8::MAX),
            }
        })?);
        let attempt = make_attempt(&self.id, &self.payment, index, next)?;
        self.attempts.push(attempt);
        Ok(index)
    }

    /// 对某次尝试应用事件。成功返回它的新状态。
    ///
    /// 额外规则：已经发布了的尝试不能再被宣告为「没执行」（`upstream_refused`、`never_left_host`），
    /// 因为字节已经给了客户端。
    pub fn apply(
        &mut self,
        index: AttemptIndex,
        event: AttemptEvent,
    ) -> Result<&AttemptState, RequestError> {
        let published = self.published;
        let attempt = self
            .attempts
            .get_mut(usize::from(index.0))
            .ok_or(RequestError::NoSuchAttempt(index))?;
        if published == Some(index)
            && matches!(
                event.kind(),
                AttemptEventKind::UpstreamRefused | AttemptEventKind::NeverLeftHost
            )
        {
            return Err(RequestError::PublishedAttemptCannotBeRefused(index));
        }
        let next = transition(&attempt.state, &event)?;
        attempt.state = next;
        Ok(&attempt.state)
    }

    /// 记下「已向客户端输出第一个字节」。
    ///
    /// - 只能发布一次；
    /// - 只有最新的尝试可以发布（已被放弃的旧尝试迟到的回答不能给客户端，用户只付一次）；
    /// - 该尝试必须已经有上游的字节：`maybe_executed`（流式，usage 还没到）或已执行（非流式，整段响应已到）。
    ///
    /// 此后 [`Request::begin_next_attempt`] 一律拒绝。
    pub fn publish_first_byte(&mut self, index: AttemptIndex) -> Result<(), RequestError> {
        if self.published.is_some() {
            return Err(RequestError::AlreadyPublished);
        }
        let latest = self.latest();
        let attempt = self
            .attempts
            .get(usize::from(index.0))
            .ok_or(RequestError::NoSuchAttempt(index))?;
        if index != latest {
            return Err(RequestError::PublishNotLatestAttempt { index, latest });
        }
        match attempt.state.kind() {
            AttemptStateKind::MaybeExecuted
            | AttemptStateKind::ExecutedUsageKnown
            | AttemptStateKind::ExecutedUsageUnknown => {}
            state => return Err(RequestError::PublishInvalidState { index, state }),
        }
        self.published = Some(index);
        Ok(())
    }

    /// 这一刻的结算计划（纯函数 [`settle_attempts`] 的包装）。要求没有尝试还在进行中。
    pub fn plan_settlement(&self) -> Result<SettlementPlan, SettlementError> {
        settle_attempts(&self.id, &self.attempts, self.published)
    }

    /// 校验一份（从存储里读出来的）请求记录是否满足全部结构不变式。反序列化会调用它。
    ///
    /// - 至少一次、至多 [`MAX_ATTEMPTS_PER_REQUEST`] 次尝试，尝试 id 与请求 id、序号一致且连续；
    /// - 只有最新一次尝试可以处于 `admitted`/`sending`（之前的必须已经离开）；
    /// - 每次尝试的预留、价目单位与付款上下文一致，`max_tokens >= 1`；
    /// - 若已发布：发布的必须是最新一次，且它的状态必须是有字节的状态（不能是 `not_sent`、`admitted`、`sending`）。
    pub fn validate(&self) -> Result<(), RequestError> {
        if self.attempts.is_empty() {
            return Err(invalid("a request needs at least one attempt"));
        }
        if self.attempts.len() > MAX_ATTEMPTS_PER_REQUEST {
            return Err(invalid(format!(
                "{} attempts exceed the maximum {MAX_ATTEMPTS_PER_REQUEST}",
                self.attempts.len()
            )));
        }
        let last = self.attempts.len() - 1;
        for (i, attempt) in self.attempts.iter().enumerate() {
            if attempt.id.request() != &self.id || usize::from(attempt.id.index().0) != i {
                return Err(invalid(format!(
                    "attempt {} does not belong at position {i} of request {}",
                    attempt.id, self.id
                )));
            }
            if i != last && attempt.state.is_in_flight() {
                return Err(invalid(format!(
                    "attempt {} is still in flight but a later attempt exists",
                    attempt.id
                )));
            }
            if attempt.effective_max_tokens == 0 {
                return Err(invalid(format!("attempt {} has max_tokens 0", attempt.id)));
            }
            check_units(
                &self.payment,
                &attempt.reservation,
                &attempt.cost_rates,
                &attempt.price_rates,
            )
            .map_err(|e| invalid(format!("attempt {}: {e}", attempt.id)))?;
        }
        if let Some(published) = self.published {
            if usize::from(published.0) != last {
                return Err(invalid("only the latest attempt may have published"));
            }
            match self.attempts[last].state.kind() {
                AttemptStateKind::MaybeExecuted
                | AttemptStateKind::ExecutedUsageKnown
                | AttemptStateKind::ExecutedUsageUnknown
                | AttemptStateKind::Settled => {}
                other => {
                    return Err(invalid(format!(
                        "the published attempt cannot be in state {other:?}"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn check_units(
    payment: &PaymentContext,
    reservation: &Reservation,
    cost_rates: &RateCard<Cost>,
    price_rates: &RateCard<Price>,
) -> Result<(), RequestError> {
    let expect = payment.unit();
    if reservation.price.unit() != expect {
        return Err(RequestError::UnitMismatch {
            what: "reservation price",
            expected: expect.clone(),
            got: reservation.price.unit().clone(),
        });
    }
    if price_rates.unit() != expect {
        return Err(RequestError::UnitMismatch {
            what: "price rates",
            expected: expect.clone(),
            got: price_rates.unit().clone(),
        });
    }
    if reservation.cost.unit() != cost_rates.unit() {
        return Err(RequestError::UnitMismatch {
            what: "reservation cost vs cost rates",
            expected: cost_rates.unit().clone(),
            got: reservation.cost.unit().clone(),
        });
    }
    Ok(())
}

fn make_attempt(
    request: &RequestId,
    payment: &PaymentContext,
    index: AttemptIndex,
    candidate: AdmittedCandidate,
) -> Result<Attempt, RequestError> {
    let AdmittedCandidate {
        decision,
        price_version,
        cost_rates,
        price_rates,
    } = candidate;
    let admitted = decision
        .admitted()
        .ok_or_else(|| RequestError::NotAdmitted {
            reason: decision.reason_code(),
        })?
        .clone();
    if admitted.payer != *payment.payer() {
        return Err(RequestError::PayerMismatch {
            expected: payment.payer().clone(),
            got: admitted.payer,
        });
    }
    if admitted.effective_max_tokens == 0 {
        return Err(RequestError::ZeroMaxTokens);
    }
    check_units(payment, &admitted.reservation, &cost_rates, &price_rates)?;
    Ok(Attempt {
        id: AttemptId::new(request.clone(), index),
        endpoint: admitted.endpoint,
        price_version,
        cost_rates,
        price_rates,
        effective_max_tokens: admitted.effective_max_tokens,
        reservation: admitted.reservation,
        state: AttemptState::Admitted,
    })
}

// ---------------------------------------------------------------------------
// 结算：一组尝试 -> 谁付多少
// ---------------------------------------------------------------------------

/// 一次尝试在结算计划里的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptRole {
    /// 发布了的那次：用户为它付一次钱。
    Answering,
    /// 其它尝试：用户付 0，成本记给平台。
    Extra,
}

/// 平台侧上游成本的依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    /// 没有产生成本（证明没执行）。
    NotIncurred,
    /// 按实际用量。
    ActualUsage,
    /// 按预留成本挂账，**待核销**：等供应商账单或 usage 回来再调整。
    ReservedPendingReconciliation,
}

/// 用户价的依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeBasis {
    ActualUsage,
    /// usage 未知，按预留价。
    ReservedAmount,
}

/// 向用户收的钱：**只有一笔**，来自发布了的那次尝试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserCharge {
    pub attempt: AttemptId,
    pub charged: Price,
    pub basis: ChargeBasis,
    pub reserved: Price,
    /// `max(0, charged - reserved)`：超出预留的部分，是事实，照常记账，怎么分见 [`split_overage`]。
    pub overage: Price,
}

/// 一次尝试的结算行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptLine {
    pub attempt: AttemptId,
    pub role: AttemptRole,
    pub certainty: Certainty,
    /// 这次尝试对应的上游成本（平台向供应商付的）。
    pub cost: Cost,
    pub cost_basis: CostBasis,
    /// 要从用户额度里释放的价预留：`Extra` 是整笔，`Answering` 是 `max(0, 预留 - 实际价)`。
    pub price_reservation_released: Price,
}

/// 一次请求的结算计划：谁付多少、释放什么、什么待核销。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementPlan {
    pub request: RequestId,
    /// 每次尝试一行，按尝试序号排列。
    pub lines: Vec<AttemptLine>,
    /// 用户只付一次；没有尝试发布过为 `None`（用户付 0）。
    pub user_charge: Option<UserCharge>,
}

impl SettlementPlan {
    /// 发布了的那次尝试的结算行。
    pub fn answering_line(&self) -> Option<&AttemptLine> {
        self.lines.iter().find(|l| l.role == AttemptRole::Answering)
    }

    /// 平台承担的额外尝试（`Extra` 且成本非零）。
    pub fn platform_absorbed(&self) -> impl Iterator<Item = &AttemptLine> {
        self.lines
            .iter()
            .filter(|l| l.role == AttemptRole::Extra && !l.cost.is_zero())
    }

    /// 平台承担的额外尝试成本，按币种汇总（不同币种不会被加在一起）。
    pub fn platform_absorbed_by_unit(&self) -> Result<BTreeMap<Unit, Cost>, MoneyError> {
        let mut totals: BTreeMap<Unit, Cost> = BTreeMap::new();
        for line in self.platform_absorbed() {
            let unit = line.cost.unit().clone();
            let next = match totals.get(&unit) {
                Some(current) => current.checked_add(&line.cost)?,
                None => line.cost.clone(),
            };
            totals.insert(unit, next);
        }
        Ok(totals)
    }

    /// 成本按预留额挂账、等待核销的尝试（含发布了的那次，若它的 usage 未知）。
    pub fn pending_reconciliation(&self) -> Vec<&AttemptId> {
        self.lines
            .iter()
            .filter(|l| l.cost_basis == CostBasis::ReservedPendingReconciliation)
            .map(|l| &l.attempt)
            .collect()
    }
}

/// 结算错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettlementError {
    #[error("attempt {attempt} is still in flight; cannot settle")]
    AttemptInFlight { attempt: AttemptId },
    #[error("attempt {attempt} is marked as published but was not executed")]
    PublishedAttemptNotExecuted { attempt: AttemptId },
    #[error("published index {0:?} does not exist")]
    NoSuchPublishedAttempt(AttemptIndex),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// 一次尝试的上游成本敞口，以及（若知道用量）实际的用户价。
struct Exposure {
    cost: Cost,
    cost_basis: CostBasis,
    actual_price: Option<Price>,
}

fn exposure(attempt: &Attempt) -> Result<Exposure, SettlementError> {
    let from_usage = |usage| -> Result<Exposure, SettlementError> {
        Ok(Exposure {
            cost: usage_amount(&attempt.cost_rates, usage, USAGE_ROUNDING)?,
            cost_basis: CostBasis::ActualUsage,
            actual_price: Some(usage_amount(&attempt.price_rates, usage, USAGE_ROUNDING)?),
        })
    };
    let reserved = || Exposure {
        cost: attempt.reservation.cost.clone(),
        cost_basis: CostBasis::ReservedPendingReconciliation,
        actual_price: None,
    };
    match &attempt.state {
        AttemptState::NotSent => Ok(Exposure {
            cost: Cost::zero(attempt.cost_rates.unit().clone()),
            cost_basis: CostBasis::NotIncurred,
            actual_price: None,
        }),
        AttemptState::MaybeExecuted
        | AttemptState::ExecutedUsageUnknown
        | AttemptState::Settled {
            basis: SettleBasis::ReservedAmount,
        } => Ok(reserved()),
        AttemptState::ExecutedUsageKnown { usage }
        | AttemptState::Settled {
            basis: SettleBasis::ActualUsage(usage),
        } => from_usage(usage),
        AttemptState::Admitted | AttemptState::Sending => Err(SettlementError::AttemptInFlight {
            attempt: attempt.id.clone(),
        }),
    }
}

/// **一组尝试 → 谁付多少**（纯函数）。
///
/// - `published`：向客户端输出过第一个字节的那次尝试；`None` 表示没有任何尝试给出过回答。
/// - 前置条件：没有尝试还在 `admitted`/`sending`，否则 [`SettlementError::AttemptInFlight`]；
///   `published` 指向的尝试必须存在、且不能是 `not_sent`。
///
/// 规则见模块文档的表。输出的每一行金额都是类型化的 [`Cost`] / [`Price`]，
/// 币种不同的成本不会被加在一起。
pub fn settle_attempts(
    request: &RequestId,
    attempts: &[Attempt],
    published: Option<AttemptIndex>,
) -> Result<SettlementPlan, SettlementError> {
    if let Some(index) = published
        && attempts.get(usize::from(index.0)).is_none()
    {
        return Err(SettlementError::NoSuchPublishedAttempt(index));
    }
    let mut lines = Vec::with_capacity(attempts.len());
    let mut user_charge = None;
    for attempt in attempts {
        let exposure = exposure(attempt)?;
        let answering = published == Some(attempt.index());
        let reserved_price = attempt.reservation.price.clone();
        let released = if answering {
            if attempt.state.kind() == AttemptStateKind::NotSent {
                return Err(SettlementError::PublishedAttemptNotExecuted {
                    attempt: attempt.id.clone(),
                });
            }
            let (charged, basis) = match &exposure.actual_price {
                Some(price) => (price.clone(), ChargeBasis::ActualUsage),
                None => (reserved_price.clone(), ChargeBasis::ReservedAmount),
            };
            let overage = charged.saturating_sub(&reserved_price)?;
            let released = reserved_price.saturating_sub(&charged)?;
            user_charge = Some(UserCharge {
                attempt: attempt.id.clone(),
                charged,
                basis,
                reserved: reserved_price,
                overage,
            });
            released
        } else {
            reserved_price
        };
        lines.push(AttemptLine {
            attempt: attempt.id.clone(),
            role: if answering {
                AttemptRole::Answering
            } else {
                AttemptRole::Extra
            },
            certainty: attempt.certainty(),
            cost: exposure.cost,
            cost_basis: exposure.cost_basis,
            price_reservation_released: released,
        });
    }
    Ok(SettlementPlan {
        request: request.clone(),
        lines,
        user_charge,
    })
}

/// 超出预留的部分（`overage`）怎么分（设计稿 §5）：
///
/// > overage 照常记账，只是事实。超出预留的部分，在担保人授信范围内的由担保人承担，超出授信的记入
/// > **平台损失账户**，告警并冻结这张凭证。预留过期后迟到的结算，同样按这条规则走。
///
/// `absorbable_within_credit`：付款账户链在担保人授信范围内还能再吸收多少（账本算：剩余额度加授信，
/// 已扣掉本次预留）。迟到的结算同样调用这个函数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverageSplit {
    /// 担保人承担的部分。
    pub sponsor: Price,
    /// 记入平台损失账户的部分。
    pub platform_loss: Price,
    /// 是否要告警并冻结这张凭证：有任何一点记入平台损失就要。
    pub freeze_credential: bool,
}

pub fn split_overage(
    overage: &Price,
    absorbable_within_credit: &Price,
) -> Result<OverageSplit, MoneyError> {
    overage
        .money()
        .require_same_unit(absorbable_within_credit.money())?;
    let sponsor = if overage.try_cmp(absorbable_within_credit)?.is_le() {
        overage.clone()
    } else {
        absorbable_within_credit.clone()
    };
    let platform_loss = overage.checked_sub(&sponsor)?;
    Ok(OverageSplit {
        freeze_credential: !platform_loss.is_zero(),
        sponsor,
        platform_loss,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::admission::{Admitted, DegradeReason};
    use crate::pricing::Usage;
    use crate::test_support::{aid, sample_payment_context};
    use proptest::prelude::*;

    fn rid(s: &str) -> RequestId {
        RequestId::new(s).unwrap()
    }
    fn thb(micros: i64) -> Price {
        Price::from_micros(Unit::Thb, micros).unwrap()
    }
    fn usd(micros: i64) -> Cost {
        Cost::from_micros(Unit::Usd, micros).unwrap()
    }
    fn price_rates() -> RateCard<Price> {
        // 10 / 40 泰铢每百万 token
        RateCard::new(thb(10_000_000), thb(40_000_000), None).unwrap()
    }
    fn cost_rates() -> RateCard<Cost> {
        RateCard::new(usd(280_000), usd(420_000), None).unwrap()
    }
    /// 1000 入 200 出：价 = 10_000 + 8_000 = 18_000 微泰铢；成本 = 280 + 84 = 364 微美元。
    fn usage() -> Usage {
        Usage::new(1_000, 200, 0).unwrap()
    }
    const ACTUAL_PRICE: i64 = 18_000;
    const ACTUAL_COST: i64 = 364;

    fn admitted_for(model: &str, reserve_price: i64, reserve_cost: i64) -> Admitted {
        Admitted {
            endpoint: EndpointKey::new("p", model, "e").unwrap(),
            payer: aid("s1"),
            effective_max_tokens: 400,
            reservation: Reservation {
                price: thb(reserve_price),
                cost: usd(reserve_cost),
            },
        }
    }

    fn candidate_with(decision: AdmissionDecision) -> AdmittedCandidate {
        AdmittedCandidate {
            decision,
            price_version: PriceVersion::new("pv-1").unwrap(),
            cost_rates: cost_rates(),
            price_rates: price_rates(),
        }
    }

    /// 预留价 20_000 微泰铢，预留成本 400 微美元。
    fn candidate(model: &str) -> AdmittedCandidate {
        candidate_with(AdmissionDecision::Allow {
            admitted: admitted_for(model, 20_000, 400),
        })
    }

    fn new_request() -> Request {
        Request::admit(rid("req-1"), sample_payment_context(), candidate("m0")).unwrap()
    }

    fn ix(i: u8) -> AttemptIndex {
        AttemptIndex(i)
    }

    /// 让某次尝试走完：dispatch -> write -> 事件序列。
    fn drive(req: &mut Request, i: u8, events: &[AttemptEvent]) {
        for e in events {
            req.apply(ix(i), e.clone()).unwrap();
        }
    }

    fn to_maybe_executed(req: &mut Request, i: u8) {
        drive(
            req,
            i,
            &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        );
    }

    fn line(plan: &SettlementPlan, i: u8) -> &AttemptLine {
        &plan.lines[usize::from(i)]
    }

    // ---------- 准入（切换前重新准入） ----------

    #[test]
    fn admit_creates_the_first_attempt_in_admitted_state() {
        let req = new_request();
        assert_eq!(req.attempts().len(), 1);
        let a = &req.attempts()[0];
        assert_eq!(a.id().to_string(), "req-1#0");
        assert_eq!(a.state(), &AttemptState::Admitted);
        assert_eq!(a.endpoint(), &EndpointKey::new("p", "m0", "e").unwrap());
        assert_eq!(a.effective_max_tokens(), 400);
        assert_eq!(a.price_version().as_str(), "pv-1");
        assert_eq!(a.reservation().price.micros(), 20_000);
        assert_eq!(a.certainty(), Certainty::NotExecuted);
        assert_eq!(req.published(), None);
        assert_eq!(req.latest(), ix(0));
        assert_eq!(req.payment().payer(), &aid("s1"));
        assert!(req.validate().is_ok());
        assert_eq!(req.id().as_str(), "req-1");
        assert!(req.attempt(ix(0)).is_some() && req.attempt(ix(1)).is_none());
    }

    #[test]
    fn a_rejected_decision_can_neither_start_nor_switch() {
        let reject = AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap();
        assert_eq!(
            Request::admit(
                rid("r"),
                sample_payment_context(),
                candidate_with(reject.clone())
            )
            .unwrap_err(),
            RequestError::NotAdmitted {
                reason: Some(ReasonCode::BudgetExhausted)
            }
        );
        let mut req = new_request();
        drive(&mut req, 0, &[AttemptEvent::NeverLeftHost]);
        assert_eq!(
            req.begin_next_attempt(candidate_with(reject)).unwrap_err(),
            RequestError::NotAdmitted {
                reason: Some(ReasonCode::BudgetExhausted)
            }
        );
        assert_eq!(req.attempts().len(), 1, "被拒绝的准入不能留下半个尝试");
    }

    #[test]
    fn every_admitting_decision_kind_can_start_an_attempt() {
        for decision in [
            AdmissionDecision::Allow {
                admitted: admitted_for("m", 1, 1),
            },
            AdmissionDecision::Degrade {
                admitted: admitted_for("m", 1, 1),
                reason: DegradeReason::Budget,
            },
            AdmissionDecision::Degrade {
                admitted: admitted_for("m", 1, 1),
                reason: DegradeReason::UpstreamFailover,
            },
            AdmissionDecision::Grace {
                admitted: admitted_for("m", 1, 1),
            },
        ] {
            assert!(
                Request::admit(rid("r"), sample_payment_context(), candidate_with(decision))
                    .is_ok()
            );
        }
    }

    #[test]
    fn admission_must_be_for_this_requests_payer_and_unit() {
        let mut other_payer = admitted_for("m", 1, 1);
        other_payer.payer = aid("s2");
        assert_eq!(
            Request::admit(
                rid("r"),
                sample_payment_context(),
                candidate_with(AdmissionDecision::Allow {
                    admitted: other_payer
                })
            )
            .unwrap_err(),
            RequestError::PayerMismatch {
                expected: aid("s1"),
                got: aid("s2")
            }
        );
        // 预留价的单位必须是付款账户的单位（泰铢）
        let mut usd_reservation = admitted_for("m", 1, 1);
        usd_reservation.reservation.price = Price::from_micros(Unit::Usd, 1).unwrap();
        assert!(matches!(
            Request::admit(
                rid("r"),
                sample_payment_context(),
                candidate_with(AdmissionDecision::Allow {
                    admitted: usd_reservation
                })
            )
            .unwrap_err(),
            RequestError::UnitMismatch {
                what: "reservation price",
                ..
            }
        ));
        // 价目表单位不对
        let mut bad_rates = candidate("m");
        bad_rates.price_rates = RateCard::new(
            Price::from_micros(Unit::Usd, 1).unwrap(),
            Price::from_micros(Unit::Usd, 1).unwrap(),
            None,
        )
        .unwrap();
        assert!(matches!(
            Request::admit(rid("r"), sample_payment_context(), bad_rates).unwrap_err(),
            RequestError::UnitMismatch {
                what: "price rates",
                ..
            }
        ));
        // 预留成本与成本价目单位不一致
        let mut bad_cost = admitted_for("m", 1, 1);
        bad_cost.reservation.cost = Cost::from_micros(Unit::Thb, 1).unwrap();
        assert!(matches!(
            Request::admit(
                rid("r"),
                sample_payment_context(),
                candidate_with(AdmissionDecision::Allow { admitted: bad_cost })
            )
            .unwrap_err(),
            RequestError::UnitMismatch {
                what: "reservation cost vs cost rates",
                ..
            }
        ));
        // max_tokens 为 0
        let mut zero = admitted_for("m", 1, 1);
        zero.effective_max_tokens = 0;
        assert_eq!(
            Request::admit(
                rid("r"),
                sample_payment_context(),
                candidate_with(AdmissionDecision::Allow { admitted: zero })
            )
            .unwrap_err(),
            RequestError::ZeroMaxTokens
        );
    }

    // ---------- 切换 ----------

    #[test]
    fn switching_after_not_sent_is_safe_and_the_user_pays_only_the_answering_attempt() {
        let mut req = new_request();
        drive(
            &mut req,
            0,
            &[AttemptEvent::Dispatch, AttemptEvent::NeverLeftHost],
        );
        let i = req.begin_next_attempt(candidate("m1")).unwrap();
        assert_eq!(i, ix(1));
        assert_eq!(req.latest(), ix(1));
        drive(
            &mut req,
            1,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported { usage: usage() },
            ],
        );
        req.publish_first_byte(ix(1)).unwrap();
        drive(&mut req, 1, &[AttemptEvent::Settle]);
        let plan = req.plan_settlement().unwrap();
        let charge = plan.user_charge.as_ref().unwrap();
        assert_eq!(charge.attempt.to_string(), "req-1#1");
        assert_eq!(charge.charged.micros(), ACTUAL_PRICE);
        assert_eq!(charge.basis, ChargeBasis::ActualUsage);
        assert_eq!(charge.overage.micros(), 0);
        // 第 0 次：没发出去，没有成本，预留整笔释放
        let l0 = line(&plan, 0);
        assert_eq!(l0.role, AttemptRole::Extra);
        assert_eq!(l0.certainty, Certainty::NotExecuted);
        assert!(l0.cost.is_zero());
        assert_eq!(l0.cost_basis, CostBasis::NotIncurred);
        assert_eq!(l0.price_reservation_released.micros(), 20_000);
        // 第 1 次：实际成本记在自己行上，预留的多余部分释放
        let l1 = line(&plan, 1);
        assert_eq!(l1.role, AttemptRole::Answering);
        assert_eq!(l1.cost.micros(), ACTUAL_COST);
        assert_eq!(
            l1.price_reservation_released.micros(),
            20_000 - ACTUAL_PRICE
        );
        assert!(plan.platform_absorbed().next().is_none());
        assert!(plan.pending_reconciliation().is_empty());
    }

    #[test]
    fn switching_after_maybe_executed_keeps_its_cost_pending_and_charges_the_platform() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0); // 发出去了，超时，没有任何确认
        assert_eq!(req.attempt(ix(0)).unwrap().certainty(), Certainty::Maybe);
        // 首字节之前切换：允许。上一次保持 maybe_executed，不被释放
        req.begin_next_attempt(candidate("m1")).unwrap();
        assert_eq!(
            req.attempt(ix(0)).unwrap().state(),
            &AttemptState::MaybeExecuted
        );
        drive(
            &mut req,
            1,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported { usage: usage() },
            ],
        );
        req.publish_first_byte(ix(1)).unwrap();
        drive(&mut req, 1, &[AttemptEvent::Settle]);
        let plan = req.plan_settlement().unwrap();
        // 用户只付第 1 次
        assert_eq!(
            plan.user_charge.as_ref().unwrap().charged.micros(),
            ACTUAL_PRICE
        );
        // 第 0 次：成本按预留额（400）挂账待核销，没有被当成 0
        let l0 = line(&plan, 0);
        assert_eq!(l0.role, AttemptRole::Extra);
        assert_eq!(l0.certainty, Certainty::Maybe);
        assert_eq!(l0.cost.micros(), 400);
        assert_eq!(l0.cost_basis, CostBasis::ReservedPendingReconciliation);
        assert_eq!(
            plan.pending_reconciliation(),
            vec![&req.attempts()[0].id().clone()]
        );
        // 记给平台
        let absorbed = plan.platform_absorbed_by_unit().unwrap();
        assert_eq!(absorbed.len(), 1);
        assert_eq!(absorbed[&Unit::Usd].micros(), 400);
        // 用户额度里这次尝试的价预留释放（用户不为它付钱）
        assert_eq!(l0.price_reservation_released.micros(), 20_000);
    }

    #[test]
    fn an_executed_but_unpublished_attempt_is_absorbed_at_actual_cost() {
        // 非流式：第 0 次执行成功，但输出没通过安全检查所以没发布；换第 1 次
        let mut req = new_request();
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported { usage: usage() },
            ],
        );
        req.begin_next_attempt(candidate("m1")).unwrap();
        drive(
            &mut req,
            1,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported {
                    usage: Usage::new(500, 100, 0).unwrap(),
                },
            ],
        );
        req.publish_first_byte(ix(1)).unwrap();
        let plan = req.plan_settlement().unwrap();
        assert_eq!(line(&plan, 0).cost.micros(), ACTUAL_COST);
        assert_eq!(line(&plan, 0).cost_basis, CostBasis::ActualUsage);
        assert_eq!(
            plan.platform_absorbed_by_unit().unwrap()[&Unit::Usd].micros(),
            ACTUAL_COST
        );
        // 用户只为第 1 次付：500 入 100 出 = 5_000 + 4_000
        assert_eq!(plan.user_charge.as_ref().unwrap().charged.micros(), 9_000);
    }

    #[test]
    fn nothing_published_means_the_user_pays_nothing() {
        let mut req = new_request();
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported { usage: usage() },
            ],
        );
        let plan = req.plan_settlement().unwrap();
        assert!(plan.user_charge.is_none());
        assert!(plan.answering_line().is_none());
        assert_eq!(line(&plan, 0).role, AttemptRole::Extra);
        assert_eq!(line(&plan, 0).price_reservation_released.micros(), 20_000);
        assert_eq!(
            plan.platform_absorbed_by_unit().unwrap()[&Unit::Usd].micros(),
            ACTUAL_COST
        );
    }

    // ---------- 发布边界 ----------

    #[test]
    fn after_the_first_byte_no_switch_or_retry_is_possible() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap(); // 流式：首个 chunk 已经给了客户端
        assert_eq!(req.published(), Some(ix(0)));
        assert_eq!(
            req.begin_next_attempt(candidate("m1")).unwrap_err(),
            RequestError::PublishBoundary
        );
        assert_eq!(req.attempts().len(), 1);
        // 中途失败，不论走哪条后续路径，发布边界始终成立
        drive(&mut req, 0, &[AttemptEvent::ExecutionObservedUsageUnknown]);
        assert_eq!(
            req.begin_next_attempt(candidate("m1")).unwrap_err(),
            RequestError::PublishBoundary
        );
        drive(&mut req, 0, &[AttemptEvent::Settle]);
        assert_eq!(
            req.begin_next_attempt(candidate("m1")).unwrap_err(),
            RequestError::PublishBoundary
        );
        // 就算 TooManyAttempts / AttemptInFlight 也排在发布边界之后：边界优先
        assert_eq!(req.attempts().len(), 1);
    }

    #[test]
    fn publish_boundary_takes_precedence_over_every_other_switch_check() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap();
        // 用一个本来就非法的候选去切：仍然先报发布边界
        let reject =
            candidate_with(AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap());
        assert_eq!(
            req.begin_next_attempt(reject).unwrap_err(),
            RequestError::PublishBoundary
        );
    }

    #[test]
    fn publishing_has_strict_preconditions() {
        // 状态不对：admitted / sending / not_sent / settled 都不能发布
        let mut req = new_request();
        assert!(matches!(
            req.publish_first_byte(ix(0)).unwrap_err(),
            RequestError::PublishInvalidState {
                state: AttemptStateKind::Admitted,
                ..
            }
        ));
        drive(&mut req, 0, &[AttemptEvent::Dispatch]);
        assert!(matches!(
            req.publish_first_byte(ix(0)).unwrap_err(),
            RequestError::PublishInvalidState {
                state: AttemptStateKind::Sending,
                ..
            }
        ));
        drive(&mut req, 0, &[AttemptEvent::NeverLeftHost]);
        assert!(matches!(
            req.publish_first_byte(ix(0)).unwrap_err(),
            RequestError::PublishInvalidState {
                state: AttemptStateKind::NotSent,
                ..
            }
        ));
        // 不存在的尝试
        assert_eq!(
            req.publish_first_byte(ix(5)).unwrap_err(),
            RequestError::NoSuchAttempt(ix(5))
        );
        // 已放弃的旧尝试（不是最新）不能发布：迟到的回答不能给客户端
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.begin_next_attempt(candidate("m1")).unwrap();
        assert_eq!(
            req.publish_first_byte(ix(0)).unwrap_err(),
            RequestError::PublishNotLatestAttempt {
                index: ix(0),
                latest: ix(1)
            }
        );
        // 只能发布一次
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap();
        assert_eq!(
            req.publish_first_byte(ix(0)).unwrap_err(),
            RequestError::AlreadyPublished
        );
    }

    #[test]
    fn a_published_attempt_cannot_be_declared_not_executed() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap();
        assert_eq!(
            req.apply(ix(0), AttemptEvent::UpstreamRefused { http_status: 429 })
                .unwrap_err(),
            RequestError::PublishedAttemptCannotBeRefused(ix(0))
        );
        assert_eq!(
            req.attempt(ix(0)).unwrap().state(),
            &AttemptState::MaybeExecuted
        );
        // 但对没发布的旧尝试，迟到的 4xx（对账）仍然可以落地
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.begin_next_attempt(candidate("m1")).unwrap();
        assert!(
            req.apply(ix(0), AttemptEvent::UpstreamRefused { http_status: 429 })
                .is_ok()
        );
    }

    // ---------- 其它规则 ----------

    #[test]
    fn the_next_attempt_waits_for_the_previous_one_to_leave_admitted_and_sending() {
        let mut req = new_request();
        assert_eq!(
            req.begin_next_attempt(candidate("m1")).unwrap_err(),
            RequestError::AttemptInFlight { index: ix(0) }
        );
        drive(&mut req, 0, &[AttemptEvent::Dispatch]);
        assert_eq!(
            req.begin_next_attempt(candidate("m1")).unwrap_err(),
            RequestError::AttemptInFlight { index: ix(0) }
        );
        drive(&mut req, 0, &[AttemptEvent::WireWriteStarted]);
        assert!(req.begin_next_attempt(candidate("m1")).is_ok());
    }

    #[test]
    fn attempts_are_capped() {
        let mut req = new_request();
        for i in 1..MAX_ATTEMPTS_PER_REQUEST {
            drive(
                &mut req,
                u8::try_from(i - 1).unwrap(),
                &[AttemptEvent::NeverLeftHost],
            );
            req.begin_next_attempt(candidate("m")).unwrap();
        }
        drive(
            &mut req,
            u8::try_from(MAX_ATTEMPTS_PER_REQUEST - 1).unwrap(),
            &[AttemptEvent::NeverLeftHost],
        );
        assert_eq!(
            req.begin_next_attempt(candidate("m")).unwrap_err(),
            RequestError::TooManyAttempts {
                max: MAX_ATTEMPTS_PER_REQUEST
            }
        );
        assert_eq!(req.attempts().len(), MAX_ATTEMPTS_PER_REQUEST);
        assert!(req.validate().is_ok());
    }

    #[test]
    fn apply_rejects_unknown_attempts_and_illegal_events() {
        let mut req = new_request();
        assert_eq!(
            req.apply(ix(3), AttemptEvent::Dispatch).unwrap_err(),
            RequestError::NoSuchAttempt(ix(3))
        );
        assert!(matches!(
            req.apply(ix(0), AttemptEvent::Settle).unwrap_err(),
            RequestError::Transition(TransitionError::IllegalTransition { .. })
        ));
        // 非法转移不改变状态
        assert_eq!(req.attempt(ix(0)).unwrap().state(), &AttemptState::Admitted);
        // 5xx 不是「没执行」的证据：状态保持 maybe_executed
        to_maybe_executed(&mut req, 0);
        assert!(matches!(
            req.apply(ix(0), AttemptEvent::UpstreamRefused { http_status: 503 })
                .unwrap_err(),
            RequestError::Transition(TransitionError::InvalidEvidence { .. })
        ));
        assert_eq!(
            req.attempt(ix(0)).unwrap().state(),
            &AttemptState::MaybeExecuted
        );
    }

    #[test]
    fn settlement_refuses_while_an_attempt_is_in_flight() {
        let mut req = new_request();
        assert!(matches!(
            req.plan_settlement().unwrap_err(),
            SettlementError::AttemptInFlight { .. }
        ));
        drive(&mut req, 0, &[AttemptEvent::Dispatch]);
        assert!(req.plan_settlement().is_err());
        drive(&mut req, 0, &[AttemptEvent::NeverLeftHost]);
        let plan = req.plan_settlement().unwrap();
        assert!(plan.user_charge.is_none());
        assert_eq!(plan.lines.len(), 1);
    }

    // ---------- 流式、usage 未知 ----------

    #[test]
    fn a_stream_that_breaks_after_the_first_byte_settles_at_the_reservation() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap();
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::ExecutionObservedUsageUnknown,
                AttemptEvent::Settle,
            ],
        );
        let plan = req.plan_settlement().unwrap();
        let charge = plan.user_charge.as_ref().unwrap();
        assert_eq!(charge.charged.micros(), 20_000);
        assert_eq!(charge.basis, ChargeBasis::ReservedAmount);
        assert_eq!(charge.overage.micros(), 0);
        let l = line(&plan, 0);
        assert_eq!(
            l.price_reservation_released.micros(),
            0,
            "按预留结算，预留全部转为扣费"
        );
        assert_eq!(l.cost.micros(), 400);
        assert_eq!(l.cost_basis, CostBasis::ReservedPendingReconciliation);
        assert_eq!(plan.pending_reconciliation().len(), 1);
    }

    #[test]
    fn a_crash_leaves_a_published_maybe_executed_attempt_settled_at_the_reservation() {
        // 恢复场景：发布了，进程崩了，尝试仍停在 maybe_executed
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap();
        let plan = req.plan_settlement().unwrap();
        assert_eq!(
            plan.user_charge.as_ref().unwrap().basis,
            ChargeBasis::ReservedAmount
        );
        assert_eq!(
            line(&plan, 0).cost_basis,
            CostBasis::ReservedPendingReconciliation
        );
    }

    #[test]
    fn reconciliation_can_upgrade_unknown_usage_before_settlement() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.publish_first_byte(ix(0)).unwrap();
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::ExecutionObservedUsageUnknown,
                AttemptEvent::UsageReported { usage: usage() },
                AttemptEvent::Settle,
            ],
        );
        let plan = req.plan_settlement().unwrap();
        assert_eq!(
            plan.user_charge.as_ref().unwrap().basis,
            ChargeBasis::ActualUsage
        );
        assert_eq!(line(&plan, 0).cost_basis, CostBasis::ActualUsage);
    }

    #[test]
    fn a_bill_that_proves_non_execution_is_reported_as_zero_usage() {
        // 对账：账单显示 0 token。成本和价都是 0，预留全部释放
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.begin_next_attempt(candidate("m1")).unwrap();
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::UsageReported { usage: Usage::ZERO },
                AttemptEvent::Settle,
            ],
        );
        drive(
            &mut req,
            1,
            &[AttemptEvent::Dispatch, AttemptEvent::NeverLeftHost],
        );
        let plan = req.plan_settlement().unwrap();
        assert!(plan.user_charge.is_none());
        assert!(line(&plan, 0).cost.is_zero());
        assert_eq!(line(&plan, 0).cost_basis, CostBasis::ActualUsage);
        assert!(plan.platform_absorbed().next().is_none());
    }

    // ---------- overage ----------

    #[test]
    fn overage_and_unused_reservation_are_reported_on_the_user_charge() {
        let mut req = new_request(); // 预留 20_000
        // 实际用量比预留大：5000 入 1000 出 = 50_000 + 40_000 = 90_000
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported {
                    usage: Usage::new(5_000, 1_000, 0).unwrap(),
                },
            ],
        );
        req.publish_first_byte(ix(0)).unwrap();
        let plan = req.plan_settlement().unwrap();
        let charge = plan.user_charge.as_ref().unwrap();
        assert_eq!(charge.charged.micros(), 90_000);
        assert_eq!(charge.reserved.micros(), 20_000);
        assert_eq!(charge.overage.micros(), 70_000);
        assert_eq!(line(&plan, 0).price_reservation_released.micros(), 0);
        // 刚好等于预留：没有 overage，也没有释放
        let mut req = Request::admit(
            rid("req-2"),
            sample_payment_context(),
            candidate_with(AdmissionDecision::Allow {
                admitted: admitted_for("m", ACTUAL_PRICE, 400),
            }),
        )
        .unwrap();
        drive(
            &mut req,
            0,
            &[
                AttemptEvent::Dispatch,
                AttemptEvent::WireWriteStarted,
                AttemptEvent::UsageReported { usage: usage() },
            ],
        );
        req.publish_first_byte(ix(0)).unwrap();
        let plan = req.plan_settlement().unwrap();
        assert_eq!(plan.user_charge.as_ref().unwrap().overage.micros(), 0);
        assert_eq!(line(&plan, 0).price_reservation_released.micros(), 0);
    }

    #[test]
    fn split_overage_boundaries() {
        // (overage, 授信内还能吸收, 期望担保人, 期望平台损失, 是否冻结)
        let cases = [
            (0, 0, 0, 0, false),
            (0, 100, 0, 0, false),
            (50, 100, 50, 0, false),
            (100, 100, 100, 0, false), // 刚好等于：全部由担保人承担，不冻结
            (101, 100, 100, 1, true),  // 多 1 微单位：1 记平台损失，冻结
            (500, 0, 0, 500, true),    // 没有授信：全部记平台损失
            (i64::MAX, 0, 0, i64::MAX, true),
            (i64::MAX, i64::MAX, i64::MAX, 0, false),
        ];
        for (overage, absorbable, sponsor, loss, freeze) in cases {
            let split = split_overage(&thb(overage), &thb(absorbable)).unwrap();
            assert_eq!(split.sponsor.micros(), sponsor, "{overage}/{absorbable}");
            assert_eq!(split.platform_loss.micros(), loss, "{overage}/{absorbable}");
            assert_eq!(split.freeze_credential, freeze, "{overage}/{absorbable}");
            assert_eq!(
                split.sponsor.micros() + split.platform_loss.micros(),
                overage
            );
        }
        assert!(split_overage(&thb(1), &Price::from_micros(Unit::Usd, 1).unwrap()).is_err());
    }

    // ---------- 记录校验（从存储读回来时） ----------

    fn json_of(req: &Request) -> serde_json::Value {
        serde_json::to_value(req).unwrap()
    }

    #[test]
    fn a_request_round_trips_through_json_and_revalidates_on_load() {
        let mut req = new_request();
        to_maybe_executed(&mut req, 0);
        req.begin_next_attempt(candidate("m1")).unwrap();
        drive(
            &mut req,
            1,
            &[AttemptEvent::Dispatch, AttemptEvent::WireWriteStarted],
        );
        req.publish_first_byte(ix(1)).unwrap();
        let json = json_of(&req);
        let back: Request = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, req);
        // 篡改：发布的不是最新一次
        let mut bad = json.clone();
        bad["published"] = serde_json::json!(0);
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // 篡改：发布指向 not_sent
        let mut bad = json.clone();
        bad["attempts"][1]["state"] = serde_json::json!({"state": "not_sent"});
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // 篡改：旧的尝试还在 sending，但已经有更新的尝试
        let mut bad = json.clone();
        bad["attempts"][0]["state"] = serde_json::json!({"state": "sending"});
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // 篡改：尝试 id 与位置不符 / 属于别的请求
        let mut bad = json.clone();
        bad["attempts"][1]["id"] = serde_json::json!("req-1#5");
        assert!(serde_json::from_value::<Request>(bad).is_err());
        let mut bad = json.clone();
        bad["attempts"][1]["id"] = serde_json::json!("other#1");
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // 篡改：没有尝试
        let mut bad = json.clone();
        bad["attempts"] = serde_json::json!([]);
        assert!(serde_json::from_value::<Request>(bad).is_err());
        // 篡改：max_tokens 为 0；单位不一致；未知字段
        let mut bad = json.clone();
        bad["attempts"][0]["effective_max_tokens"] = serde_json::json!(0);
        assert!(serde_json::from_value::<Request>(bad).is_err());
        let mut bad = json.clone();
        bad["attempts"][0]["reservation"]["price"] =
            serde_json::json!({"unit": "USD", "micros": 1});
        assert!(serde_json::from_value::<Request>(bad).is_err());
        let mut bad = json;
        bad["payer"] = serde_json::json!("someone-else");
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }

    #[test]
    fn too_many_attempts_in_a_stored_record_is_rejected() {
        let req = new_request();
        let json = json_of(&req);
        let mut bad = json;
        let first = bad["attempts"][0].clone();
        let list: Vec<_> = (0..=MAX_ATTEMPTS_PER_REQUEST)
            .map(|i| {
                let mut a = first.clone();
                a["id"] = serde_json::json!(format!("req-1#{i}"));
                a["state"] = serde_json::json!({"state": "not_sent"});
                a
            })
            .collect();
        bad["attempts"] = serde_json::Value::Array(list);
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }

    // ---------- 模型对照的性质测试 ----------

    #[derive(Debug, Clone)]
    enum Op {
        BeginNext(bool),
        Apply(u8, u8),
        /// 对最新一次尝试应用「下一个合法事件」：让随机序列能稳定走到很深的状态
        /// （发布之后、多次切换之后），而不是大多数时候卡在非法操作上。
        Progress,
        Publish(u8),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            1 => Just(Op::BeginNext(false)),
            4 => Just(Op::BeginNext(true)),
            6 => (0u8..4, 0u8..7).prop_map(|(i, e)| Op::Apply(i, e)),
            8 => Just(Op::Progress),
            4 => (0u8..4).prop_map(Op::Publish),
        ]
    }

    fn event_from(code: u8) -> AttemptEvent {
        match code {
            0 => AttemptEvent::Dispatch,
            1 => AttemptEvent::NeverLeftHost,
            2 => AttemptEvent::WireWriteStarted,
            3 => AttemptEvent::UpstreamRefused { http_status: 429 },
            4 => AttemptEvent::UsageReported { usage: usage() },
            5 => AttemptEvent::ExecutionObservedUsageUnknown,
            _ => AttemptEvent::Settle,
        }
    }

    /// 参考模型：只记每次尝试的状态种类和发布位置，规则独立于实现重写一遍。
    #[derive(Default)]
    struct Model {
        states: Vec<AttemptStateKind>,
        published: Option<usize>,
    }

    impl Model {
        fn legal(from: AttemptStateKind, code: u8) -> Option<AttemptStateKind> {
            use AttemptStateKind as S;
            match (from, code) {
                (S::Admitted, 0) => Some(S::Sending),
                (S::Admitted | S::Sending, 1) => Some(S::NotSent),
                (S::Sending, 2) => Some(S::MaybeExecuted),
                (S::Sending | S::MaybeExecuted, 3) => Some(S::NotSent),
                (S::MaybeExecuted | S::ExecutedUsageUnknown, 4) => Some(S::ExecutedUsageKnown),
                (S::MaybeExecuted, 5) => Some(S::ExecutedUsageUnknown),
                (S::ExecutedUsageKnown | S::ExecutedUsageUnknown, 6) => Some(S::Settled),
                _ => None,
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn request_follows_the_reference_model_and_settlement_invariants_hold(
            ops in proptest::collection::vec(op_strategy(), 0..40)
        ) {
            let mut req = new_request();
            let mut model = Model { states: vec![AttemptStateKind::Admitted], published: None };
            for op in ops {
                // `Progress` 先翻译成对最新尝试的、模型认为合法的下一个事件
                let op = match op {
                    Op::Progress => {
                        let latest = model.states.len() - 1;
                        let code = (0u8..7)
                            .find(|c| Model::legal(model.states[latest], *c).is_some())
                            .unwrap_or(0);
                        Op::Apply(u8::try_from(latest).unwrap(), code)
                    }
                    other => other,
                };
                match op {
                    Op::Progress => unreachable!("translated above"),
                    Op::BeginNext(admit) => {
                        let cand = if admit {
                            candidate("m")
                        } else {
                            candidate_with(AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap())
                        };
                        let expect_ok = admit
                            && model.published.is_none()
                            && model.states.len() < MAX_ATTEMPTS_PER_REQUEST
                            && !matches!(model.states.last(), Some(AttemptStateKind::Admitted | AttemptStateKind::Sending));
                        let result = req.begin_next_attempt(cand);
                        prop_assert_eq!(result.is_ok(), expect_ok);
                        if expect_ok {
                            model.states.push(AttemptStateKind::Admitted);
                        }
                    }
                    Op::Apply(i, code) => {
                        let idx = usize::from(i);
                        let result = req.apply(ix(i), event_from(code));
                        let expected = model.states.get(idx).and_then(|s| Model::legal(*s, code));
                        let blocked_by_publish = model.published == Some(idx) && matches!(code, 1 | 3);
                        let expect_ok = expected.is_some() && !blocked_by_publish
                            // 5xx 这里不会出现：事件固定用 429
                            ;
                        prop_assert_eq!(result.is_ok(), expect_ok, "apply {} {}", i, code);
                        if expect_ok {
                            model.states[idx] = expected.unwrap();
                        }
                    }
                    Op::Publish(i) => {
                        let idx = usize::from(i);
                        let last = model.states.len() - 1;
                        let expect_ok = model.published.is_none()
                            && idx == last
                            && matches!(
                                model.states[last],
                                AttemptStateKind::MaybeExecuted
                                    | AttemptStateKind::ExecutedUsageKnown
                                    | AttemptStateKind::ExecutedUsageUnknown
                            );
                        let result = req.publish_first_byte(ix(i));
                        prop_assert_eq!(result.is_ok(), expect_ok);
                        if expect_ok {
                            model.published = Some(idx);
                        }
                    }
                }
                // 每一步之后：实际状态与模型一致，记录始终通过校验
                let actual: Vec<_> = req.attempts().iter().map(|a| a.state().kind()).collect();
                prop_assert_eq!(&actual, &model.states);
                prop_assert_eq!(req.published().map(|p| usize::from(p.0)), model.published);
                prop_assert!(req.validate().is_ok());
                // 发布之后尝试数不再增加
                if model.published.is_some() {
                    prop_assert!(req.begin_next_attempt(candidate("m")).is_err());
                }
            }
            // 没有进行中的尝试时，结算计划满足全部不变式
            if let Ok(plan) = req.plan_settlement() {
                prop_assert_eq!(plan.lines.len(), req.attempts().len());
                let answering = plan.lines.iter().filter(|l| l.role == AttemptRole::Answering).count();
                prop_assert_eq!(answering, usize::from(req.published().is_some()));
                prop_assert_eq!(plan.user_charge.is_some(), req.published().is_some());
                for (attempt, l) in req.attempts().iter().zip(&plan.lines) {
                    match attempt.state().certainty() {
                        // maybe_executed 的成本永远按预留额挂账、待核销，不会是 0、不会被释放
                        Certainty::Maybe => {
                            prop_assert_eq!(l.cost_basis, CostBasis::ReservedPendingReconciliation);
                            prop_assert_eq!(l.cost.micros(), attempt.reservation().cost.micros());
                            prop_assert!(plan.pending_reconciliation().contains(&attempt.id()));
                        }
                        Certainty::NotExecuted => {
                            prop_assert!(l.cost.is_zero());
                            prop_assert_eq!(l.cost_basis, CostBasis::NotIncurred);
                        }
                        Certainty::Executed => {}
                    }
                    if l.role == AttemptRole::Extra {
                        // 用户只付一次：其余尝试的价预留整笔释放
                        prop_assert_eq!(l.price_reservation_released.micros(), attempt.reservation().price.micros());
                    }
                }
            }
        }
    }
}
