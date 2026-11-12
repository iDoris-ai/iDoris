//! 文本的两件基础设施：**不会被意外打印的字符串**，和**抗简单变形的归一化**。
//!
//! # [`Sensitive`]：正文不落盘的类型层保证
//!
//! 孩子的输入和模型的回答都是正文。设计稿 §7：「正文只在内存里处理」。最容易让正文漏进日志、panic 信息、
//! 错误文本的办法是 `{:?}`：一个 `#[derive(Debug)]` 的结构体里只要有一个 `String` 字段，它的 Debug 就把正文打出来。
//! 所以安全模块里所有装正文的字段都是 [`Sensitive`]：它的 `Debug` 只写字节数，**没有 `Display`**，没有 `Serialize`，
//! 想拿到文本必须显式调用 [`Sensitive::expose`]（grep 一下就能审计所有出口）。
//!
//! # [`fold`]：给规则检查器的归一化
//!
//! 规则检查器匹配的是关键词和模式，孩子（或越狱的人）最先想到的绕法是变形：大小写、全角字符、零宽字符、
//! 字母之间插空格或标点、`0`/`1`/`3` 代替字母、泰语声调符号写漏或写多。归一化把这些全部折叠掉，得到三个视图：
//!
//! | 视图 | 内容 | 用在 |
//! |---|---|---|
//! | `plain` | 小写；全角转半角；泰文和阿拉伯数字转 ASCII；去掉零宽字符和泰语声调符号；连续空白折成一个空格 | 泰语和英语的子串、整词匹配 |
//! | `squeezed` | `plain` 去掉所有空白和标点，只留字母、数字和泰文字符 | 字母间被插了空格或标点的写法（`s u i c i d e`、`ฆ่า ตัว ตาย`） |
//! | `leet` | 先把 `@ $ 0 1 3 4 5 7` 换成对应字母，再压缩 | 用数字符号代替字母的英文写法（`k1ll myself`） |
//!
//! 泰语声调符号（`่ ้ ๊ ๋`）和 `์`（การันต์）在匹配前统一去掉：**文本和模式用同一个函数折叠**，所以
//! 声调写漏、写错的变体也命中，代价是极少数只靠声调区分的词会被当成同一个词（对一个失败即关闭的过滤器，这个方向是安全的）。
//!
//! 重复字母（`suiciiiide`）和近音拼写没有处理：那是 LLM 检查器的事，见 `llm` 模块。

use std::fmt;

/// 装正文的字符串。`Debug` 只写字节数；没有 `Display`、没有 `Serialize`。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Sensitive(String);

impl Sensitive {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// 取出正文。每一处调用都是正文离开这个类型的地方，审计时 grep 它。
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 字节数（可以安全地打日志）。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Sensitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted {} bytes>", self.0.len())
    }
}

impl From<String> for Sensitive {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// 归一化之后的三个视图。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Folded {
    pub plain: String,
    pub squeezed: String,
    pub leet: String,
}

impl fmt::Debug for Folded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Folded(<redacted {} bytes>)", self.plain.len())
    }
}

/// 泰文字符区（U+0E00..=U+0E7F）。
fn is_thai(c: char) -> bool {
    ('\u{0E00}'..='\u{0E7F}').contains(&c)
}

/// 要丢掉的字符：零宽、格式控制、变体选择符、泰语声调符号和 การันต์。
fn is_dropped(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{0E48}'..='\u{0E4C}')
}

/// 单个字符的归一化：全角转半角、各种数字转 ASCII。
fn map_char(c: char) -> char {
    match c {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        '\u{0E50}'..='\u{0E59}' => char::from_u32('0' as u32 + (c as u32 - 0x0E50)).unwrap_or(c),
        '\u{0660}'..='\u{0669}' => char::from_u32('0' as u32 + (c as u32 - 0x0660)).unwrap_or(c),
        '\u{06F0}'..='\u{06F9}' => char::from_u32('0' as u32 + (c as u32 - 0x06F0)).unwrap_or(c),
        other => other,
    }
}

fn leet_char(c: char) -> char {
    match c {
        '@' | '4' => 'a',
        '$' | '5' => 's',
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '7' => 't',
        other => other,
    }
}

fn keeps_in_squeezed(c: char) -> bool {
    c.is_alphanumeric() || is_thai(c)
}

/// 把一段文本折叠成三个视图。
pub fn fold(text: &str) -> Folded {
    let mut plain = String::with_capacity(text.len());
    let mut leet = String::new();
    let mut squeezed = String::new();
    let mut pending_space = false;
    for original in text.chars() {
        if is_dropped(original) {
            continue;
        }
        let mapped = map_char(original);
        if mapped.is_whitespace() {
            pending_space = true;
            continue;
        }
        for lower in mapped.to_lowercase() {
            if pending_space && !plain.is_empty() {
                plain.push(' ');
            }
            pending_space = false;
            plain.push(lower);
            if keeps_in_squeezed(lower) {
                squeezed.push(lower);
            }
            let swapped = leet_char(lower);
            if keeps_in_squeezed(swapped) {
                leet.push(swapped);
            }
        }
    }
    Folded {
        plain,
        squeezed,
        leet,
    }
}

/// 一个匹配模式的预先折叠形式。
///
/// 短模式只在 `plain` 视图里匹配：`squeezed` / `leet` 视图把空格和标点都去掉了，词的边界没了，
/// 短模式会在词与词之间撞出误报（`cook meth` 会命中 `cook method`）。所以压缩视图只用于足够长的模式：
/// 纯 ASCII 的至少 10 个字符，含泰文的至少 6 个字符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    plain: String,
    squeezed: String,
    use_squeezed: bool,
    use_leet: bool,
}

/// 压缩视图只对不短于这个长度的纯 ASCII 模式启用。
const MIN_SQUEEZED_ASCII: usize = 10;
/// 压缩视图只对不短于这个长度的含泰文模式启用。
const MIN_SQUEEZED_THAI: usize = 6;

impl Needle {
    pub fn new(pattern: &str) -> Self {
        let folded = fold(pattern);
        let length = folded.squeezed.chars().count();
        let ascii_only = folded.squeezed.is_ascii();
        let use_squeezed = if ascii_only {
            length >= MIN_SQUEEZED_ASCII
        } else {
            length >= MIN_SQUEEZED_THAI
        };
        let use_leet = ascii_only
            && length >= MIN_SQUEEZED_ASCII
            && folded.squeezed.bytes().all(|b| b.is_ascii_lowercase());
        Self {
            plain: folded.plain,
            squeezed: folded.squeezed,
            use_squeezed,
            use_leet,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.plain.is_empty()
    }

    /// 模式出现在文本的任一视图里。
    pub fn found_in(&self, haystack: &Folded) -> bool {
        if self.is_empty() {
            return false;
        }
        self.find_all(&haystack.plain, View::Plain).next().is_some()
            || (self.use_squeezed && haystack.squeezed.contains(&self.squeezed))
            || (self.use_leet && haystack.leet.contains(&self.squeezed))
    }

    /// 某个视图下模式的所有出现位置（字节区间）。`plain` 视图里，模式两端是 ASCII 字母数字时要求落在词边界上
    /// （`cook meth` 不命中 `cook method`，`make a bomb` 不命中 `make a bombastic`）；泰文没有词边界，不检查。
    fn find_all<'a>(
        &'a self,
        text: &'a str,
        view: View,
    ) -> impl Iterator<Item = (usize, usize)> + 'a {
        let pattern = self.pattern(view);
        let bounded = matches!(view, View::Plain);
        let starts_alnum = pattern
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let ends_alnum = pattern
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let search = if pattern.is_empty() {
            None
        } else {
            Some(pattern)
        };
        search
            .into_iter()
            .flat_map(move |pattern| text.match_indices(pattern))
            .map(|(start, matched)| (start, start + matched.len()))
            .filter(move |&(start, end)| {
                if !bounded {
                    return true;
                }
                let before_ok = !starts_alnum
                    || !text[..start]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric());
                let after_ok = !ends_alnum
                    || !text[end..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric());
                before_ok && after_ok
            })
    }
}

#[derive(Clone, Copy)]
enum View {
    Plain,
    Squeezed,
}

impl Needle {
    /// 某个视图下用的模式；这个视图对该模式不启用时返回空串。
    fn pattern(&self, view: View) -> &str {
        match view {
            View::Plain => &self.plain,
            View::Squeezed if self.use_squeezed => &self.squeezed,
            View::Squeezed => "",
        }
    }
}

/// `text[from..to]` 是否不超过 `gap` 个字符（`from <= to`，都在字符边界上）。
fn within(text: &str, from: usize, to: usize, gap: usize) -> bool {
    to >= from && to - from <= gap.saturating_mul(4) && text[from..to].chars().count() <= gap
}

/// 最长的模式占多少字节（给「向前找」的搜索窗口留余量；泰文 3 字节一个字符，词表里最长的模式不到 40 个字符）。
const MAX_PATTERN_BYTES: usize = 160;

/// `a` 类词出现之后的 `gap` 个字符内有 `b` 类词（`either_order` 时前后都算）。
///
/// 对抗性文本（一兆字节的 `บอกวิธี` 和 `ทำระเบิด` 交替重复）不能把它拖成平方时间：`b` 的所有出现位置排好序，
/// 对每个 `a` 的出现位置只二分查找窗口内的那几个，总共 `O(A log B + 窗口内命中数)`。
pub fn near(haystack: &Folded, a: &[Needle], b: &[Needle], gap: usize, either_order: bool) -> bool {
    let reach = gap.saturating_mul(4);
    for (text, view) in [
        (haystack.plain.as_str(), View::Plain),
        (haystack.squeezed.as_str(), View::Squeezed),
    ] {
        let mut b_spans: Vec<(usize, usize)> = b
            .iter()
            .flat_map(|needle| needle.find_all(text, view))
            .collect();
        if b_spans.is_empty() {
            continue;
        }
        b_spans.sort_unstable();
        for a_needle in a {
            for (a_start, a_end) in a_needle.find_all(text, view) {
                // 向后：第一个从 a_end 开始或之后开始的 b，到窗口尽头
                let first_after = b_spans.partition_point(|&(start, _)| start < a_end);
                for &(b_start, _) in &b_spans[first_after..] {
                    if b_start - a_end > reach {
                        break;
                    }
                    if within(text, a_end, b_start, gap) {
                        return true;
                    }
                }
                // 向前：a_start 之前结束的 b。b 按起点排序，从 a_start 往回走到窗口尽头
                if either_order {
                    let last_before = b_spans.partition_point(|&(start, _)| start < a_start);
                    for &(b_start, b_end) in b_spans[..last_before].iter().rev() {
                        if a_start.saturating_sub(b_start) > reach + MAX_PATTERN_BYTES {
                            break;
                        }
                        if b_end <= a_start && within(text, b_end, a_start, gap) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// 英文整词匹配：文本按「非 ASCII 字母数字」切词，词和 `words` 中的某个相等。
pub fn has_word(haystack: &Folded, words: &[&str]) -> bool {
    haystack
        .plain
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| words.contains(&token))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn sensitive_never_shows_its_text() {
        let secret = Sensitive::new("SECRET-SENTINEL-1234");
        let shown = format!("{secret:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("20 bytes"), "{shown}");
        // 嵌在派生 Debug 的结构里同样安全
        #[derive(Debug)]
        struct Holder {
            _text: Sensitive,
            _many: Vec<Sensitive>,
            _maybe: Option<Sensitive>,
        }
        let holder = Holder {
            _text: secret.clone(),
            _many: vec![secret.clone()],
            _maybe: Some(secret.clone()),
        };
        assert!(!format!("{holder:?}").contains("SECRET"));
        assert!(!format!("{holder:#?}").contains("SECRET"));
        assert_eq!(secret.expose(), "SECRET-SENTINEL-1234");
        assert!(!format!("{:?}", fold("SECRET-SENTINEL")).contains("SECRET"));
    }

    #[test]
    fn folding_defeats_the_cheap_evasions() {
        let folded = fold("  K i L l   MY\u{200B}SELF  ");
        // 大小写折叠、零宽字符丢掉、连续空白折成一个空格、首尾空白去掉
        assert_eq!(folded.plain, "k i l l myself");
        assert_eq!(folded.squeezed, "killmyself");
        // 全角
        assert_eq!(fold("ＫＩＬＬ").plain, "kill");
        // 数字和符号代替字母
        assert_eq!(fold("k1ll my$elf").leet, "killmyself");
        assert_eq!(fold("h@ck").leet, "hack");
        // 泰文数字和阿拉伯-印度数字
        assert_eq!(fold("๐๘๐-๐๐๐-๑๐๐๑").plain, "080-000-1001");
        assert_eq!(fold("٠٨٠").plain, "080");
    }

    #[test]
    fn thai_tone_marks_are_folded_on_both_sides() {
        // 声调写漏、写多的变体和标准写法折叠成同一个串
        assert_eq!(fold("ฆ่าตัวตาย").plain, fold("ฆาตัวตาย").plain);
        assert_eq!(fold("ฆ่าตัวตาย").plain, fold("ฆ้าตัวตาย").plain);
        let needle = Needle::new("ฆ่าตัวตาย");
        assert!(needle.found_in(&fold("หนูอยากฆาตัวตาย")));
        // 字之间被插了空格
        assert!(needle.found_in(&fold("ฆ่า ตัว ตาย")));
        // 毫不相干的文本
        assert!(!needle.found_in(&fold("วันนี้อากาศดี")));
    }

    #[test]
    fn needles_match_the_plain_squeezed_and_leet_views() {
        let needle = Needle::new("kill myself");
        assert!(needle.found_in(&fold("I will kill myself")));
        assert!(needle.found_in(&fold("k.i.l.l m-y-s-e-l-f")));
        assert!(needle.found_in(&fold("k1ll my$elf")));
        assert!(!needle.found_in(&fold("kill the process then reload myself page")));
        // 太短的模式不走 leet 视图（避免数字文本误伤）
        // 太短的模式只在 plain 视图里匹配：压缩视图没有词边界，短模式会在词与词之间撞出误报
        let short = Needle::new("die");
        assert!(!short.use_leet && !short.use_squeezed);
        assert!(!short.found_in(&fold("d13")));
        assert!(Needle::new("kill myself").use_leet);
        // plain 视图里 ASCII 模式要落在词边界上：`cook meth` 不命中 `cook method`
        let cook = Needle::new("cook meth");
        assert!(cook.found_in(&fold("they cook meth at night")));
        assert!(!cook.found_in(&fold("the cook method for rice")));
        let bomb = Needle::new("make a bomb");
        assert!(bomb.found_in(&fold("How to make a bomb?")));
        assert!(!bomb.found_in(&fold("make a bombastic speech")));
        assert!(!bomb.found_in(&fold("remake a bomb")));
    }

    #[test]
    fn near_respects_the_gap_the_order_and_both_views() {
        let a = [Needle::new("บอก")];
        let b = [Needle::new("ระเบิด")];
        assert!(near(&fold("บอกวิธีทำระเบิด"), &a, &b, 14, false));
        // 超出间隔
        assert!(!near(
            &fold("บอกอะไรก็ได้ที่ไม่เกี่ยวข้องกับเรื่องนี้เลยนะแล้วค่อยพูดถึงระเบิด"),
            &a,
            &b,
            5,
            false
        ));
        // 顺序相反：只有 either_order 才算
        assert!(!near(&fold("ระเบิดบอก"), &a, &b, 10, false));
        assert!(near(&fold("ระเบิดบอก"), &a, &b, 10, true));
        // 被插了空格也命中（squeezed 视图；只对足够长的模式启用）
        let long_a = [Needle::new("สอนวิธี")];
        let long_b = [Needle::new("ทำระเบิด")];
        assert!(near(
            &fold("ส อ น วิ ธี ทำ ร ะ เ บิ ด"),
            &long_a,
            &long_b,
            5,
            false
        ));
        // 短模式不走压缩视图：词与词之间撞出的误报比漏掉一种变形更糟
        assert!(!near(
            &fold("บ อ ก ร ะ เ บิ ด"),
            &a,
            &[Needle::new("ระเบิด")],
            12,
            false
        ));
    }

    #[test]
    fn near_is_not_quadratic_on_adversarial_text() {
        // 一大片密集的 A、隔一段空白、再一大片密集的 B：两边各上万个出现位置，但没有任何一对在窗口内。
        // O(A x B) 的实现要做上亿次比较；二分之后只做 O(A log B)。
        let a = [Needle::new("บอกวิธี")];
        let b = [Needle::new("ทำระเบิด")];
        let text = format!(
            "{}{}{}",
            "บอกวิธี".repeat(15_000),
            "ก".repeat(1_000),
            "ทำระเบิด".repeat(15_000)
        );
        let folded = fold(&text);
        assert!(folded.plain.len() > 600_000);
        let started = std::time::Instant::now();
        assert!(!near(&folded, &a, &b, 10, true));
        assert!(!near(&folded, &a, &b, 10, false));
        // 间隔放宽到 1500 个字符：最后一个 A 和第一个 B 相隔 1000 个字符，命中
        assert!(near(&folded, &a, &b, 1_500, false));
        assert!(near(&folded, &a, &b, 1_500, true));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn whole_word_matching_does_not_hit_inside_longer_words() {
        let folded = fold("Bombay is a city; the shitake mushroom; assassin");
        assert!(!has_word(&folded, &["bomb", "shit", "ass"]));
        assert!(has_word(&fold("what a SHIT day"), &["shit"]));
        assert!(has_word(&fold("f*ck, shit!"), &["shit"]));
    }
}
