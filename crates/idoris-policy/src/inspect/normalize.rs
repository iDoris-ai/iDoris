//! L0 逃逸归一化（docs/15 §1.6 隐形类）：扫描前把全角 ASCII 折回半角、各种
//! 横线统一成 `-`、全角空格变成空格，并去掉零宽/格式字符；命中位置再映射回
//! 原文的 UTF-16 坐标，所以契约里的 span 永远指向调用方给的那段原文。
//!
//! 被删掉的零宽字符如果夹在一个命中中间，会被包含在映射回去的 span 里——
//! span 覆盖的是「从命中的第一个字符到最后一个字符」的原文区间。

pub(super) struct Normalized {
    pub units: Vec<u16>,
    /// `origin[i]` 是归一化后第 `i` 个 code unit 在原文里的下标。
    origin: Vec<usize>,
}

pub(super) fn normalize(units: &[u16]) -> Normalized {
    let mut out = Normalized {
        units: Vec::with_capacity(units.len()),
        origin: Vec::with_capacity(units.len()),
    };
    for (i, &u) in units.iter().enumerate() {
        let folded = match u {
            // ZWSP/ZWNJ/ZWJ、WORD JOINER、BOM/ZWNBSP、软连字符：不可见，直接丢掉。
            0x200B..=0x200D | 0x2060 | 0xFEFF | 0x00AD => continue,
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
