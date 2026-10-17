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
//! （`idoris_contracts::inspect` 模块文档）。逃逸归一化只删或折叠字符、
//! 不增字符。
//!
//! 一共扫**三个视图**，各自把命中映射回原文坐标后取并集：
//!
//! 1. **原文**（`normalize::identity`）：不折叠、不删除。这是 base 的覆盖，
//!    保证「原文能抓到的，归一化后绝不放行」。折叠本身也会粘合（全角字母
//!    数字、横线变体贴在实体边上），只有这一路不受影响。
//! 2. **折叠并保留**（`normalize::fold_keeping_ignorables`）：可忽略字符留在
//!    原地当边界，所以「可忽略字符 + 全角实体」这种形状只有它能命中。
//! 3. **折叠并删除**（`normalize::normalize`）：实体内部的隐形插入被去掉，
//!    所以「零宽字符夹在实体中间」只有它能命中。
//!
//! 三路缺一不可：`tests::union_is_never_looser_than_any_single_view` 会在
//! 任意一路被删掉时变红。

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
    // 原文视图保证 base 覆盖不丢；两个折叠视图负责补隐形插入与全角/横线变体。
    let mut hits = scan_view(&normalize::identity(&units));
    // The folded views may only add coverage, never invent an entity the original
    // spelling does not have, so their HKID hits are filtered against the original.
    hits.extend(drop_folded_malformed_hkid(
        scan_view(&normalize::fold_keeping_ignorables(&units)),
        &units,
    ));
    hits.extend(drop_folded_malformed_hkid(
        scan_view(&normalize::normalize(&units)),
        &units,
    ));
    // 同一原文区间只留一条。敏感度取更严的那条，避免并集把 S3 降成 S2。
    hits.sort_by_key(|hit| {
        (
            hit.start,
            hit.length,
            hit.kind as u8,
            std::cmp::Reverse(hit.sensitivity as u8),
        )
    });
    hits.dedup_by_key(|hit| (hit.kind, hit.start, hit.length));
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

fn scan_view(normalized: &normalize::Normalized) -> Vec<Hit> {
    let mut hits = l0::scan(&normalized.units);
    for hit in &mut hits {
        (hit.start, hit.length) = normalized.original_span(hit.start, hit.length);
    }
    hits
}

/// Folded views make some malformed HKID spellings look well formed: fullwidth
/// parentheses and U+3000 fold to `()` and a space, which is exactly the shape the
/// HKID rule accepts. Those spellings are not valid HKIDs, so a folded view must
/// not report them.
///
/// The drop is judged on the **original** text the hit covers, and only for
/// `HkIdentityCard` hits from a folded view: the raw view is exactly `base`, and
/// U+3000 folding is load-bearing for bank cards (`6222\u{3000}0212...`), so
/// neither the raw view nor the folding table may change.
fn drop_folded_malformed_hkid(hits: Vec<Hit>, original: &[u16]) -> Vec<Hit> {
    hits.into_iter()
        .filter(|hit| {
            hit.kind != FindingKind::HkIdentityCard
                || !malformed_hkid_spelling(&original[hit.start..hit.start + hit.length])
        })
        .collect()
}

/// True when the original spelling is not a valid HKID: it contains an ideographic
/// space, or its parenthesis types do not pair (a fullwidth opener must be closed
/// by a fullwidth closer).
fn malformed_hkid_spelling(span: &[u16]) -> bool {
    let opens_full = span.iter().filter(|&&u| u == 0xFF08).count();
    let opens_ascii = span.iter().filter(|&&u| u == u16::from(b'(')).count();
    let closes_full = span.iter().filter(|&&u| u == 0xFF09).count();
    let closes_ascii = span.iter().filter(|&&u| u == u16::from(b')')).count();
    span.contains(&0x3000) || opens_full != closes_full || opens_ascii != closes_ascii
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
