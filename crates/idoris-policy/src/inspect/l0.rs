//! L0 中文数字类实体（docs/15 §1.7）：居民身份证、手机号、银行卡号。
//!
//! 只用线性扫描，不用正则：先找「数字组」——ASCII 数字，组内允许单个空格或
//! `-` 分隔（`138-1234-5678`、`6222 0212 …`）。每个数字组既按整组（去掉分隔
//! 符）判定，也按组内每一段连续数字单独判定，所以「手机号 空格 银行卡号」
//! 被并成一个大组时，两段仍各自命中。宁可多报，也不能因为分组方式漏报。
//! 另以最多 19 位、5 段的窗口识别 tab、点、斜线等常见分隔格式，不跨连续分隔符或换行。
//!
//! 结构校验压倒校验和：身份证先看省级行政区划码和出生日期，校验位只决定
//! 敏感度（X-2）；银行卡没有可用的结构信号，所以 Luhn 是必要条件。

use idoris_contracts::inspect::{FindingKind, Sensitivity};

use super::Hit;

/// GB/T 2260 省级行政区划码（身份证前两位）。
pub(super) const PROVINCES: &[u8] = &[
    11, 12, 13, 14, 15, 21, 22, 23, 31, 32, 33, 34, 35, 36, 37, 41, 42, 43, 44, 45, 46, 50, 51, 52,
    53, 54, 61, 62, 63, 64, 65, 71, 81, 82,
];
/// GB 11643-1999 / ISO 7064 MOD 11-2 权重与校验码表。
const ID_WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
const ID_CHECK: &[u8; 11] = b"10X98765432";

/// 一个待判定的数字串：数字值、UTF-16 起点、终点（不含尾随 `X`）、
/// 紧跟的 `X`/`x`（只对 17 位数字有意义：身份证校验位）。
struct Candidate<'a> {
    digits: &'a [u8],
    start: usize,
    end: usize,
    x_suffix: bool,
}

pub(super) fn scan(units: &[u16]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut i = 0;
    while i < units.len() {
        if !is_digit(units[i]) {
            i += 1;
            continue;
        }
        let group_end = group_end(units, i);
        let x_suffix = units
            .get(group_end)
            .is_some_and(|&u| u == u16::from(b'X') || u == u16::from(b'x'));
        let joined: Vec<u8> = units[i..group_end]
            .iter()
            .filter(|&&u| is_digit(u))
            .map(|&u| (u - u16::from(b'0')) as u8)
            .collect();
        classify(
            &Candidate {
                digits: &joined,
                start: i,
                end: group_end,
                x_suffix,
            },
            &mut hits,
        );
        if joined.len() != group_end - i {
            let mut offset = 0;
            let mut seg_start = i;
            while seg_start < group_end {
                let mut seg_end = seg_start;
                while seg_end < group_end && is_digit(units[seg_end]) {
                    seg_end += 1;
                }
                let len = seg_end - seg_start;
                let last = seg_end == group_end;
                let candidate = Candidate {
                    digits: &joined[offset..offset + len],
                    start: seg_start,
                    end: seg_end,
                    x_suffix: last && x_suffix,
                };
                classify(&candidate, &mut hits);
                offset += len;
                seg_start = seg_end + 1;
            }
        }
        i = group_end;
    }
    scan_formatted(units, &mut hits);
    hits.sort_by_key(|hit| (hit.start, hit.length));
    hits.dedup_by_key(|hit| (hit.kind, hit.start, hit.length));
    hits
}

fn is_digit(u: u16) -> bool {
    (u16::from(b'0')..=u16::from(b'9')).contains(&u)
}

fn is_separator(u: u16) -> bool {
    u == u16::from(b' ') || u == u16::from(b'-')
}

/// Require complete decomposition, with at most two short edge numbers (time, CVV).
/// Arbitrary subwindows flag numeric tables: see 7c6cee4 / aced94d.
fn scan_formatted(units: &[u16], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i < units.len() {
        if !is_digit(units[i]) {
            i += 1;
            continue;
        }
        let mut segments = Vec::new();
        loop {
            let start = i;
            while i < units.len() && is_digit(units[i]) {
                i += 1;
            }
            segments.push((start, i));
            let joins = units.get(i).is_some_and(|&u| {
                is_separator(u) || [b'\t', b'.', b'/'].map(u16::from).contains(&u)
            }) && units.get(i + 1).is_some_and(|&u| is_digit(u));
            if !joins {
                break;
            }
            i += 1;
        }
        let n = segments.len();
        let short = |k: usize| segments[k].1 - segments[k].0 <= 3;
        let mut decomposes: Vec<bool> = (0..=n).map(|k| n - k <= 2 && (k..n).all(short)).collect();
        let mut choices = vec![None; n];
        for a in (0..n).rev() {
            for b in a..n.min(a + 5) {
                if !decomposes[b + 1] {
                    continue;
                }
                let found = formatted_window(units, &segments[a..=b]);
                if !found.is_empty() {
                    decomposes[a] = true;
                    choices[a] = Some((b + 1, found));
                    break;
                }
            }
        }
        let head = (0..=2.min(n))
            .take_while(|&h| (0..h).all(short))
            .find(|&h| choices.get(h).is_some_and(Option::is_some));
        if let Some(mut a) = head {
            while let Some((end, found)) = choices.get_mut(a).and_then(Option::take) {
                hits.extend(found);
                a = end;
            }
        }
    }
}

/// Fixed 19-digit / five-segment bound; only phone and four-digit card groupings.
/// Ordinary numbers written exactly like an entity remain unavoidably ambiguous.
fn formatted_window(units: &[u16], segments: &[(usize, usize)]) -> Vec<Hit> {
    let lengths: Vec<usize> = segments.iter().map(|&(s, e)| e - s).collect();
    let count: usize = lengths.iter().sum();
    let mobile_shape = matches!(lengths.as_slice(), [3, 4, 4] | [2, 3, 4, 4] | [2, 11]);
    let card_shape = (16..=19).contains(&count)
        && lengths.len() >= 4
        && lengths[..lengths.len() - 1].iter().all(|&len| len == 4)
        && lengths.last().is_some_and(|len| (1..=4).contains(len));
    let mut hits = Vec::new();
    if (segments.len() == 1 || mobile_shape || card_shape) && count <= 19 {
        let digits: Vec<u8> = segments
            .iter()
            .flat_map(|&(s, e)| units[s..e].iter())
            .map(|&u| (u - u16::from(b'0')) as u8)
            .collect();
        classify(
            &Candidate {
                digits: &digits,
                start: segments[0].0,
                end: segments[segments.len() - 1].1,
                x_suffix: false,
            },
            &mut hits,
        );
    }
    hits
}

/// 从 `start`（必须是数字）开始的数字组终点：分隔符只有在两侧都是数字时才算组内。
fn group_end(units: &[u16], start: usize) -> usize {
    let mut j = start;
    loop {
        while j < units.len() && is_digit(units[j]) {
            j += 1;
        }
        let joins = j + 1 < units.len() && is_separator(units[j]) && is_digit(units[j + 1]);
        if !joins {
            return j;
        }
        j += 1;
    }
}

fn classify(c: &Candidate<'_>, hits: &mut Vec<Hit>) {
    let mut push = |kind, end: usize, sensitivity| {
        hits.push(Hit {
            kind,
            start: c.start,
            length: end - c.start,
            sensitivity,
        });
    };
    let d = c.digits;
    if d.len() == 17 && c.x_suffix && id_structure(d) {
        push(
            FindingKind::CnResidentId,
            c.end + 1,
            id_sensitivity(d, b'X'),
        );
    }
    if d.len() == 18 && id_structure(&d[..17]) {
        push(
            FindingKind::CnResidentId,
            c.end,
            id_sensitivity(&d[..17], b'0' + d[17]),
        );
    }
    if d.len() == 15 && legacy_id_structure(d) {
        push(FindingKind::CnResidentId, c.end, Sensitivity::S2);
    }
    let mobile = match d.len() {
        11 => is_mobile(d),
        13 => d[..2] == [8, 6] && is_mobile(&d[2..]),
        _ => false,
    };
    if mobile {
        push(FindingKind::CnMobile, c.end, Sensitivity::S2);
    }
    if (16..=19).contains(&d.len()) && d[0] != 0 && luhn_valid(d) {
        push(FindingKind::BankCard, c.end, Sensitivity::S3);
    }
}

/// 18 位身份证前 17 位：省级区划码 + 合法的 YYYYMMDD 出生日期。
fn id_structure(d: &[u8]) -> bool {
    let year = number(&d[6..10]);
    PROVINCES.contains(&(d[0] * 10 + d[1]))
        && (1900..=2100).contains(&year)
        && valid_date(year, number(&d[10..12]), number(&d[12..14]))
}

/// 15 位旧身份证：省级区划码 + YYMMDD（19YY 年），没有校验位。
fn legacy_id_structure(d: &[u8]) -> bool {
    let year = 1900 + number(&d[6..8]);
    PROVINCES.contains(&(d[0] * 10 + d[1]))
        && valid_date(year, number(&d[8..10]), number(&d[10..12]))
}

/// 校验位正确 → S3（证件）；不正确（OCR、手误）→ 降级为 S2，但仍然是命中。
fn id_sensitivity(first17: &[u8], check: u8) -> Sensitivity {
    let sum: u32 = first17
        .iter()
        .zip(ID_WEIGHTS)
        .map(|(&digit, weight)| u32::from(digit) * weight)
        .sum();
    if ID_CHECK[(sum % 11) as usize] == check.to_ascii_uppercase() {
        Sensitivity::S3
    } else {
        Sensitivity::S2
    }
}

/// MIIT 11 位手机号 `1[3-9]\d{9}`。
fn is_mobile(d: &[u8]) -> bool {
    d.len() == 11 && d[0] == 1 && (3..=9).contains(&d[1])
}

fn luhn_valid(d: &[u8]) -> bool {
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &digit)| {
            let digit = u32::from(digit);
            match (i % 2 == 1, digit * 2) {
                (false, _) => digit,
                (true, doubled) if doubled > 9 => doubled - 9,
                (true, doubled) => doubled,
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

fn number(d: &[u8]) -> u32 {
    d.iter().fold(0, |acc, &digit| acc * 10 + u32::from(digit))
}

fn valid_date(year: u32, month: u32, day: u32) -> bool {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}
