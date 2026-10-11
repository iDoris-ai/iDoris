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

mod decode;
mod l0;
mod normalize;
pub mod redact;
#[cfg(test)]
mod redact_tests;
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

/// Compare a known credential after the same escape normalization used by L0.
pub(crate) fn contains_credential(input: &str, credential: &str) -> bool {
    let input = decode_json_unicode_escapes(&input.encode_utf16().collect::<Vec<_>>());
    let input = normalize::normalize(&input);
    let credential = normalize::normalize(&credential.encode_utf16().collect::<Vec<_>>());
    credential.units.is_empty()
        || input
            .units
            .windows(credential.units.len())
            .any(|part| part == credential.units)
}

/// A literal `\uXXXX` in JSON/JS source stands for that unit, so it can join a credential.
fn decode_json_unicode_escapes(units: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        if let Some(escaped) = json_unicode_escape(&units[index..]) {
            out.push(escaped);
            index += 6;
        } else {
            out.push(units[index]);
            index += 1;
        }
    }
    out
}

fn json_unicode_escape(units: &[u16]) -> Option<u16> {
    let window = units.get(..6)?;
    if window[0] != u16::from(b'\\') || window[1] != u16::from(b'u') {
        return None;
    }
    window[2..].iter().try_fold(0u16, |acc, &unit| {
        let digit = char::from_u32(u32::from(unit))?.to_digit(16)?;
        Some(acc << 4 | u16::try_from(digit).ok()?)
    })
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
    let hits = scan_all_views(&units, l0::scan);
    // This ASCII data-URL rule blocks opaque payloads the scanner cannot inspect.
    // Keep the real scanner findings and their original UTF-16 spans.
    let opaque = contains_base64_data_url(input);
    let sensitivity = if opaque {
        Sensitivity::S3
    } else {
        hits.iter()
            .map(|hit| hit.sensitivity)
            .max()
            .unwrap_or(Sensitivity::S0)
    };
    let action = if opaque {
        InspectAction::Block
    } else {
        decide_action(&hits, sensitivity, ctx)
    };
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

/// Share the exact normalization/HKID policy between plaintext and decoded
/// content. The decoder passes scan_plain to retain its one-layer bound.
fn scan_all_views(units: &[u16], scan: fn(&[u16]) -> Vec<Hit>) -> Vec<Hit> {
    let mut hits = scan_view(&normalize::identity(units), scan);
    for view in [
        normalize::fold_keeping_ignorables(units),
        normalize::normalize(units),
    ] {
        hits.extend(drop_folded_malformed_hkid(scan_view(&view, scan), units));
    }
    hits.sort_by_key(|hit| {
        (
            hit.start,
            hit.length,
            hit.kind as u8,
            std::cmp::Reverse(hit.sensitivity as u8),
        )
    });
    hits.dedup_by_key(|hit| (hit.kind, hit.start, hit.length));
    hits
}

fn scan_view(normalized: &normalize::Normalized, scan: fn(&[u16]) -> Vec<Hit>) -> Vec<Hit> {
    let mut hits = scan(&normalized.units);
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

/// `data:<mediatype>;base64,` anywhere, case-insensitive (RFC 2397).
fn contains_base64_data_url(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.match_indices("data:").any(|(at, _)| {
        let rest = &lower[at + 5..];
        // End the header at the first character outside the MIME token
        // alphabet plus `/`, `;` and `=` (RFC 2397 media type and
        // parameters). Never search for a comma or whitespace: that walk
        // can run to the end of every suffix. `:` is not a header
        // character, so a repeated `data:data:…` input stops each scan at
        // the very next occurrence — R4 of #454.
        let header_end = rest
            .find(|c: char| {
                // Count every character this scan examines, including the
                // terminator. A comma search that walks each suffix would
                // blow this count up; the repeated-`data:` test pins it.
                #[cfg(test)]
                DATA_URL_HEADER_CHAR_CHECKS.with(|checks| checks.set(checks.get() + 1));
                !is_data_url_header_char(c)
            })
            .unwrap_or(rest.len());
        // A header with no payload is a template or a sentence, not an opaque
        // body. Require one base64 alphabet byte after the comma.
        rest[..header_end].ends_with(";base64")
            && rest[header_end..].starts_with(',')
            && rest
                .as_bytes()
                .get(header_end + 1)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    })
}

#[cfg(test)]
thread_local! {
    static DATA_URL_HEADER_CHAR_CHECKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_data_url_header_char_checks() {
    DATA_URL_HEADER_CHAR_CHECKS.with(|checks| checks.set(0));
}

#[cfg(test)]
fn data_url_header_char_checks() -> u64 {
    DATA_URL_HEADER_CHAR_CHECKS.with(std::cell::Cell::get)
}

/// Header character of a `data:` URL after ASCII lowercasing.
///
/// RFC 2045 `token` is US-ASCII 33–126 except `tspecials`. A data URL also
/// needs the three tspecials `/` (type/subtype), `;` (parameter) and `=`
/// (attribute value). `%` is already a token character. Everything else —
/// space, controls, the remaining tspecials, and non-ASCII — ends the
/// header. It is not skipped: skipping an unknown character would hide the
/// comma and re-open the old quadratic scan. `:` stays a terminator so the
/// next `data:` occurrence stops this scan in constant work.
fn is_data_url_header_char(c: char) -> bool {
    if !c.is_ascii() || c.is_ascii_control() || c == ' ' {
        return false;
    }
    !matches!(
        c,
        '"' | '(' | ')' | '<' | '>' | '@' | ',' | ':' | '\\' | '[' | ']' | '?'
    )
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
