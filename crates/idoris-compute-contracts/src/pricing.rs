//! `cost`（上游成本）与 `price`（对用户的价）：两个互不相通的类型（设计稿 §4）。
//!
//! 每次请求都记两个数：`cost` 带**供应商的币种**，`price` 带**账户的单位**。两者不能互相
//! 赋值、不能直接相加：
//!
//! ```compile_fail
//! use idoris_compute_contracts::money::Unit;
//! use idoris_compute_contracts::pricing::{Cost, Price};
//! let cost = Cost::zero(Unit::Usd);
//! let _price: Price = cost; // Cost 和 Price 是不同类型
//! ```
//!
//! 对照组：同一段代码构造 `Price` 就能编译，说明上面是类型不匹配造成的失败。
//!
//! ```
//! use idoris_compute_contracts::money::Unit;
//! use idoris_compute_contracts::pricing::Price;
//! let _price: Price = Price::zero(Unit::Usd);
//! ```
//!
//! 二者要「对在一起」（算毛利）只能走 [`crate::money::convert`] 这条显式带汇率的路。
//!
//! # 价格的表示
//!
//! [`RateCard`] 是「每百万 token 多少微单位」：`input_per_mtok`、`output_per_mtok`，可选的
//! `cached_input_per_mtok`（端点具备「缓存计价」能力时用）。按每百万 token 而不是每 token
//! 表示，是因为 0.28 美元/百万 token 折到每 token 是 0.28 微美元，不是整数。
//!
//! 用量到金额：`ceil( (未缓存输入 × 输入价 + 缓存输入 × 缓存价 + 输出 × 输出价) / 1e6 )`，
//! **整笔只取一次整**，所以一次请求最多多算 1 微单位。默认取整方式 [`USAGE_ROUNDING`] = 向上。

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::TimestampMs;
use crate::money::{
    Converted, FxRate, Money, MoneyError, Rounding, Unit, convert_micros, div_round,
};

/// 用量计价（成本、售价、预留）默认的取整方式：向上。宁可多算 1 微单位，不少算。
pub const USAGE_ROUNDING: Rounding = Rounding::Ceil;

/// 每百万 token。
const TOKENS_PER_MTOK: i128 = 1_000_000;

mod sealed {
    pub trait Sealed {}
}

/// 带「侧」的金额：只有 [`Cost`] 和 [`Price`] 实现，外部无法再实现。
pub trait Side: sealed::Sealed + Sized + Clone + fmt::Debug {
    const NAME: &'static str;
    fn from_money(money: Money) -> Result<Self, MoneyError>;
    fn as_money(&self) -> &Money;
}

macro_rules! side_money {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "Money", into = "Money")]
        pub struct $name(Money);

        impl $name {
            /// 不允许负数；单位不限（见模块文档与设计取舍说明）。
            pub fn new(money: Money) -> Result<Self, MoneyError> {
                if money.micros < 0 {
                    return Err(MoneyError::Negative {
                        unit: money.unit,
                        micros: money.micros,
                    });
                }
                Ok(Self(money))
            }

            pub fn from_micros(unit: Unit, micros: i64) -> Result<Self, MoneyError> {
                Self::new(Money::new(unit, micros))
            }

            pub fn zero(unit: Unit) -> Self {
                Self(Money::zero(unit))
            }

            pub fn money(&self) -> &Money {
                &self.0
            }

            pub fn into_money(self) -> Money {
                self.0
            }

            pub fn unit(&self) -> &Unit {
                &self.0.unit
            }

            /// 非负的微单位数。
            pub fn micros(&self) -> i64 {
                self.0.micros
            }

            pub fn is_zero(&self) -> bool {
                self.0.micros == 0
            }

            pub fn checked_add(&self, other: &Self) -> Result<Self, MoneyError> {
                Ok(Self(self.0.checked_add(&other.0)?))
            }

            /// `self - other`；结果为负返回 [`MoneyError::Negative`]（需要下限为 0 的减法用
            /// [`Self::saturating_sub`]）。
            pub fn checked_sub(&self, other: &Self) -> Result<Self, MoneyError> {
                Self::new(self.0.checked_sub(&other.0)?)
            }

            /// `max(self - other, 0)`；单位不同仍然返回错误。
            pub fn saturating_sub(&self, other: &Self) -> Result<Self, MoneyError> {
                let diff = self.0.checked_sub(&other.0)?;
                Ok(Self(Money::new(diff.unit, diff.micros.max(0))))
            }

            pub fn try_cmp(&self, other: &Self) -> Result<Ordering, MoneyError> {
                self.0.try_cmp(&other.0)
            }

            /// 显式换算，规则同 [`crate::money::convert`]。
            pub fn convert(
                &self,
                fx: &FxRate,
                now: TimestampMs,
                max_age_ms: i64,
                rounding: Rounding,
            ) -> Result<Converted<Self>, MoneyError> {
                let micros = convert_micros(self.unit(), self.micros(), fx, now, max_age_ms, rounding)?;
                Ok(Converted {
                    amount: Self::from_micros(fx.to_unit().clone(), micros)?,
                    rate: fx.clone(),
                })
            }
        }

        impl TryFrom<Money> for $name {
            type Error = MoneyError;
            fn try_from(money: Money) -> Result<Self, MoneyError> {
                Self::new(money)
            }
        }

        impl From<$name> for Money {
            fn from(value: $name) -> Money {
                value.0
            }
        }

        impl sealed::Sealed for $name {}

        impl Side for $name {
            const NAME: &'static str = stringify!($name);
            fn from_money(money: Money) -> Result<Self, MoneyError> {
                Self::new(money)
            }
            fn as_money(&self) -> &Money {
                &self.0
            }
        }
    };
}

side_money!(
    /// 上游成本：供应商向我们收的钱，带**供应商的币种**。非负。
    ///
    /// 单位不限制为货币：本机模型没有现金成本，组织节点可以用自己的 `QUOTA` 记归因。
    /// 「`Cost` 与 `Price` 不能互相赋值」靠的是类型，不是单位。
    Cost
);
side_money!(
    /// 对用户的价：用户（账户）要付的钱，带**账户的单位**。非负。
    Price
);

/// 每百万 token 的价格表：`Cost` 版（供应商价）或 `Price` 版（对用户的价）。
///
/// 三个价格必须是同一单位；构造和反序列化都会检查。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "RateCardWire<M>",
    bound(deserialize = "M: Side + Deserialize<'de>")
)]
pub struct RateCard<M> {
    input_per_mtok: M,
    output_per_mtok: M,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_input_per_mtok: Option<M>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RateCardWire<M> {
    input_per_mtok: M,
    output_per_mtok: M,
    cached_input_per_mtok: Option<M>,
}

impl<M: Side> TryFrom<RateCardWire<M>> for RateCard<M> {
    type Error = MoneyError;
    fn try_from(wire: RateCardWire<M>) -> Result<Self, MoneyError> {
        RateCard::new(
            wire.input_per_mtok,
            wire.output_per_mtok,
            wire.cached_input_per_mtok,
        )
    }
}

impl<M: Side> RateCard<M> {
    pub fn new(
        input_per_mtok: M,
        output_per_mtok: M,
        cached_input_per_mtok: Option<M>,
    ) -> Result<Self, MoneyError> {
        input_per_mtok
            .as_money()
            .require_same_unit(output_per_mtok.as_money())?;
        if let Some(cached) = &cached_input_per_mtok {
            input_per_mtok
                .as_money()
                .require_same_unit(cached.as_money())?;
        }
        Ok(Self {
            input_per_mtok,
            output_per_mtok,
            cached_input_per_mtok,
        })
    }

    pub fn input_per_mtok(&self) -> &M {
        &self.input_per_mtok
    }

    pub fn output_per_mtok(&self) -> &M {
        &self.output_per_mtok
    }

    pub fn cached_input_per_mtok(&self) -> Option<&M> {
        self.cached_input_per_mtok.as_ref()
    }

    pub fn unit(&self) -> &Unit {
        &self.input_per_mtok.as_money().unit
    }
}

/// 计价相关的错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    #[error(transparent)]
    Money(#[from] MoneyError),
    #[error("cached prompt tokens {cached} exceed prompt tokens {prompt}")]
    CachedExceedsPrompt { cached: u64, prompt: u64 },
}

/// 一次尝试的 token 用量。`cached_prompt_tokens` 是 `prompt_tokens` 的子集（命中缓存的输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "UsageWire")]
pub struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    cached_prompt_tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageWire {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    cached_prompt_tokens: u64,
}

impl TryFrom<UsageWire> for Usage {
    type Error = PricingError;
    fn try_from(wire: UsageWire) -> Result<Self, PricingError> {
        Usage::new(
            wire.prompt_tokens,
            wire.completion_tokens,
            wire.cached_prompt_tokens,
        )
    }
}

impl Usage {
    /// 全零用量。供应商账单核对后确认「没有执行」时，用它上报（见
    /// [`crate::attempt::AttemptEvent::UsageReported`]）。
    pub const ZERO: Usage = Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        cached_prompt_tokens: 0,
    };

    pub fn new(
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_prompt_tokens: u64,
    ) -> Result<Self, PricingError> {
        if cached_prompt_tokens > prompt_tokens {
            return Err(PricingError::CachedExceedsPrompt {
                cached: cached_prompt_tokens,
                prompt: prompt_tokens,
            });
        }
        Ok(Self {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
        })
    }

    pub fn prompt_tokens(&self) -> u64 {
        self.prompt_tokens
    }

    pub fn completion_tokens(&self) -> u64 {
        self.completion_tokens
    }

    pub fn cached_prompt_tokens(&self) -> u64 {
        self.cached_prompt_tokens
    }
}

/// 用 `numerator / 1e6` 取整并落成 `M`；单位取自 `rates`。
fn finish<M: Side>(unit: &Unit, numerator: i128, rounding: Rounding) -> Result<M, MoneyError> {
    let rounded = div_round(numerator, TOKENS_PER_MTOK, rounding).ok_or(MoneyError::Overflow)?;
    let micros = i64::try_from(rounded).map_err(|_| MoneyError::Overflow)?;
    M::from_money(Money::new(unit.clone(), micros))
}

fn term(tokens: u64, rate: &Money) -> Result<i128, MoneyError> {
    i128::from(tokens)
        .checked_mul(i128::from(rate.micros))
        .ok_or(MoneyError::Overflow)
}

/// `input_tokens` 个输入 token + `output_tokens` 个输出 token 的金额，输入全部按输入价。
///
/// 预留（[`crate::reserve`]）用它：预留时不知道会命中多少缓存，保守地全按输入价。
pub fn amount_for_tokens<M: Side>(
    rates: &RateCard<M>,
    input_tokens: u64,
    output_tokens: u64,
    rounding: Rounding,
) -> Result<M, MoneyError> {
    let numerator = term(input_tokens, rates.input_per_mtok.as_money())?
        .checked_add(term(output_tokens, rates.output_per_mtok.as_money())?)
        .ok_or(MoneyError::Overflow)?;
    finish(rates.unit(), numerator, rounding)
}

/// 一次尝试实际用量的金额。缓存输入按 `cached_input_per_mtok`；没有缓存价就按输入价。
pub fn usage_amount<M: Side>(
    rates: &RateCard<M>,
    usage: &Usage,
    rounding: Rounding,
) -> Result<M, MoneyError> {
    let uncached = usage.prompt_tokens - usage.cached_prompt_tokens; // Usage 不变式：cached <= prompt
    let cached_rate = rates
        .cached_input_per_mtok
        .as_ref()
        .map_or(rates.input_per_mtok.as_money(), |c| c.as_money());
    let numerator = term(uncached, rates.input_per_mtok.as_money())?
        .checked_add(term(usage.cached_prompt_tokens, cached_rate)?)
        .ok_or(MoneyError::Overflow)?
        .checked_add(term(
            usage.completion_tokens,
            rates.output_per_mtok.as_money(),
        )?)
        .ok_or(MoneyError::Overflow)?;
    finish(rates.unit(), numerator, rounding)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ids::NodeId;
    use proptest::prelude::*;

    fn usd_price(micros: i64) -> Price {
        Price::from_micros(Unit::Usd, micros).unwrap()
    }
    fn usd_cost(micros: i64) -> Cost {
        Cost::from_micros(Unit::Usd, micros).unwrap()
    }

    fn card(input: i64, output: i64, cached: Option<i64>) -> RateCard<Price> {
        RateCard::new(usd_price(input), usd_price(output), cached.map(usd_price)).unwrap()
    }

    // ---------- Cost / Price ----------

    #[test]
    fn cost_and_price_reject_negative_amounts_everywhere() {
        assert_eq!(
            Cost::from_micros(Unit::Thb, -1).unwrap_err(),
            MoneyError::Negative {
                unit: Unit::Thb,
                micros: -1
            }
        );
        assert!(Price::new(Money::new(Unit::Usd, i64::MIN)).is_err());
        assert!(Price::from_micros(Unit::Usd, 0).is_ok());
        assert!(Price::from_micros(Unit::Usd, i64::MAX).is_ok());
        // 反序列化同样拒绝
        assert!(serde_json::from_str::<Price>(r#"{"unit":"USD","micros":-1}"#).is_err());
        assert!(serde_json::from_str::<Cost>(r#"{"unit":"USD","micros":-1}"#).is_err());
        assert!(serde_json::from_str::<Price>(r#"{"unit":"USD","micros":0}"#).is_ok());
    }

    #[test]
    fn cost_and_price_have_the_same_wire_shape_but_are_different_rust_types() {
        let cost = usd_cost(7);
        let price = usd_price(7);
        assert_eq!(
            serde_json::to_value(&cost).unwrap(),
            serde_json::to_value(&price).unwrap()
        );
        assert_eq!(<Cost as Side>::NAME, "Cost");
        assert_eq!(<Price as Side>::NAME, "Price");
    }

    #[test]
    fn same_side_arithmetic_is_checked_and_unit_safe() {
        let a = usd_price(5);
        let b = usd_price(3);
        assert_eq!(a.checked_add(&b).unwrap().micros(), 8);
        assert_eq!(a.checked_sub(&b).unwrap().micros(), 2);
        assert_eq!(
            b.checked_sub(&a).unwrap_err(),
            MoneyError::Negative {
                unit: Unit::Usd,
                micros: -2
            }
        );
        assert_eq!(b.saturating_sub(&a).unwrap().micros(), 0);
        assert_eq!(a.try_cmp(&b).unwrap(), Ordering::Greater);
        let thb = Price::from_micros(Unit::Thb, 1).unwrap();
        assert!(a.checked_add(&thb).is_err());
        assert!(a.checked_sub(&thb).is_err());
        assert!(a.saturating_sub(&thb).is_err());
        assert!(a.try_cmp(&thb).is_err());
        assert_eq!(
            usd_price(i64::MAX).checked_add(&usd_price(1)).unwrap_err(),
            MoneyError::Overflow
        );
        assert!(a.money().unit == Unit::Usd && !a.is_zero());
        assert!(Price::zero(Unit::Usd).is_zero());
        assert_eq!(Money::from(a.clone()), Money::new(Unit::Usd, 5));
    }

    #[test]
    fn typed_convert_keeps_the_side_and_returns_the_rate() {
        let fx = FxRate::new(Unit::Usd, Unit::Thb, 35_000_000_000, TimestampMs(100)).unwrap();
        let out = usd_cost(1_000_000)
            .convert(&fx, TimestampMs(100), 0, Rounding::HalfAwayFromZero)
            .unwrap();
        assert_eq!(
            out.amount,
            Cost::from_micros(Unit::Thb, 35_000_000).unwrap()
        );
        assert_eq!(out.rate, fx);
        assert!(
            usd_cost(1)
                .convert(&fx, TimestampMs(99), 0, Rounding::Ceil)
                .is_err()
        );
        let thb = Cost::from_micros(Unit::Thb, 1).unwrap();
        assert!(
            thb.convert(&fx, TimestampMs(100), 0, Rounding::Ceil)
                .is_err()
        );
    }

    // ---------- RateCard / Usage ----------

    #[test]
    fn rate_card_requires_one_unit() {
        let thb = Price::from_micros(Unit::Thb, 1).unwrap();
        assert!(RateCard::new(usd_price(1), thb.clone(), None).is_err());
        assert!(RateCard::new(usd_price(1), usd_price(1), Some(thb)).is_err());
        let q = Unit::Quota(NodeId::new("n").unwrap());
        let quota_card = RateCard::new(
            Price::from_micros(q.clone(), 1).unwrap(),
            Price::from_micros(q.clone(), 2).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(quota_card.unit(), &q);
        let ok = card(280_000, 420_000, Some(28_000));
        assert_eq!(ok.input_per_mtok().micros(), 280_000);
        assert_eq!(ok.output_per_mtok().micros(), 420_000);
        assert_eq!(ok.cached_input_per_mtok().unwrap().micros(), 28_000);
    }

    #[test]
    fn rate_card_wire_shape_and_validation_on_deserialize() {
        let rc = card(280_000, 420_000, None);
        let json = serde_json::to_string(&rc).unwrap();
        assert_eq!(
            json,
            r#"{"input_per_mtok":{"unit":"USD","micros":280000},"output_per_mtok":{"unit":"USD","micros":420000}}"#
        );
        assert_eq!(serde_json::from_str::<RateCard<Price>>(&json).unwrap(), rc);
        let mixed = r#"{"input_per_mtok":{"unit":"USD","micros":1},"output_per_mtok":{"unit":"THB","micros":1}}"#;
        assert!(serde_json::from_str::<RateCard<Price>>(mixed).is_err());
        let extra = r#"{"input_per_mtok":{"unit":"USD","micros":1},"output_per_mtok":{"unit":"USD","micros":1},"x":1}"#;
        assert!(serde_json::from_str::<RateCard<Price>>(extra).is_err());
        let neg = r#"{"input_per_mtok":{"unit":"USD","micros":-1},"output_per_mtok":{"unit":"USD","micros":1}}"#;
        assert!(serde_json::from_str::<RateCard<Price>>(neg).is_err());
        // 同一段 JSON 可以反序列化成 Cost 版，也可以成 Price 版：两个不同的类型
        assert!(serde_json::from_str::<RateCard<Cost>>(&json).is_ok());
    }

    #[test]
    fn usage_rejects_cached_above_prompt_and_unknown_fields() {
        assert_eq!(
            Usage::new(10, 5, 11).unwrap_err(),
            PricingError::CachedExceedsPrompt {
                cached: 11,
                prompt: 10
            }
        );
        assert!(Usage::new(10, 5, 10).is_ok());
        assert!(
            serde_json::from_str::<Usage>(r#"{"prompt_tokens":1,"completion_tokens":0}"#).is_ok()
        );
        assert!(
            serde_json::from_str::<Usage>(
                r#"{"prompt_tokens":1,"completion_tokens":0,"cached_prompt_tokens":2}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Usage>(
                r#"{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Usage>(r#"{"prompt_tokens":-1,"completion_tokens":0}"#).is_err()
        );
        assert_eq!(Usage::ZERO, Usage::new(0, 0, 0).unwrap());
        let u = Usage::new(7, 3, 2).unwrap();
        assert_eq!(
            (
                u.prompt_tokens(),
                u.completion_tokens(),
                u.cached_prompt_tokens()
            ),
            (7, 3, 2)
        );
    }

    // ---------- 用量计价 ----------

    #[test]
    fn usage_amount_table_ceil() {
        // 输入 0.28 美元/百万、输出 0.42 美元/百万、缓存输入 0.028 美元/百万
        let rc = card(280_000, 420_000, Some(28_000));
        let amount = |p, c, cached| {
            usage_amount(&rc, &Usage::new(p, c, cached).unwrap(), Rounding::Ceil)
                .unwrap()
                .micros()
        };
        assert_eq!(amount(0, 0, 0), 0);
        assert_eq!(amount(1, 0, 0), 1); // 0.28 微美元，向上取到 1，不是 0
        assert_eq!(amount(0, 1, 0), 1); // 0.42 -> 1
        assert_eq!(amount(1_000_000, 0, 0), 280_000);
        assert_eq!(amount(0, 1_000_000, 0), 420_000);
        assert_eq!(amount(1_000_000, 1_000_000, 0), 700_000);
        // 1500 入 + 400 出（教育场景一轮）：420 + 168 = 588 微美元，整除
        assert_eq!(amount(1_500, 400, 0), 588);
        // 缓存命中：1000 入里 600 命中 -> 400 × 0.28 + 600 × 0.028 = 112 + 16.8 = 128.8 -> 129
        assert_eq!(amount(1_000, 0, 600), 129);
    }

    #[test]
    fn usage_amount_without_cache_rate_bills_cached_tokens_at_the_input_rate() {
        let rc = card(280_000, 420_000, None);
        let with_cached =
            usage_amount(&rc, &Usage::new(1_000, 0, 600).unwrap(), Rounding::Ceil).unwrap();
        let without = usage_amount(&rc, &Usage::new(1_000, 0, 0).unwrap(), Rounding::Ceil).unwrap();
        assert_eq!(with_cached, without);
    }

    #[test]
    fn rounding_mode_matters_only_for_the_fractional_part() {
        let rc = card(280_000, 0, None);
        let u = Usage::new(1, 0, 0).unwrap();
        assert_eq!(usage_amount(&rc, &u, Rounding::Floor).unwrap().micros(), 0);
        assert_eq!(usage_amount(&rc, &u, Rounding::Ceil).unwrap().micros(), 1);
        assert_eq!(
            usage_amount(&rc, &u, Rounding::HalfAwayFromZero)
                .unwrap()
                .micros(),
            0
        );
        let u2 = Usage::new(2, 0, 0).unwrap(); // 0.56
        assert_eq!(
            usage_amount(&rc, &u2, Rounding::HalfAwayFromZero)
                .unwrap()
                .micros(),
            1
        );
        assert_eq!(USAGE_ROUNDING, Rounding::Ceil);
    }

    #[test]
    fn usage_amount_overflow_is_an_error_not_a_panic() {
        let rc = card(i64::MAX, i64::MAX, None);
        let huge = Usage::new(u64::MAX, u64::MAX, 0).unwrap();
        assert_eq!(
            usage_amount(&rc, &huge, Rounding::Ceil).unwrap_err(),
            MoneyError::Overflow
        );
        // 乘积放得进 i128，但 / 1e6 之后仍然放不进 i64
        let big = Usage::new(u64::MAX, 0, 0).unwrap();
        assert_eq!(
            usage_amount(&card(i64::MAX, 0, None), &big, Rounding::Ceil).unwrap_err(),
            MoneyError::Overflow
        );
        // 最大 token 数 × 0 价格 = 0，不是错误
        assert_eq!(
            usage_amount(&card(0, 0, None), &huge, Rounding::Ceil)
                .unwrap()
                .micros(),
            0
        );
        // 刚好放得下的最大值：1_000_000 token × i64::MAX 微/百万 = i64::MAX
        assert_eq!(
            usage_amount(
                &card(i64::MAX, 0, None),
                &Usage::new(1_000_000, 0, 0).unwrap(),
                Rounding::Ceil
            )
            .unwrap()
            .micros(),
            i64::MAX
        );
    }

    #[test]
    fn amount_for_tokens_bills_all_input_at_the_input_rate() {
        let rc = card(280_000, 420_000, Some(1));
        assert_eq!(
            amount_for_tokens(&rc, 1_000, 500, Rounding::Ceil)
                .unwrap()
                .micros(),
            490 // 280 + 210
        );
        assert_eq!(
            usage_amount(&rc, &Usage::new(1_000, 500, 0).unwrap(), Rounding::Ceil)
                .unwrap()
                .micros(),
            490
        );
    }

    #[test]
    fn cost_and_price_use_the_same_formula() {
        let price_card = card(280_000, 420_000, None);
        let cost_card: RateCard<Cost> =
            RateCard::new(usd_cost(280_000), usd_cost(420_000), None).unwrap();
        let u = Usage::new(1_234, 567, 0).unwrap();
        let p = usage_amount(&price_card, &u, Rounding::Ceil).unwrap();
        let c = usage_amount(&cost_card, &u, Rounding::Ceil).unwrap();
        assert_eq!(p.micros(), c.micros());
    }

    proptest! {
        #[test]
        fn ceil_is_floor_or_floor_plus_one_and_monotone(
            input in 0i64..10_000_000,
            output in 0i64..10_000_000,
            cached_rate in 0i64..10_000_000,
            prompt in 0u64..5_000_000,
            completion in 0u64..5_000_000,
            cached_pct in 0u64..=100,
            extra in 0u64..1_000,
        ) {
            let cached = prompt * cached_pct / 100;
            let rc = card(input, output, Some(cached_rate));
            let u = Usage::new(prompt, completion, cached).unwrap();
            let floor = usage_amount(&rc, &u, Rounding::Floor).unwrap().micros();
            let ceil = usage_amount(&rc, &u, Rounding::Ceil).unwrap().micros();
            prop_assert!(ceil == floor || ceil == floor + 1);
            // 多出来的 token 只会让金额不减
            let bigger = Usage::new(prompt + extra, completion + extra, cached).unwrap();
            let ceil_bigger = usage_amount(&rc, &bigger, Rounding::Ceil).unwrap().micros();
            prop_assert!(ceil_bigger >= ceil);
            // 向上取整对加法是次可加的：整笔算一次 <= 分两笔各算一次再加
            let a = usage_amount(&rc, &Usage::new(prompt, 0, cached).unwrap(), Rounding::Ceil).unwrap().micros();
            let b = usage_amount(&rc, &Usage::new(0, completion, 0).unwrap(), Rounding::Ceil).unwrap().micros();
            prop_assert!(ceil <= a + b);
        }
    }
}
