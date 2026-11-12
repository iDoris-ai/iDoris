//! 额度（授权上限）里的「每一笔金额」：单位、精度、用途、有效期、不可转让（设计稿 §4）。
//!
//! 设计稿原话：「每一笔金额都带 `(单位, 精度, 用途, 有效期, 可否转让=否)`」。
//!
//! - **单位 + 精度**：[`Money`]（单位 + 微单位整数）。精度固定为微单位，不是一个可配的字段。
//! - **用途**：[`Purpose`]。
//! - **有效期**：半开区间 `[valid_from, valid_until)`。`valid_until` 必填且必须晚于 `valid_from`；
//!   没有「永不过期」这个选项（见交付说明里的含糊点）。
//! - **不可转让**：[`NonTransferable`]，线上字段 `transferable` 恒为 `false`，反序列化遇到
//!   `true` 直接拒绝。再加上本 crate 没有任何「把额度从一个账户转给另一个账户」的函数，
//!   [`crate::account::Allocation`] 只是授权上限、不是转移余额：配额不可转让、不可兑现。
//!   （「不可兑现」由 [`crate::money::FxRate`] 拒绝 `QUOTA` 保证。）

use serde::{Deserialize, Serialize};

use crate::ids::TimestampMs;
use crate::money::{Money, MoneyError, Unit};

/// 金额的用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// 免费额度：平台补贴池按月授权给成员（设计稿 §4「免费额度」）。
    FreeTier,
    /// 预付：担保人 / 用户先付的钱（P2 接支付之后才会出现）。必须是货币单位。
    Prepaid,
    /// 授信：担保人开的、允许下级透支的额度（设计稿 §5「有担保的负值」）。
    CreditLine,
    /// 组织配额：组织节点按自己的价目表折算的 token 当量。必须是 `QUOTA` 单位。
    OrgQuota,
}

impl Purpose {
    /// 线上字符串，固定不变。
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::FreeTier => "free_tier",
            Purpose::Prepaid => "prepaid",
            Purpose::CreditLine => "credit_line",
            Purpose::OrgQuota => "org_quota",
        }
    }

    /// 这个用途允许的单位。`Prepaid` 是真金白银，不能是 `QUOTA`；`OrgQuota` 只能是 `QUOTA`；
    /// 其它不限。
    pub fn allows_unit(self, unit: &Unit) -> bool {
        match self {
            Purpose::Prepaid => unit.is_currency(),
            Purpose::OrgQuota => unit.is_quota(),
            Purpose::FreeTier | Purpose::CreditLine => true,
        }
    }
}

/// 「可否转让 = 否」。只有一个值：序列化为 `false`，反序列化遇到 `true` 报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "bool", into = "bool")]
pub struct NonTransferable;

impl TryFrom<bool> for NonTransferable {
    type Error = &'static str;
    fn try_from(transferable: bool) -> Result<Self, &'static str> {
        if transferable {
            Err("grants are non-transferable: `transferable` must be false")
        } else {
            Ok(NonTransferable)
        }
    }
}

impl From<NonTransferable> for bool {
    fn from(_: NonTransferable) -> bool {
        false
    }
}

/// 额度金额的校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    #[error(transparent)]
    Money(#[from] MoneyError),
    #[error(
        "validity window must be non-empty: valid_from {valid_from:?} >= valid_until {valid_until:?}"
    )]
    EmptyWindow {
        valid_from: TimestampMs,
        valid_until: TimestampMs,
    },
    #[error("purpose {purpose:?} does not allow unit {unit}")]
    PurposeUnitMismatch { purpose: Purpose, unit: Unit },
    #[error("grant is not valid yet: now {now:?} < valid_from {valid_from:?}")]
    NotYetValid {
        now: TimestampMs,
        valid_from: TimestampMs,
    },
    #[error("grant has expired: now {now:?} >= valid_until {valid_until:?}")]
    Expired {
        now: TimestampMs,
        valid_until: TimestampMs,
    },
}

/// 一笔带用途、有效期、不可转让标记的非负金额。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GrantWire")]
pub struct Grant {
    amount: Money,
    purpose: Purpose,
    valid_from: TimestampMs,
    valid_until: TimestampMs,
    transferable: NonTransferable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantWire {
    amount: Money,
    purpose: Purpose,
    valid_from: TimestampMs,
    valid_until: TimestampMs,
    transferable: NonTransferable,
}

impl TryFrom<GrantWire> for Grant {
    type Error = GrantError;
    fn try_from(wire: GrantWire) -> Result<Self, GrantError> {
        // `transferable: NonTransferable` 在反序列化时已经拒绝了 `true`；这里解构只是把它「读掉」。
        let GrantWire {
            amount,
            purpose,
            valid_from,
            valid_until,
            transferable: NonTransferable,
        } = wire;
        Grant::new(amount, purpose, valid_from, valid_until)
    }
}

impl Grant {
    /// 校验：金额非负；用途与单位相容；窗口非空。
    pub fn new(
        amount: Money,
        purpose: Purpose,
        valid_from: TimestampMs,
        valid_until: TimestampMs,
    ) -> Result<Self, GrantError> {
        if amount.micros < 0 {
            return Err(GrantError::Money(MoneyError::Negative {
                unit: amount.unit,
                micros: amount.micros,
            }));
        }
        if !purpose.allows_unit(&amount.unit) {
            return Err(GrantError::PurposeUnitMismatch {
                purpose,
                unit: amount.unit,
            });
        }
        if valid_from >= valid_until {
            return Err(GrantError::EmptyWindow {
                valid_from,
                valid_until,
            });
        }
        Ok(Self {
            amount,
            purpose,
            valid_from,
            valid_until,
            transferable: NonTransferable,
        })
    }

    pub fn amount(&self) -> &Money {
        &self.amount
    }

    pub fn unit(&self) -> &Unit {
        &self.amount.unit
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    pub fn valid_from(&self) -> TimestampMs {
        self.valid_from
    }

    pub fn valid_until(&self) -> TimestampMs {
        self.valid_until
    }

    /// 是否在有效期内：`valid_from <= now < valid_until`（右端开）。
    pub fn is_active_at(&self, now: TimestampMs) -> bool {
        self.valid_from <= now && now < self.valid_until
    }

    /// 同上，但失败时说明是「还没到」还是「已过期」。
    pub fn check_active_at(&self, now: TimestampMs) -> Result<(), GrantError> {
        if now < self.valid_from {
            Err(GrantError::NotYetValid {
                now,
                valid_from: self.valid_from,
            })
        } else if now >= self.valid_until {
            Err(GrantError::Expired {
                now,
                valid_until: self.valid_until,
            })
        } else {
            Ok(())
        }
    }

    /// `self` 的有效期是否完全包住 `other` 的有效期。
    pub fn window_covers(&self, other: &Grant) -> bool {
        self.valid_from <= other.valid_from && other.valid_until <= self.valid_until
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ids::NodeId;

    fn thb(micros: i64) -> Money {
        Money::new(Unit::Thb, micros)
    }

    fn grant(amount: Money, purpose: Purpose, from: i64, until: i64) -> Result<Grant, GrantError> {
        Grant::new(amount, purpose, TimestampMs(from), TimestampMs(until))
    }

    #[test]
    fn validity_window_boundaries_are_half_open() {
        let g = grant(thb(1), Purpose::FreeTier, 100, 200).unwrap();
        // (now, 期望)
        for (now, active) in [
            (99, false),
            (100, true),
            (101, true),
            (199, true),
            (200, false),
            (201, false),
        ] {
            assert_eq!(g.is_active_at(TimestampMs(now)), active, "now={now}");
            assert_eq!(
                g.check_active_at(TimestampMs(now)).is_ok(),
                active,
                "now={now}"
            );
        }
        assert_eq!(
            g.check_active_at(TimestampMs(99)).unwrap_err(),
            GrantError::NotYetValid {
                now: TimestampMs(99),
                valid_from: TimestampMs(100)
            }
        );
        assert_eq!(
            g.check_active_at(TimestampMs(200)).unwrap_err(),
            GrantError::Expired {
                now: TimestampMs(200),
                valid_until: TimestampMs(200)
            }
        );
        assert_eq!(
            (g.valid_from(), g.valid_until()),
            (TimestampMs(100), TimestampMs(200))
        );
    }

    #[test]
    fn empty_or_inverted_windows_are_rejected() {
        assert_eq!(
            grant(thb(1), Purpose::FreeTier, 100, 100).unwrap_err(),
            GrantError::EmptyWindow {
                valid_from: TimestampMs(100),
                valid_until: TimestampMs(100)
            }
        );
        assert!(grant(thb(1), Purpose::FreeTier, 100, 99).is_err());
        assert!(grant(thb(1), Purpose::FreeTier, 100, 101).is_ok());
        assert!(grant(thb(1), Purpose::FreeTier, i64::MIN, i64::MAX).is_ok());
    }

    #[test]
    fn negative_amounts_are_rejected_zero_is_allowed() {
        assert!(matches!(
            grant(thb(-1), Purpose::FreeTier, 0, 1).unwrap_err(),
            GrantError::Money(MoneyError::Negative { .. })
        ));
        assert!(grant(thb(0), Purpose::FreeTier, 0, 1).is_ok());
        assert!(grant(thb(i64::MAX), Purpose::FreeTier, 0, 1).is_ok());
    }

    #[test]
    fn purpose_unit_matrix() {
        let q = Unit::Quota(NodeId::new("n").unwrap());
        // (用途, 单位, 是否允许)
        let cases = [
            (Purpose::FreeTier, Unit::Thb, true),
            (Purpose::FreeTier, Unit::Usd, true),
            (Purpose::FreeTier, q.clone(), true),
            (Purpose::CreditLine, Unit::Thb, true),
            (Purpose::CreditLine, q.clone(), true),
            (Purpose::Prepaid, Unit::Thb, true),
            (Purpose::Prepaid, Unit::Usd, true),
            (Purpose::Prepaid, q.clone(), false), // 配额不是买来的
            (Purpose::OrgQuota, q.clone(), true),
            (Purpose::OrgQuota, Unit::Thb, false),
            (Purpose::OrgQuota, Unit::Usd, false),
        ];
        for (purpose, unit, allowed) in cases {
            let result = grant(Money::new(unit.clone(), 1), purpose, 0, 1);
            assert_eq!(result.is_ok(), allowed, "{purpose:?} {unit}");
            if !allowed {
                assert_eq!(
                    result.unwrap_err(),
                    GrantError::PurposeUnitMismatch { purpose, unit }
                );
            }
        }
    }

    #[test]
    fn purpose_wire_strings_are_pinned() {
        for (purpose, text) in [
            (Purpose::FreeTier, "free_tier"),
            (Purpose::Prepaid, "prepaid"),
            (Purpose::CreditLine, "credit_line"),
            (Purpose::OrgQuota, "org_quota"),
        ] {
            assert_eq!(purpose.as_str(), text);
            assert_eq!(
                serde_json::to_string(&purpose).unwrap(),
                format!("\"{text}\"")
            );
            assert_eq!(
                serde_json::from_str::<Purpose>(&format!("\"{text}\"")).unwrap(),
                purpose
            );
        }
        assert!(serde_json::from_str::<Purpose>("\"gift\"").is_err());
    }

    #[test]
    fn grant_is_never_transferable_on_the_wire() {
        let g = grant(thb(5_000_000), Purpose::FreeTier, 0, 10).unwrap();
        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(
            json,
            r#"{"amount":{"unit":"THB","micros":5000000},"purpose":"free_tier","valid_from":0,"valid_until":10,"transferable":false}"#
        );
        assert_eq!(serde_json::from_str::<Grant>(&json).unwrap(), g);
        // 想把 transferable 改成 true：拒绝
        let tampered = json.replace("\"transferable\":false", "\"transferable\":true");
        let err = serde_json::from_str::<Grant>(&tampered)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-transferable"), "{err}");
        // 缺字段、未知字段：拒绝
        let missing = json.replace(",\"transferable\":false", "");
        assert!(serde_json::from_str::<Grant>(&missing).is_err());
        let extra = json.replacen('{', "{\"cashable\":true,", 1);
        assert!(serde_json::from_str::<Grant>(&extra).is_err());
    }

    #[test]
    fn deserialization_runs_the_constructor_validation() {
        let bad_window = r#"{"amount":{"unit":"THB","micros":1},"purpose":"free_tier","valid_from":5,"valid_until":5,"transferable":false}"#;
        assert!(serde_json::from_str::<Grant>(bad_window).is_err());
        let negative = r#"{"amount":{"unit":"THB","micros":-1},"purpose":"free_tier","valid_from":0,"valid_until":5,"transferable":false}"#;
        assert!(serde_json::from_str::<Grant>(negative).is_err());
        let prepaid_quota = r#"{"amount":{"unit":"QUOTA:n","micros":1},"purpose":"prepaid","valid_from":0,"valid_until":5,"transferable":false}"#;
        assert!(serde_json::from_str::<Grant>(prepaid_quota).is_err());
    }

    #[test]
    fn window_covers_is_inclusive_on_both_ends() {
        let outer = grant(thb(1), Purpose::FreeTier, 100, 200).unwrap();
        for (from, until, covered) in [
            (100, 200, true),
            (100, 150, true),
            (150, 200, true),
            (99, 200, false),
            (100, 201, false),
            (50, 60, false),
        ] {
            let inner = grant(thb(1), Purpose::FreeTier, from, until).unwrap();
            assert_eq!(outer.window_covers(&inner), covered, "[{from},{until})");
        }
    }
}
