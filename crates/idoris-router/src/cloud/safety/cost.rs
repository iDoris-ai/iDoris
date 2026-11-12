//! 安全检查的成本：计入准入和结算的**接口**（设计稿 §7：「安全检查的成本计入准入和结算」）。
//!
//! # 谁做什么
//!
//! 规则检查器没有成本。LLM 检查器每次检查都要调用一个上游模型，花真钱：输入检查一次、输出检查一次，
//! 每次可能还要分块多次调用。这笔钱必须像回答本身一样：**事前占额度**（准入）、**事后按实际用量结算**。
//!
//! 准入和账本是 S3 的事（S3 正在同时开发）。这里只定义 S6 需要的接口形状，并给一个简单实现：
//!
//! | 阶段 | 调用 | 说明 |
//! |---|---|---|
//! | 检查之前 | [`ExtraCostMeter::reserve`] | 按检查器给出的**上界**占额度。被拒（付款账户链放不下）就不检查、不调用上游：返回准入类的拒绝（`budget_exhausted` 等） |
//! | 检查之后 | [`ExtraCostMeter::settle`] | 按实际用量结算；用量未知按预留额结算；检查根本没跑就释放。**每个 ticket 恰好结算一次**（[`ExtraCostGuard`] 在被丢弃时兜底） |
//!
//! 谁承担这笔钱（用户还是平台）是 S3 的决定：设计稿 §6 把「额外尝试的成本」记给平台，§7 只说「计入准入和结算」。
//! 接口里每笔花费同时给出上游成本（`Cost`，供应商币种）和用量，S3 想怎么记都够用。
//!
//! # 简单实现：`SimpleExtraCost`（TODO(S3)）
//!
//! 一律放行，只把结算记在**内存里**（有上限的环形缓冲，测试和排障能看到）。**它不进账本**：进程重启就丢，
//! 所以在 S3 的实现接上之前，LLM 检查器的成本**没有**真正计入准入和结算。`TODO(S3)`：换成读写账本的实现，
//! 见本模块末尾的「S3 需要提供什么」。
//!
//! # S3 需要提供什么
//!
//! 1. 一个能按 `(request_id, 阶段)` 幂等预留的入口：在付款上下文的**同一条祖先链**上占 `max_cost` 折算成付款单位后的额度
//!    （上游成本 `Cost` 的币种和付款账户的单位可能不同，需要 S3 的汇率/价目换算），放不下返回一个准入类的拒绝原因码；
//! 2. 与之对应的结算入口：`Spent`（带实际 `Cost` 与用量）、`Unknown`（按预留额挂账待核销）、`NotRun`（释放）；
//! 3. 分录要有能区分「安全检查」的 `kind`（现在 `compute_postings.kind` 只有 `user_price` / `platform_cost` / `hold` / `release`，
//!    输入检查发生在任何尝试之前，没有 `attempt_id` 可挂），并保证**用户只付一次**不受影响；
//! 4. 对账：`Unknown` 的安全检查成本和 `maybe_executed` 的尝试一样，保留待核销。

use std::collections::VecDeque;
use std::sync::Mutex;

use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{EndpointKey, RequestId, TimestampMs};
use idoris_compute_contracts::pricing::{Cost, Usage};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::safety::SafetyStage;

/// 检查器事前给出的成本上界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraCostEstimate {
    /// 这次检查会调用的上游端点（数据处理方，也在端点许可表里）。
    pub endpoint: EndpointKey,
    /// 这次检查最多花多少（上游成本，供应商币种）。
    pub max_cost: Cost,
}

/// 一次实际的花费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendItem {
    pub endpoint: EndpointKey,
    /// 实际成本；用量未知时是这次调用的预留上界（`estimated = true`）。
    pub cost: Cost,
    /// 上游报告的用量（通过了结构检查的）；未知为 `None`。
    pub usage: Option<Usage>,
    /// `cost` 是估算（用量未知）还是按实际用量算的。
    pub estimated: bool,
}

/// 事前预留的请求。
#[derive(Debug, Clone)]
pub struct ExtraCostRequest<'a> {
    pub request_id: &'a RequestId,
    pub payment: &'a PaymentContext,
    pub stage: SafetyStage,
    pub estimate: &'a ExtraCostEstimate,
    pub now: TimestampMs,
}

/// 预留凭证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraCostTicket {
    pub id: u64,
    pub request_id: RequestId,
    pub stage: SafetyStage,
    pub endpoint: EndpointKey,
    pub max_cost: Cost,
}

/// 预留被拒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraCostRefusal {
    /// 付款账户链放不下（准入类的原因码，`budget_exhausted` / `rate_limited` / ……）。
    Denied(ReasonCode),
    /// 账本本身出错：失败即关闭，不检查、不出网。
    Unavailable,
}

/// 检查的结局，用来结算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtraCostOutcome {
    /// 检查跑了，这是实际花费（可能是零笔：规则检查器拒绝了，没有调用 LLM）。
    Spent(Vec<SpendItem>),
    /// 检查可能跑了（超时、客户端断开、future 被丢弃），不知道花了多少：按预留额挂账待核销。
    Unknown,
    /// 检查没有跑：释放预留。
    NotRun,
}

/// 安全检查成本的计量接口（S3 的接缝）。
pub trait ExtraCostMeter: Send + Sync + std::fmt::Debug {
    /// 检查之前：按上界占额度。
    fn reserve(&self, request: &ExtraCostRequest<'_>) -> Result<ExtraCostTicket, ExtraCostRefusal>;

    /// 检查之后：结算。每个 ticket 恰好调用一次。
    fn settle(&self, ticket: ExtraCostTicket, outcome: ExtraCostOutcome);
}

/// 一个 ticket 的结算守卫：`finish` 之前被丢弃（客户端断开、future 被取消）就按 `Unknown` 结算。
pub struct ExtraCostGuard<'a> {
    meter: &'a dyn ExtraCostMeter,
    ticket: Option<ExtraCostTicket>,
}

impl<'a> ExtraCostGuard<'a> {
    pub fn new(meter: &'a dyn ExtraCostMeter, ticket: ExtraCostTicket) -> Self {
        Self {
            meter,
            ticket: Some(ticket),
        }
    }

    pub fn finish(mut self, outcome: ExtraCostOutcome) {
        if let Some(ticket) = self.ticket.take() {
            self.meter.settle(ticket, outcome);
        }
    }
}

impl Drop for ExtraCostGuard<'_> {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            // 被丢弃：检查可能已经调用了上游，保守地按预留额挂账
            self.meter.settle(ticket, ExtraCostOutcome::Unknown);
        }
    }
}

/// [`SimpleExtraCost`] 记下的一条结算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraCostEntry {
    pub request_id: RequestId,
    pub stage: SafetyStage,
    pub endpoint: EndpointKey,
    pub reserved: Cost,
    pub outcome: ExtraCostOutcome,
}

/// 简单实现（TODO(S3)）：一律放行，结算只记在内存里的环形缓冲。**不进账本**。
#[derive(Debug)]
pub struct SimpleExtraCost {
    capacity: usize,
    state: Mutex<SimpleState>,
}

#[derive(Debug, Default)]
struct SimpleState {
    next_id: u64,
    open: usize,
    entries: VecDeque<ExtraCostEntry>,
}

impl SimpleExtraCost {
    pub const DEFAULT_CAPACITY: usize = 4_096;

    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(SimpleState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SimpleState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 已结算的条目（最旧的在前）。
    pub fn entries(&self) -> Vec<ExtraCostEntry> {
        self.lock().entries.iter().cloned().collect()
    }

    /// 已预留、还没结算的个数。每个 ticket 都会被结算，所以请求结束后应当是 0。
    pub fn open_tickets(&self) -> usize {
        self.lock().open
    }
}

impl Default for SimpleExtraCost {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtraCostMeter for SimpleExtraCost {
    fn reserve(&self, request: &ExtraCostRequest<'_>) -> Result<ExtraCostTicket, ExtraCostRefusal> {
        let mut state = self.lock();
        state.next_id += 1;
        state.open += 1;
        Ok(ExtraCostTicket {
            id: state.next_id,
            request_id: request.request_id.clone(),
            stage: request.stage,
            endpoint: request.estimate.endpoint.clone(),
            max_cost: request.estimate.max_cost.clone(),
        })
    }

    fn settle(&self, ticket: ExtraCostTicket, outcome: ExtraCostOutcome) {
        let mut state = self.lock();
        state.open = state.open.saturating_sub(1);
        if state.entries.len() >= self.capacity {
            state.entries.pop_front();
        }
        state.entries.push_back(ExtraCostEntry {
            request_id: ticket.request_id,
            stage: ticket.stage,
            endpoint: ticket.endpoint,
            reserved: ticket.max_cost,
            outcome,
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_compute_contracts::money::Unit;

    use super::*;

    fn key() -> EndpointKey {
        EndpointKey::new("example-judge", "example-org/judge", "openai-compat").unwrap()
    }

    fn estimate() -> ExtraCostEstimate {
        ExtraCostEstimate {
            endpoint: key(),
            max_cost: Cost::from_micros(Unit::Usd, 500).unwrap(),
        }
    }

    fn ticket(meter: &SimpleExtraCost, payment: &PaymentContext) -> ExtraCostTicket {
        let id = RequestId::new("req-1").unwrap();
        meter
            .reserve(&ExtraCostRequest {
                request_id: &id,
                payment,
                stage: SafetyStage::Input,
                estimate: &estimate(),
                now: TimestampMs(0),
            })
            .unwrap()
    }

    fn payment() -> PaymentContext {
        crate::cloud::safety::test_support::payment_context()
    }

    #[test]
    fn a_guard_settles_exactly_once_and_a_dropped_guard_settles_as_unknown() {
        let meter = SimpleExtraCost::new();
        let payment = payment();
        let guard = ExtraCostGuard::new(&meter, ticket(&meter, &payment));
        assert_eq!(meter.open_tickets(), 1);
        guard.finish(ExtraCostOutcome::NotRun);
        assert_eq!(meter.open_tickets(), 0);
        assert_eq!(meter.entries()[0].outcome, ExtraCostOutcome::NotRun);

        // 被丢弃（客户端断开）：保守地按 Unknown 结算，不会漏掉
        let dropped = ExtraCostGuard::new(&meter, ticket(&meter, &payment));
        drop(dropped);
        assert_eq!(meter.open_tickets(), 0);
        assert_eq!(meter.entries()[1].outcome, ExtraCostOutcome::Unknown);
    }

    #[test]
    fn the_in_memory_log_is_bounded() {
        let meter = SimpleExtraCost::with_capacity(2);
        let payment = payment();
        for _ in 0..5 {
            meter.settle(ticket(&meter, &payment), ExtraCostOutcome::Unknown);
        }
        assert_eq!(meter.entries().len(), 2);
    }
}
