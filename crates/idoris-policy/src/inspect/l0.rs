//! L0 中文数字类实体（docs/15 §1.7）：居民身份证、手机号、银行卡号。
//!
//! 只用线性扫描，不用正则：先找「数字组」——ASCII 数字，组内允许单个空格或
//! `-` 分隔（`138-1234-5678`、`6222 0212 …`）。每个数字组既按整组（去掉分隔
//! 符）判定，也按组内每一段连续数字单独判定，所以「手机号 空格 银行卡号」
//! 被并成一个大组时，两段仍各自命中。宁可多报，也不能因为分组方式漏报。
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
    /// Lengths of the digit runs spanned: one run is an unbroken number,
    /// several are a written grouping.
    runs: &'a [usize],
    start: usize,
    end: usize,
    x_suffix: bool,
}

/// 明文规则，加上解码一层之后的同一套明文规则（[`super::decode`]）。
pub(super) fn scan(units: &[u16]) -> Vec<Hit> {
    let mut hits = scan_plain(units);
    super::decode::scan(units, &mut hits);
    sort_dedup(hits)
}

/// 只看明文：数字类、字母数字类、凭据类规则。
pub(super) fn scan_plain(units: &[u16]) -> Vec<Hit> {
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
        let mut segments: Vec<(usize, usize)> = Vec::new();
        let mut k = i;
        while k < group_end {
            if is_digit(units[k]) {
                let seg_start = k;
                while k < group_end && is_digit(units[k]) {
                    k += 1;
                }
                segments.push((seg_start, k));
            } else {
                k += 1;
            }
        }
        // Each run alone; otherwise windows of several runs must be a real
        // written form of the entity (see `classify`). The group is judged
        // as one decomposition into neighbouring entity windows, which may
        // start and end with up to two short numbers (≤ 3 digits: an
        // expiry, a CVV, an hour): `4111 1111 1111 1111 12 28`,
        // `19 138 1234 5678`, `110101 19900307 7379 138 1234 5678`. A row
        // of random numbers almost never decomposes (four reviews tonight
        // traded recall against false positives on exactly this rule).
        let n = segments.len();
        let lengths: Vec<usize> = segments.iter().map(|&(s, e)| e - s).collect();
        let mut prefix = vec![0usize; n + 1];
        for (k, &len) in lengths.iter().enumerate() {
            prefix[k + 1] = prefix[k] + len;
        }
        let window = |a: usize, b: usize| -> Vec<Hit> {
            let mut found = Vec::new();
            if prefix[b + 1] - prefix[a] > MAX_ENTITY_DIGITS {
                return found;
            }
            let digits: Vec<u8> = segments[a..=b]
                .iter()
                .flat_map(|&(seg_start, seg_end)| units[seg_start..seg_end].iter())
                .map(|&u| (u - u16::from(b'0')) as u8)
                .collect();
            classify(
                &Candidate {
                    digits: &digits,
                    runs: &lengths[a..=b],
                    start: segments[a].0,
                    end: segments[b].1,
                    x_suffix: x_suffix && b + 1 == n,
                },
                &mut found,
            );
            found
        };
        for a in 0..n {
            hits.extend(window(a, a));
        }
        if n > 1 {
            let short = |k: usize| lengths[k] <= MAX_SHORT_EDGE_DIGITS;
            // tail_ok[k]: runs k.. are at most two short numbers (or none).
            let tail_ok: Vec<bool> = (0..=n)
                .map(|k| n - k <= MAX_SHORT_EDGE_RUNS && (k..n).all(short))
                .collect();
            // decomposes[a]: runs a.. are one or more entity windows, then a
            // short tail; first_window[a]: the shortest such window at `a`.
            let mut decomposes = vec![false; n + 1];
            let mut first_window: Vec<Option<(usize, Vec<Hit>)>> = (0..n).map(|_| None).collect();
            for a in (0..n).rev() {
                for b in a..n {
                    if prefix[b + 1] - prefix[a] > MAX_ENTITY_DIGITS {
                        break;
                    }
                    if !(decomposes[b + 1] || tail_ok[b + 1]) {
                        continue;
                    }
                    let found = window(a, b);
                    if !found.is_empty() {
                        decomposes[a] = true;
                        first_window[a] = Some((b, found));
                        break;
                    }
                }
            }
            let head = (0..=MAX_SHORT_EDGE_RUNS.min(n))
                .take_while(|&h| (0..h).all(short))
                .find(|&h| decomposes[h]);
            if let Some(mut a) = head {
                while let Some((b, found)) = first_window.get_mut(a).and_then(Option::take) {
                    hits.extend(found);
                    a = b + 1;
                }
            }
        }
        i = group_end;
    }
    super::tokens::scan(units, &mut hits);
    super::secrets::scan(units, &mut hits);
    sort_dedup(hits)
}

fn sort_dedup(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by_key(|hit| (hit.start, hit.length, hit.kind as u8));
    hits.dedup_by_key(|hit| (hit.kind, hit.start, hit.length));
    hits
}

fn is_digit(u: u16) -> bool {
    (u16::from(b'0')..=u16::from(b'9')).contains(&u)
}

/// Longest digit entity (a 19-digit card).
const MAX_ENTITY_DIGITS: usize = 19;
/// Short numbers allowed around the entities of one group, and how many.
const MAX_SHORT_EDGE_DIGITS: usize = 3;
const MAX_SHORT_EDGE_RUNS: usize = 2;
/// Separators tolerated between two digit runs of one group.
const MAX_SEPARATOR_RUN: usize = 3;

/// Space and hyphen only. Tabs and line breaks would join table cells and
/// unrelated JSON values into "entities" (second review: most numeric
/// tables were flagged), and could not join text parts anyway.
fn is_separator(u: u16) -> bool {
    u == u16::from(b' ') || u == u16::from(b'-')
}

/// 从 `start`（必须是数字）开始的数字组终点：最多 3 个分隔符，且两侧都是
/// 数字时才算组内。
fn group_end(units: &[u16], start: usize) -> usize {
    let mut j = start;
    loop {
        while j < units.len() && is_digit(units[j]) {
            j += 1;
        }
        let mut k = j;
        while k < units.len() && k - j < MAX_SEPARATOR_RUN && is_separator(units[k]) {
            k += 1;
        }
        if k == j || k >= units.len() || !is_digit(units[k]) {
            return j;
        }
        j = k;
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
    let written_as = |forms: &[&[usize]]| c.runs.len() == 1 || forms.contains(&c.runs);
    if d.len() == 17
        && c.x_suffix
        && written_as(&[&[6, 8, 3], &[6, 11], &[6, 4, 4, 3]])
        && id_structure(d)
    {
        push(
            FindingKind::CnResidentId,
            c.end + 1,
            id_sensitivity(d, b'X'),
        );
    }
    if d.len() == 18 && written_as(&[&[6, 8, 4], &[6, 12], &[6, 4, 4, 4]]) && id_structure(&d[..17])
    {
        push(
            FindingKind::CnResidentId,
            c.end,
            id_sensitivity(&d[..17], b'0' + d[17]),
        );
    }
    if d.len() == 15 && written_as(&[&[6, 6, 3], &[6, 9]]) && legacy_id_structure(d) {
        push(FindingKind::CnResidentId, c.end, Sensitivity::S2);
    }
    let mobile = match d.len() {
        11 => written_as(&[&[3, 4, 4], &[3, 8], &[7, 4], &[4, 3, 4]]) && is_mobile(d),
        13 => {
            written_as(&[&[2, 3, 4, 4], &[2, 11], &[2, 3, 8], &[5, 4, 4]])
                && d[..2] == [8, 6]
                && is_mobile(&d[2..])
        }
        _ => false,
    };
    if mobile {
        push(FindingKind::CnMobile, c.end, Sensitivity::S2);
    }
    let card_form = c.runs.len() == 1
        || c.runs == [6, 13]
        || (c.runs.len() >= 4
            && c.runs[..4] == [4, 4, 4, 4]
            && (c.runs.len() == 4 || (c.runs.len() == 5 && (1..=3).contains(&c.runs[4]))));
    if (16..=19).contains(&d.len()) && card_form && card_iin(d) && luhn_valid(d) {
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

/// Issuer ranges that exist (ISO/IEC 7812 major industry 3–6, Mastercard
/// 2221–2720, UnionPay 81). Rules out years, ports and most counters.
fn card_iin(d: &[u8]) -> bool {
    match d[0] {
        3..=6 => true,
        2 => (2221..=2720).contains(&number(&d[..4])),
        8 => d[1] == 1,
        _ => false,
    }
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
