//! L0 编码类逃逸（docs/15 §1.6）：base64 / base64url、百分号编码、hex。
//!
//! 对每个长得像编码串的 token 解码**一层**，解出合法 UTF-8 就用同一套 L0
//! （先归一化再扫描）检查解码后的文本；有命中就按命中的类别和敏感度报告
//! 整个编码串的原位置。只解一层：嵌套编码的成本无界。
//!
//! 百分号编码：只要有 ≥ 1 个合法 `%XX` 就解码；非法 `%`（如 `%ZZ`）原样保留并
//! 继续扫描，避免一个坏转义把整段合法内容抹掉。短百分号密钥（例如一个转义
//! 藏起 `sk-`）不依赖高熵规则。
//!
//! 已知缺口（不在本层覆盖）：UTF-16LE base64（如 PowerShell `-EncodedCommand`）
//! 解出的数字被 NUL 隔开；按 76 列折行的 MIME/PEM base64 跨行密钥。

use super::Hit;

/// 至少编码 9 个字节才值得解（短于 12 个字符的 base64 太容易撞上普通词）。
const MIN_BASE64: usize = 12;
const MIN_HEX: usize = 16;
const MIN_PERCENT_ESCAPES: usize = 1;

pub(super) fn scan(units: &[u16], hits: &mut Vec<Hit>) {
    // base64/hex: the base64url-or-standard alphabet plus up to two `=` of
    // padding. `=` elsewhere separates (`payload=<token>`).
    for (start, run_end) in runs(units, |b| b.is_ascii_alphanumeric() || b"+/_-".contains(&b)) {
        let mut end = run_end;
        while end < units.len() && end - run_end < 2 && units[end] == u16::from(b'=') {
            end += 1;
        }
        let token = ascii_bytes(&units[start..end]);
        for decoded in [decode_hex(&token), decode_base64(&token)]
            .into_iter()
            .flatten()
        {
            report(&decoded, start, end - start, hits);
        }
    }
    if let Some(view) = decode_percent(units) {
        for mut hit in super::scan_all_views(&view.units, super::l0::scan_plain) {
            let first = view.origin[hit.start].0;
            let end = view.origin[hit.start + hit.length - 1].1;
            hit.start = first;
            hit.length = end - first;
            hits.push(hit);
        }
    }
}

/// Maximal runs of ASCII bytes accepted by `member`, as `[start, end)`.
fn runs(units: &[u16], member: impl Fn(u8) -> bool) -> Vec<(usize, usize)> {
    let is_member = |u: u16| u8::try_from(u).is_ok_and(|b| b.is_ascii() && member(b));
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        if !is_member(units[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < units.len() && is_member(units[i]) {
            i += 1;
        }
        out.push((start, i));
    }
    out
}

/// Callers only pass runs of ASCII units.
fn ascii_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().map(|&u| u as u8).collect()
}

fn report(decoded: &[u8], start: usize, length: usize, hits: &mut Vec<Hit>) {
    let Ok(text) = std::str::from_utf8(decoded) else {
        return;
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    for inner in super::scan_all_views(&units, super::l0::scan_plain) {
        hits.push(Hit {
            kind: inner.kind,
            start,
            length,
            sensitivity: inner.sensitivity,
        });
    }
}

struct PercentView {
    units: Vec<u16>,
    origin: Vec<(usize, usize)>,
}

/// Decode one percent layer across the whole input, preserving separators and
/// UTF-16 source ranges. Invalid escapes/UTF-8 stay literal, never erase text.
/// `+` stays literal: this is percent encoding, not form-urlencoded parsing.
fn decode_percent(input: &[u16]) -> Option<PercentView> {
    fn byte_at(input: &[u16], at: usize) -> Option<u8> {
        let chunk = input.get(at..at + 3)?;
        if chunk[0] != u16::from(b'%') {
            return None;
        }
        Some(
            hex_value(u8::try_from(chunk[1]).ok()?)? << 4
                | hex_value(u8::try_from(chunk[2]).ok()?)?,
        )
    }
    let mut view = PercentView {
        units: Vec::with_capacity(input.len()),
        origin: Vec::with_capacity(input.len()),
    };
    let mut escapes = 0;
    let mut i = 0;
    while i < input.len() {
        let decoded = byte_at(input, i).and_then(|first| {
            let width = match first {
                0..=0x7f => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return None,
            };
            let mut bytes = [0u8; 4];
            for (n, byte) in bytes.iter_mut().enumerate().take(width) {
                *byte = byte_at(input, i + n * 3)?;
            }
            let text = std::str::from_utf8(&bytes[..width]).ok()?;
            Some((text.chars().next()?, width * 3))
        });
        if let Some((character, consumed)) = decoded {
            let mut buffer = [0; 2];
            for &unit in character.encode_utf16(&mut buffer).iter() {
                view.units.push(unit);
                view.origin.push((i, i + consumed));
            }
            escapes += 1;
            i += consumed;
        } else {
            view.units.push(input[i]);
            view.origin.push((i, i + 1));
            i += 1;
        }
    }
    (escapes >= MIN_PERCENT_ESCAPES).then_some(view)
}

fn decode_hex(token: &[u8]) -> Option<Vec<u8>> {
    if token.len() < MIN_HEX || !token.len().is_multiple_of(2) {
        return None;
    }
    token
        .chunks(2)
        .map(|pair| Some(hex_value(pair[0])? << 4 | hex_value(pair[1])?))
        .collect()
}

/// 标准与 URL-safe 字母表都认，补位可有可无。
fn decode_base64(token: &[u8]) -> Option<Vec<u8>> {
    let body = token
        .strip_suffix(b"==")
        .or_else(|| token.strip_suffix(b"="))
        .unwrap_or(token);
    if body.len() < MIN_BASE64 || body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for &c in body {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buffer = buffer << 6 | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
