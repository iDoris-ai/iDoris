//! 准入账本对外的输入输出类型。

use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{
    AccountId, AttemptId, BillingMonth, CredentialId, EndpointKey, PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard};
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest};

use super::error::LedgerError;

/// 保留的平台损失账户 id（只出现在分录里，不在账户树里，迁移里有触发器不许树里再建同名账户）。
pub const PLATFORM_LOSS_ACCOUNT: &str = "platform-loss";

/// 主要预留的 component 名。附加成本（比如安全检查）用别的 component，见 `reserve_component`。
pub const MODEL_COMPONENT: &str = "model";

/// 账本配置：存在库里的 `ledger_config`，S4 的结算钩子在另一条连接上也读同一份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerConfig {
    /// 账期（宽限按月计数、补贴池按月限额、额度窗口）用的 IANA 时区，默认 `UTC`。泰国首发用 `Asia/Bangkok`。
    pub billing_timezone: String,
    /// 预留的存活时间。必须长于一次请求可能持续的最长时间（含流式），否则预留会在上游还在计费时就过期。
    pub reservation_ttl_ms: i64,
}

/// 一次准入的输入（沿付款账户链原子地检查并占住预留）。
#[derive(Debug, Clone)]
pub struct AdmitInput<'a> {
    /// 这次尝试的 id（`<请求 id>#<序号>`）。同一个尝试只能准入一次。
    pub attempt: &'a AttemptId,
    /// 付款上下文：凭证、付款账户、单位、（可选的）主体与成员关系。
    pub payment: &'a PaymentContext,
    /// 要准入的候选。
    pub endpoint: &'a EndpointKey,
    /// 请求的 `max_tokens` 和输入估算。
    pub sizing: ReserveRequest,
    pub policy: &'a ReservePolicy,
    /// 这个候选对付款账户单位的价目、上游成本价目。
    pub price_rates: &'a RateCard<Price>,
    pub cost_rates: &'a RateCard<Cost>,
    /// 钱不够且压低 `max_tokens` 也放不下时，是否允许用本周期唯一的宽限。
    /// 调用方（云执行器）先用 `false` 把所有候选试一遍（压低 `max_tokens`、换更便宜的候选），都不行才用 `true` 再来一遍。
    pub allow_grace: bool,
    pub now: TimestampMs,
}

/// 预留的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HoldState {
    /// 占着额度，等结算或释放。
    Open,
    /// 已按实际金额结算。
    Settled,
    /// 已释放，没有收钱。
    Released,
    /// TTL 到了还没结算：额度已经放回去，但之后仍可能有迟到的结算。
    Expired,
}

impl HoldState {
    pub fn as_str(self) -> &'static str {
        match self {
            HoldState::Open => "open",
            HoldState::Settled => "settled",
            HoldState::Released => "released",
            HoldState::Expired => "expired",
        }
    }

    pub(super) fn parse(text: &str) -> Result<Self, LedgerError> {
        match text {
            "open" => Ok(HoldState::Open),
            "settled" => Ok(HoldState::Settled),
            "released" => Ok(HoldState::Released),
            "expired" => Ok(HoldState::Expired),
            other => Err(LedgerError::Corrupt(format!(
                "unknown hold state {other:?}"
            ))),
        }
    }

    /// 已经收尾（结算或释放），不会再变。
    pub fn is_final(self) -> bool {
        matches!(self, HoldState::Settled | HoldState::Released)
    }
}

/// 一个预留的读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldRow {
    pub hold_id: String,
    pub attempt: AttemptId,
    pub component: String,
    pub payer: AccountId,
    pub credential: CredentialId,
    pub reserved: Money,
    pub period: BillingMonth,
    pub created_at: TimestampMs,
    pub expires_at: TimestampMs,
    /// 只有宽限放行的预留有值：放行时「可用额度 + 授信」。
    pub grace_floor: Option<Money>,
    pub state: HoldState,
    pub expired_at: Option<TimestampMs>,
    pub closed_at: Option<TimestampMs>,
    pub charged: Option<Money>,
    /// 预留过期之后才来的结算。
    pub late: bool,
}

/// 一条分录的读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    pub entry_id: i64,
    pub account: String,
    pub hold: Option<String>,
    pub kind: String,
    pub unit: Unit,
    pub held: i64,
    pub spent: i64,
    pub funds: i64,
    pub liability: i64,
    pub window_at: TimestampMs,
    pub at: TimestampMs,
    pub idempotency_key: String,
}

/// 告警的种类。线上（库里）字符串固定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    /// 请求靠担保人的授信放行（有担保的负值），应通知担保人。
    CreditUsed,
    /// 用了本周期唯一的宽限，应通知担保人。
    GraceUsed,
    /// overage 超出授信的部分记入平台损失账户。
    PlatformLoss,
    /// 因为平台损失冻结了凭证。
    CredentialFrozen,
    /// 该冻结凭证，但冻结没有生效（凭证不在库里、已撤销或已冻结）。
    FreezeNotApplied,
    /// 预留过期之后才来的结算。
    LateSettlement,
    /// 请求收尾时还有没结算的附加成本预留，已按 0 释放。
    ExtraUnsettled,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::CreditUsed => "credit_used",
            AlertKind::GraceUsed => "grace_used",
            AlertKind::PlatformLoss => "platform_loss",
            AlertKind::CredentialFrozen => "credential_frozen",
            AlertKind::FreezeNotApplied => "freeze_not_applied",
            AlertKind::LateSettlement => "late_settlement",
            AlertKind::ExtraUnsettled => "extra_unsettled",
        }
    }
}

/// 一条告警记录的读视图（日志之外，可以查询）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRow {
    pub alert_id: i64,
    pub kind: String,
    pub account: String,
    pub guarantor: Option<String>,
    pub credential: Option<String>,
    pub hold: Option<String>,
    pub amount: Money,
    pub detail: String,
    pub at: TimestampMs,
}

/// 一次宽限的读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraceUseRow {
    pub account: AccountId,
    pub period: BillingMonth,
    pub hold: String,
    pub reserve: Money,
    /// 放行时没被可用额度覆盖的部分（不超过一次预留）。
    pub shortfall: Money,
    pub used_at: TimestampMs,
    /// 作废时间：宽限放行的预留最后没有收费（释放或结算为 0），这次宽限不算数。
    pub voided_at: Option<TimestampMs>,
}

/// 一次结算（或释放）的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleOutcome {
    pub result: SettleResult,
    /// 预留过期之后才来的结算（额度早已放回去）。
    pub late: bool,
    /// 最终向付款账户记的金额（释放为 0）。
    pub charged: Money,
    /// 超出预留的部分（迟到结算按全额算）。
    pub overage: Money,
    /// overage 里在担保人授信范围内、由担保人承担的部分。
    pub sponsor_covered: Money,
    /// overage 里超出授信、记入平台损失账户的部分。
    pub platform_loss: Money,
    /// 宽限那一轮超出授信下限、由平台承担的部分。
    pub grace_absorbed: Money,
    /// 因为平台损失冻结了这张凭证。
    pub credential_frozen: bool,
    /// 要在事务提交之后写日志的行（不含任何请求内容）。
    pub logs: Vec<String>,
}

/// 这一次调用做了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleResult {
    /// 这次调用把预留结算了（`charged > 0`）。
    Settled,
    /// 这次调用把预留释放了（没有收钱）。
    Released,
    /// 早已按同样的金额结算过（幂等重放，什么都没写）。
    AlreadySettled,
    /// 早已释放过（幂等重放，什么都没写）。
    AlreadyReleased,
}

/// 附加成本（比如安全检查）预留的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentDecision {
    /// 五项检查沿所有祖先通过，预留已经占住。
    Reserved { hold_id: String },
    /// 被拒绝（原因码是稳定字符串）：没有占任何额度。
    Rejected(idoris_compute_contracts::reason::ReasonCode),
}

/// 免费额度发放。
#[derive(Debug, Clone)]
pub struct FreeTierIssue<'a> {
    /// 成员（主体 id）。幂等键是 `subsidy:<主体>:<YYYY-MM>`。
    pub principal: &'a PrincipalId,
    /// 这个主体在补贴池之下的成员账户。
    pub member_account: &'a AccountId,
    pub month: BillingMonth,
    /// 这个月发放的额度，必须是正数、与成员账户同一单位。
    pub amount: Money,
    pub now: TimestampMs,
}

/// 免费额度发放的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreeTierOutcome {
    /// 新发放的额度。
    Granted(Allocation),
    /// 同一个 `(主体, 月份)` 早已发放过同样内容的额度；这是那一条（没有写任何东西）。
    Replayed(Allocation),
}

impl FreeTierOutcome {
    pub fn allocation(&self) -> &Allocation {
        match self {
            FreeTierOutcome::Granted(a) | FreeTierOutcome::Replayed(a) => a,
        }
    }
}

/// 充值（自有资金）的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundOutcome {
    Funded,
    /// 同一个幂等键早已充过同样的金额。
    Replayed,
}

/// 担保人给下级开的授信（有担保的负值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditLine {
    pub account: AccountId,
    /// 担保人 = 这个账户在树里的父账户。
    pub guarantor: AccountId,
    pub enabled: bool,
    /// 授信额度（非负）：账户最多透支到 `-limit`。关闭时为 0。
    pub limit: Money,
    pub valid_until: Option<TimestampMs>,
    pub version: i64,
}

/// 授信的设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditLineSetting {
    pub limit: Money,
    /// `None` = 直到被关掉。
    pub valid_until: Option<TimestampMs>,
}

/// 账户的速率闸门与在途上限（`None` = 不设限）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountLimits {
    pub inflight_cap: Option<Money>,
    pub minute_cap: Option<Money>,
    pub hour_cap: Option<Money>,
}

/// 对账里发现的一处不一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// 出问题的对象（尝试 id、预留 id……）。
    pub subject: String,
    pub detail: String,
}

/// 账本和 S4 尝试记录的对账结果。`mismatches` 为空 = 两边一致。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub mismatches: Vec<Mismatch>,
    /// 核对了多少个预留。
    pub holds_checked: usize,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }
}
