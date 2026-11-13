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
//!    ├─ 只用规则检查器的显式确认到期了 ───────────▶ 503 safety_unavailable（no_compliant_checker）
//!    │          （没有 LLM 检查器的装配带着确认的到期日，输入检查和输出检查每次都按当前时钟核对，不是只在启动时）
//!    ▼
//!  ① 输入检查  extract_input_with：**转发的请求**（`build_chat_body` 构造的那份）里客户端写的**每一个**字符串，值和键名都取；
//!    │          客户端写的一切（含 system / developer 角色）都按孩子写的话，只有服务端登记的运营方提示得到运营方的待遇
//!    │          免费的规则预检 ──拒绝──▶ 422 safety_input_blocked（**先于**下面的预留：额度不足的孩子写了自伤内容拿到 422，不是 402）
//!    │          SafetyChecker::plan（先生成最终要发出的每一个调用，分块已定；内容分不进允许的块数 = 503 too_large，不预留）
//!    │          ExtraCostMeter::reserve（账本里预留 `safety_input`：按计划里**全部**调用算的上界占付款账户链的额度；放不下 = 准入拒绝，不检查、不出网）
//!    │          规则检查器 ──拒绝──▶ 422 safety_input_blocked（不再调用 LLM）
//!    │            └─放行─▶ LLM 检查器 ──拒绝──▶ 422 safety_input_blocked
//!    │          超时 / 出错 / 返回垃圾 / 分类器不可用 ──▶ 503 safety_unavailable
//!    │          ──────────── 上游一个字节都没收到，用户付 0 ────────────
//!    ▼
//!  ② 上游非流式调用  （S4 的执行器：许可表硬约束、准入、尝试记录、首字节前切换）
//!    ▼
//!  ③ 完整输出检查  rebuild_response：只取 content / refusal / tool_calls，重建响应体（没检查的字段不发；
//!    │          响应 id / model / 工具调用 id 由服务端写，不取上游的；工具参数解析成 JSON，检查解码后的键和值，解析不了就拒绝）
//!    │          免费的规则预检 ──拒绝──▶ 422 safety_output_blocked（先于预留）
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
//! | [`text`] | [`text::Sensitive`]（`Debug` 不打印正文）、归一化（大小写、全角、数学字母、带圈字母、西里尔 / 希腊同形字、组合符号、零宽、泰语声调、字母间插符号、数字代字母、重复字母）、base64 解码、「否定词只在命中旁边有效」 |
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
pub use extract::{
    CheckedInput, CheckedOutput, ExtractError, Origin, ResponseIdentity, Segment, ToolCallText,
    TrustedOperatorText,
};
pub use ledger_cost::LedgerExtraCost;
pub use rules::RuleChecker;

use config::RulesOnlyAcknowledgement;
use extract::{extract_input_with, rebuild_response};

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

/// 一次检查的**最终计划**（round2-r3 N3）。
///
/// 花钱的检查（LLM）要先占额度、再发调用。旧做法是按原始文本的字节估一个调用数去占额度，检查器自己再把文本展开、分块、发出去：
/// 展开（工具参数的解码视图）和分块（贪心装箱，每块都带着上下文和包装）会让真正发出的调用比估算多，超出预留的部分由平台承担。
/// 现在顺序反过来：**先生成最终要发出的每一个调用的内容（分块已经定了），再按这些调用算全部的花费上界去预留，最后发送的
/// 就是这同一份**：[`SafetyChecker::plan`] -> [`SafetyGate`] 按 [`CheckPlan::estimate`] 预留 -> [`SafetyChecker::execute`]
/// 逐个发送计划里的调用。预留和发送不会各算各的。
#[derive(Clone)]
pub struct CheckPlan {
    kind: PlanKind,
}

#[derive(Clone)]
enum PlanKind {
    /// 免费的检查（规则检查器，或者这次没有任何要检查的文本）：没有调用要预留。
    Free,
    /// 花钱的检查。`estimate` 是按计划里**全部**调用算出的花费上界；`calls` 是要发出的每一个调用的最终内容
    /// （检查器自己的计划；`None` = 旧接口 [`SafetyChecker::estimate`] 只给了上界，没有调用列表）。
    Paid {
        estimate: ExtraCostEstimate,
        calls: Option<Arc<Vec<text::Sensitive>>>,
    },
    /// 这是一次花钱的检查，但没法进行（内容分不进允许的块数、成本算不出来）：不花钱、不放行，gate 按这个原因拒绝。
    Refused(SafetyUnavailableCause),
}

impl CheckPlan {
    /// 免费的检查：没有调用要预留。
    pub fn free() -> Self {
        Self {
            kind: PlanKind::Free,
        }
    }

    /// 花钱的检查，只有上界、没有自己的调用列表（旧接口 [`SafetyChecker::estimate`] 的结果）。
    pub fn paid(estimate: ExtraCostEstimate) -> Self {
        Self {
            kind: PlanKind::Paid {
                estimate,
                calls: None,
            },
        }
    }

    /// 花钱的检查：`calls` 是要发出的每一个调用的最终内容，`estimate` 必须是按它们算出的全部调用的花费上界。
    pub fn paid_calls(calls: Vec<String>, estimate: ExtraCostEstimate) -> Self {
        Self {
            kind: PlanKind::Paid {
                estimate,
                calls: Some(Arc::new(
                    calls.into_iter().map(text::Sensitive::new).collect(),
                )),
            },
        }
    }

    /// 花钱的检查没法进行：不花钱、不放行。
    pub fn refused(cause: SafetyUnavailableCause) -> Self {
        Self {
            kind: PlanKind::Refused(cause),
        }
    }

    /// 是免费的检查（没有调用要预留）吗？
    pub fn is_free(&self) -> bool {
        matches!(self.kind, PlanKind::Free)
    }

    /// 要按它占额度的上界；免费的、或者没法进行的计划没有。
    pub fn estimate(&self) -> Option<&ExtraCostEstimate> {
        match &self.kind {
            PlanKind::Paid { estimate, .. } => Some(estimate),
            PlanKind::Free | PlanKind::Refused(_) => None,
        }
    }

    /// 没法进行的原因（内容分不进允许的块数……）。
    pub fn refusal(&self) -> Option<SafetyUnavailableCause> {
        match &self.kind {
            PlanKind::Refused(cause) => Some(*cause),
            PlanKind::Free | PlanKind::Paid { .. } => None,
        }
    }

    /// 要发出的每一个调用的最终内容（检查器自己的计划）。
    pub fn calls(&self) -> Option<&[text::Sensitive]> {
        match &self.kind {
            PlanKind::Paid { calls, .. } => calls.as_deref().map(Vec::as_slice),
            PlanKind::Free | PlanKind::Refused(_) => None,
        }
    }
}

impl std::fmt::Debug for CheckPlan {
    // 只有个数和 token 上界，没有调用的内容
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PlanKind::Free => f.write_str("CheckPlan::Free"),
            PlanKind::Paid { estimate, calls } => f
                .debug_struct("CheckPlan::Paid")
                .field("calls", &calls.as_ref().map(|calls| calls.len()))
                .field("max_input_tokens", &estimate.max_input_tokens)
                .field("max_output_tokens", &estimate.max_output_tokens)
                .finish_non_exhaustive(),
            PlanKind::Refused(cause) => write!(f, "CheckPlan::Refused({cause})"),
        }
    }
}

/// 旧接口 [`SafetyChecker::estimate`] 要的文本字节数：输入检查是请求里要检查的全部文本；输出检查是回答里的文本加上孩子最后一句话（上下文）。
fn legacy_text_bytes(input: &CheckedInput, output: Option<&CheckedOutput>) -> usize {
    match output {
        None => input.total_bytes(),
        Some(output) => output
            .total_bytes()
            .saturating_add(input.last_user_text().map_or(0, str::len)),
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
    ///
    /// 这是**按原始文本字节数估**的旧接口，只给没有自己调用列表的检查器用（[`SafetyChecker::plan`] 的默认实现用它）。
    /// 会真的发出多次调用的检查器（LLM）必须覆盖 [`SafetyChecker::plan`]：估算和发送要共用同一份调用列表。
    fn estimate(&self, _stage: SafetyStage, _text_bytes: usize) -> Option<ExtraCostEstimate> {
        None
    }

    /// **检查之前**：定下这次检查的最终计划（见 [`CheckPlan`]）。gate 按计划里的上界占额度，再把**同一份**计划交给
    /// [`SafetyChecker::execute`]。`output` 为 `None` 是输入检查。
    ///
    /// 默认：没有自己的调用列表，按旧接口 [`SafetyChecker::estimate`] 的上界（有就是花钱的检查，没有就是免费的）。
    /// 会花钱的检查器（LLM）必须覆盖它：先生成最终要发出的每个调用，再按它们算全部调用的上界。
    fn plan(
        &self,
        stage: SafetyStage,
        input: &CheckedInput,
        output: Option<&CheckedOutput>,
    ) -> CheckPlan {
        match self.estimate(stage, legacy_text_bytes(input, output)) {
            Some(estimate) => CheckPlan::paid(estimate),
            None => CheckPlan::free(),
        }
    }

    /// **按计划**检查（`output` 为 `None` 是输入检查）：gate 用的入口，`plan` 就是预留时用的那一份。
    ///
    /// 默认忽略计划，直接调用 [`SafetyChecker::check_input`] / [`SafetyChecker::check_output`]（免费的检查器、没有自己的
    /// 调用列表的检查器）。有自己的调用列表的检查器（LLM）覆盖它：发送的就是计划里的调用，不再重新展开、分块。
    fn execute<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
        output: Option<&'a CheckedOutput>,
        plan: &'a CheckPlan,
    ) -> BoxFuture<'a, CheckReport> {
        let _ = plan;
        match output {
            None => self.check_input(ctx, input),
            Some(output) => self.check_output(ctx, input, output),
        }
    }

    /// **免费的预检**（输入）：gate 在**占检查费用的额度之前**调用它。返回 `Some` 就是已经能拒绝了，gate 直接拒绝，
    /// 不占额度、不花钱。默认没有预检（返回 `None`）；规则检查器实现它。
    ///
    /// 为什么要有它：配了花钱的检查器（LLM）时，如果先占额度再跑免费的规则，额度不足的孩子写了自伤内容得到的是 402（没钱），
    /// 不是 422 `self_harm`，eduai 就收不到升级信号（PR-Daemon S5 F4）。
    /// 实现必须只做免费、确定、不出网的检查；返回 `None` 不代表放行，放行与否由后面的完整检查决定。
    fn precheck_input(&self, _input: &CheckedInput) -> Option<Denial> {
        None
    }

    /// 免费的预检（输出），见 [`SafetyChecker::precheck_input`]。
    fn precheck_output(&self, _input: &CheckedInput, _output: &CheckedOutput) -> Option<Denial> {
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

    /// 按计划把每个检查器跑一遍：任一拒绝即拒绝，后面的不再调用。每个检查器拿到的是**同一份**计划，
    /// 用不用由它自己决定（免费的检查器不用，LLM 检查器发送计划里的调用）。
    async fn run_all(
        &self,
        ctx: &CheckContext<'_>,
        input: &CheckedInput,
        output: Option<&CheckedOutput>,
        plan: &CheckPlan,
    ) -> CheckReport {
        let mut spend = Vec::new();
        for checker in self.checkers() {
            let report = checker.execute(ctx, input, output, plan).await;
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
    }
}

impl SafetyChecker for SafetyPipeline {
    fn check_input<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
    ) -> BoxFuture<'a, CheckReport> {
        // 单独调用（没有 gate 给的计划）：自己定一份计划再按它跑
        Box::pin(async move {
            let plan = self.plan(SafetyStage::Input, input, None);
            self.run_all(ctx, input, None, &plan).await
        })
    }

    fn check_output<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
        output: &'a CheckedOutput,
    ) -> BoxFuture<'a, CheckReport> {
        Box::pin(async move {
            let plan = self.plan(SafetyStage::Output, input, Some(output));
            self.run_all(ctx, input, Some(output), &plan).await
        })
    }

    fn estimate(&self, stage: SafetyStage, text_bytes: usize) -> Option<ExtraCostEstimate> {
        // 管线里至多一个会花钱的检查器（LLM）；有多个时只能占第一个的额度，所以配置层只允许一个
        self.checkers()
            .find_map(|checker| checker.estimate(stage, text_bytes))
    }

    fn plan(
        &self,
        stage: SafetyStage,
        input: &CheckedInput,
        output: Option<&CheckedOutput>,
    ) -> CheckPlan {
        // 管线里至多一个会花钱的检查器（LLM）：取第一个不是免费的计划（免费的检查器的计划都是空的）
        self.checkers()
            .map(|checker| checker.plan(stage, input, output))
            .find(|plan| !plan.is_free())
            .unwrap_or_else(CheckPlan::free)
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a CheckContext<'a>,
        input: &'a CheckedInput,
        output: Option<&'a CheckedOutput>,
        plan: &'a CheckPlan,
    ) -> BoxFuture<'a, CheckReport> {
        Box::pin(async move { self.run_all(ctx, input, output, plan).await })
    }

    fn precheck_input(&self, input: &CheckedInput) -> Option<Denial> {
        self.checkers()
            .find_map(|checker| checker.precheck_input(input))
    }

    fn precheck_output(&self, input: &CheckedInput, output: &CheckedOutput) -> Option<Denial> {
        self.checkers()
            .find_map(|checker| checker.precheck_output(input, output))
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
    /// 服务端配置里登记的受信运营方提示。默认没有：客户端写的内容一律按孩子写的话检查。
    trusted: TrustedOperatorText,
    /// 「只用规则检查器」的显式确认（`safety.rules_only_acknowledgement`，owner 2026-11-13 方案 B）。**只有没有 LLM 检查器的
    /// 装配才带着它**：确认有到期日，而启动时核对一次不够（进程一直跑下去，期限就成了摆设），所以 gate 留着它，
    /// 每次儿童请求的输入检查和输出检查都按当次的时钟再核对；过期了就在调用任何检查器之前拒绝（round2-r2 / r3 N1）。
    /// 默认没有：测试装配、有 LLM 检查器的装配不受期限约束。
    rules_only_acknowledgement: Option<RulesOnlyAcknowledgement>,
}

impl std::fmt::Debug for SafetyGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafetyGate")
            .field("checker", &self.checker)
            .field("limits", &self.limits)
            .field("trusted_operator_prompts", &self.trusted.len())
            .field("rules_only_expires_on", &self.rules_only_expires_on())
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
            trusted: TrustedOperatorText::none(),
            rules_only_acknowledgement: None,
        }
    }

    /// 装上服务端登记的受信运营方提示（`safety.trusted_operator_prompts`）：`system` / `developer` 消息的内容以它开头的那一段
    /// 按运营方的待遇检查（只查越狱类规则），其余一律按孩子写的话。**客户端填的角色、凭证的种类、请求头都不能代替这份配置。**
    pub fn with_trusted_operator_text(mut self, trusted: TrustedOperatorText) -> Self {
        self.trusted = trusted;
        self
    }

    /// 带上「只用规则检查器」的显式确认：到期之后（按每次请求的 `SafetyCall::now`，UTC 日历日，到期日当天含）这个 gate 对儿童请求
    /// 一律返回 `safety_unavailable`（`no_compliant_checker`），不调用任何检查器。只给**没有 LLM 检查器**的装配用
    /// （[`config::SafetySetup::into_gate`] 保证）。
    pub fn with_rules_only_acknowledgement(
        mut self,
        acknowledgement: RulesOnlyAcknowledgement,
    ) -> Self {
        self.rules_only_acknowledgement = Some(acknowledgement);
        self
    }

    /// 这个 gate 带着的确认的到期日（`YYYY-MM-DD`，UTC）；没有带确认就是 `None`。
    pub fn rules_only_expires_on(&self) -> Option<&str> {
        self.rules_only_acknowledgement
            .as_ref()
            .map(|acknowledgement| acknowledgement.expires_on.as_str())
    }

    /// 确认在 `call.now` 这一刻还有效吗？没有带确认的 gate 不受限。过期（或者时钟读数不可信：超出可表示范围、
    /// 离到期日超过 90 天，和启动时是同一套判断）就是拒绝：**选用 `NoCompliantChecker`，不新增线上字符串**——
    /// 它的含义「没有满足硬约束的检查器」正是这里的情形，和检查端点的核验在运行中过期是同一类（时间到了，原来合规的检查器不再合规）；
    /// `NotConfigured` 说的是「根本没配置检查器」，运营方会去查错地方。
    fn ensure_acknowledgement_current(
        &self,
        call: &SafetyCall<'_>,
        stage: SafetyStage,
    ) -> Result<(), GateDenial> {
        let Some(acknowledgement) = &self.rules_only_acknowledgement else {
            return Ok(());
        };
        if acknowledgement.ensure_current(call.now).is_ok() {
            return Ok(());
        }
        // 只有请求号、阶段和到期日（配置里的日期，不是内容）
        eprintln!(
            "[idoris] safety: request {} refused at the {stage} stage: the rules_only acknowledgement (expires_on={}) is no longer valid and no LLM checker is configured; child-data requests stay refused until safety.llm_judge is configured or the acknowledgement is renewed",
            call.request_id, acknowledgement.expires_on
        );
        Err(GateDenial::Unavailable {
            stage,
            cause: SafetyUnavailableCause::NoCompliantChecker,
        })
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
        // 最先做：确认过期了，儿童请求在取文本、调用任何检查器、也在任何上游调用之前就被拒绝
        self.ensure_acknowledgement_current(call, stage)?;
        let input = match extract_input_with(body, self.limits.max_input_bytes, &self.trusted) {
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
    ///
    /// `response_model` 是已选路由的模型（作答的那次尝试的端点）：响应的 `model` 是它，响应的 `id` 是请求号，
    /// 工具调用的 `id` 从请求号派生——**都不取上游写的**（上游能把孩子写的电话号码回显成一个合法的「短标识」）。
    pub async fn check_output(
        &self,
        call: &SafetyCall<'_>,
        input: &CheckedInput,
        upstream_body: &[u8],
        response_model: &str,
    ) -> Result<Vec<u8>, GateDenial> {
        let stage = SafetyStage::Output;
        // 上游调用可能花了很久（请求跨过了到期日）：输出检查也用**当前**的时钟核对一次，过期了回答不发
        self.ensure_acknowledgement_current(call, stage)?;
        let identity = ResponseIdentity {
            id: call.request_id.as_str(),
            model: response_model,
        };
        let rebuilt = match rebuild_response(upstream_body, &identity, self.limits.max_output_bytes)
        {
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
        // ---- 先定计划：最终要发出的每一个调用（分块已经定了），和按它们算出的**全部**调用的花费上界 ----
        // 预留和发送共用这一份（round2-r3 N3）：下面按 `plan.estimate()` 占额度，最后把同一份计划交给检查器去发。
        // 不再是「按原始文本的字节估一个调用数去占额度，检查器自己再展开、分块、发出去」——展开（工具参数解码）和分块
        // （贪心装箱、每块都带上下文和包装）会让真正发出的调用比估算多，超出预留的部分由平台承担。
        let plan = self.checker.plan(stage, input, output);
        // ---- 免费的预检在前：规则命中就直接拒绝，不占检查费用的额度 ----
        // 只有这次检查要花钱时才值得单独做一遍（免费的检查器在后面的完整检查里本来就先跑）。
        // 额度不足的孩子写了自伤内容，得到的应该是 422 `self_harm`（eduai 要靠它升级），不是 402（PR-Daemon S5 F4）。
        if !plan.is_free() {
            let precheck = match output {
                None => self.checker.precheck_input(input),
                Some(output) => self.checker.precheck_output(input, output),
            };
            match precheck {
                Some(Denial::Blocked(category)) => {
                    return Err(self.deny(call, GateDenial::Blocked { stage, category }));
                }
                Some(Denial::Unavailable(cause)) => {
                    return Err(self.deny(call, GateDenial::Unavailable { stage, cause }));
                }
                None => {}
            }
        }
        // ---- 这次花钱的检查没法进行（内容分不进允许的块数、成本算不出来）：不占额度、不出网、不放行 ----
        if let Some(cause) = plan.refusal() {
            return Err(self.deny(call, GateDenial::Unavailable { stage, cause }));
        }
        // ---- 准入：这次检查要花钱，先占额度。放不下就不检查、不调用上游 ----
        let guard = match plan.estimate() {
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
            // 发送的就是预留时的那一份计划（调用写在 async 块里：检查器同步地 panic 也被 `catch_unwind` 接住）
            let check = async { self.checker.execute(&ctx, input, output, &plan).await };
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
            Box::pin(async move {
                g.check_output(c, &input, &upstream_body(), "route-model")
                    .await
            })
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
                Box::pin(async move {
                    g.check_output(c, &input, &upstream_body(), "route-model")
                        .await
                })
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
                g.check_output(c, &CheckedInput::default(), &upstream_body(), "route-model")
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
                Box::pin(async move {
                    g.check_output(c, &CheckedInput::default(), bad, "route-model")
                        .await
                })
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

    /// 有自己调用列表的付费检查器：计划里是三个调用，`execute` 记下它拿到的计划；gate 不该绕过计划去调 `check_input` / `check_output`。
    #[derive(Debug, Default)]
    struct Planned {
        /// 每次 `execute` 看到的计划里的调用个数。
        executed: std::sync::Mutex<Vec<usize>>,
        legacy_calls: AtomicUsize,
    }

    impl Planned {
        fn estimate() -> ExtraCostEstimate {
            ExtraCostEstimate {
                endpoint: test_support::endpoint_key(),
                // 三个调用的上界之和（和单个调用的上界不一样）
                max_cost: idoris_compute_contracts::pricing::Cost::from_micros(
                    idoris_compute_contracts::money::Unit::Usd,
                    300,
                )
                .unwrap(),
                max_input_tokens: 1_950,
                max_output_tokens: 96,
            }
        }
    }

    impl SafetyChecker for Planned {
        fn check_input<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
        ) -> BoxFuture<'a, CheckReport> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { CheckReport::free(Verdict::Allow) })
        }

        fn check_output<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
            _output: &'a CheckedOutput,
        ) -> BoxFuture<'a, CheckReport> {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { CheckReport::free(Verdict::Allow) })
        }

        fn plan(
            &self,
            _stage: SafetyStage,
            _input: &CheckedInput,
            _output: Option<&CheckedOutput>,
        ) -> CheckPlan {
            CheckPlan::paid_calls(
                vec!["call-1".into(), "call-2".into(), "call-3".into()],
                Self::estimate(),
            )
        }

        fn execute<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
            _output: Option<&'a CheckedOutput>,
            plan: &'a CheckPlan,
        ) -> BoxFuture<'a, CheckReport> {
            let calls = plan.calls().map_or(0, <[_]>::len);
            self.executed.lock().unwrap().push(calls);
            Box::pin(async {
                CheckReport {
                    verdict: Verdict::Allow,
                    spend: vec![test_support_spend(40)],
                }
            })
        }
    }

    #[tokio::test]
    async fn the_reservation_comes_from_the_plan_and_the_plan_is_what_gets_executed() {
        // round2-r3 N3：先定计划（最终要发出的全部调用 + 按它们算的全部上界），按计划预留，发送的是同一份计划
        let checker = Arc::new(Planned::default());
        let meter = Arc::new(SimpleExtraCost::new());
        let gate = SafetyGate::new(checker.clone(), SafetyLimits::default(), meter.clone());
        run(&gate, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap();
        run(&gate, |g, c| {
            Box::pin(async move {
                g.check_output(c, &CheckedInput::default(), &upstream_body(), "route-model")
                    .await
            })
        })
        .await
        .unwrap();
        let entries = meter.entries();
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_eq!(
                entry.reserved,
                Planned::estimate().max_cost,
                "{:?} reserves the plan's upper bound for all of its calls",
                entry.stage
            );
        }
        assert_eq!(
            *checker.executed.lock().unwrap(),
            vec![3, 3],
            "the checker executed the same three-call plan that was reserved"
        );
        assert_eq!(
            checker.legacy_calls.load(Ordering::SeqCst),
            0,
            "the gate goes through the plan, not around it"
        );
    }

    /// 付费、但内容分不进允许的块数：计划是拒绝；任何一个检查接口都不该被调用。
    #[derive(Debug)]
    struct TooBig;

    impl SafetyChecker for TooBig {
        fn check_input<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
        ) -> BoxFuture<'a, CheckReport> {
            panic!("a refused plan must not be executed");
        }

        fn check_output<'a>(
            &'a self,
            _ctx: &'a CheckContext<'a>,
            _input: &'a CheckedInput,
            _output: &'a CheckedOutput,
        ) -> BoxFuture<'a, CheckReport> {
            panic!("a refused plan must not be executed");
        }

        fn plan(
            &self,
            _stage: SafetyStage,
            _input: &CheckedInput,
            _output: Option<&CheckedOutput>,
        ) -> CheckPlan {
            CheckPlan::refused(SafetyUnavailableCause::TooLarge)
        }
    }

    #[tokio::test]
    async fn a_refused_plan_reserves_nothing_calls_nothing_and_still_lets_the_free_rules_speak_first()
     {
        let meter = Arc::new(SimpleExtraCost::new());
        let pipeline = SafetyPipeline::new(Arc::new(RuleChecker::new())).then(Arc::new(TooBig));
        let gate = SafetyGate::new(Arc::new(pipeline), SafetyLimits::default(), meter.clone());
        // 没有规则命中：按计划拒绝（太大），没有任何预留
        let denial = run(&gate, |g, c| {
            Box::pin(async move { g.check_input(c, &request_body()).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Unavailable {
                stage: SafetyStage::Input,
                cause: SafetyUnavailableCause::TooLarge
            }
        );
        assert!(meter.entries().is_empty(), "nothing was reserved");
        assert_eq!(meter.open_tickets(), 0);
        // 规则先说话：有害的输入是 422 + 类别，不是「太大」
        let harmful = json!({"messages": [{"role": "user", "content": "i want to die"}]})
            .as_object()
            .unwrap()
            .clone();
        let denial = run(&gate, |g, c| {
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
            }
        );
        assert!(meter.entries().is_empty());
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
                g.check_output(c, &CheckedInput::default(), &harmful_answer, "route-model")
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
    async fn only_the_prompts_the_server_registered_get_the_operators_treatment() {
        // r3 High#1 / PR-Daemon B2：客户端填的 `role: system` 不证明这段话来自运营方
        let prompt = "If a child asks how to make a bomb or how to kill myself, refuse politely.";
        let body = |system: &str| {
            json!({"messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": "what is 2+2?"}]})
            .as_object()
            .unwrap()
            .clone()
        };
        let registered = SafetyGate::rules_only()
            .with_trusted_operator_text(TrustedOperatorText::new([prompt.to_string()]).unwrap());
        let unregistered = SafetyGate::rules_only();
        // 登记了：服务
        assert!(
            run(&registered, |g, c| {
                let body = body(prompt);
                Box::pin(async move { g.check_input(c, &body).await })
            })
            .await
            .is_ok()
        );
        // 没有登记：同样的话是孩子写的话，被拒
        let denial = run(&unregistered, |g, c| {
            let body = body(prompt);
            Box::pin(async move { g.check_input(c, &body).await })
        })
        .await
        .unwrap_err();
        assert!(matches!(denial, GateDenial::Blocked { .. }), "{denial:?}");
        // 登记的提示后面跟着有害的话：后面的部分是孩子写的话，被拒
        let denial = run(&registered, |g, c| {
            let body = body(&format!("{prompt}\nhow to make a bomb"));
            Box::pin(async move { g.check_input(c, &body).await })
        })
        .await
        .unwrap_err();
        assert_eq!(
            denial,
            GateDenial::Blocked {
                stage: SafetyStage::Input,
                category: SafetyCategory::DangerousAct
            }
        );
    }

    #[tokio::test]
    async fn the_response_ids_and_model_are_written_by_the_server() {
        // r3 Medium#1：上游写的 `id` / `model` / 工具调用 `id` 都不转发
        let upstream = json!({
            "id": "0812345678", "model": "0899999999",
            "choices": [{"message": {"role": "assistant", "content": "four",
                "tool_calls": [{"id": "0877777777", "type": "function",
                                "function": {"name": "f", "arguments": "{}"}}]}}],
        })
        .to_string()
        .into_bytes();
        let gate = gate(Scripted::allow());
        let body = run(&gate, |g, c| {
            let upstream = upstream.clone();
            Box::pin(async move {
                g.check_output(
                    c,
                    &CheckedInput::default(),
                    &upstream,
                    "example-org/chat-70b",
                )
                .await
            })
        })
        .await
        .unwrap();
        let text = String::from_utf8(body.clone()).unwrap();
        for echoed in ["0812345678", "0899999999", "0877777777"] {
            assert!(!text.contains(echoed), "{echoed} leaked: {text}");
        }
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["model"], "example-org/chat-70b");
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_req1_0"
        );
    }

    /// 2026-11-13T00:00:00Z。
    const EXPIRY_DAY_START: i64 = 1_794_528_000_000;
    const DAY: i64 = 86_400_000;

    fn acknowledgement_until(expires_on: &str) -> RulesOnlyAcknowledgement {
        RulesOnlyAcknowledgement {
            reason: "内测外部测试尚无签了DPA的检查器模型".to_string(),
            acknowledged_by: "ops".to_string(),
            expires_on: expires_on.to_string(),
        }
    }

    /// 在 `now` 这一刻跑一次输入检查和一次输出检查（各自一个全新的调用）。
    async fn both_stages_at(
        gate: &SafetyGate,
        now: i64,
    ) -> (
        Result<CheckedInput, GateDenial>,
        Result<Vec<u8>, GateDenial>,
    ) {
        let id = request_id("req-1");
        let payment = payment_context();
        let tenant = child_tenant();
        let cancel = CancellationToken::new();
        let call = SafetyCall {
            request_id: &id,
            payment: &payment,
            tenant: &tenant,
            billing: Billing::Paid,
            now: TimestampMs(now),
            cancel: &cancel,
        };
        let input = gate.check_input(&call, &request_body()).await;
        let output = gate
            .check_output(
                &call,
                &CheckedInput::default(),
                &upstream_body(),
                "route-model",
            )
            .await;
        (input, output)
    }

    #[tokio::test]
    async fn an_expired_rules_only_acknowledgement_refuses_every_child_request_before_any_check_runs()
     {
        // round2-r2 / r3 N1：启动时有效的确认，进程一直跑下去，期限不能变成摆设
        let checker = Scripted::allow();
        let gate = gate(checker.clone())
            .with_rules_only_acknowledgement(acknowledgement_until("2026-11-13"));
        assert_eq!(gate.rules_only_expires_on(), Some("2026-11-13"));
        let denied = |stage| {
            Err::<(), _>(GateDenial::Unavailable {
                stage,
                cause: SafetyUnavailableCause::NoCompliantChecker,
            })
        };
        for (now, served) in [
            // 到期日的前一天、当天零点、当天最后一毫秒（到期日含）：照常
            (EXPIRY_DAY_START - DAY, true),
            (EXPIRY_DAY_START, true),
            (EXPIRY_DAY_START + DAY - 1, true),
            // 跨过 UTC 午夜：到期
            (EXPIRY_DAY_START + DAY, false),
            (EXPIRY_DAY_START + 2 * DAY, false),
            // 长期运行：几个月之后、几年之后
            (EXPIRY_DAY_START + 120 * DAY, false),
            (EXPIRY_DAY_START + 1_000 * DAY, false),
            // 时钟读数不可信（回到 1970、离到期日远超 90 天）：和启动时同一套判断，失败即关闭
            (0, false),
            (-DAY, false),
        ] {
            let before = checker.calls.load(Ordering::SeqCst);
            let (input, output) = both_stages_at(&gate, now).await;
            if served {
                assert!(
                    input.is_ok() && output.is_ok(),
                    "now={now}: {input:?} {output:?}"
                );
                assert_eq!(
                    checker.calls.load(Ordering::SeqCst),
                    before + 2,
                    "now={now}"
                );
            } else {
                assert_eq!(input.map(|_| ()), denied(SafetyStage::Input), "now={now}");
                assert_eq!(output.map(|_| ()), denied(SafetyStage::Output), "now={now}");
                assert_eq!(
                    checker.calls.load(Ordering::SeqCst),
                    before,
                    "now={now}: no checker was called for an expired acknowledgement"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_gate_without_an_acknowledgement_has_no_expiry() {
        // 有 LLM 检查器的装配、测试装配：没有确认就没有期限，什么时候都照常（没有「悄悄变成永久拒绝」的情形）
        let checker = Scripted::allow();
        let gate = gate(checker.clone());
        assert_eq!(gate.rules_only_expires_on(), None);
        for now in [0, EXPIRY_DAY_START, EXPIRY_DAY_START + 5_000 * DAY] {
            let (input, output) = both_stages_at(&gate, now).await;
            assert!(input.is_ok() && output.is_ok(), "now={now}");
        }
    }

    #[test]
    fn the_acknowledgement_survives_the_clone_that_swaps_in_the_cost_meter() {
        // 生产装配先建 gate，再 `with_extra_cost_meter` 克隆一份换计量器：期限不能在这一步丢掉
        let gate = SafetyGate::rules_only()
            .with_rules_only_acknowledgement(acknowledgement_until("2026-12-31"));
        let swapped = gate.clone().with_cost(Arc::new(SimpleExtraCost::new()));
        assert_eq!(swapped.rules_only_expires_on(), Some("2026-12-31"));
        assert!(format!("{swapped:?}").contains("2026-12-31"));
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
