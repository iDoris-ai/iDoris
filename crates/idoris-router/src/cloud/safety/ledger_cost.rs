//! 安全检查成本的**账本实现**（S6 接 S3）：[`LedgerExtraCost`] 把 [`ExtraCostMeter`] 接到账户存储里的准入账本
//! （`idoris_tenancy::accounts::ledger`），让安全检查的钱和回答本身的钱一样：事前占额度、事后按实际用量结算。
//!
//! # 对应关系
//!
//! | 阶段 | 账本调用 | 预留的身份 |
//! |---|---|---|
//! | 输入检查（**任何尝试之前**） | `reserve_request_component(请求, "safety_input", 付款上下文, 预留额, now)` | `<请求 id>#0~safety_input` |
//! | 完整输出检查（回答已经出来、还没发布） | 同上，component 是 `"safety_output"` | `<请求 id>#0~safety_output` |
//! | 检查之后 | `settle_component(请求#0, component, 实际价, now)`（实际价为 0 就是释放） | 同上 |
//!
//! 两个阶段都用 `reserve_request_component`、挂在这个请求的 `#0` 名下：它只依赖付款上下文，不依赖某一次尝试的主要预留
//! 是否还开着（输出检查时作答的可能是 `#1`，主要预留的状态也不是这里该关心的）。**每个 (请求, 阶段) 只预留一次**：
//! 账本对重复预留返回 `HoldExists`，这里按失败即关闭处理（不检查、不出网），**不会**悄悄复用已有的预留。
//!
//! # 预留额怎么算：用检查端点的价目表折算
//!
//! 检查器给的上界是**上游成本**（`Cost`，供应商币种，比如 USD）。账本要的是付款账户单位的**价**（`Price`，比如 THB），
//! 两者不能直接换算。规则：检查器同时给出这个上界用到的 token 数（[`ExtraCostEstimate::max_input_tokens`] /
//! [`ExtraCostEstimate::max_output_tokens`]），这里用**检查端点配置里的 `prices`**（付款单位的价目表，和聊天端点是同一份配置）
//! 按同样的 token 数算：`amount_for_tokens(价目表[付款单位], 输入 token 上界, 输出 token 上界)`，**向上取整**
//! （[`USAGE_ROUNDING`]，宁可多占 1 微单位）。检查端点没有付款单位的价目 = 没法计价，失败即关闭（启动时的配置校验会先拒绝
//! 这种配置，见 `cloud::config`）。
//!
//! 结算同一张价目表：每次调用的实际 `Usage` 用 `usage_amount(价目表[付款单位], usage)`，各次相加，**封顶在预留额**。
//! 封顶的原因：检查模型的 tokenizer 和我们的字节估算对不上时，实际可能略超预留；账本对超出预留的部分有 overage 规则
//! （担保人承担 / 平台损失 / **冻结凭证**），不能让孩子的凭证因为检查模型的估算误差被冻结，超出的部分由平台承担。
//!
//! # 谁付钱（对设计稿的一处理解，见 `cloud/mod.rs` 的含糊点清单）
//!
//! | 检查的结局 | 向付款账户记多少 |
//! |---|---|
//! | 给出了结论（放行或拦截），用量已知 | 实际价（封顶在预留额） |
//! | 给出了结论，但用量未知 / 算不出价 | 预留额（和 S4 对「用量未知」的规则一样：按预留价） |
//! | 规则检查器就拦下了，没有调用 LLM（`Spent([])`） | 0（释放） |
//! | 没能给出结论：超时、出错、返回垃圾、客户端断开、future 被丢弃（`Unknown`）；或者检查根本没跑（`NotRun`） | 0（释放），由平台承担 |
//!
//! 理由：安全检查是一项独立的服务，跑完了就是用了；但故障的安全服务不该向孩子的账户收钱。**平台一侧的供应商成本（USD）
//! 目前没有记到任何地方**：S3 的组件接口只记付款账户一侧的额度，S4 的尝试记录里输入检查没有尝试可挂——要记就得给
//! 尝试记录库加分录种类，不在 S6 范围，列在交付说明里。
//!
//! # 失败与日志
//!
//! 账本出错（等锁超时、数据库错误……）一律是 [`ExtraCostRefusal::Unavailable`]：不检查、不出网，请求以
//! `safety_unavailable`（`internal`）结束。日志只有请求号、阶段和稳定的类别，**没有任何内容**。
//! 结算失败（`settle` 不能返回错误）只写日志：预留有 TTL 兜底，过期后额度自动放回去；请求收尾时账本还会对没结算的
//! 附加成本留 `extra_unsettled` 告警。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use idoris_compute_contracts::ids::{AttemptId, AttemptIndex, EndpointKey};
use idoris_compute_contracts::money::Unit;
use idoris_compute_contracts::pricing::{
    Price, RateCard, USAGE_ROUNDING, amount_for_tokens, usage_amount,
};
use idoris_compute_contracts::safety::SafetyStage;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{ComponentDecision, LedgerError};

use super::cost::{
    ExtraCostMeter, ExtraCostOutcome, ExtraCostRefusal, ExtraCostRequest, ExtraCostTicket,
    SpendItem,
};
use crate::cloud::clock::Clock;
use crate::cloud::config::EndpointRegistry;

/// 输入检查在账本里的 component 名（`[a-z0-9_]{1,32}`，S3 的约定）。
pub const COMPONENT_INPUT: &str = "safety_input";
/// 完整输出检查在账本里的 component 名。
pub const COMPONENT_OUTPUT: &str = "safety_output";

/// 阶段对应的账本 component 名。
pub fn component_of(stage: SafetyStage) -> &'static str {
    match stage {
        SafetyStage::Input => COMPONENT_INPUT,
        SafetyStage::Output => COMPONENT_OUTPUT,
    }
}

/// 读写账本的安全检查成本计量器（见模块文档）。
pub struct LedgerExtraCost {
    accounts: Arc<AccountStore>,
    endpoints: Arc<EndpointRegistry>,
    clock: Arc<dyn Clock>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for LedgerExtraCost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerExtraCost").finish_non_exhaustive()
    }
}

impl LedgerExtraCost {
    pub fn new(
        accounts: Arc<AccountStore>,
        endpoints: Arc<EndpointRegistry>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            accounts,
            endpoints,
            clock,
            next_id: AtomicU64::new(1),
        }
    }

    /// 这个端点对这个付款单位的价目表。
    fn price_card(&self, endpoint: &EndpointKey, unit: &Unit) -> Option<&RateCard<Price>> {
        self.endpoints.get(endpoint)?.price_rates.get(unit)
    }
}

/// 日志里账本错误的稳定类别（**不打印错误本身**：它带着预留号和账户号，虽然没有内容，但类别够排障了）。
fn error_kind(error: &LedgerError) -> &'static str {
    if error.is_busy() {
        "ledger_busy"
    } else if matches!(error, LedgerError::HoldExists(_)) {
        "hold_exists"
    } else {
        "ledger_error"
    }
}

/// 结算金额（纯函数，见模块文档的表）。`price_of` 给出某个端点对付款单位的价目表。
pub(crate) fn charge_for<'a>(
    outcome: &ExtraCostOutcome,
    reserved: &Price,
    price_of: impl Fn(&EndpointKey) -> Option<&'a RateCard<Price>>,
) -> Price {
    let zero = Price::zero(reserved.unit().clone());
    let items: &[SpendItem] = match outcome {
        // 没能给出结论，或者检查根本没跑：平台承担，不向付款账户收钱
        ExtraCostOutcome::NotRun | ExtraCostOutcome::Unknown => return zero,
        ExtraCostOutcome::Spent(items) => items,
    };
    if items.is_empty() {
        // 规则检查器就拦下了，没有调用 LLM
        return zero;
    }
    let mut total = zero;
    for item in items {
        // 用量未知（或者没通过结构检查）、端点没有价目、金额溢出：保守地按预留额
        let Some(usage) = item.usage.as_ref().filter(|_| !item.estimated) else {
            return reserved.clone();
        };
        let Some(card) = price_of(&item.endpoint) else {
            return reserved.clone();
        };
        let Ok(amount) = usage_amount(card, usage, USAGE_ROUNDING) else {
            return reserved.clone();
        };
        let Ok(next) = total.checked_add(&amount) else {
            return reserved.clone();
        };
        total = next;
    }
    // 封顶在预留额：超出预留的部分由平台承担（见模块文档）
    if total.micros() > reserved.micros() {
        reserved.clone()
    } else {
        total
    }
}

impl ExtraCostMeter for LedgerExtraCost {
    fn reserve(&self, request: &ExtraCostRequest<'_>) -> Result<ExtraCostTicket, ExtraCostRefusal> {
        let estimate = request.estimate;
        let stage = request.stage;
        let unit = request.payment.unit();
        // 上游成本不能当价用：用检查端点的价目表按同样的 token 数折算成付款单位的价
        let Some(card) = self.price_card(&estimate.endpoint, unit) else {
            eprintln!(
                "[idoris] safety: request {} stage {stage}: the check endpoint has no price for the payer's unit; failing closed",
                request.request_id
            );
            return Err(ExtraCostRefusal::Unavailable);
        };
        let Ok(amount) = amount_for_tokens(
            card,
            estimate.max_input_tokens,
            estimate.max_output_tokens,
            USAGE_ROUNDING,
        ) else {
            eprintln!(
                "[idoris] safety: request {} stage {stage}: the check cost cannot be priced; failing closed",
                request.request_id
            );
            return Err(ExtraCostRefusal::Unavailable);
        };
        let ticket = ExtraCostTicket {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            request_id: request.request_id.clone(),
            stage,
            endpoint: estimate.endpoint.clone(),
            max_cost: estimate.max_cost.clone(),
            reserved_price: Some(amount.clone()),
        };
        if amount.is_zero() {
            // 价目是 0（比如免费的本地检查模型）：没有额度可占，账本里也不留一行零额的预留
            return Ok(ticket);
        }
        match self.accounts.reserve_request_component(
            request.request_id,
            component_of(stage),
            request.payment,
            &amount,
            request.now,
        ) {
            Ok(ComponentDecision::Reserved { .. }) => Ok(ticket),
            // 付款账户链放不下（余额、速率、在途、补贴池、凭证状态……）：准入类的原因码，不占任何额度
            Ok(ComponentDecision::Rejected(code)) => Err(ExtraCostRefusal::Denied(code)),
            Err(error) => {
                // HoldExists（同一个请求同一个阶段预留了两次）也在这里：失败即关闭，不复用
                eprintln!(
                    "[idoris] safety: request {} stage {stage}: the ledger could not reserve the check cost ({}); failing closed",
                    request.request_id,
                    error_kind(&error)
                );
                Err(ExtraCostRefusal::Unavailable)
            }
        }
    }

    fn settle(&self, ticket: ExtraCostTicket, outcome: ExtraCostOutcome) {
        let Some(reserved) = ticket.reserved_price.as_ref() else {
            return;
        };
        if reserved.is_zero() {
            // 当时没有占账本（零额预留）
            return;
        }
        let charged = charge_for(&outcome, reserved, |endpoint| {
            self.price_card(endpoint, reserved.unit())
        });
        let attempt = AttemptId::new(ticket.request_id.clone(), AttemptIndex(0));
        // 实际价为 0 就是释放；幂等：同样的金额重放什么都不写
        if let Err(error) = self.accounts.settle_component(
            &attempt,
            component_of(ticket.stage),
            &charged,
            self.clock.now(),
        ) {
            eprintln!(
                "[idoris] safety: request {} stage {}: the ledger could not settle the check cost ({}); the reservation expires on its own",
                ticket.request_id,
                ticket.stage,
                error_kind(&error)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use idoris_compute_contracts::money::Unit;
    use idoris_compute_contracts::pricing::{Cost, Usage};

    use super::*;

    fn thb(micros: i64) -> Price {
        Price::from_micros(Unit::Thb, micros).unwrap()
    }

    /// 输入 10 THB / 输出 40 THB 每百万 token。
    fn card() -> RateCard<Price> {
        RateCard::new(thb(10_000_000), thb(40_000_000), None).unwrap()
    }

    fn key() -> EndpointKey {
        EndpointKey::new("example-judge", "example-org/judge", "openai-compat").unwrap()
    }

    fn item(usage: Option<(u64, u64)>, estimated: bool) -> SpendItem {
        SpendItem {
            endpoint: key(),
            cost: Cost::from_micros(Unit::Usd, 1).unwrap(),
            usage: usage.map(|(prompt, completion)| Usage::new(prompt, completion, 0).unwrap()),
            estimated,
        }
    }

    fn charge(outcome: &ExtraCostOutcome, reserved: i64) -> i64 {
        let card = card();
        charge_for(outcome, &thb(reserved), |endpoint| {
            (*endpoint == key()).then_some(&card)
        })
        .micros()
    }

    #[test]
    fn the_component_names_follow_the_ledgers_convention() {
        // [a-z0-9_]{1,32}，不能叫 model；两个阶段各一个
        for name in [COMPONENT_INPUT, COMPONENT_OUTPUT] {
            assert!(name.len() <= 32 && name != "model");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
        assert_ne!(
            component_of(SafetyStage::Input),
            component_of(SafetyStage::Output)
        );
    }

    #[test]
    fn a_check_that_reached_a_verdict_is_charged_what_it_used() {
        // 40 入 6 出：40 x 10 + 6 x 40 = 640 微泰铢
        let outcome = ExtraCostOutcome::Spent(vec![item(Some((40, 6)), false)]);
        assert_eq!(charge(&outcome, 5_000), 640);
        // 多块检查：各次相加
        let outcome = ExtraCostOutcome::Spent(vec![
            item(Some((40, 6)), false),
            item(Some((100, 10)), false),
        ]);
        assert_eq!(charge(&outcome, 5_000), 640 + 1_400);
    }

    #[test]
    fn the_charge_never_exceeds_the_reservation() {
        // 实际用量超过预留（tokenizer 对不上）：封顶在预留额，超出的部分平台承担，不触发账本的 overage
        let outcome = ExtraCostOutcome::Spent(vec![item(Some((1_000_000, 1_000)), false)]);
        assert_eq!(charge(&outcome, 700), 700);
    }

    #[test]
    fn unknown_usage_is_charged_at_the_reservation_like_the_rest_of_the_service() {
        let outcome = ExtraCostOutcome::Spent(vec![item(None, true)]);
        assert_eq!(charge(&outcome, 5_000), 5_000);
        // 一块已知、一块未知：整笔按预留额
        let outcome = ExtraCostOutcome::Spent(vec![item(Some((40, 6)), false), item(None, true)]);
        assert_eq!(charge(&outcome, 5_000), 5_000);
        // 用量有、但标了估算（算不出价的情形）：不信它，按预留额
        let outcome = ExtraCostOutcome::Spent(vec![item(Some((40, 6)), true)]);
        assert_eq!(charge(&outcome, 5_000), 5_000);
    }

    #[test]
    fn a_check_without_a_price_card_is_charged_at_the_reservation() {
        let outcome = ExtraCostOutcome::Spent(vec![item(Some((40, 6)), false)]);
        let charged = charge_for(&outcome, &thb(5_000), |_| None).micros();
        assert_eq!(charged, 5_000);
    }

    #[test]
    fn checks_that_did_not_reach_a_verdict_cost_the_user_nothing() {
        for outcome in [
            ExtraCostOutcome::Unknown,
            ExtraCostOutcome::NotRun,
            // 规则检查器就拦下了：没有调用 LLM
            ExtraCostOutcome::Spent(Vec::new()),
        ] {
            assert_eq!(charge(&outcome, 5_000), 0, "{outcome:?}");
        }
    }
}
