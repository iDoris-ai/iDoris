//! 稳定原因码与体验分级（设计稿 §5，评审 L2）。
//!
//! # 线上字符串不可变
//!
//! [`ReasonCode::as_str`] 返回的字符串一经发布就**永远不能改**：调用方（eduai 后端、浏览器、
//! Agent24）按字符串匹配，给孩子看的文案也由 eduai 根据它来写。`tests/` 里用固定快照钉住了
//! 每一个字符串。新增原因码只能加新变体、加新字符串，不能改旧的、不能复用旧的。
//!
//! # 体验分级
//!
//! 设计稿 §5 的四级：
//!
//! | 级别 | [`ExperienceTier`] | 原因码 |
//! |---|---|---|
//! | 1 正常 | `Normal` | （无） |
//! | 2 降级，同场景换便宜模型 | `Degraded` | `budget_degraded` / `capacity_degraded` / `upstream_failover` |
//! | 3 宽限一轮并通知担保人 | `Grace` | `grace_used` |
//! | 4 返回 402 | `Exhausted` | `budget_exhausted` |
//!
//! 除了这五个设计稿点名的码，下面这些是通读设计后认为**必须有**的，每个都对应设计稿里一个
//! 会让请求被拒的明确位置，见各变体的文档：`rate_limited`、`in_flight_limited`、
//! `subsidy_pool_exhausted`、`no_compliant_endpoint`、`credential_frozen`、
//! `credential_revoked`、`credential_expired`、`principal_revoked`、`membership_revoked`、
//! `account_inactive`、`payment_context_invalid`。它们不在四级阶梯上（[`ReasonCode::tier`] 返回
//! `None`），因为它们不是「钱不够」，而是「现在就是不能服务这个请求」。
//!
//! 儿童安全（S6）相关的码（内容拦截、安全服务不可用）故意**没有**放进 S0：那是 S6 的契约，
//! 加法即可，不会改动这里任何一个字符串。
//!
//! # 与既有字符串的关系
//!
//! 既有的 402 响应体用 `budget_exceeded`（`idoris-tenancy::budget::BUDGET_EXCEEDED_REASON_CODE`），
//! 本模块的第四级是 `budget_exhausted`。两者**不是同一个字符串**，也都不会被改动；设计稿没有说
//! 云入口的 402 沿用哪个，见交付说明里的含糊点。响应头名（浏览器可读）也不是 S0 的内容。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 稳定原因码。`serde` 形式与 [`ReasonCode::as_str`] 一致（snake_case）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    // ---- 第 2 级：降级（请求被服务，只是换了更便宜/另一条路）----
    /// 额度紧，自动换了同场景更便宜的模型，或压低了 `max_tokens`。
    BudgetDegraded,
    /// 容量不够（本机放不下等），换了别的候选或走了云端档。
    CapacityDegraded,
    /// 首字节之前上游失败，切换到了下一个候选（设计稿 §6）。
    UpstreamFailover,
    // ---- 第 3 级：宽限 ----
    /// 额度用尽后使用了本周期唯一一次宽限（每成员每周期最多 1 次，不超过一次预留）。
    GraceUsed,
    // ---- 第 4 级：402 ----
    /// 额度、授信都用尽，且没有宽限可用。
    BudgetExhausted,
    // ---- 其它拒绝（不在四级阶梯上）----
    /// 速率闸门（分钟/小时花费上限）不通过，准入第 3 项。
    RateLimited,
    /// 在途总额上限不通过，准入第 4 项。
    InFlightLimited,
    /// 平台补贴池总上限不通过，准入第 5 项（评审 H10）。
    SubsidyPoolExhausted,
    /// 路由硬约束过滤后没有合规候选（设计稿 §8：没有合规候选，就拒绝）。
    NoCompliantEndpoint,
    /// 凭证被冻结（overage 超出授信的部分记平台损失账户时，冻结这张凭证，设计稿 §5）。
    CredentialFrozen,
    /// 凭证已被撤销（设计稿 §13：撤销后调用被拒）。
    CredentialRevoked,
    /// 凭证已过期（短期凭证，评审 B1）。
    CredentialExpired,
    /// 开通方撤销了该主体的处理资格（同意撤回，设计稿 §7：撤销后相关凭证和成员立即失效）。
    PrincipalRevoked,
    /// 该成员关系已被撤销（被移出组织等）。
    MembershipRevoked,
    /// 付款账户本身或它的某个祖先被冻结/关闭。
    AccountInactive,
    /// 付款上下文无法唯一确定或不合法：成员 id 缺失、不属于该凭证的范围、对不该带成员 id 的凭证
    /// 带了成员 id、或同一主体在范围内对应多个账户（设计稿 §3：每次请求只落到一个付款上下文）。
    PaymentContextInvalid,
}

/// 原因码的性质。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonClass {
    /// 请求被服务，但体验降级了（第 2 级）。
    Degradation,
    /// 请求被服务，用了宽限（第 3 级）。
    Grace,
    /// 请求不被服务。
    Denial,
}

/// 体验分级（设计稿 §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceTier {
    Normal,
    Degraded,
    Grace,
    Exhausted,
}

impl ReasonCode {
    /// 全部原因码，顺序固定（= 变体声明顺序）。测试用它保证没有漏钉任何一个字符串。
    pub const ALL: [ReasonCode; 16] = [
        ReasonCode::BudgetDegraded,
        ReasonCode::CapacityDegraded,
        ReasonCode::UpstreamFailover,
        ReasonCode::GraceUsed,
        ReasonCode::BudgetExhausted,
        ReasonCode::RateLimited,
        ReasonCode::InFlightLimited,
        ReasonCode::SubsidyPoolExhausted,
        ReasonCode::NoCompliantEndpoint,
        ReasonCode::CredentialFrozen,
        ReasonCode::CredentialRevoked,
        ReasonCode::CredentialExpired,
        ReasonCode::PrincipalRevoked,
        ReasonCode::MembershipRevoked,
        ReasonCode::AccountInactive,
        ReasonCode::PaymentContextInvalid,
    ];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            ReasonCode::BudgetDegraded => "budget_degraded",
            ReasonCode::CapacityDegraded => "capacity_degraded",
            ReasonCode::UpstreamFailover => "upstream_failover",
            ReasonCode::GraceUsed => "grace_used",
            ReasonCode::BudgetExhausted => "budget_exhausted",
            ReasonCode::RateLimited => "rate_limited",
            ReasonCode::InFlightLimited => "in_flight_limited",
            ReasonCode::SubsidyPoolExhausted => "subsidy_pool_exhausted",
            ReasonCode::NoCompliantEndpoint => "no_compliant_endpoint",
            ReasonCode::CredentialFrozen => "credential_frozen",
            ReasonCode::CredentialRevoked => "credential_revoked",
            ReasonCode::CredentialExpired => "credential_expired",
            ReasonCode::PrincipalRevoked => "principal_revoked",
            ReasonCode::MembershipRevoked => "membership_revoked",
            ReasonCode::AccountInactive => "account_inactive",
            ReasonCode::PaymentContextInvalid => "payment_context_invalid",
        }
    }

    pub fn class(self) -> ReasonClass {
        match self {
            ReasonCode::BudgetDegraded
            | ReasonCode::CapacityDegraded
            | ReasonCode::UpstreamFailover => ReasonClass::Degradation,
            ReasonCode::GraceUsed => ReasonClass::Grace,
            _ => ReasonClass::Denial,
        }
    }

    /// 在四级阶梯上的位置；不在阶梯上的拒绝返回 `None`。
    pub fn tier(self) -> Option<ExperienceTier> {
        match self {
            ReasonCode::BudgetDegraded
            | ReasonCode::CapacityDegraded
            | ReasonCode::UpstreamFailover => Some(ExperienceTier::Degraded),
            ReasonCode::GraceUsed => Some(ExperienceTier::Grace),
            ReasonCode::BudgetExhausted => Some(ExperienceTier::Exhausted),
            _ => None,
        }
    }

    /// 请求是否仍被服务（降级和宽限都算）。
    pub fn is_served(self) -> bool {
        !matches!(self.class(), ReasonClass::Denial)
    }
}

impl ExperienceTier {
    /// 该级别下允许出现的原因码。`Normal` 没有原因码。
    pub fn reason_codes(self) -> &'static [ReasonCode] {
        match self {
            ExperienceTier::Normal => &[],
            ExperienceTier::Degraded => &[
                ReasonCode::BudgetDegraded,
                ReasonCode::CapacityDegraded,
                ReasonCode::UpstreamFailover,
            ],
            ExperienceTier::Grace => &[ReasonCode::GraceUsed],
            ExperienceTier::Exhausted => &[ReasonCode::BudgetExhausted],
        }
    }
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未知的原因码字符串。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown reason code {0:?}")]
pub struct UnknownReasonCode(pub String);

impl FromStr for ReasonCode {
    type Err = UnknownReasonCode;
    fn from_str(s: &str) -> Result<Self, UnknownReasonCode> {
        ReasonCode::ALL
            .into_iter()
            .find(|code| code.as_str() == s)
            .ok_or_else(|| UnknownReasonCode(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::BTreeSet;

    /// 线上字符串的权威清单。改这里就是在改线上契约：不允许。
    const PINNED: [(ReasonCode, &str); 16] = [
        (ReasonCode::BudgetDegraded, "budget_degraded"),
        (ReasonCode::CapacityDegraded, "capacity_degraded"),
        (ReasonCode::UpstreamFailover, "upstream_failover"),
        (ReasonCode::GraceUsed, "grace_used"),
        (ReasonCode::BudgetExhausted, "budget_exhausted"),
        (ReasonCode::RateLimited, "rate_limited"),
        (ReasonCode::InFlightLimited, "in_flight_limited"),
        (ReasonCode::SubsidyPoolExhausted, "subsidy_pool_exhausted"),
        (ReasonCode::NoCompliantEndpoint, "no_compliant_endpoint"),
        (ReasonCode::CredentialFrozen, "credential_frozen"),
        (ReasonCode::CredentialRevoked, "credential_revoked"),
        (ReasonCode::CredentialExpired, "credential_expired"),
        (ReasonCode::PrincipalRevoked, "principal_revoked"),
        (ReasonCode::MembershipRevoked, "membership_revoked"),
        (ReasonCode::AccountInactive, "account_inactive"),
        (ReasonCode::PaymentContextInvalid, "payment_context_invalid"),
    ];

    #[test]
    fn every_wire_string_is_pinned_in_all_three_representations() {
        for (code, text) in PINNED {
            assert_eq!(code.as_str(), text);
            assert_eq!(code.to_string(), text);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{text}\""));
            assert_eq!(
                serde_json::from_str::<ReasonCode>(&format!("\"{text}\"")).unwrap(),
                code
            );
            assert_eq!(text.parse::<ReasonCode>().unwrap(), code);
        }
    }

    #[test]
    fn the_five_strings_named_by_the_design_are_exactly_these() {
        // 设计稿 §5 逐字点名的五个。
        let named = [
            (ReasonCode::BudgetDegraded, "budget_degraded"),
            (ReasonCode::CapacityDegraded, "capacity_degraded"),
            (ReasonCode::UpstreamFailover, "upstream_failover"),
            (ReasonCode::GraceUsed, "grace_used"),
            (ReasonCode::BudgetExhausted, "budget_exhausted"),
        ];
        for (code, text) in named {
            assert_eq!(code.as_str(), text);
        }
    }

    #[test]
    fn pinned_table_covers_all_and_strings_are_unique_snake_case() {
        let in_table: Vec<ReasonCode> = PINNED.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            in_table,
            ReasonCode::ALL.to_vec(),
            "ALL 与 PINNED 顺序/内容必须一致"
        );
        let strings: BTreeSet<&str> = PINNED.iter().map(|(_, s)| *s).collect();
        assert_eq!(strings.len(), PINNED.len(), "字符串必须互不相同");
        for (_, s) in PINNED {
            assert!(
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{s}"
            );
        }
    }

    #[test]
    fn unknown_and_miscased_strings_are_rejected() {
        for bad in [
            "",
            "Budget_Exhausted",
            "budget_exceeded",
            "budget-exhausted",
            "grace",
            " grace_used",
        ] {
            assert!(bad.parse::<ReasonCode>().is_err(), "{bad:?}");
            assert!(
                serde_json::from_str::<ReasonCode>(&format!("{bad:?}")).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(
            "nope".parse::<ReasonCode>().unwrap_err(),
            UnknownReasonCode("nope".to_string())
        );
    }

    #[test]
    fn tiers_and_codes_are_mutually_consistent() {
        for code in ReasonCode::ALL {
            match code.tier() {
                Some(tier) => assert!(tier.reason_codes().contains(&code), "{code:?}"),
                None => {
                    for tier in [
                        ExperienceTier::Normal,
                        ExperienceTier::Degraded,
                        ExperienceTier::Grace,
                        ExperienceTier::Exhausted,
                    ] {
                        assert!(!tier.reason_codes().contains(&code), "{code:?} in {tier:?}");
                    }
                }
            }
        }
        for tier in [
            ExperienceTier::Normal,
            ExperienceTier::Degraded,
            ExperienceTier::Grace,
            ExperienceTier::Exhausted,
        ] {
            for code in tier.reason_codes() {
                assert_eq!(code.tier(), Some(tier));
            }
        }
        assert!(ExperienceTier::Normal.reason_codes().is_empty());
    }

    #[test]
    fn classes_match_the_design_ladder() {
        for code in [
            ReasonCode::BudgetDegraded,
            ReasonCode::CapacityDegraded,
            ReasonCode::UpstreamFailover,
        ] {
            assert_eq!(code.class(), ReasonClass::Degradation);
            assert_eq!(code.tier(), Some(ExperienceTier::Degraded));
            assert!(code.is_served());
        }
        assert_eq!(ReasonCode::GraceUsed.class(), ReasonClass::Grace);
        assert_eq!(ReasonCode::GraceUsed.tier(), Some(ExperienceTier::Grace));
        assert!(ReasonCode::GraceUsed.is_served());
        assert_eq!(
            ReasonCode::BudgetExhausted.tier(),
            Some(ExperienceTier::Exhausted)
        );
        assert!(!ReasonCode::BudgetExhausted.is_served());
        for code in [
            ReasonCode::RateLimited,
            ReasonCode::InFlightLimited,
            ReasonCode::SubsidyPoolExhausted,
            ReasonCode::NoCompliantEndpoint,
            ReasonCode::CredentialFrozen,
            ReasonCode::CredentialRevoked,
            ReasonCode::CredentialExpired,
            ReasonCode::PrincipalRevoked,
            ReasonCode::MembershipRevoked,
            ReasonCode::AccountInactive,
            ReasonCode::PaymentContextInvalid,
        ] {
            assert_eq!(code.class(), ReasonClass::Denial, "{code:?}");
            assert_eq!(code.tier(), None, "{code:?}");
            assert!(!code.is_served());
        }
    }

    #[test]
    fn tier_wire_strings_are_pinned() {
        for (tier, text) in [
            (ExperienceTier::Normal, "normal"),
            (ExperienceTier::Degraded, "degraded"),
            (ExperienceTier::Grace, "grace"),
            (ExperienceTier::Exhausted, "exhausted"),
        ] {
            assert_eq!(serde_json::to_string(&tier).unwrap(), format!("\"{text}\""));
        }
        assert!(ExperienceTier::Normal < ExperienceTier::Exhausted);
    }

    #[test]
    fn the_existing_402_reason_code_is_a_different_string() {
        // 既有 402 体用 `budget_exceeded`；本模块是 `budget_exhausted`。两者并存，不互相覆盖。
        assert_ne!(ReasonCode::BudgetExhausted.as_str(), "budget_exceeded");
        assert!("budget_exceeded".parse::<ReasonCode>().is_err());
    }
}
