//! L0 逃逸归一化（docs/15 §1.6 隐形类）：扫描前把全角 ASCII 折回半角、各种
//! 横线统一成 `-`、全角空格变成空格。`normalize` 另外去掉 Unicode
//! `Default_Ignorable_Code_Point`（含非 BMP 的 TAG / 变体选择符等）；
//! `fold_keeping_ignorables` 做同样的折叠，但把可忽略字符留在原地当边界。
//! `fold_digits_and_homoglyphs` 在删除可忽略字符的折叠上，再把 35 个 BMP
//! 十进制数字块折成 ASCII 数字，并把选定的西里尔/希腊同形字母折成拉丁字母。
//! 四路都把命中映射回原文的 UTF-16 坐标，契约里的 span 永远指向调用方给的原文。
//!
//! 只删不留会把两侧的 ASCII 字母数字粘成一个更长的 run（`Key` 粘上 `sk-`、
//! 孤立数字粘上手机号），精确长度和前缀匹配因此放行。折叠也会粘合（全角
//! 字母数字、横线变体、十进制数字、同形字母贴在实体边上）。调用方必须四路
//! 都扫：原文、折叠保留可忽略字符、折叠删除可忽略字符、数字与同形，取并集。
//! 前三路不折数字和同形字母，所以泰米尔零或西里尔邻居不能把原文已经能命中的
//! 实体粘没。被删掉的可忽略字符如果夹在一个命中中间，会被包含在映射回去的
//! span 里——span 覆盖的是「从命中的第一个字符到最后一个字符」的原文区间。
//! 代理对整对丢弃；孤立的高/低代理保留，绝不半对丢弃。

pub(super) struct Normalized {
    pub units: Vec<u16>,
    /// `origin[i]` 是归一化后第 `i` 个 code unit 在原文里的下标。
    origin: Vec<usize>,
}

/// Unicode DerivedCoreProperties.txt `Default_Ignorable_Code_Point`
/// （覆盖 BMP 与补充平面；含保留码位按属性表一并忽略）。
fn is_default_ignorable(cp: u32) -> bool {
    matches!(
        cp,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

fn is_high_surrogate(u: u16) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

fn is_low_surrogate(u: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

fn decode_surrogate_pair(high: u16, low: u16) -> u32 {
    0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
}

/// 原文视图：不折叠、不删除，逐 unit 恒等映射。
///
/// 折叠本身也会把紧挨实体的全角字母数字 / 横线变体粘成更长的 run
/// （`１13812345678`、`a@b.com－9`、`.envｘ`），于是精确长度、TLD 结尾和
/// `.env` 结尾判断都会失败——比 base 还要松。调用方必须把这一路也扫进并集，
/// 「原文能抓到的，归一化后绝不能放行」。
pub(super) fn identity(units: &[u16]) -> Normalized {
    Normalized {
        units: units.to_vec(),
        origin: (0..units.len()).collect(),
    }
}

pub(super) fn normalize(units: &[u16]) -> Normalized {
    rewrite(units, true, false)
}

/// 全角 / 横线照常折叠，Default_Ignorable 留在输出里，扫描时自然成为切分点。
pub(super) fn fold_keeping_ignorables(units: &[u16]) -> Normalized {
    rewrite(units, false, false)
}

/// 数字与同形视图：全角、横线、删除可忽略字符，再折 BMP 十进制数字和拉丁同形字母。
///
/// 只作为并集里的额外一路。原文、保留、删除三路仍不折这两类，避免泰米尔零
/// 或西里尔邻居把已经能命中的实体粘没。
pub(super) fn fold_digits_and_homoglyphs(units: &[u16]) -> Normalized {
    rewrite(units, true, true)
}

/// 字面 `\u` 后紧跟 1-3 个数字再贴着更长的数字 run（`\u1` + 18 位身份证 = 19 位）
/// 会让精确长度判断放行。本树不解码 `\u`，所以这里把 `\u` 之后最前面的 `k`
/// 个 ASCII 数字当作转义前缀跳过，其余 unit 原样保留并映射回原文坐标。
/// 只有存在 `\u` + 至少 `k + 1` 个连续数字时才返回视图，否则为 `None`。
pub(super) fn skip_short_escape_digits(units: &[u16], k: usize) -> Option<Normalized> {
    let digit = |u: u16| (u16::from(b'0')..=u16::from(b'9')).contains(&u);
    let mut out = Normalized {
        units: Vec::with_capacity(units.len()),
        origin: Vec::with_capacity(units.len()),
    };
    let mut changed = false;
    let mut i = 0;
    while i < units.len() {
        out.units.push(units[i]);
        out.origin.push(i);
        let after_escape = units[i] == u16::from(b'u') && i > 0 && units[i - 1] == u16::from(b'\\');
        i += 1;
        if after_escape && i + k < units.len() && units[i..=i + k].iter().all(|&u| digit(u)) {
            i += k;
            changed = true;
        }
    }
    changed.then_some(out)
}

fn rewrite(units: &[u16], drop_ignorables: bool, fold_scripts: bool) -> Normalized {
    let mut out = Normalized {
        units: Vec::with_capacity(units.len()),
        origin: Vec::with_capacity(units.len()),
    };
    let mut i = 0;
    while i < units.len() {
        let u = units[i];

        // 完整代理对：按标量值判断是否 Default_Ignorable；要丢就整对丢掉。
        if is_high_surrogate(u)
            && let Some(&low) = units.get(i + 1)
            && is_low_surrogate(low)
        {
            let cp = decode_surrogate_pair(u, low);
            if drop_ignorables && is_default_ignorable(cp) {
                i += 2;
                continue;
            }
            out.units.push(u);
            out.origin.push(i);
            out.units.push(low);
            out.origin.push(i + 1);
            i += 2;
            continue;
        }

        // 孤立高代理落到这里：保留，绝不半对丢弃。
        // BMP（含孤立低代理）：按单 unit 判断。
        if drop_ignorables && is_default_ignorable(u32::from(u)) {
            i += 1;
            continue;
        }

        out.units.push(fold_bmp(u, fold_scripts));
        out.origin.push(i);
        i += 1;
    }
    out
}

fn fold_bmp(u: u16, fold_scripts: bool) -> u16 {
    if fold_scripts {
        if let Some(zero) = decimal_digit_zero(u) {
            return u - zero + u16::from(b'0');
        }
        if let Some(latin) = latin_confusable(u) {
            return u16::from(latin);
        }
    }
    match u {
        // 全角 ASCII（！到～）折回半角。
        0xFF01..=0xFF5E => u - 0xFEE0,
        // 全角空格。
        0x3000 => u16::from(b' '),
        // 连字符、破折号、减号：数字分组里都按 `-` 处理。
        0x2010..=0x2015 | 0x2212 => u16::from(b'-'),
        _ => u,
    }
}

/// BMP 里十位连续的十进制数字块（Unicode `Nd`），以各自的零为起点。
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

/// 西里尔 / 希腊字母里和拉丁字母同形的那些（`ѕk-`、`АКIА`）。
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
        let last = self.origin[start + length - 1];
        (first, last + 1 - first)
    }
}
