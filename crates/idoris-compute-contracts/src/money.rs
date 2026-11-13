//! 带类型的金额（设计稿 §4，评审 H3）。
//!
//! 规则（每一条都有测试）：
//!
//! - **单位只有三种**：`THB`、`USD`、`QUOTA:<节点>`。线上形式就是这个字符串，大小写敏感。
//!   不同节点的 `QUOTA` 是**不同的单位**，不能相加。
//! - **微单位整数**：`micros` 是该单位的 1e-6，`i64`。不使用任何浮点。
//!   一次小调用的成本远小于 1 分钱，按分取整会严重失真，所以内部永远不按分取整。
//! - **不同单位之间的加、减、比较，一律返回 [`MoneyError::UnitMismatch`]**。
//!   不 panic，不静默换算。故意**没有**给 [`Money`] 实现 `Add`/`Sub`/`PartialOrd`/`Ord`，
//!   这样 `a + b`、`a < b`、`a.max(b)` 根本写不出来，只能走返回 `Result` 的方法。
//! - **溢出**：所有算术都是 checked，溢出返回 [`MoneyError::Overflow`]。
//! - **换算必须显式**：只有 [`convert`] 一条路，必须带 [`FxRate`]（含取值时间），结果把用到的汇率
//!   一起带回去，供账本记账。`QUOTA` 永远不能换成货币，也不能反过来（PGL 红线：不可兑现）。
//! - **对外展示**：[`Money::display`] 才做四舍五入，规则是 [`DISPLAY_ROUNDING`]（半值远离零），
//!   精度见 [`Unit::display_decimals`]。内部账目不受它影响。
//!
//! `cost`（上游成本）和 `price`（对用户的价）是另外两个类型，见 [`crate::pricing`]。

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{IdError, NodeId, TimestampMs};

/// 1 个单位 = 1_000_000 微单位。
pub const MICROS_PER_UNIT: i64 = 1_000_000;

/// 汇率定点刻度：`rate_e9 = 汇率 × 1e9`。
pub const FX_RATE_SCALE: u64 = 1_000_000_000;

/// 汇率上限（`rate_e9`），约 1e6 倍。防止手滑填成天文数字后在后面的乘法里溢出。
pub const MAX_FX_RATE_E9: u64 = 1_000_000_000_000_000;

/// 对外展示用的取整规则：四舍五入，**半值远离零**（0.5 → 1，-0.5 → -1），正负对称。
pub const DISPLAY_ROUNDING: Rounding = Rounding::HalfAwayFromZero;

/// 计量单位。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unit {
    Thb,
    Usd,
    /// 组织配额，单位是「按本节点价目表折算的 token 当量」（设计稿 §4）。
    Quota(NodeId),
}

impl Unit {
    /// 货币（`THB`/`USD`）为 true，`QUOTA` 为 false。
    pub fn is_currency(&self) -> bool {
        matches!(self, Unit::Thb | Unit::Usd)
    }

    pub fn is_quota(&self) -> bool {
        matches!(self, Unit::Quota(_))
    }

    pub fn quota_node(&self) -> Option<&NodeId> {
        match self {
            Unit::Quota(node) => Some(node),
            _ => None,
        }
    }

    /// 对外展示保留的小数位：泰铢、美元 2 位（satang / cent），配额 0 位（整数个 token 当量）。
    pub fn display_decimals(&self) -> u8 {
        match self {
            Unit::Thb | Unit::Usd => 2,
            Unit::Quota(_) => 0,
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unit::Thb => f.write_str("THB"),
            Unit::Usd => f.write_str("USD"),
            Unit::Quota(node) => write!(f, "QUOTA:{node}"),
        }
    }
}

impl FromStr for Unit {
    type Err = MoneyError;
    fn from_str(s: &str) -> Result<Self, MoneyError> {
        match s {
            "THB" => Ok(Unit::Thb),
            "USD" => Ok(Unit::Usd),
            other => match other.strip_prefix("QUOTA:") {
                Some(node) => NodeId::new(node)
                    .map(Unit::Quota)
                    .map_err(MoneyError::InvalidQuotaNode),
                None => Err(MoneyError::UnknownUnit(other.to_string())),
            },
        }
    }
}

impl Serialize for Unit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Unit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// 金额运算与换算的错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("unit mismatch: {left} vs {right}")]
    UnitMismatch { left: Unit, right: Unit },
    #[error("amount overflow")]
    Overflow,
    #[error("amount must not be negative: {micros} micro-{unit}")]
    Negative { unit: Unit, micros: i64 },
    #[error("unknown unit {0:?} (expected THB, USD or QUOTA:<node>)")]
    UnknownUnit(String),
    #[error("invalid node id in QUOTA unit: {0}")]
    InvalidQuotaNode(IdError),
    #[error("QUOTA can neither be converted to nor from a currency (not redeemable)")]
    QuotaNotConvertible,
    #[error("fx rate must convert between two different currencies")]
    SameCurrency,
    #[error("fx rate_e9 {rate_e9} out of range (1..={MAX_FX_RATE_E9})")]
    InvalidFxRate { rate_e9: u64 },
    #[error("fx rate is dated in the future: as_of {as_of:?} > now {now:?}")]
    FxFromFuture {
        as_of: TimestampMs,
        now: TimestampMs,
    },
    #[error("fx rate is stale: age {age_ms} ms > max {max_age_ms} ms")]
    FxStale { age_ms: i64, max_age_ms: i64 },
    #[error("max fx age must not be negative: {0}")]
    InvalidFxMaxAge(i64),
}

/// 取整方式。所有需要把「比微单位更细」的数量落成整数微单位的地方，都必须显式选一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rounding {
    /// 向负无穷。
    Floor,
    /// 向正无穷。用量计价（成本、售价、预留）默认用它：宁可多算 1 微单位，不少算。
    Ceil,
    /// 四舍五入，半值远离零。只用于对外展示和汇率换算。
    HalfAwayFromZero,
}

/// `numerator / denominator` 按 `mode` 取整。`denominator` 必须 > 0；溢出返回 `None`。
pub(crate) fn div_round(numerator: i128, denominator: i128, mode: Rounding) -> Option<i128> {
    if denominator <= 0 {
        return None;
    }
    let quotient = numerator.checked_div(denominator)?; // 向零截断
    let remainder = numerator.checked_rem(denominator)?;
    if remainder == 0 {
        return Some(quotient);
    }
    match mode {
        Rounding::Floor => {
            if numerator < 0 {
                quotient.checked_sub(1)
            } else {
                Some(quotient)
            }
        }
        Rounding::Ceil => {
            if numerator > 0 {
                quotient.checked_add(1)
            } else {
                Some(quotient)
            }
        }
        Rounding::HalfAwayFromZero => {
            let twice = remainder.checked_abs()?.checked_mul(2)?;
            if twice >= denominator {
                if numerator < 0 {
                    quotient.checked_sub(1)
                } else {
                    quotient.checked_add(1)
                }
            } else {
                Some(quotient)
            }
        }
    }
}

/// 带单位的金额：`unit` + 整数微单位。可以为负（余额透支到授信下限时就是负的）。
///
/// 需要「不能为负」的场合（成本、售价、额度、预留）用 [`crate::pricing::Cost`]、
/// [`crate::pricing::Price`]、[`crate::grant::Grant`]，它们在构造和反序列化时拒绝负数。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Money {
    pub unit: Unit,
    pub micros: i64,
}

impl Money {
    pub fn new(unit: Unit, micros: i64) -> Self {
        Self { unit, micros }
    }

    pub fn zero(unit: Unit) -> Self {
        Self { unit, micros: 0 }
    }

    /// 把既有的「最小货币单位」整数（`idoris-tenancy` 里 `limit_minor` / `spent_minor` 一类）换成带单位的
    /// 微单位金额：`minor × 10^(6 - display_decimals)`，即泰铢、美元 `× 10_000`（1 satang / cent =
    /// 10_000 微单位），`QUOTA` `× 1_000_000`（最小单位是 1 个 token 当量）。允许负数；溢出返回
    /// [`MoneyError::Overflow`]。
    ///
    /// **只有知道币种时才安全**。既有预算数据没有币种，调用方必须自己先确定币种再调用；
    /// 这个函数不替你猜，更不存在「默认币种」。
    pub fn from_minor(unit: Unit, minor: i64) -> Result<Self, MoneyError> {
        let factor = 10_i64.pow(u32::from(6_u8.saturating_sub(unit.display_decimals())));
        let micros = minor.checked_mul(factor).ok_or(MoneyError::Overflow)?;
        Ok(Self { unit, micros })
    }

    pub fn is_zero(&self) -> bool {
        self.micros == 0
    }

    pub fn is_negative(&self) -> bool {
        self.micros < 0
    }

    pub fn is_positive(&self) -> bool {
        self.micros > 0
    }

    /// 单位相同返回 `Ok`，否则 [`MoneyError::UnitMismatch`]。
    pub fn require_same_unit(&self, other: &Money) -> Result<(), MoneyError> {
        if self.unit == other.unit {
            Ok(())
        } else {
            Err(MoneyError::UnitMismatch {
                left: self.unit.clone(),
                right: other.unit.clone(),
            })
        }
    }

    pub fn checked_add(&self, other: &Money) -> Result<Money, MoneyError> {
        self.require_same_unit(other)?;
        let micros = self
            .micros
            .checked_add(other.micros)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(self.unit.clone(), micros))
    }

    pub fn checked_sub(&self, other: &Money) -> Result<Money, MoneyError> {
        self.require_same_unit(other)?;
        let micros = self
            .micros
            .checked_sub(other.micros)
            .ok_or(MoneyError::Overflow)?;
        Ok(Money::new(self.unit.clone(), micros))
    }

    pub fn checked_neg(&self) -> Result<Money, MoneyError> {
        let micros = self.micros.checked_neg().ok_or(MoneyError::Overflow)?;
        Ok(Money::new(self.unit.clone(), micros))
    }

    /// 比较大小；单位不同返回错误，而不是任意给个顺序。
    pub fn try_cmp(&self, other: &Money) -> Result<Ordering, MoneyError> {
        self.require_same_unit(other)?;
        Ok(self.micros.cmp(&other.micros))
    }

    pub fn try_min(&self, other: &Money) -> Result<Money, MoneyError> {
        Ok(match self.try_cmp(other)? {
            Ordering::Greater => other.clone(),
            _ => self.clone(),
        })
    }

    pub fn try_max(&self, other: &Money) -> Result<Money, MoneyError> {
        Ok(match self.try_cmp(other)? {
            Ordering::Less => other.clone(),
            _ => self.clone(),
        })
    }

    /// 同单位求和；空迭代器得到 `zero(unit)`。任何一项单位不同或溢出都返回错误。
    pub fn checked_sum<'a>(
        unit: Unit,
        items: impl IntoIterator<Item = &'a Money>,
    ) -> Result<Money, MoneyError> {
        let mut total = Money::zero(unit);
        for item in items {
            total = total.checked_add(item)?;
        }
        Ok(total)
    }

    /// 对外展示：按 [`Unit::display_decimals`] 和 [`DISPLAY_ROUNDING`] 取整。
    /// 只用于展示；内部账目、比较、换算一律用 `micros`。
    pub fn display(&self) -> DisplayAmount {
        let decimals = self.unit.display_decimals();
        let divisor = 10_i128.pow(u32::from(6_u8.saturating_sub(decimals)));
        // `divisor` 是 1e4 或 1e6，> 0，且 |micros| <= 2^63，所以这里的运算不会溢出；
        // 结果的量级只会更小，一定放得进 i64。下面两处 `unwrap_or` 只是不 panic 的兜底。
        let minor = div_round(i128::from(self.micros), divisor, DISPLAY_ROUNDING).unwrap_or(0);
        DisplayAmount {
            unit: self.unit.clone(),
            minor_units: i64::try_from(minor).unwrap_or(0),
            decimals,
        }
    }
}

/// 对外展示用的金额：`minor_units` 是最小展示单位（satang / cent / 1 个 token 当量）的个数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayAmount {
    pub unit: Unit,
    pub minor_units: i64,
    pub decimals: u8,
}

impl fmt::Display for DisplayAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.minor_units < 0;
        let abs = i128::from(self.minor_units).unsigned_abs();
        let sign = if negative { "-" } else { "" };
        if self.decimals == 0 {
            write!(f, "{} {sign}{abs}", self.unit)
        } else {
            let scale = 10_u128.pow(u32::from(self.decimals));
            write!(
                f,
                "{} {sign}{}.{:0width$}",
                self.unit,
                abs / scale,
                abs % scale,
                width = usize::from(self.decimals)
            )
        }
    }
}

/// 汇率：1 个 `from` = `rate_e9 / 1e9` 个 `to`，连同取值时间 `as_of` 一起记。
///
/// 设计稿 §4：「需要换算时，记下汇率和取值时间」。所以 [`convert`] 的结果会把整条 `FxRate`
/// 带回去，账本照原样存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "FxRateWire")]
pub struct FxRate {
    from: Unit,
    to: Unit,
    rate_e9: u64,
    as_of: TimestampMs,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FxRateWire {
    from: Unit,
    to: Unit,
    rate_e9: u64,
    as_of: TimestampMs,
}

impl TryFrom<FxRateWire> for FxRate {
    type Error = MoneyError;
    fn try_from(wire: FxRateWire) -> Result<Self, MoneyError> {
        FxRate::new(wire.from, wire.to, wire.rate_e9, wire.as_of)
    }
}

impl FxRate {
    /// 两端都必须是货币（`QUOTA` 不可兑现）、且互不相同；`rate_e9` 在 `1..=MAX_FX_RATE_E9`。
    pub fn new(from: Unit, to: Unit, rate_e9: u64, as_of: TimestampMs) -> Result<Self, MoneyError> {
        if !from.is_currency() || !to.is_currency() {
            return Err(MoneyError::QuotaNotConvertible);
        }
        if from == to {
            return Err(MoneyError::SameCurrency);
        }
        if rate_e9 == 0 || rate_e9 > MAX_FX_RATE_E9 {
            return Err(MoneyError::InvalidFxRate { rate_e9 });
        }
        Ok(Self {
            from,
            to,
            rate_e9,
            as_of,
        })
    }

    pub fn from_unit(&self) -> &Unit {
        &self.from
    }

    pub fn to_unit(&self) -> &Unit {
        &self.to
    }

    pub fn rate_e9(&self) -> u64 {
        self.rate_e9
    }

    pub fn as_of(&self) -> TimestampMs {
        self.as_of
    }
}

/// 换算结果：新金额 + 用到的汇率（含取值时间）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Converted<T> {
    pub amount: T,
    pub rate: FxRate,
}

/// 把 `micros`（单位须为 `fx.from`）按汇率换成 `fx.to` 的微单位。
///
/// 供 [`convert`] 和 `Cost`/`Price` 的 `convert` 共用。检查顺序固定：
/// 单位 → 汇率不是未来 → 汇率没过期 → 计算。
pub(crate) fn convert_micros(
    unit: &Unit,
    micros: i64,
    fx: &FxRate,
    now: TimestampMs,
    max_age_ms: i64,
    rounding: Rounding,
) -> Result<i64, MoneyError> {
    if *unit != fx.from {
        return Err(MoneyError::UnitMismatch {
            left: unit.clone(),
            right: fx.from.clone(),
        });
    }
    if max_age_ms < 0 {
        return Err(MoneyError::InvalidFxMaxAge(max_age_ms));
    }
    if fx.as_of > now {
        return Err(MoneyError::FxFromFuture {
            as_of: fx.as_of,
            now,
        });
    }
    let age_ms = now.checked_since(fx.as_of).ok_or(MoneyError::Overflow)?;
    if age_ms > max_age_ms {
        return Err(MoneyError::FxStale { age_ms, max_age_ms });
    }
    let numerator = i128::from(micros)
        .checked_mul(i128::from(fx.rate_e9))
        .ok_or(MoneyError::Overflow)?;
    let rounded =
        div_round(numerator, i128::from(FX_RATE_SCALE), rounding).ok_or(MoneyError::Overflow)?;
    i64::try_from(rounded).map_err(|_| MoneyError::Overflow)
}

/// 显式换算：必须给出汇率、「现在」和汇率最大允许年龄。
///
/// - 单位必须等于 `fx.from`，否则 [`MoneyError::UnitMismatch`]（不会默默换成别的币种）；
/// - `fx.as_of > now` 拒绝（未来的汇率不可信），`now - fx.as_of > max_age_ms` 拒绝（过期）；
///   刚好等于 `max_age_ms` 仍然可用；
/// - `QUOTA` 在 [`FxRate::new`] 就进不来，所以配额不可能被换算。
pub fn convert(
    amount: &Money,
    fx: &FxRate,
    now: TimestampMs,
    max_age_ms: i64,
    rounding: Rounding,
) -> Result<Converted<Money>, MoneyError> {
    let micros = convert_micros(&amount.unit, amount.micros, fx, now, max_age_ms, rounding)?;
    Ok(Converted {
        amount: Money::new(fx.to.clone(), micros),
        rate: fx.clone(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use proptest::prelude::*;

    fn thb(micros: i64) -> Money {
        Money::new(Unit::Thb, micros)
    }
    fn usd(micros: i64) -> Money {
        Money::new(Unit::Usd, micros)
    }
    fn quota(node: &str, micros: i64) -> Money {
        Money::new(Unit::Quota(NodeId::new(node).unwrap()), micros)
    }

    // ---------- Unit ----------

    #[test]
    fn unit_parse_display_round_trip_table() {
        for (text, unit) in [
            ("THB", Unit::Thb),
            ("USD", Unit::Usd),
            ("QUOTA:org-1", Unit::Quota(NodeId::new("org-1").unwrap())),
            (
                "QUOTA:a.b_c-9",
                Unit::Quota(NodeId::new("a.b_c-9").unwrap()),
            ),
        ] {
            assert_eq!(text.parse::<Unit>().unwrap(), unit);
            assert_eq!(unit.to_string(), text);
            assert_eq!(serde_json::to_string(&unit).unwrap(), format!("\"{text}\""));
            assert_eq!(
                serde_json::from_str::<Unit>(&format!("\"{text}\"")).unwrap(),
                unit
            );
        }
    }

    #[test]
    fn unit_rejects_everything_else() {
        for bad in [
            "",
            "thb",
            "Thb",
            "usd",
            "EUR",
            "CNY",
            "QUOTA",
            "QUOTA:",
            "quota:x",
            "QUOTA:a:b",
            "QUOTA:a b",
            " THB",
            "THB ",
            "QUOTA:学生",
        ] {
            assert!(bad.parse::<Unit>().is_err(), "{bad:?} must be rejected");
            assert!(serde_json::from_str::<Unit>(&format!("{bad:?}")).is_err());
        }
        assert!(matches!(
            "EUR".parse::<Unit>().unwrap_err(),
            MoneyError::UnknownUnit(_)
        ));
        assert!(matches!(
            "QUOTA:".parse::<Unit>().unwrap_err(),
            MoneyError::InvalidQuotaNode(IdError::Empty { .. })
        ));
        assert!(serde_json::from_str::<Unit>("1").is_err());
    }

    #[test]
    fn unit_predicates() {
        assert!(Unit::Thb.is_currency() && Unit::Usd.is_currency());
        let q = quota("n", 0).unit;
        assert!(q.is_quota() && !q.is_currency());
        assert_eq!(q.quota_node().unwrap().as_str(), "n");
        assert!(Unit::Thb.quota_node().is_none());
        assert_eq!(Unit::Thb.display_decimals(), 2);
        assert_eq!(Unit::Usd.display_decimals(), 2);
        assert_eq!(q.display_decimals(), 0);
    }

    // ---------- 单位不一致必须被拒绝 ----------

    #[test]
    fn every_operation_rejects_unit_mismatch_for_every_unit_pair() {
        let all = [thb(5), usd(5), quota("a", 5), quota("b", 5)];
        for (i, left) in all.iter().enumerate() {
            for (j, right) in all.iter().enumerate() {
                if i == j {
                    assert_eq!(left.checked_add(right).unwrap().micros, 10);
                    assert_eq!(left.checked_sub(right).unwrap().micros, 0);
                    assert_eq!(left.try_cmp(right).unwrap(), Ordering::Equal);
                    continue;
                }
                let expected = MoneyError::UnitMismatch {
                    left: left.unit.clone(),
                    right: right.unit.clone(),
                };
                assert_eq!(left.checked_add(right).unwrap_err(), expected);
                assert_eq!(left.checked_sub(right).unwrap_err(), expected);
                assert_eq!(left.try_cmp(right).unwrap_err(), expected);
                assert_eq!(left.try_min(right).unwrap_err(), expected);
                assert_eq!(left.try_max(right).unwrap_err(), expected);
                assert_eq!(
                    Money::checked_sum(left.unit.clone(), [left, right]).unwrap_err(),
                    expected
                );
            }
        }
    }

    #[test]
    fn different_quota_nodes_are_different_units() {
        assert_ne!(quota("a", 1).unit, quota("b", 1).unit);
        assert!(quota("a", 1).checked_add(&quota("b", 1)).is_err());
    }

    // ---------- 溢出 ----------

    #[test]
    fn overflow_is_an_error_not_a_panic_or_wraparound() {
        assert_eq!(
            thb(i64::MAX).checked_add(&thb(1)).unwrap_err(),
            MoneyError::Overflow
        );
        assert_eq!(
            thb(i64::MIN).checked_sub(&thb(1)).unwrap_err(),
            MoneyError::Overflow
        );
        assert_eq!(
            thb(i64::MIN).checked_neg().unwrap_err(),
            MoneyError::Overflow
        );
        assert_eq!(thb(i64::MAX).checked_add(&thb(0)).unwrap().micros, i64::MAX);
        assert_eq!(thb(i64::MAX).checked_sub(&thb(i64::MAX)).unwrap().micros, 0);
        assert_eq!(thb(i64::MAX).checked_neg().unwrap().micros, -i64::MAX);
        let items = [thb(i64::MAX), thb(1)];
        assert_eq!(
            Money::checked_sum(Unit::Thb, items.iter()).unwrap_err(),
            MoneyError::Overflow
        );
        assert_eq!(Money::checked_sum(Unit::Thb, []).unwrap(), thb(0));
    }

    #[test]
    fn money_may_be_negative_and_zero_predicates_work() {
        assert!(thb(-1).is_negative() && !thb(-1).is_positive() && !thb(-1).is_zero());
        assert!(thb(0).is_zero() && !thb(0).is_negative() && !thb(0).is_positive());
        assert!(thb(1).is_positive());
        assert_eq!(thb(3).try_min(&thb(-4)).unwrap(), thb(-4));
        assert_eq!(thb(3).try_max(&thb(-4)).unwrap(), thb(3));
    }

    // ---------- 对外展示取整 ----------

    #[test]
    fn display_rounds_half_away_from_zero_table() {
        // (单位, micros, 期望 minor_units, 期望字符串)
        let cases: Vec<(Money, i64, &str)> = vec![
            (thb(0), 0, "THB 0.00"),
            (thb(4_999), 0, "THB 0.00"),
            (thb(5_000), 1, "THB 0.01"), // 半值进位
            (thb(5_001), 1, "THB 0.01"),
            (thb(14_999), 1, "THB 0.01"),
            (thb(15_000), 2, "THB 0.02"),
            (thb(1_234_500), 123, "THB 1.23"),
            (thb(1_235_000), 124, "THB 1.24"), // 1.235 -> 1.24
            (thb(-4_999), 0, "THB 0.00"),
            (thb(-5_000), -1, "THB -0.01"), // 对称：远离零
            (thb(-15_000), -2, "THB -0.02"),
            (thb(-1_235_000), -124, "THB -1.24"),
            (usd(999_999), 100, "USD 1.00"),
            (usd(1_000_000), 100, "USD 1.00"),
            (quota("n", 0), 0, "QUOTA:n 0"),
            (quota("n", 499_999), 0, "QUOTA:n 0"),
            (quota("n", 500_000), 1, "QUOTA:n 1"),
            (quota("n", 1_499_999), 1, "QUOTA:n 1"),
            (quota("n", 1_500_000), 2, "QUOTA:n 2"),
            (quota("n", -500_000), -1, "QUOTA:n -1"),
        ];
        for (money, minor, text) in cases {
            let shown = money.display();
            assert_eq!(shown.minor_units, minor, "{money:?}");
            assert_eq!(shown.to_string(), text, "{money:?}");
        }
    }

    #[test]
    fn from_minor_converts_known_units_exactly_and_checks_overflow() {
        // (单位, minor, 期望微单位)
        for (unit, minor, micros) in [
            (Unit::Thb, 0, 0),
            (Unit::Thb, 1, 10_000),
            (Unit::Thb, 150, 1_500_000),
            (Unit::Usd, -250, -2_500_000),
            (quota("n", 0).unit, 3, 3_000_000),
            (quota("n", 0).unit, -2, -2_000_000),
        ] {
            let m = Money::from_minor(unit.clone(), minor).unwrap();
            assert_eq!(m, Money::new(unit, micros));
        }
        // 溢出边界：THB 因子 10_000，QUOTA 因子 1_000_000
        let max_thb = i64::MAX / 10_000;
        assert_eq!(
            Money::from_minor(Unit::Thb, max_thb).unwrap().micros,
            max_thb * 10_000
        );
        assert_eq!(
            Money::from_minor(Unit::Thb, max_thb + 1).unwrap_err(),
            MoneyError::Overflow
        );
        assert_eq!(
            Money::from_minor(Unit::Thb, i64::MIN).unwrap_err(),
            MoneyError::Overflow
        );
        let q = quota("n", 0).unit;
        assert_eq!(
            Money::from_minor(q.clone(), i64::MAX / 1_000_000 + 1).unwrap_err(),
            MoneyError::Overflow
        );
        assert!(Money::from_minor(q, i64::MAX / 1_000_000).is_ok());
    }

    proptest! {
        /// `from_minor` 与展示是互逆的：换成微单位再展示，得到的 minor 就是原来的 minor（不丢精度）。
        #[test]
        fn from_minor_then_display_round_trips(minor in -9_000_000_000_000i64..9_000_000_000_000) {
            for unit in [Unit::Thb, Unit::Usd] {
                let m = Money::from_minor(unit, minor).unwrap();
                prop_assert_eq!(m.display().minor_units, minor);
            }
        }
    }

    #[test]
    fn display_never_panics_at_the_extremes() {
        let max = thb(i64::MAX).display();
        assert_eq!(max.minor_units, 922_337_203_685_478); // 9.22e18 micro / 1e4, 四舍五入
        let min = thb(i64::MIN).display();
        assert_eq!(min.minor_units, -922_337_203_685_478);
        assert_eq!(
            quota("n", i64::MAX).display().minor_units,
            9_223_372_036_855
        );
        assert_eq!(max.to_string(), "THB 9223372036854.78");
        assert_eq!(min.to_string(), "THB -9223372036854.78");
    }

    // ---------- div_round ----------

    #[test]
    fn div_round_table() {
        use Rounding::*;
        // (numerator, denominator, floor, ceil, half_away)
        let cases: [(i128, i128, i128, i128, i128); 9] = [
            (7, 2, 3, 4, 4),
            (-7, 2, -4, -3, -4),
            (5, 2, 2, 3, 3),
            (-5, 2, -3, -2, -3),
            (1, 3, 0, 1, 0),
            (2, 3, 0, 1, 1),
            (-1, 3, -1, 0, 0),
            (6, 3, 2, 2, 2),
            (0, 7, 0, 0, 0),
        ];
        for (n, d, floor, ceil, half) in cases {
            assert_eq!(div_round(n, d, Floor), Some(floor), "{n}/{d} floor");
            assert_eq!(div_round(n, d, Ceil), Some(ceil), "{n}/{d} ceil");
            assert_eq!(
                div_round(n, d, HalfAwayFromZero),
                Some(half),
                "{n}/{d} half"
            );
        }
        assert_eq!(div_round(1, 0, Ceil), None);
        assert_eq!(div_round(1, -2, Ceil), None);
        assert_eq!(div_round(i128::MIN, 1, Ceil), Some(i128::MIN));
        assert_eq!(
            div_round(i128::MIN, 2, HalfAwayFromZero),
            Some(i128::MIN / 2)
        );
    }

    proptest! {
        #[test]
        fn div_round_modes_bracket_the_exact_quotient(
            n in -1_000_000_000_000i128..1_000_000_000_000,
            d in 1i128..1_000_000,
        ) {
            let floor = div_round(n, d, Rounding::Floor).unwrap();
            let ceil = div_round(n, d, Rounding::Ceil).unwrap();
            let half = div_round(n, d, Rounding::HalfAwayFromZero).unwrap();
            // floor * d <= n <= ceil * d，且相差不超过 1
            prop_assert!(floor * d <= n && n <= ceil * d);
            prop_assert!(ceil - floor <= 1);
            prop_assert!(half == floor || half == ceil);
            if n % d == 0 {
                prop_assert_eq!(floor, ceil);
            }
            // 半值远离零是奇函数：round(-x) = -round(x)
            prop_assert_eq!(div_round(-n, d, Rounding::HalfAwayFromZero).unwrap(), -half);
        }

        #[test]
        fn add_then_sub_round_trips_when_it_does_not_overflow(a in any::<i64>(), b in any::<i64>()) {
            let x = thb(a);
            let y = thb(b);
            match x.checked_add(&y) {
                Ok(sum) => {
                    prop_assert_eq!(sum.checked_sub(&y).unwrap(), x.clone());
                    prop_assert_eq!(y.checked_add(&x).unwrap(), sum); // 交换律
                }
                Err(e) => prop_assert_eq!(e, MoneyError::Overflow),
            }
        }

        #[test]
        fn try_cmp_matches_integer_order_within_a_unit(a in any::<i64>(), b in any::<i64>()) {
            prop_assert_eq!(thb(a).try_cmp(&thb(b)).unwrap(), a.cmp(&b));
            // 单位不同时无论数值如何都是错误
            prop_assert!(thb(a).try_cmp(&usd(b)).is_err());
        }
    }

    // ---------- FX ----------

    fn fx(from: Unit, to: Unit, rate_e9: u64, as_of: i64) -> FxRate {
        FxRate::new(from, to, rate_e9, TimestampMs(as_of)).unwrap()
    }

    #[test]
    fn fx_rate_construction_rules() {
        let q = Unit::Quota(NodeId::new("n").unwrap());
        assert_eq!(
            FxRate::new(Unit::Thb, q.clone(), 1, TimestampMs(0)).unwrap_err(),
            MoneyError::QuotaNotConvertible
        );
        assert_eq!(
            FxRate::new(q, Unit::Usd, 1, TimestampMs(0)).unwrap_err(),
            MoneyError::QuotaNotConvertible
        );
        assert_eq!(
            FxRate::new(Unit::Thb, Unit::Thb, 1, TimestampMs(0)).unwrap_err(),
            MoneyError::SameCurrency
        );
        assert_eq!(
            FxRate::new(Unit::Thb, Unit::Usd, 0, TimestampMs(0)).unwrap_err(),
            MoneyError::InvalidFxRate { rate_e9: 0 }
        );
        assert!(FxRate::new(Unit::Thb, Unit::Usd, MAX_FX_RATE_E9, TimestampMs(0)).is_ok());
        assert_eq!(
            FxRate::new(Unit::Thb, Unit::Usd, MAX_FX_RATE_E9 + 1, TimestampMs(0)).unwrap_err(),
            MoneyError::InvalidFxRate {
                rate_e9: MAX_FX_RATE_E9 + 1
            }
        );
    }

    #[test]
    fn fx_rate_deserialization_runs_the_same_validation() {
        let ok = r#"{"from":"THB","to":"USD","rate_e9":28000000,"as_of":1000}"#;
        assert!(serde_json::from_str::<FxRate>(ok).is_ok());
        for bad in [
            r#"{"from":"THB","to":"THB","rate_e9":1,"as_of":0}"#,
            r#"{"from":"QUOTA:n","to":"USD","rate_e9":1,"as_of":0}"#,
            r#"{"from":"THB","to":"USD","rate_e9":0,"as_of":0}"#,
            r#"{"from":"THB","to":"USD","rate_e9":1,"as_of":0,"extra":1}"#,
        ] {
            assert!(serde_json::from_str::<FxRate>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn convert_applies_rate_and_returns_the_rate_it_used() {
        // 1 THB = 0.028 USD；35.5 THB = 0.994 USD
        let rate = fx(Unit::Thb, Unit::Usd, 28_000_000, 10_000);
        let out = convert(
            &thb(35_500_000),
            &rate,
            TimestampMs(10_000),
            0,
            Rounding::Floor,
        )
        .unwrap();
        assert_eq!(out.amount, usd(994_000));
        assert_eq!(out.rate, rate);
        assert_eq!(out.rate.as_of(), TimestampMs(10_000));
        assert_eq!(out.rate.rate_e9(), 28_000_000);
        assert_eq!(out.rate.from_unit(), &Unit::Thb);
        assert_eq!(out.rate.to_unit(), &Unit::Usd);
    }

    #[test]
    fn convert_rounding_modes_differ_only_on_the_fraction() {
        // 3 micro THB * 0.5 = 1.5 micro USD
        let rate = fx(Unit::Thb, Unit::Usd, 500_000_000, 0);
        let now = TimestampMs(0);
        let go = |mode| convert(&thb(3), &rate, now, 0, mode).unwrap().amount.micros;
        assert_eq!(go(Rounding::Floor), 1);
        assert_eq!(go(Rounding::Ceil), 2);
        assert_eq!(go(Rounding::HalfAwayFromZero), 2);
        let go_neg = |mode| {
            convert(&thb(-3), &rate, now, 0, mode)
                .unwrap()
                .amount
                .micros
        };
        assert_eq!(go_neg(Rounding::Floor), -2);
        assert_eq!(go_neg(Rounding::Ceil), -1);
        assert_eq!(go_neg(Rounding::HalfAwayFromZero), -2);
    }

    #[test]
    fn convert_rejects_wrong_unit_future_and_stale_rates() {
        let rate = fx(Unit::Thb, Unit::Usd, 28_000_000, 1_000);
        // 单位不是 fx.from：不会替你换成别的币种
        assert_eq!(
            convert(&usd(1), &rate, TimestampMs(1_000), 0, Rounding::Ceil).unwrap_err(),
            MoneyError::UnitMismatch {
                left: Unit::Usd,
                right: Unit::Thb
            }
        );
        assert!(convert(&quota("n", 1), &rate, TimestampMs(1_000), 0, Rounding::Ceil).is_err());
        // 未来的汇率
        assert_eq!(
            convert(&thb(1), &rate, TimestampMs(999), 10, Rounding::Ceil).unwrap_err(),
            MoneyError::FxFromFuture {
                as_of: TimestampMs(1_000),
                now: TimestampMs(999)
            }
        );
        // 过期边界：年龄刚好等于上限可用，多 1 毫秒就拒绝
        assert!(convert(&thb(1), &rate, TimestampMs(1_100), 100, Rounding::Ceil).is_ok());
        assert_eq!(
            convert(&thb(1), &rate, TimestampMs(1_101), 100, Rounding::Ceil).unwrap_err(),
            MoneyError::FxStale {
                age_ms: 101,
                max_age_ms: 100
            }
        );
        // 负的最大年龄是调用方的错
        assert_eq!(
            convert(&thb(1), &rate, TimestampMs(1_000), -1, Rounding::Ceil).unwrap_err(),
            MoneyError::InvalidFxMaxAge(-1)
        );
    }

    #[test]
    fn convert_overflow_is_an_error() {
        let rate = fx(Unit::Thb, Unit::Usd, MAX_FX_RATE_E9, 0);
        assert_eq!(
            convert(&thb(i64::MAX), &rate, TimestampMs(0), 0, Rounding::Ceil).unwrap_err(),
            MoneyError::Overflow
        );
        // 汇率 1e-9（rate_e9 = 1）：最大金额也放得下
        let tiny = fx(Unit::Thb, Unit::Usd, 1, 0);
        assert_eq!(
            convert(&thb(i64::MAX), &tiny, TimestampMs(0), 0, Rounding::Floor)
                .unwrap()
                .amount
                .micros,
            9_223_372_036
        );
    }

    #[test]
    fn money_json_shape_and_strictness() {
        let json = serde_json::to_string(&thb(1_500_000)).unwrap();
        assert_eq!(json, r#"{"unit":"THB","micros":1500000}"#);
        assert_eq!(
            serde_json::from_str::<Money>(&json).unwrap(),
            thb(1_500_000)
        );
        // 未知字段、未知单位、浮点微单位：全部拒绝
        assert!(serde_json::from_str::<Money>(r#"{"unit":"THB","micros":1,"x":1}"#).is_err());
        assert!(serde_json::from_str::<Money>(r#"{"unit":"EUR","micros":1}"#).is_err());
        assert!(serde_json::from_str::<Money>(r#"{"unit":"THB","micros":1.5}"#).is_err());
        assert!(serde_json::from_str::<Money>(r#"{"unit":"THB"}"#).is_err());
    }
}
