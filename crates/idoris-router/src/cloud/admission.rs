//! 候选准入的接口，以及 S3 的真实实现 [`LedgerAdmission`]。
//!
//! 设计稿 §6：「切换到下一个候选之前，**按新候选重新准入**」。执行器对**每一个**要发出去的候选（第一个，
//! 以及每一次切换）都调用一次 [`CandidateAdmission::admit`]，拿到契约的 [`AdmissionDecision`]
//! （放行 / 降级 / 宽限 / 拒绝）；放行类的决定连同价目表一起交给 S0 的
//! `Request::admit` / `Request::begin_next_attempt`，拒绝的决定进不了契约（`NotAdmitted`）。
//!
//! # 三个实现
//!
//! - [`LedgerAdmission`]（**S3，生产用**）：包住账户存储里的准入账本（`idoris_tenancy::accounts::ledger`），
//!   沿付款账户**所有祖先**在一个数据库事务里检查余额或授权额度、授信下限、速率闸门、在途总额上限、补贴池总上限，
//!   通过就同一个事务里占住预留；钱不够先压低 `max_tokens`（`budget_degraded`），不行才（执行器把所有候选试过之后）
//!   宽限一轮（`grace_used`），再不行 `budget_exhausted`。详见那个模块的文档。
//! - [`SimpleAdmission`]：S4 留下的**测试替身**，只算预留额、一律放行，**不检查任何东西、不占任何额度**。
//!   生产装配（`CloudUpstream::from_file`）不再用它；S4 的端到端测试用它来隔离「准入」以外的行为。
//! - 测试里自己写的记录 / 拒绝替身。
//!
//! # 预留的归属与释放
//!
//! 返回放行类的决定意味着**预留已经占住**。预留的身份是尝试 id（`<请求 id>#<序号>`，序号 = `candidate_ordinal`）。
//! 之后它由这几条路收尾，每条都有账本侧的幂等保证：
//!
//! 1. 尝试创建成功、请求收尾：S4 的 `finalize_request` 在**同一个事务里**按结算计划结算 / 释放（账本钩子）；
//! 2. 切换到下一个候选：下一个候选的准入在**它自己的事务里**先释放这个请求之前失败的候选（用户对它们付 0）；
//! 3. **候选没有被采用**（准入放行了，但创建尝试失败、契约拒绝了切换）：执行器调用 [`CandidateAdmission::release`]；
//! 4. 进程崩溃：预留带 TTL，过期后额度自动放回去；之后恢复收尾按迟到结算处理。
//!
//! 没有价目表的候选（该端点没有配置付款账户单位对应的价）在调用本接口之前就被执行器跳过。

use std::sync::Arc;

use idoris_compute_contracts::admission::{AdmissionDecision, Admitted, Reservation};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{AttemptId, AttemptIndex, EndpointKey, RequestId, TimestampMs};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard};
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest, reserve};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AdmitInput, LedgerError};

/// 对一个候选的准入请求。
#[derive(Debug, Clone)]
pub struct AdmissionRequest<'a> {
    pub request_id: &'a RequestId,
    pub payment: &'a PaymentContext,
    /// 要准入的候选。
    pub endpoint: &'a EndpointKey,
    /// `max_tokens`（请求里的，`None` = 没带）与输入 token 估算。
    pub sizing: ReserveRequest,
    pub policy: &'a ReservePolicy,
    /// 这个候选对付款账户单位的价目与上游成本价目。
    pub price_rates: &'a RateCard<Price>,
    pub cost_rates: &'a RateCard<Cost>,
    /// 这个请求已经开始的尝试数：0 = 这是第一次尝试，>0 = 这是首字节之前失败后的切换，要重新做全套检查。
    /// 也是这次尝试的序号：预留的身份是 `<请求 id>#<candidate_ordinal>`。
    pub candidate_ordinal: usize,
    pub now: TimestampMs,
    /// 钱不够、压低 `max_tokens` 也放不下时，是否允许用本周期唯一的宽限（设计稿 §5 体验分级第 3 级）。
    /// 执行器先用 `false` 把所有候选试一遍（压低 `max_tokens`、同场景更便宜的候选），都放不下才用 `true` 再试一遍——
    /// 所以宽限永远是最后一招，排在换更便宜的候选之后。
    pub allow_grace: bool,
}

/// 释放一个**没有被采用**的候选的预留。
#[derive(Debug, Clone)]
pub struct ReleaseRequest<'a> {
    pub request_id: &'a RequestId,
    /// 对应 [`AdmissionRequest::candidate_ordinal`]。
    pub candidate_ordinal: usize,
    pub now: TimestampMs,
}

/// 准入本身出错（不是「拒绝」）：比如账本读不出来。执行器按**失败即关闭**处理：不放行、不出网。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("admission is unavailable: {0}")]
    Unavailable(&'static str),
    #[error("could not size the reservation: {0}")]
    Reserve(String),
}

/// 候选准入。
pub trait CandidateAdmission: Send + Sync {
    fn admit(&self, request: &AdmissionRequest<'_>) -> Result<AdmissionDecision, AdmissionError>;

    /// 候选没有被采用：释放准入时占住的预留。默认什么都不做（没有占住任何东西的实现，比如
    /// [`SimpleAdmission`]）。**失败不能影响请求**（调用方已经在失败路径上）：实现应当把错误写日志，
    /// 预留还有 TTL 兜底。
    fn release(&self, _request: &ReleaseRequest<'_>) {}
}

/// S4 的测试替身：只算预留额，一律放行（见模块文档）。**不检查余额、授信、速率、在途或补贴池，不占任何额度。**
#[derive(Debug, Clone, Copy)]
pub struct SimpleAdmission;

impl CandidateAdmission for SimpleAdmission {
    fn admit(&self, request: &AdmissionRequest<'_>) -> Result<AdmissionDecision, AdmissionError> {
        let price = reserve(request.policy, &request.sizing, request.price_rates)
            .map_err(|e| AdmissionError::Reserve(e.to_string()))?;
        let cost = reserve(request.policy, &request.sizing, request.cost_rates)
            .map_err(|e| AdmissionError::Reserve(e.to_string()))?;
        Ok(AdmissionDecision::Allow {
            admitted: Admitted {
                endpoint: request.endpoint.clone(),
                payer: request.payment.payer().clone(),
                effective_max_tokens: price.effective_max_tokens,
                reservation: Reservation {
                    price: price.amount,
                    cost: cost.amount,
                },
            },
        })
    }
}

/// **S3 的真实准入**：包住账户存储里的准入账本。
///
/// 账本在 `idoris-tenancy`（`accounts::ledger`），这里只是把执行器的 [`AdmissionRequest`] 翻译成账本的
/// [`AdmitInput`]，再把账本的错误翻译成**失败即关闭**的 [`AdmissionError`]：等锁超时、数据库错误、
/// 预留已存在……一律不是放行。
pub struct LedgerAdmission {
    accounts: Arc<AccountStore>,
}

impl std::fmt::Debug for LedgerAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerAdmission").finish_non_exhaustive()
    }
}

impl LedgerAdmission {
    pub fn new(accounts: Arc<AccountStore>) -> Self {
        Self { accounts }
    }

    fn attempt_id(request_id: &RequestId, ordinal: usize) -> Result<AttemptId, AdmissionError> {
        let index = u8::try_from(ordinal)
            .map_err(|_| AdmissionError::Unavailable("attempt ordinal out of range"))?;
        Ok(AttemptId::new(request_id.clone(), AttemptIndex(index)))
    }
}

/// 账本错误 -> 失败即关闭。内部细节只进日志（日志里没有请求内容，也没有密钥）。
fn fail_closed(error: &LedgerError) -> AdmissionError {
    eprintln!("[idoris] cloud admission failed closed: {error}");
    if error.is_busy() {
        AdmissionError::Unavailable("ledger busy")
    } else {
        AdmissionError::Unavailable("ledger error")
    }
}

impl CandidateAdmission for LedgerAdmission {
    fn admit(&self, request: &AdmissionRequest<'_>) -> Result<AdmissionDecision, AdmissionError> {
        let attempt = Self::attempt_id(request.request_id, request.candidate_ordinal)?;
        self.accounts
            .admit_attempt(&AdmitInput {
                attempt: &attempt,
                payment: request.payment,
                endpoint: request.endpoint,
                sizing: request.sizing,
                policy: request.policy,
                price_rates: request.price_rates,
                cost_rates: request.cost_rates,
                allow_grace: request.allow_grace,
                now: request.now,
            })
            .map_err(|e| fail_closed(&e))
    }

    fn release(&self, request: &ReleaseRequest<'_>) {
        let Ok(attempt) = Self::attempt_id(request.request_id, request.candidate_ordinal) else {
            return;
        };
        if let Err(error) = self.accounts.release_attempt(&attempt, request.now) {
            // 不能影响请求：预留还有 TTL 兜底，过期后额度自动放回去
            eprintln!("[idoris] cloud admission: could not release an unused reservation: {error}");
        }
    }
}
