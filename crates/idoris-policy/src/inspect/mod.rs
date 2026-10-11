//! 闸一 `INSPECTED`：L0 机械规则 + L2 确定性动作判定（M5-02）。**没有 IO、
//! 没有模型、没有回溯正则**，同一输入永远得到同一个 [`InspectResponse`]。
//!
//! 设计依据是 docs/15 §1.1b–§1.3：
//!
//! - 默认动作是 BLOCK。`ALLOW` 只在「L0 零命中 且 调用方显式声明 `any` 且
//!   租户策略允许出网」三者同时成立时给出；「没扫到」本身不构成放行理由。
//! - X-1：超限输入**整段不扫、直接 BLOCK**，绝不扫一个前缀就放行。
//! - X-2：校验和失败是降级（S3 → S2），不是丢弃命中，也不是硬拒整条请求。
//! - L0 只会让结果更严：本模块不读调用方的 header，只接收已解析的
//!   [`InspectCtx`]，由接线层负责把它传进来（接线不在本切片内）。
//!
//! 扫描跑在 UTF-16 code unit 上，命中位置就是契约要求的 span 单位
//! （`idoris_contracts::inspect` 模块文档）；扫描前的逃逸归一化只删字符、
//! 不增字符，命中再按 `normalize` 的映射表回到原文坐标。

mod l0;
mod normalize;
mod secrets;
#[cfg(test)]
mod tests;
mod tokens;

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::inspect::{
    FindingKind, InspectAction, InspectFinding, InspectResponse, InspectSpan, Sensitivity,
};

/// 默认输入上限（UTF-16 code unit）。超过即按 X-1 整段拒绝。
pub const DEFAULT_MAX_INPUT_UTF16: usize = 256 * 1024;

/// L2 判定需要的、已经由可信来源解析好的上下文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectCtx {
    /// 调用方声明的隐私下限（`X-iDoris-Privacy`）。
    pub declared: PrivacyClass,
    /// 租户策略是否允许这条请求出网。
    pub remote_allowed: bool,
    /// 调用方是否显式同意可逆脱敏（docs/15 §1.3 REDACT 的前提）。
    pub redact_consent: bool,
    /// 输入上限（UTF-16 code unit）。
    pub max_input_utf16: usize,
}

impl InspectCtx {
    /// 最保守的上下文：`local_only`、不允许出网、未同意脱敏。
    pub const fn fail_closed() -> Self {
        Self {
            declared: PrivacyClass::LocalOnly,
            remote_allowed: false,
            redact_consent: false,
            max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
        }
    }
}

/// L0 的一条命中：类别、UTF-16 位置、这条命中自身的敏感度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hit {
    pub kind: FindingKind,
    pub start: usize,
    pub length: usize,
    pub sensitivity: Sensitivity,
}

/// 对一段输入做完整的闸一判定：L0 扫描 → 敏感度取最大值 → L2 动作。
pub fn inspect(input: &str, ctx: &InspectCtx) -> InspectResponse {
    if exceeds_limit(input, ctx.max_input_utf16) {
        return InspectResponse {
            sensitivity: Sensitivity::S3,
            findings: vec![InspectFinding {
                kind: FindingKind::InputLimitExceeded,
                span: None,
            }],
            action: InspectAction::Block,
        };
    }
    let units: Vec<u16> = input.encode_utf16().collect();
    let normalized = normalize::normalize(&units);
    let mut hits = l0::scan(&normalized.units);
    for hit in &mut hits {
        (hit.start, hit.length) = normalized.original_span(hit.start, hit.length);
    }
    let sensitivity = hits
        .iter()
        .map(|hit| hit.sensitivity)
        .max()
        .unwrap_or(Sensitivity::S0);
    let action = decide_action(&hits, sensitivity, ctx);
    let findings = hits
        .iter()
        .map(|hit| InspectFinding {
            kind: hit.kind,
            span: Some(InspectSpan {
                start: hit.start as u64,
                length: hit.length as u64,
            }),
        })
        .collect();
    InspectResponse {
        sensitivity,
        findings,
        action,
    }
}

/// UTF-16 长度不会超过 UTF-8 字节数，所以字节数没超就一定没超，省掉一次计数。
fn exceeds_limit(input: &str, max_input_utf16: usize) -> bool {
    input.len() > max_input_utf16 && input.encode_utf16().count() > max_input_utf16
}

/// L2：docs/15 §1.3 的三种动作。顺序即语义——任何一条不满足都落在 BLOCK。
fn decide_action(hits: &[Hit], sensitivity: Sensitivity, ctx: &InspectCtx) -> InspectAction {
    if ctx.declared == PrivacyClass::LocalOnly || !ctx.remote_allowed {
        return InspectAction::Block;
    }
    if hits.is_empty() {
        return InspectAction::Allow;
    }
    if sensitivity == Sensitivity::S3 || !ctx.redact_consent {
        return InspectAction::Block;
    }
    InspectAction::Redact
}
