//! 准入结果与体验分级的类型与纯函数（设计稿 §5）。**不实现账本**：这里只描述「账本该查哪五项、
//! 每项怎么判、判完得到什么」，账本把读到的数填进 [`AncestorView`] 再调用 [`evaluate_admission`]。
//!
//! # 五项检查（沿付款账户的**所有祖先**同时检查，任何一项不过就不放行）
//!
//! | [`CheckKind`] | 设计稿 §5 | 判定（`reserve` 是这次的预留额，同单位） |
//! |---|---|---|
//! | `Funds` | 1 余额，或授权额度 | `可用额度 >= reserve`，可用额度 = 自有余额与授权额度剩余里**较小**的（两者都有就两者都要满足） |
//! | `CreditFloor` | 2 授信下限 | 仅在 `Funds` 不过时才看：`reserve - 可用额度 <= 授信额度`，即透支不超过担保人给的授信下限 |
//! | `RateGate` | 3 速率闸门 | 每个窗口（分钟、小时）`已花 + reserve <= 上限` |
//! | `InFlightCap` | 4 在途总额上限 | `在途 + reserve <= 上限` |
//! | `SubsidyPoolCap` | 5 平台补贴池总上限 | `补贴池剩余 >= reserve`（只有补贴池账户才有这一项） |
//!
//! 一个账户「钱够不够」这一关 = `Funds` 通过，**或者** `Funds` 不过但 `CreditFloor` 通过（用了担保人的授信，
//! 即「有担保的负值」）。所以一个被授信救回来的 `Funds` 失败**不算**阻断（[`AdmissionReport::blocking_failures`]）。
//! 其余三项没有救济，不过就是阻断。
//!
//! 同一份输入永远得到同一份报告；金额运算全部 checked，单位不一致、溢出都返回错误而不是猜。
//!
//! # 准入决定与体验分级
//!
//! [`AdmissionDecision`] 四种：放行 / 降级 / 宽限 / 拒绝，对应设计稿四级。每种携带的原因码类型**只允许
//! 该级别的码**（降级只能是 [`DegradeReason`] 三个之一，拒绝只能是 [`DenialReason`]），所以
//! 「宽限却带 `budget_exhausted`」这种组合造不出来。原因码见 [`crate::reason`]。
//!
//! 决策本身（什么时候降级、什么时候用宽限）是 S3 的逻辑，不在这里；这里给它两个纯的积木：
//! [`AdmissionReport::max_admissible`]（所有祖先里最紧的上限，喂给 [`crate::reserve::fit_max_tokens`]
//! 来压低 `max_tokens`）和 [`check_grace`]（宽限资格，**不依赖 settle 的 overage**，评审 B4）。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ids::{AccountId, BillingMonth, EndpointKey};
use crate::money::{Money, MoneyError};
use crate::pricing::{Cost, Price};
use crate::reason::{ExperienceTier, ReasonCode};

/// 每个成员每个周期最多使用宽限的次数（设计稿 §5）。
pub const MAX_GRACE_PER_PERIOD: u32 = 1;

/// 准入五项检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Funds,
    CreditFloor,
    RateGate,
    InFlightCap,
    SubsidyPoolCap,
}

impl CheckKind {
    pub const ALL: [CheckKind; 5] = [
        CheckKind::Funds,
        CheckKind::CreditFloor,
        CheckKind::RateGate,
        CheckKind::InFlightCap,
        CheckKind::SubsidyPoolCap,
    ];

    /// 这一项不过时对应的拒绝原因码。`Funds` 和 `CreditFloor` 都是「钱不够」。
    pub fn denial_code(self) -> ReasonCode {
        match self {
            CheckKind::Funds | CheckKind::CreditFloor => ReasonCode::BudgetExhausted,
            CheckKind::RateGate => ReasonCode::RateLimited,
            CheckKind::InFlightCap => ReasonCode::InFlightLimited,
            CheckKind::SubsidyPoolCap => ReasonCode::SubsidyPoolExhausted,
        }
    }
}

/// 速率闸门的窗口（设计稿 §5：分钟 / 小时花费上限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateWindowKind {
    Minute,
    Hour,
}

/// 一个速率窗口：窗口内已花的钱、窗口上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateWindowView {
    pub window: RateWindowKind,
    pub spent: Money,
    pub cap: Money,
}

/// 在途总额：已预留、还没结算的总额，以及它的上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InFlightView {
    pub current: Money,
    pub cap: Money,
}

/// 付款账户链上一个账户的当前状态，由账本在准入事务里读出来填好。所有 `Money` 必须与预留额同单位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AncestorView {
    pub account: AccountId,
    /// 自有余额（可为负，表示已经透支）。没有自有资金（纯委派）的账户为 `None`。
    pub balance: Option<Money>,
    /// 授权额度（来自父账户的 [`crate::account::Allocation`]）还剩多少。没有入向额度为 `None`。
    pub ceiling_remaining: Option<Money>,
    /// 担保人给这个账户的授信额度（非负；0 = 没有授信，不允许透支）。
    pub credit_limit: Money,
    pub rate_windows: Vec<RateWindowView>,
    pub in_flight: Option<InFlightView>,
    /// 平台补贴池还剩多少。只有补贴池账户为 `Some`。
    pub subsidy_pool_remaining: Option<Money>,
}

impl AncestorView {
    /// 一个没有任何限制的账户（所有上限都没配置，授信为 0）。测试和「根账户」用。
    pub fn unbounded(account: AccountId, unit: crate::money::Unit) -> Self {
        Self {
            account,
            balance: None,
            ceiling_remaining: None,
            credit_limit: Money::zero(unit),
            rate_windows: Vec::new(),
            in_flight: None,
            subsidy_pool_remaining: None,
        }
    }
}

/// 单项检查的结果。只有输出（进审计、进错误体），不提供 `Deserialize`：它含内部标签的单元变体，
/// serde 对这类变体不检查未知字段，而本 crate 的输入类型一律拒绝未知字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    /// 不过。`shortfall` = 还差多少才能过（总是 > 0）。
    Fail {
        shortfall: Money,
    },
    /// `CreditFloor` 专用：`Funds` 不够，但透支 `overdraft` 没超过授信，所以放行（有担保的负值）。
    PassUsingCredit {
        overdraft: Money,
    },
    /// `CreditFloor` 专用：`Funds` 已通过，用不着授信。
    NotNeeded,
    /// 这个账户没有配置这一项（视为不设限）。
    NotConfigured,
}

/// 一个账户上一项检查的结果。`RateGate` 每个窗口一条（`window` 标明）。只有输出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub account: AccountId,
    pub check: CheckKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<RateWindowKind>,
    pub outcome: CheckOutcome,
}

/// 准入输入的错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("the ancestor chain must not be empty")]
    EmptyChain,
    #[error("account {0} appears twice in the ancestor chain")]
    DuplicateAccount(AccountId),
    #[error("{field} of account {account} must not be negative")]
    NegativeLimit {
        account: AccountId,
        field: &'static str,
    },
    #[error("reason code {0} is not a denial and cannot be used as a rejection")]
    NotADenial(ReasonCode),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// 一次准入评估的报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdmissionReport {
    reserve: Price,
    results: Vec<CheckResult>,
    max_admissible: Option<Money>,
}

impl AdmissionReport {
    pub fn reserve(&self) -> &Price {
        &self.reserve
    }

    /// 全部结果，按「链上的顺序、检查顺序」排列。
    pub fn results(&self) -> &[CheckResult] {
        &self.results
    }

    /// 所有祖先里最紧的那个上限，也就是「**最大的、能让五项检查全过的预留额**」：预留额 `<=` 它就能过，
    /// `>` 它就不能过（恰好等价，有性质测试钉住）。所有账户都不设限时为 `None`。
    ///
    /// **有符号**：为负表示某个限额已经被超过，此时连 0 的预留都过不了。要喂给
    /// [`crate::reserve::fit_max_tokens`] 请用 [`Self::headroom`]。
    pub fn max_admissible(&self) -> Option<&Money> {
        self.max_admissible.as_ref()
    }

    /// [`Self::max_admissible`] 的非负版本（负数按 0），类型是 `Price`，可以直接作为
    /// [`crate::reserve::fit_max_tokens`] 的 `headroom`。注意：`max_admissible` 为负时即使压到 0
    /// 也过不了准入，调用方不应再尝试压低 `max_tokens`（`only_budget_blocked` 与否另行判断）。
    pub fn headroom(&self) -> Result<Option<Price>, MoneyError> {
        self.max_admissible
            .as_ref()
            .map(|m| Price::new(Money::new(m.unit.clone(), m.micros.max(0))))
            .transpose()
    }

    /// 是否有账户用了授信（有担保的负值）。有的话应通知担保人。
    pub fn uses_credit(&self) -> bool {
        self.results
            .iter()
            .any(|r| matches!(r.outcome, CheckOutcome::PassUsingCredit { .. }))
    }

    /// 阻断放行的失败。`Funds` 失败如果被同一账户的授信救回（`CreditFloor` 通过）就不算。
    pub fn blocking_failures(&self) -> Vec<&CheckResult> {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, CheckOutcome::Fail { .. }))
            .filter(|r| {
                !(r.check == CheckKind::Funds
                    && self.results.iter().any(|other| {
                        other.account == r.account
                            && other.check == CheckKind::CreditFloor
                            && matches!(other.outcome, CheckOutcome::PassUsingCredit { .. })
                    }))
            })
            .collect()
    }

    /// 五项检查沿所有祖先是否全部通过。
    pub fn passed(&self) -> bool {
        self.blocking_failures().is_empty()
    }

    /// 阻断失败对应的拒绝原因码，去重，按 [`ReasonCode`] 的顺序排。选哪个展示给调用方是 S3/S5 的事。
    pub fn blocking_codes(&self) -> Vec<ReasonCode> {
        self.blocking_failures()
            .iter()
            .map(|r| r.check.denial_code())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// 阻断全部来自「钱不够」（`Funds`/`CreditFloor`），且至少有一个。只有这种情况才允许走降级或宽限；
    /// 速率、在途、补贴池上限是平台保护，宽限和降级都不能绕过。
    pub fn only_budget_blocked(&self) -> bool {
        let failures = self.blocking_failures();
        !failures.is_empty()
            && failures
                .iter()
                .all(|r| matches!(r.check, CheckKind::Funds | CheckKind::CreditFloor))
    }
}

fn non_negative(account: &AccountId, field: &'static str, m: &Money) -> Result<(), AdmissionError> {
    if m.is_negative() {
        Err(AdmissionError::NegativeLimit {
            account: account.clone(),
            field,
        })
    } else {
        Ok(())
    }
}

/// 对 `chain`（`[付款账户, 父, ..., 根]`）逐个账户评估五项检查。
///
/// - `reserve` 与 `chain` 里所有金额必须同一单位，否则返回单位不一致错误；
/// - `credit_limit`、速率窗口、在途的上限与已用都不能为负；
/// - 链上账户 id 不能重复，链不能为空。
pub fn evaluate_admission(
    reserve: &Price,
    chain: &[AncestorView],
) -> Result<AdmissionReport, AdmissionError> {
    if chain.is_empty() {
        return Err(AdmissionError::EmptyChain);
    }
    let reserve_money = reserve.money();
    let mut seen = BTreeSet::new();
    let mut results = Vec::new();
    // 所有上限里最紧的：每个上限折算成「最多还能预留多少」
    let mut tightest: Option<Money> = None;
    let mut tighten = |candidate: Money| -> Result<(), MoneyError> {
        tightest = Some(match tightest.take() {
            None => candidate,
            Some(current) => current.try_min(&candidate)?,
        });
        Ok(())
    };

    for view in chain {
        if !seen.insert(view.account.clone()) {
            return Err(AdmissionError::DuplicateAccount(view.account.clone()));
        }
        let account = &view.account;

        // ---- 校验输入 ----
        non_negative(account, "credit_limit", &view.credit_limit)?;
        reserve_money.require_same_unit(&view.credit_limit)?;
        // 这三项允许为负（已经透支），只检查单位
        for money in [
            &view.balance,
            &view.ceiling_remaining,
            &view.subsidy_pool_remaining,
        ]
        .into_iter()
        .flatten()
        {
            reserve_money.require_same_unit(money)?;
        }
        for w in &view.rate_windows {
            non_negative(account, "rate_windows.spent", &w.spent)?;
            non_negative(account, "rate_windows.cap", &w.cap)?;
            reserve_money.require_same_unit(&w.spent)?;
            reserve_money.require_same_unit(&w.cap)?;
        }
        if let Some(f) = &view.in_flight {
            non_negative(account, "in_flight.current", &f.current)?;
            non_negative(account, "in_flight.cap", &f.cap)?;
            reserve_money.require_same_unit(&f.current)?;
            reserve_money.require_same_unit(&f.cap)?;
        }

        // ---- 1 Funds / 2 CreditFloor ----
        let mut available: Option<Money> = None;
        for m in [&view.balance, &view.ceiling_remaining]
            .into_iter()
            .flatten()
        {
            available = Some(match available.take() {
                None => m.clone(),
                Some(a) => a.try_min(m)?,
            });
        }
        let (funds, credit) = match &available {
            None => (CheckOutcome::NotConfigured, CheckOutcome::NotConfigured),
            Some(avail) => {
                tighten(avail.checked_add(&view.credit_limit)?)?;
                if avail.try_cmp(reserve_money)?.is_ge() {
                    (CheckOutcome::Pass, CheckOutcome::NotNeeded)
                } else {
                    let overdraft = reserve_money.checked_sub(avail)?;
                    let funds = CheckOutcome::Fail {
                        shortfall: overdraft.clone(),
                    };
                    if overdraft.try_cmp(&view.credit_limit)?.is_le() {
                        (funds, CheckOutcome::PassUsingCredit { overdraft })
                    } else {
                        let beyond = overdraft.checked_sub(&view.credit_limit)?;
                        (funds, CheckOutcome::Fail { shortfall: beyond })
                    }
                }
            }
        };
        results.push(CheckResult {
            account: account.clone(),
            check: CheckKind::Funds,
            window: None,
            outcome: funds,
        });
        results.push(CheckResult {
            account: account.clone(),
            check: CheckKind::CreditFloor,
            window: None,
            outcome: credit,
        });

        // ---- 3 RateGate ----
        if view.rate_windows.is_empty() {
            results.push(CheckResult {
                account: account.clone(),
                check: CheckKind::RateGate,
                window: None,
                outcome: CheckOutcome::NotConfigured,
            });
        }
        for w in &view.rate_windows {
            tighten(w.cap.checked_sub(&w.spent)?)?;
            let projected = w.spent.checked_add(reserve_money)?;
            let outcome = if projected.try_cmp(&w.cap)?.is_le() {
                CheckOutcome::Pass
            } else {
                CheckOutcome::Fail {
                    shortfall: projected.checked_sub(&w.cap)?,
                }
            };
            results.push(CheckResult {
                account: account.clone(),
                check: CheckKind::RateGate,
                window: Some(w.window),
                outcome,
            });
        }

        // ---- 4 InFlightCap ----
        let in_flight = match &view.in_flight {
            None => CheckOutcome::NotConfigured,
            Some(f) => {
                tighten(f.cap.checked_sub(&f.current)?)?;
                let projected = f.current.checked_add(reserve_money)?;
                if projected.try_cmp(&f.cap)?.is_le() {
                    CheckOutcome::Pass
                } else {
                    CheckOutcome::Fail {
                        shortfall: projected.checked_sub(&f.cap)?,
                    }
                }
            }
        };
        results.push(CheckResult {
            account: account.clone(),
            check: CheckKind::InFlightCap,
            window: None,
            outcome: in_flight,
        });

        // ---- 5 SubsidyPoolCap ----
        let pool = match &view.subsidy_pool_remaining {
            None => CheckOutcome::NotConfigured,
            Some(remaining) => {
                tighten(remaining.clone())?;
                if remaining.try_cmp(reserve_money)?.is_ge() {
                    CheckOutcome::Pass
                } else {
                    CheckOutcome::Fail {
                        shortfall: reserve_money.checked_sub(remaining)?,
                    }
                }
            }
        };
        results.push(CheckResult {
            account: account.clone(),
            check: CheckKind::SubsidyPoolCap,
            window: None,
            outcome: pool,
        });
    }

    Ok(AdmissionReport {
        reserve: reserve.clone(),
        results,
        max_admissible: tightest,
    })
}

/// 一次尝试的预留：对用户的价（占用付款账户链的额度）+ 上游成本（挂账，供 `maybe_executed`
/// 待核销时按预留额记）。两个类型不同，不会混。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub price: Price,
    pub cost: Cost,
}

/// 被放行的（候选、预留）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admitted {
    /// 放行的是哪个候选。切换候选前要按新候选重新准入，所以这里带着它，状态机会核对。
    pub endpoint: EndpointKey,
    /// 付款账户，必须就是这次请求的付款上下文（[`crate::credential::PaymentContext::payer`]）。
    pub payer: AccountId,
    /// 必须发给上游的 `max_tokens`（见 [`crate::reserve`]）。
    pub effective_max_tokens: u64,
    pub reservation: Reservation,
}

/// 降级的原因（设计稿 §5 第 2 级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradeReason {
    /// 额度紧，换了更便宜的模型或压低了 `max_tokens`。
    Budget,
    /// 容量不够。
    Capacity,
    /// 首字节前上游失败，切换了候选。
    UpstreamFailover,
}

impl DegradeReason {
    pub fn reason_code(self) -> ReasonCode {
        match self {
            DegradeReason::Budget => ReasonCode::BudgetDegraded,
            DegradeReason::Capacity => ReasonCode::CapacityDegraded,
            DegradeReason::UpstreamFailover => ReasonCode::UpstreamFailover,
        }
    }
}

/// 拒绝的原因：一个**不服务请求**的原因码（[`ReasonCode::is_served`] 为 false）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "ReasonCode", into = "ReasonCode")]
pub struct DenialReason(ReasonCode);

impl DenialReason {
    pub fn new(code: ReasonCode) -> Result<Self, AdmissionError> {
        if code.is_served() {
            Err(AdmissionError::NotADenial(code))
        } else {
            Ok(Self(code))
        }
    }

    pub fn code(self) -> ReasonCode {
        self.0
    }
}

impl TryFrom<ReasonCode> for DenialReason {
    type Error = AdmissionError;
    fn try_from(code: ReasonCode) -> Result<Self, AdmissionError> {
        Self::new(code)
    }
}

impl From<DenialReason> for ReasonCode {
    fn from(reason: DenialReason) -> ReasonCode {
        reason.0
    }
}

/// 准入决定（设计稿 §5 的四级）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmissionDecision {
    /// 第 1 级：正常。
    Allow { admitted: Admitted },
    /// 第 2 级：降级，同场景换便宜模型 / 压低 `max_tokens` / 换候选。
    Degrade {
        admitted: Admitted,
        reason: DegradeReason,
    },
    /// 第 3 级：宽限一轮（原因码 `grace_used`），**必须通知担保人**。
    Grace { admitted: Admitted },
    /// 第 4 级：拒绝。`budget_exhausted` 即设计稿的 402；其它拒绝不在四级阶梯上。
    Reject { reason: DenialReason },
}

impl AdmissionDecision {
    /// 在四级阶梯上的位置。不在阶梯上的拒绝（限速、凭证冻结、没有合规候选等）返回 `None`：
    /// 它们不是「钱不够」，不能说成第 4 级。
    pub fn tier(&self) -> Option<ExperienceTier> {
        match self {
            AdmissionDecision::Allow { .. } => Some(ExperienceTier::Normal),
            AdmissionDecision::Degrade { .. } => Some(ExperienceTier::Degraded),
            AdmissionDecision::Grace { .. } => Some(ExperienceTier::Grace),
            AdmissionDecision::Reject { reason } => reason.code().tier(),
        }
    }

    pub fn reason_code(&self) -> Option<ReasonCode> {
        match self {
            AdmissionDecision::Allow { .. } => None,
            AdmissionDecision::Degrade { reason, .. } => Some(reason.reason_code()),
            AdmissionDecision::Grace { .. } => Some(ReasonCode::GraceUsed),
            AdmissionDecision::Reject { reason } => Some(reason.code()),
        }
    }

    /// 放行（含降级、宽限）时返回被放行的内容；拒绝返回 `None`。
    pub fn admitted(&self) -> Option<&Admitted> {
        match self {
            AdmissionDecision::Allow { admitted }
            | AdmissionDecision::Degrade { admitted, .. }
            | AdmissionDecision::Grace { admitted } => Some(admitted),
            AdmissionDecision::Reject { .. } => None,
        }
    }

    pub fn is_admitted(&self) -> bool {
        self.admitted().is_some()
    }

    /// 便捷构造：校验 `code` 确实是拒绝类原因码。
    pub fn reject(code: ReasonCode) -> Result<Self, AdmissionError> {
        Ok(AdmissionDecision::Reject {
            reason: DenialReason::new(code)?,
        })
    }
}

/// 宽限的计数键：每个付款账户每个账期一次（设计稿 §5：每个成员每个周期最多 1 次）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraceKey {
    pub account: AccountId,
    pub period: BillingMonth,
}

/// 宽限资格通过：这一轮没被可用额度覆盖的部分（不超过一次预留）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraceGrant {
    pub shortfall: Price,
}

/// 宽限被拒绝的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraceDenied {
    #[error("funds already cover the reservation; grace is not needed")]
    NotNeeded,
    #[error("grace already used {used} time(s) this period (max {MAX_GRACE_PER_PERIOD})")]
    AlreadyUsed { used: u32 },
    #[error("the uncovered amount {shortfall:?} exceeds one reservation {reserve:?}")]
    ExceedsOneReservation { shortfall: Money, reserve: Money },
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// 宽限资格（**独立计算，不依赖 settle 的 overage**，评审 B4）。
///
/// - `used_in_period`：这个账户这个账期已经用掉的宽限次数，必须 `< MAX_GRACE_PER_PERIOD`；
/// - `available`：这个账户当前可用额度（可为负，表示已经透支）；
/// - `reserve`：这次的预留额。
///
/// 没被覆盖的部分 `shortfall = reserve - available`，必须 `<= reserve`（不超过**一次**预留）：
/// 账户已经透支（`available < 0`）时 `shortfall > reserve`，宽限被拒绝——宽限是「最后一单」，
/// 不是给已经欠着钱的账户续命。检查顺序固定：单位 → 用不着 → 次数 → 金额。
pub fn check_grace(
    used_in_period: u32,
    available: &Money,
    reserve: &Price,
) -> Result<GraceGrant, GraceDenied> {
    let shortfall = reserve.money().checked_sub(available)?;
    if !shortfall.is_positive() {
        return Err(GraceDenied::NotNeeded);
    }
    if used_in_period >= MAX_GRACE_PER_PERIOD {
        return Err(GraceDenied::AlreadyUsed {
            used: used_in_period,
        });
    }
    if shortfall.try_cmp(reserve.money())?.is_gt() {
        return Err(GraceDenied::ExceedsOneReservation {
            shortfall,
            reserve: reserve.money().clone(),
        });
    }
    Ok(GraceGrant {
        shortfall: Price::new(shortfall)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::money::Unit;
    use crate::test_support::aid;
    use proptest::prelude::*;

    fn m(micros: i64) -> Money {
        Money::new(Unit::Thb, micros)
    }
    fn price(micros: i64) -> Price {
        Price::from_micros(Unit::Thb, micros).unwrap()
    }
    fn view(name: &str) -> AncestorView {
        AncestorView::unbounded(aid(name), Unit::Thb)
    }
    fn only<'a>(report: &'a AdmissionReport, account: &str, check: CheckKind) -> &'a CheckOutcome {
        let hits: Vec<_> = report
            .results()
            .iter()
            .filter(|r| r.account == aid(account) && r.check == check)
            .collect();
        assert_eq!(hits.len(), 1, "{account} {check:?}");
        &hits[0].outcome
    }

    // ---------- Funds / CreditFloor ----------

    #[test]
    fn funds_check_boundaries_without_credit() {
        let mut v = view("a");
        v.balance = Some(m(100));
        // (reserve, 期望)
        for (reserve, pass) in [(99, true), (100, true), (101, false), (0, true)] {
            let report = evaluate_admission(&price(reserve), std::slice::from_ref(&v)).unwrap();
            assert_eq!(report.passed(), pass, "reserve={reserve}");
            if pass {
                assert_eq!(only(&report, "a", CheckKind::Funds), &CheckOutcome::Pass);
                assert_eq!(
                    only(&report, "a", CheckKind::CreditFloor),
                    &CheckOutcome::NotNeeded
                );
            } else {
                assert_eq!(
                    only(&report, "a", CheckKind::Funds),
                    &CheckOutcome::Fail { shortfall: m(1) }
                );
                assert_eq!(
                    only(&report, "a", CheckKind::CreditFloor),
                    &CheckOutcome::Fail { shortfall: m(1) }
                );
                assert_eq!(report.blocking_codes(), vec![ReasonCode::BudgetExhausted]);
            }
        }
    }

    #[test]
    fn credit_floor_rescues_funds_only_up_to_the_limit() {
        let mut v = view("a");
        v.balance = Some(m(50));
        v.credit_limit = m(30);
        // reserve 80：差 30，刚好等于授信：放行，用了授信
        let r = evaluate_admission(&price(80), std::slice::from_ref(&v)).unwrap();
        assert!(r.passed() && r.uses_credit());
        assert_eq!(
            only(&r, "a", CheckKind::Funds),
            &CheckOutcome::Fail { shortfall: m(30) }
        );
        assert_eq!(
            only(&r, "a", CheckKind::CreditFloor),
            &CheckOutcome::PassUsingCredit { overdraft: m(30) }
        );
        assert!(
            r.blocking_failures().is_empty(),
            "被授信救回的 Funds 失败不是阻断"
        );
        // reserve 81：超出授信 1：拒绝，CreditFloor 差 1
        let r = evaluate_admission(&price(81), std::slice::from_ref(&v)).unwrap();
        assert!(!r.passed() && !r.uses_credit());
        assert_eq!(
            only(&r, "a", CheckKind::CreditFloor),
            &CheckOutcome::Fail { shortfall: m(1) }
        );
        assert_eq!(r.blocking_failures().len(), 2); // Funds 与 CreditFloor 都阻断
        // 没有授信（0）：和没有 CreditFloor 一样
        v.credit_limit = m(0);
        let r = evaluate_admission(&price(51), std::slice::from_ref(&v)).unwrap();
        assert!(!r.passed());
    }

    #[test]
    fn an_already_overdrawn_account_may_go_further_only_within_the_credit_floor() {
        // 余额 -10，授信 30（下限 -30）：reserve 20 -> 余额变 -30，刚好在下限上
        let mut v = view("a");
        v.balance = Some(m(-10));
        v.credit_limit = m(30);
        assert!(
            evaluate_admission(&price(20), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
        assert!(
            !evaluate_admission(&price(21), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
    }

    #[test]
    fn balance_and_ceiling_must_both_hold() {
        let mut v = view("a");
        v.balance = Some(m(1_000));
        v.ceiling_remaining = Some(m(10));
        assert!(
            evaluate_admission(&price(10), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
        assert!(
            !evaluate_admission(&price(11), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
        // 反过来
        v.balance = Some(m(5));
        v.ceiling_remaining = Some(m(1_000));
        assert!(
            !evaluate_admission(&price(6), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
    }

    #[test]
    fn unconfigured_everything_means_unbounded() {
        let r = evaluate_admission(&price(i64::MAX), &[view("root")]).unwrap();
        assert!(r.passed());
        assert!(r.max_admissible().is_none());
        for check in CheckKind::ALL {
            let outcomes: Vec<_> = r.results().iter().filter(|x| x.check == check).collect();
            assert_eq!(outcomes.len(), 1, "{check:?}");
            assert_eq!(outcomes[0].outcome, CheckOutcome::NotConfigured);
        }
    }

    // ---------- 沿所有祖先 ----------

    #[test]
    fn every_ancestor_is_checked_and_a_failure_names_the_account() {
        let mut member = view("member");
        member.ceiling_remaining = Some(m(100));
        let mut sponsor = view("sponsor");
        sponsor.balance = Some(m(40)); // 担保人钱不够
        let mut org = view("org");
        org.balance = Some(m(10_000));
        let chain = [member, sponsor, org];
        let r = evaluate_admission(&price(50), &chain).unwrap();
        assert!(!r.passed());
        let failing: Vec<_> = r
            .blocking_failures()
            .iter()
            .map(|x| (x.account.as_str().to_string(), x.check))
            .collect();
        assert_eq!(
            failing,
            vec![
                ("sponsor".to_string(), CheckKind::Funds),
                ("sponsor".to_string(), CheckKind::CreditFloor)
            ]
        );
        assert_eq!(only(&r, "member", CheckKind::Funds), &CheckOutcome::Pass);
        assert_eq!(only(&r, "org", CheckKind::Funds), &CheckOutcome::Pass);
        // 每个账户都出了完整的五项结果
        for name in ["member", "sponsor", "org"] {
            for check in CheckKind::ALL {
                assert!(
                    r.results()
                        .iter()
                        .any(|x| x.account == aid(name) && x.check == check)
                );
            }
        }
        // 预留压到 40 就全过
        assert!(evaluate_admission(&price(40), &chain).unwrap().passed());
    }

    // ---------- 速率闸门 / 在途 / 补贴池 ----------

    #[test]
    fn rate_gate_checks_every_window_independently() {
        let mut v = view("a");
        v.rate_windows = vec![
            RateWindowView {
                window: RateWindowKind::Minute,
                spent: m(90),
                cap: m(100),
            },
            RateWindowView {
                window: RateWindowKind::Hour,
                spent: m(500),
                cap: m(1_000),
            },
        ];
        // reserve 10：分钟窗口刚好满，小时窗口充裕
        let r = evaluate_admission(&price(10), std::slice::from_ref(&v)).unwrap();
        assert!(r.passed());
        let rate: Vec<_> = r
            .results()
            .iter()
            .filter(|x| x.check == CheckKind::RateGate)
            .collect();
        assert_eq!(rate.len(), 2);
        assert_eq!(rate[0].window, Some(RateWindowKind::Minute));
        assert_eq!(rate[1].window, Some(RateWindowKind::Hour));
        // reserve 11：分钟窗口超 1
        let r = evaluate_admission(&price(11), std::slice::from_ref(&v)).unwrap();
        assert!(!r.passed());
        let failures = r.blocking_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].window, Some(RateWindowKind::Minute));
        assert_eq!(failures[0].outcome, CheckOutcome::Fail { shortfall: m(1) });
        assert_eq!(r.blocking_codes(), vec![ReasonCode::RateLimited]);
        // 授信救不了速率
        v.balance = Some(m(0));
        v.credit_limit = m(1_000_000);
        assert!(
            !evaluate_admission(&price(11), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
    }

    #[test]
    fn in_flight_and_subsidy_pool_boundaries() {
        let mut v = view("a");
        v.in_flight = Some(InFlightView {
            current: m(70),
            cap: m(100),
        });
        v.subsidy_pool_remaining = Some(m(25));
        for (reserve, in_flight_ok, pool_ok) in [
            (24, true, true),
            (25, true, true),
            (26, true, false),
            (30, true, false),
            (31, false, false),
        ] {
            let r = evaluate_admission(&price(reserve), std::slice::from_ref(&v)).unwrap();
            assert_eq!(
                matches!(only(&r, "a", CheckKind::InFlightCap), CheckOutcome::Pass),
                in_flight_ok,
                "in-flight reserve={reserve}"
            );
            assert_eq!(
                matches!(only(&r, "a", CheckKind::SubsidyPoolCap), CheckOutcome::Pass),
                pool_ok,
                "pool reserve={reserve}"
            );
            assert_eq!(r.passed(), in_flight_ok && pool_ok);
        }
        let r = evaluate_admission(&price(31), std::slice::from_ref(&v)).unwrap();
        assert_eq!(
            r.blocking_codes(),
            vec![
                ReasonCode::InFlightLimited,
                ReasonCode::SubsidyPoolExhausted
            ]
        );
    }

    #[test]
    fn only_budget_blocked_gates_grace_and_degrade() {
        let mut v = view("a");
        v.balance = Some(m(10));
        // 只有钱不够
        let r = evaluate_admission(&price(20), std::slice::from_ref(&v)).unwrap();
        assert!(r.only_budget_blocked());
        // 钱不够，同时速率也超：不能走宽限/降级来绕过平台保护
        v.rate_windows = vec![RateWindowView {
            window: RateWindowKind::Minute,
            spent: m(0),
            cap: m(5),
        }];
        let r = evaluate_admission(&price(20), std::slice::from_ref(&v)).unwrap();
        assert!(!r.only_budget_blocked());
        assert_eq!(
            r.blocking_codes(),
            vec![ReasonCode::BudgetExhausted, ReasonCode::RateLimited]
        );
        // 全通过：不是「被阻断」
        let r = evaluate_admission(&price(1), std::slice::from_ref(&view("x"))).unwrap();
        assert!(!r.only_budget_blocked());
    }

    // ---------- 输入校验 ----------

    #[test]
    fn invalid_inputs_are_errors_not_guesses() {
        let ok = view("a");
        assert_eq!(
            evaluate_admission(&price(1), &[]).unwrap_err(),
            AdmissionError::EmptyChain
        );
        assert_eq!(
            evaluate_admission(&price(1), &[view("a"), view("a")]).unwrap_err(),
            AdmissionError::DuplicateAccount(aid("a"))
        );
        // 单位不一致：任何一个字段
        let usd = |micros| Money::new(Unit::Usd, micros);
        let mut cases: Vec<AncestorView> = Vec::new();
        let mut v = ok.clone();
        v.balance = Some(usd(1));
        cases.push(v);
        let mut v = ok.clone();
        v.ceiling_remaining = Some(usd(1));
        cases.push(v);
        let mut v = ok.clone();
        v.credit_limit = usd(0);
        cases.push(v);
        let mut v = ok.clone();
        v.subsidy_pool_remaining = Some(usd(1));
        cases.push(v);
        let mut v = ok.clone();
        v.in_flight = Some(InFlightView {
            current: usd(0),
            cap: usd(1),
        });
        cases.push(v);
        let mut v = ok.clone();
        v.rate_windows = vec![RateWindowView {
            window: RateWindowKind::Hour,
            spent: usd(0),
            cap: usd(1),
        }];
        cases.push(v);
        for bad in cases {
            assert!(
                matches!(
                    evaluate_admission(&price(1), std::slice::from_ref(&bad)).unwrap_err(),
                    AdmissionError::Money(MoneyError::UnitMismatch { .. })
                ),
                "{bad:?}"
            );
        }
        // 负的限额
        let mut v = ok.clone();
        v.credit_limit = m(-1);
        assert!(matches!(
            evaluate_admission(&price(1), &[v]).unwrap_err(),
            AdmissionError::NegativeLimit {
                field: "credit_limit",
                ..
            }
        ));
        let mut v = ok.clone();
        v.in_flight = Some(InFlightView {
            current: m(-1),
            cap: m(1),
        });
        assert!(matches!(
            evaluate_admission(&price(1), &[v]).unwrap_err(),
            AdmissionError::NegativeLimit {
                field: "in_flight.current",
                ..
            }
        ));
        // 溢出：已花 + 预留超过 i64
        let mut v = ok;
        v.rate_windows = vec![RateWindowView {
            window: RateWindowKind::Hour,
            spent: m(i64::MAX),
            cap: m(i64::MAX),
        }];
        assert_eq!(
            evaluate_admission(&price(1), &[v]).unwrap_err(),
            AdmissionError::Money(MoneyError::Overflow)
        );
    }

    // ---------- max_admissible ----------

    #[test]
    fn max_admissible_is_the_tightest_bound_across_the_chain() {
        let mut member = view("member");
        member.ceiling_remaining = Some(m(100));
        member.credit_limit = m(5); // 100 + 5 = 105
        let mut org = view("org");
        org.balance = Some(m(500));
        org.rate_windows = vec![RateWindowView {
            window: RateWindowKind::Minute,
            spent: m(900),
            cap: m(1_000),
        }]; // 100
        org.in_flight = Some(InFlightView {
            current: m(10),
            cap: m(60),
        }); // 50
        let mut pool = view("pool");
        pool.subsidy_pool_remaining = Some(m(75));
        let chain = [member, org, pool];
        let r = evaluate_admission(&price(1), &chain).unwrap();
        assert_eq!(r.max_admissible().unwrap().micros, 50);
        assert_eq!(r.headroom().unwrap().unwrap().micros(), 50);
        assert!(evaluate_admission(&price(50), &chain).unwrap().passed());
        assert!(!evaluate_admission(&price(51), &chain).unwrap().passed());
    }

    #[test]
    fn max_admissible_is_negative_when_a_limit_is_already_exceeded() {
        // 余额 -100，授信 10：已经穿过授信下限，连 0 的预留都过不了
        let mut v = view("a");
        v.balance = Some(m(-100));
        v.credit_limit = m(10);
        let r = evaluate_admission(&price(1), std::slice::from_ref(&v)).unwrap();
        assert_eq!(r.max_admissible().unwrap().micros, -90);
        assert!(!r.passed());
        assert!(
            !evaluate_admission(&price(0), std::slice::from_ref(&v))
                .unwrap()
                .passed()
        );
        // headroom 是非负版本，给 fit_max_tokens 用
        assert_eq!(r.headroom().unwrap().unwrap().micros(), 0);
    }

    proptest! {
        /// 核心性质：报告通过 ⟺ 预留额不超过 max_admissible；刚好等于通过，多 1 微单位不通过。
        #[test]
        fn passes_exactly_when_reserve_is_within_max_admissible(
            balance in proptest::option::of(-1_000i64..1_000_000),
            ceiling in proptest::option::of(-1_000i64..1_000_000),
            credit in 0i64..10_000,
            spent in proptest::option::of((0i64..1_000_000, 0i64..2_000_000)),
            in_flight in proptest::option::of((0i64..1_000_000, 0i64..2_000_000)),
            pool in proptest::option::of(-1_000i64..1_000_000),
            reserve in 0i64..2_000_000,
        ) {
            let mut v = view("a");
            v.balance = balance.map(m);
            v.ceiling_remaining = ceiling.map(m);
            v.credit_limit = m(credit);
            if let Some((s, c)) = spent {
                v.rate_windows = vec![RateWindowView { window: RateWindowKind::Hour, spent: m(s), cap: m(c) }];
            }
            if let Some((cur, cap)) = in_flight {
                v.in_flight = Some(InFlightView { current: m(cur), cap: m(cap) });
            }
            v.subsidy_pool_remaining = pool.map(m);
            let chain = [v];
            let report = evaluate_admission(&price(reserve), &chain).unwrap();
            match report.max_admissible() {
                None => prop_assert!(report.passed()),
                Some(max) => {
                    let max = max.micros;
                    prop_assert_eq!(report.passed(), reserve <= max);
                    if max >= 0 {
                        // 边界：max 本身通过，max + 1 不通过
                        prop_assert!(evaluate_admission(&price(max), &chain).unwrap().passed());
                        prop_assert!(!evaluate_admission(&price(max + 1), &chain).unwrap().passed());
                    } else {
                        // 已经超限：连 0 都过不了
                        prop_assert!(!evaluate_admission(&price(0), &chain).unwrap().passed());
                    }
                }
            }
        }

        /// 链上多加一个账户只会让准入更严，不会更松。
        #[test]
        fn adding_an_ancestor_never_loosens_admission(
            balance in 0i64..1_000_000,
            extra_balance in 0i64..1_000_000,
            reserve in 0i64..1_000_000,
        ) {
            let mut a = view("a");
            a.balance = Some(m(balance));
            let mut b = view("b");
            b.balance = Some(m(extra_balance));
            let alone = evaluate_admission(&price(reserve), std::slice::from_ref(&a)).unwrap().passed();
            let both = evaluate_admission(&price(reserve), &[a, b]).unwrap().passed();
            prop_assert!(!both || alone);
        }
    }

    // ---------- 决定类型 ----------

    fn admitted() -> Admitted {
        Admitted {
            endpoint: EndpointKey::new("deepinfra", "org/model", "default").unwrap(),
            payer: aid("s1"),
            effective_max_tokens: 400,
            reservation: Reservation {
                price: price(588),
                cost: Cost::from_micros(Unit::Usd, 300).unwrap(),
            },
        }
    }

    #[test]
    fn decisions_map_to_the_four_tiers_and_reason_codes() {
        let allow = AdmissionDecision::Allow {
            admitted: admitted(),
        };
        assert_eq!(allow.tier(), Some(ExperienceTier::Normal));
        assert_eq!(allow.reason_code(), None);
        for (reason, code) in [
            (DegradeReason::Budget, ReasonCode::BudgetDegraded),
            (DegradeReason::Capacity, ReasonCode::CapacityDegraded),
            (
                DegradeReason::UpstreamFailover,
                ReasonCode::UpstreamFailover,
            ),
        ] {
            let d = AdmissionDecision::Degrade {
                admitted: admitted(),
                reason,
            };
            assert_eq!(d.tier(), Some(ExperienceTier::Degraded));
            assert_eq!(d.reason_code(), Some(code));
            assert_eq!(reason.reason_code(), code);
            assert!(d.is_admitted());
        }
        let grace = AdmissionDecision::Grace {
            admitted: admitted(),
        };
        assert_eq!(grace.tier(), Some(ExperienceTier::Grace));
        assert_eq!(grace.reason_code(), Some(ReasonCode::GraceUsed));
        assert!(grace.is_admitted());
        let exhausted = AdmissionDecision::reject(ReasonCode::BudgetExhausted).unwrap();
        assert_eq!(exhausted.tier(), Some(ExperienceTier::Exhausted));
        assert_eq!(exhausted.reason_code(), Some(ReasonCode::BudgetExhausted));
        assert!(!exhausted.is_admitted() && exhausted.admitted().is_none());
        // 不在阶梯上的拒绝：不是「钱不够」，不能说成第 4 级
        let limited = AdmissionDecision::reject(ReasonCode::RateLimited).unwrap();
        assert_eq!(limited.tier(), None);
        assert_eq!(limited.reason_code(), Some(ReasonCode::RateLimited));
        assert_eq!(allow.admitted().unwrap().effective_max_tokens, 400);
    }

    #[test]
    fn rejection_only_accepts_denial_codes() {
        for code in ReasonCode::ALL {
            let result = AdmissionDecision::reject(code);
            assert_eq!(result.is_ok(), !code.is_served(), "{code:?}");
            if code.is_served() {
                assert_eq!(result.unwrap_err(), AdmissionError::NotADenial(code));
            }
        }
        // 反序列化同样拒绝：`grace_used` 不能当拒绝原因
        assert!(
            serde_json::from_str::<AdmissionDecision>(
                r#"{"decision":"reject","reason":"grace_used"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<AdmissionDecision>(
                r#"{"decision":"reject","reason":"budget_exhausted"}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn check_kinds_map_to_denial_codes_that_are_denials() {
        for check in CheckKind::ALL {
            assert!(!check.denial_code().is_served(), "{check:?}");
        }
        assert_eq!(CheckKind::Funds.denial_code(), ReasonCode::BudgetExhausted);
        assert_eq!(
            CheckKind::CreditFloor.denial_code(),
            ReasonCode::BudgetExhausted
        );
        assert_eq!(CheckKind::RateGate.denial_code(), ReasonCode::RateLimited);
        assert_eq!(
            CheckKind::InFlightCap.denial_code(),
            ReasonCode::InFlightLimited
        );
        assert_eq!(
            CheckKind::SubsidyPoolCap.denial_code(),
            ReasonCode::SubsidyPoolExhausted
        );
    }

    // ---------- 宽限资格 ----------

    #[test]
    fn grace_eligibility_table() {
        // (已用次数, 可用额度, 预留, 期望)
        let cases: Vec<(u32, i64, i64, Result<i64, GraceDenied>)> = vec![
            (0, 30, 80, Ok(50)), // 差 50，不超过一次预留 80
            (0, 0, 80, Ok(80)),  // 可用额度刚好 0：缺口 = 一次预留，允许（边界）
            (
                0,
                -1,
                80,
                Err(GraceDenied::ExceedsOneReservation {
                    shortfall: m(81),
                    reserve: m(80),
                }),
            ),
            (0, 80, 80, Err(GraceDenied::NotNeeded)),
            (0, 100, 80, Err(GraceDenied::NotNeeded)),
            (1, 30, 80, Err(GraceDenied::AlreadyUsed { used: 1 })),
            (7, 30, 80, Err(GraceDenied::AlreadyUsed { used: 7 })),
            (1, 100, 80, Err(GraceDenied::NotNeeded)), // 用不着时，先报「用不着」，不是「用过了」
        ];
        for (used, available, reserve, expected) in cases {
            let got = check_grace(used, &m(available), &price(reserve));
            match expected {
                Ok(shortfall) => assert_eq!(
                    got.unwrap().shortfall.micros(),
                    shortfall,
                    "{used} {available} {reserve}"
                ),
                Err(e) => assert_eq!(got.unwrap_err(), e, "{used} {available} {reserve}"),
            }
        }
    }

    #[test]
    fn grace_never_looks_at_overage_and_rejects_other_units() {
        // 签名里没有 overage/settle 的任何输入：宽限资格只由「次数、可用额度、预留」决定。
        let usd = Money::new(Unit::Usd, 0);
        assert!(matches!(
            check_grace(0, &usd, &price(1)).unwrap_err(),
            GraceDenied::Money(MoneyError::UnitMismatch { .. })
        ));
        assert_eq!(MAX_GRACE_PER_PERIOD, 1);
        // 极端值不 panic
        assert!(matches!(
            check_grace(0, &m(i64::MIN), &price(1)).unwrap_err(),
            GraceDenied::Money(MoneyError::Overflow)
        ));
        let key = GraceKey {
            account: aid("s1"),
            period: "2026-11".parse().unwrap(),
        };
        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            r#"{"account":"s1","period":"2026-11"}"#
        );
    }
}
