//! 候选准入的接口（S3 的接缝）。
//!
//! 设计稿 §6：「切换到下一个候选之前，**按新候选重新准入**」。执行器对**每一个**要发出去的候选（第一个，
//! 以及每一次切换）都调用一次 [`CandidateAdmission::admit`]，拿到契约的 [`AdmissionDecision`]
//! （放行 / 降级 / 宽限 / 拒绝）；放行类的决定连同价目表一起交给 S0 的
//! `Request::admit` / `Request::begin_next_attempt`，拒绝的决定进不了契约（`NotAdmitted`）。
//!
//! 真正的五项准入（沿付款账户**所有祖先**检查余额或授权额度、授信下限、速率闸门、在途总额上限、平台补贴池总上限，
//! 以及有担保的负值、独立的宽限资格）是 **S3** 的事。S4 只需要一个清晰的接口和一个简单实现：
//!
//! - [`SimpleAdmission`] 只做**预留额计算**（S0 的 `reserve::reserve`：`max_tokens × 输出价 + 输入估算`，
//!   带 `max_tokens` 硬上限），一律放行。**不检查任何余额、授信、速率、在途或补贴池。**
//!   TODO(S3)：换成读账本、填 `AncestorView`、调 `evaluate_admission` 的实现，用 `AdmissionReport::headroom`
//!   喂 `fit_max_tokens` 压低 `max_tokens`，并按需返回 `Degrade` / `Grace` / `Reject`；预留要在同一个事务里占住，
//!   切换失败的候选要释放（这个接口的调用方在候选不被采用时不会调用任何释放，S3 接上账本时要补
//!   `release`，见 `AdmissionRequest` 的文档）。
//!
//! 没有价目表的候选（该端点没有配置付款账户单位对应的价）在调用本接口之前就被执行器跳过。

use idoris_compute_contracts::admission::{AdmissionDecision, Admitted, Reservation};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{EndpointKey, RequestId, TimestampMs};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard};
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest, reserve};

/// 对一个候选的准入请求。
///
/// 返回放行类的决定意味着「预留已经占住」：S3 的实现在这里占用付款账户链的额度。如果之后这个候选没有被
/// 采用（比如 `Request::begin_next_attempt` 因为发布边界拒绝），占住的预留要释放——所以执行器在调用本接口
/// 之前先确认没有越过发布边界，S4 的简单实现也没有任何需要释放的东西。
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
}

/// S4 的简单实现：只算预留额，一律放行（见模块文档，TODO(S3)）。
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
