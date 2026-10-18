//! L0 逃逸归一化（docs/15 §1.6 隐形类）：扫描前把全角 ASCII 折回半角、各种
//! 横线统一成 `-`、全角空格变成空格，并去掉零宽/格式字符；命中位置再映射回
//! 原文的 UTF-16 坐标，所以契约里的 span 永远指向调用方给的那段原文。
//!
//! 被删掉的零宽字符如果夹在一个命中中间，会被包含在映射回去的 span 里——
//! span 覆盖的是「从命中的第一个字符到最后一个字符」的原文区间。

#[derive(PartialEq, Eq)]
pub(super) struct Normalized {
    pub units: Vec<u16>,
    /// `origin[i]..origin_end[i]` is the original range the `i`-th
    /// normalized unit came from (one unit, or a whole `\uXXXX` escape or
    /// surrogate pair that collapsed into it).
    origin: Vec<usize>,
    origin_end: Vec<usize>,
}

pub(super) fn normalize(units: &[u16]) -> Normalized {
    normalize_with_options(units, true, true)
}

/// Keep digit/fullwidth normalization but preserve non-Latin token boundaries.
/// This view is scanned too, so letter folding cannot erase existing findings.
pub(super) fn normalize_without_letter_folding(units: &[u16]) -> Normalized {
    normalize_with_options(units, false, true)
}

/// Independent letter and escape views prevent either fold from erasing hits.
pub(super) fn views(units: &[u16]) -> Vec<Normalized> {
    let escaped = units
        .windows(2)
        .any(|pair| pair == [u16::from(b'\\'), u16::from(b'u')]);
    let letters = escaped || units.iter().any(|&u| latin_confusable(u).is_some());
    let mut views = vec![normalize(units)];
    let mut add = |view: Normalized| {
        // Equal units alone are insufficient: spans must share the same source map.
        if !views.contains(&view) {
            views.push(view);
        }
    };
    if letters {
        add(normalize_without_letter_folding(units));
    }
    if escaped {
        add(normalize_with_options(units, true, false));
        if letters {
            add(normalize_with_options(units, false, false));
        }
    }
    views
}

fn normalize_with_options(units: &[u16], fold_letters: bool, fold_escapes: bool) -> Normalized {
    let mut out = Normalized {
        units: Vec::with_capacity(units.len()),
        origin: Vec::with_capacity(units.len()),
        origin_end: Vec::with_capacity(units.len()),
    };
    let mut i = 0;
    while i < units.len() {
        // A literal `\uXXXX` escape (JSON/JS source) stands for its unit, in
        // place, so it joins the surrounding text; a lone surrogate is
        // kept as-is rather than failing the decode.
        let escaped = fold_escapes.then(|| unicode_escape(&units[i..])).flatten();
        let (unit, width) = if let Some(escaped) = escaped {
            (escaped, 6)
        } else if let Some(digit) = math_digit(&units[i..]) {
            (digit, 2)
        } else {
            (units[i], 1)
        };
        if let Some(folded) = fold(unit, fold_letters) {
            out.units.push(folded);
            out.origin.push(i);
            out.origin_end.push(i + width);
        }
        i += width;
    }
    out
}

/// `\uXXXX` at the start of `units`.
fn unicode_escape(units: &[u16]) -> Option<u16> {
    let window = units.get(..6)?;
    if window[0] != u16::from(b'\\') || window[1] != u16::from(b'u') {
        return None;
    }
    window[2..].iter().try_fold(0u16, |acc, &u| {
        let digit = char::from_u32(u32::from(u))?.to_digit(16)?;
        Some(acc << 4 | u16::try_from(digit).ok()?)
    })
}

/// Mathematical alphanumeric digits U+1D7CE..=U+1D7FF (bold, double-struck,
/// sans-serif, monospace …), encoded as the surrogate pair D835 DFCE..DFFF.
fn math_digit(units: &[u16]) -> Option<u16> {
    match units {
        [0xD835, low @ 0xDFCE..=0xDFFF, ..] => Some((low - 0xDFCE) % 10 + u16::from(b'0')),
        _ => None,
    }
}

/// One unit's fold, or `None` to drop it.
fn fold(u: u16, fold_letters: bool) -> Option<u16> {
    Some(match u {
        // ZWSP/ZWNJ/ZWJ、WORD JOINER、BOM/ZWNBSP、软连字符，以及双向文字
        // 控制字符（LRM/RLM/ALM、嵌入/覆盖、隔离）：不可见，直接丢掉。
        0x200B..=0x200F | 0x2060 | 0xFEFF | 0x00AD | 0x061C | 0x202A..=0x202E | 0x2066..=0x2069 => {
            return None;
        }
        // 不换行空格、各种宽度的空格：按普通空格处理。
        0x00A0 | 0x2000..=0x200A | 0x202F | 0x205F => u16::from(b' '),
        // 其他文字的十进制数字（阿拉伯-印度、天城文、泰文……）。
        _ if decimal_digit_zero(u).is_some() => {
            decimal_digit_zero(u).map_or(u, |zero| u - zero + u16::from(b'0'))
        }
        // 西里尔/希腊字母里和拉丁字母同形的那些（`ѕk-`、`АКIА`）。
        _ if fold_letters && latin_confusable(u).is_some() => {
            latin_confusable(u).map_or(u, u16::from)
        }
        // 全角 ASCII（！到～）折回半角。
        0xFF01..=0xFF5E => u - 0xFEE0,
        // 全角空格。
        0x3000 => u16::from(b' '),
        // 连字符、破折号、减号：数字分组里都按 `-` 处理。
        0x2010..=0x2015 | 0x2212 => u16::from(b'-'),
        _ => u,
    })
}

/// BMP blocks whose ten decimal digits are contiguous, by the code point of
/// their zero (Unicode `Nd`).
const DIGIT_ZEROS: &[u16] = &[
    0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6, 0x0D66,
    0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
];

fn decimal_digit_zero(u: u16) -> Option<u16> {
    DIGIT_ZEROS
        .iter()
        .copied()
        .find(|&zero| (zero..zero + 10).contains(&u))
}

/// Cyrillic and Greek letters that render like a Latin letter.
fn latin_confusable(u: u16) -> Option<u8> {
    Some(match u {
        0x0430 => b'a',
        0x0435 => b'e',
        0x043E | 0x03BF => b'o',
        0x0440 => b'p',
        0x0441 => b'c',
        0x0443 => b'y',
        0x0445 => b'x',
        0x0455 => b's',
        0x0456 => b'i',
        0x0458 => b'j',
        0x0410 | 0x0391 => b'A',
        0x0412 | 0x0392 => b'B',
        0x0415 | 0x0395 => b'E',
        0x041A | 0x039A => b'K',
        0x041C | 0x039C => b'M',
        0x041D | 0x0397 => b'H',
        0x041E | 0x039F => b'O',
        0x0420 | 0x03A1 => b'P',
        0x0421 => b'C',
        0x0422 | 0x03A4 => b'T',
        0x0425 | 0x03A7 => b'X',
        0x0406 | 0x0399 => b'I',
        0x0408 => b'J',
        0x0405 => b'S',
        0x0396 => b'Z',
        0x039D => b'N',
        0x03A5 => b'Y',
        _ => return None,
    })
}

impl Normalized {
    /// 归一化坐标里的 `[start, start + length)` 对应的原文 `(start, length)`。
    /// `length` 必须 ≥ 1（命中永远非空）。
    pub fn original_span(&self, start: usize, length: usize) -> (usize, usize) {
        let first = self.origin[start];
        let end = self.origin_end[start + length - 1];
        (first, end - first)
    }
}
