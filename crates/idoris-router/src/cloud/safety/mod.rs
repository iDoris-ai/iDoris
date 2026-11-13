//! idoris.ai/api 计算服务 **S6：儿童安全**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §7、§6 的 P0 范围（edu 一律非流式）、§12 的 S6 一行、§13 的 edu 验收；
//! 泰国 PDPA 对未成年人数据的结论见 `docs/design/compute-service-compliance-research.md`。服务对象是泰国的孩子：
//! 这一片任何一处放水，都可能把不当内容交给孩子，或把孩子的数据留在磁盘上，所以**安全优先于一切**，每一条规则都是失败即关闭。
//!
//! # 流程
//!
//! ```text
//!  edu 租户（TenantDataPolicy::handles_child_data()，来自服务端配置，请求改不了）
//!    │
//!    ├─ stream=true ─────────────────────────────▶ 400 streaming_not_allowed（handler 与执行器各查一次）
//!    ├─ 没有合规的主上游候选 ────────────────────▶ 503 no_compliant_endpoint（便宜的检查先做，不白花检查成本）
//!    ├─ 没有配置安全检查器 ──────────────────────▶ 503 safety_unavailable（not_configured）
//!    ▼
//!  ① 输入检查  extract_input：messages / tools / tool_choice / response_format / stop 里的**所有**字符串
//!    │          ExtraCostMeter::reserve（账本里预留 `safety_input`：LLM 检查的成本占付款账户链的额度；放不下 = 准入拒绝，不检查、不出网）
//!    │          规则检查器 ──拒绝──▶ 422 safety_input_blocked（不再调用 LLM）
//!    │            └─放行─▶ LLM 检查器 ──拒绝──▶ 422 safety_input_blocked
//!    │          超时 / 出错 / 返回垃圾 / 分类器不可用 ──▶ 503 safety_unavailable
//!    │          ──────────── 上游一个字节都没收到，用户付 0 ────────────
//!    ▼
//!  ② 上游非流式调用  （S4 的执行器：许可表硬约束、准入、尝试记录、首字节前切换）
//!    ▼
//!  ③ 完整输出检查  rebuild_response：只取 content / refusal / tool_calls，重建响应体（没检查的字段不发）
//!    │          ExtraCostMeter::reserve（账本里预留 `safety_output`，放不下 = 准入拒绝，回答照样不发）
//!    │          规则检查器（含「复述了孩子写的个人信息」）──▶ LLM 检查器
//!    │          拒绝 ──▶ 422 safety_output_blocked；没结论 ──▶ 503 safety_unavailable
//!    │          ── PendingAnswer::withhold：**客户端一个正文字节都收不到**，用户付 0，上游成本由平台承担 ──
//!    ▼
//!  ④ PendingAnswer::publish()  ← 只有输出检查放行后才可能被调用（clearance 守卫：没放行就拒绝发布）
//! ```
//!
//! 检查结果只有**放行**和**拒绝**两种（[`Verdict`]）。「拒绝」有两个来源：内容被拦（[`Denial::Blocked`]）和没能给出结论
//! （[`Denial::Unavailable`]：超时、出错、返回无法解析、分类器不可用、没有配置、文本太长……）。**没有「不确定但放行」。**
//!
//! # 模块
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`text`] | [`text::Sensitive`]（`Debug` 不打印正文）、归一化（大小写、全角、零宽、泰语声调、字母间插符号、数字代字母） |
//! | [`extract`] | 取出要检查的文本（输入）；为 edu 重建只含被检查字段的响应体（输出） |
//! | [`rules`] | 规则检查器：泰语 + 英语的关键词与模式，输出侧含「复述孩子个人信息」检测 |
//! | [`llm`] | LLM 判分检查器：调用可配置的 OpenAI 兼容端点；端点必须是许可表里允许 `child_edu` 的 |
//! | [`cost`] | 安全检查成本计入准入和结算的接口（[`ExtraCostMeter`]）与没有账本的简单实现 |
//! | [`ledger_cost`] | 接口的账本实现（S3）：输入 / 输出检查各预留一次，按检查端点的价目表折算，按实际用量结算 |
//! | [`config`] | 配置里的 `safety:` 段，以及用端点许可表校验 LLM 检查端点（不合规启动失败） |
//! | [`hardening`] | 进程级「不落盘」加固：panic 信息脱敏、关闭 core dump |
//!
//! # 租户属性，不是别名
//!
//! 是否启用这条管线只看 `call.tenant`（[`TenantDataPolicy`]）：它由服务端配置（今天是 `CallerBook`，S1/S2/S5 之后是账户存储）
//! 经虚拟 key 绑定的租户解析出来，**不看**请求体里的模型名、不看路由别名、不看任何请求头。管线在执行器里
//! （[`crate::cloud::CloudExecutor::execute`]），所以不论上层是哪个 HTTP 入口，只要走执行器，edu 就绕不过去。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{RequestId, TimestampMs};
use idoris_compute_contracts::permit::{Billing, TenantDataPolicy};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::safety::{
    SafetyCategory, SafetyReasonCode, SafetyStage, SafetyUnavailableCause,
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

pub mod config;
pub mod cost;
pub mod extract;
pub mod hardening;
pub mod ledger_cost;
pub mod llm;
pub mod rules;
pub mod text;

#[cfg(test)]
pub(crate) mod test_support;

pub use cost::{
    ExtraCostEntry, ExtraCostEstimate, ExtraCostGuard, ExtraCostMeter, ExtraCostOutcome,
    ExtraCostRefusal, ExtraCostRequest, ExtraCostTicket, SimpleExtraCost, SpendItem,
};
pub use extract::{CheckedInput, CheckedOutput, ExtractError, Origin, Segment, ToolCallText};
pub use ledger_cost::LedgerExtraCost;
pub use rules::RuleChecker;

use extract::{extract_input, rebuild_response};

/// 手写的盒装 future（不引入 `async-trait`）。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 为什么拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// 内容被拦。
    Blocked(SafetyCategory),
    /// 没能给出结论（超时、出错、返回无法解析、分类器不可用……）：失败即关闭。
    Unavailable(SafetyUnavailableCause),
}

/// 检查结果：只有放行和拒绝两种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(Denial),
}

impl Verdict {
    pub fn blocked(category: SafetyCategory) -> Self {
        Verdict::Deny(Denial::Blocked(category))
    }

    pub fn unavailable(cause: SafetyUnavailableCause) -> Self {
        Verdict::Deny(Denial::Unavailable(cause))
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Verdict::Allow)
    }
}

/// 一次检查的结果和花费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub verdict: Verdict,
    /// 这次检查实际花的钱（规则检查器没有）。
    pub spend: Vec<SpendItem>,
}

impl CheckReport {
    pub fn free(verdict: Verdict) -> Self {
        Self {
            verdict,
            spend: Vec::new(),
        }
    }
}

/// 检查器拿到的上下文。
pub struct CheckContext<'a> {
    pub request_id: &'a RequestId,
    pub tenant: &'a TenantDataPolicy,
    pub billing: Billing,
    pub now: TimestampMs,
    pub cancel: &'a CancellationToken,
    /// 这个阶段的截止时间：检查器里的任何网络调用都不能超过它。
    pub deadline: Instant,
}

impl std::fmt::Debug for CheckContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckContext")
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

/// 安全检查器：输入检查和完整输出检查。
///
/// 实现的约束（gate 会再兜一层，但实现自己也必须遵守）：
///
/// - **失败即关闭**：任何没能得出「放行」的情形都返回 `Deny`，绝不 panic、绝不「出错就放行」；
/// - **不记录、不打印正文**：日志、错误、Debug 里没有输入或输出的文本（用 [`text::Sensitive`]）；
/// - 网络调用必须遵守 `ctx.deadline`。
pub trait SafetyChecker: Send + Sync + std::fmt::Debug {
    /// 输入检查：用户消息、系统消息、工具返回的内容（见 [`extract`]）。
    fn check_input<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
    ) -> BoxFuture<'a, CheckReport>;

    /// 完整输出检查：完整的回答文本和工具调用参数。`input` 是对应的请求（用来发现「复述了孩子写的个人信息」）。
    fn check_output<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
        output: &'a CheckedOutput,
    ) -> BoxFuture<'a, CheckReport>;

    /// 这次检查最多花多少钱（上界）；免费的检查器返回 `None`。用于事前占额度，见 [`cost`]。
    fn estimate(&self, _stage: SafetyStage, _text_bytes: usize) -> Option<ExtraCostEstimate> {
        None
    }
}

/// 多个检查器串起来：按顺序检查，**任一拒绝即拒绝**，前一个拒绝后面的不再调用（规则先行，规则放行后再过 LLM）。
/// 构造时至少有一个检查器：空管线会放行一切，所以不存在这种东西。
#[derive(Debug, Clone)]
pub struct SafetyPipeline {
    first: Arc<dyn SafetyChecker>,
    rest: Vec<Arc<dyn SafetyChecker>>,
}

impl SafetyPipeline {
    pub fn new(first: Arc<dyn SafetyChecker>) -> Self {
        Self {
            first,
            rest: Vec::new(),
        }
    }

    /// 在末尾追加一个检查器（前面的都放行之后才会调用它）。
    pub fn then(mut self, next: Arc<dyn SafetyChecker>) -> Self {
        self.rest.push(next);
        self
    }

    fn checkers(&self) -> impl Iterator<Item = &Arc<dyn SafetyChecker>> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
}

impl SafetyChecker for SafetyPipeline {
    fn check_input<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
    ) -> BoxFuture<'a, CheckReport> {
        Box::pin(async move {
            let mut spend = Vec::new();
            for checker in self.checkers() {
                let report = checker.check_input(ctx, input).await;
                spend.extend(report.spend);
                if !report.verdict.is_allow() {
                    return CheckReport {
                        verdict: report.verdict,
                        spend,
                    };
                }
            }
            CheckReport {
                verdict: Verdict::Allow,
                spend,
            }
        })
    }

    fn check_output<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
        output: &'a CheckedOutput,
    ) -> BoxFuture<'a, CheckReport> {
        Box::pin(async move {
            let mut spend = Vec::new();
            for checker in self.checkers() {
                let report = checker.check_output(ctx, input, output).await;
                spend.extend(report.spend);
                if !report.verdict.is_allow() {
                    return CheckReport {
                        verdict: report.verdict,
                        spend,
                    };
                }
            }
            CheckReport {
                verdict: Verdict::Allow,
                spend,
            }
        })
    }

    fn estimate(&self, stage: SafetyStage, text_bytes: usize) -> Option<ExtraCostEstimate> {
        // 管线里至多一个会花钱的检查器（LLM）；有多个时只能占第一个的额度，所以配置层只允许一个
        self.checkers()
            .find_map(|checker| checker.estimate(stage, text_bytes))
    }
}

/// 检查的时间与大小上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafetyLimits {
    /// 一个阶段（输入检查或输出检查）的总预算：超过就是 `safety_unavailable`（失败即关闭）。
    pub check_timeout: Duration,
    /// 请求里最多检查多少字节的文本；更多的直接拒绝（`too_large`）。
    pub max_input_bytes: usize,
    /// 上游回答里最多检查多少字节；更多的直接拒绝（`too_large`）。
    pub max_output_bytes: usize,
}

impl SafetyLimits {
    pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
    pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
}

impl Default for SafetyLimits {
    fn default() -> Self {
        Self {
            check_timeout: Self::DEFAULT_CHECK_TIMEOUT,
            max_input_bytes: Self::DEFAULT_MAX_BYTES,
            max_output_bytes: Self::DEFAULT_MAX_BYTES,
        }
    }
}

/// 一次调用里 gate 需要的信息（都来自服务端：付款上下文和租户数据策略不是请求给的）。
pub struct SafetyCall<'a> {
    pub request_id: &'a RequestId,
    pub payment: &'a PaymentContext,
    pub tenant: &'a TenantDataPolicy,
    pub billing: Billing,
    pub now: TimestampMs,
    pub cancel: &'a CancellationToken,
}

/// gate 的拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDenial {
    Blocked {
        stage: SafetyStage,
        category: SafetyCategory,
    },
    Unavailable {
        stage: SafetyStage,
        cause: SafetyUnavailableCause,
    },
    /// 安全检查的成本没通过准入（付款账户链放不下）：准入类的原因码。
    Admission(ReasonCode),
    /// 客户端断开。
    Cancelled,
}

impl GateDenial {
    /// 稳定的原因字符串（写进请求记录）。**不含类别**：类别是从孩子的话里推出来的敏感推断，不进账本。
    pub fn close_reason(&self) -> &'static str {
        match self {
            GateDenial::Blocked { stage, .. } => SafetyReasonCode::blocked_at(*stage).as_str(),
            GateDenial::Unavailable { .. } => SafetyReasonCode::SafetyUnavailable.as_str(),
            GateDenial::Admission(code) => code.as_str(),
            GateDenial::Cancelled => "client_cancelled",
        }
    }
}

/// 安全 gate：把检查器、时间预算、成本计量和失败即关闭的规则装在一起，给执行器用。
#[derive(Clone)]
pub struct SafetyGate {
    checker: Arc<dyn SafetyChecker>,
    limits: SafetyLimits,
    cost: Arc<dyn ExtraCostMeter>,
}

impl std::fmt::Debug for SafetyGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafetyGate")
            .field("checker", &self.checker)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// 一个阶段跑完的结果（内部）。
enum StageOutcome {
    Cancelled,
    TimedOut,
    Panicked,
    Report(CheckReport),
}

impl SafetyGate {
    pub fn new(
        checker: Arc<dyn SafetyChecker>,
        limits: SafetyLimits,
        cost: Arc<dyn ExtraCostMeter>,
    ) -> Self {
        Self {
            checker,
            limits,
            cost,
        }
    }

    /// 只有规则检查器的 gate（没有 LLM，没有成本）。
    pub fn rules_only() -> Self {
        Self::new(
            Arc::new(RuleChecker::new()),
            SafetyLimits::default(),
            Arc::new(SimpleExtraCost::new()),
        )
    }

    /// 换掉安全检查成本的计量器：生产装配换成读写账本的 [`LedgerExtraCost`]（`CloudUpstream::with_ledger_extra_cost`）。
    pub fn with_cost(mut self, cost: Arc<dyn ExtraCostMeter>) -> Self {
        self.cost = cost;
        self
    }

    pub fn limits(&self) -> &SafetyLimits {
        &self.limits
    }

    /// 输入检查：取出要检查的文本，检查，返回可供输出检查用的 [`CheckedInput`]。
    pub async fn check_input(
        &self,
        call: &SafetyCall<'_>,
        body: &Map<String, Value>,
    ) -> Result<CheckedInput, GateDenial> {
        let stage = SafetyStage::Input;
        let input = match extract_input(body, self.limits.max_input_bytes) {
            Ok(input) => input,
            Err(ExtractError::TooLarge) => {
                return Err(self.deny(
                    call,
                    GateDenial::Unavailable {
                        stage,
                        cause: SafetyUnavailableCause::TooLarge,
                    },
                ));
            }
            Err(ExtractError::Malformed) => {
                return Err(self.deny(
                    call,
                    GateDenial::Unavailable {
                        stage,
                        cause: SafetyUnavailableCause::Malformed,
                    },
                ));
            }
        };
        if input.has_uncheckable_content {
            // 图片、音频、文件……：规则和文本模型都检查不了，失败即关闭
            return Err(self.deny(
                call,
                GateDenial::Blocked {
                    stage,
                    category: SafetyCategory::UnsupportedContent,
                },
            ));
        }
        self.run_stage(call, stage, &input, None).await?;
        Ok(input)
    }

    /// 完整输出检查：只取会被检查的字段重建响应体，检查通过才返回**可以发给客户端的字节**。
    pub async fn check_output(
        &self,
        call: &SafetyCall<'_>,
        input: &CheckedInput,
        upstream_body: &[u8],
    ) -> Result<Vec<u8>, GateDenial> {
        let stage = SafetyStage::Output;
        let rebuilt = match rebuild_response(
            upstream_body,
            call.request_id.as_str(),
            self.limits.max_output_bytes,
        ) {
            Ok(rebuilt) => rebuilt,
            Err(ExtractError::TooLarge) => {
                return Err(self.deny(
                    call,
                    GateDenial::Unavailable {
                        stage,
                        cause: SafetyUnavailableCause::TooLarge,
                    },
                ));
            }
            Err(ExtractError::Malformed) => {
                return Err(self.deny(
                    call,
                    GateDenial::Unavailable {
                        stage,
                        cause: SafetyUnavailableCause::Malformed,
                    },
                ));
            }
        };
        self.run_stage(call, stage, input, Some(&rebuilt.output))
            .await?;
        Ok(rebuilt.body)
    }

    /// 跑一个阶段：占成本额度 -> 带时间预算和取消地调用检查器 -> 结算 -> 把结果翻译成放行或拒绝。
    async fn run_stage(
        &self,
        call: &SafetyCall<'_>,
        stage: SafetyStage,
        input: &CheckedInput,
        output: Option<&CheckedOutput>,
    ) -> Result<(), GateDenial> {
        let text_bytes = match output {
            None => input.total_bytes(),
            Some(output) => output
                .total_bytes()
                .saturating_add(input.last_user_text().map_or(0, str::len)),
        };
        // ---- 准入：这次检查要花钱，先占额度。放不下就不检查、不调用上游 ----
        let estimate = self.checker.estimate(stage, text_bytes);
        let guard = match &estimate {
            None => None,
            Some(estimate) => match self.cost.reserve(&ExtraCostRequest {
                request_id: call.request_id,
                payment: call.payment,
                stage,
                estimate,
                now: call.now,
            }) {
                Ok(ticket) => Some(ExtraCostGuard::new(&*self.cost, ticket)),
                Err(ExtraCostRefusal::Denied(code)) => {
                    return Err(self.deny(call, GateDenial::Admission(code)));
                }
                Err(ExtraCostRefusal::Unavailable) => {
                    return Err(self.deny(
                        call,
                        GateDenial::Unavailable {
                            stage,
                            cause: SafetyUnavailableCause::Internal,
                        },
                    ));
                }
            },
        };

        // ---- 检查：时间预算、客户端取消、检查器 panic 都按失败即关闭处理 ----
        let ctx = CheckContext {
            request_id: call.request_id,
            tenant: call.tenant,
            billing: call.billing,
            now: call.now,
            cancel: call.cancel,
            deadline: Instant::now() + self.limits.check_timeout,
        };
        let outcome = {
            let check = async {
                match output {
                    None => self.checker.check_input(&ctx, input).await,
                    Some(output) => self.checker.check_output(&ctx, input, output).await,
                }
            };
            let guarded = std::panic::AssertUnwindSafe(check).catch_unwind();
            tokio::select! {
                biased;
                () = call.cancel.cancelled() => StageOutcome::Cancelled,
                result = tokio::time::timeout(self.limits.check_timeout, guarded) => match result {
                    Err(_elapsed) => StageOutcome::TimedOut,
                    Ok(Err(_panic)) => StageOutcome::Panicked,
                    Ok(Ok(report)) => StageOutcome::Report(report),
                },
            }
        };

        // ---- 结算 + 翻译 ----
        match outcome {
            StageOutcome::Cancelled => {
                if let Some(guard) = guard {
                    guard.finish(ExtraCostOutcome::Unknown);
                }
                Err(GateDenial::Cancelled)
            }
            StageOutcome::TimedOut => {
                if let Some(guard) = guard {
                    guard.finish(ExtraCostOutcome::Unknown);
                }
                Err(self.deny(
                    call,
                    GateDenial::Unavailable {
                        stage,
                        cause: SafetyUnavailableCause::Timeout,
                    },
                ))
            }
            StageOutcome::Panicked => {
                if let Some(guard) = guard {
                    guard.finish(ExtraCostOutcome::Unknown);
                }
                Err(self.deny(
                    call,
                    GateDenial::Unavailable {
                        stage,
                        cause: SafetyUnavailableCause::Internal,
                    },
                ))
            }
            StageOutcome::Report(report) => {
                if let Some(guard) = guard {
                    // 给出了结论（放行或拦截）：按这次检查报告的花费结算。没能给出结论（超时、出错、返回垃圾、分类器
                    // 不可用……）：检查可能已经调用了上游，花了多少不知道，这是 `Unknown`，不是「花了估算的那么多」——
                    // 账本实现对前者按实际用量向付款账户收费，对后者不收（故障的安全服务不该向孩子的账户收钱）。
                    let outcome = match report.verdict {
                        Verdict::Deny(Denial::Unavailable(_)) => ExtraCostOutcome::Unknown,
                        Verdict::Allow | Verdict::Deny(Denial::Blocked(_)) => {
                            ExtraCostOutcome::Spent(report.spend)
                        }
                    };
                    guard.finish(outcome);
                }
                match report.verdict {
                    Verdict::Allow => Ok(()),
                    Verdict::Deny(Denial::Blocked(category)) => {
                        Err(self.deny(call, GateDenial::Blocked { stage, category }))
                    }
                    Verdict::Deny(Denial::Unavailable(cause)) => {
                        Err(self.deny(call, GateDenial::Unavailable { stage, cause }))
                    }
                }
            }
        }
    }

    /// 记一行日志（只有请求号、阶段和稳定的码，**没有类别、没有任何内容**），原样返回。
    fn deny(&self, call: &SafetyCall<'_>, denial: GateDenial) -> GateDenial {
        match denial {
            GateDenial::Blocked { stage, .. } => eprintln!(
                "[idoris] safety: request {} withheld at the {stage} stage: {}",
                call.request_id,
                denial.close_reason()
            ),
            GateDenial::Unavailable { stage, cause } => eprintln!(
                "[idoris] safety: request {} withheld at the {stage} stage: {} ({cause})",
                call.request_id,
                denial.close_reason()
            ),
            GateDenial::Admission(_) => eprintln!(
                "[idoris] safety: request {} refused by admission of the safety-check cost: {}",
                call.request_id,
                denial.close_reason()
            ),
            GateDenial::Cancelled => {}
        }
        denial
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::test_support::{child_tenant, payment_context, request_id};
    use super::*;

    /// 可编排的检查器：按脚本返回结果，可以睡、可以 panic，记录被调用的次数。
    #[derive(Debug)]
    struct Scripted {
        input: Verdict,
        output: Verdict,
        delay: Duration,
        panic: bool,
        calls: AtomicUsize,
    }

    impl Scripted {
        fn new(input: Verdict, output: Verdict) -> Arc<Self> {
            Arc::new(Self {
                input,
                output,
                delay: Duration::ZERO,
                panic: false,
                calls: AtomicUsize::new(0),
            })
        }

        fn allow() -> Arc<Self> {
            Self::new(Verdict::Allow, Verdict::Allow)
        }
    }

    impl SafetyChecker for Scripted {
        fn check_input<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
        ) -> BoxFuture<'a, CheckReport> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.panic {
                    panic!("scripted checker panic");
                }
                tokio::time::sleep(self.delay).await;
                CheckReport::free(self.input)
            })
        }

        fn check_output<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
            _output: &'a CheckedOutput,
        ) -> BoxFuture<'a, CheckReport> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.panic {
                    panic!("scripted checker panic");
                }
                tokio::time::sleep(self.delay).await;
                CheckReport::free(self.output)
            })
        }
    }

    fn gate(checker: Arc<dyn SafetyChecker>) -> SafetyGate {
        SafetyGate::new(
            checker,
            SafetyLimits {
                check_timeout: Duration::from_millis(200),
                ..SafetyLimits::default()
            },
            Arc::new(SimpleExtraCost::new()),
        )
    }

    fn request_body() -> Map<String, Value> {
        json!({"messages": [{"role": "user", "content": "what is 2+2?"}]})
            .as_object()
            .unwrap()
            .clone()
    }

    fn upstream_body() -> Vec<u8> {
        json!({"id": "x", "choices": [{"message": {"role": "assistant", "content": "four"}}]})
            .to_string()
            .into_bytes()
    }

    async fn run<T>(
        gate: &SafetyGate,
        f: impl for<'a> FnOnce(&'a SafetyGate, &'a SafetyCall<'a>) -> BoxFuture<'a, T>,
    ) -> T {
        let id = request_id("req-1");
        let payment = payment_context();
        let tenant = child_tenant();
        let cancel = CancellationToken::new();
        let call = SafetyCall {
            request_id: &id,
            payment: &payment,
            tenant: &tenant,
            billing: Billing::Paid,
            now: TimestampMs(0),
            cancel: &cancel,
        };
        f(gate, &call).await
    }

    #[tokio::test]
    async fn allow_denies_and_unavailable_map_to_the_gate_result() {
        let allow = gate(Scripted::allow());
        let input = run(&allow, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap();
        let rebuilt = run(&allow, |g, c| {
            let input = input.clone();
            Box::pin(async move { g.check_output(c, &input, &upstream_body()).await })
        })
        .await
        .unwrap();
        assert!(String::from_utf8(rebuilt).unwrap().contains("four"));

        let blocked = gate(Scripted::new(
            Verdict::blocked(SafetyCategory::SelfHarm),
            Verdict::blocked(SafetyCategory::AdultContent),
        ));
        assert_eq!(
            run(&blocked, |g, c| Box::pin(async move {
                g.check_input(c, &request_body()).await
            }))
            .await
            .unwrap_err(),
            GateDenial::Blocked {
                stage: SafetyStage::Input,
                category: SafetyCategory::SelfHarm
            }
        );
        let input = CheckedInput::default();
        assert_eq!(
            run(&blocked, |g, c| {
                let input = input.clone();
                Box::pin(async move { g.check_output(c, &input, &upstream_body()).await })
            })
            .await
            .unwrap_err(),
            GateDenial::Blocked {
                stage: SafetyStage::Output,
                category: SafetyCategory::AdultContent
            }
        );

        let unavailable = gate(Scripted::new(
            Verdict::unavailable(SafetyUnavailableCause::BadResponse),
            Verdict::Allow,
        ));
        assert_eq!(
            run(&unavailable, |g, c| Box::pin(async move {
                g.check_input(c, &request_body()).await
            }))
            .await
            .unwrap_err(),
            GateDenial::Unavailable {
                stage: SafetyStage::Input,
                cause: SafetyUnavailableCause::BadResponse
            }
        );
    }

    #[tokio::test]
    async fn a_slow_checker_is_a_timeout_not_an_allow() {
        let slow = Arc::new(Scripted {
            input: Verdict::Allow,
            output: Verdict::Allow,
            delay: Duration::from_secs(5),
            panic: false,
            calls: AtomicUsize::new(0),
        });
        let gate = gate(slow.clone());
        let started = Instant::now();
        let denial = run(&gate, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Unavailable {
                stage: SafetyStage::Input,
                cause: SafetyUnavailableCause::Timeout
            }
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the budget is enforced from outside the checker"
        );
        // 输出阶段同样
        let denial = run(&gate, |g, c| {
            Box::pin(async move {
                g.check_output(c, &CheckedInput::default(), &upstream_body())
                    .await
            })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Unavailable {
                stage: SafetyStage::Output,
                cause: SafetyUnavailableCause::Timeout
            }
        );
    }

    #[tokio::test]
    async fn a_panicking_checker_fails_closed_instead_of_dropping_the_connection_or_allowing() {
        let panicking = Arc::new(Scripted {
            input: Verdict::Allow,
            output: Verdict::Allow,
            delay: Duration::ZERO,
            panic: true,
            calls: AtomicUsize::new(0),
        });
        let denial = run(&gate(panicking), |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Unavailable {
                stage: SafetyStage::Input,
                cause: SafetyUnavailableCause::Internal
            }
        );
    }

    #[tokio::test]
    async fn uncheckable_malformed_and_oversized_requests_never_reach_the_checker() {
        let checker = Scripted::allow();
        let gate = SafetyGate::new(
            checker.clone(),
            SafetyLimits {
                max_input_bytes: 64,
                ..SafetyLimits::default()
            },
            Arc::new(SimpleExtraCost::new()),
        );
        let bodies = [
            (
                json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "u"}}]}]}),
                GateDenial::Blocked {
                    stage: SafetyStage::Input,
                    category: SafetyCategory::UnsupportedContent,
                },
            ),
            (
                json!({"messages": "nope"}),
                GateDenial::Unavailable {
                    stage: SafetyStage::Input,
                    cause: SafetyUnavailableCause::Malformed,
                },
            ),
            (
                json!({"messages": [{"role": "user", "content": "x".repeat(65)}]}),
                GateDenial::Unavailable {
                    stage: SafetyStage::Input,
                    cause: SafetyUnavailableCause::TooLarge,
                },
            ),
        ];
        for (body, expected) in bodies {
            let body = body.as_object().unwrap().clone();
            let denial = run(&gate, |g, c| {
                Box::pin(async move { g.check_input(c, &body).await })
            })
            .await
            .unwrap_err();
            assert_eq!(denial, expected);
        }
        assert_eq!(checker.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unparseable_upstream_answer_is_withheld() {
        let gate = gate(Scripted::allow());
        for bad in [&b"not json"[..], b"[]", br#"{"choices":[]}"#] {
            let denial = run(&gate, |g, c| {
                Box::pin(async move { g.check_output(c, &CheckedInput::default(), bad).await })
            })
            .await
            .unwrap_err();
            assert_eq!(
                denial,
                GateDenial::Unavailable {
                    stage: SafetyStage::Output,
                    cause: SafetyUnavailableCause::Malformed
                }
            );
        }
    }

    #[tokio::test]
    async fn a_cancelled_call_is_cancelled() {
        let gate = gate(Scripted::allow());
        let id = request_id("req-1");
        let payment = payment_context();
        let tenant = child_tenant();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let call = SafetyCall {
            request_id: &id,
            payment: &payment,
            tenant: &tenant,
            billing: Billing::Paid,
            now: TimestampMs(0),
            cancel: &cancel,
        };
        assert_eq!(
            gate.check_input(&call, &request_body()).await.unwrap_err(),
            GateDenial::Cancelled
        );
    }

    #[tokio::test]
    async fn a_pipeline_stops_at_the_first_refusal_and_needs_every_checker_to_allow() {
        let first = Scripted::allow();
        let second = Scripted::new(
            Verdict::blocked(SafetyCategory::Jailbreak),
            Verdict::blocked(SafetyCategory::Jailbreak),
        );
        let third = Scripted::allow();
        let pipeline = SafetyPipeline::new(first.clone())
            .then(second.clone())
            .then(third.clone());
        let gate = gate(Arc::new(pipeline));
        let denial = run(&gate, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Blocked {
                stage: SafetyStage::Input,
                category: SafetyCategory::Jailbreak
            }
        );
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            third.calls.load(Ordering::SeqCst),
            0,
            "a refusal stops the chain: later checkers (the paid ones) are not called"
        );
    }

    #[test]
    fn close_reasons_are_stable_and_carry_no_category() {
        use SafetyStage::{Input, Output};
        for category in SafetyCategory::ALL {
            assert_eq!(
                GateDenial::Blocked {
                    stage: Input,
                    category
                }
                .close_reason(),
                "safety_input_blocked"
            );
            assert_eq!(
                GateDenial::Blocked {
                    stage: Output,
                    category
                }
                .close_reason(),
                "safety_output_blocked"
            );
        }
        for cause in SafetyUnavailableCause::ALL {
            assert_eq!(
                GateDenial::Unavailable {
                    stage: Input,
                    cause
                }
                .close_reason(),
                "safety_unavailable"
            );
        }
        assert_eq!(
            GateDenial::Admission(ReasonCode::BudgetExhausted).close_reason(),
            "budget_exhausted"
        );
        assert_eq!(GateDenial::Cancelled.close_reason(), "client_cancelled");
    }

    /// 花钱的检查器：事前占额度、事后结算；额度被拒时不调用检查器。
    #[derive(Debug)]
    struct Paid {
        calls: AtomicUsize,
    }

    impl SafetyChecker for Paid {
        fn check_input<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
        ) -> BoxFuture<'a, CheckReport> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                CheckReport {
                    verdict: Verdict::Allow,
                    spend: vec![test_support_spend(40)],
                }
            })
        }

        fn check_output<'a>(
            &'a self,
            ctx: &'a CheckContext<'a>,
            input: &'a CheckedInput,
            _output: &'a CheckedOutput,
        ) -> BoxFuture<'a, CheckReport> {
            self.check_input(ctx, input)
        }

        fn estimate(&self, _stage: SafetyStage, _bytes: usize) -> Option<ExtraCostEstimate> {
            Some(ExtraCostEstimate {
                endpoint: test_support::endpoint_key(),
                max_cost: idoris_compute_contracts::pricing::Cost::from_micros(
                    idoris_compute_contracts::money::Unit::Usd,
                    100,
                )
                .unwrap(),
                max_input_tokens: 100,
                max_output_tokens: 10,
            })
        }
    }

    fn test_support_spend(micros: i64) -> SpendItem {
        SpendItem {
            endpoint: test_support::endpoint_key(),
            cost: idoris_compute_contracts::pricing::Cost::from_micros(
                idoris_compute_contracts::money::Unit::Usd,
                micros,
            )
            .unwrap(),
            usage: None,
            estimated: false,
        }
    }

    /// 拒绝额度的计量器。
    #[derive(Debug)]
    struct Refusing(ExtraCostRefusal);

    impl ExtraCostMeter for Refusing {
        fn reserve(
            &self,
            _request: &ExtraCostRequest<'_>,
        ) -> Result<ExtraCostTicket, ExtraCostRefusal> {
            Err(self.0)
        }

        fn settle(&self, _ticket: ExtraCostTicket, _outcome: ExtraCostOutcome) {
            panic!("nothing was reserved, so nothing may be settled");
        }
    }

    #[tokio::test]
    async fn the_check_cost_is_reserved_before_and_settled_after_the_check() {
        let meter = Arc::new(SimpleExtraCost::new());
        let paid = Arc::new(Paid {
            calls: AtomicUsize::new(0),
        });
        let gate = SafetyGate::new(paid.clone(), SafetyLimits::default(), meter.clone());
        run(&gate, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap();
        let entries = meter.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].stage, SafetyStage::Input);
        assert_eq!(entries[0].reserved.micros(), 100);
        match &entries[0].outcome {
            ExtraCostOutcome::Spent(items) => assert_eq!(items[0].cost.micros(), 40),
            other => panic!("{other:?}"),
        }
        assert_eq!(meter.open_tickets(), 0);
    }

    #[tokio::test]
    async fn an_admission_refusal_means_no_check_and_no_upstream_call() {
        let paid = Arc::new(Paid {
            calls: AtomicUsize::new(0),
        });
        let denied = SafetyGate::new(
            paid.clone(),
            SafetyLimits::default(),
            Arc::new(Refusing(ExtraCostRefusal::Denied(
                ReasonCode::BudgetExhausted,
            ))),
        );
        let denial = run(&denied, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(denial, GateDenial::Admission(ReasonCode::BudgetExhausted));
        // 账本出错：失败即关闭
        let broken = SafetyGate::new(
            paid.clone(),
            SafetyLimits::default(),
            Arc::new(Refusing(ExtraCostRefusal::Unavailable)),
        );
        let denial = run(&broken, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Unavailable {
                stage: SafetyStage::Input,
                cause: SafetyUnavailableCause::Internal
            }
        );
        assert_eq!(paid.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_free_rules_run_before_the_check_cost_is_reserved() {
        // PR-Daemon S5 F4：配了 LLM 检查器时，检查费用在免费的规则检查之前预留，额度不足的孩子写自伤内容得到 402，不是 422。
        let paid = Arc::new(Paid {
            calls: AtomicUsize::new(0),
        });
        let pipeline = SafetyPipeline::new(Arc::new(RuleChecker::new())).then(paid.clone());
        let broke = SafetyGate::new(
            Arc::new(pipeline),
            SafetyLimits::default(),
            Arc::new(Refusing(ExtraCostRefusal::Denied(
                ReasonCode::BudgetExhausted,
            ))),
        );
        let harmful = json!({"messages": [{"role": "user", "content": "i want to die"}]})
            .as_object()
            .unwrap()
            .clone();
        let denial = run(&broke, |g, c| {
            let harmful = harmful.clone();
            Box::pin(async move { g.check_input(c, &harmful).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Blocked {
                stage: SafetyStage::Input,
                category: SafetyCategory::SelfHarm
            },
            "the rules refuse first; the child's account having no money must not turn that into a 402"
        );
        // 输出阶段同样：有害的回答被规则拦下，不是因为钱不够
        let harmful_answer = json!({"id": "x", "choices": [{"message": {
            "role": "assistant", "content": "ไปตายซะ ไม่มีใครต้องการเธอหรอก"}}]})
        .to_string()
        .into_bytes();
        let denial = run(&broke, |g, c| {
            let harmful_answer = harmful_answer.clone();
            Box::pin(async move {
                g.check_output(c, &CheckedInput::default(), &harmful_answer)
                    .await
            })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Blocked {
                stage: SafetyStage::Output,
                category: SafetyCategory::SelfHarm
            }
        );
        // 没有被规则拦下的请求照常走到预留：钱不够就是准入拒绝，付费检查器一次也没被调用
        let denial = run(&broke, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(denial, GateDenial::Admission(ReasonCode::BudgetExhausted));
        assert_eq!(paid.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_timed_out_paid_check_is_settled_as_unknown_not_forgotten() {
        #[derive(Debug)]
        struct Hanging;
        impl SafetyChecker for Hanging {
            fn check_input<'a>(
                &'a self,
                _ctx: &'a CheckContext<'a>,
                _input: &'a CheckedInput,
            ) -> BoxFuture<'a, CheckReport> {
                Box::pin(std::future::pending())
            }
            fn check_output<'a>(
                &'a self,
                _ctx: &'a CheckContext<'a>,
                _input: &'a CheckedInput,
                _output: &'a CheckedOutput,
            ) -> BoxFuture<'a, CheckReport> {
                Box::pin(std::future::pending())
            }
            fn estimate(&self, _stage: SafetyStage, _bytes: usize) -> Option<ExtraCostEstimate> {
                Some(ExtraCostEstimate {
                    endpoint: test_support::endpoint_key(),
                    max_cost: idoris_compute_contracts::pricing::Cost::from_micros(
                        idoris_compute_contracts::money::Unit::Usd,
                        100,
                    )
                    .unwrap(),
                    max_input_tokens: 100,
                    max_output_tokens: 10,
                })
            }
        }
        let meter = Arc::new(SimpleExtraCost::new());
        let gate = SafetyGate::new(
            Arc::new(Hanging),
            SafetyLimits {
                check_timeout: Duration::from_millis(50),
                ..SafetyLimits::default()
            },
            meter.clone(),
        );
        let denial = run(&gate, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert!(matches!(
            denial,
            GateDenial::Unavailable {
                cause: SafetyUnavailableCause::Timeout,
                ..
            }
        ));
        assert_eq!(meter.open_tickets(), 0);
        assert_eq!(meter.entries()[0].outcome, ExtraCostOutcome::Unknown);
    }
}
