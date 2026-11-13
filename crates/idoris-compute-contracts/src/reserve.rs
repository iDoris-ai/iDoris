//! 预留额的计算（设计稿 §5）：
//!
//! > **预留** = `max_tokens × 输出单价 + 输入估算`，并设硬性的 `max_tokens` 上限。余额不够时，先自动压低
//! > `max_tokens`，或者降级到更便宜的模型。
//!
//! 这里只有纯函数，不碰账本：
//!
//! - [`reserve`]：给定请求的 `max_tokens`、输入 token 估算、价格表，得到**有效的** `max_tokens`
//!   和预留额。
//! - [`fit_max_tokens`]：余额（或所有祖先里最紧的那个上限）不够时，找出**最大的、放得下的**
//!   `max_tokens`。放不下返回 `None`，由调用方决定降级、宽限还是拒绝。
//!
//! # 规则
//!
//! - **硬上限**：`有效 max_tokens = min(请求的 max_tokens, 硬上限)`。请求没带 `max_tokens`
//!   就用硬上限（按最坏情况预留）。`max_tokens = 0` 是错误（OpenAI 的下限是 1）。
//!   调用方**必须**把返回的 [`Reserved::effective_max_tokens`] 发给上游，否则上游可以输出超过预留的内容。
//! - **公式**：`预留 = ceil( (输入估算 × 输入价 + 有效 max_tokens × 输出价) / 1e6 )`，整笔只取一次整、
//!   向上（[`crate::pricing::USAGE_ROUNDING`]）。预留时不知道会命中多少缓存，输入估算全按输入价（保守）。
//! - **溢出**：任何一步放不下 `i64` 都返回错误，不回绕、不 panic。
//! - **单位**：`headroom` 必须与价格表同一单位，否则报单位不一致。
//!
//! 泛型 `M` 是 [`Price`]（对用户的价，进准入）或 [`Cost`]（上游成本，挂账待核销），公式相同，
//! 类型不同，互相不能混。
//!
//! [`Price`]: crate::pricing::Price
//! [`Cost`]: crate::pricing::Cost

use serde::{Deserialize, Serialize};

use crate::money::{MoneyError, Rounding, div_round};
use crate::pricing::{RateCard, Side, USAGE_ROUNDING, amount_for_tokens};

/// 预留策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReservePolicy {
    /// `max_tokens` 的硬上限，必须 >= 1。
    pub max_tokens_hard_cap: u64,
    /// 压低 `max_tokens` 时的最低有用值：压到比它还小就不如换个办法（降级/宽限/拒绝），
    /// 必须在 `1..=max_tokens_hard_cap`。
    pub min_useful_max_tokens: u64,
}

impl ReservePolicy {
    pub fn new(max_tokens_hard_cap: u64, min_useful_max_tokens: u64) -> Result<Self, ReserveError> {
        if max_tokens_hard_cap == 0 {
            return Err(ReserveError::InvalidPolicy(
                "max_tokens_hard_cap must be >= 1",
            ));
        }
        if min_useful_max_tokens == 0 || min_useful_max_tokens > max_tokens_hard_cap {
            return Err(ReserveError::InvalidPolicy(
                "min_useful_max_tokens must be in 1..=max_tokens_hard_cap",
            ));
        }
        Ok(Self {
            max_tokens_hard_cap,
            min_useful_max_tokens,
        })
    }

    fn validate(&self) -> Result<(), ReserveError> {
        Self::new(self.max_tokens_hard_cap, self.min_useful_max_tokens).map(|_| ())
    }
}

/// 一次请求的预留输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveRequest {
    /// 请求里的 `max_tokens`；`None` = 没带。
    pub requested_max_tokens: Option<u64>,
    /// 输入 token 的估算（保守估算，见 `idoris-tenancy` 的 `ConservativeTokenEstimator`）。
    pub input_tokens_estimate: u64,
}

/// 有效 `max_tokens` 是怎么来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensSource {
    /// 请求带了，且没超过硬上限。
    AsRequested,
    /// 请求带了，但超过硬上限，被压到硬上限。
    ClampedToHardCap,
    /// 请求没带，按硬上限预留。
    DefaultedToHardCap,
    /// 余额放不下，被 [`fit_max_tokens`] 压低到放得下的最大值。
    ReducedToFit,
}

/// 预留结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(serialize = "M: Serialize", deserialize = "M: Deserialize<'de>")
)]
pub struct Reserved<M> {
    /// 必须发给上游的 `max_tokens`。
    pub effective_max_tokens: u64,
    pub source: MaxTokensSource,
    pub amount: M,
}

/// 预留计算错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReserveError {
    #[error("invalid reserve policy: {0}")]
    InvalidPolicy(&'static str),
    #[error("max_tokens must be >= 1")]
    ZeroMaxTokens,
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// 有效 `max_tokens`：`min(请求值, 硬上限)`，没带用硬上限。
fn effective_max_tokens(
    policy: &ReservePolicy,
    request: &ReserveRequest,
) -> Result<(u64, MaxTokensSource), ReserveError> {
    policy.validate()?;
    let cap = policy.max_tokens_hard_cap;
    match request.requested_max_tokens {
        None => Ok((cap, MaxTokensSource::DefaultedToHardCap)),
        Some(0) => Err(ReserveError::ZeroMaxTokens),
        Some(requested) if requested > cap => Ok((cap, MaxTokensSource::ClampedToHardCap)),
        Some(requested) => Ok((requested, MaxTokensSource::AsRequested)),
    }
}

/// 预留额：`ceil((输入估算 × 输入价 + 有效 max_tokens × 输出价) / 1e6)`。
pub fn reserve<M: Side>(
    policy: &ReservePolicy,
    request: &ReserveRequest,
    rates: &RateCard<M>,
) -> Result<Reserved<M>, ReserveError> {
    let (effective, source) = effective_max_tokens(policy, request)?;
    let amount = amount_for_tokens(
        rates,
        request.input_tokens_estimate,
        effective,
        USAGE_ROUNDING,
    )?;
    Ok(Reserved {
        effective_max_tokens: effective,
        source,
        amount,
    })
}

/// 在 `headroom`（所有祖先里最紧的上限，见 [`crate::admission::AdmissionReport::max_admissible`]）
/// 之内，找最大的、预留额放得下的 `max_tokens`。
///
/// 1. 先按 [`reserve`] 算；放得下就原样返回（来源不变）。
/// 2. 放不下就解 `ceil((I × r_in + m × r_out) / 1e6) <= headroom`，因为 `headroom` 是整数，
///    等价于 `I × r_in + m × r_out <= headroom × 1e6`，所以 `m = floor((headroom × 1e6 − I × r_in) / r_out)`。
///    这个 `m` 是**最大的**放得下的值（`m + 1` 一定放不下）。
/// 3. `m` 小于 `policy.min_useful_max_tokens`，或者输入估算本身就放不下（`m < 0`），或者输出价是 0
///    却仍然放不下（说明是输入部分放不下），返回 `Ok(None)`：压 `max_tokens` 救不了，交给调用方换办法。
///
/// `headroom` 必须与 `rates` 同一单位。`headroom` 为负按 0 处理（放不下任何东西）。
pub fn fit_max_tokens<M: Side>(
    policy: &ReservePolicy,
    request: &ReserveRequest,
    rates: &RateCard<M>,
    headroom: &M,
) -> Result<Option<Reserved<M>>, ReserveError> {
    headroom
        .as_money()
        .require_same_unit(rates.input_per_mtok().as_money())?;
    let first = reserve(policy, request, rates)?;
    let budget = headroom.as_money().micros.max(0);
    if first.amount.as_money().micros <= budget {
        return Ok(Some(first));
    }

    let output_rate = i128::from(rates.output_per_mtok().as_money().micros);
    if output_rate == 0 {
        // 输出免费，金额与 max_tokens 无关；第一遍放不下就是输入部分放不下。
        return Ok(None);
    }
    let input_part = i128::from(request.input_tokens_estimate)
        .checked_mul(i128::from(rates.input_per_mtok().as_money().micros))
        .ok_or(MoneyError::Overflow)?;
    let budget_scaled = i128::from(budget)
        .checked_mul(1_000_000)
        .ok_or(MoneyError::Overflow)?;
    let remaining = budget_scaled
        .checked_sub(input_part)
        .ok_or(MoneyError::Overflow)?;
    if remaining < 0 {
        return Ok(None);
    }
    let fit = div_round(remaining, output_rate, Rounding::Floor).ok_or(MoneyError::Overflow)?;
    // `fit` 一定小于第一遍的有效 max_tokens（否则第一遍就放得下了）；转 u64 不会失败，
    // 失败就当放不下。
    let Ok(fit) = u64::try_from(fit) else {
        return Ok(None);
    };
    if fit < policy.min_useful_max_tokens {
        return Ok(None);
    }
    let amount = amount_for_tokens(rates, request.input_tokens_estimate, fit, USAGE_ROUNDING)?;
    Ok(Some(Reserved {
        effective_max_tokens: fit,
        source: MaxTokensSource::ReducedToFit,
        amount,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::money::Unit;
    use crate::pricing::{Cost, Price};
    use proptest::prelude::*;

    fn price(micros: i64) -> Price {
        Price::from_micros(Unit::Usd, micros).unwrap()
    }

    fn card(input: i64, output: i64) -> RateCard<Price> {
        RateCard::new(price(input), price(output), None).unwrap()
    }

    fn policy(cap: u64, min: u64) -> ReservePolicy {
        ReservePolicy::new(cap, min).unwrap()
    }

    fn req(max: Option<u64>, input: u64) -> ReserveRequest {
        ReserveRequest {
            requested_max_tokens: max,
            input_tokens_estimate: input,
        }
    }

    // ---------- 硬上限与公式边界 ----------

    #[test]
    fn effective_max_tokens_boundaries() {
        let p = policy(4_096, 16);
        let rc = card(280_000, 420_000);
        // (请求的 max_tokens, 期望的有效值, 期望的来源)
        let cases = [
            (Some(1), 1, MaxTokensSource::AsRequested),
            (Some(4_095), 4_095, MaxTokensSource::AsRequested),
            (Some(4_096), 4_096, MaxTokensSource::AsRequested), // 刚好等于上限
            (Some(4_097), 4_096, MaxTokensSource::ClampedToHardCap), // 多 1
            (Some(u64::MAX), 4_096, MaxTokensSource::ClampedToHardCap),
            (None, 4_096, MaxTokensSource::DefaultedToHardCap),
        ];
        for (requested, effective, source) in cases {
            let r = reserve(&p, &req(requested, 0), &rc).unwrap();
            assert_eq!(r.effective_max_tokens, effective, "{requested:?}");
            assert_eq!(r.source, source, "{requested:?}");
            assert!(r.effective_max_tokens <= p.max_tokens_hard_cap);
        }
        assert_eq!(
            reserve(&p, &req(Some(0), 10), &rc).unwrap_err(),
            ReserveError::ZeroMaxTokens
        );
    }

    #[test]
    fn reserve_formula_matches_hand_computed_values() {
        let rc = card(280_000, 420_000); // 0.28 / 0.42 美元每百万 token
        let p = policy(1_000_000, 1);
        // (输入估算, max_tokens, 期望微美元)
        let cases = [
            (0, 1, 1),         // 0.42 -> 1
            (1, 1, 1),         // 0.28 + 0.42 = 0.70 -> 1
            (1_500, 400, 588), // 420 + 168，整除
            (1_500, 401, 589), // 588.42 -> 589
            (0, 1_000_000, 420_000),
            (1_000_000, 1_000_000, 700_000),
            (1_000_000, 1, 280_001), // 280_000 + 0.42 -> 280_001
        ];
        for (input, max, expected) in cases {
            let r = reserve(&p, &req(Some(max), input), &rc).unwrap();
            assert_eq!(r.amount.micros(), expected, "input={input} max={max}");
        }
    }

    #[test]
    fn zero_rates_reserve_nothing_but_still_clamp_max_tokens() {
        let r = reserve(&policy(100, 1), &req(Some(500), 1_000_000), &card(0, 0)).unwrap();
        assert_eq!(r.amount.micros(), 0);
        assert_eq!(r.effective_max_tokens, 100);
    }

    #[test]
    fn extreme_values_overflow_cleanly() {
        let huge = card(i64::MAX, i64::MAX);
        let p = policy(u64::MAX, 1);
        assert_eq!(
            reserve(&p, &req(None, u64::MAX), &huge).unwrap_err(),
            ReserveError::Money(MoneyError::Overflow)
        );
        // 乘积放得下 i128，但除以 1e6 之后仍放不下 i64
        assert_eq!(
            reserve(&p, &req(Some(u64::MAX), 0), &huge).unwrap_err(),
            ReserveError::Money(MoneyError::Overflow)
        );
        // 最大 token × 零价格：不是错误
        assert_eq!(
            reserve(&p, &req(None, u64::MAX), &card(0, 0))
                .unwrap()
                .amount
                .micros(),
            0
        );
    }

    #[test]
    fn policy_validation() {
        assert!(ReservePolicy::new(0, 1).is_err());
        assert!(ReservePolicy::new(10, 0).is_err());
        assert!(ReservePolicy::new(10, 11).is_err());
        assert!(ReservePolicy::new(10, 10).is_ok());
        assert!(ReservePolicy::new(1, 1).is_ok());
        // 绕过构造函数写出非法策略，reserve 照样拒绝
        let bad = ReservePolicy {
            max_tokens_hard_cap: 0,
            min_useful_max_tokens: 0,
        };
        assert!(matches!(
            reserve(&bad, &req(Some(1), 1), &card(1, 1)).unwrap_err(),
            ReserveError::InvalidPolicy(_)
        ));
    }

    #[test]
    fn cost_side_uses_the_same_formula_with_its_own_type() {
        let cost_card: RateCard<Cost> = RateCard::new(
            Cost::from_micros(Unit::Usd, 280_000).unwrap(),
            Cost::from_micros(Unit::Usd, 420_000).unwrap(),
            None,
        )
        .unwrap();
        let r: Reserved<Cost> =
            reserve(&policy(1_000, 1), &req(Some(400), 1_500), &cost_card).unwrap();
        assert_eq!(r.amount.micros(), 588);
    }

    // ---------- 压低 max_tokens ----------

    #[test]
    fn fit_returns_the_original_reserve_when_it_fits() {
        let rc = card(280_000, 420_000);
        let p = policy(4_096, 16);
        let r = req(Some(400), 1_500);
        let plain = reserve(&p, &r, &rc).unwrap();
        // 刚好等于：不压低
        let fit = fit_max_tokens(&p, &r, &rc, &price(plain.amount.micros()))
            .unwrap()
            .unwrap();
        assert_eq!(fit, plain);
        // 多 1 微单位也不压低
        let fit = fit_max_tokens(&p, &r, &rc, &price(plain.amount.micros() + 1))
            .unwrap()
            .unwrap();
        assert_eq!(fit, plain);
    }

    #[test]
    fn fit_reduces_max_tokens_to_the_largest_that_fits() {
        let rc = card(280_000, 420_000);
        let p = policy(4_096, 16);
        let r = req(Some(4_000), 1_500);
        // 输入部分 420 微美元；headroom 588 -> 输出部分 168 -> 400 个 token
        let fit = fit_max_tokens(&p, &r, &rc, &price(588)).unwrap().unwrap();
        assert_eq!(fit.effective_max_tokens, 400);
        assert_eq!(fit.source, MaxTokensSource::ReducedToFit);
        assert_eq!(fit.amount.micros(), 588);
        // headroom 少 1：输出部分剩 167 微美元，每个输出 token 0.42 微美元，只放得下 397 个
        // （397 个 = 586.74 -> 587；398 个 = 587.16 -> 588，超）
        let fit = fit_max_tokens(&p, &r, &rc, &price(587)).unwrap().unwrap();
        assert_eq!(fit.effective_max_tokens, 397);
        assert_eq!(fit.amount.micros(), 587);
        let one_more = reserve(&p, &req(Some(398), 1_500), &rc).unwrap();
        assert_eq!(one_more.amount.micros(), 588);
    }

    #[test]
    fn fit_gives_up_when_clamping_cannot_help() {
        let rc = card(280_000, 420_000);
        let p = policy(4_096, 16);
        let r = req(Some(4_000), 1_500);
        // 输入部分就要 420：headroom 419 放不下输入
        assert_eq!(fit_max_tokens(&p, &r, &rc, &price(419)).unwrap(), None);
        assert_eq!(fit_max_tokens(&p, &r, &rc, &price(0)).unwrap(), None);
        // 压到比最低有用值还小：420 + 6 = 426 只能放下 14 个 token (14 × 0.42 = 5.88 -> 6)，
        // 最低有用值是 16，所以放弃
        assert_eq!(fit_max_tokens(&p, &r, &rc, &price(426)).unwrap(), None);
        // 刚好够 16 个 token（6.72 -> 7）：420 + 7 = 427
        let fit = fit_max_tokens(&p, &r, &rc, &price(427)).unwrap().unwrap();
        assert!(fit.effective_max_tokens >= 16);
        // 输出免费、输入放不下：压 max_tokens 没用
        assert_eq!(
            fit_max_tokens(&p, &r, &card(280_000, 0), &price(419)).unwrap(),
            None
        );
    }

    #[test]
    fn fit_requires_the_same_unit() {
        let rc = card(1, 1);
        let thb = Price::from_micros(Unit::Thb, 10).unwrap();
        assert!(matches!(
            fit_max_tokens(&policy(10, 1), &req(Some(1), 1), &rc, &thb).unwrap_err(),
            ReserveError::Money(MoneyError::UnitMismatch { .. })
        ));
    }

    // ---------- 性质 ----------

    /// 独立的参考实现：用 u128/i128 直接按公式算，不经过 `amount_for_tokens`。
    fn reference_amount(input: u64, max: u64, r_in: i64, r_out: i64) -> i128 {
        let numerator = i128::from(input) * i128::from(r_in) + i128::from(max) * i128::from(r_out);
        (numerator + 999_999) / 1_000_000
    }

    proptest! {
        #[test]
        fn reserve_equals_the_reference_formula_and_respects_the_cap(
            r_in in 0i64..50_000_000,
            r_out in 0i64..50_000_000,
            cap in 1u64..100_000,
            requested in proptest::option::of(1u64..200_000),
            input in 0u64..2_000_000,
        ) {
            let p = ReservePolicy::new(cap, 1).unwrap();
            let rc = card(r_in, r_out);
            let r = reserve(&p, &req(requested, input), &rc).unwrap();
            let expected_max = requested.map_or(cap, |m| m.min(cap));
            prop_assert_eq!(r.effective_max_tokens, expected_max);
            prop_assert!(r.effective_max_tokens <= cap);
            prop_assert_eq!(i128::from(r.amount.micros()), reference_amount(input, expected_max, r_in, r_out));
        }

        #[test]
        fn reserve_is_monotone_in_max_tokens_and_input(
            r_in in 0i64..50_000_000,
            r_out in 0i64..50_000_000,
            input in 0u64..1_000_000,
            m in 1u64..50_000,
            extra in 0u64..1_000,
        ) {
            let p = ReservePolicy::new(1_000_000, 1).unwrap();
            let rc = card(r_in, r_out);
            let base = reserve(&p, &req(Some(m), input), &rc).unwrap().amount.micros();
            let more_out = reserve(&p, &req(Some(m + extra), input), &rc).unwrap().amount.micros();
            let more_in = reserve(&p, &req(Some(m), input + extra), &rc).unwrap().amount.micros();
            prop_assert!(more_out >= base);
            prop_assert!(more_in >= base);
        }

        /// `fit_max_tokens` 的两个性质：放得下；并且是最大的（再多 1 个 token 就放不下，除非已经到顶）。
        #[test]
        fn fit_is_feasible_and_maximal(
            r_in in 0i64..50_000_000,
            r_out in 1i64..50_000_000,
            cap in 1u64..100_000,
            requested in proptest::option::of(1u64..200_000),
            input in 0u64..500_000,
            headroom in 0i64..2_000_000_000,
        ) {
            let p = ReservePolicy::new(cap, 1).unwrap();
            let rc = card(r_in, r_out);
            let request = req(requested, input);
            let ceiling = requested.map_or(cap, |m| m.min(cap));
            match fit_max_tokens(&p, &request, &rc, &price(headroom)).unwrap() {
                Some(fit) => {
                    prop_assert!(fit.amount.micros() <= headroom, "feasible");
                    prop_assert!(fit.effective_max_tokens >= 1);
                    prop_assert!(fit.effective_max_tokens <= ceiling);
                    if fit.effective_max_tokens < ceiling {
                        let next = reference_amount(input, fit.effective_max_tokens + 1, r_in, r_out);
                        prop_assert!(next > i128::from(headroom), "maximal");
                    }
                }
                None => {
                    // 连 1 个 token 都放不下（min_useful = 1）
                    prop_assert!(reference_amount(input, 1, r_in, r_out) > i128::from(headroom));
                }
            }
        }
    }
}
