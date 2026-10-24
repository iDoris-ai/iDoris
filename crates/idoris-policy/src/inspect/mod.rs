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
//! 一共扫**四个视图**，各自把命中映射回原文坐标后取并集：
//!
//! 1. **原文**（`normalize::identity`）：不折叠、不删除。这是 base 的覆盖，
//!    保证「原文能抓到的，归一化后绝不放行」。折叠本身也会粘合（全角字母
//!    数字、横线变体、十进制数字、同形字母贴在实体边上），只有这一路不受影响。
//! 2. **折叠并保留**（`normalize::fold_keeping_ignorables`）：可忽略字符留在
//!    原地当边界，所以「可忽略字符 + 全角实体」这种形状只有它能命中。
//! 3. **折叠并删除**（`normalize::normalize`）：实体内部的隐形插入被去掉，
//!    所以「零宽字符夹在实体中间」只有它能命中。
//! 4. **数字与同形**（`normalize::fold_digits_and_homoglyphs`）：在删除可忽略
//!    字符的折叠上，再折 35 个 BMP 十进制数字块和选定的拉丁同形字母。它只
//!    增加命中，不替代前三路。
//!
//! 前三路缺一不可：`tests::union_is_never_looser_than_any_single_view` 会在
//! 任意一路被删掉时变红。第四路也不折进前三路，泰米尔零紧贴合法证件号、
//! 非拉丁文本旁边的原文密钥，仍然由原文视图保住。

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

/// Compare a known credential in raw text, JSON-style escaped text, and one
/// layer of common textual encodings. This deliberately does not recurse.
pub(crate) fn contains_credential(input: &str, credential: &str) -> bool {
    let credential = normalize::normalize(&credential.encode_utf16().collect::<Vec<_>>());
    if credential.units.is_empty() {
        return true;
    }
    let raw = input.as_bytes();
    if normalized_contains(input, &credential.units) || encoded_contains(raw, &credential.units) {
        return true;
    }
    let escaped = decode_json_escapes(input);
    normalized_contains(&escaped, &credential.units)
        || encoded_contains(escaped.as_bytes(), &credential.units)
}

fn normalized_contains(input: &str, credential: &[u16]) -> bool {
    let input = normalize::normalize(&input.encode_utf16().collect::<Vec<_>>());
    contains_seq(&input.units, credential)
}

/// Decode one JSON-style escape layer; malformed escapes remain literal.
fn decode_json_escapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 == bytes.len() {
            let Some(ch) = input[i..].chars().next() else {
                break;
            };
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match bytes[i + 1] {
            b'/' => {
                out.push('/');
                i += 2;
            }
            b'\\' => {
                out.push('\\');
                i += 2;
            }
            b'"' => {
                out.push('"');
                i += 2;
            }
            b'u' if i + 6 <= bytes.len() => {
                if let Some(first) = escaped_hex_unit(bytes, i + 2) {
                    if (0xD800..=0xDBFF).contains(&first) && bytes.get(i + 6..i + 8) == Some(b"\\u")
                    {
                        if let Some(second) = escaped_hex_unit(bytes, i + 8)
                            && (0xDC00..=0xDFFF).contains(&second)
                        {
                            let scalar = 0x1_0000
                                + ((u32::from(first) - 0xD800) << 10)
                                + (u32::from(second) - 0xDC00);
                            if let Some(ch) = char::from_u32(scalar) {
                                out.push(ch);
                                i += 12;
                                continue;
                            }
                        }
                    } else if let Some(ch) = char::from_u32(u32::from(first)) {
                        out.push(ch);
                        i += 6;
                        continue;
                    }
                }
                out.push('\\');
                i += 1;
            }
            _ => {
                out.push('\\');
                i += 1;
            }
        }
    }
    out
}

fn escaped_hex_unit(bytes: &[u8], start: usize) -> Option<u16> {
    bytes
        .get(start..start + 4)?
        .iter()
        .try_fold(0u16, |value, byte| {
            Some((value << 4) | u16::from(hex_value(*byte)?))
        })
}

/// Match only one common encoding layer. Prefixes cannot shift the hex view.
fn encoded_contains(input: &[u8], credential: &[u16]) -> bool {
    let Ok(secret) = String::from_utf16(credential) else {
        return false;
    };
    let bytes = secret.as_bytes();
    let hex_len = bytes.len().saturating_mul(2);
    if hex_len > 0 && hex_len <= input.len() {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = Vec::with_capacity(hex_len);
        for byte in bytes {
            encoded.push(HEX[usize::from(byte >> 4)]);
            encoded.push(HEX[usize::from(byte & 15)]);
        }
        let lowered = input.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
        if contains_seq(&lowered, &encoded) {
            return true;
        }
    }
    let mut start = 0;
    while start < input.len() {
        while start < input.len() && !is_encoded_member(input[start]) {
            start += 1;
        }
        let begin = start;
        while start < input.len() && is_encoded_member(input[start]) {
            start += 1;
        }
        let mut end = start;
        while end < input.len() && end - start < 2 && input[end] == b'=' {
            end += 1;
        }
        if begin < start {
            let token = &input[begin..end];
            if let Some(decoded) = decode_hex_token(token) {
                if contains_seq(&decoded, bytes) {
                    return true;
                }
                if let Ok(text) = std::str::from_utf8(&decoded)
                    && normalized_contains(text, credential)
                {
                    return true;
                }
            }
            for shift in 0..4.min(token.len()) {
                if let Some(decoded) = decode_base64_token(&token[shift..]) {
                    if contains_seq(&decoded, bytes) {
                        return true;
                    }
                    if let Ok(text) = std::str::from_utf8(&decoded)
                        && normalized_contains(text, credential)
                    {
                        return true;
                    }
                }
            }
        }
        if end > start {
            start = end;
        }
    }
    percent_decoded(input).is_some_and(|decoded| {
        contains_seq(&decoded, bytes)
            || std::str::from_utf8(&decoded).is_ok_and(|text| normalized_contains(text, credential))
    })
}

/// Linear-time exact substring matching, including long bounded fields.
fn contains_seq<T: Eq>(haystack: &[T], needle: &[T]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for i in 1..needle.len() {
        while matched > 0 && needle[i] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[i] == needle[matched] {
            matched += 1;
        }
        prefix[i] = matched;
    }
    matched = 0;
    for value in haystack {
        while matched > 0 && *value != needle[matched] {
            matched = prefix[matched - 1];
        }
        if *value == needle[matched] {
            matched += 1;
            if matched == needle.len() {
                return true;
            }
        }
    }
    false
}

fn is_encoded_member(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"+/_-".contains(&b)
}

fn decode_hex_token(token: &[u8]) -> Option<Vec<u8>> {
    let token = token
        .strip_prefix(b"0x")
        .or_else(|| token.strip_prefix(b"0X"))
        .unwrap_or(token);
    if token.is_empty() || !token.len().is_multiple_of(2) {
        return None;
    }
    token
        .chunks_exact(2)
        .map(|pair| Some(hex_value(pair[0])? << 4 | hex_value(pair[1])?))
        .collect()
}

fn decode_base64_token(token: &[u8]) -> Option<Vec<u8>> {
    let body = token
        .strip_suffix(b"==")
        .or_else(|| token.strip_suffix(b"="))
        .unwrap_or(token);
    if body.is_empty() || body.len() % 4 == 1 {
        return None;
    }
    let mut output = Vec::with_capacity(body.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in body {
        let value = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

fn percent_decoded(input: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    let mut escapes = 0;
    while i < input.len() {
        if input[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                input.get(i + 1).copied().and_then(hex_value),
                input.get(i + 2).copied().and_then(hex_value),
            ) {
                output.push(hi << 4 | lo);
                escapes += 1;
                i += 3;
            } else {
                output.push(input[i]);
                i += 1;
            }
        } else {
            output.push(input[i]);
            i += 1;
        }
    }
    (escapes > 0).then_some(output)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod encoded_credential_tests {
    use super::{contains_credential, decode_json_escapes, encoded_contains};

    #[test]
    fn exact_matcher_handles_prefixed_hex_and_binary_wrappers() {
        let credential = "ordinary-secret".encode_utf16().collect::<Vec<_>>();
        assert!(encoded_contains(
            b"X-6f7264696e6172792d736563726574",
            &credential
        ));
        assert!(encoded_contains(
            b"prefix:QUFvcmRpbmFyeS1zZWNyZXRaWg==:suffix",
            &credential
        ));
        assert!(encoded_contains(b"ordinary%2dsecret", &credential));
        assert!(!encoded_contains(
            b"X-6f7264696e6172792d726573756c74",
            &credential
        ));
    }

    #[test]
    fn raw_and_json_simple_escape_views_are_both_compared() {
        assert!(contains_credential("a/b", "a/b"));
        assert!(contains_credential(r#"a\/b"#, "a/b"));
        assert!(contains_credential(r#"a\\b"#, "a\\b"));
        assert!(contains_credential(r#"a\"b"#, "a\"b"));
        assert!(contains_credential(r#"\uD83D\uDE00"#, "😀"));
        assert_eq!(decode_json_escapes(r#"a\/b"#), "a/b");
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
        normalize::fold_digits_and_homoglyphs(units),
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
