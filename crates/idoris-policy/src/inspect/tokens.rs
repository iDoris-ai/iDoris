//! L0 其余中文/港澳标识符与邮箱（docs/15 §1.7）：统一社会信用代码、护照、
//! 港澳居民来往内地通行证（回乡证）、香港身份证、车牌、邮箱。
//!
//! 先切出最大的 ASCII 字母数字串（token），再按长度和结构判定。统一社会
//! 信用代码和香港身份证的结构信号太弱（任意大写字母数字都可能长得像），所以
//! 校验位是必要条件；护照、回乡证、车牌没有校验位，只看结构。

use idoris_contracts::inspect::{FindingKind, Sensitivity};

use super::Hit;

/// GB 32100-2015 字符集（去掉 I、O、Z、S、V），下标就是字符值。
const USCC_CHARS: &[u8; 31] = b"0123456789ABCDEFGHJKLMNPQRTUWXY";
const USCC_WEIGHTS: [u32; 17] = [
    1, 3, 9, 27, 19, 26, 16, 17, 20, 29, 25, 13, 8, 24, 10, 30, 28,
];
/// GA 36-2018 省级简称。
const PLATE_PROVINCES: &str = "京津沪渝冀豫云辽黑湘皖鲁新苏浙赣鄂桂甘晋蒙陕吉闽贵粤青藏川宁琼";

pub(super) fn scan(units: &[u16], hits: &mut Vec<Hit>) {
    let mut i = 0;
    while i < units.len() {
        if !is_alnum(units[i]) {
            i += 1;
            continue;
        }
        let mut end = i;
        while end < units.len() && is_alnum(units[end]) {
            end += 1;
        }
        let token: Vec<u8> = units[i..end]
            .iter()
            .map(|&u| (u as u8).to_ascii_uppercase())
            .collect();
        classify_token(&token, i, &units[end..], hits);
        i = end;
    }
    scan_plates(units, hits);
    scan_emails(units, hits);
}

fn classify_token(t: &[u8], start: usize, rest: &[u16], hits: &mut Vec<Hit>) {
    {
        let mut push = |kind, length, sensitivity| {
            hits.push(Hit {
                kind,
                start,
                length,
                sensitivity,
            });
        };
        if t.len() == 18 && uscc_valid(t) {
            push(FindingKind::CnUnifiedSocialCreditCode, 18, Sensitivity::S1);
        }
        if t.len() == 9 && is_passport(t) {
            push(FindingKind::CnPassport, 9, Sensitivity::S3);
        }
        if t.len() == 9 && matches!(t[0], b'H' | b'M') && all_digits(&t[1..]) {
            push(FindingKind::HkMacauTravelPermit, 9, Sensitivity::S3);
        }
    }
    // `A123456(3)`、`A123456 (3)`、`A123456（3）`：校验位写在紧跟的成对括号里。
    // 单字母与双字母共用 [`hkid_paren_window`]，span 含那个 ASCII 空格和括号。
    push_parenthesized_hkid(t, 1, start, rest, hits);
    push_parenthesized_hkid(t, 2, start, rest, hits);
    if let Some((&check, body)) = t.split_last()
        && hkid_valid(body, check)
    {
        hits.push(Hit {
            kind: FindingKind::HkIdentityCard,
            start,
            length: t.len(),
            sensitivity: Sensitivity::S3,
        });
    }
}

fn is_alnum(u: u16) -> bool {
    u < 0x80 && (u as u8).is_ascii_alphanumeric()
}

fn all_digits(t: &[u8]) -> bool {
    t.iter().all(u8::is_ascii_digit)
}

/// 普通护照 `E`+8 位数字或 `E`+字母（不含 I、O）+7 位数字；旧式 `G`+8 位数字。
fn is_passport(t: &[u8]) -> bool {
    match t[0] {
        b'G' => all_digits(&t[1..]),
        b'E' => {
            all_digits(&t[1..])
                || (t[1].is_ascii_uppercase()
                    && !matches!(t[1], b'I' | b'O')
                    && all_digits(&t[2..]))
        }
        _ => false,
    }
}

/// 第 3–8 位是行政区划（数字），第 18 位是 MOD 31-3 校验位。
fn uscc_valid(t: &[u8]) -> bool {
    let values: Option<Vec<u32>> = t
        .iter()
        .map(|c| USCC_CHARS.iter().position(|x| x == c).map(|v| v as u32))
        .collect();
    let Some(values) = values else {
        return false;
    };
    let sum: u32 = values.iter().zip(USCC_WEIGHTS).map(|(v, w)| v * w).sum();
    all_digits(&t[2..8]) && values[17] == (31 - sum % 31) % 31
}

/// U+FF08 `（` / U+FF09 `）`。只认这两个码位，不做通用 Unicode 规范化。
const HKID_FULLWIDTH_OPEN: u16 = 0xFF08;
const HKID_FULLWIDTH_CLOSE: u16 = 0xFF09;

/// 主体后面的有界括号窗口：可选恰好一个 ASCII 空格，然后开括号、一位
/// ASCII 校验、闭括号。`()` 与 `（）` 必须各自成对。缺闭括号或闭括号
/// 不在窗口内就返回 `None`，不往后再扫。返回的长度含空格和括号。
fn hkid_paren_window(rest: &[u16]) -> Option<(u8, usize)> {
    let spaced = rest.first().is_some_and(|&unit| unit == u16::from(b' '));
    let base = usize::from(spaced);
    let open = *rest.get(base)?;
    let check = *rest.get(base + 1)?;
    let close = *rest.get(base + 2)?;
    let ascii_pair = open == u16::from(b'(') && close == u16::from(b')');
    let fullwidth_pair = open == HKID_FULLWIDTH_OPEN && close == HKID_FULLWIDTH_CLOSE;
    if !(ascii_pair || fullwidth_pair) || check > 0x7F {
        return None;
    }
    Some(((check as u8).to_ascii_uppercase(), base + 3))
}

/// `letters` 为 1 或 2。两个调用点共用 [`hkid_paren_window`]。
fn push_parenthesized_hkid(
    t: &[u8],
    letters: usize,
    start: usize,
    rest: &[u16],
    hits: &mut Vec<Hit>,
) {
    if t.len() != letters + 6 {
        return;
    }
    let Some((check, extra)) = hkid_paren_window(rest) else {
        return;
    };
    if hkid_valid(t, check) {
        hits.push(Hit {
            kind: FindingKind::HkIdentityCard,
            start,
            length: t.len() + extra,
            sensitivity: Sensitivity::S3,
        });
    }
}

/// 1–2 个字母 + 6 位数字，加权 9..2（单字母时首位按空格 = 36），
/// 校验 `(11 - sum % 11) % 11`，10 记作 `A`。
fn hkid_valid(body: &[u8], check: u8) -> bool {
    let letters = body.len().saturating_sub(6);
    if !(1..=2).contains(&letters)
        || !body[..letters].iter().all(u8::is_ascii_uppercase)
        || !all_digits(&body[letters..])
    {
        return false;
    }
    let values = (letters == 1)
        .then_some(36)
        .into_iter()
        .chain(body[..letters].iter().map(|&c| u32::from(c - b'A') + 10))
        .chain(body[letters..].iter().map(|&c| u32::from(c - b'0')));
    let sum: u32 = values.zip((2..=9).rev()).map(|(v, w)| v * w).sum();
    match (11 - sum % 11) % 11 {
        10 => check == b'A',
        digit => check == b'0' + digit as u8,
    }
}

/// 省级简称 + 发牌机关字母 + 可选 `·`/空格 + 5 位（普通）或 6 位（新能源）。
fn scan_plates(units: &[u16], hits: &mut Vec<Hit>) {
    let plate_char = |u: u16| {
        u < 0x80
            && (u as u8).is_ascii_alphanumeric()
            && !matches!(u as u8, b'I' | b'O')
            && !(u as u8).is_ascii_lowercase()
    };
    for (i, &u) in units.iter().enumerate() {
        if !PLATE_PROVINCES.encode_utf16().any(|p| p == u) {
            continue;
        }
        let Some(&office) = units.get(i + 1) else {
            continue;
        };
        if !plate_char(office) || (office as u8).is_ascii_digit() {
            continue;
        }
        let mut j = i + 2;
        if units
            .get(j)
            .is_some_and(|&s| s == 0xB7 || s == u16::from(b' '))
        {
            j += 1;
        }
        let tail_start = j;
        while j < units.len() && plate_char(units[j]) {
            j += 1;
        }
        let tail = &units[tail_start..j];
        let boundary = units.get(j).is_none_or(|&next| !is_alnum(next));
        if (5..=6).contains(&tail.len())
            && boundary
            && tail.iter().any(|&c| (c as u8).is_ascii_digit())
        {
            hits.push(Hit {
                kind: FindingKind::CnLicensePlate,
                start: i,
                length: j - i,
                sensitivity: Sensitivity::S2,
            });
        }
    }
}

/// ASCII `local@domain.tld`：本地部分 `[A-Za-z0-9._%+-]+`，域名至少一个点、
/// 顶级域至少两个字母。全角和非 ASCII 邮箱不在这里匹配。
fn scan_emails(units: &[u16], hits: &mut Vec<Hit>) {
    let local_char = |u: u16| is_alnum(u) || b"._%+-".iter().any(|&c| u16::from(c) == u);
    let domain_char = |u: u16| is_alnum(u) || u == u16::from(b'.') || u == u16::from(b'-');
    for (at, &u) in units.iter().enumerate() {
        if u != u16::from(b'@') {
            continue;
        }
        let mut start = at;
        while start > 0 && local_char(units[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < units.len() && domain_char(units[end]) {
            end += 1;
        }
        while end > at + 1 && units[end - 1] == u16::from(b'.') {
            end -= 1;
        }
        let domain = &units[at + 1..end];
        let tld_ok = domain
            .iter()
            .rposition(|&c| c == u16::from(b'.'))
            .is_some_and(|dot| {
                let tld = &domain[dot + 1..];
                tld.len() >= 2
                    && tld
                        .iter()
                        .all(|&c| c < 0x80 && (c as u8).is_ascii_alphabetic())
            });
        if start < at && tld_ok {
            hits.push(Hit {
                kind: FindingKind::EmailAddress,
                start,
                length: end - start,
                sensitivity: Sensitivity::S2,
            });
        }
    }
}
