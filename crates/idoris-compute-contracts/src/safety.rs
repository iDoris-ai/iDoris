//! 儿童安全（设计稿 §7，切片 S6）的稳定原因码、类别和阶段。
//!
//! 这是 S0 [`ReasonCode`](crate::reason::ReasonCode) 之外**新增**的契约（加法）：S0 的 16 个字符串一个没动，
//! 本模块只加新的类型和新的字符串。`reason.rs` 的文档里早就写明：儿童安全（内容拦截、安全服务不可用）的码是 S6 的契约。
//!
//! # 线上字符串不可变
//!
//! 下面每个 `as_str` 返回的字符串一经发布就**永远不能改**：eduai 后端按字符串匹配，给孩子看的文案
//! （比如自伤类要换成关怀的话术和求助渠道）由它根据码和类别来写。新增只能加变体、加新字符串，
//! 不能改旧的、不能复用旧的。每个字符串都被本文件的测试和 `tests/safety_wire_snapshots.rs` 的固定快照钉住，
//! 并且保证和 S0 的原因码字符串互不相同（同一个 `X-iDoris-Reason` 响应头里会出现两边的码）。
//!
//! # 三个原因码
//!
//! | 码 | 含义 | 孩子的内容有没有被送到上游 | 用户付费 |
//! |---|---|---|---|
//! | `safety_input_blocked` | 输入检查拒绝（请求里有不该送给模型的内容） | 没有（上游调用之前就被拒） | 0 |
//! | `safety_output_blocked` | 完整输出检查拒绝（模型的回答不该给孩子看） | 已经送过（上游已经执行） | 0，上游成本由平台承担 |
//! | `safety_unavailable` | 安全检查没能给出结论：超时、出错、返回无法解析、分类器不可用、没有配置 | 输入阶段没有；输出阶段已经送过 | 0 |
//!
//! 检查结果只有放行和拒绝两种；**没有「不确定但放行」**：任何没能得出「放行」的情形一律是拒绝（失败即关闭）。
//!
//! # 类别和原因细节
//!
//! [`SafetyCategory`] 只出现在给 eduai 的响应体里，让它选对给孩子的话术；它**不是正文**，但它是从孩子的话里推出来的
//! 敏感推断（比如「这条消息和自伤有关」），所以路由层**不把它写进任何账本、日志或审计**，只给当次响应。
//! [`SafetyUnavailableCause`] 只描述「检查为什么没能给出结论」，同样不含任何内容，只用于排障。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 检查发生在请求的哪个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyStage {
    /// 输入检查：上游调用之前，检查用户消息、系统消息、工具返回的内容。
    Input,
    /// 完整输出检查：上游返回完整回答之后、发给客户端之前，检查回答文本和工具调用参数。
    Output,
}

impl SafetyStage {
    pub const ALL: [SafetyStage; 2] = [SafetyStage::Input, SafetyStage::Output];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            SafetyStage::Input => "input",
            SafetyStage::Output => "output",
        }
    }
}

impl fmt::Display for SafetyStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 儿童安全的稳定原因码（S0 `ReasonCode` 之外新增的）。`serde` 形式与 [`SafetyReasonCode::as_str`] 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyReasonCode {
    /// 输入检查拒绝。上游没有收到任何字节，用户付 0。
    SafetyInputBlocked,
    /// 完整输出检查拒绝。回答没有发给客户端（一个字节也没有），用户付 0，上游成本由平台承担。
    SafetyOutputBlocked,
    /// 安全检查没能给出结论（超时、出错、返回无法解析、分类器不可用、没有配置）：一律拒绝放行。
    SafetyUnavailable,
}

impl SafetyReasonCode {
    /// 全部原因码，顺序固定（= 变体声明顺序）。
    pub const ALL: [SafetyReasonCode; 3] = [
        SafetyReasonCode::SafetyInputBlocked,
        SafetyReasonCode::SafetyOutputBlocked,
        SafetyReasonCode::SafetyUnavailable,
    ];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            SafetyReasonCode::SafetyInputBlocked => "safety_input_blocked",
            SafetyReasonCode::SafetyOutputBlocked => "safety_output_blocked",
            SafetyReasonCode::SafetyUnavailable => "safety_unavailable",
        }
    }

    /// 被内容拦截的码对应的阶段；`safety_unavailable` 两个阶段都可能出现，返回 `None`。
    pub fn stage(self) -> Option<SafetyStage> {
        match self {
            SafetyReasonCode::SafetyInputBlocked => Some(SafetyStage::Input),
            SafetyReasonCode::SafetyOutputBlocked => Some(SafetyStage::Output),
            SafetyReasonCode::SafetyUnavailable => None,
        }
    }

    /// 内容拦截的码（不含 `safety_unavailable`）。
    pub fn blocked_at(stage: SafetyStage) -> SafetyReasonCode {
        match stage {
            SafetyStage::Input => SafetyReasonCode::SafetyInputBlocked,
            SafetyStage::Output => SafetyReasonCode::SafetyOutputBlocked,
        }
    }

    /// 请求是否仍被服务：三个码都是拒绝，没有任何一个会把回答交给孩子。
    pub fn is_served(self) -> bool {
        false
    }
}

impl fmt::Display for SafetyReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未知的安全码字符串。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown safety wire string {0:?}")]
pub struct UnknownSafetyString(pub String);

impl FromStr for SafetyReasonCode {
    type Err = UnknownSafetyString;
    fn from_str(s: &str) -> Result<Self, UnknownSafetyString> {
        SafetyReasonCode::ALL
            .into_iter()
            .find(|code| code.as_str() == s)
            .ok_or_else(|| UnknownSafetyString(s.to_string()))
    }
}

/// 被拒绝的内容属于哪一类（只给 eduai 选话术用）。
///
/// 前五类对应评测集 `eval/thai-edu/cases/child-safety.jsonl` 的 `safety_topic`：
/// `self_harm`、`adult_content`、`dangerous_act`、`pii_solicit`、`jailbreak`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyCategory {
    /// 自伤、自杀（索要方法、表达轻生念头、鼓励他人自伤）。
    SelfHarm,
    /// 成人内容（性描写、色情材料）。
    AdultContent,
    /// 危险行为（爆炸物、有毒气体、纵火等的方法）。
    DangerousAct,
    /// 诱导泄露个人信息（骗取别人的住址、电话、密码）。
    PiiSolicit,
    /// 越狱话术（让模型忘掉规则、关掉过滤、进入无限制角色）。
    Jailbreak,
    /// 输出里复述了孩子写在提示里的个人信息（姓名、学校、电话、住址……）。
    PiiDisclosure,
    /// 辱骂、仇恨、粗口。
    AbusiveLanguage,
    /// 内容检查器无法检查的形态（图片、音频、文件等非文本内容）。
    UnsupportedContent,
    /// 判定为拒绝，但没有（或不是已知的）类别。
    Other,
}

impl SafetyCategory {
    pub const ALL: [SafetyCategory; 9] = [
        SafetyCategory::SelfHarm,
        SafetyCategory::AdultContent,
        SafetyCategory::DangerousAct,
        SafetyCategory::PiiSolicit,
        SafetyCategory::Jailbreak,
        SafetyCategory::PiiDisclosure,
        SafetyCategory::AbusiveLanguage,
        SafetyCategory::UnsupportedContent,
        SafetyCategory::Other,
    ];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            SafetyCategory::SelfHarm => "self_harm",
            SafetyCategory::AdultContent => "adult_content",
            SafetyCategory::DangerousAct => "dangerous_act",
            SafetyCategory::PiiSolicit => "pii_solicit",
            SafetyCategory::Jailbreak => "jailbreak",
            SafetyCategory::PiiDisclosure => "pii_disclosure",
            SafetyCategory::AbusiveLanguage => "abusive_language",
            SafetyCategory::UnsupportedContent => "unsupported_content",
            SafetyCategory::Other => "other",
        }
    }
}

impl fmt::Display for SafetyCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SafetyCategory {
    type Err = UnknownSafetyString;
    fn from_str(s: &str) -> Result<Self, UnknownSafetyString> {
        SafetyCategory::ALL
            .into_iter()
            .find(|category| category.as_str() == s)
            .ok_or_else(|| UnknownSafetyString(s.to_string()))
    }
}

/// 安全检查为什么没能给出结论（`safety_unavailable` 的细节，只用于排障，不含任何内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyUnavailableCause {
    /// 这个租户处理儿童数据，但服务端没有配置任何安全检查器。
    NotConfigured,
    /// 检查超过了时间预算。
    Timeout,
    /// 安全服务出错：连接失败、非 2xx、传输中断。
    ServiceError,
    /// 安全服务返回了无法解析或不在约定词表里的结果（垃圾、半截 JSON、未知取值、多余字段）。
    BadResponse,
    /// 安全服务的端点不再满足端点许可表的硬约束（核验过期、数据类别、留存、DPA、区域）。
    NoCompliantChecker,
    /// 要检查的文本超过了检查器能处理的上限。
    TooLarge,
    /// 要检查的内容无法从请求或响应里可靠地取出（结构不符合约定）。
    Malformed,
    /// 内部错误。
    Internal,
}

impl SafetyUnavailableCause {
    pub const ALL: [SafetyUnavailableCause; 8] = [
        SafetyUnavailableCause::NotConfigured,
        SafetyUnavailableCause::Timeout,
        SafetyUnavailableCause::ServiceError,
        SafetyUnavailableCause::BadResponse,
        SafetyUnavailableCause::NoCompliantChecker,
        SafetyUnavailableCause::TooLarge,
        SafetyUnavailableCause::Malformed,
        SafetyUnavailableCause::Internal,
    ];

    /// 线上字符串。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            SafetyUnavailableCause::NotConfigured => "not_configured",
            SafetyUnavailableCause::Timeout => "timeout",
            SafetyUnavailableCause::ServiceError => "service_error",
            SafetyUnavailableCause::BadResponse => "bad_response",
            SafetyUnavailableCause::NoCompliantChecker => "no_compliant_checker",
            SafetyUnavailableCause::TooLarge => "too_large",
            SafetyUnavailableCause::Malformed => "malformed",
            SafetyUnavailableCause::Internal => "internal",
        }
    }
}

impl fmt::Display for SafetyUnavailableCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SafetyUnavailableCause {
    type Err = UnknownSafetyString;
    fn from_str(s: &str) -> Result<Self, UnknownSafetyString> {
        SafetyUnavailableCause::ALL
            .into_iter()
            .find(|cause| cause.as_str() == s)
            .ok_or_else(|| UnknownSafetyString(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeSet;

    use super::*;
    use crate::reason::ReasonCode;

    /// 线上字符串的权威清单。改这里就是在改线上契约：不允许。
    const PINNED_CODES: [(SafetyReasonCode, &str); 3] = [
        (SafetyReasonCode::SafetyInputBlocked, "safety_input_blocked"),
        (
            SafetyReasonCode::SafetyOutputBlocked,
            "safety_output_blocked",
        ),
        (SafetyReasonCode::SafetyUnavailable, "safety_unavailable"),
    ];

    const PINNED_CATEGORIES: [(SafetyCategory, &str); 9] = [
        (SafetyCategory::SelfHarm, "self_harm"),
        (SafetyCategory::AdultContent, "adult_content"),
        (SafetyCategory::DangerousAct, "dangerous_act"),
        (SafetyCategory::PiiSolicit, "pii_solicit"),
        (SafetyCategory::Jailbreak, "jailbreak"),
        (SafetyCategory::PiiDisclosure, "pii_disclosure"),
        (SafetyCategory::AbusiveLanguage, "abusive_language"),
        (SafetyCategory::UnsupportedContent, "unsupported_content"),
        (SafetyCategory::Other, "other"),
    ];

    const PINNED_CAUSES: [(SafetyUnavailableCause, &str); 8] = [
        (SafetyUnavailableCause::NotConfigured, "not_configured"),
        (SafetyUnavailableCause::Timeout, "timeout"),
        (SafetyUnavailableCause::ServiceError, "service_error"),
        (SafetyUnavailableCause::BadResponse, "bad_response"),
        (
            SafetyUnavailableCause::NoCompliantChecker,
            "no_compliant_checker",
        ),
        (SafetyUnavailableCause::TooLarge, "too_large"),
        (SafetyUnavailableCause::Malformed, "malformed"),
        (SafetyUnavailableCause::Internal, "internal"),
    ];

    #[test]
    fn every_reason_code_string_is_pinned_in_all_three_representations() {
        for (code, text) in PINNED_CODES {
            assert_eq!(code.as_str(), text);
            assert_eq!(code.to_string(), text);
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{text}\""));
            assert_eq!(
                serde_json::from_str::<SafetyReasonCode>(&format!("\"{text}\"")).unwrap(),
                code
            );
            assert_eq!(text.parse::<SafetyReasonCode>().unwrap(), code);
        }
        let in_table: Vec<SafetyReasonCode> = PINNED_CODES.iter().map(|(c, _)| *c).collect();
        assert_eq!(in_table, SafetyReasonCode::ALL.to_vec());
    }

    #[test]
    fn every_category_string_is_pinned_in_all_three_representations() {
        for (category, text) in PINNED_CATEGORIES {
            assert_eq!(category.as_str(), text);
            assert_eq!(category.to_string(), text);
            assert_eq!(
                serde_json::to_string(&category).unwrap(),
                format!("\"{text}\"")
            );
            assert_eq!(
                serde_json::from_str::<SafetyCategory>(&format!("\"{text}\"")).unwrap(),
                category
            );
            assert_eq!(text.parse::<SafetyCategory>().unwrap(), category);
        }
        let in_table: Vec<SafetyCategory> = PINNED_CATEGORIES.iter().map(|(c, _)| *c).collect();
        assert_eq!(in_table, SafetyCategory::ALL.to_vec());
    }

    #[test]
    fn every_unavailable_cause_string_is_pinned_in_all_three_representations() {
        for (cause, text) in PINNED_CAUSES {
            assert_eq!(cause.as_str(), text);
            assert_eq!(cause.to_string(), text);
            assert_eq!(
                serde_json::to_string(&cause).unwrap(),
                format!("\"{text}\"")
            );
            assert_eq!(
                serde_json::from_str::<SafetyUnavailableCause>(&format!("\"{text}\"")).unwrap(),
                cause
            );
            assert_eq!(text.parse::<SafetyUnavailableCause>().unwrap(), cause);
        }
        let in_table: Vec<SafetyUnavailableCause> = PINNED_CAUSES.iter().map(|(c, _)| *c).collect();
        assert_eq!(in_table, SafetyUnavailableCause::ALL.to_vec());
    }

    #[test]
    fn stages_are_pinned() {
        for (stage, text) in [
            (SafetyStage::Input, "input"),
            (SafetyStage::Output, "output"),
        ] {
            assert_eq!(stage.as_str(), text);
            assert_eq!(
                serde_json::to_string(&stage).unwrap(),
                format!("\"{text}\"")
            );
        }
        assert_eq!(SafetyStage::ALL.len(), 2);
    }

    #[test]
    fn the_safety_codes_never_collide_with_the_s0_reason_codes() {
        // 同一个 `X-iDoris-Reason` 响应头里会出现 S0 的码和这里的码：字符串必须互不相同。
        let s0: BTreeSet<&str> = ReasonCode::ALL.iter().map(|c| c.as_str()).collect();
        for code in SafetyReasonCode::ALL {
            assert!(!s0.contains(code.as_str()), "{code} collides with S0");
            assert!(
                code.as_str().parse::<ReasonCode>().is_err(),
                "{code} must not parse as an S0 reason code"
            );
        }
        // S0 的字符串一个没动：抽查设计稿逐字点名的和 S0 新增的几个
        for text in [
            "budget_degraded",
            "capacity_degraded",
            "upstream_failover",
            "grace_used",
            "budget_exhausted",
            "no_compliant_endpoint",
        ] {
            assert!(s0.contains(text));
        }
    }

    #[test]
    fn strings_are_unique_lowercase_snake_case_and_unknown_ones_are_rejected() {
        let mut all: Vec<&str> = Vec::new();
        all.extend(PINNED_CODES.iter().map(|(_, s)| *s));
        all.extend(PINNED_CATEGORIES.iter().map(|(_, s)| *s));
        all.extend(PINNED_CAUSES.iter().map(|(_, s)| *s));
        for text in &all {
            assert!(
                !text.is_empty() && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{text}"
            );
        }
        let codes: BTreeSet<&str> = PINNED_CODES.iter().map(|(_, s)| *s).collect();
        assert_eq!(codes.len(), PINNED_CODES.len());
        let categories: BTreeSet<&str> = PINNED_CATEGORIES.iter().map(|(_, s)| *s).collect();
        assert_eq!(categories.len(), PINNED_CATEGORIES.len());
        for bad in [
            "",
            "Safety_Input_Blocked",
            "safety-input-blocked",
            "blocked",
            " safety_unavailable",
        ] {
            assert!(bad.parse::<SafetyReasonCode>().is_err(), "{bad:?}");
            assert!(
                serde_json::from_str::<SafetyReasonCode>(&format!("{bad:?}")).is_err(),
                "{bad:?}"
            );
        }
        assert!("nope".parse::<SafetyCategory>().is_err());
        assert!("nope".parse::<SafetyUnavailableCause>().is_err());
        assert_eq!(
            "nope".parse::<SafetyReasonCode>().unwrap_err(),
            UnknownSafetyString("nope".to_string())
        );
    }

    #[test]
    fn stages_and_service_semantics() {
        assert_eq!(
            SafetyReasonCode::SafetyInputBlocked.stage(),
            Some(SafetyStage::Input)
        );
        assert_eq!(
            SafetyReasonCode::SafetyOutputBlocked.stage(),
            Some(SafetyStage::Output)
        );
        assert_eq!(SafetyReasonCode::SafetyUnavailable.stage(), None);
        assert_eq!(
            SafetyReasonCode::blocked_at(SafetyStage::Input),
            SafetyReasonCode::SafetyInputBlocked
        );
        assert_eq!(
            SafetyReasonCode::blocked_at(SafetyStage::Output),
            SafetyReasonCode::SafetyOutputBlocked
        );
        // 三个码都是拒绝：没有任何一个会把回答交给孩子
        for code in SafetyReasonCode::ALL {
            assert!(!code.is_served());
        }
    }
}
