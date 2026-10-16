//! L0 逃逸归一化（docs/15 §1.6 隐形类）：扫描前把全角 ASCII 折回半角、各种
//! 横线统一成 `-`、全角空格变成空格。`normalize` 另外去掉 Unicode
//! `Default_Ignorable_Code_Point`（含非 BMP 的 TAG / 变体选择符等）；
//! `fold_keeping_ignorables` 做同样的折叠，但把可忽略字符留在原地当边界。
//! 两路都把命中映射回原文的 UTF-16 坐标，契约里的 span 永远指向调用方给的原文。
//!
//! 只删不留会把两侧的 ASCII 字母数字粘成一个更长的 run（`Key` 粘上 `sk-`、
//! 孤立数字粘上手机号），精确长度和前缀匹配因此放行。调用方必须两路都扫，
//! 取并集。被删掉的可忽略字符如果夹在一个命中中间，会被包含在映射回去的
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

pub(super) fn normalize(units: &[u16]) -> Normalized {
    rewrite(units, true)
}

/// 全角 / 横线照常折叠，Default_Ignorable 留在输出里，扫描时自然成为切分点。
pub(super) fn fold_keeping_ignorables(units: &[u16]) -> Normalized {
    rewrite(units, false)
}

fn rewrite(units: &[u16], drop_ignorables: bool) -> Normalized {
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

        let folded = match u {
            // 全角 ASCII（！到～）折回半角。
            0xFF01..=0xFF5E => u - 0xFEE0,
            // 全角空格。
            0x3000 => u16::from(b' '),
            // 连字符、破折号、减号：数字分组里都按 `-` 处理。
            0x2010..=0x2015 | 0x2212 => u16::from(b'-'),
            _ => u,
        };
        out.units.push(folded);
        out.origin.push(i);
        i += 1;
    }
    out
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
